//! OpenAI-compatible chat completions adapter (DeepSeek, OpenAI, Ollama,
//! SiliconFlow, vLLM, ...).
//!
//! The adapter implements streaming, tool-call fragment reassembly and
//! DeepSeek's `reasoning_content` extension. It deliberately keeps the wire
//! format invisible to the rest of the runtime.

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
use crate::message::{FinishReason, ToolCall, Usage};
use crate::provider::{
    ChatMessage, ChatRequest, Completion, ModelProvider, ProviderEvent, send_event,
};
use crate::providers::{estimate_chat_message_tokens, http_error, sse_data, take_sse_event};

#[derive(Debug, Clone)]
pub struct OpenAiCompatConfig {
    /// Either a base URL (`https://api.deepseek.com`) or a full endpoint.
    pub base_url: String,
    pub api_key: String,
    pub context_window: usize,
    pub extra_headers: Vec<(String, String)>,
    pub include_usage: bool,
    pub request_timeout: Duration,
    pub connect_timeout: Duration,
}

impl OpenAiCompatConfig {
    pub fn deepseek(api_key: impl Into<String>) -> Self {
        Self {
            base_url: "https://api.deepseek.com".to_string(),
            api_key: api_key.into(),
            context_window: 128_000,
            extra_headers: Vec::new(),
            include_usage: true,
            request_timeout: Duration::from_secs(300),
            connect_timeout: Duration::from_secs(20),
        }
    }
}

pub struct OpenAiCompatProvider {
    config: OpenAiCompatConfig,
    client: reqwest::Client,
}

impl OpenAiCompatProvider {
    pub fn new(config: OpenAiCompatConfig) -> Result<Self, ProviderError> {
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
        if base.ends_with("/chat/completions") {
            base.to_string()
        } else {
            format!("{base}/chat/completions")
        }
    }
}

#[async_trait]
impl ModelProvider for OpenAiCompatProvider {
    fn name(&self) -> &str {
        "openai-compatible"
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
        let mut body = json!({
            "model": request.model,
            "messages": wire_messages(&request.messages),
            "stream": true,
        });
        if self.config.include_usage {
            body["stream_options"] = json!({ "include_usage": true });
        }
        if let Some(temperature) = request.temperature {
            body["temperature"] = json!(temperature);
        }
        if let Some(max_tokens) = request.max_tokens {
            body["max_tokens"] = json!(max_tokens);
        }
        // Only send the knob when the selected model explicitly exposes
        // reasoning levels; providers that do not understand it reject some
        // request shapes instead of ignoring unknown fields.
        if let Some(level) = request
            .reasoning_effort
            .as_deref()
            .filter(|level| !level.trim().is_empty())
        {
            body["reasoning_effort"] = json!(level);
        }
        if !request.tools.is_empty() {
            body["tools"] = Value::Array(
                request
                    .tools
                    .iter()
                    .map(|spec| {
                        json!({
                            "type": "function",
                            "function": {
                                "name": spec.name,
                                "description": spec.description,
                                "parameters": spec.parameters,
                            }
                        })
                    })
                    .collect(),
            );
            body["tool_choice"] = json!("auto");
        }

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
        let mut partial = PartialCompletion::default();
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

        // A well-behaved server ends events with a blank line, but tolerate a
        // final event without one.
        if !done && !buffer.trim().is_empty() {
            if let SseOutcome::Done = apply_sse_event(&buffer, &mut partial, &events)? {
                done = true;
            }
        }

        // A socket that closes without a terminal chunk is a truncated
        // response, not a finished turn: OpenAI-compatible servers send the
        // finish reason in a final event (and usually `[DONE]`). Defaulting
        // the missing finish to `stop` made a dropped connection look like a
        // successful completion, so the transcript simply stopped mid-word
        // with no error. Surface it as a retryable network error instead.
        if !done && partial.finish_reason.is_none() {
            return Err(ProviderError::Network(
                "stream ended before the response completed (missing finish reason)".to_string(),
            ));
        }

        let mut usage = partial.usage.clone();
        if usage.total_tokens == 0 {
            let prompt_estimate: usize = request
                .messages
                .iter()
                .map(estimate_chat_message_tokens)
                .sum();
            let completion_estimate =
                estimate_tokens(&partial.text) + estimate_tokens(&partial.reasoning);
            usage.prompt_tokens = prompt_estimate as u64;
            usage.completion_tokens = completion_estimate as u64;
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

fn wire_messages(messages: &[ChatMessage]) -> Vec<Value> {
    let mut wire = Vec::with_capacity(messages.len());
    for message in messages {
        match message.role.as_str() {
            "assistant" => {
                let calls: Vec<Value> = message
                    .tool_calls
                    .iter()
                    .map(|call| {
                        json!({
                            "id": call.id,
                            "type": "function",
                            "function": {
                                "name": call.name,
                                "arguments": serde_json::to_string(&call.arguments)
                                    .unwrap_or_else(|_| "{}".to_string()),
                            }
                        })
                    })
                    .collect();
                let content = if message.content.is_empty() && !calls.is_empty() {
                    Value::Null
                } else {
                    Value::String(message.content.clone())
                };
                let mut value = json!({
                    "role": "assistant",
                    "content": content,
                });
                if !calls.is_empty() {
                    value["tool_calls"] = Value::Array(calls);
                }
                wire.push(value);
            }
            "tool" => {
                wire.push(json!({
                    "role": "tool",
                    "tool_call_id": message.tool_call_id.clone().unwrap_or_default(),
                    "content": message.content,
                }));
                // Defensive: a tool message carrying pixels is unusual (the
                // context layer normally groups them into a user message), but
                // never drop them.
                if !message.images.is_empty() {
                    wire.push(user_content_message(message));
                }
            }
            "user" => wire.push(user_content_message(message)),
            role => wire.push(json!({
                "role": role,
                "content": message.content,
            })),
        }
    }
    wire
}

fn user_content_message(message: &ChatMessage) -> Value {
    if message.images.is_empty() {
        return json!({
            "role": "user",
            "content": message.content,
        });
    }
    let mut parts = Vec::new();
    if !message.content.is_empty() {
        parts.push(json!({"type": "text", "text": message.content}));
    }
    for image in &message.images {
        parts.push(json!({
            "type": "image_url",
            "image_url": {"url": image.data_url()},
        }));
    }
    json!({
        "role": "user",
        "content": parts,
    })
}

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
struct PartialCompletion {
    text: String,
    reasoning: String,
    tool_calls: BTreeMap<usize, PartialToolCall>,
    usage: Usage,
    finish_reason: Option<FinishReason>,
}

impl PartialCompletion {
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
    partial: &mut PartialCompletion,
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

    if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
        partial.usage.prompt_tokens = usage
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        partial.usage.completion_tokens = usage
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        partial.usage.total_tokens = usage
            .get("total_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(partial.usage.prompt_tokens + partial.usage.completion_tokens);
        partial.usage.cached_tokens = usage
            .get("prompt_cache_hit_tokens")
            .and_then(Value::as_u64)
            .or_else(|| {
                usage
                    .get("prompt_tokens_details")
                    .and_then(|details| details.get("cached_tokens"))
                    .and_then(Value::as_u64)
            })
            .unwrap_or(0);
    }

    let Some(choices) = value.get("choices").and_then(Value::as_array) else {
        return Ok(SseOutcome::Data);
    };

    for choice in choices {
        let is_first = choice.get("index").and_then(Value::as_u64).unwrap_or(0) == 0;
        if !is_first {
            continue;
        }
        if let Some(finish) = choice.get("finish_reason").and_then(Value::as_str) {
            partial.finish_reason = Some(FinishReason::from_wire(Some(finish)));
        }
        let Some(delta) = choice.get("delta") else {
            continue;
        };

        if let Some(reasoning) = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"))
            .and_then(Value::as_str)
        {
            if !reasoning.is_empty() {
                partial.reasoning.push_str(reasoning);
                send_event(events, ProviderEvent::ReasoningDelta(reasoning.to_string()))?;
            }
        }

        if let Some(content) = delta.get("content").and_then(Value::as_str) {
            if !content.is_empty() {
                partial.text.push_str(content);
                send_event(events, ProviderEvent::TextDelta(content.to_string()))?;
            }
        }

        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for (position, call) in calls.iter().enumerate() {
                let index = call
                    .get("index")
                    .and_then(Value::as_u64)
                    .map(|value| value as usize)
                    .unwrap_or(position);
                let entry = partial.tool_calls.entry(index).or_default();
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    if !id.is_empty() {
                        entry.id = id.to_string();
                    }
                }
                if let Some(function) = call.get("function") {
                    if let Some(name) = function.get("name").and_then(Value::as_str) {
                        if !name.is_empty() {
                            entry.name = name.to_string();
                        }
                    }
                    if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                        entry.arguments.push_str(arguments);
                    }
                }
            }
        }
    }

    Ok(SseOutcome::Data)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Harness {
        partial: PartialCompletion,
        _events: tokio::sync::mpsc::UnboundedReceiver<ProviderEvent>,
        sender: UnboundedSender<ProviderEvent>,
    }

    fn harness() -> Harness {
        let (sender, events) = tokio::sync::mpsc::unbounded_channel();
        Harness {
            partial: PartialCompletion::default(),
            _events: events,
            sender,
        }
    }

    #[test]
    fn parses_text_reasoning_and_tool_fragments() {
        let mut harness = harness();
        apply_sse_event(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"think \"}}]}",
            &mut harness.partial,
            &harness.sender,
        )
        .unwrap();
        apply_sse_event(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hel\"}}]}",
            &mut harness.partial,
            &harness.sender,
        )
        .unwrap();
        apply_sse_event(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"}}]}",
            &mut harness.partial,
            &harness.sender,
        )
        .unwrap();
        apply_sse_event(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\"}}]}}]}",
            &mut harness.partial,
            &harness.sender,
        )
        .unwrap();
        apply_sse_event(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"a.txt\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}",
            &mut harness.partial,
            &harness.sender,
        )
        .unwrap();
        apply_sse_event("data: [DONE]", &mut harness.partial, &harness.sender).unwrap();

        assert_eq!(harness.partial.text, "hello");
        assert_eq!(harness.partial.reasoning, "think ");
        let calls = std::mem::take(&mut harness.partial).finish_tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].arguments, json!({"path": "a.txt"}));
        assert!(calls[0].argument_parse_error.is_none());
    }

    #[test]
    fn sse_event_extraction_handles_crlf_and_partial_events() {
        let mut buffer = "data: one\r\n\r\ndata: two\n\npartial".to_string();
        assert_eq!(take_sse_event(&mut buffer).as_deref(), Some("data: one"));
        assert_eq!(take_sse_event(&mut buffer).as_deref(), Some("data: two"));
        assert!(take_sse_event(&mut buffer).is_none());
        assert_eq!(buffer, "partial");
    }

    #[test]
    fn invalid_tool_arguments_are_reported_not_dropped() {
        let mut partial = PartialCompletion::default();
        partial.tool_calls.insert(
            0,
            PartialToolCall {
                id: "call_1".to_string(),
                name: "write_file".to_string(),
                arguments: "{not json".to_string(),
            },
        );
        let calls = partial.finish_tool_calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].argument_parse_error.is_some());
    }

    #[test]
    fn wire_messages_emit_null_content_for_tool_only_assistant_messages() {
        let messages = vec![ChatMessage::assistant(
            "",
            vec![ToolCall::new("read_file", json!({"path": "a"}))],
        )];
        let wire = wire_messages(&messages);
        assert!(wire[0]["content"].is_null());
        assert_eq!(wire[0]["tool_calls"][0]["function"]["name"], "read_file");
    }

    #[test]
    fn wire_messages_emit_image_parts_for_user_messages() {
        let messages = vec![ChatMessage::user_with_images(
            "screenshot",
            vec![crate::message::ImageAttachment::png("YWJj", 640, 480)],
        )];
        let wire = wire_messages(&messages);
        assert_eq!(wire[0]["content"][0]["type"], "text");
        assert_eq!(wire[0]["content"][1]["type"], "image_url");
        assert_eq!(
            wire[0]["content"][1]["image_url"]["url"],
            "data:image/png;base64,YWJj"
        );
    }

    /// Serve one complete HTTP response whose SSE body is exactly `body`, then
    /// close the connection. This is a well-formed HTTP exchange that ends the
    /// stream without a terminal SSE marker — the shape a dropped provider
    /// connection can also take after the last delta.
    fn serve_sse_once(body: &'static str) -> String {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let Ok((mut socket, _)) = listener.accept() else {
                return;
            };
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes());
            let _ = socket.flush();
        });
        format!("http://{addr}")
    }

    fn test_provider(base_url: String) -> OpenAiCompatProvider {
        OpenAiCompatProvider::new(OpenAiCompatConfig {
            base_url,
            api_key: "test-key".to_string(),
            context_window: 8_192,
            extra_headers: Vec::new(),
            include_usage: false,
            request_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(2),
        })
        .unwrap()
    }

    fn test_request() -> ChatRequest {
        ChatRequest {
            model: "test-model".to_string(),
            messages: vec![ChatMessage::user("hello")],
            tools: Vec::new(),
            temperature: None,
            max_tokens: None,
            reasoning_effort: None,
        }
    }

    /// A stream that closes before any finish reason must be an error, not a
    /// silently completed turn (that is what made the transcript stop
    /// mid-reasoning with no message in the UI).
    #[tokio::test]
    async fn stream_without_a_finish_reason_is_a_network_error() {
        let base_url = serve_sse_once(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"half a thought\"}}]}\n\n",
        );
        let provider = test_provider(base_url);
        let (sender, _events) = tokio::sync::mpsc::unbounded_channel();
        let result = provider
            .stream(test_request(), CancellationToken::new(), sender)
            .await;
        assert!(
            matches!(result, Err(ProviderError::Network(_))),
            "expected a network error, got {result:?}"
        );
    }

    /// Servers that omit `[DONE]` but do send the terminal finish-reason chunk
    /// are still accepted; otherwise the stricter EOF check would break them.
    #[tokio::test]
    async fn stream_with_a_finish_reason_is_accepted_without_done() {
        let base_url = serve_sse_once(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"answer\"}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        );
        let provider = test_provider(base_url);
        let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
        let completion = provider
            .stream(test_request(), CancellationToken::new(), sender)
            .await
            .unwrap();
        assert_eq!(completion.text, "answer");
        assert_eq!(completion.finish_reason, Some(FinishReason::Stop));
        while let Ok(event) = events.try_recv() {
            if let ProviderEvent::TextDelta(delta) = event {
                assert_eq!(delta, "answer");
            }
        }
    }
}
