//! Crash reporting: keep the log and leave a window behind.
//!
//! Before this module, a panic on the UI thread made the desktop shell vanish
//! without a trace: stderr goes to the launcher (or /dev/null), and even a
//! graceful unwind ends `Application::run`. The panic hook installed here
//!
//! 1. always writes a readable report under `<data dir>/crashes/`, and
//! 2. for panics on the main thread (the GPUI event loop) launches a separate
//!    copy of the binary with `--crash-report <path>`, which shows the report
//!    in a small window with copy / open / restart actions.
//!
//! The child process is deliberate. A panic that unwinds through the
//! platform's FFI event loop aborts the process, and `Command::spawn` in the
//! hook is the only thing guaranteed to survive that. Panics on background
//! threads (tokio workers, tool panics isolated by the harness) are logged but
//! do not open a window: the app keeps running and a popup would be noise.

use std::backtrace::Backtrace;
use std::io::Write;
use std::panic::{AssertUnwindSafe, PanicHookInfo};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use effibuddy_ui::{Button, ButtonVariant, EffiBuddyAssets, IconButton, Theme, icons};
use gpui::{
    App, Application, Bounds, ClipboardItem, Context, IntoElement, Render, SharedString, Window,
    WindowBounds, WindowControlArea, WindowOptions, div, prelude::*, px, size, svg,
};

use crate::config::AppConfig;

/// Argument that puts the binary into crash-reporter mode. Kept `pub` because
/// `main` must dispatch it before any window or GPU initialization.
pub const CRASH_REPORT_ARG: &str = "--crash-report";

const ISSUES_URL: &str = "https://github.com/lumenShen/EffiBuddy/issues";

/// Report prefix used by the reporter window to pull out the interesting
/// fields without re-reading the panic itself.
const LABEL_MESSAGE: &str = "消息: ";
const LABEL_LOCATION: &str = "位置: ";
const LABEL_VERSION: &str = "版本: ";
const LABEL_TIME: &str = "时间: ";
const LABEL_THREAD: &str = "线程: ";

/// The view shows the head of the report (panic message and the first frames
/// of the backtrace); the full file is always on disk and copied verbatim.
const MAX_DISPLAY_BYTES: usize = 128 * 1024;
/// Crash logs pile up slowly, but they should not grow forever.
const MAX_LOGS: usize = 20;

/// Set while the panic hook is already bringing up a window. The process is
/// on its way out; a second window (from a double panic) would only race the
/// abort.
static REPORT_LAUNCHED: AtomicBool = AtomicBool::new(false);
static HOOK_INSTALLED: AtomicBool = AtomicBool::new(false);

/// Install the crash hook. Call once, on the main thread, after choosing
/// between normal and `--crash-report` mode.
pub fn install_panic_reporter() {
    if HOOK_INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let main_thread = std::thread::current().id();
    // Chain to whatever hook was installed before (the default hook prints
    // the panic and honors RUST_BACKTRACE), so terminal launches keep their
    // familiar output.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let path = std::panic::catch_unwind(AssertUnwindSafe(|| handle_panic(info)))
            .ok()
            .flatten();
        previous(info);
        if std::thread::current().id() == main_thread {
            if let Some(path) = &path {
                eprintln!("EffiBuddy 崩溃日志已保存到 {}", path.display());
                if !REPORT_LAUNCHED.swap(true, Ordering::SeqCst) {
                    spawn_report_window(path);
                }
            }
        }
    }));
}

/// Path passed after `--crash-report`, if any.
pub fn crash_report_path() -> Option<PathBuf> {
    let mut args = std::env::args_os().skip(1);
    while let Some(argument) = args.next() {
        if argument == CRASH_REPORT_ARG {
            return args.next().map(PathBuf::from);
        }
    }
    None
}

fn handle_panic(info: &PanicHookInfo<'_>) -> Option<PathBuf> {
    let now = chrono::Local::now();
    let message = panic_message(info);
    let thread = std::thread::current()
        .name()
        .unwrap_or("<unnamed>")
        .to_string();
    // Independent of RUST_BACKTRACE: when the window is the only way to see
    // the report, a bare message is not enough to file a useful issue.
    let backtrace = Backtrace::force_capture().to_string();
    let report = format_report(&message, info.location(), &thread, &backtrace, now);
    write_crash_log(&report, now)
}

fn panic_message(info: &PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "panic with a non-string payload".to_string()
    }
}

fn format_report(
    message: &str,
    location: Option<&std::panic::Location<'_>>,
    thread: &str,
    backtrace: &str,
    now: chrono::DateTime<chrono::Local>,
) -> String {
    // Continuation lines are indented so the `消息: ` label still parses when
    // the panic message itself contains newlines.
    let message = message.replace('\n', "\n      ");
    let location = location
        .map(|location| {
            format!(
                "{}:{}:{}",
                location.file(),
                location.line(),
                location.column()
            )
        })
        .unwrap_or_else(|| "<unknown>".to_string());
    format!(
        "EffiBuddy 崩溃报告\n\
         ==================\n\n\
         {LABEL_VERSION}{} ({})\n\
         {LABEL_TIME}{}\n\
         系统: {} {}\n\
         进程: {}\n\
         {LABEL_THREAD}{thread}\n\
         {LABEL_LOCATION}{location}\n\
         {LABEL_MESSAGE}{message}\n\n\
         回溯\n----\n\
         （开头几帧属于崩溃上报器自身，真正的 panic 位置见上面的「位置」）\n\
         {backtrace}\n\n\
         提示\n----\n\
         本文件只保存在本机，不会自动上传。反馈问题时请附上完整内容：\n\
         {ISSUES_URL}\n",
        crate::update::current_version(),
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        now.format("%Y-%m-%d %H:%M:%S %:z"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::process::id(),
    )
}

fn write_crash_log(report: &str, now: chrono::DateTime<chrono::Local>) -> Option<PathBuf> {
    let primary = crate::config::base_dir().join("crashes");
    write_crash_log_in(&primary, report, now).or_else(|| {
        // The data dir can be unwritable (read-only mount, disk full). A
        // report in the temp dir still beats no report at all.
        write_crash_log_in(&std::env::temp_dir().join("EffiBuddy-crashes"), report, now)
    })
}

fn write_crash_log_in(
    dir: &Path,
    report: &str,
    now: chrono::DateTime<chrono::Local>,
) -> Option<PathBuf> {
    std::fs::create_dir_all(dir).ok()?;
    let stamp = now.format("%Y%m%d-%H%M%S").to_string();
    let path = write_report(dir, report, &stamp, std::process::id())?;
    prune_logs(dir);
    Some(path)
}

fn write_report(dir: &Path, report: &str, stamp: &str, pid: u32) -> Option<PathBuf> {
    let path = dir.join(format!("crash-{stamp}-{pid}.log"));
    let mut file = std::fs::File::create(&path).ok()?;
    file.write_all(report.as_bytes()).ok()?;
    let _ = file.flush();
    Some(path)
}

/// Keep the newest [`MAX_LOGS`] reports. Best effort: a failure here must
/// never turn into a second panic.
fn prune_logs(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut logs: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?;
            if !(name.starts_with("crash-") && name.ends_with(".log")) {
                return None;
            }
            Some((entry.metadata().ok()?.modified().ok()?, path))
        })
        .collect();
    if logs.len() <= MAX_LOGS {
        return;
    }
    logs.sort_by_key(|(modified, _)| *modified);
    let excess = logs.len() - MAX_LOGS;
    for (_, path) in logs.into_iter().take(excess) {
        let _ = std::fs::remove_file(path);
    }
}

fn spawn_report_window(path: &Path) {
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    let mut command = Command::new(executable);
    command
        .arg(CRASH_REPORT_ARG)
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        // The reporter draws its own window; do not flash a console on top.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let _ = command.spawn();
}

/// Run the standalone crash-report window. `main` dispatches here before
/// creating the normal application, so this function owns the process.
pub fn run_crash_report_window(path: PathBuf) {
    let log = load_report(&path);
    let theme = if AppConfig::load().theme == "light" {
        Theme::light()
    } else {
        Theme::dark()
    };

    Application::new()
        .with_assets(EffiBuddyAssets)
        .run(move |cx: &mut App| {
            theme.clone().set_global(cx);
            let bounds = Bounds::centered(None, size(px(820.0), px(640.0)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                // Custom head like the main shell; the platform titlebar is
                // not drawn by GPUI on Linux.
                titlebar: None,
                window_min_size: Some(size(px(620.0), px(440.0))),
                app_id: Some("effibuddy-crash".to_string()),
                ..Default::default()
            };
            let root = theme.clone();
            match cx.open_window(options, move |window, cx| {
                window.set_window_title("EffiBuddy 崩溃报告");
                window.set_app_id("effibuddy-crash");
                window.activate_window();
                cx.new(|_| CrashReportView::new(log, path, root))
            }) {
                Ok(_) => cx.activate(true),
                Err(error) => {
                    // Nothing to show; exit rather than run an empty loop.
                    eprintln!("cannot open the crash report window: {error}");
                    cx.quit();
                }
            }
        });
}

/// Read the report, tolerating the tiny window where the parent has created
/// the file but not finished writing it (and filesystems that lag).
fn load_report(path: &Path) -> String {
    for _ in 0..20 {
        if let Ok(report) = std::fs::read_to_string(path)
            && !report.trim().is_empty()
        {
            return report;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    format!(
        "无法读取崩溃日志：{}\n\n请确认文件是否仍然存在，或在应用数据目录的 crashes/ 下查找。",
        path.display()
    )
}

fn display_text(log: &str) -> (SharedString, bool) {
    if log.len() <= MAX_DISPLAY_BYTES {
        return (SharedString::from(log.to_string()), false);
    }
    let mut end = MAX_DISPLAY_BYTES;
    while end > 0 && !log.is_char_boundary(end) {
        end -= 1;
    }
    (
        SharedString::from(format!(
            "{}\n\n… 日志较长，窗口只显示前 {} KiB；完整内容请打开日志文件或点击「复制日志」。",
            &log[..end],
            MAX_DISPLAY_BYTES / 1024
        )),
        true,
    )
}

fn report_field<'a>(report: &'a str, label: &str) -> Option<&'a str> {
    report
        .lines()
        .find_map(|line| line.strip_prefix(label))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

struct CrashReportView {
    /// Full report; what 「复制日志」 puts on the clipboard.
    log: String,
    /// Possibly truncated report shown in the scroll area.
    display: SharedString,
    truncated: bool,
    path: PathBuf,
    reason: SharedString,
    location: SharedString,
    meta: SharedString,
    theme: Theme,
    copied: bool,
    notice: Option<SharedString>,
}

impl CrashReportView {
    fn new(log: String, path: PathBuf, theme: Theme) -> Self {
        let (display, truncated) = display_text(&log);
        let reason = report_field(&log, LABEL_MESSAGE)
            .unwrap_or("未知错误")
            .to_string();
        let location = report_field(&log, LABEL_LOCATION).unwrap_or("").to_string();
        let meta = [
            report_field(&log, LABEL_VERSION).map(|version| format!("v{version}")),
            report_field(&log, LABEL_TIME).map(str::to_string),
            report_field(&log, LABEL_THREAD).map(|thread| format!("线程 {thread}")),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
        Self {
            log,
            display,
            truncated,
            path,
            reason: reason.into(),
            location: location.into(),
            meta: meta.into(),
            theme,
            copied: false,
            notice: None,
        }
    }

    fn copy_log(&mut self, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(self.log.clone()));
        self.copied = true;
        self.notice = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1800))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.copied = false;
                cx.notify();
            });
        })
        .detach();
    }

    fn restart(&mut self, cx: &mut Context<Self>) {
        let spawned = std::env::current_exe().and_then(|executable| {
            Command::new(executable)
                .stdin(Stdio::null())
                .spawn()
                .map(|_| ())
        });
        match spawned {
            Ok(()) => cx.quit(),
            Err(error) => {
                self.notice = Some(format!("无法重新启动：{error}").into());
                cx.notify();
            }
        }
    }

    fn render_titlebar(&self, theme: &Theme) -> impl IntoElement {
        div()
            .h(px(40.0))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(14.0))
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.bg)
            .child(
                svg()
                    .path(icons::ALERT)
                    .size(px(14.0))
                    .text_color(theme.danger),
            )
            .child(
                div()
                    .text_size(px(12.5))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text_muted)
                    .child("EffiBuddy 崩溃报告"),
            )
            // Same drag region as the main titlebar: `start_window_move()` is a
            // no-op on Windows, so the portable hit-test path is used instead.
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .window_control_area(WindowControlArea::Drag),
            )
            .child(
                IconButton::new("crash-titlebar-close", icons::X, theme)
                    .size(28.0, 14.0)
                    .tooltip("关闭")
                    .on_click(|_, window, cx| {
                        window.remove_window();
                        cx.quit();
                    }),
            )
    }

    fn render_footer(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let path_for_open = self.path.clone();
        let path_for_reveal = self.path.clone();
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.0))
            .when_some(self.notice.clone(), |element, notice| {
                element.child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(11.5))
                        .text_color(theme.danger)
                        .child(notice),
                )
            })
            .when(self.notice.is_none(), |element| {
                element.child(div().flex_1())
            })
            .child(
                Button::new(
                    "crash-copy",
                    if self.copied {
                        "已复制"
                    } else {
                        "复制日志"
                    },
                    theme,
                )
                .icon(if self.copied {
                    icons::CHECK
                } else {
                    icons::COPY
                })
                .variant(ButtonVariant::Primary)
                .on_click(cx.listener(|this, _, _, cx| this.copy_log(cx))),
            )
            .child(
                Button::new("crash-open", "打开日志文件", theme)
                    .icon(icons::FILE)
                    .variant(ButtonVariant::Secondary)
                    .on_click(move |_, _, cx| cx.open_with_system(&path_for_open)),
            )
            .child(
                Button::new("crash-reveal", "在文件夹中显示", theme)
                    .icon(icons::FOLDER)
                    .variant(ButtonVariant::Ghost)
                    .on_click(move |_, _, cx| cx.reveal_path(&path_for_reveal)),
            )
            .child(
                Button::new("crash-restart", "重新启动", theme)
                    .variant(ButtonVariant::Secondary)
                    .on_click(cx.listener(|this, _, _, cx| this.restart(cx))),
            )
            .child(
                Button::new("crash-dismiss", "关闭", theme)
                    .variant(ButtonVariant::Ghost)
                    .on_click(|_, window, cx| {
                        window.remove_window();
                        cx.quit();
                    }),
            )
    }
}

impl Render for CrashReportView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme.clone();
        let detail = format!(
            "{}{} 行 · {}",
            if self.truncated { "已截断 · " } else { "" },
            self.log.lines().count(),
            crate::attachments::format_bytes(self.log.len() as u64)
        );
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.bg)
            .text_color(theme.text)
            .child(self.render_titlebar(&theme))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .flex()
                    .flex_col()
                    .gap(px(14.0))
                    .p(px(22.0))
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .flex_col()
                            .gap(px(4.0))
                            .child(
                                div()
                                    .text_size(px(17.0))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme.text)
                                    .child("EffiBuddy 遇到问题并已停止"),
                            )
                            .child(
                                div()
                                    .text_size(px(12.5))
                                    .text_color(theme.text_muted)
                                    .child(
                                        "崩溃信息已保存在本机，不会自动上传。可以复制日志反馈给开发者。",
                                    ),
                            )
                            .when(!self.meta.is_empty(), |element| {
                                element.child(
                                    div()
                                        .text_size(px(11.5))
                                        .text_color(theme.text_faint)
                                        .child(self.meta.clone()),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .flex_col()
                            .gap(px(4.0))
                            .rounded(px(10.0))
                            .border_1()
                            .border_color(theme.danger.opacity(0.28))
                            .bg(theme.danger_soft)
                            .p(px(12.0))
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme.danger)
                                    .child("崩溃原因"),
                            )
                            .child(
                                div()
                                    .max_h(px(96.0))
                                    .overflow_hidden()
                                    .text_size(px(13.0))
                                    .text_color(theme.text)
                                    .child(self.reason.clone()),
                            )
                            .when(!self.location.is_empty(), |element| {
                                element.child(
                                    div()
                                        .truncate()
                                        .font_family(theme.mono_font())
                                        .text_size(px(11.0))
                                        .text_color(theme.text_muted)
                                        .child(self.location.clone()),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .text_size(px(11.5))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme.text_muted)
                                    .child("崩溃日志"),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .truncate()
                                    .font_family(theme.mono_font())
                                    .text_size(px(11.0))
                                    .text_color(theme.text_faint)
                                    .child(self.path.display().to_string()),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(px(11.0))
                                    .text_color(theme.text_faint)
                                    .child(detail),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_h(px(0.0))
                            .w_full()
                            .rounded(px(10.0))
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.code_bg)
                            .overflow_hidden()
                            .child(
                                div()
                                    .id("crash-log")
                                    .size_full()
                                    .p(px(14.0))
                                    .overflow_y_scroll()
                                    .font_family(theme.mono_font())
                                    .text_size(px(11.5))
                                    .text_color(theme.text_muted)
                                    .child(self.display.clone()),
                            ),
                    )
                    .child(self.render_footer(&theme, cx)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timestamp() -> chrono::DateTime<chrono::Local> {
        chrono::Local::now()
    }

    #[test]
    fn report_carries_message_location_and_backtrace() {
        let report = format_report(
            "index out of bounds",
            None,
            "main",
            "   0: std::panicking::begin_panic",
            timestamp(),
        );
        assert!(report.starts_with("EffiBuddy 崩溃报告"));
        assert!(report.contains("版本: "));
        assert!(report.contains("线程: main"));
        assert!(report.contains("消息: index out of bounds"));
        assert!(report.contains("   0: std::panicking::begin_panic"));
        assert!(report.contains(ISSUES_URL));
    }

    #[test]
    fn fields_parse_back_from_the_report() {
        let report = format_report("tool `x` panicked", None, "main", "", timestamp());
        assert_eq!(
            report_field(&report, LABEL_MESSAGE),
            Some("tool `x` panicked")
        );
        assert_eq!(report_field(&report, LABEL_THREAD), Some("main"));
        assert_eq!(report_field(&report, LABEL_LOCATION), Some("<unknown>"));
        assert!(
            report_field(&report, LABEL_VERSION)
                .is_some_and(|version| version.starts_with(crate::update::current_version()))
        );
    }

    #[test]
    fn multiline_messages_keep_the_label_on_one_line() {
        let report = format_report("first\nsecond", None, "main", "", timestamp());
        assert_eq!(report_field(&report, LABEL_MESSAGE), Some("first"));
        assert!(report.contains("first\n      second"));
    }

    #[test]
    fn display_text_truncates_on_char_boundary() {
        let short = "hello";
        assert_eq!(display_text(short).0.as_ref(), short);

        let long = "崩".repeat(MAX_DISPLAY_BYTES);
        let (display, truncated) = display_text(&long);
        assert!(truncated);
        assert!(display.contains("窗口只显示前 128 KiB"));
        assert!(display.len() < MAX_DISPLAY_BYTES + 120);
    }

    #[test]
    fn writes_and_prunes_old_reports() {
        let dir = tempfile::tempdir().expect("temp dir");
        for index in 0..(MAX_LOGS + 5) {
            let path = write_report(dir.path(), "report", &format!("stamp-{index:03}"), 1)
                .expect("write report");
            // Distinct mtimes so the prune order is deterministic.
            let stamp =
                std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(index as u64);
            let file = std::fs::File::options().write(true).open(&path).unwrap();
            file.set_modified(stamp).unwrap();
        }
        prune_logs(dir.path());
        let mut remaining: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        remaining.sort();
        assert_eq!(remaining.len(), MAX_LOGS);
        assert!(!remaining.contains(&"crash-stamp-000-1.log".to_string()));
        assert!(remaining.contains(&format!("crash-stamp-{:03}-1.log", MAX_LOGS + 4)));
    }
}
