//! OpenAI Responses API adapter.
//!
//! The Responses wire format differs from Chat Completions: system prompts are
//! `instructions`, tool calls are top-level `function_call` items and stream
//! events are typed (`response.output_text.delta`, ...). The adapter keeps the
//! same runtime-facing [`crate::ModelProvider`] contract as the other adapters.

use std::collections::{BTreeMap, HashMap};
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
pub struct OpenAiResponsesConfig {
    pub base_url: String,
    pub api_key: String,
    pub context_window: usize,
    pub extra_headers: Vec<(String, String)>,
    pub request_timeout: Duration,
    pub connect_timeout: Duration,
}

impl OpenAiResponsesConfig {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: api_key.into(),
            context_window: 400_000,
            extra_headers: Vec::new(),
            request_timeout: Duration::from_secs(300),
            connect_timeout: Duration::from_secs(20),
        }
    }
}

pub struct OpenAiResponsesProvider {
    config: OpenAiResponsesConfig,
    client: reqwest::Client,
}

impl OpenAiResponsesProvider {
    pub fn new(config: OpenAiResponsesConfig) -> Result<Self, ProviderError> {
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
        if base.ends_with("/responses") {
            base.to_string()
        } else if base.ends_with("/v1") {
            format!("{base}/responses")
        } else {
            format!("{base}/v1/responses")
        }
    }
}

#[async_trait]
impl ModelProvider for OpenAiResponsesProvider {
    fn name(&self) -> &str {
        "openai-responses"
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
            .header("Accept", "text/event-stream");
        if !self.config.api_key.trim().is_empty() {
            builder = builder.bearer_auth(self.config.api_key.trim());
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
        let mut partial = PartialResponses::default();
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
            let _ = apply_sse_event(&buffer, &mut partial, &events)?;
        }

        let mut usage = partial.usage.clone();
        if usage.total_tokens == 0 {
            usage.prompt_tokens = estimate_request_tokens(&request.messages) as u64;
            usage.completion_tokens =
                (estimate_tokens(&partial.text) + estimate_tokens(&partial.reasoning)) as u64;
            usage.total_tokens = usage.prompt_tokens + usage.completion_tokens;
        }

        let tool_calls = partial.finish_tool_calls();
        let finish_reason = partial.finish_reason.unwrap_or_else(|| {
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
    let mut instructions = String::new();
    let mut input: Vec<Value> = Vec::new();

    for message in &request.messages {
        match message.role.as_str() {
            "system" | "developer" => {
                if !message.content.trim().is_empty() {
                    if !instructions.is_empty() {
                        instructions.push_str("\n\n");
                    }
                    instructions.push_str(&message.content);
                }
            }
            "assistant" => {
                if !message.content.is_empty() {
                    input.push(json!({
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": message.content}],
                    }));
                }
                for call in &message.tool_calls {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": call.id,
                        "name": call.name,
                        "arguments": serde_json::to_string(&call.arguments)
                            .unwrap_or_else(|_| "{}".to_string()),
                    }));
                }
            }
            "tool" => {
                let output = if message.images.is_empty() {
                    json!(message.content)
                } else {
                    let mut blocks = vec![json!({"type": "input_text", "text": message.content})];
                    blocks.extend(message.images.iter().map(image_block));
                    Value::Array(blocks)
                };
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": message.tool_call_id.clone().unwrap_or_default(),
                    "output": output,
                }));
            }
            _ => {
                let mut blocks = Vec::new();
                if !message.content.is_empty() {
                    blocks.push(json!({"type": "input_text", "text": message.content}));
                }
                blocks.extend(message.images.iter().map(image_block));
                if !blocks.is_empty() {
                    input.push(json!({
                        "role": "user",
                        "content": blocks,
                    }));
                }
            }
        }
    }

    let mut body = json!({
        "model": request.model,
        "input": input,
        "stream": true,
        "store": false,
    });
    if !instructions.is_empty() {
        body["instructions"] = json!(instructions);
    }
    if let Some(temperature) = request.temperature {
        body["temperature"] = json!(temperature);
    }
    if let Some(max_tokens) = request.max_tokens {
        body["max_output_tokens"] = json!(max_tokens);
    }
    if let Some(level) = request
        .reasoning_effort
        .as_deref()
        .filter(|level| !level.trim().is_empty())
    {
        body["reasoning"] = json!({"effort": level});
    }
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(
            request
                .tools
                .iter()
                .map(|spec| {
                    json!({
                        "type": "function",
                        "name": spec.name,
                        "description": spec.description,
                        "parameters": spec.parameters,
                    })
                })
                .collect(),
        );
        body["tool_choice"] = json!("auto");
    }
    body
}

fn image_block(image: &ImageAttachment) -> Value {
    json!({
        "type": "input_image",
        "image_url": image.data_url(),
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
struct PartialResponses {
    text: String,
    reasoning: String,
    tool_calls: BTreeMap<usize, PartialToolCall>,
    item_index: HashMap<String, usize>,
    next_index: usize,
    usage: Usage,
    finish_reason: Option<FinishReason>,
}

impl PartialResponses {
    fn index_for_item(&mut self, item_id: &str) -> usize {
        if let Some(index) = self.item_index.get(item_id) {
            return *index;
        }
        let index = self.next_index;
        self.next_index += 1;
        self.item_index.insert(item_id.to_string(), index);
        index
    }

    fn index_for_value(&mut self, item: &Value) -> usize {
        if let Some(item_id) = item.get("id").and_then(Value::as_str) {
            self.index_for_item(item_id)
        } else {
            let index = self.next_index;
            self.next_index += 1;
            index
        }
    }

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
}

fn apply_sse_event(
    event: &str,
    partial: &mut PartialResponses,
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

    let event_type = value.get("type").and_then(Value::as_str).unwrap_or("");

    if event_type == "error" {
        let message = value
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .or_else(|| value.get("message").and_then(Value::as_str))
            .unwrap_or("unknown responses error");
        return Err(ProviderError::Api(message.to_string()));
    }

    match event_type {
        "response.output_text.delta" => {
            if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                if !delta.is_empty() {
                    partial.text.push_str(delta);
                    send_event(events, ProviderEvent::TextDelta(delta.to_string()))?;
                }
            }
        }
        "response.output_text.done" => {
            if partial.text.is_empty() {
                if let Some(text) = value.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        partial.text.push_str(text);
                        send_event(events, ProviderEvent::TextDelta(text.to_string()))?;
                    }
                }
            }
        }
        "response.reasoning_text.delta"
        | "response.reasoning_summary_text.delta"
        | "response.reasoning.delta" => {
            if let Some(delta) = value
                .get("delta")
                .or_else(|| value.get("text"))
                .and_then(Value::as_str)
            {
                if !delta.is_empty() {
                    partial.reasoning.push_str(delta);
                    send_event(events, ProviderEvent::ReasoningDelta(delta.to_string()))?;
                }
            }
        }
        "response.output_item.added" => {
            let Some(item) = value.get("item") else {
                return Ok(SseOutcome::Data);
            };
            if item.get("type").and_then(Value::as_str) == Some("function_call") {
                let index = partial.index_for_value(item);
                let entry = partial.tool_calls.entry(index).or_default();
                if let Some(call_id) = item.get("call_id").and_then(Value::as_str) {
                    entry.id = call_id.to_string();
                }
                if let Some(name) = item.get("name").and_then(Value::as_str) {
                    entry.name = name.to_string();
                }
                if let Some(arguments) = item.get("arguments").and_then(Value::as_str) {
                    if !arguments.is_empty() {
                        entry.arguments.push_str(arguments);
                    }
                }
            }
        }
        "response.function_call_arguments.delta" => {
            let item_id = value
                .get("item_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                let index = partial.index_for_item(item_id);
                partial
                    .tool_calls
                    .entry(index)
                    .or_default()
                    .arguments
                    .push_str(delta);
            }
        }
        "response.function_call_arguments.done" => {
            let item_id = value
                .get("item_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if let Some(arguments) = value.get("arguments").and_then(Value::as_str) {
                let index = partial.index_for_item(item_id);
                partial.tool_calls.entry(index).or_default().arguments = arguments.to_string();
            }
        }
        "response.output_item.done" => {
            let Some(item) = value.get("item") else {
                return Ok(SseOutcome::Data);
            };
            if item.get("type").and_then(Value::as_str) == Some("function_call") {
                let index = partial.index_for_value(item);
                let entry = partial.tool_calls.entry(index).or_default();
                if let Some(call_id) = item.get("call_id").and_then(Value::as_str) {
                    entry.id = call_id.to_string();
                }
                if let Some(name) = item.get("name").and_then(Value::as_str) {
                    entry.name = name.to_string();
                }
                if let Some(arguments) = item.get("arguments").and_then(Value::as_str) {
                    if !arguments.is_empty() {
                        entry.arguments = arguments.to_string();
                    }
                }
            }
        }
        "response.completed" => {
            if let Some(response) = value.get("response") {
                if let Some(usage) = response.get("usage") {
                    apply_usage(usage, &mut partial.usage);
                }
            }
            partial.finish_reason = Some(if partial.tool_calls.is_empty() {
                FinishReason::Stop
            } else {
                FinishReason::ToolCalls
            });
            return Ok(SseOutcome::Done);
        }
        "response.incomplete" => {
            if let Some(response) = value.get("response") {
                if let Some(usage) = response.get("usage") {
                    apply_usage(usage, &mut partial.usage);
                }
                if let Some(reason) = response
                    .get("incomplete_details")
                    .and_then(|details| details.get("reason"))
                    .and_then(Value::as_str)
                {
                    partial.finish_reason = Some(match reason {
                        "max_output_tokens" => FinishReason::Length,
                        other => FinishReason::Other(other.to_string()),
                    });
                }
            }
            return Ok(SseOutcome::Done);
        }
        "response.failed" => {
            let message = value
                .get("response")
                .and_then(|response| response.get("error"))
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("responses request failed");
            return Err(ProviderError::Api(message.to_string()));
        }
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
    target.total_tokens = usage
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(target.prompt_tokens + target.completion_tokens);
    if let Some(cached) = usage
        .get("input_tokens_details")
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_u64)
    {
        target.cached_tokens = cached;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ChatMessage, ToolSpec};

    fn harness() -> (
        PartialResponses,
        UnboundedSender<ProviderEvent>,
        tokio::sync::mpsc::UnboundedReceiver<ProviderEvent>,
    ) {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        (PartialResponses::default(), sender, receiver)
    }

    #[test]
    fn wire_body_uses_instructions_and_function_items() {
        let mut assistant =
            ChatMessage::assistant("", vec![ToolCall::new("read_file", json!({"path": "a"}))]);
        assistant.tool_calls[0].id = "call_1".to_string();
        let request = ChatRequest {
            model: "gpt-5.1".to_string(),
            messages: vec![
                ChatMessage::system("sys"),
                ChatMessage::user("hello"),
                assistant,
                ChatMessage::tool("call_1", "body"),
            ],
            tools: vec![ToolSpec {
                name: "read_file".to_string(),
                description: "read".to_string(),
                parameters: json!({"type": "object"}),
            }],
            temperature: None,
            max_tokens: Some(256),
            reasoning_effort: Some("high".to_string()),
        };
        let body = build_body(&request);
        assert_eq!(body["instructions"], "sys");
        assert_eq!(body["input"][1]["type"], "function_call");
        assert_eq!(body["input"][2]["type"], "function_call_output");
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["max_output_tokens"], 256);
        assert_eq!(body["store"], false);
    }

    #[test]
    fn wire_body_emits_input_image_parts() {
        let request = ChatRequest {
            model: "gpt-5.1".to_string(),
            messages: vec![ChatMessage::user_with_images(
                "screenshot",
                vec![crate::message::ImageAttachment::png("YWJj", 640, 480)],
            )],
            tools: Vec::new(),
            temperature: None,
            max_tokens: Some(256),
            reasoning_effort: None,
        };
        let body = build_body(&request);
        assert_eq!(body["input"][0]["content"][1]["type"], "input_image");
        assert_eq!(
            body["input"][0]["content"][1]["image_url"],
            "data:image/png;base64,YWJj"
        );
    }

    #[test]
    fn parses_text_and_function_call_stream() {
        let (mut partial, sender, mut receiver) = harness();
        apply_sse_event(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hel\"}",
            &mut partial,
            &sender,
        )
        .unwrap();
        apply_sse_event(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}",
            &mut partial,
            &sender,
        )
        .unwrap();
        apply_sse_event(
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"id\":\"item_1\",\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"read_file\",\"arguments\":\"\"}}",
            &mut partial,
            &sender,
        )
        .unwrap();
        apply_sse_event(
            "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"item_1\",\"delta\":\"{\\\"path\\\":\"}",
            &mut partial,
            &sender,
        )
        .unwrap();
        apply_sse_event(
            "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"item_1\",\"delta\":\"\\\"a.txt\\\"}\"}",
            &mut partial,
            &sender,
        )
        .unwrap();
        apply_sse_event(
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":4,\"total_tokens\":14,\"input_tokens_details\":{\"cached_tokens\":3}}}}",
            &mut partial,
            &sender,
        )
        .unwrap();

        assert_eq!(partial.text, "hello");
        assert_eq!(partial.usage.total_tokens, 14);
        assert_eq!(partial.usage.cached_tokens, 3);
        let calls = partial.finish_tool_calls();
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].arguments, json!({"path": "a.txt"}));
        let events: Vec<_> = std::iter::from_fn(|| receiver.try_recv().ok()).collect();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ProviderEvent::TextDelta(text) if text == "hel"))
        );
    }

    #[test]
    fn failed_response_is_an_api_error() {
        let (mut partial, sender, _receiver) = harness();
        let error = apply_sse_event(
            "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"boom\"}}}",
            &mut partial,
            &sender,
        )
        .unwrap_err();
        assert!(matches!(error, ProviderError::Api(message) if message == "boom"));
    }
}
