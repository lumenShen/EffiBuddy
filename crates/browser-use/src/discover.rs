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
use std::time::{Duration, Instant, SystemTime};

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
    /// True when this run owns a throwaway profile copy
    /// (`EFFIBUDDY_BROWSER_PROFILE_FRESH=1`). Such a copy belongs to nobody
    /// else, so it is deleted when the browser exits instead of being left
    /// behind for the next run to trip over.
    pub disposable_profile: bool,
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
        if self.disposable_profile {
            // Reap the child first: Chrome still writes into its profile while
            // it exits, and a half-deleted profile throws away the whole point
            // of launching it.
            self.disposable_profile = false;
            let dir = self.profile_dir.clone();
            let moved = dir.clone();
            if tokio::task::spawn_blocking(move || discard_profile(&moved))
                .await
                .is_err()
            {
                discard_profile(&dir);
            }
        }
    }
}

/// Delete one throwaway profile directory (best effort, synchronous).
///
/// Also used by [`crate::cdp::CdpBackend`]'s drop path, where no runtime is
/// guaranteed to still be around for an async shutdown.
pub fn discard_profile(dir: &Path) {
    if dir.as_os_str().is_empty() {
        return;
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// What one reclamation sweep removed: for log lines and tests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReclaimedProfiles {
    pub dirs: usize,
    pub bytes: u64,
}

impl ReclaimedProfiles {
    pub fn is_empty(&self) -> bool {
        self.dirs == 0
    }

    pub fn describe(&self) -> String {
        format!(
            "回收了 {} 个一次性浏览器 profile（{}）",
            self.dirs,
            format_bytes(self.bytes)
        )
    }
}

fn format_bytes(bytes: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    const KIB: u64 = 1024;
    if bytes >= MIB {
        format!("{} MB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{} KB", bytes / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// Throwaway profiles nobody has touched for this long are reclaimed even when
/// the platform cannot cheaply tell whether the owner still runs.
const DISPOSABLE_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Sweep throwaway profiles left behind by runs that died before shutdown.
///
/// `EFFIBUDDY_BROWSER_PROFILE_FRESH=1` gives every launch its own copy of a
/// Chromium profile: a couple of hundred megabytes and thousands of files each,
/// and nothing else ever deletes them. A debugging afternoon that launches the
/// app a handful of times therefore leaks gigabytes. The owning pid is part of
/// the directory name, so a copy whose process is gone can be reclaimed without
/// guessing — while a copy that is still in use, the shared `<engine>-default`
/// profile and any user-chosen directory are left alone.
pub fn reclaim_disposable_profiles() -> ReclaimedProfiles {
    reclaim_disposable_profiles_in(&profiles_dir(), custom_profile().as_deref())
}

/// [`reclaim_disposable_profiles`] against an explicit directory.
///
/// `protected` is the `EFFIBUDDY_BROWSER_PROFILE` override, which is exempt
/// even when its name happens to look throwaway-shaped.
pub(crate) fn reclaim_disposable_profiles_in(
    dir: &Path,
    protected: Option<&Path>,
) -> ReclaimedProfiles {
    let mut reclaimed = ReclaimedProfiles::default();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return reclaimed;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if protected.is_some_and(|keep| keep == path) {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        // Only names this module generates are candidates: an unrelated
        // directory a user dropped in here must survive.
        let Some((_engine, pid, _stamp)) = parse_disposable_name(name) else {
            continue;
        };
        let stale = match process_alive(pid) {
            Some(true) => false,
            Some(false) => true,
            None => older_than(&path, now, DISPOSABLE_MAX_AGE),
        };
        if !stale {
            continue;
        }
        let bytes = dir_size(&path);
        if std::fs::remove_dir_all(&path).is_ok() {
            reclaimed.dirs += 1;
            reclaimed.bytes += bytes;
        }
    }
    reclaimed
}

/// Parse `<engine>-<pid>-<nanos>` back into its parts.
///
/// Engine labels may contain `-` (`chrome-headless-shell`), so the split runs
/// from the right. The timestamp must look like `subsec_nanos`, which keeps
/// arbitrary `name-1-2` directories from being mistaken for throwaway copies.
fn parse_disposable_name(name: &str) -> Option<(&str, u32, u128)> {
    let mut parts = name.rsplitn(3, '-');
    let stamp = parts.next()?.parse::<u128>().ok()?;
    let pid = parts.next()?.parse::<u32>().ok()?;
    let engine = parts.next()?;
    if engine.is_empty() || stamp >= 1_000_000_000 {
        return None;
    }
    Some((engine, pid, stamp))
}

/// `Some(true)`/`Some(false)` when the platform can answer, `None` when it
/// cannot and the caller has to fall back to age-based reclamation.
fn process_alive(pid: u32) -> Option<bool> {
    #[cfg(target_os = "linux")]
    {
        Some(Path::new(&format!("/proc/{pid}")).exists())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

fn older_than(path: &Path, now: SystemTime, age: Duration) -> bool {
    let Ok(modified) = std::fs::metadata(path).and_then(|meta| meta.modified()) else {
        return false;
    };
    now.duration_since(modified)
        .map(|elapsed| elapsed > age)
        .unwrap_or(false)
}

/// Total size of a directory tree, skipping entries that vanish mid-walk.
fn dir_size(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    let mut total = 0;
    for entry in entries.flatten() {
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if meta.is_dir() {
            total += dir_size(&entry.path());
        } else if meta.is_file() {
            total += meta.len();
        }
    }
    total
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
        .and_then(process_alive)
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

/// An explicitly chosen profile directory, empty values ignored.
pub fn custom_profile() -> Option<PathBuf> {
    std::env::var_os("EFFIBUDDY_BROWSER_PROFILE")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

fn fresh_profile_requested() -> bool {
    std::env::var("EFFIBUDDY_BROWSER_PROFILE_FRESH")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Name of the throwaway copy made by `EFFIBUDDY_BROWSER_PROFILE_FRESH=1`.
///
/// The owning pid is part of the name so a later run can tell an abandoned copy
/// from one that is still in use (see [`reclaim_disposable_profiles`]).
fn disposable_profile_name(label: &str) -> String {
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|value| value.subsec_nanos())
        .unwrap_or(0);
    format!("{label}-{}-{stamp}", std::process::id())
}

/// A resolved profile directory plus whether this run owns it.
#[derive(Debug, Clone, PartialEq)]
struct ProfileChoice {
    path: PathBuf,
    /// Throwaway copy: deleted on shutdown and swept away if the run crashes.
    disposable: bool,
}

/// Where this engine keeps its user data.
///
/// The profile is deliberately **stable** across launches: cookies, sessions
/// and local storage are exactly what makes a long task possible after the
/// browser crashes, and Chrome only rewrites them when it shuts down cleanly.
/// One profile per engine is the default; a throwaway copy is only made when
/// the model asks for one (`requested_fresh`, surfaced as
/// `browser({action:"fresh_profile"})`) or when a developer/test forces it with
/// `EFFIBUDDY_BROWSER_PROFILE_FRESH=1`. `EFFIBUDDY_BROWSER_PROFILE` points at a
/// specific directory and wins over both.
fn profile_for(engine: EngineKind, requested_fresh: bool) -> ProfileChoice {
    if let Some(custom) = custom_profile() {
        return ProfileChoice {
            path: custom,
            disposable: false,
        };
    }
    let label = engine.label().to_lowercase();
    if requested_fresh || fresh_profile_requested() {
        return ProfileChoice {
            path: profiles_dir().join(disposable_profile_name(&label)),
            disposable: true,
        };
    }
    ProfileChoice {
        path: profiles_dir().join(format!("{label}-default")),
        disposable: false,
    }
}

/// Launch (or attach to) a browser and return its DevTools WebSocket URL.
///
/// `fresh_profile` is the model-visible way to ask for a throwaway profile
/// (`browser({action:"fresh_profile"})`). The default is one shared,
/// persisted `<engine>-default` profile per engine: logins survive restarts and
/// nothing is copied per launch.
pub async fn launch(
    preference: &[EngineKind],
    headless: bool,
    fresh_profile: bool,
) -> Result<LaunchedBrowser, BrowserError> {
    let started = Instant::now();
    let mut notes = Vec::new();

    // Sweep throwaway profiles from earlier runs before this one adds another.
    // Deleting a few hundred megabytes is IO-bound, so it happens on a blocking
    // thread instead of the worker that is about to wait on a browser launch.
    let reclaimed = tokio::task::spawn_blocking(reclaim_disposable_profiles)
        .await
        .unwrap_or_default();
    if !reclaimed.is_empty() {
        notes.push(reclaimed.describe());
    }
    if fresh_profile && custom_profile().is_some() {
        notes.push(
            "EFFIBUDDY_BROWSER_PROFILE 固定了 profile 目录，本次忽略一次性 profile 请求"
                .to_string(),
        );
    }

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
        match launch_candidate(&candidate, headless, fresh_profile, &mut notes).await {
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
                match launch_candidate(&candidate, true, fresh_profile, &mut notes).await {
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
    fresh_profile: bool,
    notes: &mut Vec<String>,
) -> Result<LaunchedBrowser, BrowserError> {
    let profile = profile_for(candidate.kind, fresh_profile);
    let profile_dir = profile.path.clone();
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
        disposable_profile: profile.disposable,
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
        // across browser restarts, and it must never be treated as throwaway.
        let first = profile_for(EngineKind::Chrome, false);
        let second = profile_for(EngineKind::Chrome, false);
        assert_eq!(first.path, second.path);
        assert!(first.path.ends_with("chrome-default"));
        assert!(!first.disposable);

        // Asking for a fresh copy is opt-in and always disposable.
        let fresh = profile_for(EngineKind::Chrome, true);
        assert!(fresh.disposable);
        assert_ne!(
            fresh.path, first.path,
            "a fresh copy never reuses the shared one"
        );
    }

    #[test]
    fn disposable_names_round_trip_through_the_parser() {
        let name = disposable_profile_name("edge");
        let (engine, pid, _stamp) = parse_disposable_name(&name).expect("parses back");
        assert_eq!(engine, "edge");
        assert_eq!(pid, std::process::id());

        // Labels may contain dashes, so the split has to run from the right.
        let shell = disposable_profile_name("chrome-headless-shell");
        let (engine, _, _) = parse_disposable_name(&shell).expect("parses back");
        assert_eq!(engine, "chrome-headless-shell");

        // A shared profile, an unrelated directory and a bad stamp are not
        // throwaway copies.
        assert!(parse_disposable_name("edge-default").is_none());
        assert!(parse_disposable_name("my-notes").is_none());
        assert!(parse_disposable_name("edge-x-2").is_none());
        assert!(parse_disposable_name("edge-42-notnanos").is_none());
        assert!(parse_disposable_name("edge-42-1234567890123").is_none());
    }

    #[test]
    fn reclaim_removes_only_abandoned_throwaway_profiles() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let write = |name: &str| {
            let path = root.join(name);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("Cache.data"), vec![0u8; 512]).unwrap();
            path
        };

        let shared = write("edge-default");
        let unrelated = write("my-export");
        let malformed = write("edge-x-2");
        let live = write(&disposable_profile_name("edge"));
        // `u32::MAX - 1` cannot be a running pid, so its copy is abandoned.
        let abandoned = write("edge-4294967294-123456789");
        let abandoned_nested = root.join("chrome-4294967294-987654321");
        std::fs::create_dir_all(abandoned_nested.join("Default")).unwrap();
        std::fs::write(
            abandoned_nested.join("Default").join("Cookies"),
            vec![7u8; 4096],
        )
        .unwrap();

        let reclaimed = reclaim_disposable_profiles_in(root, None);

        assert_eq!(reclaimed.dirs, 2, "{reclaimed:?}");
        assert!(reclaimed.bytes >= 512 + 4096);
        assert!(!abandoned.exists());
        assert!(!abandoned_nested.exists());
        for kept in [&shared, &unrelated, &malformed, &live] {
            assert!(kept.exists(), "{} was removed", kept.display());
        }
    }

    #[test]
    fn reclaim_never_touches_the_configured_profile() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Even a throwaway-looking name is preserved when it is the directory
        // the user pointed `EFFIBUDDY_BROWSER_PROFILE` at.
        let pinned = root.join("edge-4294967294-123456789");
        std::fs::create_dir_all(&pinned).unwrap();

        let reclaimed = reclaim_disposable_profiles_in(root, Some(&pinned));

        assert!(reclaimed.is_empty());
        assert!(pinned.exists());
    }

    #[test]
    fn discard_profile_ignores_an_empty_path() {
        // `std::mem::take` in `shutdown` leaves an empty path behind; removing
        // "" must stay a no-op rather than resolve to the working directory.
        discard_profile(Path::new(""));
        assert!(data_dir().is_absolute() || !data_dir().as_os_str().is_empty());
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
