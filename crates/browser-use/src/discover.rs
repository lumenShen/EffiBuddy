//! Engine discovery, launching and DevTools endpoint attachment.
//!
//! Priority on every platform:
//!
//! 1. An installed Chrome / Edge / Chromium (Windows ships Edge, so this almost
//!    always succeeds there).
//! 2. `chrome-headless-shell` in the data directory, installed on first use.
//!
//! We never ship a browser inside the app bundle: the RPM is 15 MB today and a
//! bundled Chromium would push it past 300 MB plus sandbox packaging problems.
//! Users can also point `EFFIBUDDY_BROWSER_PATH` at any Chromium build.
//!
//! `--no-sandbox` is deliberately never passed: an AI-driven browser without a
//! sandbox is exactly the risk this feature has to avoid.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::types::{BrowserError, EngineKind};

/// How long a fresh browser gets to publish its DevTools endpoint.
pub const LAUNCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Overall budget for trying every candidate plus the fallback shell.
pub const LAUNCH_TOTAL_TIMEOUT: Duration = Duration::from_secs(60);

/// One discovered executable.
#[derive(Debug, Clone, PartialEq)]
pub struct EngineCandidate {
    pub kind: EngineKind,
    pub path: PathBuf,
    /// `false` for user-provided paths we have not inspected yet.
    pub installed: bool,
}

impl EngineCandidate {
    pub fn new(kind: EngineKind, path: impl Into<PathBuf>, installed: bool) -> Self {
        Self {
            kind,
            path: path.into(),
            installed,
        }
    }
}

/// Environment override, empty strings ignored.
pub fn engine_override() -> Option<PathBuf> {
    std::env::var_os("EFFIBUDDY_BROWSER_PATH")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

/// Preferred engine order for this platform.
pub fn default_engine_preference() -> Vec<EngineKind> {
    if cfg!(target_os = "windows") {
        vec![EngineKind::Edge, EngineKind::Chrome, EngineKind::Chromium]
    } else {
        vec![EngineKind::Chrome, EngineKind::Edge, EngineKind::Chromium]
    }
}

/// Where downloaded engines and profiles live.
pub fn data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("effibuddy")
        .join("browser")
}

pub fn profiles_dir() -> PathBuf {
    data_dir().join("profiles")
}

/// Executable name candidates per engine on this platform.
fn executable_names(kind: EngineKind) -> &'static [&'static str] {
    match kind {
        EngineKind::Chrome => {
            if cfg!(target_os = "windows") {
                &["chrome.exe"]
            } else {
                &["google-chrome", "google-chrome-stable", "chrome"]
            }
        }
        EngineKind::Edge => {
            if cfg!(target_os = "windows") {
                &["msedge.exe"]
            } else {
                &["microsoft-edge", "microsoft-edge-stable"]
            }
        }
        EngineKind::Chromium => {
            if cfg!(target_os = "windows") {
                &["chrome.exe", "chromium.exe"]
            } else {
                &["chromium", "chromium-browser"]
            }
        }
        EngineKind::HeadlessShell => &["chrome-headless-shell"],
    }
}

/// Well-known install directories, in addition to `PATH`.
fn install_dirs(kind: EngineKind) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    #[cfg(target_os = "windows")]
    {
        for base in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
            let Some(root) = std::env::var_os(base) else {
                continue;
            };
            let root = PathBuf::from(root);
            match kind {
                EngineKind::Edge => {
                    dirs.push(root.join("Microsoft").join("Edge").join("Application"))
                }
                EngineKind::Chrome => {
                    dirs.push(root.join("Google").join("Chrome").join("Application"))
                }
                EngineKind::Chromium => dirs.push(root.join("Chromium").join("Application")),
                EngineKind::HeadlessShell => {}
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        match kind {
            EngineKind::Chrome => dirs.push(PathBuf::from("/opt/google/chrome")),
            EngineKind::Edge => dirs.push(PathBuf::from("/opt/microsoft/msedge")),
            EngineKind::Chromium => {
                dirs.push(PathBuf::from("/usr/lib/chromium"));
            }
            EngineKind::HeadlessShell => {}
        }
    }
    #[cfg(target_os = "macos")]
    {
        match kind {
            EngineKind::Chrome => dirs.push(PathBuf::from(
                "/Applications/Google Chrome.app/Contents/MacOS",
            )),
            EngineKind::Edge => dirs.push(PathBuf::from(
                "/Applications/Microsoft Edge.app/Contents/MacOS",
            )),
            EngineKind::Chromium => {
                dirs.push(PathBuf::from("/Applications/Chromium.app/Contents/MacOS"))
            }
            EngineKind::HeadlessShell => {}
        }
    }
    dirs
}

fn find_in_path(names: &[&str]) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for name in names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Candidates in preference order. Cheap: filesystem lookups only.
pub fn discover_engines() -> Vec<EngineCandidate> {
    if let Some(override_path) = engine_override()
        && override_path.is_file()
    {
        return vec![EngineCandidate::new(
            EngineKind::Chromium,
            override_path,
            true,
        )];
    }

    let mut found: Vec<EngineCandidate> = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();

    for kind in default_engine_preference() {
        let mut push = |path: PathBuf, found: &mut Vec<EngineCandidate>| {
            if seen.contains(&path) {
                return;
            }
            seen.push(path.clone());
            found.push(EngineCandidate::new(kind, path, true));
        };

        if let Some(path) = find_in_path(executable_names(kind)) {
            push(path, &mut found);
        }
        for dir in install_dirs(kind) {
            for name in executable_names(kind) {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    push(candidate, &mut found);
                }
            }
        }
    }

    found
}

/// Sub-path of the headless shell inside its download directory, per platform.
fn headless_shell_relative() -> &'static str {
    if cfg!(target_os = "windows") {
        "chrome-headless-shell-win64/chrome-headless-shell.exe"
    } else if cfg!(target_os = "macos") {
        "chrome-headless-shell-mac-x64/chrome-headless-shell"
    } else {
        "chrome-headless-shell-linux64/chrome-headless-shell"
    }
}

pub fn headless_shell_path() -> PathBuf {
    data_dir().join(headless_shell_relative())
}

pub fn headless_shell_available() -> bool {
    headless_shell_path().is_file()
}

/// Resolve the fallback headless shell.
///
/// Automatic download is intentionally out of scope for this milestone. The URL
/// shape is known
/// (`https://storage.googleapis.com/chrome-for-testing-public/<version>/linux64/chrome-headless-shell-linux64.zip`),
/// and adding it later is a self-contained change: download, verify SHA-256,
/// unzip into [`data_dir`].
pub fn ensure_headless_shell() -> Result<PathBuf, BrowserError> {
    let path = headless_shell_path();
    if path.is_file() {
        return Ok(path);
    }
    Err(BrowserError::NoEngine(format!(
        "未找到可用的浏览器：请安装 Chrome 或 Edge，或设置 EFFIBUDDY_BROWSER_PATH。\
         也可以从 https://googlechromelabs.github.io/chrome-for-testing/ 手动下载 \
         chrome-headless-shell 后放到 {}",
        data_dir().display()
    )))
}

/// A running browser process plus its debug endpoint.
#[derive(Debug)]
pub struct LaunchedBrowser {
    pub engine: EngineCandidate,
    pub child: Option<tokio::process::Child>,
    pub debug_url: String,
    pub port: u16,
    pub profile_dir: PathBuf,
    pub headless: bool,
    pub notes: Vec<String>,
}

impl LaunchedBrowser {
    /// Terminate the browser if we started it. Best effort: an error here must
    /// not mask the real failure the caller is reporting. The owner is
    /// responsible for calling this; there is deliberately no `Drop` impl so an
    /// explicit shutdown cannot race with one.
    pub async fn shutdown(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }
}

/// Chrome writes this into a fresh profile; a stale one blocks startup.
fn clear_stale_profile_lock(profile_dir: &Path, notes: &mut Vec<String>) {
    let lock = profile_dir.join("SingletonLock");
    if !lock.exists() {
        return;
    }
    // On Linux the lock is a symlink whose target embeds `<hostname>-<pid>`.
    let alive = std::fs::read_link(&lock)
        .ok()
        .and_then(|target| {
            target
                .to_str()
                .and_then(|text| text.rsplit('-').next())
                .and_then(|pid| pid.parse::<u32>().ok())
        })
        .map(|pid| Path::new(&format!("/proc/{pid}")).exists())
        .unwrap_or(false);
    if alive {
        notes.push(format!(
            "profile {} 仍被另一个浏览器实例占用",
            profile_dir.display()
        ));
        return;
    }
    for name in ["SingletonLock", "SingletonCookie", "SingletonSocket"] {
        let _ = std::fs::remove_file(profile_dir.join(name));
    }
    notes.push("清理了上次遗留的 profile 锁".to_string());
}

/// Where this engine keeps its user data.
///
/// The profile is deliberately **stable** across launches: cookies, sessions
/// and local storage are exactly what makes a long task possible after the
/// browser crashes, and Chrome only rewrites them when it shuts down cleanly.
/// `EFFIBUDDY_BROWSER_PROFILE` points at a specific directory;
/// `EFFIBUDDY_BROWSER_PROFILE_FRESH=1` restores the old throwaway behaviour,
/// which is what tests and "log out everything" runs want.
fn profile_for(engine: EngineKind) -> PathBuf {
    if let Some(custom) = std::env::var_os("EFFIBUDDY_BROWSER_PROFILE")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
    {
        return custom;
    }
    let label = engine.label().to_lowercase();
    let fresh = std::env::var("EFFIBUDDY_BROWSER_PROFILE_FRESH")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if fresh {
        let stamp = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.subsec_nanos())
                .unwrap_or(0)
        );
        return profiles_dir().join(format!("{label}-{stamp}"));
    }
    profiles_dir().join(format!("{label}-default"))
}

/// Launch (or attach to) a browser and return its DevTools WebSocket URL.
pub async fn launch(
    preference: &[EngineKind],
    headless: bool,
) -> Result<LaunchedBrowser, BrowserError> {
    let started = Instant::now();
    let mut notes = Vec::new();

    let mut candidates: Vec<EngineCandidate> = discover_engines()
        .into_iter()
        .filter(|candidate| preference.is_empty() || preference.contains(&candidate.kind))
        .collect();
    // A usable browser beats a strict preference.
    if candidates.is_empty() {
        candidates = discover_engines();
    }
    if candidates.is_empty() {
        notes.push("没有找到已安装的 Chrome/Edge/Chromium".to_string());
    }

    let mut last_error: Option<BrowserError> = None;
    for candidate in candidates {
        if started.elapsed() > LAUNCH_TOTAL_TIMEOUT {
            notes.push("启动总超时".to_string());
            break;
        }
        match launch_candidate(&candidate, headless, &mut notes).await {
            Ok(mut browser) => {
                browser.notes.append(&mut notes);
                return Ok(browser);
            }
            Err(error) => {
                notes.push(format!("{} 启动失败：{error}", candidate.path.display()));
                last_error = Some(error);
            }
        }
    }

    if headless {
        match ensure_headless_shell() {
            Ok(path) => {
                let candidate = EngineCandidate::new(EngineKind::HeadlessShell, path, true);
                match launch_candidate(&candidate, true, &mut notes).await {
                    Ok(mut browser) => {
                        browser.notes.append(&mut notes);
                        return Ok(browser);
                    }
                    Err(error) => {
                        notes.push(format!("chrome-headless-shell 启动失败：{error}"));
                        last_error = Some(error);
                    }
                }
            }
            Err(error) => notes.push(error.to_string()),
        }
    }

    Err(match last_error {
        Some(error) => error,
        None => BrowserError::NoEngine(notes.join("; ")),
    })
}

async fn launch_candidate(
    candidate: &EngineCandidate,
    headless: bool,
    notes: &mut Vec<String>,
) -> Result<LaunchedBrowser, BrowserError> {
    let profile_dir = profile_for(candidate.kind);
    std::fs::create_dir_all(&profile_dir)?;
    clear_stale_profile_lock(&profile_dir, notes);

    let _ = std::fs::create_dir_all(data_dir());
    let log_path = data_dir().join("launch.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .ok();

    let mut command = tokio::process::Command::new(&candidate.path);
    command
        .arg("--remote-debugging-port=0")
        .arg(format!("--user-data-dir={}", profile_dir.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-features=Translate,MediaRouter")
        .arg("--disable-background-networking")
        .arg("--disable-sync")
        .arg("--disable-default-apps")
        .arg("--window-size=1280,800")
        .arg("about:blank")
        // Never `--no-sandbox`: that would turn an AI-driven browser into an
        // unsandboxed one.
        .stdout(Stdio::null())
        .stderr(match log {
            Some(log) => Stdio::from(log),
            None => Stdio::null(),
        })
        .stdin(Stdio::null());

    if headless {
        command.arg("--headless=new");
    }
    if candidate.kind == EngineKind::HeadlessShell {
        command.arg("--hide-scrollbars");
    }

    let child = command
        .spawn()
        .map_err(|error| BrowserError::Launch(format!("{}: {error}", candidate.path.display())))?;

    let (port, ws_path) =
        match read_active_port(&profile_dir, LAUNCH_TIMEOUT.as_millis() as u64).await {
            Ok(value) => value,
            Err(error) => {
                let mut child = child;
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err(error);
            }
        };

    let base_url = format!("http://127.0.0.1:{port}");
    let version = probe_version(&base_url).await.unwrap_or_default();
    let debug_url = format!("ws://127.0.0.1:{port}{ws_path}");

    let kind = engine_from_version(&version).unwrap_or(candidate.kind);
    let mut engine = candidate.clone();
    engine.kind = kind;

    let mut browser = LaunchedBrowser {
        engine,
        child: Some(child),
        debug_url,
        port,
        profile_dir,
        headless,
        notes: Vec::new(),
    };
    if !version.is_empty() {
        browser.notes.push(format!("browser: {version}"));
    }
    trim_log(&log_path);
    Ok(browser)
}

/// Map a `/json/version` `Browser` string onto an engine kind.
pub fn engine_from_version(version: &str) -> Option<EngineKind> {
    if version.starts_with("Edg/") || version.starts_with("Edge/") {
        Some(EngineKind::Edge)
    } else if version.starts_with("HeadlessChrome/") {
        Some(EngineKind::HeadlessShell)
    } else if version.starts_with("Chrome/") || version.starts_with("Chromium/") {
        Some(EngineKind::Chrome)
    } else {
        None
    }
}

/// Keep the launch log bounded; a runaway browser can log for hours.
fn trim_log(path: &Path) {
    const LIMIT: u64 = 200 * 1024;
    if let Ok(metadata) = std::fs::metadata(path)
        && metadata.len() > LIMIT
    {
        let _ = std::fs::write(path, b"");
    }
}

/// Read `<profile>/DevToolsActivePort` (`port\n/browser/<id>`), retrying until
/// the browser writes it.
pub async fn read_active_port(
    profile_dir: &Path,
    timeout_ms: u64,
) -> Result<(u16, String), BrowserError> {
    let path = profile_dir.join("DevToolsActivePort");
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if let Ok(contents) = std::fs::read_to_string(&path) {
            let mut lines = contents.lines();
            let port = lines
                .next()
                .and_then(|line| line.trim().parse::<u16>().ok());
            let path_line = lines
                .next()
                .map(str::trim)
                .filter(|line| line.starts_with("/devtools/"));
            if let (Some(port), Some(path_line)) = (port, path_line) {
                return Ok((port, path_line.to_string()));
            }
        }
        if Instant::now() >= deadline {
            return Err(BrowserError::Launch(format!(
                "browser did not expose a devtools port in {timeout_ms}ms"
            )));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Ask the endpoint for `/json/version` to confirm it is a Chromium DevTools
/// server and to record the version string.
pub async fn probe_version(base_url: &str) -> Result<String, BrowserError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|error| BrowserError::Protocol(error.to_string()))?;
    let response = client
        .get(format!("{base_url}/json/version"))
        .send()
        .await
        .map_err(|error| BrowserError::Protocol(format!("version probe failed: {error}")))?;
    if !response.status().is_success() {
        return Err(BrowserError::Protocol(format!(
            "version probe returned HTTP {}",
            response.status()
        )));
    }
    let value: serde_json::Value = response
        .json()
        .await
        .map_err(|error| BrowserError::Protocol(error.to_string()))?;
    value
        .get("Browser")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            BrowserError::Protocol("version probe reply had no `Browser` field".to_string())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preference_prefers_edge_on_windows_only() {
        let preference = default_engine_preference();
        if cfg!(target_os = "windows") {
            assert_eq!(preference.first(), Some(&EngineKind::Edge));
        } else {
            assert_eq!(preference.first(), Some(&EngineKind::Chrome));
        }
        assert!(!preference.contains(&EngineKind::HeadlessShell));
    }

    #[test]
    fn version_strings_map_onto_engines() {
        assert_eq!(
            engine_from_version("Edg/131.0.2903.86"),
            Some(EngineKind::Edge)
        );
        assert_eq!(
            engine_from_version("HeadlessChrome/131.0.6778.85"),
            Some(EngineKind::HeadlessShell)
        );
        assert_eq!(
            engine_from_version("Chrome/131.0.6778.85"),
            Some(EngineKind::Chrome)
        );
        assert_eq!(engine_from_version("Firefox/131.0"), None);
    }

    #[test]
    fn discovery_only_reports_existing_files() {
        for candidate in discover_engines() {
            assert!(
                candidate.path.is_file(),
                "{:?} does not exist",
                candidate.path
            );
        }
    }

    #[test]
    fn data_dir_is_under_the_app_namespace() {
        let dir = data_dir();
        assert!(dir.ends_with("effibuddy/browser") || dir.ends_with("effibuddy\\browser"));
        assert!(profiles_dir().ends_with("profiles"));

        // The default profile is stable: that is what keeps logins alive
        // across browser restarts.
        let first = profile_for(EngineKind::Chrome);
        let second = profile_for(EngineKind::Chrome);
        assert_eq!(first, second);
        assert!(first.ends_with("chrome-default"));
    }

    #[test]
    fn headless_shell_path_mentions_the_platform_directory() {
        let text = headless_shell_path().to_string_lossy().to_string();
        assert!(text.contains("chrome-headless-shell-"), "{text}");
    }

    #[tokio::test]
    async fn read_active_port_times_out_with_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let error = read_active_port(dir.path(), 300)
            .await
            .expect_err("must fail");
        match error {
            BrowserError::Launch(message) => assert!(message.contains("devtools port")),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_active_port_parses_both_lines() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("DevToolsActivePort"),
            "51234\n/devtools/browser/abc-123\n",
        )
        .unwrap();
        let (port, path) = read_active_port(dir.path(), 1000).await.unwrap();
        assert_eq!(port, 51234);
        assert_eq!(path, "/devtools/browser/abc-123");
    }

    #[tokio::test]
    async fn read_active_port_retries_while_the_file_is_incomplete() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("DevToolsActivePort");
        // A partially written file must not be accepted as-is.
        std::fs::write(&path, "51234\n").unwrap();
        let writer = {
            let path = path.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(250)).await;
                std::fs::write(&path, "51234\n/devtools/browser/xyz\n").unwrap();
            })
        };
        let (port, ws) = read_active_port(dir.path(), 3000).await.unwrap();
        writer.await.unwrap();
        assert_eq!(port, 51234);
        assert_eq!(ws, "/devtools/browser/xyz");
    }
}
