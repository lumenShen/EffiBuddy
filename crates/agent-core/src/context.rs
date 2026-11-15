//! Context construction and compaction.
//!
//! Token counts are deliberately estimates: they must be cheap, synchronous
//! and provider independent. The safety margin is configurable via the
//! compaction threshold.

use chrono::Utc;

use crate::message::{ContentBlock, ImageAttachment, Message, Role, truncate_chars};
use crate::provider::ChatMessage;
use crate::session::{CompactionRecord, SessionState};

pub fn estimate_tokens(text: &str) -> usize {
    let mut ascii = 0usize;
    let mut wide = 0usize;
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii += 1;
        } else {
            wide += 1;
        }
    }
    ascii.div_ceil(4) + wide
}

pub fn estimate_message_tokens(message: &Message) -> usize {
    let mut total = 6; // role + framing overhead
    for block in &message.blocks {
        match block {
            ContentBlock::Text { text } | ContentBlock::Reasoning { text } => {
                total += estimate_tokens(text);
            }
            ContentBlock::ToolCall { call } => {
                total += estimate_tokens(&call.name);
                total +=
                    estimate_tokens(&serde_json::to_string(&call.arguments).unwrap_or_default());
                total += 8;
            }
            ContentBlock::ToolResult { result } => {
                total += estimate_tokens(&result.content);
                total += result
                    .images
                    .iter()
                    .map(ImageAttachment::estimated_tokens)
                    .sum::<usize>();
                total += 8;
            }
        }
    }
    total
}

pub fn estimate_request_tokens(session: &SessionState, system_prompt: &str) -> usize {
    let mut total = estimate_tokens(system_prompt) + 12;
    if let Some(compaction) = &session.compaction {
        total += estimate_tokens(&compaction.summary) + 12;
    }
    for message in session.visible_messages() {
        total += estimate_message_tokens(message);
    }
    total
}

/// Wire template for a mid-turn ("steered") user message.
///
/// The transcript stores the raw text; this framing is applied only when the
/// request is built, mirroring the way a queued user message must not be
/// mistaken for a fresh conversation start. Keep the denial explicit: a
/// background notification that arrives in the same step must never be
/// readable as user acknowledgement.
pub(crate) fn mid_turn_wire_text(raw: &str) -> String {
    format!(
        "<user-steer>\n\
         The user sent a new message while you were working. This is genuine user \
         input for the current task: fold it into the running turn and address it. \
         It is not a request to restart, and any system notice next to it is not \
         user acknowledgement.\n\n{raw}\n</user-steer>"
    )
}

/// Flatten the internal transcript into the provider wire model.
pub fn build_chat_messages(session: &SessionState, system_prompt: &str) -> Vec<ChatMessage> {
    let mut messages = Vec::with_capacity(session.visible_messages().len() + 2);
    messages.push(ChatMessage::system(system_prompt));

    if let Some(compaction) = &session.compaction {
        messages.push(ChatMessage::system(format!(
            "[Earlier conversation, compacted to save context. Treat it as accurate background.]\n{}",
            compaction.summary
        )));
    }

    // Images from consecutive tool results must not be interleaved between
    // tool messages: OpenAI requires every tool result for an assistant
    // tool-call turn to follow one another. We collect rasters and flush them
    // as one user message after the whole tool-result run.
    let mut pending_images: Vec<(String, Vec<ImageAttachment>)> = Vec::new();

    for message in session.visible_messages() {
        match message.role {
            Role::System => {
                flush_tool_images(&mut messages, &mut pending_images);
                let text = message.text();
                if !text.is_empty() {
                    messages.push(ChatMessage::system(text));
                }
            }
            Role::User => {
                flush_tool_images(&mut messages, &mut pending_images);
                let text = if message.mid_turn {
                    mid_turn_wire_text(&message.text())
                } else {
                    message.text()
                };
                if !text.is_empty() {
                    messages.push(ChatMessage::user(text));
                }
            }
            Role::Assistant => {
                flush_tool_images(&mut messages, &mut pending_images);
                let text = message.text();
                let calls = message.tool_calls();
                if text.is_empty() && calls.is_empty() {
                    continue;
                }
                messages.push(ChatMessage::assistant(text, calls));
            }
            Role::Tool => {
                if let Some(result) = message.tool_result() {
                    messages.push(ChatMessage::tool(
                        result.call_id.clone(),
                        result.content.clone(),
                    ));
                    if !result.images.is_empty() {
                        pending_images.push((result.call_id.clone(), result.images.clone()));
                    }
                }
            }
        }
    }
    flush_tool_images(&mut messages, &mut pending_images);

    messages
}

fn flush_tool_images(
    messages: &mut Vec<ChatMessage>,
    pending: &mut Vec<(String, Vec<ImageAttachment>)>,
) {
    if pending.is_empty() {
        return;
    }
    let call_ids: Vec<String> = pending.iter().map(|(id, _)| id.clone()).collect();
    let images: Vec<ImageAttachment> = pending.drain(..).flat_map(|(_, images)| images).collect();
    messages.push(ChatMessage::user_with_images(
        format!(
            "Image output for tool call(s) {}. These are the pixels the next computer action \
             coordinates refer to; always use the frame_id returned with the screenshot.",
            call_ids.join(", ")
        ),
        images,
    ));
}

/// Compact old turns into a rolling summary if the estimated context is above
/// `threshold * context_window`.
///
/// The boundary is always a user message, so the retained window never starts
/// in the middle of an assistant tool-call / tool-result group.
pub fn maybe_compact(
    session: &mut SessionState,
    context_window: usize,
    threshold: f32,
    keep_recent_turns: usize,
    system_prompt: &str,
    max_summary_chars: usize,
) -> Option<CompactionRecord> {
    let estimated = estimate_request_tokens(session, system_prompt);
    let budget = (context_window as f32 * threshold.clamp(0.2, 0.98)) as usize;
    if estimated <= budget {
        return None;
    }

    let user_indices: Vec<usize> = session
        .messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| (message.role == Role::User).then_some(index))
        .collect();

    let keep_recent_turns = keep_recent_turns.max(1);
    if user_indices.len() <= keep_recent_turns {
        return None;
    }

    let boundary = user_indices[user_indices.len() - keep_recent_turns];
    if boundary <= session.compacted_upto {
        return None;
    }

    let mut summary = session
        .compaction
        .as_ref()
        .map(|record| record.summary.clone())
        .unwrap_or_default();

    if !summary.is_empty() {
        summary.push_str("\n\n");
    }
    summary.push_str("### Earlier conversation\n");
    for message in &session.messages[session.compacted_upto..boundary] {
        summary.push_str(&summarize_message(message));
        summary.push('\n');
    }

    // Keep the newest part of the summary; older detail matters less.
    if summary.chars().count() > max_summary_chars {
        let tail: String = summary
            .chars()
            .rev()
            .take(max_summary_chars)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        summary = format!("…[older summary trimmed]…\n{tail}");
    }

    let record = CompactionRecord {
        summary,
        compacted_messages: boundary,
        at: Utc::now(),
    };
    session.compacted_upto = boundary;
    session.compaction = Some(record.clone());
    session.touch();
    Some(record)
}

fn summarize_message(message: &Message) -> String {
    match message.role {
        Role::User => format!("- **User**: {}", message.preview(600)),
        Role::Assistant => {
            let mut out = String::new();
            let text = message.preview(700);
            if !text.is_empty() {
                out.push_str(&format!("- **Assistant**: {text}"));
            }
            for call in message.tool_calls() {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(&format!(
                    "[called `{}` with {}]",
                    call.name,
                    truncate_chars(
                        &serde_json::to_string(&call.arguments).unwrap_or_default(),
                        300
                    )
                ));
            }
            if out.is_empty() {
                "- **Assistant**: (empty)".to_string()
            } else {
                out
            }
        }
        Role::Tool => match message.tool_result() {
            Some(result) => format!(
                "- **Tool `{}`** ({}): {}",
                result.name,
                if result.is_error { "error" } else { "ok" },
                truncate_chars(
                    &result
                        .content
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" "),
                    400
                )
            ),
            None => "- **Tool**: (empty)".to_string(),
        },
        Role::System => format!("- **System**: {}", message.preview(300)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{ToolCall, ToolResult};
    use std::path::PathBuf;

    fn session_with_dialog(turns: usize) -> SessionState {
        let mut session = SessionState::new(PathBuf::from("."), "test-model");
        for index in 0..turns {
            session.push_message(Message::user(format!("question {index}")));
            let mut assistant = Message::assistant();
            assistant.set_text(format!("answer {index}"));
            assistant.push_tool_call(ToolCall::new("read_file", serde_json::json!({"path": "a"})));
            session.push_message(assistant);
            let call = session
                .messages
                .last()
                .unwrap()
                .tool_calls()
                .into_iter()
                .next()
                .unwrap();
            session.push_message(Message::from_tool_result(ToolResult::success(
                &call, "contents",
            )));
        }
        session
    }

    #[test]
    fn compacts_at_a_user_boundary_and_keeps_recent_turns() {
        let mut session = session_with_dialog(8);
        let prompt = "system";
        let record = maybe_compact(&mut session, 100, 0.5, 2, prompt, 20_000).unwrap();
        assert!(record.compacted_messages > 0);
        assert_eq!(session.visible_messages()[0].role, Role::User);
        let compact_user_count = session
            .visible_messages()
            .iter()
            .filter(|m| m.role == Role::User)
            .count();
        assert_eq!(compact_user_count, 2);
    }

    #[test]
    fn no_compaction_when_small() {
        let mut session = session_with_dialog(2);
        assert!(maybe_compact(&mut session, 1_000_000, 0.8, 2, "system", 20_000).is_none());
        assert_eq!(session.compacted_upto, 0);
    }

    #[test]
    fn estimates_wide_characters_conservatively() {
        assert!(estimate_tokens("你好世界") >= 4);
        assert!(estimate_tokens("hello world") >= 2);
    }

    #[test]
    fn mid_turn_user_messages_are_framed_on_the_wire_only() {
        let mut session = SessionState::new(PathBuf::from("."), "test-model");
        session.push_message(Message::user("hello"));
        session.push_message(Message::mid_turn_user("check the logs"));
        let messages = build_chat_messages(&session, "system");
        assert_eq!(messages[1].content, "hello");
        assert!(
            messages[2].content.contains("<user-steer>"),
            "steer must be framed: {}",
            messages[2].content
        );
        assert!(messages[2].content.contains("check the logs"));
        // The transcript keeps the raw text; only the wire request frames it.
        assert_eq!(session.messages[1].text(), "check the logs");
    }

    #[test]
    fn tool_images_are_grouped_after_all_tool_results() {
        let mut session = SessionState::new(PathBuf::from("."), "test-model");
        session.push_message(Message::user("take a look"));
        let mut assistant = Message::assistant();
        let first = ToolCall::new("computer", serde_json::json!({"action": "screenshot"}))
            .with_id("call_1");
        let second = ToolCall::new("computer", serde_json::json!({"action": "screenshot"}))
            .with_id("call_2");
        assistant.push_tool_call(first.clone());
        assistant.push_tool_call(second.clone());
        session.push_message(assistant);
        session.push_message(Message::from_tool_result(
            ToolResult::success(&first, "{\"ok\":1}")
                .with_images(vec![ImageAttachment::png("YQ==", 100, 80)]),
        ));
        session.push_message(Message::from_tool_result(
            ToolResult::success(&second, "{\"ok\":2}")
                .with_images(vec![ImageAttachment::png("Yg==", 100, 80)]),
        ));

        let messages = build_chat_messages(&session, "system");
        // system, user, assistant, tool, tool, one grouped image user message
        assert_eq!(messages.len(), 6);
        assert_eq!(messages[3].role, "tool");
        assert_eq!(messages[4].role, "tool");
        assert_eq!(messages[5].role, "user");
        assert_eq!(messages[5].images.len(), 2);
        assert!(messages[5].content.contains("call_1"));
        assert!(messages[5].content.contains("call_2"));
    }
}
