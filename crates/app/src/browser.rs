//! Browser-use UI: the composer hint.
//!
//! The runtime lives in `effibuddy-browser`. Permission is *not* decided here:
//! `browser` is a regular tool (`ToolClass::Device`) and the session's
//! `PermissionMode` approves or denies it, for the main agent and for subagents
//! alike — `FullAccess` no longer stops at a one-time consent card.

use effibuddy_agent_core::tool::PermissionMode;
use effibuddy_browser::BrowserUseMode;

/// Compact chip text for the composer. Kept here so the UI and tests agree on
/// the wording.
pub(crate) fn composer_hint(
    mode: BrowserUseMode,
    permission_mode: PermissionMode,
) -> Option<(&'static str, &'static str)> {
    if !mode.is_enabled() {
        return None;
    }
    let label = if permission_mode == PermissionMode::FullAccess {
        "浏览器已允许 · 完全访问"
    } else {
        "浏览器已开启 · 按操作审批"
    };
    Some((
        label,
        "已允许 AI 通过 DevTools 协议操控浏览器；是否弹审批卡由当前权限模式决定。\
         页面内容是不可信输入。点击打开设置。",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints_follow_the_capability_switch_and_permission_mode() {
        assert!(composer_hint(BrowserUseMode::Off, PermissionMode::Default).is_none());
        assert_eq!(
            composer_hint(BrowserUseMode::On, PermissionMode::Default)
                .unwrap()
                .0,
            "浏览器已开启 · 按操作审批"
        );
        assert_eq!(
            composer_hint(BrowserUseMode::On, PermissionMode::FullAccess)
                .unwrap()
                .0,
            "浏览器已允许 · 完全访问"
        );
        assert!(
            composer_hint(BrowserUseMode::On, PermissionMode::Default)
                .unwrap()
                .1
                .contains("不可信")
        );
    }
}
