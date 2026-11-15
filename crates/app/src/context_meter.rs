//! 输入框上下文用量环的数据与悬停说明。
//!
//! 环上的进度取供应商上报的 `last_prompt_tokens`（当前上下文足迹）；悬停
//! tips 里的占用构成为本地估算：消息历史与压缩摘要用 `context::estimate_*`
//! 逐条估算，系统提示与工具定义没有本地副本，取“上报值 − 估算部分”的残差。
//!
//! 累计消耗按整棵会话树统计：主智能体的 `session.usage` 加上所有子智能体
//! 各自的会话用量；上下文占用（环与分段条）仍只看主会话。

use std::collections::HashSet;

use effibuddy_agent_core::context::{estimate_message_tokens_with_cleared, estimate_tokens};
use effibuddy_agent_core::{SessionState, SharedSession, lock_session};
use effibuddy_ui::Theme;
use gpui::{AnyElement, AnyView, App, Context, IntoElement, Render, Window, div, prelude::*, px};

/// 子智能体（含孙代理）的累计消耗。每个子代理有自己的会话与计量，这里
/// 按当前会话树汇总；上下文占用仍只看主会话的窗口，不混入子会话。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SubagentUsage {
    /// 计入的子智能体数（运行中 + 已结束 + 历史恢复）。
    pub(crate) count: usize,
    pub(crate) prompt_tokens: u64,
    pub(crate) completion_tokens: u64,
    pub(crate) cached_tokens: u64,
    pub(crate) total_tokens: u64,
}

impl SubagentUsage {
    pub(crate) fn is_empty(&self) -> bool {
        self.count == 0
    }
}

/// 一次快照：环与 tips 共用的最小数据集。
#[derive(Clone, Debug, Default)]
pub(crate) struct ContextUsage {
    pub(crate) context_window: usize,
    pub(crate) last_prompt_tokens: u64,
    /// 可见消息历史的本地估算 tokens（不含压缩摘要）。
    pub(crate) message_tokens: usize,
    /// 压缩摘要 tokens；无压缩时为 0。
    pub(crate) summary_tokens: usize,
    /// 可见消息条数。
    pub(crate) message_count: usize,
    /// 主智能体（当前会话）自身的累计消耗。
    pub(crate) prompt_total: u64,
    pub(crate) completion_total: u64,
    pub(crate) cached_total: u64,
    pub(crate) total: u64,
    /// 当前会话树里所有子智能体的累计消耗。
    pub(crate) subagents: SubagentUsage,
}

impl ContextUsage {
    /// 无会话时的空快照（环画空轨）。
    pub(crate) fn empty(context_window: usize) -> Self {
        Self {
            context_window,
            ..Default::default()
        }
    }

    /// 环的进度 0..1。
    pub(crate) fn fraction(&self) -> f32 {
        (self.last_prompt_tokens as f32 / self.context_window.max(1) as f32).clamp(0.0, 1.0)
    }

    /// 占用百分比 0..100；沿用旧指示器的 99.9 上限，避免压缩前显示 100%。
    pub(crate) fn percent(&self) -> f64 {
        (self.last_prompt_tokens as f64 / self.context_window.max(1) as f64 * 100.0)
            .clamp(0.0, 99.9)
    }

    /// 系统提示与工具定义 ≈ 供应商上报的当前 prompt − 本地可估算部分；
    /// 尚无请求上报（或残差为 0）时不展示该行。
    pub(crate) fn system_tool_tokens(&self) -> Option<usize> {
        if self.last_prompt_tokens == 0 {
            return None;
        }
        let residual = (self.last_prompt_tokens as usize)
            .saturating_sub(self.message_tokens + self.summary_tokens);
        (residual > 0).then_some(residual)
    }

    /// 会话树累计输入 = 主智能体 + 子智能体。
    pub(crate) fn prompt_total_all(&self) -> u64 {
        self.prompt_total + self.subagents.prompt_tokens
    }

    /// 会话树累计输出 = 主智能体 + 子智能体。
    pub(crate) fn completion_total_all(&self) -> u64 {
        self.completion_total + self.subagents.completion_tokens
    }

    /// 会话树累计缓存命中 = 主智能体 + 子智能体。
    pub(crate) fn cached_total_all(&self) -> u64 {
        self.cached_total + self.subagents.cached_tokens
    }

    /// 会话树累计合计 = 主智能体 + 子智能体。
    pub(crate) fn total_all(&self) -> u64 {
        self.total + self.subagents.total_tokens
    }
}

/// 锁定会话收集一次用量快照。
pub(crate) fn collect(shared: &SharedSession, context_window: usize) -> ContextUsage {
    let session = lock_session(shared);
    collect_from_session(&session, context_window)
}

/// 已持有会话锁时的快照构建。概览页在同一把锁里读取计划与用量，避免
/// 两次加锁之间数据被 harness 更新。
pub(crate) fn collect_from_session(
    session: &SessionState,
    context_window: usize,
) -> ContextUsage {
    let visible = session.visible_messages();
    let cleared: HashSet<String> = session.cleared_tool_calls.iter().cloned().collect();
    let message_tokens: usize = visible
        .iter()
        .map(|message| estimate_message_tokens_with_cleared(message, &cleared))
        .sum();
    let summary_tokens = session
        .compaction
        .as_ref()
        .map(|record| estimate_tokens(&record.summary))
        .unwrap_or(0);
    let usage = session.usage.clone();
    ContextUsage {
        context_window,
        last_prompt_tokens: usage.last_prompt_tokens,
        message_tokens,
        summary_tokens,
        message_count: visible.len(),
        prompt_total: usage.prompt_tokens,
        completion_total: usage.completion_tokens,
        cached_total: usage.cached_tokens,
        total: usage.total_tokens,
        subagents: SubagentUsage::default(),
    }
}

/// 进度环的悬停 tips：上下文使用构成 + 当前智能体的累计消耗。
pub(crate) struct ContextUsageTip {
    theme: Theme,
    usage: ContextUsage,
}

impl ContextUsageTip {
    pub(crate) fn new(theme: Theme, usage: ContextUsage) -> Self {
        Self { theme, usage }
    }
}

impl Render for ContextUsageTip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme.clone();
        let usage = self.usage.clone();
        let tip_fg = theme.tooltip_fg;
        let percent_label = if usage.percent() < 10.0 {
            format!("{:.1}%", usage.percent())
        } else {
            format!("{:.0}%", usage.percent())
        };

        div()
            .w(px(232.0))
            .flex()
            .flex_col()
            .gap(px(4.0))
            .px(px(10.0))
            .py(px(8.0))
            .rounded(px(8.0))
            .bg(theme.tooltip_bg)
            .text_color(tip_fg)
            .text_size(px(11.0))
            // 标题行：右侧补上环上刻意省略的百分比。
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("上下文使用"),
                    )
                    .child(div().font_family(theme.mono_font()).child(percent_label)),
            )
            .child(tip_row(
                &theme,
                "当前占用",
                format!(
                    "{} / {}",
                    format_tokens(usage.last_prompt_tokens),
                    format_window(usage.context_window)
                ),
            ))
            .children(usage.system_tool_tokens().map(|tokens| {
                tip_row(
                    &theme,
                    "系统提示 · 工具",
                    format!("~{}", format_tokens(tokens as u64)),
                )
            }))
            .child(tip_row(
                &theme,
                "消息历史",
                format!(
                    "{} 条 · ~{}",
                    usage.message_count,
                    format_tokens(usage.message_tokens as u64)
                ),
            ))
            .when(usage.summary_tokens > 0, |element| {
                element.child(tip_row(
                    &theme,
                    "压缩摘要",
                    format!("~{}", format_tokens(usage.summary_tokens as u64)),
                ))
            })
            .child(
                div()
                    .h(px(1.0))
                    .w_full()
                    .my(px(2.0))
                    .bg(tip_fg.opacity(0.14)),
            )
            .child(
                div()
                    .text_color(tip_fg.opacity(0.62))
                    .child(if usage.subagents.is_empty() {
                        "累计消耗（当前智能体）"
                    } else {
                        "累计消耗（含子智能体）"
                    }),
            )
            .child(tip_row(
                &theme,
                "输入",
                format_tokens(usage.prompt_total_all()),
            ))
            .child(tip_row(
                &theme,
                "输出",
                format_tokens(usage.completion_total_all()),
            ))
            .when(usage.cached_total_all() > 0, |element| {
                element.child(tip_row(
                    &theme,
                    "缓存命中",
                    format_tokens(usage.cached_total_all()),
                ))
            })
            .child(tip_row(&theme, "合计", format_tokens(usage.total_all())))
            .when(!usage.subagents.is_empty(), |element| {
                element.child(tip_row(
                    &theme,
                    "其中子智能体",
                    format!(
                        "{} 个 · {}",
                        usage.subagents.count,
                        format_tokens(usage.subagents.total_tokens)
                    ),
                ))
            })
            .child(
                div()
                    .mt(px(2.0))
                    .text_size(px(10.0))
                    .text_color(tip_fg.opacity(0.55))
                    .child("点击打开概览查看明细"),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fraction_and_percent_derive_from_prompt_tokens() {
        let usage = ContextUsage {
            context_window: 200_000,
            last_prompt_tokens: 50_000,
            ..Default::default()
        };
        assert!((usage.fraction() - 0.25).abs() < 1e-6);
        assert!((usage.percent() - 25.0).abs() < 1e-6);
    }

    #[test]
    fn system_tool_residual_never_overcounts_local_estimates() {
        let mut usage = ContextUsage {
            context_window: 200_000,
            last_prompt_tokens: 10_000,
            message_tokens: 12_000,
            summary_tokens: 0,
            ..Default::default()
        };
        // 本地估算高于上报值（估算偏大）时残差归零、不展示该行。
        assert_eq!(usage.system_tool_tokens(), None);
        usage.message_tokens = 6_000;
        assert_eq!(usage.system_tool_tokens(), Some(4_000));
        usage.last_prompt_tokens = 0;
        assert_eq!(usage.system_tool_tokens(), None);
    }

    #[test]
    fn accumulated_totals_include_subagent_consumption() {
        let usage = ContextUsage {
            prompt_total: 1_000,
            completion_total: 100,
            cached_total: 600,
            total: 1_100,
            subagents: SubagentUsage {
                count: 2,
                prompt_tokens: 2_000,
                completion_tokens: 200,
                cached_tokens: 1_200,
                total_tokens: 2_200,
            },
            ..Default::default()
        };
        assert_eq!(usage.prompt_total_all(), 3_000);
        assert_eq!(usage.completion_total_all(), 300);
        assert_eq!(usage.cached_total_all(), 1_800);
        assert_eq!(usage.total_all(), 3_300);
        assert!(!usage.subagents.is_empty());
    }
}

/// tips 内的一行：左标签弱化、右数值等宽对齐。
fn tip_row(theme: &Theme, label: &str, value: String) -> AnyElement {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(16.0))
        .child(
            div()
                .text_color(theme.tooltip_fg.opacity(0.62))
                .child(label.to_string()),
        )
        .child(div().font_family(theme.mono_font()).child(value))
        .into_any_element()
}

/// 构造挂到进度环上的 tooltip 闭包；快照在渲染时定格，悬停期间保持一致。
pub(crate) fn context_tooltip(
    theme: &Theme,
    usage: ContextUsage,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let theme = theme.clone();
    move |_window, cx| {
        let theme = theme.clone();
        let usage = usage.clone();
        cx.new(|_cx| ContextUsageTip { theme, usage }).into()
    }
}

fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}m", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn format_window(tokens: usize) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}m", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{}k", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}
