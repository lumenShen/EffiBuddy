//! The conversation data model.
//!
//! The internal transcript keeps more information than any single provider
//! API: reasoning traces, tool calls and tool results are first-class blocks.
//! Provider adapters flatten this representation into their own wire format,
//! which is why the harness never depends on OpenAI/DeepSeek message shapes.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::new_id;
use crate::network::{AgentMessage, AgentMessageKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_wire(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
    /// Set by a provider when the model produced syntactically invalid tool
    /// arguments. Execution fails with this message so the model can retry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argument_parse_error: Option<String>,
}

impl ToolCall {
    pub fn new(name: impl Into<String>, arguments: Value) -> Self {
        Self {
            id: format!("call_{}", new_id()),
            name: name.into(),
            arguments,
            argument_parse_error: None,
        }
    }

    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    pub fn arguments_pretty(&self) -> String {
        serde_json::to_string_pretty(&self.arguments).unwrap_or_else(|_| "{}".to_string())
    }
}

/// A model-visible image returned by a tool (usually a screenshot).
///
/// `data` is base64 so the attachment survives session persistence and can be
/// embedded in provider payloads without a second encode step. Images are
/// deliberately kept out of `ToolResult.content`: text truncation must never
/// cut a raster in half.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageAttachment {
    pub mime_type: String,
    pub data: String,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
}

impl ImageAttachment {
    pub fn png(data: impl Into<String>, width: u32, height: u32) -> Self {
        Self {
            mime_type: "image/png".to_string(),
            data: data.into(),
            width,
            height,
        }
    }

    pub fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.mime_type, self.data)
    }

    /// Rough vision-token estimate. Anthropic/OpenAI both bill images by
    /// pixels; 750 pixels/token is the commonly used approximation.
    pub fn estimated_tokens(&self) -> usize {
        let pixels = (self.width as usize).saturating_mul(self.height as usize);
        (pixels / 750).max(85) + 10
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub name: String,
    pub content: String,
    pub is_error: bool,
    pub duration_ms: Option<u64>,
    /// Structured, UI-facing metadata produced by special tools (for example
    /// the `agent` tool reports the child session id here). The model never
    /// sees this field; it is not part of the wire request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    /// Rasters returned by the tool. They are sent to the model as a separate
    /// image message after the tool result (see `context::build_chat_messages`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageAttachment>,
}

impl ToolResult {
    pub fn success(call: &ToolCall, content: impl Into<String>) -> Self {
        Self {
            call_id: call.id.clone(),
            name: call.name.clone(),
            content: content.into(),
            is_error: false,
            duration_ms: None,
            metadata: None,
            images: Vec::new(),
        }
    }

    pub fn failure(call: &ToolCall, content: impl Into<String>) -> Self {
        Self {
            call_id: call.id.clone(),
            name: call.name.clone(),
            content: content.into(),
            is_error: true,
            duration_ms: None,
            metadata: None,
            images: Vec::new(),
        }
    }

    pub fn with_metadata(mut self, metadata: Value) -> Self {
        self.metadata = Some(metadata);
        self
    }

    pub fn with_images(mut self, images: Vec<ImageAttachment>) -> Self {
        self.images = images;
        self
    }

    pub fn cancelled(call: &ToolCall) -> Self {
        Self::failure(call, "Cancelled by the user before the tool finished.")
    }
}

/// One block inside a message. Tagged so persisted sessions stay readable and
/// forward compatible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    Reasoning { text: String },
    ToolCall { call: ToolCall },
    ToolResult { result: ToolResult },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub role: Role,
    #[serde(default)]
    pub blocks: Vec<ContentBlock>,
    pub created_at: DateTime<Utc>,
    /// Set when the user interrupted the model while this message was being
    /// streamed. Partial content is kept so the transcript stays truthful.
    #[serde(default)]
    pub interrupted: bool,
    /// Model-visible input that was generated by the runtime rather than typed
    /// by the user (background task notifications). The UI renders these as
    /// system notices, not as user bubbles.
    #[serde(default)]
    pub synthetic: bool,
    /// Set when this user-role message was actually delivered by another
    /// agent. The UI renders a dedicated inter-agent card instead of a user
    /// bubble or a background-task notice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_message: Option<AgentMessageMeta>,
    /// The user typed this message while the model was already working and it
    /// was injected at a step boundary ("steer"). The transcript keeps the raw
    /// text; the wire request wraps it in a mid-turn template so the model
    /// treats it as new input for the running task, not a restart.
    #[serde(default)]
    pub mid_turn: bool,
}

/// UI-facing metadata for an inter-agent message. The text itself lives in the
/// message's [`ContentBlock::Text`] blocks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMessageMeta {
    pub message_id: String,
    pub from_id: String,
    pub from_name: String,
    pub to_id: String,
    pub to_name: String,
    pub kind: AgentMessageKind,
    /// Raw, unframed message text for the UI. The model still receives the
    /// framed [`AgentMessage::framed_text`] form.
    #[serde(default)]
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl Message {
    pub fn new(role: Role) -> Self {
        Self {
            id: new_id(),
            role,
            blocks: Vec::new(),
            created_at: Utc::now(),
            interrupted: false,
            synthetic: false,
            agent_message: None,
            mid_turn: false,
        }
    }

    pub fn user(text: impl Into<String>) -> Self {
        let mut message = Self::new(Role::User);
        message.push_text(text.into());
        message
    }

    /// A runtime-generated user-role message (background task notification).
    /// It is sent to the model like any user message, but the UI must not
    /// present it as something the user typed.
    pub fn synthetic_user(text: impl Into<String>) -> Self {
        let mut message = Self::user(text);
        message.synthetic = true;
        message
    }

    /// A user message typed while the model was already working ("steer").
    /// Stored with the raw text the user wrote; `build_chat_messages` wraps
    /// `mid_turn` messages so the model keeps them inside the running task.
    pub fn mid_turn_user(text: impl Into<String>) -> Self {
        let mut message = Self::user(text);
        message.mid_turn = true;
        message
    }

    /// A model-visible message delivered by another agent. It keeps the
    /// structured envelope available for the UI while the text block carries
    /// the same framed content the model sees.
    pub fn from_agent_message(message: &AgentMessage) -> Self {
        let mut delivered = Self::synthetic_user(message.framed_text());
        delivered.agent_message = Some(AgentMessageMeta {
            message_id: message.id.clone(),
            from_id: message.from_id.clone(),
            from_name: message.from_name.clone(),
            to_id: message.to_id.clone(),
            to_name: message.to_name.clone(),
            kind: message.kind,
            text: message.text.clone(),
            data: message.data.clone(),
        });
        delivered
    }

    pub fn assistant() -> Self {
        Self::new(Role::Assistant)
    }

    pub fn from_tool_result(result: ToolResult) -> Self {
        let mut message = Self::new(Role::Tool);
        message.blocks.push(ContentBlock::ToolResult { result });
        message
    }

    pub fn push_text(&mut self, delta: impl AsRef<str>) {
        let delta = delta.as_ref();
        if delta.is_empty() {
            return;
        }
        match self.blocks.last_mut() {
            Some(ContentBlock::Text { text }) => text.push_str(delta),
            _ => self.blocks.push(ContentBlock::Text {
                text: delta.to_string(),
            }),
        }
    }

    pub fn push_reasoning(&mut self, delta: impl AsRef<str>) {
        let delta = delta.as_ref();
        if delta.is_empty() {
            return;
        }
        match self.blocks.last_mut() {
            Some(ContentBlock::Reasoning { text }) => text.push_str(delta),
            _ => self.blocks.push(ContentBlock::Reasoning {
                text: delta.to_string(),
            }),
        }
    }

    pub fn push_tool_call(&mut self, call: ToolCall) {
        self.blocks.push(ContentBlock::ToolCall { call });
    }

    pub fn set_text(&mut self, value: impl Into<String>) {
        let value = value.into();
        if let Some(ContentBlock::Text { text }) = self
            .blocks
            .iter_mut()
            .find(|block| matches!(block, ContentBlock::Text { .. }))
        {
            *text = value;
        } else if !value.is_empty() {
            self.blocks.insert(0, ContentBlock::Text { text: value });
        }
    }

    pub fn set_reasoning(&mut self, value: impl Into<String>) {
        let value = value.into();
        if let Some(ContentBlock::Reasoning { text }) = self
            .blocks
            .iter_mut()
            .find(|block| matches!(block, ContentBlock::Reasoning { .. }))
        {
            *text = value;
        } else if !value.is_empty() {
            self.blocks
                .insert(0, ContentBlock::Reasoning { text: value });
        }
    }

    pub fn text(&self) -> String {
        let mut out = String::new();
        for block in &self.blocks {
            if let ContentBlock::Text { text } = block {
                out.push_str(text);
            }
        }
        out
    }

    pub fn reasoning(&self) -> String {
        let mut out = String::new();
        for block in &self.blocks {
            if let ContentBlock::Reasoning { text } = block {
                out.push_str(text);
            }
        }
        out
    }

    pub fn tool_calls(&self) -> Vec<ToolCall> {
        self.blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolCall { call } => Some(call.clone()),
                _ => None,
            })
            .collect()
    }

    pub fn tool_result(&self) -> Option<ToolResult> {
        self.blocks.iter().find_map(|block| match block {
            ContentBlock::ToolResult { result } => Some(result.clone()),
            _ => None,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    pub fn preview(&self, max_chars: usize) -> String {
        let raw = match self.role {
            Role::User => self.text(),
            Role::Assistant => self.text(),
            Role::Tool => self.tool_result().map(|r| r.content).unwrap_or_default(),
            Role::System => self.text(),
        };
        let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        truncate_chars(&collapsed, max_chars)
    }
}

pub fn truncate_chars(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    let mut out: String = input.chars().take(max_chars).collect();
    out.push('…');
    out
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
    /// Tokens served from a provider-side prompt cache, when reported.
    #[serde(default)]
    pub cached_tokens: u64,
    /// Prompt size of the most recent request, i.e. the *current* context
    /// footprint. Every other counter here is cumulative across the session;
    /// divide this one by the context window to get the context fill ratio.
    #[serde(default)]
    pub last_prompt_tokens: u64,
}

impl Usage {
    pub fn add(&mut self, other: &Usage) {
        self.prompt_tokens += other.prompt_tokens;
        self.completion_tokens += other.completion_tokens;
        self.total_tokens += other.total_tokens;
        self.cached_tokens += other.cached_tokens;
        self.last_prompt_tokens = other.prompt_tokens;
    }

    pub fn estimated(total_tokens: u64) -> Self {
        Self {
            prompt_tokens: total_tokens,
            completion_tokens: 0,
            total_tokens,
            cached_tokens: 0,
            last_prompt_tokens: total_tokens,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    Other(String),
}

impl FinishReason {
    pub fn from_wire(value: Option<&str>) -> Self {
        match value {
            Some("stop") | None => FinishReason::Stop,
            Some("length") => FinishReason::Length,
            Some("tool_calls") | Some("function_call") => FinishReason::ToolCalls,
            Some("content_filter") => FinishReason::ContentFilter,
            Some(other) => FinishReason::Other(other.to_string()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
}

impl TodoStatus {
    pub fn label(self) -> &'static str {
        match self {
            TodoStatus::Pending => "pending",
            TodoStatus::InProgress => "in_progress",
            TodoStatus::Completed => "completed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    #[serde(default)]
    pub status: TodoStatus,
}

impl TodoItem {
    pub fn new(content: impl Into<String>, status: TodoStatus) -> Self {
        Self {
            id: new_id(),
            content: content.into(),
            status,
        }
    }
}

#[cfg(test)]
mod usage_tests {
    use super::*;

    #[test]
    fn mid_turn_flag_round_trips_and_defaults_false_for_old_sessions() {
        let mut message = Message::mid_turn_user("sent while running");
        message.mid_turn = true;
        let encoded = serde_json::to_string(&message).unwrap();
        assert!(encoded.contains("mid_turn"));

        // A session saved before the flag existed must load unchanged.
        let legacy = r#"{"id":"m1","role":"user","created_at":"2026-01-01T00:00:00Z","blocks":[{"type":"text","text":"old"}]}"#;
        let loaded: Message = serde_json::from_str(legacy).unwrap();
        assert!(!loaded.mid_turn);
    }

    #[test]
    fn add_keeps_last_prompt_tokens_as_the_current_footprint() {
        let mut usage = Usage::default();
        usage.add(&Usage {
            prompt_tokens: 1_000,
            completion_tokens: 100,
            total_tokens: 1_100,
            cached_tokens: 800,
            last_prompt_tokens: 0,
        });
        assert_eq!(usage.total_tokens, 1_100);
        assert_eq!(usage.last_prompt_tokens, 1_000);

        usage.add(&Usage {
            prompt_tokens: 2_500,
            completion_tokens: 50,
            total_tokens: 2_550,
            cached_tokens: 0,
            last_prompt_tokens: 0,
        });
        // Cumulative counters keep growing, but `last_prompt_tokens` tracks
        // the most recent request — the value the context fill ratio uses.
        assert_eq!(usage.total_tokens, 3_650);
        assert_eq!(usage.last_prompt_tokens, 2_500);
    }
}
