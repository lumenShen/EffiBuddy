//! Computer-use UI: the floating control bar and the consent approval gate.
//!
//! The tool runtime lives in `effibuddy-computer`; this module is the app-side
//! safety boundary. The gate deliberately bypasses the normal permission-mode
//! fast path for `computer` so FullAccess cannot silently grant screen control:
//! Session mode always shows a real consent card, and Plan mode rejects
//! mutating actions.

use std::sync::Arc;

use async_trait::async_trait;
use effibuddy_agent_core::tool::{PermissionMode, ToolClass};
use effibuddy_agent_core::{ApprovalDecision, ApprovalGate, ApprovalRequest};
use effibuddy_computer::{ComputerRuntime, ComputerUseMode, OverlayEvent, OverlayEventKind};
use effibuddy_ui::{Button, ButtonSize, ButtonVariant, Spinner, Theme, dot, shadow_float};
use gpui::prelude::*;
use gpui::{AnyElement, Context, IntoElement, Render, SharedString, WeakEntity, Window, div, px};

use crate::app::AppView;

pub(crate) struct ComputerOverlayView {
    runtime: Arc<ComputerRuntime>,
    app: WeakEntity<AppView>,
    theme: Theme,
    session_id: Option<String>,
    message: String,
    paused: bool,
}

impl ComputerOverlayView {
    pub fn new(runtime: Arc<ComputerRuntime>, app: WeakEntity<AppView>, theme: Theme) -> Self {
        Self {
            runtime,
            app,
            theme,
            session_id: None,
            message: "正在连接桌面…".to_string(),
            paused: false,
        }
    }

    pub fn apply(&mut self, event: &OverlayEvent, cx: &mut Context<Self>) {
        self.session_id = Some(event.session_id.clone());
        match &event.kind {
            OverlayEventKind::Show { message, paused }
            | OverlayEventKind::Update { message, paused } => {
                self.message = message.clone();
                self.paused = *paused;
            }
            OverlayEventKind::Hide | OverlayEventKind::CaptureHide => {}
        }
        cx.notify();
    }

    fn take_over(&mut self, cx: &mut Context<Self>) {
        if let Some(session_id) = &self.session_id {
            self.runtime.take_over(session_id);
            self.paused = true;
            self.message =
                "你已接管操作。完成后点「我帮你点好了」继续，或点「停止操作」。".to_string();
            cx.notify();
        }
    }

    fn resume(&mut self, cx: &mut Context<Self>) {
        if let Some(session_id) = &self.session_id {
            self.runtime.resume(session_id);
            self.paused = false;
            self.message = "EffiBuddy 正在继续操作…".to_string();
            cx.notify();
        }
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(session_id) = &self.session_id {
            self.runtime.stop(session_id);
        }
        // Stop the current run immediately instead of waiting for the model's
        // next tool call to notice the stopped flag.
        let _ = self.app.update(cx, |app, cx| app.interrupt(cx));
    }
}

impl Render for ComputerOverlayView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme.clone();
        let status_color = if self.paused {
            theme.warning
        } else {
            theme.accent
        };
        let status_icon: AnyElement = if self.paused {
            dot(&theme, status_color, 8.0).into_any_element()
        } else {
            Spinner::new(status_color, 14.0).into_any_element()
        };
        let primary_label: SharedString = if self.paused {
            "我帮你点好了".into()
        } else {
            "接管操作".into()
        };

        div()
            .size_full()
            .px(px(16.0))
            .flex()
            .items_center()
            .justify_between()
            .gap(px(12.0))
            .rounded(px(18.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.elevated)
            .shadow(shadow_float(&theme))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .min_w(px(0.0))
                    .child(status_icon)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(1.0))
                            .min_w(px(0.0))
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme.text)
                                    .child("EffiBuddy 正在帮您使用电脑"),
                            )
                            .child(
                                div()
                                    .text_size(px(10.5))
                                    .text_color(theme.text_muted)
                                    .truncate()
                                    .child(self.message.clone()),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        Button::new("computer-takeover", primary_label, &theme)
                            .size(ButtonSize::Sm)
                            .variant(if self.paused {
                                ButtonVariant::Primary
                            } else {
                                ButtonVariant::Secondary
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this.paused {
                                    this.resume(cx);
                                } else {
                                    this.take_over(cx);
                                }
                            })),
                    )
                    .child(
                        Button::new("computer-stop", "停止操作", &theme)
                            .size(ButtonSize::Sm)
                            .variant(ButtonVariant::Danger)
                            .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
                    ),
            )
    }
}

/// Wraps the normal mode gate with computer-specific consent.
pub(crate) struct ComputerApprovalGate {
    /// Used for every non-computer tool.
    inner: Arc<dyn ApprovalGate>,
    /// Forces a real UI card for first-time computer consent even when the
    /// session is in FullAccess mode.
    consent: Arc<dyn ApprovalGate>,
    runtime: Arc<ComputerRuntime>,
    session_id: String,
    permission_mode: Arc<dyn Fn() -> PermissionMode + Send + Sync>,
}

impl ComputerApprovalGate {
    pub fn new(
        inner: Arc<dyn ApprovalGate>,
        consent: Arc<dyn ApprovalGate>,
        runtime: Arc<ComputerRuntime>,
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
impl ApprovalGate for ComputerApprovalGate {
    async fn request_approval(&self, request: ApprovalRequest) -> ApprovalDecision {
        if request.tool_name != "computer" {
            return self.inner.request_approval(request).await;
        }

        let action = request
            .arguments
            .get("action")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();

        if (self.permission_mode)() == PermissionMode::Plan && is_mutating_action(action) {
            // Plan mode is a read-only contract. Even the consent card must not
            // let a click or a keypress through.
            return ApprovalDecision::Deny;
        }

        match self.runtime.mode() {
            ComputerUseMode::Off => ApprovalDecision::Deny,
            ComputerUseMode::Always => {
                self.runtime.grant(&self.session_id);
                ApprovalDecision::Approve
            }
            ComputerUseMode::Session => {
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

fn is_mutating_action(action: &str) -> bool {
    matches!(
        action,
        "click"
            | "drag"
            | "scroll"
            | "move"
            | "type"
            | "key"
            | "activate_window"
            | "lock_desktop"
            | "move_window_to_desktop"
            | "switch_desktop"
    )
}

/// Compact chip text for the composer. Kept here so the UI and tests agree on
/// the same three-state wording.
pub(crate) fn composer_hint(
    mode: ComputerUseMode,
    granted: bool,
    vision: bool,
) -> Option<(&'static str, &'static str)> {
    if !mode.is_enabled() {
        return None;
    }
    let label = match (mode, granted) {
        (ComputerUseMode::Always, _) => "电脑操作已允许 · 键鼠模拟",
        (ComputerUseMode::Session, true) => "电脑操作已授权 · 本会话",
        (ComputerUseMode::Session, false) => "电脑操作 · 首次使用询问",
        (ComputerUseMode::Off, _) => return None,
    };
    let tooltip = if vision {
        "已允许 AI 使用你的电脑键鼠模拟输入；点击打开设置。"
    } else {
        "允许 AI 使用键鼠模拟输入，但当前模型未开启视觉能力，截图无法发给模型；点击打开设置。"
    };
    Some((label, tooltip))
}

/// Used by `ToolClass` tests elsewhere; computer is intentionally `Other`
/// because consent is handled by [`ComputerApprovalGate`], not by the normal
/// permission matrix.
#[allow(dead_code)]
fn computer_tool_class() -> ToolClass {
    ToolClass::of_tool_name("computer")
}

#[cfg(test)]
mod tests {
    use super::*;
    use effibuddy_agent_core::AutoApprovalGate;
    use effibuddy_computer::testing::FakeBackend;
    use serde_json::json;

    struct FixedGate(ApprovalDecision, std::sync::atomic::AtomicUsize);

    impl FixedGate {
        fn new(decision: ApprovalDecision) -> Self {
            Self(decision, std::sync::atomic::AtomicUsize::new(0))
        }

        fn calls(&self) -> usize {
            self.1.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl ApprovalGate for FixedGate {
        async fn request_approval(&self, _request: ApprovalRequest) -> ApprovalDecision {
            self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.0
        }
    }

    fn request(action: &str) -> ApprovalRequest {
        ApprovalRequest {
            id: "r".to_string(),
            tool_name: "computer".to_string(),
            call_id: "c".to_string(),
            summary: "use computer".to_string(),
            arguments: json!({"action": action}),
        }
    }

    fn gate(
        runtime: Arc<ComputerRuntime>,
        consent: Arc<FixedGate>,
        mode: PermissionMode,
    ) -> ComputerApprovalGate {
        ComputerApprovalGate::new(
            Arc::new(AutoApprovalGate),
            consent,
            runtime,
            "s",
            Arc::new(move || mode),
        )
    }

    #[tokio::test]
    async fn session_mode_forces_real_consent_even_when_always_would_approve() {
        let runtime = Arc::new(ComputerRuntime::new(Arc::new(FakeBackend::new())));
        let consent = Arc::new(FixedGate::new(ApprovalDecision::ApproveForSession));
        let gate = gate(runtime.clone(), consent.clone(), PermissionMode::FullAccess);
        assert_eq!(
            gate.request_approval(request("screenshot")).await,
            ApprovalDecision::ApproveForSession
        );
        assert_eq!(consent.calls(), 1);
        assert!(runtime.is_granted("s"));
        // The second call is served by the session grant without another card.
        assert_eq!(
            gate.request_approval(request("click")).await,
            ApprovalDecision::Approve
        );
        assert_eq!(consent.calls(), 1);
    }

    #[tokio::test]
    async fn always_mode_skips_the_consent_card_and_grants() {
        let runtime = Arc::new(ComputerRuntime::new(Arc::new(FakeBackend::new())));
        runtime.set_mode(ComputerUseMode::Always);
        let consent = Arc::new(FixedGate::new(ApprovalDecision::Deny));
        let gate = gate(runtime.clone(), consent.clone(), PermissionMode::Default);
        assert_eq!(
            gate.request_approval(request("click")).await,
            ApprovalDecision::Approve
        );
        assert_eq!(consent.calls(), 0);
    }

    #[tokio::test]
    async fn off_mode_denies_without_a_card() {
        let runtime = Arc::new(ComputerRuntime::new(Arc::new(FakeBackend::new())));
        runtime.set_mode(ComputerUseMode::Off);
        let consent = Arc::new(FixedGate::new(ApprovalDecision::ApproveForSession));
        let gate = gate(runtime, consent.clone(), PermissionMode::Default);
        assert_eq!(
            gate.request_approval(request("screenshot")).await,
            ApprovalDecision::Deny
        );
        assert_eq!(consent.calls(), 0);
    }

    #[tokio::test]
    async fn plan_mode_denies_mutating_actions_but_allows_reads_to_ask() {
        let runtime = Arc::new(ComputerRuntime::new(Arc::new(FakeBackend::new())));
        let consent = Arc::new(FixedGate::new(ApprovalDecision::ApproveForSession));
        let gate = gate(runtime.clone(), consent.clone(), PermissionMode::Plan);
        assert_eq!(
            gate.request_approval(request("click")).await,
            ApprovalDecision::Deny
        );
        assert_eq!(consent.calls(), 0);
        assert_eq!(
            gate.request_approval(request("screenshot")).await,
            ApprovalDecision::ApproveForSession
        );
        assert_eq!(consent.calls(), 1);
    }

    #[tokio::test]
    async fn non_computer_tools_delegate_to_the_normal_gate() {
        let runtime = Arc::new(ComputerRuntime::new(Arc::new(FakeBackend::new())));
        let consent = Arc::new(FixedGate::new(ApprovalDecision::Deny));
        let gate = gate(runtime, consent, PermissionMode::Default);
        let mut request = request("screenshot");
        request.tool_name = "shell".to_string();
        assert_eq!(
            gate.request_approval(request).await,
            ApprovalDecision::Approve
        );
    }
}
