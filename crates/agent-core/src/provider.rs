//! Provider boundary.
//!
//! The harness speaks a small provider-neutral protocol: a request made of
//! messages and tool specs, streamed deltas, and one final [`Completion`].
//! Adapters are responsible for wire formats, SSE parsing and tool-call
//! reassembly.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::error::ProviderError;
use crate::message::{FinishReason, ImageAttachment, ToolCall, Usage};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Model-visible images attached to this message. Tool results are emitted
    /// as text first; `context::build_chat_messages` then appends one user
    /// message carrying every image from that tool step.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageAttachment>,
    /// The reasoning trace the provider returned alongside this assistant
    /// message. DeepSeek's thinking mode treats it as protocol state: a
    /// request that carries `tools` is rejected unless every historical
    /// assistant message echoes its trace back, and an empty trace must still
    /// be present as an empty string. `None` means the provider never supplied
    /// one; adapters that do not speak this protocol ignore the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            images: Vec::new(),
            reasoning: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            images: Vec::new(),
            reasoning: None,
        }
    }

    pub fn user_with_images(content: impl Into<String>, images: Vec<ImageAttachment>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            images,
            reasoning: None,
        }
    }

    pub fn assistant(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.into(),
            tool_calls,
            tool_call_id: None,
            images: Vec::new(),
            reasoning: None,
        }
    }

    /// Attach the reasoning trace returned with an assistant message. The
    /// trace may be empty; `Some("")` is meaningful because DeepSeek requires
    /// the field to be present even when the model produced no reasoning.
    pub fn with_reasoning(mut self, reasoning: impl Into<String>) -> Self {
        self.reasoning = Some(reasoning.into());
        self
    }

    pub fn tool(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".to_string(),
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
            images: Vec::new(),
            reasoning: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolSpec>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    /// Provider-specific reasoning/thinking effort (`low`/`medium`/`high`).
    /// Adapters only emit it when the selected model exposes levels.
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone)]
pub enum ProviderEvent {
    TextDelta(String),
    ReasoningDelta(String),
    Usage(Usage),
}

#[derive(Debug, Clone, Default)]
pub struct Completion {
    pub text: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    pub finish_reason: Option<FinishReason>,
}

impl Completion {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..Default::default()
        }
    }

    pub fn with_reasoning(mut self, reasoning: impl Into<String>) -> Self {
        self.reasoning = reasoning.into();
        self
    }

    pub fn with_tool_call(mut self, call: ToolCall) -> Self {
        self.tool_calls.push(call);
        self
    }
}

/// Async model adapter.
///
/// Implementations must:
/// - emit deltas through `events` as they arrive (best effort);
/// - return the *complete* text/reasoning/tool calls in [`Completion`], even
///   if no deltas were emitted;
/// - return [`ProviderError::Cancelled`] promptly when `cancel` fires.
#[async_trait]
pub trait ModelProvider: Send + Sync + 'static {
    fn name(&self) -> &str;

    /// Soft limit used for context compaction. Adapters should override it.
    fn context_window(&self) -> usize {
        128_000
    }

    async fn stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
        events: UnboundedSender<ProviderEvent>,
    ) -> Result<Completion, ProviderError>;
}

pub fn send_event(
    events: &UnboundedSender<ProviderEvent>,
    event: ProviderEvent,
) -> Result<(), ProviderError> {
    events.send(event).map_err(|_| ProviderError::Cancelled)
}

/// Split text into small chunks for fake/streaming providers.
pub fn chunk_chars(text: &str, size: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(size.max(1))
        .map(|chunk| chunk.iter().collect())
        .collect()
}
