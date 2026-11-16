//! Anthropic Messages adapter.
//!
//! Covers the native Anthropic API and the growing set of compatibility
//! endpoints that speak the same wire format (Z.ai, Kimi, ...). The runtime's
//! transcript is OpenAI-shaped, so the adapter translates tool calls into
//! `tool_use` blocks and consecutive tool results into a single `tool_result`
//! user message.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::context::estimate_tokens;
use crate::error::ProviderError;
use crate::ids::new_id;
use crate::message::{FinishReason, ImageAttachment, ToolCall, Usage};
use crate::provider::{ChatRequest, Completion, ModelProvider, ProviderEvent, send_event};
use crate::providers::{estimate_request_tokens, http_error, sse_data, take_sse_event};

#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    pub base_url: String,
    pub api_key: String,
    pub context_window: usize,
    pub extra_headers: Vec<(String, String)>,
    pub anthropic_version: String,
    pub request_timeout: Duration,
    pub connect_timeout: Duration,
}

impl AnthropicConfig {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            base_url: "https://api.anthropic.com".to_string(),
            api_key: api_key.into(),
            context_window: 200_000,
            extra_headers: Vec::new(),
            anthropic_version: "2023-06-01".to_string(),
            request_timeout: Duration::from_secs(300),
            connect_timeout: Duration::from_secs(20),
        }
    }
}

pub struct AnthropicMessagesProvider {
    config: AnthropicConfig,
    client: reqwest::Client,
}

impl AnthropicMessagesProvider {
    pub fn new(config: AnthropicConfig) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .timeout(config.request_timeout)
            .build()
            .map_err(|error| {
                ProviderError::Other(format!("failed to build HTTP client: {error}"))
            })?;
        Ok(Self { config, client })
    }

    fn endpoint(&self) -> String {
        let base = self.config.base_url.trim().trim_end_matches('/');
        if base.ends_with("/v1/messages") || base.ends_with("/messages") {
            base.to_string()
        } else if base.ends_with("/v1") {
            format!("{base}/messages")
        } else {
            format!("{base}/v1/messages")
        }
    }
}

#[async_trait]
impl ModelProvider for AnthropicMessagesProvider {
    fn name(&self) -> &str {
        "anthropic-messages"
    }

    fn context_window(&self) -> usize {
        self.config.context_window
    }

    async fn stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
        events: UnboundedSender<ProviderEvent>,
    ) -> Result<Completion, ProviderError> {
        let body = build_body(&request);
        let mut builder = self
            .client
            .post(self.endpoint())
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .header("anthropic-version", &self.config.anthropic_version);
        if !self.config.api_key.trim().is_empty() {
            builder = builder.header("x-api-key", self.config.api_key.trim());
        }
        for (key, value) in &self.config.extra_headers {
            builder = builder.header(key, value);
        }

        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
            result = builder.json(&body).send() => result.map_err(|error| {
                if error.is_timeout() {
                    ProviderError::Network("request timed out".to_string())
                } else {
                    ProviderError::Network(error.to_string())
                }
            })?,
        };

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(http_error(status.as_u16(), body));
        }

        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        let mut partial = PartialAnthropic::default();
        let mut done = false;

        while !done {
            let chunk = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
                chunk = stream.next() => chunk,
            };
            let Some(chunk) = chunk else {
                break;
            };
            let bytes = chunk.map_err(|error| ProviderError::Network(error.to_string()))?;
            buffer.push_str(&String::from_utf8_lossy(&bytes));

            while let Some(event) = take_sse_event(&mut buffer) {
                match apply_sse_event(&event, &mut partial, &events)? {
                    SseOutcome::Data => {}
                    SseOutcome::Done => {
                        done = true;
                        break;
                    }
                }
            }
        }

        if !done && !buffer.trim().is_empty() {
            if let SseOutcome::Done = apply_sse_event(&buffer, &mut partial, &events)? {
                done = true;
            }
        }

        // A closed socket without `message_stop` is a truncated response.
        // Accepting it as `stop` turned a dropped connection into a silently
        // finished turn; make it a retryable network error the UI can show.
        if !done && partial.finish_reason().is_none() {
            return Err(ProviderError::Network(
                "stream ended before the response completed (missing message_stop)".to_string(),
            ));
        }

        let mut usage = partial.usage.clone();
        if usage.total_tokens == 0 {
            usage.prompt_tokens = estimate_request_tokens(&request.messages) as u64;
            usage.completion_tokens =
                (estimate_tokens(&partial.text) + estimate_tokens(&partial.reasoning)) as u64;
            usage.total_tokens = usage.prompt_tokens + usage.completion_tokens;
        }

        let tool_calls = partial.finish_tool_calls();
        let finish_reason = partial.finish_reason().unwrap_or_else(|| {
            if tool_calls.is_empty() {
                FinishReason::Stop
            } else {
                FinishReason::ToolCalls
            }
        });

        Ok(Completion {
            text: partial.text,
            reasoning: partial.reasoning,
            tool_calls,
            usage,
            finish_reason: Some(finish_reason),
        })
    }
}

fn build_body(request: &ChatRequest) -> Value {
    let mut system = String::new();
    let mut messages: Vec<Value> = Vec::new();
    let mut pending_tool_results: Vec<Value> = Vec::new();

    let flush_tool_results = |messages: &mut Vec<Value>, pending: &mut Vec<Value>| {
        if !pending.is_empty() {
            messages.push(json!({
                "role": "user",
                "content": std::mem::take(pending),
            }));
        }
    };

    for message in &request.messages {
        match message.role.as_str() {
            "system" => {
                if !message.content.trim().is_empty() {
                    if !system.is_empty() {
                        system.push_str("\n\n");
                    }
                    system.push_str(&message.content);
                }
            }
            "assistant" => {
                flush_tool_results(&mut messages, &mut pending_tool_results);
                let mut blocks = Vec::new();
                if !message.content.is_empty() {
                    blocks.push(json!({"type": "text", "text": message.content}));
                }
                for call in &message.tool_calls {
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call.id,
                        "name": call.name,
                        "input": call.arguments,
                    }));
                }
                if !blocks.is_empty() {
                    messages.push(json!({"role": "assistant", "content": blocks}));
                }
            }
            "tool" => {
                let content = if message.images.is_empty() {
                    json!(message.content)
                } else {
                    let mut blocks = vec![json!({"type": "text", "text": message.content})];
                    blocks.extend(message.images.iter().map(image_block));
                    Value::Array(blocks)
                };
                pending_tool_results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": message.tool_call_id.clone().unwrap_or_default(),
                    "content": content,
                }));
            }
            _ => {
                flush_tool_results(&mut messages, &mut pending_tool_results);
                let mut blocks = Vec::new();
                if !message.content.is_empty() {
                    blocks.push(json!({"type": "text", "text": message.content}));
                }
                blocks.extend(message.images.iter().map(image_block));
                if !blocks.is_empty() {
                    messages.push(json!({
                        "role": "user",
                        "content": blocks,
                    }));
                }
            }
        }
    }
    flush_tool_results(&mut messages, &mut pending_tool_results);

    let mut body = json!({
        "model": request.model,
        // Anthropic requires the field on every request. The harness always
        // sends the per-model budget; the fallback only covers direct callers
        // (tests, probes) that build a request by hand.
        "max_tokens": request
            .max_tokens
            .unwrap_or(crate::model_config::DEFAULT_MAX_OUTPUT_TOKENS),
        "messages": messages,
        "stream": true,
    });
    if !system.is_empty() {
        body["system"] = json!(system);
    }
    if let Some(temperature) = request.temperature {
        body["temperature"] = json!(temperature);
    }
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(
            request
                .tools
                .iter()
                .map(|spec| {
                    json!({
                        "name": spec.name,
                        "description": spec.description,
                        "input_schema": spec.parameters,
                    })
                })
                .collect(),
        );
        body["tool_choice"] = json!({"type": "auto"});
    }
    body
}

fn image_block(image: &ImageAttachment) -> Value {
    json!({
        "type": "image",
        "source": {
            "type": "base64",
            "media_type": image.mime_type,
            "data": image.data,
        }
    })
}

#[derive(Debug)]
enum SseOutcome {
    Data,
    Done,
}

#[derive(Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
struct PartialAnthropic {
    text: String,
    reasoning: String,
    tool_calls: BTreeMap<usize, PartialToolCall>,
    usage: Usage,
    stop_reason: Option<String>,
}

impl PartialAnthropic {
    fn finish_tool_calls(&mut self) -> Vec<ToolCall> {
        let mut calls = Vec::new();
        for (_, partial) in std::mem::take(&mut self.tool_calls) {
            if partial.name.is_empty() {
                continue;
            }
            let trimmed = partial.arguments.trim();
            let (arguments, parse_error) = if trimmed.is_empty() {
                (json!({}), None)
            } else {
                match serde_json::from_str::<Value>(trimmed) {
                    Ok(value) => (value, None),
                    Err(error) => (json!({}), Some(error.to_string())),
                }
            };
            calls.push(ToolCall {
                id: if partial.id.is_empty() {
                    format!("call_{}", new_id())
                } else {
                    partial.id
                },
                name: partial.name,
                arguments,
                argument_parse_error: parse_error,
            });
        }
        calls
    }

    fn finish_reason(&self) -> Option<FinishReason> {
        self.stop_reason.as_deref().map(|reason| match reason {
            "end_turn" | "stop_sequence" => FinishReason::Stop,
            "max_tokens" => FinishReason::Length,
            "tool_use" => FinishReason::ToolCalls,
            "refusal" => FinishReason::ContentFilter,
            other => FinishReason::Other(other.to_string()),
        })
    }
}

fn apply_sse_event(
    event: &str,
    partial: &mut PartialAnthropic,
    events: &UnboundedSender<ProviderEvent>,
) -> Result<SseOutcome, ProviderError> {
    let Some(data) = sse_data(event) else {
        return Ok(SseOutcome::Data);
    };
    if data.trim() == "[DONE]" {
        return Ok(SseOutcome::Done);
    }
    let value: Value = serde_json::from_str(&data)
        .map_err(|error| ProviderError::Parse(format!("invalid SSE JSON: {error}")))?;

    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| error.to_string());
        return Err(ProviderError::Api(message));
    }

    let event_type = value.get("type").and_then(Value::as_str).unwrap_or("");

    match event_type {
        "message_start" => {
            if let Some(usage) = value
                .get("message")
                .and_then(|message| message.get("usage"))
            {
                apply_usage(usage, &mut partial.usage);
            }
        }
        "content_block_start" => {
            let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let Some(block) = value.get("content_block") else {
                return Ok(SseOutcome::Data);
            };
            if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                let entry = partial.tool_calls.entry(index).or_default();
                if let Some(id) = block.get("id").and_then(Value::as_str) {
                    entry.id = id.to_string();
                }
                if let Some(name) = block.get("name").and_then(Value::as_str) {
                    entry.name = name.to_string();
                }
                if let Some(input) = block.get("input") {
                    if input.is_object() && input.as_object().is_some_and(|value| !value.is_empty())
                    {
                        entry.arguments = input.to_string();
                    }
                }
            }
        }
        "content_block_delta" => {
            let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let Some(delta) = value.get("delta") else {
                return Ok(SseOutcome::Data);
            };
            match delta.get("type").and_then(Value::as_str).unwrap_or("") {
                "text_delta" => {
                    if let Some(text) = delta.get("text").and_then(Value::as_str) {
                        if !text.is_empty() {
                            partial.text.push_str(text);
                            send_event(events, ProviderEvent::TextDelta(text.to_string()))?;
                        }
                    }
                }
                "thinking_delta" => {
                    if let Some(thinking) = delta.get("thinking").and_then(Value::as_str) {
                        if !thinking.is_empty() {
                            partial.reasoning.push_str(thinking);
                            send_event(
                                events,
                                ProviderEvent::ReasoningDelta(thinking.to_string()),
                            )?;
                        }
                    }
                }
                "input_json_delta" => {
                    if let Some(json) = delta.get("partial_json").and_then(Value::as_str) {
                        partial
                            .tool_calls
                            .entry(index)
                            .or_default()
                            .arguments
                            .push_str(json);
                    }
                }
                _ => {}
            }
        }
        "message_delta" => {
            if let Some(delta) = value.get("delta") {
                if let Some(reason) = delta.get("stop_reason").and_then(Value::as_str) {
                    partial.stop_reason = Some(reason.to_string());
                }
            }
            if let Some(usage) = value.get("usage") {
                apply_usage(usage, &mut partial.usage);
            }
        }
        "message_stop" => return Ok(SseOutcome::Done),
        _ => {}
    }

    Ok(SseOutcome::Data)
}

fn apply_usage(usage: &Value, target: &mut Usage) {
    if let Some(input) = usage.get("input_tokens").and_then(Value::as_u64) {
        target.prompt_tokens = input;
    }
    if let Some(output) = usage.get("output_tokens").and_then(Value::as_u64) {
        target.completion_tokens = output;
    }
    target.total_tokens = target.prompt_tokens + target.completion_tokens;
    if let Some(cached) = usage
        .get("cache_read_input_tokens")
        .and_then(Value::as_u64)
        .or_else(|| usage.get("cached_tokens").and_then(Value::as_u64))
    {
        target.cached_tokens = cached;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ChatMessage, ToolSpec};

    fn harness() -> (
        PartialAnthropic,
        UnboundedSender<ProviderEvent>,
        tokio::sync::mpsc::UnboundedReceiver<ProviderEvent>,
    ) {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        (PartialAnthropic::default(), sender, receiver)
    }

    #[test]
    fn wire_messages_map_tools_and_tool_results() {
        let mut assistant = ChatMessage::assistant(
            "checking",
            vec![ToolCall::new("read_file", json!({"path": "a.txt"}))],
        );
        assistant.tool_calls[0].id = "call_1".to_string();
        let request = ChatRequest {
            model: "claude".to_string(),
            messages: vec![
                ChatMessage::system("sys"),
                ChatMessage::user("hello"),
                assistant,
                ChatMessage::tool("call_1", "file body"),
            ],
            tools: vec![ToolSpec {
                name: "read_file".to_string(),
                description: "read".to_string(),
                parameters: json!({"type": "object"}),
            }],
            temperature: Some(0.2),
            max_tokens: Some(128),
            reasoning_effort: None,
        };
        let body = build_body(&request);
        assert_eq!(body["system"], "sys");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][1]["content"][1]["type"], "tool_use");
        assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(body["max_tokens"], 128);
    }

    #[test]
    fn wire_messages_emit_anthropic_image_blocks() {
        let request = ChatRequest {
            model: "claude".to_string(),
            messages: vec![ChatMessage::user_with_images(
                "screenshot",
                vec![crate::message::ImageAttachment::png("YWJj", 640, 480)],
            )],
            tools: Vec::new(),
            temperature: None,
            max_tokens: Some(128),
            reasoning_effort: None,
        };
        let body = build_body(&request);
        assert_eq!(body["messages"][0]["content"][1]["type"], "image");
        assert_eq!(
            body["messages"][0]["content"][1]["source"]["media_type"],
            "image/png"
        );
        assert_eq!(body["messages"][0]["content"][1]["source"]["data"], "YWJj");
    }

    #[test]
    fn parses_text_thinking_and_tool_use_stream() {
        let (mut partial, sender, mut receiver) = harness();
        apply_sse_event(
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":12,\"output_tokens\":0}}}",
            &mut partial,
            &sender,
        )
        .unwrap();
        apply_sse_event(
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"hmm\"}}",
            &mut partial,
            &sender,
        )
        .unwrap();
        apply_sse_event(
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}",
            &mut partial,
            &sender,
        )
        .unwrap();
        apply_sse_event(
            "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read_file\",\"input\":{}}}",
            &mut partial,
            &sender,
        )
        .unwrap();
        apply_sse_event(
            "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}",
            &mut partial,
            &sender,
        )
        .unwrap();
        apply_sse_event(
            "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"a.txt\\\"}\"}}",
            &mut partial,
            &sender,
        )
        .unwrap();
        apply_sse_event(
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":7}}",
            &mut partial,
            &sender,
        )
        .unwrap();
        apply_sse_event("data: {\"type\":\"message_stop\"}", &mut partial, &sender).unwrap();

        assert_eq!(partial.text, "hi");
        assert_eq!(partial.reasoning, "hmm");
        assert_eq!(partial.usage.prompt_tokens, 12);
        assert_eq!(partial.usage.completion_tokens, 7);
        let calls = partial.finish_tool_calls();
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].arguments, json!({"path": "a.txt"}));
        assert!(partial.finish_reason().is_none() || partial.stop_reason.is_some());

        let events: Vec<_> = std::iter::from_fn(|| receiver.try_recv().ok()).collect();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ProviderEvent::TextDelta(text) if text == "hi"))
        );
    }

    #[test]
    fn errors_are_surfaced() {
        let (mut partial, sender, _receiver) = harness();
        let error = apply_sse_event(
            "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"overloaded\"}}",
            &mut partial,
            &sender,
        )
        .unwrap_err();
        assert!(error.is_retryable());
    }
}
