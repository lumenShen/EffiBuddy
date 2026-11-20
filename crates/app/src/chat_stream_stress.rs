//! 真实会话流式 + 鼠标滚动压力采样。
//!
//! 用**一份真实会话文件**（`sessions/*.json`）作为历史，用它的真实推理 / 正文 /
//! 工具调用 / 工具结果作为 100 tok/s 的流式输出源，然后在真实的 `ChatList` 上
//! 逐帧驱动 5 分钟：
//!
//! ```text
//! 30s 随机上下滚轮 + 随机展开折叠
//! 30s 时刻滚动到底部（滚动条按钮的 glide 路径）并自动吸附
//! 20s 只让真实输出继续，检查吸附是否贴底
//! 再 30s 随机上下滚轮 …
//! ```
//!
//! 每帧记录偏移 / 可滚动高度 / 内容高度 / 行锚点 / 已测量行数 / 拼接触发器，
//! 结束时把「单帧内容高度或可见锚点突然跳变」的时刻挑出来并归因。
//!
//! 运行（真实时间 5 分钟，默认 60fps）：
//!
//! ```text
//! EFFIBUDDY_STREAM_STRESS=1 cargo test --release -p effibuddy-app --bin effibuddy \
//!     chat_stream_stress -- --nocapture
//! ```
//!
//! 可选：
//! * `EFFIBUDDY_REAL_SESSION=/path/to/session.json` 指定会话；
//! * `EFFIBUDDY_STREAM_STRESS_SECS` 采样时长（默认 300）；
//! * `EFFIBUDDY_STREAM_STRESS_FPS` 帧率上限（默认 60）；
//! * `EFFIBUDDY_STREAM_STRESS_SEED` 随机种子（默认 20261119）。

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use effibuddy_agent_core::{ContentBlock, Message, SessionState, ToolCall, ToolResult};
use effibuddy_ui::Theme;
use gpui::{
    AppContext as _, IntoElement, Render, Styled, WeakEntity, Window, div, list, point, px, size,
};
use serde_json::Value;

use crate::app::AppView;
use crate::chat::{AgentCard, ChatList, ChatViewState, RunSnapshot, render_chat_item};

const VIEWPORT_W: f32 = 1280.0;
const VIEWPORT_H: f32 = 840.0;
/// 用户要求的输出速度。
const TOKENS_PER_SEC: f64 = 100.0;
/// 随机滚动相位长度。
const WHEEL_SECS: f64 = 30.0;
/// 吸附后静候吸附贴底的长度。
const HOLD_SECS: f64 = 20.0;
const CYCLE_SECS: f64 = WHEEL_SECS + HOLD_SECS;

// --- 真实会话加载 ----------------------------------------------------------

fn data_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(dir) = std::env::var("EFFIBUDDY_DATA_DIR") {
        if !dir.trim().is_empty() {
            dirs.push(PathBuf::from(dir).join("sessions"));
        }
    }
    if let Some(base) = dirs::data_local_dir() {
        dirs.push(base.join("EffiBuddy").join("sessions"));
        dirs.push(base.join("EffiBuddy-Dev").join("sessions"));
    }
    dirs
}

/// 挑一份「像真人在用」的会话：足够长、带思考、带工具调用与结果、不是子代理
/// 转录、不是草稿。按修改时间从新到旧找，最多解析 12 个候选。
fn load_real_session() -> Option<(PathBuf, SessionState)> {
    if let Ok(path) = std::env::var("EFFIBUDDY_REAL_SESSION") {
        let path = PathBuf::from(path);
        let text = std::fs::read_to_string(&path).ok()?;
        let session: SessionState = serde_json::from_str(&text).ok()?;
        return Some((path, session));
    }
    let mut files: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    for dir in data_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let size = meta.len();
            // 太小的没有可滚动历史，太大的解析和每帧重建都太慢。
            if !(250_000..=1_600_000).contains(&size) {
                continue;
            }
            files.push((path, meta.modified().unwrap_or(std::time::UNIX_EPOCH)));
        }
    }
    files.sort_by(|left, right| right.1.cmp(&left.1));
    let mut parsed = 0usize;
    for (path, _) in files {
        if parsed >= 12 {
            break;
        }
        parsed += 1;
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(session): Result<SessionState, _> = serde_json::from_str(&text) else {
            continue;
        };
        if session.parent_id.is_some() || session.archived {
            continue;
        }
        let mut assistant = 0usize;
        let mut tools = 0usize;
        let mut reasoning_bytes = 0usize;
        let mut text_bytes = 0usize;
        for message in &session.messages {
            match message.role {
                effibuddy_agent_core::Role::Assistant => assistant += 1,
                effibuddy_agent_core::Role::Tool => tools += 1,
                _ => {}
            }
            for block in &message.blocks {
                match block {
                    ContentBlock::Reasoning { text } => reasoning_bytes += text.len(),
                    ContentBlock::Text { text } => text_bytes += text.len(),
                    _ => {}
                }
            }
            if assistant >= 80 && tools >= 50 && reasoning_bytes >= 40_000 && text_bytes >= 1_000 {
                return Some((path, session));
            }
        }
        // 一段足够长的真实历史就算合格；工具/思考分布不必强求。
        if session.messages.len() >= 250 && assistant >= 80 && reasoning_bytes >= 40_000 {
            return Some((path, session));
        }
    }
    None
}

// --- 把真实会话变成流式脚本 ------------------------------------------------

#[derive(Clone)]
struct PreparedResult {
    content: String,
    is_error: bool,
    duration_ms: Option<u64>,
}

#[derive(Clone)]
struct PreparedCall {
    name: String,
    arguments: Value,
    result: Option<PreparedResult>,
}

struct AssistantScript {
    reasoning: String,
    text: String,
    calls: Vec<PreparedCall>,
}

/// 把真实会话按消息顺序拆成「助手脚本」：思考 → 正文 → 工具调用（+ 真实结果）。
fn build_scripts(session: &SessionState) -> Vec<AssistantScript> {
    let mut results: HashMap<String, PreparedResult> = HashMap::new();
    for message in &session.messages {
        for block in &message.blocks {
            if let ContentBlock::ToolResult { result } = block {
                results.insert(
                    result.call_id.clone(),
                    PreparedResult {
                        content: result.content.clone(),
                        is_error: result.is_error,
                        duration_ms: result.duration_ms,
                    },
                );
            }
        }
    }
    let mut scripts = Vec::new();
    for message in &session.messages {
        if message.role != effibuddy_agent_core::Role::Assistant {
            continue;
        }
        let calls = message
            .blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolCall { call } => Some(PreparedCall {
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                    result: results.get(&call.id).cloned(),
                }),
                _ => None,
            })
            .collect::<Vec<_>>();
        let reasoning = message.reasoning();
        let text = message.text();
        if reasoning.is_empty() && text.is_empty() && calls.is_empty() {
            continue;
        }
        scripts.push(AssistantScript {
            reasoning,
            text,
            calls,
        });
    }
    scripts
}

/// 近似 token 切分：CJK 一个字算一个 token，其余字符按 4 字节 ≈ 1 token。
/// 只用来控制「每秒多少个 token 进入缓冲区」，不需要和模型分词器一致。
fn token_spans(text: &str) -> Vec<(u32, u32)> {
    fn weight(ch: char) -> f32 {
        let code = ch as u32;
        if code >= 0x2E80 {
            1.0
        } else if ch.is_ascii_whitespace() {
            0.25
        } else if ch.is_ascii_alphanumeric() || ch.is_ascii_punctuation() {
            0.25
        } else {
            0.5
        }
    }
    let mut spans = Vec::new();
    let mut start = 0usize;
    let mut acc = 0.0f32;
    for (index, ch) in text.char_indices() {
        acc += weight(ch);
        if acc >= 0.999 {
            let end = index + ch.len_utf8();
            spans.push((start as u32, end as u32));
            start = end;
            acc = 0.0;
        }
    }
    if start < text.len() {
        spans.push((start as u32, text.len() as u32));
    }
    spans
}

#[derive(Clone, Copy)]
enum Atom {
    Start(usize),
    Token {
        script: usize,
        reasoning: bool,
        start: u32,
        end: u32,
    },
    Finish(usize),
    Tools(usize),
    /// 工具执行 / 下一个 step 之间的空档：期间不产生 token。
    Pause(f64),
}

struct StreamEngine {
    atoms: Vec<Atom>,
    cursor: usize,
    budget: f64,
    pause_until: f64,
    live: Option<LiveAssistant>,
}

struct LiveAssistant {
    script: usize,
    message_ix: usize,
    message_id: String,
    calls: Vec<ToolCall>,
}

impl StreamEngine {
    fn new(scripts: &[AssistantScript]) -> Self {
        let mut atoms = Vec::new();
        for (index, script) in scripts.iter().enumerate() {
            atoms.push(Atom::Start(index));
            for (start, end) in token_spans(&script.reasoning) {
                atoms.push(Atom::Token {
                    script: index,
                    reasoning: true,
                    start,
                    end,
                });
            }
            for (start, end) in token_spans(&script.text) {
                atoms.push(Atom::Token {
                    script: index,
                    reasoning: false,
                    start,
                    end,
                });
            }
            atoms.push(Atom::Finish(index));
            if !script.calls.is_empty() {
                atoms.push(Atom::Tools(index));
            }
            atoms.push(Atom::Pause(0.12));
        }
        Self {
            atoms,
            cursor: 0,
            budget: 0.0,
            pause_until: 0.0,
            live: None,
        }
    }

    /// 推进 `dt` 秒的真实输出。返回本帧进入缓冲区的 token 数。
    fn advance(
        &mut self,
        chat: &mut ChatList,
        session: &mut SessionState,
        scripts: &[AssistantScript],
        wall: f64,
        dt: f64,
        dirty: &mut bool,
    ) -> (u32, bool, bool) {
        if wall < self.pause_until {
            return (0, false, false);
        }
        self.budget = (self.budget + TOKENS_PER_SEC * dt).min(400.0);
        let mut tokens = 0u32;
        let mut started = false;
        let mut finished = false;
        while self.cursor < self.atoms.len() {
            let atom = self.atoms[self.cursor];
            match atom {
                Atom::Token {
                    script,
                    reasoning,
                    start,
                    end,
                } => {
                    if self.budget < 1.0 {
                        break;
                    }
                    self.budget -= 1.0;
                    let Some(live) = self.live.as_ref() else {
                        self.cursor += 1;
                        continue;
                    };
                    if live.script != script {
                        self.cursor += 1;
                        continue;
                    }
                    let source = &scripts[script];
                    let body = if reasoning {
                        &source.reasoning
                    } else {
                        &source.text
                    };
                    let (start, end) = (start as usize, end as usize);
                    if end > body.len() {
                        self.cursor += 1;
                        continue;
                    }
                    let delta = &body[start..end];
                    let message = &mut session.messages[live.message_ix];
                    if reasoning {
                        message.push_reasoning(delta);
                        chat.push_reasoning(&live.message_id, delta);
                    } else {
                        message.push_text(delta);
                        chat.push_text(&live.message_id, delta);
                    }
                    tokens += 1;
                    self.cursor += 1;
                }
                Atom::Start(script) => {
                    let message = Message::assistant();
                    let message_id = message.id.clone();
                    session.push_message(message);
                    let message_ix = session.messages.len() - 1;
                    chat.begin_message(&message_id);
                    self.live = Some(LiveAssistant {
                        script,
                        message_ix,
                        message_id,
                        calls: Vec::new(),
                    });
                    *dirty = true;
                    started = true;
                    self.cursor += 1;
                }
                Atom::Finish(script) => {
                    let Some(live) = self.live.as_mut() else {
                        self.cursor += 1;
                        continue;
                    };
                    if live.script != script {
                        self.cursor += 1;
                        continue;
                    }
                    let source = &scripts[script];
                    let mut calls = Vec::with_capacity(source.calls.len());
                    for prepared in &source.calls {
                        let call = ToolCall::new(prepared.name.clone(), prepared.arguments.clone());
                        calls.push(call);
                    }
                    {
                        let message = &mut session.messages[live.message_ix];
                        for call in &calls {
                            message.push_tool_call(call.clone());
                        }
                    }
                    live.calls = calls;
                    let message = session.messages[live.message_ix].clone();
                    chat.finish_message(&message);
                    *dirty = true;
                    finished = true;
                    self.cursor += 1;
                }
                Atom::Tools(script) => {
                    let script_calls = scripts[script].calls.clone();
                    let Some(live) = self.live.as_ref() else {
                        self.cursor += 1;
                        continue;
                    };
                    for (prepared, call) in script_calls.iter().zip(&live.calls) {
                        let Some(result) = &prepared.result else {
                            continue;
                        };
                        let mut tool_result = if result.is_error {
                            ToolResult::failure(call, result.content.clone())
                        } else {
                            ToolResult::success(call, result.content.clone())
                        };
                        tool_result.duration_ms = result.duration_ms;
                        session.push_message(Message::from_tool_result(tool_result));
                    }
                    self.live = None;
                    *dirty = true;
                    self.cursor += 1;
                }
                Atom::Pause(secs) => {
                    self.pause_until = wall + secs;
                    // 工具执行期间不积累输出预算；否则空档结束后会一次性爆发。
                    self.budget = 0.0;
                    self.cursor += 1;
                    break;
                }
            }
        }
        (tokens, started, finished)
    }
}

// --- 确定性随机 ------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        ((x >> 32) as u32) ^ (x as u32)
    }

    fn range(&mut self, low: u32, high: u32) -> u32 {
        low + self.next_u32() % (high - low + 1)
    }

    fn chance(&mut self, percent: u32) -> bool {
        self.next_u32() % 100 < percent
    }

    fn f64(&mut self, low: f64, high: f64) -> f64 {
        low + (self.next_u32() as f64 / u32::MAX as f64) * (high - low)
    }
}

// --- 展开 / 折叠目标 -------------------------------------------------------

#[derive(Clone)]
struct Expando {
    kind: u8, // 0 reasoning, 1 tool, 2 turn
    id: String,
}

fn collect_expandos(session: &SessionState) -> Vec<Expando> {
    let mut out = Vec::new();
    // turn：会话里连续的助手消息（中间只隔着工具消息）合并成一个 turn，
    // id 与 `flush_assistant_group` 一致：`turn:{第一个助手消息 id}`。
    let mut run_first: Option<String> = None;
    for message in &session.messages {
        match message.role {
            effibuddy_agent_core::Role::Assistant => {
                if !message.reasoning().is_empty() {
                    out.push(Expando {
                        kind: 0,
                        id: message.id.clone(),
                    });
                }
                let mut cursor = 0usize;
                let calls = message.tool_calls();
                while cursor < calls.len() {
                    if calls[cursor].name == "agent" {
                        cursor += 1;
                        continue;
                    }
                    let start = cursor;
                    while cursor < calls.len() && calls[cursor].name != "agent" {
                        cursor += 1;
                    }
                    if cursor - start >= 3 {
                        out.push(Expando {
                            kind: 1,
                            id: format!("group:{}:{}", message.id, start),
                        });
                    } else {
                        for call in &calls[start..cursor] {
                            out.push(Expando {
                                kind: 1,
                                id: call.id.clone(),
                            });
                        }
                    }
                }
                run_first.get_or_insert_with(|| message.id.clone());
            }
            effibuddy_agent_core::Role::Tool => {}
            _ => {
                if let Some(first) = run_first.take() {
                    out.push(Expando {
                        kind: 2,
                        id: format!("turn:{first}"),
                    });
                }
            }
        }
    }
    if let Some(first) = run_first.take() {
        out.push(Expando {
            kind: 2,
            id: format!("turn:{first}"),
        });
    }
    out
}

// --- 滚轮手势 --------------------------------------------------------------

struct WheelEvent {
    at: f64,
    pixel_y: f32,
}

struct WheelGenerator {
    pending: VecDeque<WheelEvent>,
    next_gesture_at: f64,
    rng: Rng,
}

impl WheelGenerator {
    fn new(seed: u64) -> Self {
        Self {
            pending: VecDeque::new(),
            next_gesture_at: 0.15,
            rng: Rng(seed | 1),
        }
    }

    fn schedule(&mut self, now: f64, offset: f32, max: f32) {
        if !self.pending.is_empty() || now < self.next_gesture_at {
            return;
        }
        let ratio = if max > 1.0 {
            (offset / max).clamp(0.0, 1.0)
        } else {
            0.5
        };
        // 贴边时朝有空间的一侧滚，避免整段时间都顶在端点。
        let toward_start = if ratio < 0.15 {
            false
        } else if ratio > 0.85 {
            true
        } else {
            self.rng.chance(50)
        };
        let notches = self.rng.range(1, 12);
        let (gap_low, gap_high) = if self.rng.chance(60) {
            (2, 18)
        } else {
            (30, 90)
        };
        let mut at = now * 1000.0;
        for _ in 0..notches {
            let magnitude = 20.0 * if self.rng.chance(20) { 2.0 } else { 1.0 };
            self.pending.push_back(WheelEvent {
                at,
                pixel_y: if toward_start { magnitude } else { -magnitude },
            });
            at += self.rng.range(gap_low, gap_high) as f64;
        }
        self.next_gesture_at = (at / 1000.0) + self.rng.f64(0.10, 1.20);
    }

    /// 取出 `now` 之前应当交付的滚轮事件（每帧最多 48 个，防止病态追赶）。
    fn drain(&mut self, now: f64) -> Vec<WheelEvent> {
        let now_ms = now * 1000.0;
        let mut out = Vec::new();
        while let Some(event) = self.pending.front() {
            if event.at > now_ms || out.len() >= 48 {
                break;
            }
            let event = self.pending.pop_front().expect("front checked");
            out.push(event);
        }
        out
    }
}

// --- 渲染夹具 --------------------------------------------------------------

struct Harness {
    state: gpui::ListState,
    view: Rc<ChatViewState>,
}

impl Render for Harness {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        let view = Rc::clone(&self.view);
        let entity = WeakEntity::<AppView>::new_invalid();
        let theme = Theme::light();
        let count = view.slots.len();
        list(self.state.clone(), move |index: usize, _, _| {
            if index >= count {
                return div().into_any_element();
            }
            render_chat_item(index, &view, &theme, &entity)
        })
        .w_full()
        .h_full()
    }
}

// --- 采样 ------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Wheel,
    Hold,
}

impl Phase {
    fn label(self) -> &'static str {
        match self {
            Phase::Wheel => "wheel",
            Phase::Hold => "hold",
        }
    }
}

#[derive(Clone)]
struct Sample {
    t: f64,
    cycle: u32,
    phase: Phase,
    offset: f32,
    max: f32,
    viewport: f32,
    content: f32,
    top_ix: usize,
    top_off: f32,
    items: usize,
    slots: usize,
    measured: usize,
    seeded: f32,
    pinned: bool,
    glide: bool,
    splice: Option<(usize, usize, usize)>,
    calibrated: bool,
    wheel_px: f32,
    wheel_events: u32,
    tokens: u32,
    expand: i32,
}

struct Sim {
    chat: ChatList,
    session: SessionState,
    run: RunSnapshot,
    expanded_reasoning: HashSet<String>,
    expanded_tools: HashSet<String>,
    expanded_turns: HashSet<String>,
    collapsed_turns: HashSet<String>,
    agents: HashMap<String, AgentCard>,
    chat_dirty: bool,
    samples: Vec<Sample>,
    engine: StreamEngine,
    scripts: Vec<AssistantScript>,
    expandos: Vec<Expando>,
    /// 运行中的最后一个 turn 会自动展开；`is_expanded` 必须和渲染规则一致，
    /// 否则“展开”一个本来就自动展开的 live turn 会重播高度渐显（先缩后长）。
    live_turn: Option<String>,
    wheel: WheelGenerator,
    rng: Rng,
    next_expand_at: f64,
    snapped_cycle: u32,
}

impl Sim {
    fn new(session: SessionState, seed: u64) -> Self {
        let scripts = build_scripts(&session);
        let expandos = collect_expandos(&session);
        let live_turn = expandos
            .iter()
            .rev()
            .find(|expando| expando.kind == 2)
            .map(|expando| expando.id.clone());
        let engine = StreamEngine::new(&scripts);
        Self {
            chat: ChatList::new(),
            session,
            run: RunSnapshot {
                running: true,
                ..RunSnapshot::default()
            },
            expanded_reasoning: HashSet::new(),
            expanded_tools: HashSet::new(),
            expanded_turns: HashSet::new(),
            collapsed_turns: HashSet::new(),
            agents: HashMap::new(),
            chat_dirty: true,
            samples: Vec::new(),
            engine,
            scripts,
            expandos,
            live_turn,
            wheel: WheelGenerator::new(seed),
            rng: Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1),
            next_expand_at: 1.0,
            snapped_cycle: u32::MAX,
        }
    }

    fn expand_count(&self) -> usize {
        self.expanded_reasoning.len() + self.expanded_tools.len() + self.expanded_turns.len()
    }

    fn is_expanded(&self, expando: &Expando) -> bool {
        match expando.kind {
            0 => self.expanded_reasoning.contains(&expando.id),
            1 => self.expanded_tools.contains(&expando.id),
            _ => {
                !self.collapsed_turns.contains(&expando.id)
                    && (self.expanded_turns.contains(&expando.id)
                        || self.live_turn.as_deref() == Some(expando.id.as_str()))
            }
        }
    }

    fn toggle_expansion(&mut self, now: f64) -> i32 {
        if self.expandos.is_empty() {
            return 0;
        }
        self.next_expand_at = now + self.rng.f64(0.8, 2.5);
        // 一半展开、一半折叠；折叠从「当前确实展开」的集合里挑，否则随机挑到
        // 一个本来就没展开的项会变成空操作，让这段压力测试名不副实。
        let collapse = self.rng.chance(45) || self.expand_count() >= 6;
        let mut candidates: Vec<&Expando> = self
            .expandos
            .iter()
            .filter(|expando| self.is_expanded(expando) == collapse)
            .collect();
        if candidates.is_empty() {
            candidates = self.expandos.iter().collect();
        }
        let index = self.rng.next_u32() as usize % candidates.len();
        let expando = candidates[index].clone();
        let expanded = self.is_expanded(&expando);
        if expanded {
            match expando.kind {
                0 => {
                    self.expanded_reasoning.remove(&expando.id);
                }
                1 => {
                    self.expanded_tools.remove(&expando.id);
                }
                _ => {
                    self.expanded_turns.remove(&expando.id);
                    self.collapsed_turns.insert(expando.id.clone());
                }
            };
            -1
        } else {
            match expando.kind {
                0 => {
                    self.expanded_reasoning.insert(expando.id.clone());
                }
                1 => {
                    self.expanded_tools.insert(expando.id.clone());
                }
                _ => {
                    self.collapsed_turns.remove(&expando.id);
                    self.chat.note_turn_expanded(&expando.id);
                    self.expanded_turns.insert(expando.id.clone());
                }
            };
            1
        }
    }

    fn frame(&mut self, cx: &mut gpui::VisualTestContext, wall: f64, dt: f64) {
        let cycle = (wall / CYCLE_SECS).floor() as u32;
        let within = wall - cycle as f64 * CYCLE_SECS;
        let phase = if within < WHEEL_SECS {
            Phase::Wheel
        } else {
            Phase::Hold
        };

        let max = f32::from(self.chat.list_state.max_offset_for_scrollbar().height);
        let offset = -f32::from(self.chat.list_state.scroll_px_offset_for_scrollbar().y);

        let mut wheel_px = 0.0f32;
        let mut wheel_events = 0u32;
        let mut expand = 0i32;
        if phase == Phase::Wheel {
            self.wheel.schedule(wall, offset, max);
            for event in self.wheel.drain(wall) {
                self.chat.wheel(event.pixel_y);
                wheel_px += event.pixel_y;
                wheel_events += 1;
            }
            if wall >= self.next_expand_at {
                expand = self.toggle_expansion(wall);
            }
        } else {
            self.wheel.pending.clear();
        }
        // 30s 的滚动结束：用和滚动条按钮一致的 glide 吸附到底部。
        if phase == Phase::Hold && self.snapped_cycle != cycle {
            self.snapped_cycle = cycle;
            self.chat.glide_to_bottom();
        }

        let (tokens, _started, _finished) = self.engine.advance(
            &mut self.chat,
            &mut self.session,
            &self.scripts,
            wall,
            dt,
            &mut self.chat_dirty,
        );
        if self.chat_dirty {
            let session = &self.session;
            let run = &self.run;
            self.chat.rebuild(session, run);
            self.chat_dirty = false;
        }
        self.chat.tick_scroll_at(Instant::now());
        self.chat.tick();
        self.chat.sync_scroll();

        let entity = WeakEntity::<AppView>::new_invalid();
        let view = Rc::new(self.chat.prepare_frame(
            self.run.running,
            true,
            &self.expanded_reasoning,
            &self.expanded_tools,
            &self.expanded_turns,
            &self.collapsed_turns,
            &self.agents,
            &entity,
        ));
        let trace = self.chat.frame_scroll_trace();
        let state = self.chat.list_state.clone();
        cx.draw(
            point(px(0.), px(0.)),
            size(px(VIEWPORT_W), px(VIEWPORT_H)),
            move |_, cx| cx.new(|_| Harness { state, view }),
        );

        let list = &self.chat.list_state;
        let offset = -f32::from(list.scroll_px_offset_for_scrollbar().y);
        let max = f32::from(list.max_offset_for_scrollbar().height);
        let viewport = f32::from(list.viewport_bounds().size.height);
        let top = list.logical_scroll_top();
        let rows = list.debug_rows();
        let measured = rows.iter().filter(|(_, _, done)| *done).count();
        let (items, slots) = self.chat.trace_counts();
        self.samples.push(Sample {
            t: wall,
            cycle,
            phase,
            offset,
            max,
            viewport,
            content: max + viewport,
            top_ix: top.item_ix,
            top_off: f32::from(top.offset_in_item),
            items,
            slots,
            measured,
            seeded: trace.seeded_row_height,
            pinned: self.chat.pinned(),
            glide: self.chat.glide_is_active(),
            splice: trace.splice,
            calibrated: trace.calibrated,
            wheel_px,
            wheel_events,
            tokens,
            expand,
        });
    }
}

fn elapsed_frame(target: Instant) {
    let now = Instant::now();
    if target > now {
        std::thread::sleep(target.duration_since(now));
    }
}

// --- 报告 ------------------------------------------------------------------

fn label_event(sample: &Sample) -> String {
    let mut parts = Vec::new();
    if sample.wheel_events > 0 {
        parts.push(format!(
            "wheel x{} ({:+.0}px)",
            sample.wheel_events, sample.wheel_px
        ));
    }
    if sample.expand > 0 {
        parts.push("expand".to_string());
    } else if sample.expand < 0 {
        parts.push("collapse".to_string());
    }
    if sample.calibrated {
        parts.push("calibrate".to_string());
    }
    if let Some((prefix, old, new)) = sample.splice {
        parts.push(format!("splice[{prefix}..{old}] -> {new}"));
    }
    if sample.tokens > 0 {
        parts.push(format!("{}tok", sample.tokens));
    }
    if parts.is_empty() {
        parts.push("-".to_string());
    }
    parts.join(" ")
}

fn print_report(samples: &[Sample], title: &str, session_messages: usize) {
    println!(
        "\n=== 真实会话流式滚动采样：{title}（历史 {session_messages} 条消息，{} 帧） ===",
        samples.len()
    );
    println!(
        "  采样 {:.1}s / {:?} 相位轮次 / 平均 {:.0} 帧每秒",
        samples.last().map(|s| s.t).unwrap_or(0.0),
        samples.iter().map(|s| s.cycle).max().unwrap_or(0) + 1,
        samples.len() as f64 / samples.last().map(|s| s.t).unwrap_or(1.0).max(0.001)
    );

    for phase in [Phase::Wheel, Phase::Hold] {
        let rows: Vec<&Sample> = samples.iter().filter(|s| s.phase == phase).collect();
        if rows.is_empty() {
            continue;
        }
        let mut content_jump = 0.0f32;
        let mut content_jump_at = 0.0;
        let mut offset_jump = 0.0f32;
        let mut offset_jump_at = 0.0;
        let mut top_jump = 0usize;
        let mut top_jump_at = 0.0;
        let mut inner_jump = 0.0f32;
        let mut inner_jump_at = 0.0;
        for pair in rows.windows(2) {
            let content = (pair[1].content - pair[0].content).abs();
            if content > content_jump {
                content_jump = content;
                content_jump_at = pair[1].t;
            }
            let offset = (pair[1].offset - pair[0].offset).abs();
            if offset > offset_jump {
                offset_jump = offset;
                offset_jump_at = pair[1].t;
            }
            if pair[0].top_ix != pair[1].top_ix && pair[1].wheel_events == 0 && pair[1].expand == 0
            {
                let rows_moved = pair[0].top_ix.abs_diff(pair[1].top_ix);
                if rows_moved > top_jump {
                    top_jump = rows_moved;
                    top_jump_at = pair[1].t;
                }
            }
            if pair[1].wheel_events == 0 && pair[1].expand == 0 {
                let inner = (pair[1].top_off - pair[0].top_off).abs();
                if inner > inner_jump {
                    inner_jump = inner;
                    inner_jump_at = pair[1].t;
                }
            }
        }
        let unpinned = rows.iter().filter(|s| !s.pinned).count();
        let max_gap = rows
            .iter()
            .filter(|s| s.pinned)
            .map(|s| (s.content - s.viewport - s.offset).max(0.0))
            .fold(0.0f32, f32::max);
        let calibrated = rows.iter().filter(|s| s.calibrated).count();
        let spliced = rows.iter().filter(|s| s.splice.is_some()).count();
        println!(
            "  [{}] 帧 {} | 内容高度单帧最大跳变 {:.0}px @ {:.1}s | 偏移单帧最大跳变 {:.0}px @ {:.1}s\n        无输入时行内最大跳变 {inner_jump:.0}px @ {inner_jump_at:.1}s | 锚点最大跳行 {top_jump} @ {top_jump_at:.1}s | 未贴底帧 {unpinned} | 贴底时最大空隙 {max_gap:.0}px | 行高校准 {calibrated} 帧 | 拼接 {spliced} 帧",
            phase.label(),
            rows.len(),
            content_jump,
            content_jump_at,
            offset_jump,
            offset_jump_at,
        );
    }

    // 吸附后的收敛：每个 hold 相位开头若干帧的空隙。
    let cycles = samples.iter().map(|s| s.cycle).max().unwrap_or(0) + 1;
    for cycle in 0..cycles {
        let hold: Vec<&Sample> = samples
            .iter()
            .filter(|s| s.cycle == cycle && s.phase == Phase::Hold)
            .collect();
        if hold.is_empty() {
            continue;
        }
        let start = hold[0].t;
        let mut settle = None;
        for sample in &hold {
            let gap = (sample.content - sample.viewport - sample.offset).max(0.0);
            if gap <= 2.0 {
                settle = Some(sample.t - start);
                break;
            }
        }
        let max_gap = hold
            .iter()
            .map(|s| (s.content - s.viewport - s.offset).max(0.0))
            .fold(0.0f32, f32::max);
        let unpinned = hold.iter().filter(|s| !s.pinned).count();
        let late = hold
            .iter()
            .filter(|s| s.t - start > 3.0)
            .map(|s| (s.content - s.viewport - s.offset).max(0.0))
            .fold(0.0f32, f32::max);
        println!(
            "  吸附 cycle {cycle}: 贴合耗时 {:?} | 20s 内最大空隙 {max_gap:.1}px（3s 后 {late:.1}px）| 失去吸附 {unpinned} 帧",
            settle
                .map(|s| format!("{s:.2}s"))
                .unwrap_or_else(|| "未贴合".into()),
        );
        // 每秒一行，直接看吸附后的跟随是否真的收敛。
        let mut next_mark = start;
        for sample in &hold {
            if sample.t + 0.001 < next_mark {
                continue;
            }
            next_mark += 1.0;
            let gap = (sample.content - sample.viewport - sample.offset).max(0.0);
            println!(
                "    hold {:6.2}s gap={:7.1}px offset={:8.1} content={:8.1} pin={} glide={} 已测 {} 事件 {}",
                sample.t - start,
                gap,
                sample.offset,
                sample.content,
                sample.pinned as u8,
                sample.glide as u8,
                sample.measured,
                label_event(sample),
            );
        }
    }

    // 内容高度异常跳变：按幅度排序。
    let mut jumps: Vec<(f32, usize)> = samples
        .windows(2)
        .enumerate()
        .map(|(index, pair)| ((pair[1].content - pair[0].content), index + 1))
        .collect();
    jumps.sort_by(|left, right| right.0.abs().total_cmp(&left.0.abs()));
    println!("\n  -- 单帧内容高度跳变 Top 12（正=变高 / 负=变矮） --");
    for (delta, index) in jumps.iter().take(12) {
        let sample = &samples[*index];
        println!(
            "  {:+.0}px @ {:7.2}s cycle {} {} | content {:.0} ∅{:.0}px 行 {}/{} 已测 {} | 事件 {}",
            delta,
            sample.t,
            sample.cycle,
            sample.phase.label(),
            sample.content,
            sample.seeded,
            sample.measured,
            sample.slots,
            sample.items,
            label_event(sample),
        );
    }
    for (_, center) in jumps.iter().take(3) {
        let from = center.saturating_sub(7);
        let to = (*center + 4).min(samples.len());
        println!("  · 异常周边帧（中心 {:.2}s）:", samples[*center].t);
        for sample in &samples[from..to] {
            println!(
                "    t={:7.2} {:5} content={:8.0} ∅={:6.1} 已测={:2} pin={} glide={} 事件 {}",
                sample.t,
                sample.phase.label(),
                sample.content,
                sample.seeded,
                sample.measured,
                sample.pinned as u8,
                sample.glide as u8,
                label_event(sample),
            );
        }
    }

    // 无输入时的可见锚点跳变：这是用户真正看到的「内容突然被顶走」。
    //
    // 不能用像素 offset 判断：行被测量 / 重设估算后，同一条逻辑锚点会映射到
    // 不同的像素位置，而画面并没有动。可见锚点是（行号，行内偏移）。
    let mut visible: Vec<(f32, usize, f64, usize)> = Vec::new();
    for (index, pair) in samples.windows(2).enumerate() {
        let next = &pair[1];
        if next.wheel_events > 0 || next.expand != 0 {
            continue;
        }
        let rows_moved = pair[0].top_ix.abs_diff(next.top_ix);
        let inner_moved = (next.top_off - pair[0].top_off).abs();
        if rows_moved > 1 || inner_moved > 80.0 {
            visible.push((inner_moved, rows_moved, next.t, index + 1));
        }
    }
    visible.sort_by(|left, right| right.0.total_cmp(&left.0));
    println!("\n  -- 无滚轮/折叠输入时的可见锚点跳变 Top 10（行内偏移优先） --");
    for (inner_moved, rows_moved, at, index) in visible.iter().take(10) {
        let sample = &samples[*index];
        println!(
            "  行内 {:+.0}px / 跨 {:?} 行 @ {:7.2}s cycle {} {} | pinned {} glide {} 已测 {} | 事件 {}",
            inner_moved,
            rows_moved,
            at,
            sample.cycle,
            sample.phase.label(),
            sample.pinned,
            sample.glide,
            sample.measured,
            label_event(sample),
        );
    }

    // 滚动高度「先塌再回」的摆动：滚动条会明显抽搐，是用户最容易察觉的
    // 「高度异常」形态。
    let mut swings: Vec<(f32, f32, f64, usize)> = Vec::new();
    for (index, pair) in samples.windows(2).enumerate() {
        let delta = pair[1].content - pair[0].content;
        if delta > -400.0 {
            continue;
        }
        let before = pair[0].content;
        let at = pair[1].t;
        let recovered = samples
            .iter()
            .skip(index + 2)
            .take(120)
            .find(|sample| sample.content >= before - 40.0)
            .map(|sample| sample.t);
        if let Some(recovered) = recovered {
            swings.push((-delta, before, recovered - at, index + 1));
        }
    }
    swings.sort_by(|left, right| right.0.total_cmp(&left.0));
    println!("\n  -- 滚动高度骤降后 2s 内又回升的摆动 Top 8 --");
    for (drop, before, recovery, index) in swings.iter().take(8) {
        let sample = &samples[*index];
        println!(
            "  从 {before:.0}px 跌落 {drop:.0}px，{recovery:.2}s 后回升 @ {:7.2}s cycle {} {} | 事件 {}",
            sample.t,
            sample.cycle,
            sample.phase.label(),
            label_event(sample),
        );
    }

    // 校准帧的影响：一次性重设未测量行高度会整体平移像素坐标系。
    let mut calibrated_delta = 0.0f64;
    let mut calibrated_n = 0usize;
    let mut ordinary_delta = 0.0f64;
    let mut ordinary_n = 0usize;
    for pair in samples.windows(2) {
        let delta = (pair[1].content - pair[0].content).abs() as f64;
        if pair[1].calibrated {
            calibrated_delta += delta;
            calibrated_n += 1;
        } else {
            ordinary_delta += delta;
            ordinary_n += 1;
        }
    }
    if calibrated_n > 0 {
        println!(
            "\n  行高校准帧：{} 帧，平均 |Δ内容| {:.0}px（普通帧 {:.0}px）",
            calibrated_n,
            calibrated_delta / calibrated_n as f64,
            if ordinary_n > 0 {
                ordinary_delta / ordinary_n as f64
            } else {
                0.0
            }
        );
    }

    let drops = samples
        .windows(2)
        .filter(|pair| pair[1].content - pair[0].content < -400.0)
        .count();
    let spikes = samples
        .windows(2)
        .filter(|pair| pair[1].content - pair[0].content > 2_500.0)
        .count();
    println!("\n  内容高度单帧骤降 (>400px) {drops} 次 / 骤升 (>2500px) {spikes} 次\n");
}

// --- 测试入口 --------------------------------------------------------------

fn env_f64(name: &str, fallback: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(fallback)
}

#[gpui::test]
fn real_session_streaming_scroll_stress(cx: &mut gpui::TestAppContext) {
    if std::env::var("EFFIBUDDY_STREAM_STRESS").is_err() {
        println!("[stream-stress] 跳过：设置 EFFIBUDDY_STREAM_STRESS=1 以运行");
        return;
    }
    let Some((path, session)) = load_real_session() else {
        panic!("[stream-stress] 找不到可用的真实会话；用 EFFIBUDDY_REAL_SESSION 指定");
    };
    let total_secs = env_f64("EFFIBUDDY_STREAM_STRESS_SECS", 300.0);
    let fps = env_f64("EFFIBUDDY_STREAM_STRESS_FPS", 60.0).max(1.0);
    let seed = std::env::var("EFFIBUDDY_STREAM_STRESS_SEED")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(2026_1119u64);
    println!(
        "[stream-stress] 真实会话 {} | {} 条消息 | {:.1}KB | {total_secs:.0}s @ {fps:.0}fps | 100 tok/s",
        path.display(),
        session.messages.len(),
        std::fs::metadata(&path)
            .map(|meta| meta.len() as f64 / 1024.0)
            .unwrap_or(0.0),
    );
    let title = session.display_title();
    let mut sim = Sim::new(session, seed);
    println!(
        "[stream-stress] 流式脚本 {} 段助手消息 / 可展开目标 {} 个",
        sim.scripts.len(),
        sim.expandos.len()
    );
    if sim.scripts.is_empty() {
        panic!("[stream-stress] 真实会话里没有可流式的助手消息");
    }

    let cx = cx.add_empty_window();
    let started = Instant::now();
    sim.frame(cx, 0.0, 1.0 / fps);
    sim.frame(cx, 0.0, 1.0 / fps);
    sim.chat.pin_to_bottom();
    sim.frame(cx, 0.0, 1.0 / fps);
    println!(
        "[stream-stress] 冷启动：{} 行 / 可滚动 {:.0}px / 顶部行 {}",
        sim.chat.list_state.item_count(),
        f32::from(sim.chat.list_state.max_offset_for_scrollbar().height),
        sim.chat.list_state.logical_scroll_top().item_ix,
    );
    sim.samples.clear();

    let mut last = 0.0f64;
    let mut next_frame_at = Instant::now();
    while last < total_secs {
        let now = Instant::now();
        let wall = now.duration_since(started).as_secs_f64();
        let dt = (wall - last).max(0.0);
        sim.frame(cx, wall, dt);
        last = wall;
        next_frame_at += Duration::from_secs_f64(1.0 / fps);
        elapsed_frame(next_frame_at);
    }

    let messages = sim.session.messages.len();
    print_report(&sim.samples, &title, messages);

    // 采样期间最基本的物理约束（先只做不会和「发现新 bug」冲突的检查）。
    let viewport = sim.samples.last().map(|s| s.viewport).unwrap_or(0.0);
    assert!(viewport > 0.0, "视口没有测量出来");
    assert!(
        sim.samples.iter().any(|s| s.max > viewport * 5.0),
        "整段采样里消息流始终没有可滚动高度"
    );
}
