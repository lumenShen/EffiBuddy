//! File/link context menu for the message stream.
//!
//! Markdown links and inline images report a [`MarkdownTarget`] to the app;
//! this module turns that into a popover anchored at the click position. The
//! menu covers the actions people expect from a file reference: open (the
//! system default), open with a specific application, reveal in the file
//! manager, save a copy, copy absolute/relative paths, and stage the file as a
//! composer attachment for the next message.
//!
//! The renderer only needs the click position and the raw target, so the
//! markdown IR cache stays independent of the workspace. Path resolution and
//! metadata happen here, once per menu open.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use effibuddy_ui::{
    MarkdownTarget, Spinner, Theme, ease_out_cubic, icons, overlay_motion_duration, radius,
    shadow_float,
};
use gpui::{
    AnimationExt, AnyElement, ClipboardItem, Hsla, MouseButton, Pixels, Point, SharedString,
    Window, anchored, div, point, prelude::*, px, svg,
};

use crate::app::AppView;

/// Monotonic token for async "open with" scans. A menu close/reopen must not
/// let an old scan overwrite the new file's app list.
static OPEN_WITH_GENERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TargetKind {
    File,
    Dir,
    Missing,
}

#[derive(Clone, Debug)]
pub(crate) struct FileTarget {
    pub path: PathBuf,
    /// Workspace-relative form (`crates/app/src/app.rs`) when the file lives
    /// under the session cwd.
    pub relative: Option<String>,
    pub kind: TargetKind,
    pub size: u64,
    pub is_image: bool,
}

impl FileTarget {
    pub(crate) fn new(path: PathBuf, workspace: &Path) -> Self {
        let metadata = std::fs::metadata(&path).ok();
        let kind = match &metadata {
            Some(metadata) if metadata.is_dir() => TargetKind::Dir,
            Some(metadata) if metadata.is_file() => TargetKind::File,
            _ => TargetKind::Missing,
        };
        let relative = relative_display(&path, workspace);
        Self {
            is_image: kind == TargetKind::File && is_image_path(&path),
            size: metadata.map(|metadata| metadata.len()).unwrap_or_default(),
            path,
            relative,
            kind,
        }
    }

    fn name(&self) -> String {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| self.path.display().to_string())
    }

    fn display_path(&self) -> String {
        self.relative
            .clone()
            .unwrap_or_else(|| self.path.display().to_string())
    }

    fn subtitle(&self) -> String {
        let location = self.display_path();
        match self.kind {
            TargetKind::Dir => format!("文件夹 · {location}"),
            TargetKind::Missing => format!("文件不存在 · {location}"),
            TargetKind::File if self.is_image => {
                format!(
                    "图片 · {} · {location}",
                    crate::attachments::format_bytes(self.size)
                )
            }
            TargetKind::File => {
                format!(
                    "文件 · {} · {location}",
                    crate::attachments::format_bytes(self.size)
                )
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum LinkMenuTarget {
    File(FileTarget),
    Url { url: String },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum LinkMenuScreen {
    #[default]
    Main,
    OpenWith,
}

#[derive(Clone, Debug)]
enum LaunchKind {
    /// A freedesktop `Exec=` line with `%f`/`%U` field codes.
    Exec { line: String },
    /// A program that accepts the path as its final argument (macOS/Windows).
    #[allow(dead_code)]
    Program { program: String, args: Vec<String> },
    /// Windows' native “Open with…” picker.
    #[cfg(target_os = "windows")]
    OpenAsDialog,
}

#[derive(Clone, Debug)]
pub(crate) struct OpenWithApp {
    pub name: String,
    kind: LaunchKind,
}

#[derive(Default)]
pub(crate) struct LinkMenuState {
    pub target: Option<LinkMenuTarget>,
    pub position: Option<Point<Pixels>>,
    pub screen: LinkMenuScreen,
    pub apps: Vec<OpenWithApp>,
    pub apps_loading: bool,
    /// Bumped whenever the menu target changes: a late app scan for an old
    /// file must not populate the new file's list.
    apps_generation: u64,
}

impl LinkMenuState {
    fn open(&mut self, target: LinkMenuTarget, position: Point<Pixels>) {
        self.target = Some(target);
        self.position = Some(position);
        self.screen = LinkMenuScreen::Main;
        self.apps.clear();
        self.apps_loading = false;
    }
}

impl AppView {
    pub(crate) fn close_link_menu(&mut self, cx: &mut Context<Self>) {
        if self.link_menu.target.is_none() {
            return;
        }
        self.link_menu = LinkMenuState::default();
        cx.notify();
    }

    /// Entry point for clicks inside rendered markdown. Files, images and
    /// external URLs all open the same context menu for either mouse button.
    pub(crate) fn activate_markdown_target(
        &mut self,
        target: MarkdownTarget,
        _button: MouseButton,
        position: Point<Pixels>,
        base_dir: &Path,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match target {
            MarkdownTarget::Url(url) => {
                self.link_menu.open(
                    LinkMenuTarget::Url {
                        url: url.to_string(),
                    },
                    position,
                );
                cx.notify();
            }
            MarkdownTarget::File(raw) => {
                let roots = self.link_search_roots(base_dir);
                let path = resolve_file_reference_in(raw.as_ref(), &roots);
                let display_root = roots
                    .iter()
                    .find(|root| path.starts_with(root))
                    .map(PathBuf::as_path)
                    .unwrap_or(base_dir);
                let target = FileTarget::new(path, display_root);
                self.link_menu.open(LinkMenuTarget::File(target), position);
                cx.notify();
            }
        }
    }

    /// Session cwd plus the active project's other folders, for link
    /// resolution and for relative-path display.
    fn link_search_roots(&self, base_dir: &Path) -> Vec<PathBuf> {
        let mut roots = vec![base_dir.to_path_buf()];
        let Some(active) = &self.active else {
            return roots;
        };
        let session = effibuddy_agent_core::lock_session(&active.shared);
        let Some(project) = self
            .projects
            .project_for_session(session.project_id.as_deref(), session.project.as_deref())
        else {
            return roots;
        };
        drop(session);
        for root in project.root_paths() {
            if !roots.contains(&root) {
                roots.push(root);
            }
        }
        roots
    }

    pub(crate) fn render_link_menu(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let target = self.link_menu.target.clone()?;
        let position = self.link_menu.position?;
        let theme = self.theme.clone();
        let body = match &target {
            LinkMenuTarget::Url { url } => render_url_menu(&theme, url, cx),
            LinkMenuTarget::File(file) => match self.link_menu.screen {
                LinkMenuScreen::Main => self.render_file_menu_main(&theme, file, cx),
                LinkMenuScreen::OpenWith => self.render_open_with(&theme, file, cx),
            },
        };
        Some(
            div()
                .id("link-menu-backdrop")
                .absolute()
                .top(px(0.0))
                .left(px(0.0))
                .size_full()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.close_link_menu(cx);
                    }),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, _, _, cx| {
                        this.close_link_menu(cx);
                    }),
                )
                .child(
                    anchored()
                        .position(position)
                        .offset(point(px(2.0), px(2.0)))
                        .snap_to_window_with_margin(px(10.0))
                        .child(
                            div()
                                .id("link-menu")
                                .w(px(252.0))
                                .rounded(px(radius::MD))
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.elevated)
                                .shadow(shadow_float(&theme))
                                .p(px(4.0))
                                .flex()
                                .flex_col()
                                .gap(px(1.0))
                                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                    cx.stop_propagation();
                                })
                                .on_mouse_down(MouseButton::Right, |_, _, cx| {
                                    cx.stop_propagation();
                                })
                                .children(body)
                                .with_animation(
                                    "link-menu-enter",
                                    gpui::Animation::new(overlay_motion_duration())
                                        .with_easing(ease_out_cubic),
                                    |element, delta| {
                                        element.opacity(delta).mt(px(4.0 * (1.0 - delta)))
                                    },
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_file_menu_main(
        &self,
        theme: &Theme,
        file: &FileTarget,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut rows: Vec<AnyElement> = Vec::new();
        let exists = file.kind != TargetKind::Missing;
        let icon = if file.is_image {
            icons::IMAGE
        } else if file.kind == TargetKind::Dir {
            icons::FOLDER
        } else if exists {
            icons::FILE
        } else {
            icons::ALERT
        };
        let tint = if file.is_image {
            theme.agent_green
        } else if file.kind == TargetKind::Dir {
            theme.accent
        } else if exists {
            theme.text_muted
        } else {
            theme.danger
        };
        rows.push(render_menu_header(
            theme,
            icon,
            tint,
            &file.name(),
            &file.subtitle(),
            !exists,
        ));
        rows.push(render_menu_separator(theme));

        let open_label = if file.kind == TargetKind::Dir {
            "在文件管理器中打开"
        } else {
            "打开"
        };
        let target_for_open = file.clone();
        rows.push(
            menu_row(
                theme,
                SharedString::from("link-menu-open"),
                icons::EXTERNAL_LINK,
                open_label,
                exists,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if target_for_open.kind != TargetKind::Missing {
                    cx.open_with_system(&target_for_open.path);
                }
                this.close_link_menu(cx);
            }))
            .into_any_element(),
        );

        if file.kind == TargetKind::File {
            rows.push(
                menu_row(
                    theme,
                    SharedString::from("link-menu-open-with"),
                    icons::FILE,
                    "打开方式",
                    exists,
                )
                .child(
                    svg()
                        .path(icons::CHEVRON_RIGHT)
                        .size(px(11.0))
                        .text_color(theme.text_faint),
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.show_open_with(cx);
                }))
                .into_any_element(),
            );
        }

        if exists && file.kind != TargetKind::Dir {
            let target_for_reveal = file.clone();
            rows.push(
                menu_row(
                    theme,
                    SharedString::from("link-menu-reveal"),
                    icons::FOLDER,
                    "在文件管理器中显示",
                    true,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.reveal_path(&target_for_reveal.path);
                    this.close_link_menu(cx);
                }))
                .into_any_element(),
            );
        }

        if file.kind == TargetKind::File {
            let target_for_save = file.clone();
            rows.push(
                menu_row(
                    theme,
                    SharedString::from("link-menu-save-as"),
                    icons::DOWNLOAD,
                    "另存为…",
                    true,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.save_file_as(&target_for_save, cx);
                }))
                .into_any_element(),
            );
        }

        rows.push(render_menu_separator(theme));

        let absolute = file.path.display().to_string();
        rows.push(
            menu_row(
                theme,
                SharedString::from("link-menu-copy-abs"),
                icons::COPY,
                "复制绝对路径",
                true,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(absolute.clone()));
                this.notice = Some("已复制绝对路径。".to_string());
                this.close_link_menu(cx);
            }))
            .into_any_element(),
        );

        if let Some(relative) = file.relative.clone() {
            rows.push(
                menu_row(
                    theme,
                    SharedString::from("link-menu-copy-rel"),
                    icons::COPY,
                    "复制相对路径",
                    true,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(relative.clone()));
                    this.notice = Some("已复制相对路径。".to_string());
                    this.close_link_menu(cx);
                }))
                .into_any_element(),
            );
        }

        let target_for_quote = file.clone();
        let quote_row = menu_row(
            theme,
            SharedString::from("link-menu-quote"),
            icons::SPARKLES,
            "引用到对话",
            exists,
        );
        let quote_row = if exists {
            quote_row.on_click(cx.listener(move |this, _, window, cx| {
                this.quote_file(&target_for_quote, window, cx);
            }))
        } else {
            quote_row
        };
        rows.push(quote_row.into_any_element());
        rows
    }

    fn render_open_with(
        &self,
        theme: &Theme,
        file: &FileTarget,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut rows: Vec<AnyElement> = Vec::new();
        rows.push(
            menu_row(
                theme,
                SharedString::from("link-menu-open-with-back"),
                icons::CHEVRON_LEFT,
                "打开方式",
                true,
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.link_menu.screen = LinkMenuScreen::Main;
                cx.notify();
            }))
            .into_any_element(),
        );
        rows.push(render_menu_separator(theme));

        if self.link_menu.apps_loading {
            rows.push(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .h(px(30.0))
                    .px(px(7.0))
                    .text_size(px(11.5))
                    .text_color(theme.text_faint)
                    .child(Spinner::new(theme.text_faint, 12.0))
                    .child("正在查找应用…")
                    .into_any_element(),
            );
            return rows;
        }

        if self.link_menu.apps.is_empty() {
            rows.push(
                div()
                    .px(px(7.0))
                    .py(px(8.0))
                    .text_size(px(11.0))
                    .line_height(px(16.0))
                    .text_color(theme.text_faint)
                    .child("没有找到能打开这种文件的应用。")
                    .into_any_element(),
            );
            return rows;
        }

        let path = file.path.clone();
        let app_icon = if file.is_image {
            icons::IMAGE
        } else {
            icons::FILE
        };
        for (index, app) in self.link_menu.apps.iter().enumerate() {
            let app = app.clone();
            let path = path.clone();
            rows.push(
                menu_row(
                    theme,
                    SharedString::from(format!("link-menu-app-{index}")),
                    app_icon,
                    app.name.clone(),
                    true,
                )
                .on_click(
                    cx.listener(move |this, _, _, cx| match launch_with_app(&app, &path) {
                        Ok(()) => {
                            this.close_link_menu(cx);
                        }
                        Err(error) => {
                            this.last_error = Some(error);
                            this.link_menu = LinkMenuState::default();
                            cx.notify();
                        }
                    }),
                )
                .into_any_element(),
            );
        }
        rows
    }

    pub(crate) fn show_open_with(&mut self, cx: &mut Context<Self>) {
        let Some(LinkMenuTarget::File(file)) = self.link_menu.target.as_ref() else {
            return;
        };
        let path = file.path.clone();
        self.link_menu.screen = LinkMenuScreen::OpenWith;
        self.link_menu.apps.clear();
        self.link_menu.apps_loading = true;
        let generation = OPEN_WITH_GENERATION.fetch_add(1, Ordering::Relaxed);
        self.link_menu.apps_generation = generation;
        cx.notify();
        cx.spawn(async move |weak, cx| {
            let apps = cx
                .background_executor()
                .spawn(async move { list_open_with_apps(&path) })
                .await;
            weak.update(cx, |this, cx| {
                if this.link_menu.apps_generation != generation {
                    return;
                }
                this.link_menu.apps = apps;
                this.link_menu.apps_loading = false;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn save_file_as(&mut self, file: &FileTarget, cx: &mut Context<Self>) {
        let source = file.path.clone();
        let directory = source
            .parent()
            .map(Path::to_path_buf)
            .or_else(dirs::download_dir)
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| self.active_workspace());
        let suggested = source
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".to_string());
        let receiver = cx.prompt_for_new_path(&directory, Some(&suggested));
        self.link_menu = LinkMenuState::default();
        self.notice = Some("选择另存位置…".to_string());
        cx.notify();
        cx.spawn(async move |weak, cx| {
            let path = match receiver.await {
                Ok(Ok(Some(path))) => path,
                Ok(Ok(None)) => {
                    let _ = weak.update(cx, |this, cx| {
                        if this.notice.as_deref() == Some("选择另存位置…") {
                            this.notice = None;
                        }
                        cx.notify();
                    });
                    return;
                }
                Ok(Err(error)) => {
                    let _ = weak.update(cx, |this, cx| {
                        this.notice = None;
                        this.last_error = Some(format!("无法打开保存对话框：{error}"));
                        cx.notify();
                    });
                    return;
                }
                Err(_) => {
                    let _ = weak.update(cx, |this, cx| {
                        this.notice = None;
                        cx.notify();
                    });
                    return;
                }
            };
            if same_file(&source, &path) {
                let _ = weak.update(cx, |this, cx| {
                    this.notice = Some("源文件和目标位置相同，未复制。".to_string());
                    cx.notify();
                });
                return;
            }
            let result = cx
                .background_executor()
                .spawn({
                    let source = source.clone();
                    let path = path.clone();
                    async move { std::fs::copy(&source, &path) }
                })
                .await;
            let _ = weak.update(cx, |this, cx| {
                match result {
                    Ok(_) => {
                        this.notice = Some(format!("已另存为 {}", path.display()));
                    }
                    Err(error) => {
                        this.notice = None;
                        this.last_error = Some(format!("另存失败：{error}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn quote_file(&mut self, file: &FileTarget, window: &mut Window, cx: &mut Context<Self>) {
        self.link_menu = LinkMenuState::default();
        if file.kind == TargetKind::Dir {
            let reference = file
                .relative
                .clone()
                .unwrap_or_else(|| file.path.display().to_string());
            let insertion = format!("@{reference} ");
            let cursor = self.composer_state.read(cx).cursor_offset();
            self.composer_state.update(cx, |state, cx| {
                state.replace_byte_range(cursor..cursor, &insertion, window, cx);
            });
            self.notice = Some("已在输入框引用该文件夹。".to_string());
        } else {
            self.attach_paths(vec![file.path.clone()], cx);
        }
        self.composer_focus_handle(cx).focus(window);
        cx.notify();
    }
}

fn render_url_menu(theme: &Theme, url: &str, cx: &mut Context<AppView>) -> Vec<AnyElement> {
    let host = url
        .split("://")
        .nth(1)
        .unwrap_or(url)
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(url)
        .to_string();
    let mut rows = vec![
        render_menu_header(theme, icons::EXTERNAL_LINK, theme.accent, &host, url, false),
        render_menu_separator(theme),
    ];
    let url_for_open = url.to_string();
    rows.push(
        menu_row(
            theme,
            SharedString::from("link-menu-url-open"),
            icons::EXTERNAL_LINK,
            "打开链接",
            true,
        )
        .on_click(cx.listener(move |this, _, _, cx| {
            cx.open_url(&url_for_open);
            this.close_link_menu(cx);
        }))
        .into_any_element(),
    );
    let url_for_copy = url.to_string();
    rows.push(
        menu_row(
            theme,
            SharedString::from("link-menu-url-copy"),
            icons::COPY,
            "复制链接",
            true,
        )
        .on_click(cx.listener(move |this, _, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(url_for_copy.clone()));
            this.notice = Some("已复制链接。".to_string());
            this.close_link_menu(cx);
        }))
        .into_any_element(),
    );
    rows
}

fn render_menu_header(
    theme: &Theme,
    icon: &'static str,
    tint: Hsla,
    name: &str,
    subtitle: &str,
    danger: bool,
) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .px(px(7.0))
        .py(px(6.0))
        .child(
            div()
                .flex_none()
                .size(px(24.0))
                .rounded(px(6.0))
                .bg(tint.opacity(0.14))
                .flex()
                .items_center()
                .justify_center()
                .child(svg().path(icon).size(px(13.0)).text_color(tint)),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .flex_col()
                .gap(px(1.0))
                .child(
                    div()
                        .truncate()
                        .text_size(px(12.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme.text)
                        .child(name.to_string()),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(px(9.5))
                        .text_color(if danger {
                            theme.danger
                        } else {
                            theme.text_faint
                        })
                        .child(subtitle.to_string()),
                ),
        )
        .into_any_element()
}

fn render_menu_separator(theme: &Theme) -> AnyElement {
    div()
        .h(px(1.0))
        .w_full()
        .bg(theme.border)
        .my(px(2.0))
        .into_any_element()
}

fn menu_row(
    theme: &Theme,
    id: SharedString,
    icon: &'static str,
    label: impl Into<String>,
    enabled: bool,
) -> gpui::Stateful<gpui::Div> {
    let label = label.into();
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(8.0))
        .h(px(30.0))
        .px(px(7.0))
        .rounded(px(radius::XS))
        .when(enabled, |element| {
            element
                .cursor_pointer()
                .hover(|style| style.bg(theme.hover))
        })
        .child(svg().path(icon).size(px(12.0)).text_color(if enabled {
            theme.text_muted
        } else {
            theme.text_faint
        }))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(12.0))
                .text_color(if enabled {
                    theme.text
                } else {
                    theme.text_faint
                })
                .child(label),
        )
}

/// Resolve a click target against the session cwd. Handles `file://`, `~`,
/// percent-escaped spaces, `#fragment` suffixes and `path:line:column` forms;
/// line suffixes only win when the full token does not exist.
pub(crate) fn resolve_file_reference(raw: &str, workspace: &Path) -> PathBuf {
    resolve_file_reference_in(raw, &[workspace.to_path_buf()])
}

/// Resolve against the session cwd first, then the project's other folders.
/// Multi-root projects can reference the same relative path from any folder.
pub(crate) fn resolve_file_reference_in(raw: &str, roots: &[PathBuf]) -> PathBuf {
    let raw = raw.trim();
    let raw = raw.strip_prefix('@').unwrap_or(raw);
    let raw = raw.strip_prefix("file://").unwrap_or(raw);
    // Windows file URLs arrive as `/C:/dir/file`.
    #[cfg(target_os = "windows")]
    let raw = raw
        .strip_prefix('/')
        .filter(|rest| rest.as_bytes().get(1) == Some(&b':'))
        .unwrap_or(raw);
    let decoded = percent_decode(raw);
    let decoded = decoded.trim_start_matches('<').trim_end_matches('>');
    let without_fragment = decoded.split(['#', '?']).next().unwrap_or(decoded);
    let fallback = roots.first().cloned().unwrap_or_else(|| PathBuf::from("."));
    let explicit = Path::new(without_fragment).is_absolute() || without_fragment.starts_with('~');
    if explicit {
        let direct = absolute_path(without_fragment, &fallback);
        if direct.exists() {
            return direct;
        }
        if let Some(prefix) = strip_line_suffix(without_fragment) {
            let candidate = absolute_path(prefix, &fallback);
            if candidate.exists() {
                return candidate;
            }
        }
        return direct;
    }
    for root in roots {
        let direct = root.join(without_fragment);
        if direct.exists() {
            return direct;
        }
        if let Some(prefix) = strip_line_suffix(without_fragment) {
            let candidate = root.join(prefix);
            if candidate.exists() {
                return candidate;
            }
        }
    }
    fallback.join(without_fragment)
}

fn absolute_path(raw: &str, workspace: &Path) -> PathBuf {
    let raw = raw.trim();
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    if raw == "~" {
        return dirs::home_dir().unwrap_or_else(|| workspace.to_path_buf());
    }
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        path
    } else {
        workspace.join(path)
    }
}

fn strip_line_suffix(token: &str) -> Option<&str> {
    let index = token.rfind(':')?;
    let suffix = &token[index + 1..];
    if suffix.is_empty() || !suffix.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    let head = &token[..index];
    if let Some(second) = head.rfind(':') {
        let line = &head[second + 1..];
        if !line.is_empty() && line.chars().all(|ch| ch.is_ascii_digit()) {
            return Some(&head[..second]);
        }
    }
    Some(head)
}

fn relative_display(path: &Path, workspace: &Path) -> Option<String> {
    let relative = path.strip_prefix(workspace).ok()?;
    let relative = relative.to_string_lossy().replace('\\', "/");
    if relative.is_empty() {
        Some(".".to_string())
    } else {
        Some(relative)
    }
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
                out.push(high * 16 + low);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn same_file(left: &Path, right: &Path) -> bool {
    let left = std::fs::canonicalize(left).unwrap_or_else(|_| left.to_path_buf());
    let right = std::fs::canonicalize(right).unwrap_or_else(|_| right.to_path_buf());
    left == right
}

const IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "avif", "tif", "tiff", "svg",
];

fn is_image_path(path: &Path) -> bool {
    path.extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
        .is_some_and(|extension| IMAGE_EXTENSIONS.contains(&extension.as_str()))
}

fn mime_for_path(path: &Path) -> String {
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        "avif" => "image/avif",
        "tif" | "tiff" => "image/tiff",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "rs" | "py" | "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx" | "go" | "java" | "c" | "h"
        | "cc" | "cpp" | "hpp" | "sh" | "bash" | "zsh" | "rb" | "php" | "lua" | "kt" | "swift"
        | "cs" | "fs" | "scala" | "dart" | "ex" | "exs" | "erl" | "hs" | "clj" | "sql" | "css"
        | "scss" | "html" | "htm" | "xml" | "toml" | "yaml" | "yml" | "ini" | "cfg" | "conf"
        | "txt" | "log" | "md" | "markdown" | "csv" | "tsv" | "json" | "jsonc" => {
            if matches!(extension.as_str(), "md" | "markdown") {
                "text/markdown"
            } else if matches!(extension.as_str(), "html" | "htm") {
                "text/html"
            } else if matches!(extension.as_str(), "css" | "scss") {
                "text/css"
            } else if matches!(
                extension.as_str(),
                "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx"
            ) {
                "text/javascript"
            } else {
                "text/plain"
            }
        }
        _ => "application/octet-stream",
    }
    .to_string()
}

#[cfg(target_os = "linux")]
fn list_open_with_apps(path: &Path) -> Vec<OpenWithApp> {
    let mime = mime_for_path(path);
    let mut apps: Vec<OpenWithApp> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for directory in desktop_dirs() {
        collect_desktop_apps(&directory, &directory, &mime, &mut apps, &mut seen, 0);
    }
    apps.sort_by(|left, right| left.name.to_lowercase().cmp(&right.name.to_lowercase()));
    apps.truncate(8);
    apps
}

#[cfg(target_os = "linux")]
fn desktop_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(data_home) = std::env::var_os("XDG_DATA_HOME") {
        dirs.push(PathBuf::from(data_home).join("applications"));
    } else if let Some(data_dir) = dirs::data_dir() {
        dirs.push(data_dir.join("applications"));
    }
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
    for directory in data_dirs.split(':').filter(|entry| !entry.is_empty()) {
        dirs.push(PathBuf::from(directory).join("applications"));
    }
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".local/share/flatpak/exports/share/applications"));
    }
    dirs.push(PathBuf::from("/var/lib/flatpak/exports/share/applications"));
    dirs.push(PathBuf::from("/var/lib/snapd/desktop/applications"));
    dirs
}

#[cfg(target_os = "linux")]
fn collect_desktop_apps(
    root: &Path,
    directory: &Path,
    mime: &str,
    apps: &mut Vec<OpenWithApp>,
    seen: &mut std::collections::HashSet<String>,
    depth: usize,
) {
    if depth > 3 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_desktop_apps(root, &path, mime, apps, seen, depth + 1);
            continue;
        }
        if path.extension().and_then(|extension| extension.to_str()) != Some("desktop") {
            continue;
        }
        let Some(entry) = parse_desktop_entry(&path, root) else {
            continue;
        };
        if !mime_compatible(&entry.mimes, mime) && !category_compatible(&entry.categories, mime) {
            continue;
        }
        if !seen.insert(entry.id.to_lowercase()) {
            continue;
        }
        apps.push(OpenWithApp {
            name: entry.name,
            kind: LaunchKind::Exec { line: entry.exec },
        });
    }
}

#[cfg(target_os = "linux")]
struct DesktopEntry {
    id: String,
    name: String,
    exec: String,
    mimes: Vec<String>,
    categories: Vec<String>,
}

#[cfg(target_os = "linux")]
fn parse_desktop_entry(path: &Path, root: &Path) -> Option<DesktopEntry> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut in_main = false;
    let mut name = None;
    let mut localized_name = None;
    let mut exec = None;
    let mut mimes = Vec::new();
    let mut categories = Vec::new();
    let mut hidden = false;
    let mut no_display = false;
    let mut terminal = false;
    let mut application = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_main = line == "[Desktop Entry]";
            continue;
        }
        if !in_main || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "Name" => name = Some(unescape_desktop(value)),
            "Name[zh_CN]" | "Name[zh]" => localized_name = Some(unescape_desktop(value)),
            "Exec" => exec = Some(unescape_desktop(value)),
            "MimeType" => {
                mimes = value
                    .split(';')
                    .map(str::trim)
                    .filter(|entry| !entry.is_empty())
                    .map(ToString::to_string)
                    .collect();
            }
            "Categories" => {
                categories = value
                    .split(';')
                    .map(str::trim)
                    .filter(|entry| !entry.is_empty())
                    .map(ToString::to_string)
                    .collect();
            }
            "Hidden" => hidden = value.eq_ignore_ascii_case("true"),
            "NoDisplay" => no_display = value.eq_ignore_ascii_case("true"),
            "Terminal" => terminal = value.eq_ignore_ascii_case("true"),
            "Type" => application = value == "Application",
            _ => {}
        }
    }
    if hidden || no_display || terminal || !application {
        return None;
    }
    let relative = path.strip_prefix(root).ok()?;
    let id = relative
        .to_string_lossy()
        .trim_end_matches(".desktop")
        .replace('/', "-");
    Some(DesktopEntry {
        id,
        name: localized_name.or(name)?,
        exec: exec?,
        mimes,
        categories,
    })
}

#[cfg(target_os = "linux")]
fn unescape_desktop(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('s') => out.push(' '),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('\\') => out.push('\\'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

#[cfg(target_os = "linux")]
fn mime_compatible(declared: &[String], mime: &str) -> bool {
    let category = mime.split('/').next().unwrap_or_default();
    let category_wildcard = format!("{category}/*");
    declared.iter().any(|entry| {
        entry == mime
            || entry == &category_wildcard
            || (category == "text" && matches!(entry.as_str(), "text/plain" | "text/*"))
            || (category == "image" && entry == "image/*")
    })
}

#[cfg(target_os = "linux")]
fn category_compatible(categories: &[String], mime: &str) -> bool {
    let category = mime.split('/').next().unwrap_or_default();
    let texty = category == "text"
        || matches!(
            mime,
            "application/json" | "application/toml" | "application/yaml" | "application/xml"
        );
    texty
        && categories
            .iter()
            .any(|category| matches!(category.as_str(), "TextEditor" | "IDE"))
}

#[cfg(target_os = "windows")]
fn list_open_with_apps(path: &Path) -> Vec<OpenWithApp> {
    let mut apps = Vec::new();
    if is_image_path(path) {
        apps.push(OpenWithApp {
            name: "画图".to_string(),
            kind: LaunchKind::Program {
                program: "mspaint.exe".to_string(),
                args: Vec::new(),
            },
        });
    }
    apps.push(OpenWithApp {
        name: "记事本".to_string(),
        kind: LaunchKind::Program {
            program: "notepad.exe".to_string(),
            args: Vec::new(),
        },
    });
    apps.push(OpenWithApp {
        name: "选择其他应用…".to_string(),
        kind: LaunchKind::OpenAsDialog,
    });
    apps
}

#[cfg(target_os = "macos")]
fn list_open_with_apps(path: &Path) -> Vec<OpenWithApp> {
    let mut apps = Vec::new();
    if is_image_path(path) {
        apps.push(OpenWithApp {
            name: "预览".to_string(),
            kind: LaunchKind::Program {
                program: "open".to_string(),
                args: vec!["-a".to_string(), "Preview".to_string()],
            },
        });
    }
    apps.push(OpenWithApp {
        name: "文本编辑".to_string(),
        kind: LaunchKind::Program {
            program: "open".to_string(),
            args: vec!["-a".to_string(), "TextEdit".to_string()],
        },
    });
    apps
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
fn list_open_with_apps(_path: &Path) -> Vec<OpenWithApp> {
    Vec::new()
}

fn launch_with_app(app: &OpenWithApp, path: &Path) -> Result<(), String> {
    let mut command = match &app.kind {
        LaunchKind::Exec { line } => {
            let argv = exec_argv(line, path);
            let Some((program, args)) = argv.split_first() else {
                return Err(format!("「{}」没有可执行的启动命令。", app.name));
            };
            let mut command = Command::new(program);
            command.args(args);
            command
        }
        LaunchKind::Program { program, args } => {
            let mut command = Command::new(program);
            command.args(args).arg(path);
            command
        }
        #[cfg(target_os = "windows")]
        LaunchKind::OpenAsDialog => {
            let mut command = Command::new("rundll32.exe");
            let path = path.to_string_lossy().into_owned();
            command.args(["shell32.dll,OpenAs_RunDLL", path.as_str()]);
            command
        }
    };
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("无法用「{}」打开：{error}", app.name))
}

/// Split a freedesktop Exec line and substitute `%f`/`%F`/`%u`/`%U` with the
/// target path. Other field codes (`%i`, `%c`, …) are removed; `%%` becomes a
/// literal percent. The result is passed directly to `Command`, never a shell.
fn exec_argv(line: &str, path: &Path) -> Vec<String> {
    let tokens = split_exec_line(line);
    let path = path.to_string_lossy();
    let mut argv = Vec::new();
    let mut substituted = false;
    for token in tokens {
        let mut out = String::with_capacity(token.len());
        let mut chars = token.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '%' {
                match chars.next() {
                    Some('f' | 'F' | 'u' | 'U') => {
                        out.push_str(&path);
                        substituted = true;
                    }
                    Some('%') => out.push('%'),
                    Some(_) => {}
                    None => out.push('%'),
                }
            } else {
                out.push(ch);
            }
        }
        if !out.is_empty() {
            argv.push(out);
        }
    }
    if !substituted {
        argv.push(path.to_string());
    }
    argv
}

fn split_exec_line(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        match quote {
            Some(delimiter) => {
                if ch == delimiter {
                    quote = None;
                } else if ch == '\\' && delimiter == '"' {
                    if let Some(next) = chars.next() {
                        current.push(next);
                    }
                } else {
                    current.push(ch);
                }
            }
            None => match ch {
                '"' | '\'' => quote = Some(ch),
                '\\' => {
                    if let Some(next) = chars.next() {
                        current.push(next);
                    }
                }
                ch if ch.is_whitespace() => {
                    if !current.is_empty() {
                        tokens.push(std::mem::take(&mut current));
                    }
                }
                _ => current.push(ch),
            },
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_line_substitutes_path_and_drops_unused_codes() {
        let path = Path::new("/tmp/hello world.md");
        assert_eq!(
            exec_argv(r#"gedit --new-window %U"#, path),
            vec!["gedit", "--new-window", "/tmp/hello world.md"]
        );
        assert_eq!(
            exec_argv(r#"env FOO=1 code %F"#, path),
            vec!["env", "FOO=1", "code", "/tmp/hello world.md"]
        );
        assert_eq!(
            exec_argv(r#"app --icon %i %c %% done"#, path),
            vec!["app", "--icon", "%", "done", "/tmp/hello world.md"]
        );
    }

    #[test]
    fn quoted_exec_arguments_stay_one_token() {
        assert_eq!(
            split_exec_line(r#"/usr/bin/app --title "Hello World" file%U"#),
            vec!["/usr/bin/app", "--title", "Hello World", "file%U"]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn desktop_app_discovery_parses_system_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        std::fs::write(&path, "# hi\n").unwrap();
        let apps = list_open_with_apps(&path);
        assert!(apps.iter().all(|app| !app.name.is_empty()));
    }

    #[test]
    fn file_references_resolve_relative_paths_and_line_suffixes() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::fs::write(workspace.join("src/main.rs"), "fn main() {}").unwrap();
        assert_eq!(
            resolve_file_reference("src/main.rs", workspace),
            workspace.join("src/main.rs")
        );
        assert_eq!(
            resolve_file_reference("src/main.rs:12", workspace),
            workspace.join("src/main.rs")
        );
        assert_eq!(
            resolve_file_reference("file://%2Ftmp%2Fhello.md", Path::new("/")),
            PathBuf::from("/tmp/hello.md")
        );

        let second = tempfile::tempdir().unwrap();
        let second_root = second.path();
        std::fs::create_dir_all(second_root.join("src")).unwrap();
        std::fs::write(second_root.join("src/lib.rs"), "pub fn lib() {}").unwrap();
        let roots = vec![workspace.to_path_buf(), second_root.to_path_buf()];
        assert_eq!(
            resolve_file_reference_in("src/lib.rs", &roots),
            second_root.join("src/lib.rs")
        );
    }
}
