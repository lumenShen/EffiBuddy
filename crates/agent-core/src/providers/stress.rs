//! The virtual model provider for the stress lab.
//!
//! Deterministic, stateless per request (safe to share one instance across
//! 1000 concurrent harnesses), and paced at a configurable token rate —
//! 500 tokens/s per stream by default. Scenarios are drawn from
//! [`super::stress_templates`] with a seed derived from the user prompt, so
//! the same session replays the same plan while different sessions diverge.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::error::ProviderError;
use crate::message::{FinishReason, ToolCall, Usage};
use crate::provider::{ChatRequest, Completion, ModelProvider, ProviderEvent, send_event};

use super::stress_templates::{FINAL_TEXTS, REASONING, TOOL_TEMPLATES, USER_PROMPTS};

/// Pacing tick. 20ms keeps event rates realistic (~42 events/s at the default
/// 500 tok/s with 12-token chunks) without one timer per token.
const TICK: Duration = Duration::from_millis(20);
const TICK_SECS: f32 = 0.02;

/// Public user prompts for runners that need to seed sessions
/// (the stress bin and the in-app runner both use this).
pub fn user_prompt(seed: u64) -> &'static str {
    USER_PROMPTS[(seed % USER_PROMPTS.len() as u64) as usize]
}

/// Tiny xorshift* RNG — deterministic, no external dependency.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        // Zero is a fixed point for xorshift; bail out of it.
        Self(seed | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform value in `[0, n)` (`n` is clamped to ≥ 1).
    pub fn below(&mut self, n: u64) -> u64 {
        (self.next_u64() >> 16) % n.max(1)
    }

    pub fn pick<'a, T>(&mut self, slice: &'a [T]) -> &'a T {
        &slice[(self.below(slice.len() as u64) as usize) % slice.len().max(1)]
    }
}

fn hash_seed(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// Weight of one character in "tokens", mirroring [`crate::context::estimate_tokens`]
/// (ASCII ≈ 4 chars/token, CJK ≈ 1 char/token).
fn char_weight(ch: char) -> f32 {
    if ch.is_ascii() { 0.25 } else { 1.0 }
}

#[derive(Debug, Clone)]
enum Step {
    /// Stream a short lead-in, then return these tool templates' calls.
    Tools,
    /// Stream a full markdown answer; the run completes after this step.
    Final,
}

pub struct StressProvider {
    /// Tokens per second **per stream**.
    rate: f32,
    /// Target tokens per `ProviderEvent` (controls event rate = rate/chunk).
    chunk_tokens: f32,
    seed: u64,
    /// Scenario length: 1..=max_steps steps, the last one always `Final`.
    max_steps: usize,
    /// Context window the provider advertises (compaction threshold input).
    context_window_tokens: usize,
    /// Context-fill mode: each session grows its transcript to a random
    /// fraction of the window within `min..max`, then idles with hold
    /// responses. The extreme-memory scenario.
    fill: Option<(f32, f32)>,
}

/// Per-turn context growth cap in fill mode: bounds the transient copies a
/// single turn makes (stream buffer + message + completion clones) while the
/// transcript accumulates over several turns.
const FILL_GROWTH_CAP_TOKENS: f32 = 80_000.0;
/// Slack below the target before more filler is emitted.
const FILL_TARGET_SLACK_TOKENS: f32 = 1_000.0;

impl StressProvider {
    pub fn new() -> Self {
        Self {
            rate: 500.0,
            chunk_tokens: 12.0,
            seed: 0x5EED_B0DD,
            max_steps: 6,
            context_window_tokens: 262_144,
            fill: None,
        }
    }

    pub fn with_rate(mut self, rate: f32) -> Self {
        self.rate = rate.max(1.0);
        self
    }

    pub fn with_chunk_tokens(mut self, chunk_tokens: usize) -> Self {
        self.chunk_tokens = (chunk_tokens.max(1)) as f32;
        self
    }

    pub fn with_seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }

    pub fn with_max_steps(mut self, max_steps: usize) -> Self {
        self.max_steps = max_steps.max(1);
        self
    }

    pub fn with_context_window(mut self, tokens: usize) -> Self {
        self.context_window_tokens = tokens.max(1_000);
        self
    }

    /// Enable context-fill mode: each turn grows the transcript toward a
    /// per-session random fraction of the context window (`min..max`).
    pub fn with_context_fill(mut self, min: f32, max: f32) -> Self {
        self.fill = Some((min.clamp(0.01, 1.0), max.clamp(0.01, 1.0)));
        self
    }

    pub fn rate(&self) -> f32 {
        self.rate
    }

    /// The scenario plan for one turn: which step is which.
    ///
    /// `step` is derived from the transcript (number of completed tool steps
    /// since the last user message), so consecutive model calls walk the plan
    /// without any provider-side state.
    fn step_kind(&self, rng: &mut Rng, step: usize) -> Step {
        let total = 1 + (rng.below(self.max_steps as u64) as usize);
        if step + 1 >= total {
            Step::Final
        } else {
            Step::Tools
        }
    }

    /// Stream `text` at `rate` tokens/s. Returns tokens actually emitted
    /// (fractional weights summed, rounded up).
    async fn paced_stream(
        &self,
        text: &str,
        make_event: fn(String) -> ProviderEvent,
        cancel: &CancellationToken,
        events: &UnboundedSender<ProviderEvent>,
    ) -> Result<u64, ProviderError> {
        use tokio::time::MissedTickBehavior;

        let mut interval = tokio::time::interval(TICK);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut chars = text.chars().peekable();
        let mut carried: f32 = 0.0;
        let mut weight_emitted: f32 = 0.0;

        loop {
            interval.tick().await;
            if cancel.is_cancelled() {
                return Err(ProviderError::Cancelled);
            }
            // Exact token-bucket accounting: every character emitted this
            // tick (including chunk-boundary overshoot) is debited against
            // the next tick's budget, so the long-run rate converges on
            // `rate` instead of drifting high by up to one chunk per tick.
            let start_budget = self.rate * TICK_SECS + carried;
            let mut w_tick = 0.0f32;
            let mut chunk = String::new();
            let mut chunk_weight = 0.0f32;
            while start_budget - w_tick >= 1.0 {
                let Some(ch) = chars.next() else { break };
                chunk.push(ch);
                let w = char_weight(ch);
                w_tick += w;
                chunk_weight += w;
                if chunk_weight >= self.chunk_tokens {
                    weight_emitted += chunk_weight;
                    send_event(events, make_event(std::mem::take(&mut chunk)))?;
                    chunk_weight = 0.0;
                }
            }
            if !chunk.is_empty() {
                weight_emitted += chunk_weight;
                send_event(events, make_event(chunk))?;
            }
            carried = (start_budget - w_tick).clamp(-1.0, 64.0);
            if chars.peek().is_none() {
                break;
            }
        }
        Ok(weight_emitted.ceil() as u64)
    }
}

impl Default for StressProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl StressProvider {
    /// Context-fill turn: grow the transcript toward the session's random
    /// watermark, then hold with a minimal response so the memory footprint
    /// stays at the target.
    async fn stream_fill(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
        events: UnboundedSender<ProviderEvent>,
        min_frac: f32,
        max_frac: f32,
    ) -> Result<Completion, ProviderError> {
        let prompt_tokens = super::estimate_request_tokens(&request.messages) as f32;
        let mut rng = Rng::new(hash_seed(&first_user_content(&request)) ^ self.seed ^ 0xF11A_0001);
        let frac = min_frac + (max_frac - min_frac) * (rng.below(10_000) as f32 / 10_000.0);
        let target = self.context_window_tokens as f32 * frac;

        let (text, completion_tokens) = if prompt_tokens < target - FILL_TARGET_SLACK_TOKENS {
            // Still below the watermark: emit filler markdown sized to the
            // remaining deficit (capped per turn).
            let deficit = (target - prompt_tokens).min(FILL_GROWTH_CAP_TOKENS);
            let filler = filler_text(rng.next_u64(), deficit);
            let tokens = self
                .paced_stream(&filler, ProviderEvent::TextDelta, &cancel, &events)
                .await?;
            (filler, tokens)
        } else {
            // Watermark reached: hold with a tiny response so memory stays at
            // the target without growing further.
            let hold = "上下文水位已达标，保持待命。\n";
            let tokens = self
                .paced_stream(hold, ProviderEvent::TextDelta, &cancel, &events)
                .await?;
            (hold.to_string(), tokens)
        };

        let usage = Usage {
            prompt_tokens: prompt_tokens as u64,
            completion_tokens,
            total_tokens: prompt_tokens as u64 + completion_tokens,
            cached_tokens: 0,
            last_prompt_tokens: prompt_tokens as u64,
        };
        Ok(Completion {
            text,
            reasoning: String::new(),
            tool_calls: Vec::new(),
            usage,
            finish_reason: Some(FinishReason::Stop),
        })
    }
}

/// Content of the first user message — the per-session scenario seed.
fn first_user_content(request: &ChatRequest) -> String {
    request
        .messages
        .iter()
        .find(|message| message.role == "user")
        .map(|message| message.content.clone())
        .unwrap_or_default()
}

/// Character-weight of a string in "tokens" (mirrors [`crate::context::estimate_tokens`]).
fn weight_of(text: &str) -> f32 {
    text.chars().map(char_weight).sum()
}

/// Generate a realistic-looking markdown block of approximately `target`
/// tokens: prose paragraphs with occasional code fences.
fn filler_text(seed: u64, target: f32) -> String {
    let mut rng = Rng::new(seed);
    let mut out = String::with_capacity((target * 4.3) as usize);
    let mut weight = 0.0f32;
    let mut section: u64 = rng.below(9_000);
    while weight < target {
        section += 1;
        let header = format!("\n\n## 分析片段 {section:04}\n\n");
        weight += weight_of(&header);
        out.push_str(&header);

        let paragraphs = 2 + rng.below(5);
        for _ in 0..paragraphs {
            let paragraph = filler_paragraph(&mut rng, section);
            weight += weight_of(&paragraph);
            out.push_str(&paragraph);
        }

        if rng.below(2) == 0 {
            let code = filler_code(&mut rng, section);
            weight += weight_of(&code);
            out.push_str(&code);
        }
    }
    out
}

const FILLER_TOPICS: &[&str] = &[
    "事件泵的批处理窗口",
    "transcript 序列化的摊销成本",
    "markdown 分块缓存的命中率",
    "取消信号的传播路径",
    "消息索引的代数校验",
    "内存工作区的页回收",
    "流式协议的乱序容错",
    "工具观察结果的截断边界",
];

const FILLER_VERBS: &[&str] = &[
    "先量化基线，再",
    "以滑动窗口对齐后",
    "在保证排序不变的前提下",
    "沿调用链向上追溯，",
    "对照 p50/p95 分布，",
    "按生成代数分桶之后，",
];

const FILLER_TAILS: &[&str] = &[
    "结论是收益集中在常量因子上。",
    "这一层可以推迟到回归窗口之后。",
    "风险点集中在 poison 恢复路径。",
    "对应的基准差异落在噪声区间内。",
    "进一步压缩需要重排元素树。",
    "报告中要标注测量口径。",
];

fn filler_paragraph(rng: &mut Rng, section: u64) -> String {
    let topic = FILLER_TOPICS[(rng.below(FILLER_TOPICS.len() as u64)) as usize];
    let verb = FILLER_VERBS[(rng.below(FILLER_VERBS.len() as u64)) as usize];
    let tail = FILLER_TAILS[(rng.below(FILLER_TAILS.len() as u64)) as usize];
    let sentences = 3 + rng.below(4);
    let mut out = String::with_capacity(640);
    out.push_str(&format!("围绕{topic}：{verb}展开第 {section} 轮验证。"));
    for index in 0..sentences {
        let inner_topic = FILLER_TOPICS[(rng.below(FILLER_TOPICS.len() as u64)) as usize];
        out.push_str(&format!(
            "步骤 {index} 记录{inner_topic}的观测值与上轮偏差；"
        ));
    }
    out.push_str(tail);
    out.push_str("\n\n");
    out
}

fn filler_code(rng: &mut Rng, section: u64) -> String {
    let samples = 4 + rng.below(5);
    let mut out = String::with_capacity(720);
    out.push_str(&format!("```text\n# profile[{section}]\n"));
    for index in 0..samples {
        let value = 400 + rng.below(1_000);
        out.push_str(&format!(
            "window_{index:02}: {value} tok/s  p95 0.{:02}ms\n",
            value % 100
        ));
    }
    out.push_str("```\n\n");
    out
}

#[async_trait]
impl ModelProvider for StressProvider {
    fn name(&self) -> &str {
        "stress"
    }

    fn context_window(&self) -> usize {
        self.context_window_tokens
    }

    async fn stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
        events: UnboundedSender<ProviderEvent>,
    ) -> Result<Completion, ProviderError> {
        // Context-fill mode: grow the transcript toward a per-session random
        // watermark, then hold. This is the extreme-memory scenario.
        if let Some((min_frac, max_frac)) = self.fill {
            return self
                .stream_fill(request, cancel, events, min_frac, max_frac)
                .await;
        }
        // Locate the current turn.
        let last_user = request
            .messages
            .iter()
            .rposition(|message| message.role == "user");
        let turn_messages: &[crate::provider::ChatMessage] = match last_user {
            Some(index) => &request.messages[index + 1..],
            None => &request.messages,
        };
        let step = turn_messages
            .iter()
            .filter(|message| message.role == "assistant" && !message.tool_calls.is_empty())
            .count();
        let prompt = last_user
            .map(|index| request.messages[index].content.as_str())
            .unwrap_or_default();

        // Per-turn RNG: same prompt → same plan; different steps diverge.
        let mut rng = Rng::new(
            hash_seed(prompt) ^ self.seed ^ (step as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15),
        );

        let mut completion = Completion::default();
        let mut completion_tokens: u64 = 0;

        // Reasoning precedes every step.
        let reasoning = rng.pick(REASONING);
        completion_tokens += self
            .paced_stream(reasoning, ProviderEvent::ReasoningDelta, &cancel, &events)
            .await?;
        completion.reasoning.push_str(reasoning);

        match self.step_kind(&mut rng, step) {
            Step::Tools => {
                let lead_in = "我先查一下相关内容，然后给你结论。\n\n";
                completion_tokens += self
                    .paced_stream(lead_in, ProviderEvent::TextDelta, &cancel, &events)
                    .await?;
                completion.text.push_str(lead_in);

                // Only templates whose tool is actually registered can fire.
                let available: Vec<&ToolCallTemplate> = TOOL_TEMPLATES
                    .iter()
                    .filter(|template| request.tools.iter().any(|spec| spec.name == template.name))
                    .collect();
                if available.is_empty() {
                    // Fall through to a final answer instead of stalling.
                    let text = rng.pick(FINAL_TEXTS);
                    completion_tokens += self
                        .paced_stream(text, ProviderEvent::TextDelta, &cancel, &events)
                        .await?;
                    completion.text.push_str(text);
                    completion.finish_reason = Some(FinishReason::Stop);
                } else {
                    let calls = 1 + rng.below(3) as usize;
                    for _ in 0..calls {
                        let template = rng.pick(&available);
                        let args = (template.build_args)(&mut rng);
                        completion
                            .tool_calls
                            .push(ToolCall::new(template.name, args));
                    }
                    completion.finish_reason = Some(FinishReason::ToolCalls);
                }
            }
            Step::Final => {
                let text = rng.pick(FINAL_TEXTS);
                completion_tokens += self
                    .paced_stream(text, ProviderEvent::TextDelta, &cancel, &events)
                    .await?;
                completion.text.push_str(text);
                completion.finish_reason = Some(FinishReason::Stop);
            }
        }

        let prompt_tokens = super::estimate_request_tokens(&request.messages) as u64;
        // Match the real adapters' convention: usage travels in the
        // authoritative `Completion` (the harness adds it exactly once in
        // `commit_completion`), not as a streamed `ProviderEvent::Usage`.
        completion.usage = Usage {
            prompt_tokens,
            completion_tokens,
            total_tokens: prompt_tokens + completion_tokens,
            cached_tokens: 0,
            last_prompt_tokens: prompt_tokens,
        };

        Ok(completion)
    }
}

type ToolCallTemplate = super::stress_templates::ToolTemplate;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ChatMessage;
    use tokio::sync::mpsc;

    fn request_with_prompt(prompt: &str, tools: &[&str]) -> ChatRequest {
        ChatRequest {
            model: "stress-1".into(),
            messages: vec![ChatMessage::user(prompt)],
            tools: tools
                .iter()
                .map(|name| crate::provider::ToolSpec {
                    name: (*name).to_string(),
                    description: String::new(),
                    parameters: serde_json::json!({}),
                })
                .collect(),
            temperature: None,
            max_tokens: None,
            reasoning_effort: None,
        }
    }

    async fn collect(
        provider: &StressProvider,
        request: ChatRequest,
    ) -> (Vec<ProviderEvent>, Result<Completion, ProviderError>) {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let result = provider.stream(request, CancellationToken::new(), tx).await;
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        (events, result)
    }

    #[test]
    fn template_count_reaches_dozens() {
        assert!(USER_PROMPTS.len() >= 10);
        assert!(REASONING.len() >= 10);
        assert!(FINAL_TEXTS.len() >= 12);
        assert!(TOOL_TEMPLATES.len() >= 12);
        assert!(super::super::stress_templates::TEMPLATE_COUNT >= 40);
    }

    #[tokio::test]
    async fn first_call_streams_reasoning_and_text() {
        let provider = StressProvider::new().with_rate(50_000.0); // fast for tests
        let (events, result) =
            collect(&provider, request_with_prompt("hello", &["list_dir"])).await;
        let completion = result.expect("stream ok");
        assert!(!events.is_empty());
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ProviderEvent::ReasoningDelta(_)))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ProviderEvent::TextDelta(_)))
        );
        // Round-trip: streamed deltas must reconstruct the authoritative text.
        let streamed: String = events
            .iter()
            .filter_map(|event| match event {
                ProviderEvent::TextDelta(delta) => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(streamed, completion.text);
    }

    #[tokio::test]
    async fn tool_step_returns_registered_tools_only() {
        let provider = StressProvider::new().with_rate(50_000.0);
        // Loop until the scenario reaches a Tools step (plans vary 1..=6 steps).
        let mut messages = vec![ChatMessage::user("inspect")];
        for _ in 0..12 {
            let (_, result) = collect(
                &provider,
                request_with_prompt_from(&messages, &["list_dir", "grep"]),
            )
            .await;
            let completion = result.expect("stream ok");
            if completion.tool_calls.is_empty() {
                break; // scenario finished
            }
            for call in &completion.tool_calls {
                assert!(["list_dir", "grep"].contains(&call.name.as_str()));
            }
            // Append assistant + tool observations so the next call advances a step.
            let calls = completion.tool_calls.clone();
            messages.push(ChatMessage::assistant(
                completion.text.clone(),
                calls.clone(),
            ));
            for call in &calls {
                messages.push(ChatMessage::tool(&call.id, "virtual observation"));
            }
        }
    }

    fn request_with_prompt_from(messages: &[ChatMessage], tools: &[&str]) -> ChatRequest {
        ChatRequest {
            model: "stress-1".into(),
            messages: messages.to_vec(),
            tools: tools
                .iter()
                .map(|name| crate::provider::ToolSpec {
                    name: (*name).to_string(),
                    description: String::new(),
                    parameters: serde_json::json!({}),
                })
                .collect(),
            temperature: None,
            max_tokens: None,
            reasoning_effort: None,
        }
    }

    #[tokio::test]
    async fn same_seed_same_prompt_same_output() {
        let provider = StressProvider::new().with_rate(50_000.0).with_seed(42);
        let request = request_with_prompt("determinism check", &[]);
        let (_, left) = collect(&provider, request.clone()).await;
        let (_, right) = collect(&provider, request).await;
        assert_eq!(left.expect("left").text, right.expect("right").text);
    }

    #[tokio::test]
    async fn cancellation_returns_promptly() {
        let provider = StressProvider::new(); // slow: 500 tok/s
        let (tx, _rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let started = std::time::Instant::now();
        let result = provider
            .stream(request_with_prompt("cancel me", &[]), cancel, tx)
            .await;
        assert!(matches!(result, Err(ProviderError::Cancelled)));
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[tokio::test]
    async fn paces_close_to_target_rate() {
        // ~600 tokens at 1000 tok/s → expect ~0.6s. Generous bounds keep the
        // test stable on loaded machines.
        let provider = StressProvider::new()
            .with_rate(1_000.0)
            .with_chunk_tokens(8);
        let text = "字".repeat(600); // 1 token per char
        let started = std::time::Instant::now();
        let (tx, _rx) = mpsc::unbounded_channel();
        let tokens = provider
            .paced_stream(
                &text,
                ProviderEvent::TextDelta,
                &CancellationToken::new(),
                &tx,
            )
            .await
            .expect("streamed");
        let elapsed = started.elapsed();
        assert_eq!(tokens, 600);
        assert!(
            elapsed >= Duration::from_millis(400) && elapsed <= Duration::from_millis(1_500),
            "elapsed {elapsed:?} should be ~0.6s at 1000 tok/s"
        );
    }

    #[test]
    fn filler_hits_target_weight() {
        for target in [2_000.0f32, 20_000.0, 78_000.0] {
            let text = filler_text(42, target);
            let weight = weight_of(&text);
            assert!(
                weight >= target * 0.9 && weight <= target * 1.35,
                "filler weight {weight} vs target {target}"
            );
        }
    }

    #[tokio::test]
    async fn fill_mode_grows_then_holds() {
        let provider = StressProvider::new()
            .with_rate(200_000.0)
            .with_context_window(10_000)
            .with_context_fill(0.5, 0.5);

        // Turn 1: transcript ≈ user prompt only → filler ≈ 50% of window.
        let (events1, result1) = collect(&provider, request_with_prompt("fill", &[])).await;
        let completion1 = result1.expect("turn 1");
        assert!(
            completion1.tool_calls.is_empty(),
            "fill turns carry no tools"
        );
        let streamed: String = events1
            .iter()
            .filter_map(|event| match event {
                ProviderEvent::TextDelta(delta) => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(streamed, completion1.text);
        let grown = weight_of(&completion1.text);
        assert!(
            grown >= 4_000.0 && grown <= 6_000.0,
            "turn-1 filler should approach 50% of the 10k window, got {grown}"
        );

        // Turn 2: transcript already carries the filler → hold response.
        let mut messages = vec![
            ChatMessage::user("fill"),
            ChatMessage::assistant(completion1.text, Vec::new()),
        ];
        let (_, result2) = collect(
            &provider,
            ChatRequest {
                model: "stress-1".into(),
                messages,
                tools: Vec::new(),
                temperature: None,
                max_tokens: None,
                reasoning_effort: None,
            },
        )
        .await;
        let completion2 = result2.expect("turn 2");
        assert!(
            weight_of(&completion2.text) < 50.0,
            "hold response must stay tiny, got {}",
            weight_of(&completion2.text)
        );
        // The hold turn reports the (large) transcript as prompt tokens.
        assert!(completion2.usage.prompt_tokens >= 4_000);
    }

    #[tokio::test]
    async fn usage_reports_prompt_and_completion() {
        let provider = StressProvider::new().with_rate(50_000.0);
        let (events, result) = collect(&provider, request_with_prompt("usage", &[])).await;
        let completion = result.expect("stream ok");
        // Convention (matching the real adapters): usage lives only in the
        // completion, never as a streamed event.
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, ProviderEvent::Usage(_)))
        );
        let usage = &completion.usage;
        assert!(usage.prompt_tokens > 0);
        assert!(usage.completion_tokens > 0);
        assert_eq!(
            usage.total_tokens,
            usage.prompt_tokens + usage.completion_tokens
        );
    }
}
