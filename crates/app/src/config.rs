//! Application configuration and provider construction.

use std::path::{Path, PathBuf};

use effibuddy_agent_core::tool::PermissionMode;
use effibuddy_computer::ComputerUseMode;
use serde::{Deserialize, Serialize};

pub const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";
pub const DEFAULT_MODEL: &str = "deepseek-flash";

/// Upper bound of the command palette's most-recently-used list.
pub const PALETTE_RECENT_LIMIT: usize = 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_model")]
    pub model: String,
    /// Force the offline demo provider even when a key exists.
    #[serde(default)]
    pub use_demo: bool,
    /// Legacy flag from the old two-state approval switch. Kept so existing
    /// config files keep parsing; see [`AppConfig::effective_default_mode`].
    #[serde(default)]
    pub auto_approve_tools: bool,
    /// Permission mode new sessions start in. `None` until the user picks one
    /// explicitly, so legacy `auto_approve_tools` configs still migrate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_permission_mode: Option<PermissionMode>,
    /// Whether the model may read the screen and simulate keyboard/mouse.
    /// `session` asks once per conversation; `always` permits new
    /// conversations up front; `off` hides the computer tool entirely.
    #[serde(default)]
    pub computer_use_mode: ComputerUseMode,
    /// Most recently used command palette item ids (commands and sessions) in
    /// MRU order; the palette pins matching entries to the top.
    #[serde(default)]
    pub palette_recent: Vec<String>,
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(default)]
    pub workspace: Option<PathBuf>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f32>,
    /// Right side pane visibility and geometry. Persisted app-wide, like
    /// ZCode's side pane layout, because tabs are part of the shell chrome.
    #[serde(default = "default_side_bar_open")]
    pub side_bar_open: bool,
    #[serde(default = "default_side_bar_width")]
    pub side_bar_width: f32,
    #[serde(default = "default_side_bar_tabs")]
    pub side_bar_tabs: Vec<String>,
    #[serde(default = "default_side_bar_active")]
    pub side_bar_active: String,
    /// Left sidebar collapsed into a narrow icon rail. Persisted app-wide so
    /// the shell restores the layout the user left it in.
    #[serde(default)]
    pub sidebar_collapsed: bool,
    /// Collapsed groups inside the session sidebar: `pinned`, `projects`,
    /// `tasks`, or `project:<path>` for a single project.
    #[serde(default)]
    pub sidebar_collapsed_sections: Vec<String>,
    /// Workspaces registered from the sidebar's "添加工作区" button. Projects
    /// that already own sessions are derived from those sessions; this list
    /// keeps empty workspaces visible and persists them across restarts.
    #[serde(default)]
    pub projects: Vec<PathBuf>,
    /// Whether the app queries GitHub Releases for a newer version at startup
    /// (see `update.rs`). The About page offers a manual check regardless.
    #[serde(default = "default_true")]
    pub update_check_enabled: bool,
    /// Release tag the user chose to skip; the update banner stays hidden for
    /// that version until a newer one is published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_skip_version: Option<String>,
}

fn default_true() -> bool {
    true
}

fn default_side_bar_open() -> bool {
    true
}

fn default_side_bar_width() -> f32 {
    320.0
}

fn default_side_bar_tabs() -> Vec<String> {
    vec!["plan".to_string(), "agents".to_string()]
}

fn default_side_bar_active() -> String {
    "plan".to_string()
}

fn default_base_url() -> String {
    DEFAULT_BASE_URL.to_string()
}

fn default_model() -> String {
    DEFAULT_MODEL.to_string()
}

fn default_theme() -> String {
    "light".to_string()
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            base_url: default_base_url(),
            api_key: String::new(),
            model: default_model(),
            use_demo: false,
            auto_approve_tools: false,
            default_permission_mode: None,
            computer_use_mode: ComputerUseMode::Session,
            palette_recent: Vec::new(),
            theme: default_theme(),
            workspace: None,
            max_tokens: Some(8192),
            temperature: Some(0.2),
            side_bar_open: default_side_bar_open(),
            side_bar_width: default_side_bar_width(),
            side_bar_tabs: default_side_bar_tabs(),
            side_bar_active: default_side_bar_active(),
            sidebar_collapsed: false,
            sidebar_collapsed_sections: Vec::new(),
            projects: Vec::new(),
            update_check_enabled: true,
            update_skip_version: None,
        }
    }
}

impl AppConfig {
    pub fn path() -> PathBuf {
        base_dir().join("config.json")
    }

    pub fn load() -> Self {
        let mut config = std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|contents| serde_json::from_str::<AppConfig>(&contents).ok())
            .unwrap_or_default();

        if config.workspace.is_none() {
            config.workspace = std::env::current_dir().ok();
        }
        if config.api_key.trim().is_empty() {
            if let Some(key) = discover_api_key(config.workspace.as_deref()) {
                config.api_key = key;
            }
        }
        config.api_key = config.api_key.trim().to_string();
        config.base_url = config.base_url.trim().trim_end_matches('/').to_string();
        if config.base_url.is_empty() {
            config.base_url = default_base_url();
        }
        if config.model.trim().is_empty() {
            config.model = default_model();
        }
        config.side_bar_width = config.side_bar_width.clamp(260.0, 620.0);
        config
    }

    pub fn save(&self) -> Result<(), String> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create config directory: {error}"))?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|error| format!("cannot serialize config: {error}"))?;
        std::fs::write(&path, json)
            .map_err(|error| format!("cannot write {}: {error}", path.display()))
    }

    pub fn workspace(&self) -> PathBuf {
        self.workspace
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// The permission mode new sessions start in. An explicit
    /// `default_permission_mode` wins; otherwise the legacy
    /// `auto_approve_tools` flag migrates (true → full access, false → ask).
    pub fn effective_default_mode(&self) -> PermissionMode {
        self.default_permission_mode
            .unwrap_or(if self.auto_approve_tools {
                PermissionMode::FullAccess
            } else {
                PermissionMode::Default
            })
    }

    /// Record a command palette selection as most recently used: dedupe, move
    /// to the front and cap the list so it never grows unbounded.
    pub fn record_palette_use(&mut self, id: &str) {
        self.palette_recent.retain(|entry| entry != id);
        self.palette_recent.insert(0, id.to_string());
        self.palette_recent.truncate(PALETTE_RECENT_LIMIT);
    }

    /// Instructions from `AGENTS.md` / `EFFIBUDDY.md` in the workspace.
    pub fn project_instructions(&self) -> String {
        let workspace = self.workspace();
        let mut parts = Vec::new();
        for name in ["AGENTS.md", "EFFIBUDDY.md", "CLAUDE.md"] {
            let path = workspace.join(name);
            if let Ok(contents) = std::fs::read_to_string(&path) {
                let contents = contents.trim();
                if contents.is_empty() {
                    continue;
                }
                let contents: String = contents.chars().take(12_000).collect();
                parts.push(format!("## {name}\n{contents}"));
            }
        }
        parts.join("\n\n")
    }
}

/// 应用数据目录名：debug 构建（`cargo run` / `cargo test` 默认配置）使用
/// Dev 后缀，release 构建使用正式名，使开发调试数据与真实用户数据互不干扰。
pub fn app_dir_name() -> &'static str {
    if cfg!(debug_assertions) {
        "EffiBuddy-Dev"
    } else {
        "EffiBuddy"
    }
}

pub fn base_dir() -> PathBuf {
    // 显式覆盖：CI、多实例隔离或性能实验时把数据目录整体重定向。
    if let Ok(dir) = std::env::var("EFFIBUDDY_DATA_DIR") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    dirs::data_local_dir()
        .or_else(dirs::config_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(app_dir_name())
}

fn discover_api_key(workspace: Option<&Path>) -> Option<String> {
    if let Ok(key) = std::env::var("DEEPSEEK_API_KEY") {
        let key = key.trim().to_string();
        if !key.is_empty() {
            return Some(key);
        }
    }
    if let Ok(key) = std::env::var("EFFIBUDDY_API_KEY") {
        let key = key.trim().to_string();
        if !key.is_empty() {
            return Some(key);
        }
    }

    let mut candidates = Vec::new();
    if let Some(workspace) = workspace {
        candidates.push(workspace.join("deepseekAPIKey.env"));
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("deepseekAPIKey.env"));
    }
    candidates.push(base_dir().join("deepseekAPIKey.env"));

    for path in candidates {
        if let Ok(contents) = std::fs::read_to_string(&path) {
            let contents = contents.trim();
            let key = contents
                .split_once('=')
                .map(|(_, value)| value.trim())
                .unwrap_or(contents)
                .trim()
                .trim_matches('"')
                .trim_matches('\'');
            if !key.is_empty() {
                return Some(key.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_default_mode_migrates_legacy_flag() {
        let mut config = AppConfig::default();
        assert_eq!(config.effective_default_mode(), PermissionMode::Default);

        config.auto_approve_tools = true;
        assert_eq!(config.effective_default_mode(), PermissionMode::FullAccess);

        // An explicit mode always wins over the legacy flag.
        config.default_permission_mode = Some(PermissionMode::Plan);
        assert_eq!(config.effective_default_mode(), PermissionMode::Plan);
    }

    #[test]
    fn legacy_config_json_still_parses() {
        let legacy = serde_json::json!({
            "base_url": "https://api.deepseek.com",
            "api_key": "k",
            "model": "deepseek-flash",
            "auto_approve_tools": true,
            "theme": "light"
        });
        let config: AppConfig = serde_json::from_value(legacy).unwrap();
        assert!(config.auto_approve_tools);
        assert_eq!(config.default_permission_mode, None);
        assert!(config.palette_recent.is_empty());
        assert!(!config.sidebar_collapsed);
        assert_eq!(config.effective_default_mode(), PermissionMode::FullAccess);
    }

    #[test]
    fn sidebar_collapsed_roundtrips() {
        assert!(!AppConfig::default().sidebar_collapsed);

        let mut config = AppConfig::default();
        config.sidebar_collapsed = true;
        let text = serde_json::to_string(&config).unwrap();
        assert!(text.contains("\"sidebar_collapsed\":true"));
        let restored: AppConfig = serde_json::from_str(&text).unwrap();
        assert!(restored.sidebar_collapsed);
    }

    #[test]
    fn update_fields_roundtrip_and_default() {
        // Old config files without the keys fall back to enabled + no skip.
        let legacy: AppConfig =
            serde_json::from_value(serde_json::json!({"theme": "dark"})).unwrap();
        assert!(legacy.update_check_enabled);
        assert_eq!(legacy.update_skip_version, None);

        let mut config = AppConfig::default();
        config.update_check_enabled = false;
        config.update_skip_version = Some("v0.2.0".to_string());
        let text = serde_json::to_string(&config).unwrap();
        assert!(text.contains("\"update_check_enabled\":false"));
        assert!(text.contains("\"update_skip_version\":\"v0.2.0\""));
        let restored: AppConfig = serde_json::from_str(&text).unwrap();
        assert!(!restored.update_check_enabled);
        assert_eq!(restored.update_skip_version.as_deref(), Some("v0.2.0"));

        // Skipped version stays out of the file while unset.
        let plain = serde_json::to_string(&AppConfig::default()).unwrap();
        assert!(!plain.contains("update_skip_version"));
    }

    #[test]
    fn default_permission_mode_serializes_snake_case() {
        let mut config = AppConfig::default();
        config.default_permission_mode = Some(PermissionMode::AcceptEdits);
        let text = serde_json::to_string(&config).unwrap();
        assert!(text.contains("\"default_permission_mode\":\"accept_edits\""));
        let restored: AppConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(
            restored.default_permission_mode,
            Some(PermissionMode::AcceptEdits)
        );

        // Unset mode stays out of the file so legacy parsing is unaffected.
        let plain = serde_json::to_string(&AppConfig::default()).unwrap();
        assert!(!plain.contains("default_permission_mode"));
    }

    #[test]
    fn record_palette_use_is_mru_and_capped() {
        let mut config = AppConfig::default();
        for index in 0..(PALETTE_RECENT_LIMIT + 5) {
            config.record_palette_use(&format!("cmd-{index}"));
        }
        assert_eq!(config.palette_recent.len(), PALETTE_RECENT_LIMIT);
        assert_eq!(
            config.palette_recent[0],
            format!("cmd-{}", PALETTE_RECENT_LIMIT + 4)
        );

        config.record_palette_use("cmd-10");
        assert_eq!(config.palette_recent[0], "cmd-10");
        assert_eq!(
            config
                .palette_recent
                .iter()
                .filter(|id| *id == "cmd-10")
                .count(),
            1,
            "re-selecting an entry must move it, not duplicate it",
        );
    }

    #[test]
    fn app_dir_name_follows_build_profile() {
        // Locks the dev/prod separation contract: debug builds (including this
        // test run unless executed with --release) isolate into EffiBuddy-Dev.
        if cfg!(debug_assertions) {
            assert_eq!(app_dir_name(), "EffiBuddy-Dev");
        } else {
            assert_eq!(app_dir_name(), "EffiBuddy");
        }
    }

    #[test]
    fn computer_use_mode_defaults_to_session_and_roundtrips() {
        let config = AppConfig::default();
        assert_eq!(config.computer_use_mode, ComputerUseMode::Session);

        let mut json = serde_json::to_value(&config).unwrap();
        assert_eq!(json["computer_use_mode"], "session");
        json["computer_use_mode"] = serde_json::json!("always");
        let parsed: AppConfig = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.computer_use_mode, ComputerUseMode::Always);

        // Old config files without the key fall back to Session.
        let parsed: AppConfig =
            serde_json::from_value(serde_json::json!({"theme": "dark"})).unwrap();
        assert_eq!(parsed.computer_use_mode, ComputerUseMode::Session);
    }

    #[test]
    fn effibuddy_data_dir_env_overrides_base_dir() {
        // The only test touching this variable, so no cross-test races.
        let override_dir = std::env::temp_dir().join(format!(
            "effibuddy-data-dir-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // SAFETY: single-threaded use of this variable within the test suite.
        unsafe { std::env::set_var("EFFIBUDDY_DATA_DIR", &override_dir) };
        assert_eq!(base_dir(), override_dir);

        // An empty value must fall through to the normal resolution.
        unsafe { std::env::set_var("EFFIBUDDY_DATA_DIR", "   ") };
        assert_ne!(base_dir(), PathBuf::from("   "));
        assert!(base_dir().ends_with(app_dir_name()));

        unsafe { std::env::remove_var("EFFIBUDDY_DATA_DIR") };
        assert!(base_dir().ends_with(app_dir_name()));
        let _ = std::fs::remove_dir_all(&override_dir);
    }
}
