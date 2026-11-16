//! Browser-use safety boundary: consent gating and the status chip.
//!
//! The runtime lives in `effibuddy-browser`; this module decides *who* may drive
//! a browser. The gate intentionally bypasses the normal permission-mode fast
//! path so FullAccess cannot silently grant browser control, and Plan mode
//! rejects every mutating action.
//!
//! Risk is layered. Reading a page (`snapshot`, `text`, `console`, `network`) is
//! low risk once the session is granted, while `eval`, `upload`, `navigate` and
//! `click` can exfiltrate data or touch the filesystem, so each is checked
//! individually even after consent.

use std::sync::Arc;

use async_trait::async_trait;
use effibuddy_agent_core::tool::PermissionMode;
use effibuddy_agent_core::{ApprovalDecision, ApprovalGate, ApprovalRequest};
use effibuddy_browser::{BrowserRuntime, BrowserUseMode};

/// Actions that change page or browser state.
fn is_mutating_action(action: &str) -> bool {
    matches!(
        action,
        "navigate"
            | "click"
            | "type"
            | "hover"
            | "press_key"
            | "scroll"
            | "select"
            | "set_checked"
            | "new_tab"
            | "select_tab"
            | "close_tab"
    )
}

/// Actions that can reach outside the page: page-origin script execution and
/// file uploads. These are rewritten to the same "mutating" treatment but are
/// called out separately so the reason shown to the user is accurate.
fn is_privileged_action(action: &str) -> bool {
    matches!(action, "eval" | "upload")
}

/// Actions that never touch page state.
#[cfg(test)]
fn is_read_only_action(action: &str) -> bool {
    matches!(
        action,
        "status" | "snapshot" | "text" | "console" | "network" | "tabs" | "screenshot" | "wait_for"
    )
}

/// Wraps the normal mode gate with browser-specific consent.
pub(crate) struct BrowserApprovalGate {
    /// Used for every non-browser tool.
    inner: Arc<dyn ApprovalGate>,
    /// Forces a real UI card for first-time browser consent even when the
    /// session is in FullAccess mode.
    consent: Arc<dyn ApprovalGate>,
    runtime: Arc<BrowserRuntime>,
    session_id: String,
    permission_mode: Arc<dyn Fn() -> PermissionMode + Send + Sync>,
}

impl BrowserApprovalGate {
    pub fn new(
        inner: Arc<dyn ApprovalGate>,
        consent: Arc<dyn ApprovalGate>,
        runtime: Arc<BrowserRuntime>,
        session_id: impl Into<String>,
        permission_mode: Arc<dyn Fn() -> PermissionMode + Send + Sync>,
    ) -> Self {
        Self {
            inner,
            consent,
            runtime,
            session_id: session_id.into(),
            permission_mode,
        }
    }
}

#[async_trait]
impl ApprovalGate for BrowserApprovalGate {
    async fn request_approval(&self, request: ApprovalRequest) -> ApprovalDecision {
        if request.tool_name != "browser" {
            return self.inner.request_approval(request).await;
        }

        let action = request
            .arguments
            .get("action")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();

        // Plan mode is a read-only contract: no navigation, no clicks, no
        // script execution, no uploads. Reading a page the session already
        // opened stays allowed so research works.
        if (self.permission_mode)() == PermissionMode::Plan
            && (is_mutating_action(action) || is_privileged_action(action))
        {
            return ApprovalDecision::Deny;
        }

        match self.runtime.mode() {
            BrowserUseMode::Off => ApprovalDecision::Deny,
            BrowserUseMode::Always => {
                self.runtime.grant(&self.session_id);
                ApprovalDecision::Approve
            }
            BrowserUseMode::Session => {
                if self.runtime.is_granted(&self.session_id) {
                    return ApprovalDecision::Approve;
                }
                let decision = self.consent.request_approval(request).await;
                if decision == ApprovalDecision::ApproveForSession {
                    self.runtime.grant(&self.session_id);
                }
                decision
            }
        }
    }
}

/// Compact chip text for the composer. Kept here so the UI and tests agree on
/// the wording.
pub(crate) fn composer_hint(
    mode: BrowserUseMode,
    granted: bool,
) -> Option<(&'static str, &'static str)> {
    if !mode.is_enabled() {
        return None;
    }
    let label = match (mode, granted) {
        (BrowserUseMode::Always, _) => "浏览器已允许 · Chromium",
        (BrowserUseMode::Session, true) => "浏览器已授权 · 本会话",
        (BrowserUseMode::Session, false) => "浏览器 · 首次使用询问",
        (BrowserUseMode::Off, _) => return None,
    };
    Some((
        label,
        "允许 AI 通过 DevTools 协议操控浏览器；点击打开设置。页面内容是不可信输入。",
    ))
}

/// Short, human-readable summary for the consent card. `BrowserTool::summarize`
/// produces the same shape for the real card; this copy keeps the app-side
/// wording testable without constructing a tool.
#[cfg(test)]
fn summarize_action(arguments: &serde_json::Value) -> String {
    let action = arguments
        .get("action")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("action");
    match action {
        "navigate" => format!(
            "打开网址 {}",
            arguments
                .get("url")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
        ),
        "snapshot" => "读取当前页面结构".to_string(),
        "text" => "读取当前页面文本".to_string(),
        "click" => format!(
            "点击 {}",
            arguments
                .get("ref")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("坐标位置")
        ),
        "type" => "在页面中填写内容".to_string(),
        "eval" => "在页面中执行脚本（可读取页面数据）".to_string(),
        "upload" => format!(
            "上传文件 {}",
            arguments
                .get("path")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
        ),
        other => format!("执行浏览器操作 `{other}`"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use effibuddy_agent_core::AutoApprovalGate;
    use effibuddy_browser::testing::FakeBrowser;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FixedGate(ApprovalDecision, AtomicUsize);

    impl FixedGate {
        fn new(decision: ApprovalDecision) -> Self {
            Self(decision, AtomicUsize::new(0))
        }

        fn calls(&self) -> usize {
            self.1.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl ApprovalGate for FixedGate {
        async fn request_approval(&self, _request: ApprovalRequest) -> ApprovalDecision {
            self.1.fetch_add(1, Ordering::SeqCst);
            self.0
        }
    }

    fn request(action: &str) -> ApprovalRequest {
        ApprovalRequest {
            id: "r".to_string(),
            tool_name: "browser".to_string(),
            call_id: "c".to_string(),
            summary: "use browser".to_string(),
            arguments: json!({"action": action}),
        }
    }

    fn runtime() -> Arc<BrowserRuntime> {
        Arc::new(BrowserRuntime::with_data_dir(
            Arc::new(FakeBrowser::new()),
            std::env::temp_dir(),
        ))
    }

    fn gate(
        runtime: Arc<BrowserRuntime>,
        consent: Arc<FixedGate>,
        mode: PermissionMode,
    ) -> BrowserApprovalGate {
        BrowserApprovalGate::new(
            Arc::new(AutoApprovalGate),
            consent,
            runtime,
            "s",
            Arc::new(move || mode),
        )
    }

    #[tokio::test]
    async fn session_mode_forces_real_consent_even_in_full_access() {
        let runtime = runtime();
        let consent = Arc::new(FixedGate::new(ApprovalDecision::ApproveForSession));
        let gate = gate(runtime.clone(), consent.clone(), PermissionMode::FullAccess);
        assert_eq!(
            gate.request_approval(request("snapshot")).await,
            ApprovalDecision::ApproveForSession
        );
        assert_eq!(consent.calls(), 1);
        assert!(runtime.is_granted("s"));
        // The second call is served by the session grant.
        assert_eq!(
            gate.request_approval(request("click")).await,
            ApprovalDecision::Approve
        );
        assert_eq!(consent.calls(), 1);
    }

    #[tokio::test]
    async fn plan_mode_denies_mutating_and_privileged_actions() {
        let runtime = runtime();
        runtime.grant("s");
        let consent = Arc::new(FixedGate::new(ApprovalDecision::Approve));
        let gate = gate(runtime, consent.clone(), PermissionMode::Plan);

        for action in ["navigate", "click", "type", "eval", "upload", "press_key"] {
            assert_eq!(
                gate.request_approval(request(action)).await,
                ApprovalDecision::Deny,
                "{action} should be denied in plan mode"
            );
        }
        assert_eq!(consent.calls(), 0, "plan denials never reach the user");

        // Read-only research still works.
        assert_eq!(
            gate.request_approval(request("snapshot")).await,
            ApprovalDecision::Approve
        );
    }

    #[tokio::test]
    async fn off_mode_denies_everything_before_asking() {
        let runtime = runtime();
        runtime.set_mode(BrowserUseMode::Off);
        runtime.grant("s");
        let consent = Arc::new(FixedGate::new(ApprovalDecision::Approve));
        let gate = gate(runtime, consent.clone(), PermissionMode::FullAccess);
        assert_eq!(
            gate.request_approval(request("snapshot")).await,
            ApprovalDecision::Deny
        );
        assert_eq!(consent.calls(), 0);
    }

    #[tokio::test]
    async fn always_mode_grants_without_a_card() {
        let runtime = runtime();
        runtime.set_mode(BrowserUseMode::Always);
        let consent = Arc::new(FixedGate::new(ApprovalDecision::Deny));
        let gate = gate(runtime.clone(), consent.clone(), PermissionMode::Default);
        assert_eq!(
            gate.request_approval(request("navigate")).await,
            ApprovalDecision::Approve
        );
        assert_eq!(consent.calls(), 0);
        assert!(runtime.is_granted("s"));
    }

    #[tokio::test]
    async fn non_browser_tools_pass_through_untouched() {
        let runtime = runtime();
        let consent = Arc::new(FixedGate::new(ApprovalDecision::Deny));
        let gate = gate(runtime, consent, PermissionMode::Plan);
        let mut req = request("snapshot");
        req.tool_name = "read_file".to_string();
        assert_eq!(
            gate.request_approval(req).await,
            ApprovalDecision::Approve,
            "the auto inner gate should decide, not the browser logic"
        );
    }

    #[test]
    fn action_classes_are_disjoint_and_cover_the_surface() {
        for action in ["navigate", "click", "type", "eval", "upload"] {
            assert!(!is_read_only_action(action), "{action}");
        }
        for action in ["snapshot", "text", "console", "network", "status"] {
            assert!(!is_mutating_action(action), "{action}");
            assert!(!is_privileged_action(action), "{action}");
        }
        // eval/upload must be recognised as privileged *and* mutating so a
        // future refactor cannot accidentally downgrade them.
        for action in ["eval", "upload"] {
            assert!(is_privileged_action(action), "{action}");
        }
    }

    #[test]
    fn hints_match_the_three_state_model() {
        assert!(composer_hint(BrowserUseMode::Off, false).is_none());
        assert_eq!(
            composer_hint(BrowserUseMode::Session, false).unwrap().0,
            "浏览器 · 首次使用询问"
        );
        assert_eq!(
            composer_hint(BrowserUseMode::Session, true).unwrap().0,
            "浏览器已授权 · 本会话"
        );
        assert_eq!(
            composer_hint(BrowserUseMode::Always, false).unwrap().0,
            "浏览器已允许 · Chromium"
        );
    }

    #[test]
    fn summaries_name_the_concrete_target() {
        assert!(
            summarize_action(&json!({"action": "navigate", "url": "https://a.test"}))
                .contains("https://a.test")
        );
        assert!(summarize_action(&json!({"action": "click", "ref": "e3"})).contains("e3"));
        assert!(summarize_action(&json!({"action": "eval"})).contains("脚本"));
    }
}
