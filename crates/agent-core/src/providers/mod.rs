//! Provider adapters and the small amount of wire machinery they share.

pub mod anthropic;
pub mod demo;
pub mod openai;
pub mod responses;

pub use anthropic::{AnthropicConfig, AnthropicMessagesProvider};
pub use demo::DemoProvider;
pub use openai::{OpenAiCompatConfig, OpenAiCompatProvider};
pub use responses::{OpenAiResponsesConfig, OpenAiResponsesProvider};

use serde_json::Value;

use crate::context::estimate_tokens;
use crate::error::ProviderError;
use crate::message::truncate_chars;
use crate::provider::ChatMessage;

/// Map a non-success HTTP response to a retryable/permanent provider error.
/// OpenAI, Anthropic and Responses all put a human-readable message at
/// `error.message`, so one parser covers the three wire formats.
pub(crate) fn http_error(status: u16, body: String) -> ProviderError {
    let body = truncate_chars(body.trim(), 2_000);
    if let Ok(value) = serde_json::from_str::<Value>(&body) {
        if let Some(message) = value
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
        {
            return ProviderError::Http {
                status,
                body: message.to_string(),
            };
        }
    }
    ProviderError::Http { status, body }
}

/// Extract the `data:` payload from one SSE event block. Multi-line data is
/// joined with newlines as required by the SSE spec.
pub(crate) fn sse_data(event: &str) -> Option<String> {
    let mut data = String::new();
    for line in event.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.trim_start());
        }
    }
    if data.is_empty() { None } else { Some(data) }
}

/// Read one complete SSE event (terminated by a blank line) from `buffer`.
/// CRLF and LF are both accepted; a trailing partial event stays buffered.
pub(crate) fn take_sse_event(buffer: &mut String) -> Option<String> {
    let lf = buffer.find("\n\n").map(|index| (index, 2));
    let crlf = buffer.find("\r\n\r\n").map(|index| (index, 4));
    let (index, length) = match (lf, crlf) {
        (Some(left), Some(right)) => {
            if left.0 <= right.0 {
                left
            } else {
                right
            }
        }
        (Some(left), None) => left,
        (None, Some(right)) => right,
        (None, None) => return None,
    };
    let event = buffer[..index].to_string();
    buffer.drain(..index + length);
    Some(event)
}

pub(crate) fn estimate_chat_message_tokens(message: &ChatMessage) -> usize {
    let mut total = estimate_tokens(&message.role) + estimate_tokens(&message.content) + 6;
    for call in &message.tool_calls {
        total += estimate_tokens(&call.name);
        total += estimate_tokens(&serde_json::to_string(&call.arguments).unwrap_or_default());
    }
    total
}

pub(crate) fn estimate_request_tokens(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .map(estimate_chat_message_tokens)
        .sum::<usize>()
}
