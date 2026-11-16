//! Composer slash commands.
//!
//! A `/name` token at the start of the message runs a host action instead of
//! being sent to the model. Commands are deliberately few and objective: the
//! composer's `/` menu already lists tools, skills and subagents, and those
//! keep their existing meaning (a tool token is a hint, a skill stages its
//! body). Anything that is not a known command falls through to the model, so
//! typing a path such as `/usr/bin/env` is never swallowed.

/// One host-side command reachable from the composer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SlashCommand {
    /// Summarize the conversation so far into a rolling summary and drop the
    /// summarized messages from the request window.
    Compact,
}

impl SlashCommand {
    /// Canonical name without the leading slash.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Compact => "compact",
        }
    }

    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::Compact => "压缩当前上下文：把较早的对话汇总成滚动摘要，腾出窗口",
        }
    }

    /// Longer hint shown in the `/` menu's detail column.
    pub(crate) fn detail(self) -> &'static str {
        match self {
            Self::Compact => "命令 · 立即执行",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        match name {
            "compact" => Some(Self::Compact),
            _ => None,
        }
    }

    /// Every command, in the order the `/` menu lists them.
    pub(crate) const ALL: &'static [SlashCommand] = &[SlashCommand::Compact];

    /// Match a full command token, case-insensitively. Whitespace is rejected:
    /// the caller has already split the token off the message.
    pub(crate) fn parse(token: &str) -> Option<Self> {
        let token = token.strip_prefix('/')?;
        Self::from_name(&token.to_ascii_lowercase())
    }
}

/// Split a composer message into its leading command and the remaining text.
///
/// A command must be the whole first token: `/compact` and `/compact 继续` both
/// work, while `/compactor` and `/usr/bin/env` are ordinary text. Text after a
/// command is handed back so the host can decide what to do with it (the
/// current implementation asks the user to resend rather than silently
/// dropping it).
pub(crate) fn split_command(text: &str) -> Option<(SlashCommand, String)> {
    let trimmed = text.trim_start();
    if !trimmed.starts_with('/') {
        return None;
    }
    let token_end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
    let command = SlashCommand::parse(&trimmed[..token_end])?;
    Some((command, trimmed[token_end..].trim().to_string()))
}

/// Compact token counts for notices ("12.3k"). Kept next to the commands so the
/// wording of a command result lives in one file.
pub(crate) fn format_tokens(tokens: usize) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}m", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_the_compact_command() {
        assert_eq!(SlashCommand::parse("/compact"), Some(SlashCommand::Compact));
        assert_eq!(SlashCommand::parse("/COMPACT"), Some(SlashCommand::Compact));
        assert_eq!(SlashCommand::parse("/ compact"), None);
        assert_eq!(SlashCommand::parse("compact"), None);
    }

    #[test]
    fn leading_token_must_match_exactly() {
        assert_eq!(
            split_command("/compact"),
            Some((SlashCommand::Compact, String::new()))
        );
        assert_eq!(
            split_command("  /compact  继续吧 "),
            Some((SlashCommand::Compact, "继续吧".to_string()))
        );
        assert_eq!(split_command("/compactor"), None);
        // A path or prose is not swallowed by the command layer.
        assert_eq!(split_command("/usr/bin/env python"), None);
        assert_eq!(split_command("请用 /compact 压缩"), None);
        assert_eq!(split_command(""), None);
    }
}
