//! The chat stream: a virtualized transcript plus the live "run" furniture.
//!
//! The conversation is rendered through GPUI's variable-height [`List`], so a
//! long session only lays out (and Markdown-parses) the slice that is actually
//! on screen. Assistant bodies are split into fence-safe markdown chunks and
//! each chunk becomes its own virtual item ([`ChatSlot`]): GPUI re-measures
//! every visible item on every frame, so while the typewriter advances or the
//! smooth scroll glides, the per-frame layout cost is one small chunk instead
//! of a whole multi-kilobyte answer. Chunks are prefix-stable under appends,
//! which keeps measured item heights valid while a message grows.
//!
//! On top of that this module owns the streaming reveal cursor (zero-copy:
//! deltas append into a shared buffer, frames only slice it), the ZCode-like
//! activity rows, and an exponential smooth-scroll glide that intercepts
//! wheel events in the capture phase and drives [`ListState::scroll_by`].
//!
//! Data is snapshotted from [`SessionState`] only on structural events. Token
//! deltas update the reveal buffer in place, which keeps a streaming answer at
//! O(visible chunks) per frame instead of O(transcript).

use std::cell::{Ref, RefCell};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use effibuddy_agent_core::{
    AgentMessageMeta, ContentBlock, Message, Role, RunStatus, SessionState, SubagentColor,
    SubagentStatus, ToolCall, ToolResult,
};
use effibuddy_ui::{
    BadgeTone, GlideTick, IconButton, ScrollGlide, Spinner, Theme, badge, chunk_ranges,
    disclosure_chevron, icons, markdown_source_key, render_markdown_chunk, render_markdown_keyed,
    shadow_float, wheel_capture_overlay,
};
use gpui::prelude::*;
use gpui::{
    Animation, AnimationExt, AnyElement, Canvas, ClipboardItem, Div, Hsla, IntoElement,
    ListAlignment, ListOffset, ListState, ScrollHandle, SharedString, WeakEntity, div,
    pulsating_between, px, svg,
};

use crate::app::AppView;

type ToolResults = HashMap<String, Rc<ToolResult>>;

const CONTENT_MAX_WIDTH: f32 = 760.0;
const CONTENT_PADDING: f32 = 32.0;
const LIST_OVERDRAW: f32 = 560.0;
const ENTER_ANIMATION: Duration = Duration::from_millis(260);
const ENTER_WINDOW: Duration = Duration::from_millis(320);
/// Assistant bodies larger than this are split into per-chunk virtual items.
const SPLIT_TEXT_THRESHOLD: usize = 2048;

/// Measurement escape hatch: `EFFIBUDDY_NO_SPLIT=1` renders every assistant
/// body as one virtual item, reproducing the pre-chunk-split layout cost.
fn no_split() -> bool {
    static NO_SPLIT: OnceLock<bool> = OnceLock::new();
    *NO_SPLIT.get_or_init(|| std::env::var_os("EFFIBUDDY_NO_SPLIT").is_some())
}

// --- public data -----------------------------------------------------------

/// A structural snapshot of the run that the status row depends on. Built by
/// `AppView` right before [`ChatList::rebuild`].
#[derive(Default)]
pub(crate) struct RunSnapshot {
    pub running: bool,
    pub started_at: Option<Instant>,
    pub waiting_approval: bool,
    pub approval_tool: Option<String>,
    pub last_error: Option<String>,
    pub notice: Option<String>,
}

/// Everything a list item needs at layout time. Cheap to clone (the item,
/// slot, and tool-result tables are `Rc`s); expanded-id sets are small.
pub(crate) struct ChatViewState {
    pub items: Rc<Vec<Rc<ChatItem>>>,
    /// One virtual list row per entry: either a whole item or one markdown
    /// chunk of a split assistant message. Rebuilt whenever the transcript is
    /// or the streaming reveal grows past a chunk boundary.
    pub slots: Rc<Vec<ChatSlot>>,
    pub results: Rc<ToolResults>,
    /// Agent task projection keyed by the parent tool-call id.
    pub agents: Rc<HashMap<String, AgentCard>>,
    pub expanded_reasoning: HashSet<String>,
    pub expanded_tools: HashSet<String>,
    /// Turn blocks the user explicitly expanded (overrides the auto fold).
    pub expanded_turns: HashSet<String>,
    /// Turn blocks the user explicitly collapsed (overrides the auto fold).
    pub collapsed_turns: HashSet<String>,
    pub running: bool,
    pub stream: Option<StreamSnapshot>,
    /// Fence-safe chunk table of the revealed streaming text, recomputed per
    /// frame in `prepare_frame`. Rows of the streaming message slice the
    /// revealed text with it.
    pub stream_chunks: Option<Rc<Vec<TextChunk>>>,
    /// Per-message reasoning-body scroll handles (shared with the chat list).
    pub reasoning_scrolls: Rc<RefCell<HashMap<String, ScrollHandle>>>,
    /// Per-code-panel scroll handles (shared with the chat list).
    pub code_scrolls: Rc<RefCell<HashMap<String, ScrollHandle>>>,
}

/// Which piece of an item a virtual row renders. Chunks index into the item's
/// markdown chunk table (or, for the streaming message, the per-frame chunk
/// table of the revealed text).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SlotPart {
    Whole,
    Lead,
    Chunk(usize),
    Tail,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ChatSlot {
    pub item_ix: usize,
    pub part: SlotPart,
}

/// One fence-safe markdown block of an assistant body: a byte range into the
/// message text plus the content hash that keys the markdown chunk cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TextChunk {
    pub start: usize,
    pub end: usize,
    pub key: u64,
}

#[derive(Clone)]
pub(crate) struct AgentCard {
    pub agent_id: String,
    pub profile: String,
    pub color: SubagentColor,
    pub description: String,
    pub status: SubagentStatus,
    pub background: bool,
    pub started_at: Instant,
    pub finished_at: Option<Instant>,
    pub tool_uses: usize,
    pub waiting_approval: bool,
}

pub(crate) struct StreamSnapshot {
    pub message_id: String,
    /// The live reveal buffer shared with the typewriter loop. Frames borrow
    /// it and slice the first [`StreamSnapshot::shown`] bytes; nothing copies
    /// per frame.
    pub text: Rc<RefCell<String>>,
    /// Bytes of [`StreamSnapshot::text`] revealed so far (UTF-8 boundary).
    pub shown: usize,
    pub reasoning: Rc<RefCell<String>>,
    pub shown_reasoning: usize,
    /// Still generating (shows the caret and live reasoning timer).
    pub active: bool,
    /// Time since the assistant message started, for the live "思考中" clock.
    pub elapsed: Duration,
}

impl StreamSnapshot {
    /// Byte length of the revealed text (`shown` clamped to the buffer).
    pub fn shown_len(&self) -> usize {
        self.shown.min(self.text.borrow().len())
    }
}

pub(crate) struct ChatItem {
    pub id: String,
    pub kind: ChatItemKind,
    pub entered_at: Instant,
    signature: u64,
}

pub(crate) enum ChatItemKind {
    User {
        text: String,
    },
    /// A solo assistant message (single-step turn). Shared as an `Rc` so a
    /// rebuild can reuse unchanged messages without cloning them.
    Assistant(Rc<AssistantItem>),
    /// Two or more consecutive assistant messages merged into one block.
    Turn(TurnItem),
    System {
        text: String,
    },
    /// A background-task notification injected into the model context. It is
    /// deliberately not shown as a user bubble or as raw XML.
    TaskNotification {
        summary: String,
    },
    /// A model-visible message delivered by another agent. Rendered as a
    /// first-class inter-agent card in the same stream as main-agent turns.
    AgentMessage {
        meta: AgentMessageMeta,
        text: String,
    },
    RunStatus(RunStatusItem),
    Error {
        text: String,
    },
    Notice {
        text: String,
    },
    Empty,
}

pub(crate) struct AssistantItem {
    pub message: Message,
    pub reasoning_duration: Option<Duration>,
    /// Pre-computed at rebuild time so the per-frame render path neither
    /// joins the text blocks nor hashes/counts them again. `text_key` feeds
    /// the markdown document cache ([`effibuddy_ui::render_markdown_keyed`]).
    pub text: SharedString,
    pub text_key: u64,
    pub reasoning: SharedString,
    pub char_count: usize,
    /// Fence-safe chunk table for long bodies (`None` when the body renders
    /// as one virtual item). Present whenever the body exceeds
    /// [`SPLIT_TEXT_THRESHOLD`], so scrolling a long transcript lays out one
    /// chunk per visible row instead of the whole message every frame.
    pub chunks: Option<Rc<Vec<TextChunk>>>,
}

/// A multi-step agent turn: consecutive assistant messages (one per harness
/// step, separated by tool calls) merged into one visual block, ZCode-style.
/// Every member's reasoning and tool rows stack into one compact activity
/// column; the last member's text renders as the answer body. While the turn
/// is live the activity stays expanded; once it finishes it folds into a
/// one-line summary unless the user toggled it.
pub(crate) struct TurnItem {
    /// `turn:{first_member_id}` — stable as the turn grows, so the expand
    /// state sets and the entry animation survive merges.
    pub id: String,
    pub members: Vec<Rc<AssistantItem>>,
    /// Rebuild-time signatures of `members`, in order. Keys the item
    /// signature and lets the next rebuild reuse unchanged members.
    pub signatures: Vec<u64>,
    /// Members with a non-empty reasoning block.
    pub thinking_count: usize,
    /// Non-agent tool calls across all members.
    pub tool_calls: usize,
    /// `agent` (subagent) tool calls across all members.
    pub agent_calls: usize,
    /// Summed tool durations, for the summary line.
    pub total_ms: u64,
    /// Whether any tool result errored.
    pub any_error: bool,
}

pub(crate) struct RunStatusItem {
    pub started_at: Option<Instant>,
    pub step: usize,
    pub activity: String,
    pub detail: Option<String>,
}

// --- the list --------------------------------------------------------------

pub(crate) struct ChatList {
    pub list_state: ListState,
    items: Rc<Vec<Rc<ChatItem>>>,
    /// Virtual rows derived from `items` (+ the live streaming chunks). Kept
    /// so a rebuild can splice while preserving measured chunk heights.
    slots: Rc<Vec<ChatSlot>>,
    /// Row identity for the prefix splice: `(item signature, chunk key)`.
    /// Part tags are deliberately excluded so a grown streaming tail — whose
    /// finalized chunks reappear as middle rows — still matches positionally
    /// and the splice stays an append.
    slot_keys: Rc<Vec<(u64, u64)>>,
    results: Rc<ToolResults>,
    durations: HashMap<String, Duration>,
    entered: HashMap<String, Instant>,
    first_sync: bool,
    /// Following the tail. Cleared when the user scrolls up, restored when
    /// they return to the bottom (or tap the jump button).
    pinned: bool,
    scroll_dirty: bool,
    new_content_while_unpinned: bool,
    reveal: StreamReveal,
    /// Per-message reasoning-body scroll handles, created lazily at render
    /// time and shared with the view through `Rc<RefCell<_>>`.
    reasoning_scrolls: Rc<RefCell<HashMap<String, ScrollHandle>>>,
    /// Per-code-panel scroll handles, keyed `call-id + title`.
    code_scrolls: Rc<RefCell<HashMap<String, ScrollHandle>>>,
    /// Guards against scheduling more than one reveal frame at a time.
    pub reveal_frame_scheduled: bool,
    /// Smooth-scroll glide state (wheel capture + animated scroll-to-bottom).
    glide: ScrollGlide,
    /// Guards against scheduling more than one glide frame at a time.
    pub glide_frame_scheduled: bool,
}

/// Exponential glide toward a target pixel offset. Wheel deltas move the
/// target; each frame moves `current` a fraction of the remaining distance,
/// so repeated notches accumulate into one continuous motion instead of
/// discrete jumps. Shared with every other overflow surface through
/// `effibuddy_ui::ScrollGlide`.

impl ChatList {
    pub fn new() -> Self {
        Self {
            list_state: ListState::new(0, ListAlignment::Top, px(LIST_OVERDRAW)),
            items: Rc::new(Vec::new()),
            slots: Rc::new(Vec::new()),
            slot_keys: Rc::new(Vec::new()),
            results: Rc::new(HashMap::new()),
            durations: HashMap::new(),
            entered: HashMap::new(),
            first_sync: true,
            pinned: true,
            scroll_dirty: false,
            new_content_while_unpinned: false,
            reveal: StreamReveal::default(),
            reasoning_scrolls: Rc::new(RefCell::new(HashMap::new())),
            code_scrolls: Rc::new(RefCell::new(HashMap::new())),
            reveal_frame_scheduled: false,
            glide: ScrollGlide::new(),
            glide_frame_scheduled: false,
        }
    }

    pub fn install_scroll_handler(&self, entity: WeakEntity<AppView>) {
        self.list_state
            .set_scroll_handler(move |_event, _window, cx| {
                let _ = entity.update(cx, |this, cx| {
                    this.chat.scroll_dirty = true;
                    cx.notify();
                });
            });
    }

    /// The child transcript uses its own list state; scroll intent must be
    /// recorded on the matching side-pane agent view.
    pub fn install_agent_scroll_handler(&self, agent_id: String, entity: WeakEntity<AppView>) {
        self.list_state
            .set_scroll_handler(move |_event, _window, cx| {
                let _ = entity.update(cx, |this, cx| {
                    if let Some(view) = this.side.agent_views.get_mut(&agent_id) {
                        view.chat.scroll_dirty = true;
                    }
                    cx.notify();
                });
            });
    }

    // --- session synchronization ------------------------------------------

    /// Drop every cached snapshot. Called when switching sessions so the next
    /// rebuild starts from a clean slate and nothing entry-animates.
    pub fn reset_session(&mut self) {
        self.items = Rc::new(Vec::new());
        self.slots = Rc::new(Vec::new());
        self.slot_keys = Rc::new(Vec::new());
        self.results = Rc::new(HashMap::new());
        self.durations.clear();
        self.entered.clear();
        self.first_sync = true;
        self.pinned = true;
        self.scroll_dirty = false;
        self.new_content_while_unpinned = false;
        self.reveal = StreamReveal::default();
        self.reasoning_scrolls = Rc::new(RefCell::new(HashMap::new()));
        self.code_scrolls = Rc::new(RefCell::new(HashMap::new()));
        self.glide = ScrollGlide::new();
        self.list_state.reset(0);
    }

    /// Rebuild the item snapshot from the session. Only invoked when a
    /// structural event happened, never on token deltas.
    pub fn rebuild(&mut self, session: &SessionState, run: &RunSnapshot) {
        let now = Instant::now();
        let old_items = self.items.clone();
        let old_results = self.results.clone();

        // Reuse tool result payloads that did not change; transcripts can hold
        // megabyte-sized observations and this runs on every tool boundary.
        let mut results: ToolResults = HashMap::new();
        for message in &session.messages {
            for block in &message.blocks {
                if let ContentBlock::ToolResult { result } = block {
                    let reused = old_results
                        .get(&result.call_id)
                        .filter(|old| same_result(old, result))
                        .cloned();
                    results.insert(
                        result.call_id.clone(),
                        reused.unwrap_or_else(|| Rc::new(result.clone())),
                    );
                }
            }
        }

        let mut items: Vec<Rc<ChatItem>> = Vec::with_capacity(session.messages.len() + 3);
        // Reuse lookups by id: with turn merging the item list no longer maps
        // 1:1 onto messages, so positional reuse would misfire after a merge.
        let old_by_id: HashMap<&str, &Rc<ChatItem>> = old_items
            .iter()
            .map(|item| (item.id.as_str(), item))
            .collect();
        let mut old_members: HashMap<&str, (Rc<AssistantItem>, u64)> = HashMap::new();
        for item in old_items.iter() {
            match &item.kind {
                ChatItemKind::Assistant(assistant) => {
                    old_members.insert(item.id.as_str(), (assistant.clone(), item.signature));
                }
                ChatItemKind::Turn(turn) => {
                    for (member, signature) in turn.members.iter().zip(&turn.signatures) {
                        old_members
                            .insert(member.message.id.as_str(), (member.clone(), *signature));
                    }
                }
                _ => {}
            }
        }

        // A run of consecutive assistant messages (one per harness step) is
        // one turn; flush it whenever a different kind of message shows up.
        let mut group: Vec<(&Message, u64, Rc<AssistantItem>)> = Vec::new();
        for message in session.messages.iter() {
            if message.role == Role::Tool {
                continue;
            }
            let duration = self.durations.get(&message.id).copied();
            let signature = message_signature(message, &results, duration);
            if message.role == Role::Assistant {
                let assistant = match old_members.get(message.id.as_str()) {
                    Some((existing, old_signature)) if *old_signature == signature => {
                        existing.clone()
                    }
                    _ => Rc::new(build_assistant_item(message, duration)),
                };
                group.push((message, signature, assistant));
                continue;
            }
            flush_assistant_group(
                &mut items,
                &mut group,
                &old_by_id,
                &results,
                &self.entered,
                now,
            );
            if let Some(existing) = old_by_id
                .get(message.id.as_str())
                .filter(|item| item.signature == signature)
            {
                items.push((*existing).clone());
                continue;
            }
            let kind = match message.role {
                Role::User if message.agent_message.is_some() => {
                    let meta = message.agent_message.clone().expect("checked");
                    ChatItemKind::AgentMessage {
                        text: meta.text.clone(),
                        meta,
                    }
                }
                Role::User if message.synthetic => ChatItemKind::TaskNotification {
                    summary: notification_summary(&message.text()),
                },
                Role::User => ChatItemKind::User {
                    text: message.text(),
                },
                Role::System => ChatItemKind::System {
                    text: message.text(),
                },
                // Assistant and Tool roles are handled above.
                _ => continue,
            };
            items.push(Rc::new(ChatItem {
                id: message.id.clone(),
                kind,
                entered_at: self.entered.get(&message.id).copied().unwrap_or(now),
                signature,
            }));
        }
        flush_assistant_group(
            &mut items,
            &mut group,
            &old_by_id,
            &results,
            &self.entered,
            now,
        );

        let mut extras: Vec<(String, ChatItemKind, u64)> = Vec::new();
        if run.running {
            let waiting = run.waiting_approval || session.status == RunStatus::WaitingApproval;
            let (activity, detail) = if waiting {
                ("等待你的批准".to_string(), run.approval_tool.clone())
            } else if let Some(call) = unresolved_call(session, &results) {
                (format!("正在执行 {}", call.name), Some(tool_detail(call)))
            } else {
                ("正在思考…".to_string(), None)
            };
            let signature = hash_values(&(activity.as_str(), detail.as_deref(), session.steps));
            extras.push((
                "item:run-status".to_string(),
                ChatItemKind::RunStatus(RunStatusItem {
                    started_at: run.started_at,
                    step: session.steps,
                    activity,
                    detail,
                }),
                signature,
            ));
        }
        if let Some(error) = &run.last_error {
            let signature = hash_values(error);
            extras.push((
                "item:error".to_string(),
                ChatItemKind::Error {
                    text: error.clone(),
                },
                signature,
            ));
        }
        if !run.running && !items.is_empty() {
            if let Some(notice) = &run.notice {
                let signature = hash_values(notice);
                extras.push((
                    "item:notice".to_string(),
                    ChatItemKind::Notice {
                        text: notice.clone(),
                    },
                    signature,
                ));
            }
        }
        if items.is_empty() {
            extras.push((
                "item:empty".to_string(),
                ChatItemKind::Empty,
                hash_values(&"empty"),
            ));
        }

        for (id, kind, signature) in extras {
            // Synthetic rows are cheap; always rebuild them so a new run gets
            // a fresh started_at instead of reusing the previous run's clock.
            items.push(Rc::new(ChatItem {
                id,
                kind,
                // Synthetic rows never fade in; only real messages do.
                entered_at: now - Duration::from_secs(10),
                signature,
            }));
        }

        let mut next_entered = HashMap::with_capacity(items.len());
        for item in &items {
            next_entered.insert(item.id.clone(), item.entered_at);
        }

        self.items = Rc::new(items);
        self.results = Rc::new(results);
        self.entered = next_entered;
        // Virtual-row splicing happens in `prepare_frame`, which also knows
        // the live streaming chunk table and preserves measured heights.
        if self.pinned {
            self.scroll_to_bottom();
        }
    }

    /// Fence-safe chunk table for an assistant body, or `None` when the body
    /// is short enough to render as a single virtual item.
    fn split_chunks(text: &str) -> Option<Rc<Vec<TextChunk>>> {
        if text.len() <= SPLIT_TEXT_THRESHOLD || no_split() {
            return None;
        }
        let chunks: Vec<TextChunk> = chunk_ranges(text)
            .into_iter()
            .map(|range| TextChunk {
                start: range.start,
                end: range.end,
                key: markdown_source_key(&text[range]),
            })
            .collect();
        (chunks.len() >= 2).then(|| Rc::new(chunks))
    }

    /// Per-frame preparation: rebuild the virtual-row plan from the current
    /// items and streaming reveal, splice the list so measured chunk heights
    /// survive structural changes, and hand the render pass its view state.
    pub fn prepare_frame(
        &mut self,
        running: bool,
        expanded_reasoning: &HashSet<String>,
        expanded_tools: &HashSet<String>,
        expanded_turns: &HashSet<String>,
        collapsed_turns: &HashSet<String>,
        agents: &HashMap<String, AgentCard>,
    ) -> ChatViewState {
        let stream = self.reveal.snapshot();
        let streaming_id = stream.as_ref().map(|stream| stream.message_id.clone());
        let streaming_chunks = if no_split() {
            None
        } else {
            stream
                .as_ref()
                .map(|stream| Rc::new(stream_chunk_table(stream)))
        };

        // Build the virtual-row plan. Assistant items with a chunk table
        // expand into lead/chunk/tail rows; the streaming message expands
        // into the per-frame table of its revealed text.
        let mut slots: Vec<ChatSlot> = Vec::with_capacity(self.items.len() + 8);
        let mut keys: Vec<(u64, u64)> = Vec::with_capacity(slots.capacity());
        for (item_ix, item) in self.items.iter().enumerate() {
            let signature = item.signature;
            // The streaming message is always the newest assistant message:
            // the last member of its turn, or a solo assistant item.
            let streaming = match &item.kind {
                ChatItemKind::Assistant(assistant) => {
                    streaming_id.as_deref() == Some(assistant.message.id.as_str())
                }
                ChatItemKind::Turn(turn) => turn.members.last().is_some_and(|member| {
                    streaming_id.as_deref() == Some(member.message.id.as_str())
                }),
                _ => false,
            };
            let chunks: Option<&[TextChunk]> = if streaming {
                streaming_chunks.as_deref().map(Vec::as_slice)
            } else {
                match &item.kind {
                    ChatItemKind::Assistant(assistant) => {
                        assistant.chunks.as_deref().map(Vec::as_slice)
                    }
                    ChatItemKind::Turn(turn) => turn
                        .members
                        .last()
                        .and_then(|member| member.chunks.as_deref().map(Vec::as_slice)),
                    _ => None,
                }
            };
            match chunks {
                Some(chunks) if chunks.len() >= 2 => {
                    slots.push(ChatSlot {
                        item_ix,
                        part: SlotPart::Lead,
                    });
                    keys.push((signature, 0));
                    for index in 1..chunks.len() - 1 {
                        slots.push(ChatSlot {
                            item_ix,
                            part: SlotPart::Chunk(index),
                        });
                        keys.push((signature, chunks[index].key));
                    }
                    slots.push(ChatSlot {
                        item_ix,
                        part: SlotPart::Tail,
                    });
                    keys.push((signature, chunks[chunks.len() - 1].key));
                }
                _ => {
                    slots.push(ChatSlot {
                        item_ix,
                        part: SlotPart::Whole,
                    });
                    keys.push((signature, 0));
                }
            }
        }

        // Splice only from the first changed row so the measured heights of
        // the stable prefix survive. See `slot_keys` for why this stays an
        // append while the streaming tail grows.
        let new_slots = Rc::new(slots);
        let new_keys = Rc::new(keys);
        let old_len = self.slots.len();
        let new_len = new_slots.len();
        if self.first_sync {
            self.list_state.reset(new_len);
        } else {
            let prefix = self
                .slot_keys
                .iter()
                .zip(new_keys.iter())
                .take_while(|(old, new)| old == new)
                .count();
            if prefix < old_len || new_len != old_len {
                self.list_state.splice(prefix..old_len, new_len - prefix);
            }
        }
        self.slots = new_slots;
        self.slot_keys = new_keys;
        self.first_sync = false;

        ChatViewState {
            items: self.items.clone(),
            slots: Rc::clone(&self.slots),
            results: Rc::clone(&self.results),
            agents: Rc::new(agents.clone()),
            expanded_reasoning: expanded_reasoning.clone(),
            expanded_tools: expanded_tools.clone(),
            expanded_turns: expanded_turns.clone(),
            collapsed_turns: collapsed_turns.clone(),
            running,
            stream_chunks: streaming_chunks,
            stream,
            reasoning_scrolls: Rc::clone(&self.reasoning_scrolls),
            code_scrolls: Rc::clone(&self.code_scrolls),
        }
    }

    // --- streaming reveal --------------------------------------------------

    pub fn begin_message(&mut self, message_id: &str) {
        self.reveal.begin(message_id);
    }

    /// Seed the reveal buffer from a message that was already partially
    /// streamed before this list was opened (for example when the user opens a
    /// running subagent transcript mid-turn).
    pub fn prime_stream(&mut self, message: &Message) {
        if self.reveal.message_id.as_deref() == Some(message.id.as_str()) {
            return;
        }
        self.reveal.begin(&message.id);
        *self.reveal.text.borrow_mut() = message.text();
        self.reveal.shown_text = self.reveal.text.borrow().len();
        *self.reveal.reasoning.borrow_mut() = message.reasoning();
        self.reveal.shown_reasoning = self.reveal.reasoning.borrow().len();
        self.reveal.active = true;
    }

    /// Preview/test hook: seed a reveal buffer that is still draining a large
    /// backlog, so the typewriter stress path runs without a live model.
    pub fn prime_stream_backlog(&mut self, message: &Message, shown_chars: usize) {
        self.reveal.begin(&message.id);
        *self.reveal.text.borrow_mut() = message.text();
        let text = self.reveal.text.borrow();
        self.reveal.shown_text = advance_chars(&text, 0, shown_chars);
        self.reveal.active = true;
    }

    /// Remaining typewriter backlog in bytes (0 once fully revealed).
    pub fn stream_backlog_bytes(&self) -> usize {
        self.reveal
            .text
            .borrow()
            .len()
            .saturating_sub(self.reveal.shown_text)
    }

    /// Bench helper: reset the reveal cursor back to `shown_chars` so the
    /// stress storm keeps running across a whole bench window.
    pub fn refill_stream_backlog(&mut self, message: &Message, shown_chars: usize) {
        self.reveal.begin(&message.id);
        if self.reveal.text.borrow().is_empty() {
            *self.reveal.text.borrow_mut() = message.text();
        }
        let text = self.reveal.text.borrow();
        self.reveal.shown_text = advance_chars(&text, 0, shown_chars);
        self.reveal.active = true;
    }

    pub fn push_text(&mut self, message_id: &str, delta: &str) {
        self.reveal.push_text(message_id, delta);
        self.note_growth();
    }

    pub fn push_reasoning(&mut self, message_id: &str, delta: &str) {
        self.reveal.push_reasoning(message_id, delta);
        self.note_growth();
    }

    /// Reconcile the reveal buffer with the authoritative finished message and
    /// remember how long the thinking phase took.
    pub fn finish_message(&mut self, message: &Message) {
        if self.reveal.message_id.as_deref() != Some(message.id.as_str()) {
            self.reveal.begin(&message.id);
        }
        self.reveal.flush_from(message);
        if let Some(duration) = self.reveal.reasoning_duration() {
            self.durations.insert(message.id.clone(), duration);
        }
    }

    pub fn end_stream(&mut self) {
        self.reveal.message_id = None;
        self.reveal.active = false;
    }

    /// Advance the typewriter. Returns true while text is still hidden.
    pub fn tick(&mut self) -> bool {
        self.reveal.advance()
    }

    pub fn note_growth(&mut self) {
        if !self.pinned {
            self.new_content_while_unpinned = true;
        }
    }

    // --- scroll ------------------------------------------------------------

    pub fn pinned(&self) -> bool {
        self.pinned
    }

    pub fn new_content(&self) -> bool {
        self.new_content_while_unpinned
    }

    /// Re-evaluate whether the viewport is at the bottom, then keep it pinned
    /// when it is. Called before the list is laid out for the frame.
    pub fn sync_scroll(&mut self) {
        if self.scroll_dirty {
            self.scroll_dirty = false;
            let max = self.list_state.max_offset_for_scrollbar().height;
            let offset = -self.list_state.scroll_px_offset_for_scrollbar().y;
            let at_bottom = offset + px(0.75) >= max;
            self.pinned = at_bottom;
            if at_bottom {
                self.new_content_while_unpinned = false;
            }
        }
        if self.pinned {
            self.scroll_to_bottom();
        }
    }

    pub fn mark_scrolled(&mut self) {
        self.scroll_dirty = true;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.list_state.scroll_to(ListOffset {
            item_ix: self.list_state.item_count(),
            offset_in_item: px(0.0),
        });
    }

    pub fn pin_to_bottom(&mut self) {
        self.pinned = true;
        self.new_content_while_unpinned = false;
        self.scroll_to_bottom();
    }

    // --- smooth scrolling ---------------------------------------------------

    /// Feed one wheel event into the glide (called from the capture-phase
    /// interceptor). `pixel_y` follows GPUI's wheel convention: positive =
    /// scroll up (toward smaller offsets).
    pub fn wheel(&mut self, pixel_y: f32) {
        if pixel_y == 0.0 {
            return;
        }
        if pixel_y > 0.0 {
            // Scrolling up always unpins, even before the first glide tick
            // moves the list: `sync_scroll` would otherwise force the viewport
            // back to the bottom with a stale `pinned` flag.
            self.pinned = false;
        }
        let max = f32::from(self.list_state.max_offset_for_scrollbar().height);
        let current = -f32::from(self.list_state.scroll_px_offset_for_scrollbar().y);
        self.glide.wheel(pixel_y, current, max);
    }

    /// Start an animated glide to the bottom (scroll-to-bottom button).
    pub fn glide_to_bottom(&mut self) {
        let max = f32::from(self.list_state.max_offset_for_scrollbar().height);
        let current = -f32::from(self.list_state.scroll_px_offset_for_scrollbar().y);
        self.glide.glide_to_bottom(current, max);
    }

    /// Stop any glide and resync to wherever the list actually is (scrollbar
    /// jump, external scroll).
    pub fn cancel_glide(&mut self) {
        self.glide.cancel();
        self.glide_frame_scheduled = false;
    }

    /// Advance one glide step. Returns true while the glide still needs
    /// frames; `render_chat` schedules the next frame and notifies.
    pub fn tick_scroll(&mut self) -> bool {
        if !self.glide.active() {
            return false;
        }
        let max = f32::from(self.list_state.max_offset_for_scrollbar().height);
        let real = -f32::from(self.list_state.scroll_px_offset_for_scrollbar().y);
        match self.glide.tick(real, max) {
            GlideTick::Continue => true,
            GlideTick::Step(delta) => {
                if delta.abs() > 0.01 {
                    self.list_state.scroll_by(px(delta));
                }
                self.glide.commit(-f32::from(
                    self.list_state.scroll_px_offset_for_scrollbar().y,
                ));
                true
            }
            GlideTick::Done { delta, at_bottom } => {
                if delta.abs() > 0.01 {
                    self.list_state.scroll_by(px(delta));
                }
                self.glide.commit(-f32::from(
                    self.list_state.scroll_px_offset_for_scrollbar().y,
                ));
                if at_bottom {
                    self.pinned = true;
                    self.new_content_while_unpinned = false;
                    self.scroll_to_bottom();
                }
                false
            }
            GlideTick::Cancelled => false,
        }
    }

    /// Scroll a nested tracked region (reasoning body, code panel) by a raw
    /// wheel delta. Called from its claim overlay, which has already won the
    /// capture phase against the chat-level interceptor.
    fn apply_nested_scroll(&mut self, handle: &ScrollHandle, pixel_y: f32) {
        let max = handle.max_offset();
        let offset = handle.offset();
        let next_y = (offset.y - px(pixel_y)).clamp(-max.height, px(0.0));
        handle.set_offset(gpui::point(offset.x, next_y));
    }

    // --- scrollbar ---------------------------------------------------------

    /// `(thumb_top, thumb_height, viewport)` for the overlay scrollbar, or
    /// `None` when the content fits.
    pub fn scrollbar_geometry(&self) -> Option<(f32, f32, f32)> {
        let viewport = self.list_state.viewport_bounds().size.height;
        if viewport <= px(0.0) {
            return None;
        }
        let max = self.list_state.max_offset_for_scrollbar().height;
        if max <= px(0.5) {
            return None;
        }
        let viewport = f32::from(viewport);
        let max = f32::from(max);
        let content = viewport + max;
        let thumb_height = (viewport / content * viewport).max(26.0);
        let offset = -f32::from(self.list_state.scroll_px_offset_for_scrollbar().y);
        let travel = (viewport - thumb_height).max(0.0);
        let top = if max > 0.0 {
            (offset / max).clamp(0.0, 1.0) * travel
        } else {
            0.0
        };
        Some((top, thumb_height, viewport))
    }
}

impl Default for ChatList {
    fn default() -> Self {
        Self::new()
    }
}

// --- derived row data ------------------------------------------------------

fn unresolved_call<'a>(session: &'a SessionState, results: &ToolResults) -> Option<&'a ToolCall> {
    let message = session
        .messages
        .iter()
        .rev()
        .find(|message| message.role == Role::Assistant)?;
    message.blocks.iter().find_map(|block| match block {
        ContentBlock::ToolCall { call } if !results.contains_key(&call.id) => Some(call),
        _ => None,
    })
}

fn same_result(left: &ToolResult, right: &ToolResult) -> bool {
    left.call_id == right.call_id
        && left.is_error == right.is_error
        && left.duration_ms == right.duration_ms
        && left.content == right.content
}

/// Derive the per-message assistant snapshot: joined text/reasoning, the
/// markdown source key, and the fence-safe chunk table for long bodies.
fn build_assistant_item(message: &Message, reasoning_duration: Option<Duration>) -> AssistantItem {
    let text = message.text();
    let reasoning = message.reasoning();
    let chunks = ChatList::split_chunks(&text);
    AssistantItem {
        message: message.clone(),
        reasoning_duration,
        text_key: markdown_source_key(&text),
        char_count: text.chars().count(),
        text: text.into(),
        reasoning: reasoning.into(),
        chunks,
    }
}

/// Flush a pending run of consecutive assistant messages into the item list.
/// A single message keeps the plain assistant item; two or more merge into
/// one [`TurnItem`] block whose id is derived from the first member, so the
/// entry animation and expand state survive as the turn grows.
fn flush_assistant_group(
    items: &mut Vec<Rc<ChatItem>>,
    group: &mut Vec<(&Message, u64, Rc<AssistantItem>)>,
    old_by_id: &HashMap<&str, &Rc<ChatItem>>,
    results: &ToolResults,
    entered: &HashMap<String, Instant>,
    now: Instant,
) {
    if group.is_empty() {
        return;
    }
    if group.len() == 1 {
        let (message, signature, assistant) = group.pop().expect("non-empty");
        if let Some(existing) = old_by_id
            .get(message.id.as_str())
            .filter(|item| item.signature == signature)
        {
            items.push((*existing).clone());
            return;
        }
        items.push(Rc::new(ChatItem {
            id: message.id.clone(),
            kind: ChatItemKind::Assistant(assistant),
            entered_at: entered.get(&message.id).copied().unwrap_or(now),
            signature,
        }));
        return;
    }

    let signatures: Vec<u64> = group.iter().map(|(_, signature, _)| *signature).collect();
    let first_id = group.first().expect("non-empty").0.id.clone();
    let id = format!("turn:{first_id}");
    let signature = {
        let mut hasher = DefaultHasher::new();
        "turn".hash(&mut hasher);
        for member_signature in &signatures {
            member_signature.hash(&mut hasher);
        }
        hasher.finish()
    };
    // Nothing changed since the last rebuild: reuse the whole block.
    if let Some(existing) = old_by_id
        .get(id.as_str())
        .filter(|item| item.signature == signature)
    {
        group.clear();
        items.push((*existing).clone());
        return;
    }

    let mut members = Vec::with_capacity(group.len());
    let mut thinking_count = 0usize;
    let mut tool_calls = 0usize;
    let mut agent_calls = 0usize;
    let mut total_ms = 0u64;
    let mut any_error = false;
    for (message, _, assistant) in group.drain(..) {
        if !assistant.reasoning.is_empty() {
            thinking_count += 1;
        }
        for block in &message.blocks {
            if let ContentBlock::ToolCall { call } = block {
                if call.name == "agent" {
                    agent_calls += 1;
                } else {
                    tool_calls += 1;
                }
                if let Some(result) = results.get(&call.id) {
                    total_ms += result.duration_ms.unwrap_or(0);
                    any_error |= result.is_error;
                }
            }
        }
        members.push(assistant);
    }

    // The first merge replaces a standalone item whose id was the message id;
    // carry its entry time over so the block does not re-fade.
    let entered_at = entered
        .get(&id)
        .or_else(|| entered.get(&first_id))
        .copied()
        .unwrap_or(now);
    let turn = TurnItem {
        id: id.clone(),
        members,
        signatures,
        thinking_count,
        tool_calls,
        agent_calls,
        total_ms,
        any_error,
    };
    items.push(Rc::new(ChatItem {
        id,
        kind: ChatItemKind::Turn(turn),
        entered_at,
        signature,
    }));
}

fn message_signature(
    message: &Message,
    results: &ToolResults,
    reasoning_duration: Option<Duration>,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    message.id.hash(&mut hasher);
    message.interrupted.hash(&mut hasher);
    reasoning_duration
        .map(|duration| duration.as_millis())
        .hash(&mut hasher);
    for block in &message.blocks {
        match block {
            ContentBlock::Text { text } => {
                0u8.hash(&mut hasher);
                text.hash(&mut hasher);
            }
            ContentBlock::Reasoning { text } => {
                1u8.hash(&mut hasher);
                text.hash(&mut hasher);
            }
            ContentBlock::ToolCall { call } => {
                2u8.hash(&mut hasher);
                call.id.hash(&mut hasher);
                call.name.hash(&mut hasher);
                if let Some(result) = results.get(&call.id) {
                    result.content.len().hash(&mut hasher);
                    result.is_error.hash(&mut hasher);
                    result.duration_ms.hash(&mut hasher);
                } else {
                    0usize.hash(&mut hasher);
                }
            }
            ContentBlock::ToolResult { result } => {
                3u8.hash(&mut hasher);
                result.call_id.hash(&mut hasher);
                result.content.len().hash(&mut hasher);
                result.is_error.hash(&mut hasher);
                result.duration_ms.hash(&mut hasher);
            }
        }
    }
    hasher.finish()
}

fn hash_values(values: &impl Hash) -> u64 {
    let mut hasher = DefaultHasher::new();
    values.hash(&mut hasher);
    hasher.finish()
}

// --- reveal ----------------------------------------------------------------

#[derive(Default)]
struct StreamReveal {
    message_id: Option<String>,
    /// Shared with the per-frame `StreamSnapshot`: deltas append in place,
    /// frames only borrow and slice, so nothing copies per frame.
    text: Rc<RefCell<String>>,
    shown_text: usize,
    reasoning: Rc<RefCell<String>>,
    shown_reasoning: usize,
    active: bool,
    started_at: Option<Instant>,
    reasoning_started: Option<Instant>,
    reasoning_elapsed: Option<Duration>,
}

impl StreamReveal {
    fn begin(&mut self, message_id: &str) {
        if self.message_id.as_deref() == Some(message_id) {
            return;
        }
        let started_at = Some(Instant::now());
        *self = Self {
            message_id: Some(message_id.to_string()),
            active: true,
            started_at,
            ..Self::default()
        };
    }

    fn push_text(&mut self, message_id: &str, delta: &str) {
        self.begin(message_id);
        self.active = true;
        if self.reasoning_started.is_some() && self.reasoning_elapsed.is_none() {
            self.reasoning_elapsed = self.reasoning_started.map(|start| start.elapsed());
        }
        self.text.borrow_mut().push_str(delta);
    }

    fn push_reasoning(&mut self, message_id: &str, delta: &str) {
        self.begin(message_id);
        self.active = true;
        if self.reasoning_started.is_none() {
            self.reasoning_started = Some(Instant::now());
        }
        self.reasoning.borrow_mut().push_str(delta);
    }

    fn flush_from(&mut self, message: &Message) {
        let text = message.text();
        let reasoning = message.reasoning();
        *self.text.borrow_mut() = text;
        self.shown_text = self.text.borrow().len();
        *self.reasoning.borrow_mut() = reasoning;
        self.shown_reasoning = self.reasoning.borrow().len();
        if self.reasoning_started.is_some() && self.reasoning_elapsed.is_none() {
            self.reasoning_elapsed = self.reasoning_started.map(|start| start.elapsed());
        }
        self.active = false;
        if self.started_at.is_none() {
            self.started_at = Some(Instant::now());
        }
    }

    fn reasoning_duration(&self) -> Option<Duration> {
        if self.reasoning.borrow().is_empty() {
            return None;
        }
        self.reasoning_elapsed
            .or_else(|| self.reasoning_started.map(|start| start.elapsed()))
    }

    fn advance(&mut self) -> bool {
        {
            let text = self.text.borrow_mut();
            let text_backlog = text.len().saturating_sub(self.shown_text);
            if text_backlog > 0 {
                let step = (4 + text_backlog / 10).min(160);
                self.shown_text = advance_chars(&text, self.shown_text, step);
            }
        }
        {
            let reasoning = self.reasoning.borrow_mut();
            let reasoning_backlog = reasoning.len().saturating_sub(self.shown_reasoning);
            if reasoning_backlog > 0 {
                let step = (3 + reasoning_backlog / 12).min(140);
                self.shown_reasoning = advance_chars(&reasoning, self.shown_reasoning, step);
            }
        }
        self.text.borrow().len() > self.shown_text
            || self.reasoning.borrow().len() > self.shown_reasoning
    }

    /// Zero-copy view of the reveal state: shares the buffers, copies only
    /// scalars. Render paths borrow and slice; the typewriter appends.
    fn snapshot(&self) -> Option<StreamSnapshot> {
        let message_id = self.message_id.clone()?;
        Some(StreamSnapshot {
            message_id,
            text: Rc::clone(&self.text),
            shown: self.shown_text,
            reasoning: Rc::clone(&self.reasoning),
            shown_reasoning: self.shown_reasoning,
            active: self.active,
            elapsed: self
                .started_at
                .map(|start| start.elapsed())
                .unwrap_or_default(),
        })
    }
}

/// Fence-safe chunk table over the revealed streaming text. Prefix-stable as
/// the typewriter advances, so chunk-cache entries and measured item heights
/// survive between frames.
fn stream_chunk_table(stream: &StreamSnapshot) -> Vec<TextChunk> {
    let text = stream.text.borrow();
    let revealed = &text[..stream.shown_len()];
    chunk_ranges(revealed)
        .into_iter()
        .map(|range| TextChunk {
            start: range.start,
            end: range.end,
            key: markdown_source_key(&revealed[range]),
        })
        .collect()
}

/// Advance `from` by `count` characters, always landing on a UTF-8 boundary.
fn advance_chars(text: &str, from: usize, count: usize) -> usize {
    let mut index = from.min(text.len());
    let mut remaining = count;
    while remaining > 0 && index < text.len() {
        match text[index..].chars().next() {
            Some(ch) => {
                index += ch.len_utf8();
                remaining -= 1;
            }
            None => break,
        }
    }
    index
}

// --- rendering -------------------------------------------------------------

pub(crate) fn render_chat_item(
    index: usize,
    view: &ChatViewState,
    theme: &Theme,
    entity: &WeakEntity<AppView>,
) -> AnyElement {
    let Some(slot) = view.slots.get(index) else {
        return div().into_any_element();
    };
    let Some(item) = view.items.get(slot.item_ix) else {
        return div().into_any_element();
    };
    let content = match (&item.kind, slot.part) {
        (_, SlotPart::Whole) => match &item.kind {
            ChatItemKind::User { text } => render_user(theme, text),
            ChatItemKind::Assistant(assistant) => render_assistant(assistant, view, theme, entity),
            ChatItemKind::Turn(turn) => render_turn(turn, view, theme, entity),
            ChatItemKind::System { text } => div()
                .w_full()
                .flex()
                .justify_center()
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(theme.text_faint)
                        .child(text.clone()),
                )
                .into_any_element(),
            ChatItemKind::TaskNotification { summary } => render_task_notification(theme, summary),
            ChatItemKind::AgentMessage { meta, text } => {
                crate::chat_agent::render_agent_message(theme, meta, text)
            }
            ChatItemKind::RunStatus(status) => render_run_status(
                status,
                view.stream
                    .as_ref()
                    .is_some_and(|stream| stream.active && stream.shown_len() > 0),
                theme,
                entity,
            ),
            ChatItemKind::Error { text } => render_error(theme, text),
            ChatItemKind::Notice { text } => div()
                .w_full()
                .flex()
                .justify_center()
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(theme.text_faint)
                        .child(text.clone()),
                )
                .into_any_element(),
            ChatItemKind::Empty => div()
                .w_full()
                .flex()
                .justify_center()
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.text_faint)
                        .child("还没有消息。"),
                )
                .into_any_element(),
        },
        // Split assistant message: each chunk is its own virtual row, so a
        // frame during streaming or scrolling lays out one chunk, not the
        // whole multi-kilobyte body.
        (ChatItemKind::Assistant(assistant), SlotPart::Lead) => {
            render_assistant_lead(assistant, view, theme, entity)
        }
        (ChatItemKind::Assistant(assistant), SlotPart::Chunk(chunk_ix)) => {
            render_assistant_chunk(assistant, view, chunk_ix, theme)
        }
        (ChatItemKind::Assistant(assistant), SlotPart::Tail) => {
            render_assistant_tail(assistant, view, theme, entity)
        }
        (ChatItemKind::Turn(turn), SlotPart::Lead) => render_turn_lead(turn, view, theme, entity),
        (ChatItemKind::Turn(turn), SlotPart::Chunk(chunk_ix)) => {
            render_assistant_chunk(turn_body(turn), view, chunk_ix, theme)
        }
        (ChatItemKind::Turn(turn), SlotPart::Tail) => render_turn_tail(turn, view, theme, entity),
        _ => div().into_any_element(),
    };

    let fresh = !matches!(item.kind, ChatItemKind::RunStatus(_) | ChatItemKind::Empty)
        && item.entered_at.elapsed() < ENTER_WINDOW;
    let last_part = view
        .slots
        .get(index + 1)
        .is_none_or(|next| next.item_ix != slot.item_ix);

    let wrapper = div()
        .w_full()
        .flex()
        .justify_center()
        .when(index == 0, |element| element.pt(px(28.0)))
        // Within one message the inner column used a 9px gap between
        // reasoning/markdown/tool rows; across messages the gap was 22px.
        .pb(px(if last_part { 22.0 } else { 9.0 }))
        .child(
            div()
                .w_full()
                .max_w(px(CONTENT_MAX_WIDTH))
                .min_w(px(0.0))
                .px(px(CONTENT_PADDING))
                .child(content),
        );

    if fresh {
        wrapper
            .with_animation(
                SharedString::from(format!("chat-enter-{}", item.id)),
                Animation::new(ENTER_ANIMATION).with_easing(|delta| 1.0 - (1.0 - delta).powi(3)),
                |element, delta| element.opacity(delta),
            )
            .into_any_element()
    } else {
        wrapper.into_any_element()
    }
}

fn render_user(theme: &Theme, text: &str) -> AnyElement {
    div()
        .w_full()
        .flex()
        .justify_end()
        .child(
            div()
                .max_w(px(600.0))
                .min_w(px(0.0))
                .rounded(px(18.0))
                .border_1()
                .border_color(theme.border)
                .bg(theme.user_bubble)
                .px(px(15.0))
                .py(px(10.0))
                .text_size(px(14.0))
                .line_height(px(22.0))
                .text_color(theme.text)
                .child(text.to_string()),
        )
        .into_any_element()
}

/// Borrowed text/reasoning for one assistant row: the live reveal slice when
/// the message is streaming, the finished snapshot otherwise. The `RefCell`
/// guards live as long as the struct; the slices are taken through accessor
/// methods so nothing copies per frame.
struct AssistantTexts<'a> {
    /// Live reveal state (buffers, revealed byte lengths, activity clock).
    stream: Option<StreamBorrow<'a>>,
    text: &'a str,
    reasoning: &'a str,
}

struct StreamBorrow<'a> {
    text: Ref<'a, String>,
    reasoning: Ref<'a, String>,
    shown: usize,
    shown_reasoning: usize,
    active: bool,
    elapsed: Duration,
}

impl<'a> AssistantTexts<'a> {
    /// The revealed (or finished) markdown source of this row.
    fn text(&'a self) -> &'a str {
        match &self.stream {
            Some(stream) => {
                let buffer = &stream.text[..stream.shown.min(stream.text.len())];
                &buffer[..buffer.len()]
            }
            None => self.text,
        }
    }

    /// The revealed (or finished) reasoning text of this row.
    fn reasoning(&'a self) -> &'a str {
        match &self.stream {
            Some(stream) => {
                let buffer = &stream.reasoning[..];
                &buffer[..stream.shown_reasoning.min(buffer.len())]
            }
            None => self.reasoning,
        }
    }

    fn active(&self) -> bool {
        self.stream.as_ref().is_some_and(|stream| stream.active)
    }

    fn elapsed(&self) -> Option<Duration> {
        self.stream.as_ref().map(|stream| stream.elapsed)
    }
}

fn assistant_texts<'a>(
    assistant: &'a AssistantItem,
    view: &'a ChatViewState,
) -> AssistantTexts<'a> {
    match view
        .stream
        .as_ref()
        .filter(|stream| stream.message_id == assistant.message.id)
    {
        Some(stream) => AssistantTexts {
            stream: Some(StreamBorrow {
                text: stream.text.borrow(),
                reasoning: stream.reasoning.borrow(),
                shown: stream.shown,
                shown_reasoning: stream.shown_reasoning,
                active: stream.active,
                elapsed: stream.elapsed,
            }),
            text: &assistant.text,
            reasoning: &assistant.reasoning,
        },
        None => AssistantTexts {
            stream: None,
            text: &assistant.text,
            reasoning: &assistant.reasoning,
        },
    }
}

impl TextChunk {
    /// Slice the chunk out of its source text (clamped for safety).
    fn slice<'a>(&self, text: &'a str) -> &'a str {
        &text[self.start..self.end.min(text.len())]
    }
}

/// The chunk table a slot renders against: the per-frame table for the
/// streaming message, the rebuild-time table for finished ones.
fn slot_chunks<'a>(
    assistant: &'a AssistantItem,
    view: &'a ChatViewState,
) -> Option<&'a [TextChunk]> {
    if view
        .stream
        .as_ref()
        .is_some_and(|stream| stream.message_id == assistant.message.id)
    {
        view.stream_chunks.as_deref().map(Vec::as_slice)
    } else {
        assistant.chunks.as_deref().map(Vec::as_slice)
    }
}

fn slot_chunk<'a>(
    assistant: &'a AssistantItem,
    view: &'a ChatViewState,
    index: usize,
) -> Option<&'a TextChunk> {
    slot_chunks(assistant, view).and_then(|chunks| chunks.get(index))
}

fn assistant_column(message_group: &SharedString) -> Div {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(9.0))
        .group(message_group.clone())
}

/// Markdown source with the streaming caret appended (flows inline with the
/// last sentence, exactly like the old whole-document streaming path).
fn with_stream_caret(chunk_text: &str) -> String {
    let mut streaming = String::with_capacity(chunk_text.len() + 2);
    streaming.push_str(chunk_text);
    if !streaming.is_empty() {
        streaming.push(' ');
    }
    streaming.push('▍');
    streaming
}

/// Short assistant message (or a still-tiny streaming one): everything in a
/// single virtual row.
fn render_assistant(
    assistant: &AssistantItem,
    view: &ChatViewState,
    theme: &Theme,
    entity: &WeakEntity<AppView>,
) -> AnyElement {
    let texts = assistant_texts(assistant, view);
    let message_group = SharedString::from(format!("assistant-{}", assistant.message.id));
    let mut column = assistant_column(&message_group);

    if !texts.reasoning().is_empty() {
        column = column.child(render_reasoning(
            &assistant.message.id,
            texts.reasoning(),
            assistant.reasoning_duration,
            texts.elapsed(),
            texts.active(),
            view.expanded_reasoning.contains(&assistant.message.id),
            view,
            theme,
            entity,
        ));
    }

    let has_text = !texts.text().trim().is_empty();
    if has_text || texts.active() {
        let markdown = if texts.active() {
            let streaming = with_stream_caret(texts.text());
            render_markdown_chunk(theme, &streaming, markdown_source_key(&streaming))
        } else {
            render_markdown_keyed(theme, &assistant.text, assistant.text_key)
        };
        column = column.child(markdown);
    }

    let mut column = append_tool_rows(column, assistant, view, theme, entity);
    if !texts.active() && has_text {
        column = column.child(render_copy_footer(assistant, &message_group, theme));
    }
    column.into_any_element()
}

/// First row of a split assistant body: reasoning plus the first chunk.
fn render_assistant_lead(
    assistant: &AssistantItem,
    view: &ChatViewState,
    theme: &Theme,
    entity: &WeakEntity<AppView>,
) -> AnyElement {
    let texts = assistant_texts(assistant, view);
    let message_group = SharedString::from(format!("assistant-{}", assistant.message.id));
    let mut column = assistant_column(&message_group);

    if !texts.reasoning().is_empty() {
        column = column.child(render_reasoning(
            &assistant.message.id,
            texts.reasoning(),
            assistant.reasoning_duration,
            texts.elapsed(),
            texts.active(),
            view.expanded_reasoning.contains(&assistant.message.id),
            view,
            theme,
            entity,
        ));
    }

    if let Some(chunk) = slot_chunk(assistant, view, 0) {
        column = column.child(render_markdown_chunk(
            theme,
            chunk.slice(texts.text()),
            chunk.key,
        ));
    }
    column.into_any_element()
}

/// Middle row of a split assistant body: one cached markdown chunk.
fn render_assistant_chunk(
    assistant: &AssistantItem,
    view: &ChatViewState,
    chunk_ix: usize,
    theme: &Theme,
) -> AnyElement {
    let texts = assistant_texts(assistant, view);
    match slot_chunk(assistant, view, chunk_ix) {
        Some(chunk) => render_markdown_chunk(theme, chunk.slice(texts.text()), chunk.key),
        None => div().into_any_element(),
    }
}

/// Last row of a split assistant body: final chunk (+ streaming caret), tool
/// rows, and the hover copy footer.
fn render_assistant_tail(
    assistant: &AssistantItem,
    view: &ChatViewState,
    theme: &Theme,
    entity: &WeakEntity<AppView>,
) -> AnyElement {
    let texts = assistant_texts(assistant, view);
    let message_group = SharedString::from(format!("assistant-{}", assistant.message.id));
    let mut column = assistant_column(&message_group);

    let last = slot_chunks(assistant, view).and_then(|chunks| chunks.last());
    if let Some(chunk) = last {
        let chunk_text = chunk.slice(texts.text());
        let markdown = if texts.active() {
            let streaming = with_stream_caret(chunk_text);
            render_markdown_chunk(theme, &streaming, markdown_source_key(&streaming))
        } else {
            render_markdown_chunk(theme, chunk_text, chunk.key)
        };
        column = column.child(markdown);
    }

    let mut column = append_tool_rows(column, assistant, view, theme, entity);
    if !texts.active() && !texts.text().trim().is_empty() {
        column = column.child(render_copy_footer(assistant, &message_group, theme));
    }
    column.into_any_element()
}

// --- turn blocks -----------------------------------------------------------

/// The turn's body member: the last assistant message, whose text renders as
/// the answer below the activity stack.
fn turn_body(turn: &TurnItem) -> &AssistantItem {
    turn.members.last().expect("turn has at least one member")
}

fn turn_message_group(turn: &TurnItem) -> SharedString {
    SharedString::from(format!("turn-{}", turn.id))
}

/// Whether this turn contains the live streaming message.
fn turn_is_live(turn: &TurnItem, view: &ChatViewState) -> bool {
    view.stream.as_ref().is_some_and(|stream| {
        turn.members
            .iter()
            .any(|member| member.message.id == stream.message_id)
    })
}

/// Fold state of the turn activity: a manual collapse wins, then a manual
/// expand; otherwise a live turn stays expanded and finished ones fold into
/// their summary line.
fn turn_expanded(turn: &TurnItem, view: &ChatViewState) -> bool {
    if view.collapsed_turns.contains(&turn.id) {
        return false;
    }
    view.expanded_turns.contains(&turn.id) || (view.running && turn_is_live(turn, view))
}

/// Distinct tool-kind labels across the turn, for the summary line.
fn turn_kind_summary(turn: &TurnItem) -> String {
    let mut kinds: Vec<&'static str> = Vec::new();
    for member in &turn.members {
        for block in &member.message.blocks {
            if let ContentBlock::ToolCall { call } = block {
                let kind = tool_kind_label(&call.name);
                if !kinds.contains(&kind) {
                    kinds.push(kind);
                }
            }
        }
    }
    kinds.join(" · ")
}

/// Aggregate summary header of a turn block: status icon over every tool
/// result, counts of thinking phases / tool calls / subagents, and the
/// chevron. Clicking it records the explicit fold choice.
fn render_turn_header(
    turn: &TurnItem,
    expanded: bool,
    view: &ChatViewState,
    theme: &Theme,
    entity: &WeakEntity<AppView>,
) -> AnyElement {
    let live = view.running && turn_is_live(turn, view);
    let unresolved = turn.members.iter().any(|member| {
        member.message.blocks.iter().any(|block| match block {
            ContentBlock::ToolCall { call } => !view.results.contains_key(&call.id),
            _ => false,
        })
    });
    let status_icon: AnyElement = if turn.any_error {
        svg()
            .path(icons::ALERT)
            .size(px(12.0))
            .text_color(theme.danger)
            .into_any_element()
    } else if live || unresolved {
        Spinner::new(theme.text_faint, 11.0).into_any_element()
    } else {
        svg()
            .path(icons::CHECK)
            .size(px(12.0))
            .text_color(theme.text_faint)
            .into_any_element()
    };

    let mut label = String::new();
    if turn.thinking_count > 0 {
        label.push_str(&format!("思考 {} 次", turn.thinking_count));
    }
    if turn.tool_calls > 0 {
        if !label.is_empty() {
            label.push_str(" · ");
        }
        label.push_str(&format!("{} 个工具调用", turn.tool_calls));
    }
    if turn.agent_calls > 0 {
        if !label.is_empty() {
            label.push_str(" · ");
        }
        label.push_str(&format!("{} 个子代理", turn.agent_calls));
    }
    if label.is_empty() {
        label.push_str("执行中");
    }

    let turn_id = turn.id.clone();
    let toggle_entity = entity.clone();
    let summary = turn_kind_summary(turn);

    div()
        .w_full()
        .child(
            div()
                .id(SharedString::from(format!("turn-head-{}", turn.id)))
                .flex()
                .items_center()
                .gap(px(7.0))
                .h(px(26.0))
                .px(px(6.0))
                .mx(px(-6.0))
                .rounded(px(6.0))
                .cursor_pointer()
                .hover(|style| style.bg(theme.hover))
                .on_click(move |_, _, cx| {
                    let turn_id = turn_id.clone();
                    let _ = toggle_entity.update(cx, |this, cx| {
                        this.toggle_turn(&turn_id, expanded);
                        cx.notify();
                    });
                })
                .child(status_icon)
                .child(
                    svg()
                        .path(icons::WRENCH)
                        .size(px(12.0))
                        .text_color(theme.text_faint)
                        .flex_none(),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(px(11.5))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(label),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(10.5))
                        .text_color(theme.text_faint)
                        .child(summary),
                )
                .when(turn.total_ms > 0, |element| {
                    element.child(
                        div()
                            .flex_none()
                            .text_size(px(10.0))
                            .text_color(theme.text_faint)
                            .opacity(0.8)
                            .child(format_duration_ms(turn.total_ms)),
                    )
                })
                .child(disclosure_chevron(
                    SharedString::from(format!("turn-chevron-{}", turn.id)),
                    expanded,
                    theme.text_faint.opacity(0.7),
                    10.0,
                )),
        )
        .into_any_element()
}

/// One intermediate member of an expanded turn: reasoning row, dimmed
/// narration text, and its tool rows, in chronological order.
fn render_turn_member(
    column: Div,
    member: &AssistantItem,
    view: &ChatViewState,
    theme: &Theme,
    entity: &WeakEntity<AppView>,
) -> Div {
    let mut column = column;
    if !member.reasoning.is_empty() {
        column = column.child(render_reasoning(
            &member.message.id,
            &member.reasoning,
            member.reasoning_duration,
            None,
            false,
            view.expanded_reasoning.contains(&member.message.id),
            view,
            theme,
            entity,
        ));
    }
    if !member.text.trim().is_empty() {
        column = column.child(div().opacity(0.72).child(render_markdown_keyed(
            theme,
            &member.text,
            member.text_key,
        )));
    }
    append_tool_rows(column, member, view, theme, entity)
}

/// The body member's reasoning row; only mounted while the activity is
/// expanded so a folded turn costs one header row.
fn render_turn_body_reasoning(
    column: Div,
    body: &AssistantItem,
    view: &ChatViewState,
    theme: &Theme,
    entity: &WeakEntity<AppView>,
) -> Div {
    let texts = assistant_texts(body, view);
    if texts.reasoning().is_empty() {
        return column;
    }
    column.child(render_reasoning(
        &body.message.id,
        texts.reasoning(),
        body.reasoning_duration,
        texts.elapsed(),
        texts.active(),
        view.expanded_reasoning.contains(&body.message.id),
        view,
        theme,
        entity,
    ))
}

/// Short turn (or a still-tiny streaming body): header, activity, answer
/// body, tool rows and footer in one virtual row.
fn render_turn(
    turn: &TurnItem,
    view: &ChatViewState,
    theme: &Theme,
    entity: &WeakEntity<AppView>,
) -> AnyElement {
    let expanded = turn_expanded(turn, view);
    let body = turn_body(turn);
    let texts = assistant_texts(body, view);
    let message_group = turn_message_group(turn);
    let mut column = assistant_column(&message_group);

    column = column.child(render_turn_header(turn, expanded, view, theme, entity));
    if expanded {
        for member in &turn.members[..turn.members.len() - 1] {
            column = render_turn_member(column, member, view, theme, entity);
        }
        column = render_turn_body_reasoning(column, body, view, theme, entity);
    }

    let has_text = !texts.text().trim().is_empty();
    if has_text || texts.active() {
        let markdown = if texts.active() {
            let streaming = with_stream_caret(texts.text());
            render_markdown_chunk(theme, &streaming, markdown_source_key(&streaming))
        } else {
            render_markdown_keyed(theme, &body.text, body.text_key)
        };
        column = column.child(markdown);
    }

    if expanded {
        column = append_tool_rows(column, body, view, theme, entity);
    }
    if !texts.active() && has_text {
        column = column.child(render_copy_footer(body, &message_group, theme));
    }
    column.into_any_element()
}

/// First row of a split turn body: header, activity, body reasoning, then the
/// first markdown chunk of the answer.
fn render_turn_lead(
    turn: &TurnItem,
    view: &ChatViewState,
    theme: &Theme,
    entity: &WeakEntity<AppView>,
) -> AnyElement {
    let expanded = turn_expanded(turn, view);
    let body = turn_body(turn);
    let texts = assistant_texts(body, view);
    let message_group = turn_message_group(turn);
    let mut column = assistant_column(&message_group);

    column = column.child(render_turn_header(turn, expanded, view, theme, entity));
    if expanded {
        for member in &turn.members[..turn.members.len() - 1] {
            column = render_turn_member(column, member, view, theme, entity);
        }
        column = render_turn_body_reasoning(column, body, view, theme, entity);
    }
    if let Some(chunk) = slot_chunk(body, view, 0) {
        column = column.child(render_markdown_chunk(
            theme,
            chunk.slice(texts.text()),
            chunk.key,
        ));
    }
    column.into_any_element()
}

/// Last row of a split turn body: final chunk (+ streaming caret), body tool
/// rows while expanded, and the hover copy footer.
fn render_turn_tail(
    turn: &TurnItem,
    view: &ChatViewState,
    theme: &Theme,
    entity: &WeakEntity<AppView>,
) -> AnyElement {
    let expanded = turn_expanded(turn, view);
    let body = turn_body(turn);
    let texts = assistant_texts(body, view);
    let message_group = turn_message_group(turn);
    let mut column = assistant_column(&message_group);

    let last = slot_chunks(body, view).and_then(|chunks| chunks.last());
    if let Some(chunk) = last {
        let chunk_text = chunk.slice(texts.text());
        let markdown = if texts.active() {
            let streaming = with_stream_caret(chunk_text);
            render_markdown_chunk(theme, &streaming, markdown_source_key(&streaming))
        } else {
            render_markdown_chunk(theme, chunk_text, chunk.key)
        };
        column = column.child(markdown);
    }

    if expanded {
        column = append_tool_rows(column, body, view, theme, entity);
    }
    if !texts.active() && !texts.text().trim().is_empty() {
        column = column.child(render_copy_footer(body, &message_group, theme));
    }
    column.into_any_element()
}

/// Tool-call rows (agent cards, groups, singles) shared by every assistant
/// row variant.
fn append_tool_rows(
    column: Div,
    assistant: &AssistantItem,
    view: &ChatViewState,
    theme: &Theme,
    entity: &WeakEntity<AppView>,
) -> Div {
    let message = &assistant.message;
    let calls: Vec<&ToolCall> = message
        .blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolCall { call } => Some(call),
            _ => None,
        })
        .collect();
    let mut column = column;
    let mut cursor = 0;
    while cursor < calls.len() {
        let call = calls[cursor];
        if call.name == "agent" {
            let result = view.results.get(&call.id).map(Rc::as_ref);
            let card = view.agents.get(&call.id);
            column = column.child(render_agent_row(call, result, card, theme, entity));
            cursor += 1;
            continue;
        }

        let start = cursor;
        while cursor < calls.len() && calls[cursor].name != "agent" {
            cursor += 1;
        }
        let run = &calls[start..cursor];
        if run.len() >= 3 {
            let group_id = format!("group:{}:{}", message.id, start);
            let expanded = view.expanded_tools.contains(&group_id);
            column = column.child(render_tool_group(
                &group_id, run, view, expanded, theme, entity,
            ));
        } else {
            for call in run {
                let result = view.results.get(&call.id).map(Rc::as_ref);
                let expanded = view.expanded_tools.contains(&call.id);
                column = column.child(render_tool_row(call, result, view, expanded, theme, entity));
            }
        }
    }
    column
}

/// Hover-revealed copy button + character count, shared by assistant rows.
fn render_copy_footer(
    assistant: &AssistantItem,
    message_group: &SharedString,
    theme: &Theme,
) -> AnyElement {
    let message = &assistant.message;
    let copy_id = SharedString::from(format!("copy-{}", message.id));
    let copy_text = assistant.text.clone();
    let chars = assistant.char_count;
    div()
        .flex()
        .items_center()
        .gap_2()
        .mt_1()
        .opacity(0.0)
        .group_hover(message_group.clone(), |style| style.opacity(1.0))
        .child(
            IconButton::new(copy_id, icons::COPY, theme)
                .size(22.0, 12.0)
                .on_click(move |_, _, cx| {
                    if !copy_text.is_empty() {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy_text.to_string()));
                    }
                }),
        )
        .child(
            div()
                .text_size(px(10.5))
                .text_color(theme.text_faint)
                .child(format!("{chars} 字")),
        )
        .when(message.interrupted, |element| {
            element.child(badge(theme, "已中断", BadgeTone::Warning))
        })
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn render_reasoning(
    message_id: &str,
    reasoning: &str,
    duration: Option<Duration>,
    stream_elapsed: Option<Duration>,
    active: bool,
    expanded: bool,
    view: &ChatViewState,
    theme: &Theme,
    entity: &WeakEntity<AppView>,
) -> AnyElement {
    let live = active;
    let label = if live {
        let elapsed = stream_elapsed.unwrap_or_default();
        format!("正在思考 · {}", format_elapsed(elapsed))
    } else if let Some(duration) = duration {
        if duration < Duration::from_secs(1) {
            "思考了不到 1 秒".to_string()
        } else {
            format!("思考了 {}", format_elapsed(duration))
        }
    } else {
        "思考过程".to_string()
    };

    let toggle_id = message_id.to_string();
    let toggle_entity = entity.clone();
    let row = div()
        .id(SharedString::from(format!("reasoning-{}", message_id)))
        .flex()
        .items_center()
        .gap(px(7.0))
        .h(px(26.0))
        .px(px(6.0))
        .mx(px(-6.0))
        .rounded(px(6.0))
        .cursor_pointer()
        .hover(|style| style.bg(theme.hover))
        .on_click(move |_, _, cx| {
            let _ = toggle_entity.update(cx, |this, cx| {
                this.toggle_reasoning(&toggle_id);
                cx.notify();
            });
        })
        .child(disclosure_chevron(
            SharedString::from(format!("reasoning-chevron-{message_id}")),
            expanded,
            theme.text_faint,
            11.0,
        ))
        .child(
            svg()
                .path(icons::SPARKLES)
                .size(px(12.0))
                .text_color(if live { theme.accent } else { theme.text_faint }),
        )
        .child(
            div()
                .flex_none()
                .text_size(px(11.5))
                .text_color(if live {
                    theme.text_muted
                } else {
                    theme.text_faint
                })
                .child(label),
        )
        .when(live, |element| {
            element.child(Spinner::new(theme.text_faint, 11.0))
        })
        .when(live && !expanded, |element| {
            element.child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(10.5))
                    .text_color(theme.text_faint)
                    .opacity(0.8)
                    .child(latest_reasoning_line(reasoning)),
            )
        });

    // The body is only built when expanded: a collapsed reasoning block used
    // to clone the full reasoning text on every frame.
    let body = expanded.then(|| {
        let mut body_text = String::with_capacity(reasoning.len() + 2);
        body_text.push_str(reasoning);
        if live {
            body_text.push_str(" ▍");
        }
        let scroll_handle = view
            .reasoning_scrolls
            .borrow_mut()
            .entry(message_id.to_string())
            .or_insert_with(ScrollHandle::new)
            .clone();
        div()
            .id(SharedString::from(format!("reasoning-body-{}", message_id)))
            .ml(px(17.0))
            .pl(px(11.0))
            .border_l_2()
            .border_color(theme.border_strong)
            .max_h(px(280.0))
            .overflow_y_scroll()
            .track_scroll(&scroll_handle)
            .child(
                div()
                    .py(px(2.0))
                    .text_size(px(12.5))
                    .line_height(px(20.0))
                    .text_color(theme.text_muted)
                    .child(body_text),
            )
            // Claim wheel events over the expanded body in the capture phase:
            // the body scrolls by its own handle and the chat glide stays out.
            .child(wheel_claim_overlay(scroll_handle, entity.clone()))
    });

    div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(5.0))
        .child(row)
        .when_some(body, |element, body| element.child(body))
        .into_any_element()
}

fn render_agent_row(
    call: &ToolCall,
    result: Option<&ToolResult>,
    card: Option<&AgentCard>,
    theme: &Theme,
    entity: &WeakEntity<AppView>,
) -> AnyElement {
    let profile = card
        .map(|card| card.profile.clone())
        .or_else(|| {
            call.arguments
                .get("subagent_type")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| "general-purpose".to_string());
    let description = card
        .map(|card| card.description.clone())
        .or_else(|| {
            call.arguments
                .get("description")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| "子代理任务".to_string());
    let color = agent_color(
        theme,
        card.map(|card| card.color).unwrap_or(SubagentColor::Blue),
    );
    let soft = with_alpha(color, if theme.is_dark() { 0.20 } else { 0.10 });

    let status = card.map(|card| card.status);
    let waiting = card.is_some_and(|card| card.waiting_approval);
    let status_icon: AnyElement = if waiting {
        svg()
            .path(icons::ALERT)
            .size(px(13.0))
            .text_color(theme.warning)
            .into_any_element()
    } else {
        match status {
            Some(SubagentStatus::Completed) => svg()
                .path(icons::CHECK)
                .size(px(13.0))
                .text_color(theme.success)
                .into_any_element(),
            Some(SubagentStatus::Failed) => svg()
                .path(icons::ALERT)
                .size(px(13.0))
                .text_color(theme.danger)
                .into_any_element(),
            Some(SubagentStatus::Cancelled) => svg()
                .path(icons::STOP)
                .size(px(11.0))
                .text_color(theme.text_faint)
                .into_any_element(),
            _ => Spinner::new(color, 12.0).into_any_element(),
        }
    };

    let status_label = if waiting {
        "等待批准".to_string()
    } else if let Some(status) = status {
        status.label().to_string()
    } else if let Some(result) = result {
        if result.is_error {
            "失败".to_string()
        } else {
            "已完成".to_string()
        }
    } else if result.is_none() && card.is_none() {
        "准备中".to_string()
    } else {
        "运行中".to_string()
    };
    let status_color = if waiting {
        theme.warning
    } else {
        match status {
            Some(SubagentStatus::Completed) => theme.success,
            Some(SubagentStatus::Failed) => theme.danger,
            _ => theme.text_faint,
        }
    };

    let elapsed = card
        .map(|card| {
            card.finished_at
                .unwrap_or_else(Instant::now)
                .saturating_duration_since(card.started_at)
        })
        .unwrap_or_default();
    let tool_uses = card.map(|card| card.tool_uses).unwrap_or(0);
    let background = card.is_some_and(|card| card.background);
    let open_agent = card.map(|card| card.agent_id.clone());
    let call_id = call.id.clone();
    let toggle_entity = entity.clone();

    let mut row = div()
        .id(SharedString::from(format!("agent-{}", call.id)))
        .w_full()
        .rounded(px(10.0))
        .border_1()
        .border_color(with_alpha(color, if theme.is_dark() { 0.35 } else { 0.22 }))
        .bg(soft)
        .px(px(9.0))
        .py(px(8.0))
        .flex()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .hover(|style| style.bg(with_alpha(color, if theme.is_dark() { 0.28 } else { 0.15 })))
        .on_click(move |_, _, cx| {
            if let Some(agent_id) = &open_agent {
                let agent_id = agent_id.clone();
                let _ = toggle_entity.update(cx, |this, cx| {
                    this.open_agent_tab(&agent_id, cx);
                });
            } else {
                let call_id = call_id.clone();
                let _ = toggle_entity.update(cx, |this, cx| {
                    this.toggle_tool(&call_id);
                    cx.notify();
                });
            }
        })
        .child(
            div()
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .size(px(22.0))
                .rounded(px(7.0))
                .bg(with_alpha(color, if theme.is_dark() { 0.30 } else { 0.16 }))
                .child(svg().path(icons::SPARKLES).size(px(12.0)).text_color(color)),
        )
        .child(status_icon)
        .child(
            div()
                .flex_none()
                .text_size(px(12.0))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(color)
                .child(profile),
        )
        .child(
            div()
                .flex_none()
                .h(px(17.0))
                .px(px(5.0))
                .rounded(px(5.0))
                .bg(with_alpha(color, if theme.is_dark() { 0.24 } else { 0.12 }))
                .flex()
                .items_center()
                .text_size(px(9.5))
                .text_color(color)
                .child("子代理"),
        )
        .when(background, |element| {
            element.child(
                div()
                    .flex_none()
                    .h(px(17.0))
                    .px(px(5.0))
                    .rounded(px(5.0))
                    .bg(theme.surface_alt)
                    .flex()
                    .items_center()
                    .text_size(px(9.5))
                    .text_color(theme.text_faint)
                    .child("后台"),
            )
        })
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(12.0))
                .text_color(theme.text_muted)
                .child(description),
        )
        .when(tool_uses > 0, |element| {
            element.child(
                div()
                    .flex_none()
                    .text_size(px(10.0))
                    .text_color(theme.text_faint)
                    .child(format!("{tool_uses} 次工具")),
            )
        })
        .child(
            div()
                .flex_none()
                .text_size(px(10.0))
                .text_color(theme.text_faint)
                .child(format_elapsed(elapsed)),
        )
        .child(
            div()
                .flex_none()
                .text_size(px(10.5))
                .text_color(status_color)
                .child(status_label),
        );

    row = row.child(
        svg()
            .path(icons::CHEVRON_RIGHT)
            .size(px(12.0))
            .text_color(theme.text_faint)
            .flex_none(),
    );
    row.into_any_element()
}

fn render_tool_row(
    call: &ToolCall,
    result: Option<&ToolResult>,
    view: &ChatViewState,
    expanded: bool,
    theme: &Theme,
    entity: &WeakEntity<AppView>,
) -> AnyElement {
    let running = view.running;
    let (status_label, status_color) = match result {
        Some(result) if result.is_error => ("失败", theme.danger),
        Some(_) => ("完成", theme.text_faint),
        None if running => ("运行中", theme.text_faint),
        None => ("未执行", theme.text_faint),
    };
    let detail = tool_detail(call);
    let call_id = call.id.clone();
    let toggle_entity = entity.clone();
    // Expanded-only work: JSON pretty-printing and cloning the (possibly
    // megabyte-sized) output used to run on every frame even when collapsed.
    let args = expanded.then(|| pretty_truncated(&call.arguments, 6000));
    let output = if expanded {
        result.map(|result| (result.content.clone(), result.is_error, result.duration_ms))
    } else {
        None
    };
    let meta = result.map(|result| (result.is_error, result.duration_ms));

    let status_icon: AnyElement = match result {
        Some(result) if result.is_error => svg()
            .path(icons::ALERT)
            .size(px(12.0))
            .text_color(theme.danger)
            .into_any_element(),
        Some(_) => svg()
            .path(icons::CHECK)
            .size(px(12.0))
            .text_color(theme.text_faint)
            .into_any_element(),
        None if running => Spinner::new(theme.text_faint, 11.0).into_any_element(),
        None => svg()
            .path(icons::CLOCK)
            .size(px(12.0))
            .text_color(theme.text_faint)
            .into_any_element(),
    };

    div()
        .w_full()
        .rounded(px(8.0))
        .child(
            div()
                .id(SharedString::from(format!("tool-{}", call.id)))
                .flex()
                .items_center()
                .gap(px(7.0))
                .h(px(26.0))
                .px(px(6.0))
                .mx(px(-6.0))
                .rounded(px(6.0))
                .cursor_pointer()
                .hover(|style| style.bg(theme.hover))
                .on_click(move |_, _, cx| {
                    let _ = toggle_entity.update(cx, |this, cx| {
                        this.toggle_tool(&call_id);
                        cx.notify();
                    });
                })
                .child(status_icon)
                .child(
                    svg()
                        .path(tool_icon(&call.name))
                        .size(px(12.0))
                        .text_color(theme.text_faint)
                        .flex_none(),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(px(11.5))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(if running && result.is_none() {
                            theme.text
                        } else {
                            theme.text_muted
                        })
                        .child(tool_kind_label(&call.name)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(11.0))
                        .text_color(theme.text_faint)
                        .child(detail),
                )
                .when_some(
                    meta.and_then(|(_, duration)| duration),
                    |element, duration| {
                        element.child(
                            div()
                                .flex_none()
                                .text_size(px(10.0))
                                .text_color(theme.text_faint)
                                .opacity(0.8)
                                .child(format_duration_ms(duration)),
                        )
                    },
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(px(10.0))
                        .text_color(status_color)
                        .child(status_label),
                )
                .child(disclosure_chevron(
                    SharedString::from(format!("tool-chevron-{}", call.id)),
                    expanded,
                    theme.text_faint.opacity(0.7),
                    10.0,
                )),
        )
        .when(expanded, |element| {
            element.child(
                div()
                    .ml(px(8.0))
                    .pl(px(9.0))
                    .border_l_1()
                    .border_color(theme.border)
                    .pt(px(4.0))
                    .pb(px(8.0))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .when_some(args, |element, args| {
                        element.child(code_panel(
                            theme,
                            "参数",
                            &args,
                            format!("{}-args", call.id),
                            view,
                            entity,
                        ))
                    })
                    .when_some(output, |element, (content, is_error, _)| {
                        element.child(code_panel(
                            theme,
                            if is_error { "错误" } else { "输出" },
                            &truncate_text(&content, 12_000),
                            format!("{}-out", call.id),
                            view,
                            entity,
                        ))
                    }),
            )
        })
        .into_any_element()
}

/// Consecutive tool calls collapse into one summary row. Expanding it reveals
/// the individual rows on a vertical rail, which keeps a long agent turn
/// readable without hiding evidence.
fn render_tool_group(
    group_id: &str,
    calls: &[&ToolCall],
    view: &ChatViewState,
    expanded: bool,
    theme: &Theme,
    entity: &WeakEntity<AppView>,
) -> AnyElement {
    let any_running = calls
        .iter()
        .any(|call| view.running && !view.results.contains_key(&call.id));
    let any_error = calls.iter().any(|call| {
        view.results
            .get(&call.id)
            .is_some_and(|result| result.is_error)
    });
    let total_ms: u64 = calls
        .iter()
        .filter_map(|call| view.results.get(&call.id))
        .filter_map(|result| result.duration_ms)
        .sum();

    let mut kinds: Vec<&'static str> = Vec::new();
    for call in calls {
        let kind = tool_kind_label(&call.name);
        if !kinds.contains(&kind) {
            kinds.push(kind);
        }
    }
    let summary = kinds.join(" · ");

    let status_icon: AnyElement = if any_error {
        svg()
            .path(icons::ALERT)
            .size(px(12.0))
            .text_color(theme.danger)
            .into_any_element()
    } else if any_running {
        Spinner::new(theme.text_faint, 11.0).into_any_element()
    } else {
        svg()
            .path(icons::CHECK)
            .size(px(12.0))
            .text_color(theme.text_faint)
            .into_any_element()
    };

    let group_key = group_id.to_string();
    let toggle_entity = entity.clone();
    // Rows are only mounted when the group is expanded; skip building them
    // entirely while collapsed so a collapsed group costs one cheap row.
    let rail: Vec<AnyElement> = if expanded {
        calls
            .iter()
            .map(|call| {
                let result = view.results.get(&call.id).map(Rc::as_ref);
                let expanded = view.expanded_tools.contains(&call.id);
                render_tool_row(call, result, view, expanded, theme, entity)
            })
            .collect()
    } else {
        Vec::new()
    };

    div()
        .w_full()
        .child(
            div()
                .id(SharedString::from(format!("tool-group-{group_id}")))
                .flex()
                .items_center()
                .gap(px(7.0))
                .h(px(26.0))
                .px(px(6.0))
                .mx(px(-6.0))
                .rounded(px(6.0))
                .cursor_pointer()
                .hover(|style| style.bg(theme.hover))
                .on_click(move |_, _, cx| {
                    let group_key = group_key.clone();
                    let _ = toggle_entity.update(cx, |this, cx| {
                        this.toggle_tool(&group_key);
                        cx.notify();
                    });
                })
                .child(status_icon)
                .child(
                    svg()
                        .path(icons::WRENCH)
                        .size(px(12.0))
                        .text_color(theme.text_faint)
                        .flex_none(),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(px(11.5))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(format!("{} 个工具调用", calls.len())),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(10.5))
                        .text_color(theme.text_faint)
                        .child(summary),
                )
                .when(total_ms > 0, |element| {
                    element.child(
                        div()
                            .flex_none()
                            .text_size(px(10.0))
                            .text_color(theme.text_faint)
                            .opacity(0.8)
                            .child(format_duration_ms(total_ms)),
                    )
                })
                .child(disclosure_chevron(
                    SharedString::from(format!("tool-group-chevron-{group_id}")),
                    expanded,
                    theme.text_faint.opacity(0.7),
                    10.0,
                )),
        )
        .when(expanded, |element| {
            element.child(
                div()
                    .ml(px(8.0))
                    .pl(px(9.0))
                    .border_l_1()
                    .border_color(theme.border)
                    .pt(px(2.0))
                    .pb(px(4.0))
                    .flex()
                    .flex_col()
                    .gap(px(1.0))
                    .children(rail),
            )
        })
        .into_any_element()
}

fn render_run_status(
    status: &RunStatusItem,
    streaming_text: bool,
    theme: &Theme,
    entity: &WeakEntity<AppView>,
) -> AnyElement {
    let elapsed = status
        .started_at
        .map(|started| started.elapsed())
        .unwrap_or_default();
    let activity = if streaming_text && status.activity == "正在思考…" {
        "正在回复…"
    } else {
        status.activity.as_str()
    };
    let stop_entity = entity.clone();

    div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(3.0))
        .pt(px(2.0))
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_none()
                        .size(px(7.0))
                        .rounded(px(999.0))
                        .bg(theme.accent)
                        .with_animation(
                            "effibuddy-run-pulse",
                            Animation::new(Duration::from_millis(1100))
                                .repeat()
                                .with_easing(pulsating_between(0.25, 1.0)),
                            |element, delta| element.opacity(delta),
                        ),
                )
                .child(
                    div()
                        .text_size(px(12.5))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(format!("工作中 · {}", format_elapsed(elapsed))),
                )
                .child(
                    div()
                        .text_size(px(10.5))
                        .text_color(theme.text_faint)
                        .child(format!("第 {} 步", status.step)),
                )
                .child(div().flex_1())
                .child(
                    div()
                        .id("run-stop")
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(24.0))
                        .rounded(px(999.0))
                        .bg(theme.surface_alt)
                        .cursor_pointer()
                        .hover(|style| style.bg(theme.active))
                        .on_click(move |_, _, cx| {
                            let _ = stop_entity.update(cx, |this, cx| this.interrupt(cx));
                        })
                        .child(
                            svg()
                                .path(icons::STOP)
                                .size(px(10.0))
                                .text_color(theme.text),
                        ),
                ),
        )
        .child(
            div()
                .pl(px(15.0))
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(theme.text_muted)
                        .child(activity.to_string()),
                )
                .when_some(status.detail.clone(), |element, detail| {
                    element.child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(11.0))
                            .font_family(theme.mono_font())
                            .text_color(theme.text_faint)
                            .child(detail),
                    )
                }),
        )
        .into_any_element()
}

fn render_task_notification(theme: &Theme, summary: &str) -> AnyElement {
    div()
        .w_full()
        .flex()
        .justify_start()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .rounded(px(9.0))
                .border_1()
                .border_color(theme.border)
                .bg(theme.surface)
                .px(px(10.0))
                .py(px(7.0))
                .child(
                    svg()
                        .path(icons::SPARKLES)
                        .size(px(13.0))
                        .text_color(theme.text_faint),
                )
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(theme.text_muted)
                        .child("后台子代理已返回结果"),
                )
                .child(
                    div()
                        .max_w(px(320.0))
                        .truncate()
                        .text_size(px(11.0))
                        .text_color(theme.text_faint)
                        .child(summary.to_string()),
                ),
        )
        .into_any_element()
}

fn render_error(theme: &Theme, text: &str) -> AnyElement {
    div()
        .w_full()
        .rounded(px(10.0))
        .border_1()
        .border_color(theme.danger)
        .bg(theme.danger_soft)
        .p_3()
        .flex()
        .items_start()
        .gap_2()
        .child(
            svg()
                .path(icons::ALERT)
                .size(px(15.0))
                .text_color(theme.danger),
        )
        .child(
            div()
                .flex_1()
                .text_size(px(12.5))
                .text_color(theme.text)
                .child(text.to_string()),
        )
        .into_any_element()
}

pub(crate) fn render_scroll_to_bottom(
    theme: &Theme,
    has_new_content: bool,
    entity: WeakEntity<AppView>,
) -> AnyElement {
    div()
        .id("chat-scroll-to-bottom")
        .absolute()
        .right(px(26.0))
        .bottom(px(18.0))
        .flex()
        .items_center()
        .justify_center()
        .size(px(34.0))
        .rounded(px(999.0))
        .border_1()
        .border_color(theme.border)
        .bg(theme.surface)
        .shadow(shadow_float(theme))
        .cursor_pointer()
        .hover(|style| style.bg(theme.surface_alt))
        .on_click(move |_, _, cx| {
            let _ = entity.update(cx, |this, cx| {
                this.scroll_chat_to_bottom();
                cx.notify();
            });
        })
        .child(
            svg()
                .path(icons::CHEVRON_DOWN)
                .size(px(14.0))
                .text_color(theme.text_muted),
        )
        .when(has_new_content, |element| {
            element.child(
                div()
                    .absolute()
                    .top(px(3.0))
                    .right(px(3.0))
                    .size(px(7.0))
                    .rounded(px(999.0))
                    .bg(theme.accent),
            )
        })
        .into_any_element()
}

/// Wheel-event routing, part 1: nested scrollable regions. This transparent
/// overlay is painted *inside* a nested scrollable (reasoning body, code
/// panel), so its capture-phase listener registers — and therefore runs —
/// before the chat-level interceptor. It scrolls the region by the raw delta
/// and stops propagation, so the outer list never double-scrolls.
fn wheel_claim_overlay(handle: ScrollHandle, entity: WeakEntity<AppView>) -> Canvas<&'static str> {
    wheel_capture_overlay(move |_pixel_x, pixel_y, _window, cx| {
        entity
            .update(cx, |this, cx| {
                this.chat.apply_nested_scroll(&handle, pixel_y);
                cx.notify();
            })
            .is_ok()
    })
}

/// Wheel-event routing, part 2: the chat list itself. Painted as the LAST
/// child of the chat container, so its capture listener runs after every
/// nested claim overlay. It converts the raw wheel delta into a glide target
/// and stops propagation, which suppresses the built-in `list()` wheel
/// handler (bubble phase) entirely — the glide then drives
/// [`ListState::scroll_by`] once per frame in `tick_scroll`.
pub(crate) fn chat_wheel_interceptor(entity: WeakEntity<AppView>) -> Canvas<&'static str> {
    wheel_capture_overlay(move |_pixel_x, pixel_y, _window, cx| {
        entity
            .update(cx, |this, cx| {
                // A floating palette/menu owns the wheel while it is open.
                if this.modal_blocks_scroll() {
                    return false;
                }
                this.chat.wheel(pixel_y);
                cx.notify();
                true
            })
            .unwrap_or(false)
    })
}

/// The side-pane subagent transcript keeps its own list state, so its wheel
/// interceptor must feed that transcript's glide rather than the main chat's.
pub(crate) fn agent_chat_wheel_interceptor(
    agent_id: String,
    entity: WeakEntity<AppView>,
) -> Canvas<&'static str> {
    wheel_capture_overlay(move |_pixel_x, pixel_y, _window, cx| {
        let agent_id = agent_id.clone();
        entity
            .update(cx, |this, cx| {
                if this.modal_blocks_scroll() {
                    return false;
                }
                let Some(view) = this.side.agent_views.get_mut(&agent_id) else {
                    return false;
                };
                view.chat.wheel(pixel_y);
                cx.notify();
                true
            })
            .unwrap_or(false)
    })
}

pub(crate) fn render_scrollbar(
    theme: &Theme,
    geometry: Option<(f32, f32, f32)>,
    chat: ListState,
    entity: WeakEntity<AppView>,
) -> Option<AnyElement> {
    let (top, thumb_height, viewport) = geometry?;
    let track_height = (viewport - 16.0).max(1.0);
    let thumb_top = top.max(0.0).min((track_height - thumb_height).max(0.0));
    let state = chat;
    Some(
        div()
            .id("chat-scrollbar")
            .absolute()
            .top(px(8.0))
            .bottom(px(8.0))
            .right(px(4.0))
            .w(px(8.0))
            .cursor_pointer()
            .on_mouse_down(gpui::MouseButton::Left, move |event, _, cx| {
                let bounds = state.viewport_bounds();
                if bounds.size.height <= px(0.0) {
                    return;
                }
                let ratio =
                    ((event.position.y - bounds.top()) / bounds.size.height).clamp(0.0, 1.0);
                let max = state.max_offset_for_scrollbar().height;
                state.set_offset_from_scrollbar(gpui::point(px(0.0), max * ratio));
                let _ = entity.update(cx, |this, cx| {
                    this.chat.cancel_glide();
                    this.chat.mark_scrolled();
                    cx.notify();
                });
            })
            .child(
                div()
                    .absolute()
                    .top(px(thumb_top))
                    .h(px(thumb_height.min(track_height)))
                    .w_full()
                    .rounded(px(999.0))
                    .bg(theme.scrollbar)
                    .hover(|style| style.bg(theme.scrollbar_hover)),
            )
            .into_any_element(),
    )
}

// --- small helpers ---------------------------------------------------------

pub(crate) fn format_elapsed(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 60 {
        format!("{seconds} 秒")
    } else {
        format!("{} 分 {} 秒", seconds / 60, seconds % 60)
    }
}

fn format_duration_ms(milliseconds: u64) -> String {
    if milliseconds < 1000 {
        format!("{milliseconds} ms")
    } else {
        format!("{:.1} s", milliseconds as f64 / 1000.0)
    }
}

pub(crate) fn agent_color(theme: &Theme, color: SubagentColor) -> Hsla {
    match color {
        SubagentColor::Blue => theme.agent_blue,
        SubagentColor::Green => theme.agent_green,
        SubagentColor::Purple => theme.agent_purple,
        SubagentColor::Amber => theme.agent_amber,
    }
}

fn with_alpha(mut color: Hsla, alpha: f32) -> Hsla {
    color.a = alpha;
    color
}

fn tool_kind_label(name: &str) -> &'static str {
    match name {
        "read_file" => "读取",
        "write_file" => "写入",
        "edit_file" => "编辑",
        "list_dir" => "目录",
        "grep" => "搜索",
        "shell" => "命令",
        "todo_write" => "计划",
        "run_js" => "JS",
        "agent" => "子代理",
        _ => "工具",
    }
}

fn notification_summary(text: &str) -> String {
    if let Some(start) = text.find("<summary>") {
        let rest = &text[start + "<summary>".len()..];
        if let Some(end) = rest.find("</summary>") {
            let summary = rest[..end].trim();
            if !summary.is_empty() {
                return summary.to_string();
            }
        }
    }
    "任务通知已注入上下文".to_string()
}

fn latest_reasoning_line(text: &str) -> String {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| truncate_text(line, 120))
        .unwrap_or_default()
}

fn tool_icon(name: &str) -> &'static str {
    match name {
        "shell" => icons::TERMINAL,
        "read_file" | "write_file" | "edit_file" => icons::FILE,
        "list_dir" => icons::FOLDER,
        "grep" => icons::SEARCH,
        "todo_write" => icons::CHECK,
        "run_js" => icons::CODE,
        _ => icons::WRENCH,
    }
}

fn tool_detail(call: &ToolCall) -> String {
    let arguments = &call.arguments;
    let detail = match call.name.as_str() {
        "read_file" | "write_file" | "edit_file" | "list_dir" => arguments
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(".")
            .to_string(),
        "shell" => arguments
            .get("command")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        "grep" => format!(
            "{} in {}",
            arguments
                .get("pattern")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(""),
            arguments
                .get("path")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(".")
        ),
        "todo_write" => format!(
            "{} 项任务",
            arguments
                .get("todos")
                .and_then(serde_json::Value::as_array)
                .map(Vec::len)
                .unwrap_or(0)
        ),
        "run_js" => arguments
            .get("code")
            .and_then(serde_json::Value::as_str)
            .and_then(|code| code.lines().find(|line| !line.trim().is_empty()))
            .unwrap_or("JavaScript")
            .trim()
            .to_string(),
        _ => serde_json::to_string(arguments).unwrap_or_default(),
    };
    truncate_text(&detail, 140)
}

fn truncate_text(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let mut out: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

fn pretty_truncated(value: &serde_json::Value, max_chars: usize) -> String {
    let pretty = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    truncate_text(&pretty, max_chars)
}

/// Tool result/argument panel with its own scrollable body and wheel claim.
fn code_panel(
    theme: &Theme,
    title: &str,
    body: &str,
    panel_key: String,
    view: &ChatViewState,
    entity: &WeakEntity<AppView>,
) -> AnyElement {
    let scroll_handle = view
        .code_scrolls
        .borrow_mut()
        .entry(panel_key.clone())
        .or_insert_with(ScrollHandle::new)
        .clone();
    div()
        .w_full()
        .rounded(px(10.0))
        .bg(theme.code_bg)
        .overflow_hidden()
        .child(
            div()
                .px(px(12.0))
                .py(px(6.0))
                .text_size(px(10.5))
                .text_color(theme.text_faint)
                .child(title.to_string()),
        )
        .child(
            div()
                .id(SharedString::from(format!("code-{}", panel_key)))
                .max_h(px(320.0))
                .overflow_y_scroll()
                .track_scroll(&scroll_handle)
                .px(px(12.0))
                .pb(px(12.0))
                .font_family(theme.mono_font())
                .text_size(px(12.0))
                .line_height(px(18.0))
                .text_color(theme.text)
                .child(body.to_string())
                .child(wheel_claim_overlay(scroll_handle, entity.clone())),
        )
        .into_any_element()
}

// --- tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use effibuddy_agent_core::{SessionState, ToolCall, ToolResult};

    #[test]
    fn advance_chars_stays_on_utf8_boundaries() {
        let text = "你好，世界！EffiBuddy";
        let mut index = 0;
        while index < text.len() {
            index = advance_chars(text, index, 3);
            assert!(
                text.is_char_boundary(index),
                "index {index} is not a boundary"
            );
        }
        assert_eq!(index, text.len());
    }

    #[test]
    fn select_snapshot_skips_tool_messages_and_pairs_results() {
        let mut session = SessionState::new(std::env::temp_dir(), "test");
        session.push_message(Message::user("hello"));
        let call = ToolCall::new("read_file", serde_json::json!({"path": "a.rs"}));
        let mut assistant = Message::assistant();
        assistant.set_text("working");
        assistant.push_tool_call(call.clone());
        session.push_message(assistant);
        session.push_message(Message::from_tool_result(ToolResult::success(&call, "ok")));
        let mut final_message = Message::assistant();
        final_message.set_text("done");
        session.push_message(final_message);

        let mut chat = ChatList::new();
        chat.rebuild(&session, &RunSnapshot::default());

        // The two consecutive assistant messages (the tool step and the final
        // answer) merge into one turn block; the tool message itself is not
        // rendered, its result is looked up by call id.
        assert_eq!(chat.items.len(), 2);
        assert!(matches!(chat.items[0].kind, ChatItemKind::User { .. }));
        let ChatItemKind::Turn(turn) = &chat.items[1].kind else {
            panic!("expected consecutive assistant messages to merge into a turn");
        };
        assert_eq!(turn.members.len(), 2);
        assert_eq!(turn.tool_calls, 1);
        assert!(chat.results.contains_key(&call.id));
    }

    #[test]
    fn single_assistant_message_stays_a_plain_item() {
        let mut session = SessionState::new(std::env::temp_dir(), "test");
        session.push_message(Message::user("hello"));
        let mut assistant = Message::assistant();
        assistant.set_text("direct answer, no tools");
        session.push_message(assistant);

        let mut chat = ChatList::new();
        chat.rebuild(&session, &RunSnapshot::default());

        assert_eq!(chat.items.len(), 2);
        assert!(matches!(chat.items[0].kind, ChatItemKind::User { .. }));
        assert!(matches!(chat.items[1].kind, ChatItemKind::Assistant(_)));
    }

    #[test]
    fn turn_grows_with_a_stable_id_and_entry_time() {
        let mut session = SessionState::new(std::env::temp_dir(), "test");
        session.push_message(Message::user("hello"));
        let call = ToolCall::new("grep", serde_json::json!({"pattern": "x", "path": "."}));
        let mut first = Message::assistant();
        first.set_text("working");
        first.push_tool_call(call.clone());
        session.push_message(first.clone());
        session.push_message(Message::from_tool_result(ToolResult::success(&call, "ok")));

        let mut chat = ChatList::new();
        chat.rebuild(&session, &RunSnapshot::default());
        assert!(matches!(chat.items[1].kind, ChatItemKind::Assistant(_)));
        let solo_entered = chat.items[1].entered_at;

        // The next step arrives: the run merges into one turn, keyed by the
        // first message id, carrying the entry time so nothing re-fades.
        let mut second = Message::assistant();
        second.set_text("done");
        second.push_reasoning("checked once more");
        session.push_message(second);
        chat.rebuild(&session, &RunSnapshot::default());

        assert_eq!(chat.items.len(), 2);
        let ChatItemKind::Turn(turn) = &chat.items[1].kind else {
            panic!("expected a merged turn block");
        };
        assert_eq!(turn.id, format!("turn:{}", first.id));
        assert_eq!(turn.members.len(), 2);
        assert_eq!(turn.thinking_count, 1);
        assert_eq!(turn.tool_calls, 1);
        assert_eq!(chat.items[1].entered_at, solo_entered);

        // A third step keeps growing the same block.
        let mut third = Message::assistant();
        third.set_text("really done");
        session.push_message(third);
        chat.rebuild(&session, &RunSnapshot::default());

        assert_eq!(chat.items.len(), 2);
        let ChatItemKind::Turn(turn) = &chat.items[1].kind else {
            panic!("expected the turn block to persist");
        };
        assert_eq!(turn.members.len(), 3);
        assert_eq!(turn.id, format!("turn:{}", first.id));
    }

    #[test]
    fn rebuild_reuses_turn_items_and_members() {
        let mut session = SessionState::new(std::env::temp_dir(), "test");
        session.push_message(Message::user("hello"));
        let call = ToolCall::new("read_file", serde_json::json!({"path": "a.rs"}));
        let mut assistant = Message::assistant();
        assistant.set_text("working");
        assistant.push_tool_call(call.clone());
        session.push_message(assistant);
        session.push_message(Message::from_tool_result(ToolResult::success(&call, "ok")));
        let mut final_message = Message::assistant();
        final_message.set_text("done");
        session.push_message(final_message);

        let mut chat = ChatList::new();
        chat.rebuild(&session, &RunSnapshot::default());
        let old_item = chat.items[1].clone();
        let ChatItemKind::Turn(old_turn) = &old_item.kind else {
            panic!("expected a merged turn block");
        };
        let old_member = old_turn.members[0].clone();

        chat.rebuild(&session, &RunSnapshot::default());
        assert!(Rc::ptr_eq(&old_item, &chat.items[1]));
        let ChatItemKind::Turn(turn) = &chat.items[1].kind else {
            panic!("expected a merged turn block");
        };
        assert!(Rc::ptr_eq(&old_member, &turn.members[0]));
    }

    #[test]
    fn elapsed_format_matches_chat_copy() {
        assert_eq!(format_elapsed(Duration::from_secs(7)), "7 秒");
        assert_eq!(format_elapsed(Duration::from_secs(76)), "1 分 16 秒");
    }

    #[test]
    fn reveal_snapshot_never_splits_characters() {
        let mut reveal = StreamReveal::default();
        reveal.begin("m1");
        reveal.push_text("m1", "你好世界");
        reveal.advance();
        let snapshot = reveal.snapshot().expect("streaming snapshot");
        let text = snapshot.text.borrow();
        let revealed = &text[..snapshot.shown_len()];
        assert!(revealed.len() <= "你好世界".len());
        assert!("你好世界".starts_with(revealed));
        // The shared buffer must never be borrowed mutably while the
        // snapshot's guards are alive in a render path.
        drop(text);
        reveal.push_text("m1", "！");
        assert_eq!(
            snapshot.shown_len() + "！".len(),
            snapshot.text.borrow().len()
        );
    }

    #[test]
    fn stream_chunk_table_is_prefix_stable() {
        let mut reveal = StreamReveal::default();
        reveal.begin("m1");
        reveal.push_text("m1", "第一段。\n\n第二段。\n\n");
        let snapshot = reveal.snapshot().expect("streaming snapshot");
        let before = stream_chunk_table(&snapshot);
        reveal.push_text("m1", "第三段的开头");
        let snapshot = reveal.snapshot().expect("streaming snapshot");
        let after = stream_chunk_table(&snapshot);
        assert!(after.len() >= before.len());
        for (old, new) in before.iter().zip(after.iter()) {
            assert_eq!(old, new, "stable chunks must keep their ranges");
        }
    }
}
