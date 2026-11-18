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
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use base64::Engine as _;
use effibuddy_agent_core::{
    AgentMessageMeta, AttachmentKind, ContentBlock, Message, Role, RunStatus, SessionState,
    SubagentColor, SubagentStatus, ToolCall, ToolResult, UserAttachment,
};
use effibuddy_ui::{
    BadgeTone, GlideTick, IconButton, MarkdownInteraction, ScrollGlide, Spinner, Theme, badge,
    chunk_ranges, disclosure_chevron, ease_out_cubic, icons, markdown_source_key, reduce_motion,
    render_markdown_chunk_with, render_markdown_keyed_with, render_markdown_streaming_with,
    shadow_float, wheel_capture_overlay,
};
use gpui::prelude::*;
use gpui::{
    Animation, AnimationExt, AnyElement, Canvas, ClipboardItem, Div, Hsla, Image, ImageFormat,
    IntoElement, ListAlignment, ListOffset, ListState, ObjectFit, ScrollHandle, SharedString,
    WeakEntity, canvas, div, img, pulsating_between, px, svg,
};

use crate::app::AppView;

type ToolResults = HashMap<String, Rc<ToolResult>>;

// ZCode conversation column: 896px at the desktop breakpoint with 24px gutters
// (its `@min-[864px]` step), instead of the old 760px/32px.
const CONTENT_MAX_WIDTH: f32 = 896.0;
const CONTENT_PADDING: f32 = 24.0;
/// Vertical gap between chat items (turn boundaries). ZCode stacks turns with
/// `gap-5` + `pb-5`, which lands at ~40px of air between exchanges.
const ITEM_GAP: f32 = 40.0;
/// Vertical gap between the virtual rows of one split message (its markdown
/// chunks), matching the inter-block spacing inside a single document.
const CHUNK_GAP: f32 = 9.0;
const LIST_OVERDRAW: f32 = 560.0;
/// Per-frame row-plan cache counters, reported by `--bench-scroll`.
static PLAN_HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static PLAN_MISSES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How often `prepare_frame` reused the previous row plan, and how often it
/// had to rebuild it. A frame that reuses the plan is O(1) in transcript size.
pub(crate) fn plan_cache_stats() -> (u64, u64) {
    (
        PLAN_HITS.load(std::sync::atomic::Ordering::Relaxed),
        PLAN_MISSES.load(std::sync::atomic::Ordering::Relaxed),
    )
}
/// First-frame height estimate for a virtual row (message block / markdown
/// chunk) that has never been painted. Only affects scroll geometry before
/// the first paint of each row; rows are re-measured as they render.
pub(crate) const ESTIMATED_ROW_HEIGHT: f32 = 96.0;
/// Fraction of the remaining distance a pinned transcript closes per frame when
/// the tail grows by more than one smooth step (see `ChatList::follow_bottom`).
const FOLLOW_FRACTION: f32 = 0.5;
/// Largest single-frame follow step, in px. Bounds the speed so a big jump
/// (a long block painted for the first time) slides instead of teleporting.
const FOLLOW_MAX_STEP: f32 = 400.0;
/// Gaps smaller than this are closed immediately, so routine streaming keeps
/// the newest line glued to the bottom edge.
const FOLLOW_SNAP_EPSILON: f32 = 48.0;
const ENTER_ANIMATION: Duration = Duration::from_millis(260);
const ENTER_WINDOW: Duration = Duration::from_millis(320);
/// Assistant bodies larger than this are split into per-chunk virtual items.
const SPLIT_TEXT_THRESHOLD: usize = 2048;
/// Turn 展开时，展开内容逐块渐显：第 n 块延迟 n×50ms，再 180ms ease-out。
/// stagger 封顶 8 块，长 turn 的末块不会让人等动画。
const TURN_EXPAND_STAGGER: Duration = Duration::from_millis(50);
const TURN_EXPAND_STAGGER_STEPS: usize = 8;
const TURN_EXPAND_FADE: Duration = Duration::from_millis(180);
/// 展开多久之内的挂载仍然播渐显；之外（例如滚动重新挂载）直接全亮。
const TURN_EXPAND_WINDOW: Duration = Duration::from_millis(700);
/// 展开高度从 0 长到自然高度的时间：列表逐帧重测可见项，下方内容跟着真实
/// 布局平滑下移，而不是瞬间被“蹦出来的一截”顶开。
const TURN_HEIGHT_REVEAL: Duration = Duration::from_millis(280);

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
    /// "正在重试（n/m）…" while a retryable provider failure is recovering.
    /// Replaces the generic "正在思考…" line; never rendered as an error.
    pub retry: Option<String>,
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
    /// Whether user-message hover actions (copy/edit/undo) are available.
    /// Read-only child transcripts disable them; editing a child message would
    /// target the parent session.
    pub actions_enabled: bool,
    pub stream: Option<StreamSnapshot>,
    /// Fence-safe chunk table of the revealed streaming text, recomputed per
    /// frame in `prepare_frame`. Rows of the streaming message slice the
    /// revealed text with it.
    pub stream_chunks: Option<Rc<Vec<TextChunk>>>,
    /// Per-message reasoning-body scroll handles (shared with the chat list).
    pub reasoning_scrolls: Rc<RefCell<HashMap<String, ScrollHandle>>>,
    /// Per-code-panel scroll handles (shared with the chat list).
    pub code_scrolls: Rc<RefCell<HashMap<String, ScrollHandle>>>,
    /// Click hooks + base directory for links and images in markdown bodies.
    pub markdown: MarkdownInteraction,
    /// Lazily decoded tool-result rasters, keyed `call_id:index` (shared with
    /// the chat list so session switches drop the memory).
    pub tool_images: ImageCache,
    /// Conversation these rows belong to: its thumbnails live in
    /// `<data dir>/thumbs/<session id>/` (see [`crate::thumbs`]).
    pub session_id: SharedString,
    /// Downscale queue for rasters too large to hand to the renderer as-is.
    pub thumbs: Rc<RefCell<ThumbnailState>>,
    /// Set by the rail after a jump: the target user bubble paints a one-shot
    /// accent border so the destination is obvious in a long transcript.
    pub jump_flash: Option<JumpFlash>,
    /// Turn 展开时刻（含自动展开）：窗口内的展开内容按 50ms stagger 渐显。
    pub turn_expand_at: Rc<HashMap<String, Instant>>,
    /// Turn 展开内容的自然高度缓存（渲染期由观测副本回填，用于高度渐显）。
    pub turn_block_heights: Rc<RefCell<HashMap<String, f32>>>,
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

// --- prompt rail (left-edge query navigator) -------------------------------

/// One user query in the left-edge navigator. Built once per structural
/// rebuild; the previews are flattened and truncated at build time so a frame
/// only clones ref-counted strings. Clicking a mark jumps straight to the
/// stored slot index — the rail never searches the transcript.
#[derive(Clone)]
pub(crate) struct PromptMark {
    /// User message id; also keys the jump-flash animation.
    pub id: SharedString,
    /// Index into [`ChatViewState::items`] for the current rebuild.
    pub item_ix: usize,
    pub user_preview: SharedString,
    pub assistant_preview: SharedString,
    /// True when the reply has no body text yet (running or empty).
    pub assistant_placeholder: bool,
    /// Newest query of a live run.
    pub running: bool,
    /// Fingerprint of the query and every reply body that feeds the two
    /// previews. An unchanged fingerprint lets a rebuild reuse the previous
    /// mark verbatim instead of re-flattening every body it spans — on a long
    /// transcript the rail then costs O(queries), not O(bytes).
    pub fingerprint: u64,
}

/// Row footprint of one mark in the rail, in px. The rail is a compact index,
/// so rows stay fixed-height and the rail itself scrolls when queries overflow.
pub(crate) const PROMPT_RAIL_ROW: f32 = 10.0;
const PROMPT_RAIL_WIDTH: f32 = 36.0;
pub(crate) const PROMPT_RAIL_INSET: f32 = 24.0;
/// Minimum conversation width that leaves room for the rail in the left
/// margin. `CONTENT_MAX_WIDTH` (896) is centered with 24px gutters, so below
/// ~960 the rail would overlap the message column; ZCode hides its navigator
/// at the same point.
const PROMPT_RAIL_MIN_WIDTH: f32 = 960.0;
const PROMPT_RAIL_POPOVER_WIDTH: f32 = 320.0;
/// Clamp estimate for the hover card (two user lines + three reply lines).
const PROMPT_RAIL_POPOVER_ESTIMATE: f32 = 140.0;
const PROMPT_PREVIEW_CHARS: usize = 220;
const PROMPT_PREVIEW_PARAGRAPHS: usize = 2;
const PROMPT_LINE_IDLE: f32 = 12.0;
const PROMPT_LINE_HOVER: f32 = 30.0;
const PROMPT_LINE_MOTION: Duration = Duration::from_millis(140);

/// Rail geometry for the current frame, derived from the list viewport (O(1)).
#[derive(Clone, Copy, Debug)]
pub(crate) struct PromptRailMetrics {
    pub body_height: f32,
    pub view_height: f32,
    /// Top inset that vertically centers a rail shorter than the viewport.
    pub pad: f32,
    pub max_scroll: f32,
}

/// App-owned rail interaction state.
#[derive(Default)]
pub(crate) struct PromptRailState {
    pub hover: Option<usize>,
    /// Row that was hovered last frame, so its bar can animate back to idle.
    pub previous_hover: Option<usize>,
    /// Bumped on every hover change; keys the width/card animations so an
    /// interrupted grow/shrink restarts instead of resuming stale state.
    pub hover_seq: u64,
    pub card: Option<usize>,
    /// Rail scroll in px from the top of the full mark list.
    pub scroll: f32,
    /// `(active mark, viewport height)` last auto-followed. Wheel input moves
    /// `scroll` without touching this, so manual browsing is not snapped back.
    followed: Option<(usize, u32)>,
}

impl PromptRailState {
    /// Recenter on the active query whenever the transcript moves to a new
    /// query or the rail is resized. Returns whether the rail moved, so the
    /// app can drop a hover card that would otherwise detach from its row.
    pub fn follow(&mut self, active: usize, metrics: &PromptRailMetrics) -> bool {
        let before = self.scroll;
        let key = (active, (metrics.view_height * 64.0).round() as u32);
        if self.followed != Some(key) {
            self.followed = Some(key);
            let target =
                active as f32 * PROMPT_RAIL_ROW - (metrics.view_height - PROMPT_RAIL_ROW) * 0.5;
            self.scroll = target.clamp(0.0, metrics.max_scroll);
        }
        self.scroll = self.scroll.clamp(0.0, metrics.max_scroll);
        (self.scroll - before).abs() > 0.5
    }
}

/// One-shot highlight of the user bubble the rail just jumped to. Kept until
/// the next jump; the animation itself fades to zero opacity.
#[derive(Clone)]
pub(crate) struct JumpFlash {
    pub message_id: SharedString,
    pub seq: u64,
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
        /// Shared so the bubble and the hover copy action can both clone a
        /// ref-counted handle instead of copying the message text every frame.
        text: SharedString,
        /// True for a message the user typed while the model was working and
        /// that was injected at a step boundary. Renders with a「回合中」chip
        /// and without edit/undo actions (the turn checkpoint belongs to the
        /// message that started the run).
        steered: bool,
        /// Files / images / skills the user attached in the composer.
        attachments: Vec<UserAttachment>,
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

/// Approximate memory held by the virtual chat snapshot.
///
/// The list keeps its own clones of assistant messages and every tool result,
/// so on a long conversation this can be a significant fraction of the
/// transcript — and unlike the session it is rebuilt on structural changes.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ChatMemory {
    pub items: usize,
    pub text_bytes: u64,
    pub tool_results: usize,
    pub tool_result_bytes: u64,
    pub image_cache_entries: usize,
    pub image_cache_bytes: u64,
}

/// Bytes retained by a [`MessageSource`]: the id, the timestamp and the tool
/// calls. Kept next to the type so the memory report stays honest when the
/// representation changes.
fn source_bytes(source: &MessageSource) -> u64 {
    let calls: usize = source
        .tool_calls
        .iter()
        .map(|call| call.name.len() + call.arguments.to_string().len())
        .sum();
    source.id.len() as u64 + calls as u64
}

/// The parts of the source [`Message`] the render path still reads.
///
/// The item used to hold a full `Message` clone *and* the flattened
/// `text`/`reasoning`, so every assistant body was resident twice (measured at
/// ~3 MB for a 10k-message session, on top of the session's own copy). The
/// blocks are never read again once `text`, `chunks` and `text_key` are
/// derived; only identity, the two flags, the timestamp and the tool calls
/// survive, and tool calls are small compared to a body.
pub(crate) struct MessageSource {
    /// Kept as `String` (not `SharedString`) so the many `contains(&id)` /
    /// `== id` sites keep working unchanged.
    pub id: String,
    /// See [`Message::revision`]: the fingerprint a rebuild compares instead of
    /// re-hashing the body.
    pub revision: u64,
    pub interrupted: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Tool calls in source order; the row renders one card (or one group) per
    /// run of them.
    pub tool_calls: Rc<[ToolCall]>,
}

pub(crate) struct AssistantItem {
    pub message: MessageSource,
    pub reasoning_duration: Option<Duration>,
    /// Pre-computed at rebuild time so the per-frame render path neither
    /// joins the text blocks nor hashes/counts them again. `text_key` feeds
    /// the markdown document cache ([`effibuddy_ui::render_markdown_keyed`]).
    pub text: SharedString,
    pub text_key: u64,
    pub reasoning: SharedString,
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

/// Inputs that determine the virtual-row plan (see
/// [`ChatList::plan_cache`]). Comparing them is O(expanded items), which is
/// what a user has expanded — never the transcript length.
struct PlanCache {
    items: Rc<Vec<Rc<ChatItem>>>,
    running: bool,
    expanded_reasoning: HashSet<String>,
    expanded_tools: HashSet<String>,
    expanded_turns: HashSet<String>,
    collapsed_turns: HashSet<String>,
}

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
    /// Session cwd of the rebuilt transcript; markdown file links and relative
    /// image sources resolve against it.
    workspace: PathBuf,
    /// Decoded tool-result images, created lazily at render time.
    tool_images: ImageCache,
    /// Conversation the current transcript belongs to (thumbnail directory).
    session_id: SharedString,
    /// Rasters waiting for a background downscale, shared with the view.
    thumbs: Rc<RefCell<ThumbnailState>>,
    /// One mark per user query for the left-edge rail, in transcript order.
    prompt_marks: Rc<Vec<PromptMark>>,
    /// `item_ix → first virtual slot`, rebuilt with the slot plan. Jumping is
    /// an O(1) lookup here instead of a scan through the transcript.
    first_slots: Vec<usize>,
    /// Height the list currently assumes for rows it has never measured, and
    /// when it last re-derived that from real measurements (see
    /// [`Self::calibrate_row_height`]).
    seeded_row_height: f32,
    estimate_refreshed_at: Option<Instant>,
    /// Test hook: the mouse-replay harness runs its own virtual clock, so a
    /// calibration throttled by real time would fire at an arbitrary point in
    /// the replay. Calibration has its own test.
    row_calibration_off: bool,
    /// Inputs of the last virtual-row plan. When the items, the run state and
    /// the expansion sets are unchanged (and no answer is being revealed), the
    /// plan is reused verbatim and a frame costs O(1) instead of O(messages).
    plan_cache: Option<PlanCache>,
    /// Guards against scheduling more than one reveal frame at a time.
    pub reveal_frame_scheduled: bool,
    /// Smooth-scroll glide state (wheel capture + animated scroll-to-bottom).
    glide: ScrollGlide,
    /// Guards against scheduling more than one glide frame at a time.
    pub glide_frame_scheduled: bool,
    /// Guards against scheduling more than one smooth-follow frame at a time.
    pub follow_frame_scheduled: bool,
    /// Turn 展开时刻；展开窗口内的新挂载内容做 stagger 渐显。
    turn_expand_at: HashMap<String, Instant>,
    /// 已经播过自动展开渐显的 turn。`turn_expand_at` 会按窗口过期清理，
    /// 而运行中的 turn 每帧都满足「自动展开」，若不记住「已播过」，清理
    /// 后的下一帧就会重新打点，让整个活动栈在每个工具步骤之后重播渐显。
    revealed_turns: HashSet<String>,
    /// Turn 展开内容的自然高度（渲染期由屏幕外观测副本回填）：高度渐显按
    /// 「进度 × 自然高度」逐帧长高，列表重测后下方内容平滑让位。
    turn_block_heights: Rc<RefCell<HashMap<String, f32>>>,
    /// 高度渐显期间的逐帧重绘泵标记（进度在渲染期计算）。
    pub turn_height_frame_scheduled: bool,
}

/// Exponential glide toward a target pixel offset. Wheel deltas move the
/// target; each frame moves `current` a fraction of the remaining distance,
/// so repeated notches accumulate into one continuous motion instead of
/// discrete jumps. Shared with every other overflow surface through
/// `effibuddy_ui::ScrollGlide`.

impl ChatList {
    pub fn new() -> Self {
        let list_state = ListState::new(0, ListAlignment::Top, px(LIST_OVERDRAW));
        // A transcript's rows are message blocks: user bubbles, tool rows,
        // markdown chunks. A cold open resets the list before anything has been
        // painted, so without this estimate every row would count as zero
        // height and the scroll range would collapse to a single viewport
        // (the wheel then jumps straight to the top of the history).
        list_state.set_estimated_item_height(px(ESTIMATED_ROW_HEIGHT));
        Self {
            list_state,
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
            workspace: PathBuf::new(),
            tool_images: Rc::new(RefCell::new(ImageCacheInner::default())),
            session_id: SharedString::default(),
            thumbs: Rc::new(RefCell::new(ThumbnailState::default())),
            prompt_marks: Rc::new(Vec::new()),
            first_slots: Vec::new(),
            plan_cache: None,
            seeded_row_height: ESTIMATED_ROW_HEIGHT,
            estimate_refreshed_at: None,
            row_calibration_off: false,
            reveal_frame_scheduled: false,
            glide: ScrollGlide::new(),
            glide_frame_scheduled: false,
            follow_frame_scheduled: false,
            turn_expand_at: HashMap::new(),
            revealed_turns: HashSet::new(),
            turn_block_heights: Rc::new(RefCell::new(HashMap::new())),
            turn_height_frame_scheduled: false,
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
        // A glide points at offsets of the *previous* transcript; keeping it
        // would drag the new session around as soon as it lays out.
        self.cancel_glide();
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
        // Keep the shared handle (the outgoing view snapshot still holds it)
        // and release its contents, so the decoded pixels can be handed back
        // to GPUI's asset cache by the next frame.
        self.tool_images.borrow_mut().release_all();
        // Queued downscales belong to the transcript leaving the screen.
        self.thumbs.borrow_mut().cancel_pending();
        self.prompt_marks = Rc::new(Vec::new());
        self.first_slots.clear();
        self.plan_cache = None;
        self.glide = ScrollGlide::new();
        self.turn_expand_at.clear();
        self.revealed_turns.clear();
        self.turn_block_heights.borrow_mut().clear();
        self.list_state.reset(0);
    }

    /// 记录一次 turn 展开（手动或自动）：展开窗口内挂载的内容做 stagger 渐显。
    pub fn note_turn_expanded(&mut self, turn_id: &str) {
        self.turn_expand_at
            .insert(turn_id.to_string(), Instant::now());
    }

    /// 展开窗口过期后清理打点。自动展开的 live turn 不会因此重新打点——
    /// 记录仍在 `revealed_turns` 里，见 [`Self::note_auto_expanded`]。
    fn expire_turn_reveals(&mut self) {
        self.turn_expand_at
            .retain(|_, at| at.elapsed() < TURN_EXPAND_WINDOW * 2);
    }

    /// 自动展开（live turn）只打点一次：`turn_expand_at` 的条目会在窗口
    /// 过期后清理，但 turn 仍然是 live，若重新打点就会周期性重播整套展开
    /// 动画。手动展开走 [`Self::note_turn_expanded`]，仍然可以重播。
    ///
    /// 以关联函数形式接收两个字段，`prepare_frame` 遍历 `items` 时才能做
    /// 不相交的字段借用。
    fn note_auto_expanded(
        revealed: &mut HashSet<String>,
        expand_at: &mut HashMap<String, Instant>,
        turn_id: &str,
    ) {
        if revealed.insert(turn_id.to_string()) {
            expand_at.insert(turn_id.to_string(), Instant::now());
        }
    }

    /// Rebuild the item snapshot from the session. Only invoked when a
    /// structural event happened, never on token deltas.
    pub fn rebuild(&mut self, session: &SessionState, run: &RunSnapshot) {
        let trace = std::env::var_os("EFFIBUDDY_CHAT_TRACE").is_some();
        // 分段计时（只在 `EFFIBUDDY_CHAT_TRACE=1` 时打印）：几次 `Instant::now()`
        // 相对于整个重建可以忽略，所以无条件记账、按需输出。
        let mut marks: Vec<(&'static str, f64)> = Vec::new();
        let mut t0 = Instant::now();
        self.workspace = session.cwd.clone();
        if self.session_id.as_ref() != session.id {
            self.session_id = SharedString::from(session.id.clone());
        }
        let now = Instant::now();
        let old_items = self.items.clone();
        let old_results = self.results.clone();

        // Reuse tool result payloads that did not change; transcripts can hold
        // megabyte-sized observations and this runs on every tool boundary.
        // The comparison deliberately stays cheap (shape, not bytes): results
        // are write-once in practice, and `message_signature` already keys on
        // the same fields, so a byte-wise compare here would only add cost
        // without catching anything the signature misses.
        let mut results: ToolResults = HashMap::with_capacity(old_results.len());
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
        if trace {
            marks.push(("results", elapsed_ms(t0)));
            t0 = Instant::now();
        }
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
        if trace {
            marks.push(("index", elapsed_ms(t0)));
            t0 = Instant::now();
        }

        // A run of consecutive assistant messages (one per harness step) is
        // one turn; flush it whenever a different kind of message shows up.
        let mut group: Vec<(&Message, u64, Rc<AssistantItem>)> = Vec::new();
        let mut sig_ms = 0.0;
        for message in session.messages.iter() {
            if message.role == Role::Tool {
                continue;
            }
            let duration = self.durations.get(&message.id).copied();
            let sig_started = Instant::now();
            let signature = message_signature(message, &results, duration);
            sig_ms += elapsed_ms(sig_started);
            if message.role == Role::Assistant {
                let assistant = match old_members.get(message.id.as_str()) {
                    // The revision is bumped by every mutator and the signature
                    // folds in the tool results / duration, so a match means the
                    // snapshot (joined text, source key, chunk table) is still
                    // valid and can be reused as-is.
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
                    text: message.text().into(),
                    steered: message.mid_turn,
                    attachments: message.attachments.clone(),
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
            let (activity, detail) = if let Some(retry) = &run.retry {
                (retry.clone(), None)
            } else if waiting {
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
        if trace {
            marks.push(("items", elapsed_ms(t0) - sig_ms));
            marks.push(("signatures", sig_ms));
            t0 = Instant::now();
        }
        // 已播过渐显的记录只跟着现存 turn 走：编辑/撤销把 turn 从尾部截掉后
        // 不必再为它保留记录，长期会话也不会无限增长。
        let turn_ids: HashSet<&str> = items
            .iter()
            .filter(|item| matches!(&item.kind, ChatItemKind::Turn(_)))
            .map(|item| item.id.as_str())
            .collect();
        self.revealed_turns
            .retain(|id| turn_ids.contains(id.as_str()));

        self.prompt_marks = Rc::new(build_prompt_marks(
            &items,
            run.running,
            self.prompt_marks.as_slice(),
        ));
        if trace {
            marks.push(("prompt_marks", elapsed_ms(t0)));
            t0 = Instant::now();
        }
        self.items = Rc::new(items);
        self.results = Rc::new(results);
        self.entered = next_entered;
        // Virtual-row splicing happens in `prepare_frame`, which also knows
        // the live streaming chunk table and preserves measured heights.
        if self.pinned {
            self.scroll_to_bottom();
        }
        if trace {
            marks.push(("publish", elapsed_ms(t0)));
            let total: f64 = marks.iter().map(|(_, ms)| *ms).sum();
            let detail: Vec<String> = marks
                .iter()
                .map(|(label, ms)| format!("{label} {ms:.2}"))
                .collect();
            eprintln!(
                "[chat-trace] rebuild {total:.2}ms ({} msgs) | {}",
                session.messages.len(),
                detail.join(" · ")
            );
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
        actions_enabled: bool,
        expanded_reasoning: &HashSet<String>,
        expanded_tools: &HashSet<String>,
        expanded_turns: &HashSet<String>,
        collapsed_turns: &HashSet<String>,
        agents: &HashMap<String, AgentCard>,
        entity: &WeakEntity<AppView>,
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

        // 展开动画只在窗口内生效；过期的时刻顺手清掉，避免无限增长。
        self.expire_turn_reveals();
        // 高度缓存与展开时刻同寿命：key 形如 `turn:… #activity|#tools`。
        {
            let heights = self.turn_block_heights.clone();
            heights.borrow_mut().retain(|key, _| {
                let turn_id = key.split('#').next().unwrap_or(key.as_str());
                self.turn_expand_at.contains_key(turn_id)
            });
        }

        // Fast path: while nothing structural changed and no answer is being
        // revealed, this frame's row plan is byte-identical to the last one's.
        // Reusing it makes an idle or scrolling frame O(1) instead of
        // O(messages) — the difference between "0.4ms of bookkeeping every
        // frame" and "nothing", which is what a 300 Hz refresh actually needs.
        //
        // A live stream bypasses the cache on purpose: the reveal table grows
        // per frame, so both the row count and the tail row's key change.
        self.calibrate_row_height();

        let plan_unchanged = streaming_id.is_none()
            && self.plan_cache.as_ref().is_some_and(|cache| {
                Rc::ptr_eq(&cache.items, &self.items)
                    && cache.running == running
                    && cache.expanded_reasoning == *expanded_reasoning
                    && cache.expanded_tools == *expanded_tools
                    && cache.expanded_turns == *expanded_turns
                    && cache.collapsed_turns == *collapsed_turns
            });
        if plan_unchanged {
            PLAN_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let markdown = markdown_interaction(&self.workspace, entity);
            return ChatViewState {
                items: self.items.clone(),
                slots: Rc::clone(&self.slots),
                results: Rc::clone(&self.results),
                agents: Rc::new(agents.clone()),
                expanded_reasoning: expanded_reasoning.clone(),
                expanded_tools: expanded_tools.clone(),
                expanded_turns: expanded_turns.clone(),
                collapsed_turns: collapsed_turns.clone(),
                running,
                actions_enabled,
                stream_chunks: streaming_chunks,
                stream,
                reasoning_scrolls: Rc::clone(&self.reasoning_scrolls),
                code_scrolls: Rc::clone(&self.code_scrolls),
                markdown,
                tool_images: Rc::clone(&self.tool_images),
                session_id: self.session_id.clone(),
                thumbs: Rc::clone(&self.thumbs),
                jump_flash: None,
                turn_expand_at: Rc::new(self.turn_expand_at.clone()),
                turn_block_heights: Rc::clone(&self.turn_block_heights),
            };
        }

        PLAN_MISSES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Build the virtual-row plan. Assistant items with a chunk table
        // expand into lead/chunk/tail rows; the streaming message expands
        // into the per-frame table of its revealed text.
        let mut slots: Vec<ChatSlot> = Vec::with_capacity(self.items.len() + 8);
        let mut keys: Vec<(u64, u64)> = Vec::with_capacity(slots.capacity());
        self.first_slots.clear();
        self.first_slots.reserve(self.items.len());
        for (item_ix, item) in self.items.iter().enumerate() {
            self.first_slots.push(slots.len());
            let signature = item.signature;
            // 折叠/展开状态是渲染高度的一部分：混进 slot key 后，切换会让
            // 前缀对齐从该行断开并触发 splice，ListState 便会重新测量这个
            // 条目（视口外的高度缓存不再失真，滚动边界保持正确）。
            let presentation = item_presentation(
                item,
                expanded_reasoning,
                expanded_tools,
                expanded_turns,
                collapsed_turns,
                running,
                streaming_id.as_deref(),
            );
            // 自动展开（live turn）也记一次时刻，展开内容做 stagger 渐显。
            if let ChatItemKind::Turn(turn) = &item.kind {
                let auto_expanded = !expanded_turns.contains(&turn.id)
                    && turn_expanded_with(
                        turn,
                        collapsed_turns,
                        expanded_turns,
                        running,
                        streaming_id.as_deref(),
                    );
                if auto_expanded {
                    Self::note_auto_expanded(
                        &mut self.revealed_turns,
                        &mut self.turn_expand_at,
                        &turn.id,
                    );
                }
            }
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
                    ChatItemKind::Turn(turn) => {
                        let body_ix = turn_body_index(turn, streaming_id.as_deref());
                        turn.members[body_ix].chunks.as_deref().map(Vec::as_slice)
                    }
                    _ => None,
                }
            };
            match chunks {
                Some(chunks) if chunks.len() >= 2 => {
                    slots.push(ChatSlot {
                        item_ix,
                        part: SlotPart::Lead,
                    });
                    keys.push((signature, hash_values(&(0_u64, presentation))));
                    for index in 1..chunks.len() - 1 {
                        slots.push(ChatSlot {
                            item_ix,
                            part: SlotPart::Chunk(index),
                        });
                        keys.push((signature, hash_values(&(chunks[index].key, presentation))));
                    }
                    slots.push(ChatSlot {
                        item_ix,
                        part: SlotPart::Tail,
                    });
                    keys.push((
                        signature,
                        hash_values(&(chunks[chunks.len() - 1].key, presentation)),
                    ));
                }
                _ => {
                    slots.push(ChatSlot {
                        item_ix,
                        part: SlotPart::Whole,
                    });
                    keys.push((signature, hash_values(&(0_u64, presentation))));
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
            // `reset` clears the logical scroll top, so a transcript that was
            // following the tail would land on row 0 — a cold open of a long
            // session would show its opening instead of the newest messages.
            // Re-arm the tail chase now that the row count is known.
            if self.pinned {
                self.scroll_to_bottom();
            }
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
        // Remember what the plan was derived from, so the next frame can skip
        // the whole walk when nothing changed (see the plan_cache field).
        self.plan_cache = Some(PlanCache {
            items: self.items.clone(),
            running,
            expanded_reasoning: expanded_reasoning.clone(),
            expanded_tools: expanded_tools.clone(),
            expanded_turns: expanded_turns.clone(),
            collapsed_turns: collapsed_turns.clone(),
        });

        let markdown = markdown_interaction(&self.workspace, entity);
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
            actions_enabled,
            stream_chunks: streaming_chunks,
            stream,
            reasoning_scrolls: Rc::clone(&self.reasoning_scrolls),
            code_scrolls: Rc::clone(&self.code_scrolls),
            markdown,
            tool_images: Rc::clone(&self.tool_images),
            session_id: self.session_id.clone(),
            thumbs: Rc::clone(&self.thumbs),
            jump_flash: None,
            turn_expand_at: Rc::new(self.turn_expand_at.clone()),
            turn_block_heights: Rc::clone(&self.turn_block_heights),
        }
    }

    /// turn 高度渐显期间需要逐帧重绘（进度在渲染期计算）。
    /// trace helper: how many logical rows the transcript has.
    pub fn trace_counts(&self) -> (usize, usize) {
        (self.items.len(), self.slots.len())
    }

    /// Rasters the cache dropped since the last call, for the host to unload
    /// from GPUI's asset cache (see [`ImageCacheInner::dropped`]).
    pub fn take_dropped_images(&mut self) -> Vec<Arc<Image>> {
        std::mem::take(&mut self.tool_images.borrow_mut().dropped)
    }

    /// Downscales the next frame asked for, for the host to run off the UI
    /// thread (see [`ThumbnailState`]).
    pub fn take_thumbnail_requests(&mut self) -> Vec<ThumbnailRequest> {
        std::mem::take(&mut self.thumbs.borrow_mut().requests)
    }

    /// A built thumbnail: hold it so the next frame paints without touching
    /// the disk, and charge the raster cache its real size.
    pub fn thumbnail_ready(&mut self, key: &str, png: Vec<u8>) {
        self.thumbs.borrow_mut().mark_ready(key);
        let image = Arc::new(Image::from_bytes(ImageFormat::Png, png));
        self.tool_images
            .borrow_mut()
            .insert(key.to_string(), Some(image), THUMBNAIL_PIXEL_BYTES);
    }

    /// No thumbnail for this raster: fall back to drawing it as it came.
    pub fn thumbnail_unavailable(&mut self, key: &str) {
        self.thumbs.borrow_mut().mark_unavailable(key);
    }

    /// Approximate footprint of this snapshot for the memory diagnostics
    /// panel. O(items + results), only called while the panel is open.
    pub fn memory_stats(&self) -> ChatMemory {
        let mut stats = ChatMemory {
            items: self.items.len(),
            ..ChatMemory::default()
        };
        for item in self.items.iter() {
            match &item.kind {
                ChatItemKind::User {
                    text, attachments, ..
                } => {
                    stats.text_bytes += text.len() as u64;
                    for attachment in attachments {
                        stats.text_bytes += attachment.text.as_ref().map_or(0, String::len) as u64;
                        stats.text_bytes += attachment
                            .image
                            .as_ref()
                            .map_or(0, |image| image.data.len())
                            as u64;
                    }
                }
                ChatItemKind::Assistant(assistant) => {
                    // Only the flattened text/reasoning are retained now; the
                    // source message stays in the session and is not cloned.
                    stats.text_bytes +=
                        assistant.text.len() as u64 + assistant.reasoning.len() as u64;
                    stats.text_bytes += source_bytes(&assistant.message);
                }
                ChatItemKind::Turn(turn) => {
                    for member in &turn.members {
                        stats.text_bytes +=
                            member.text.len() as u64 + member.reasoning.len() as u64;
                        stats.text_bytes += source_bytes(&member.message);
                    }
                }
                ChatItemKind::System { text }
                | ChatItemKind::Error { text }
                | ChatItemKind::Notice { text } => stats.text_bytes += text.len() as u64,
                ChatItemKind::TaskNotification { summary } => {
                    stats.text_bytes += summary.len() as u64;
                }
                ChatItemKind::AgentMessage { meta, text } => {
                    stats.text_bytes += (meta.text.len() + text.len()) as u64;
                }
                ChatItemKind::RunStatus(_) | ChatItemKind::Empty => {}
            }
        }
        for result in self.results.values() {
            stats.tool_results += 1;
            stats.tool_result_bytes += result.content.len() as u64
                + result
                    .images
                    .iter()
                    .map(|image| image.data.len() as u64)
                    .sum::<u64>();
        }
        // Report the decompressed footprint, not the compressed payload: the
        // compressed size is off by an order of magnitude for screenshots and
        // made this field useless for judging the cache.
        let images = self.tool_images.borrow();
        stats.image_cache_entries = images.entries.len();
        stats.image_cache_bytes = images.bytes;
        stats
    }

    pub fn turn_height_animating(&self) -> bool {
        self.turn_expand_at
            .values()
            .any(|at| at.elapsed() < TURN_HEIGHT_REVEAL)
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

    /// Drop every local trace of a partially streamed message that the
    /// harness rolled back before retrying. The next attempt opens a new
    /// message id, so the reveal would reset anyway; clearing it here also
    /// stops the old buffer from surviving on the frame in between.
    pub fn discard_message(&mut self, message_id: &str) {
        if self.reveal.message_id.as_deref() == Some(message_id) {
            self.end_stream();
        }
        self.durations.remove(message_id);
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
            self.follow_bottom();
        }
    }

    /// Keep a pinned transcript at the bottom.
    ///
    /// Contrary to a hard `scroll_to(item_count)` this eases the last stretch:
    /// when the tail grows by a viewport or more in one frame (a big delta, a
    /// tool result, the first paint of a long block) the viewport is moved by
    /// at most [`FOLLOW_MAX_STEP`] toward the new bottom and the rest is left
    /// for the following frames. The result reads as a smooth follow instead of
    /// a jump, and small growth (normal streaming) is still applied in the same
    /// frame so the typing indicator never leaves the bottom edge.
    fn follow_bottom(&mut self) {
        let max = f32::from(self.list_state.max_offset_for_scrollbar().height);
        let offset = -f32::from(self.list_state.scroll_px_offset_for_scrollbar().y);
        if offset > max {
            // A previous `scroll_to(item_count)` parks the anchor *past* the
            // last row; layout clamps it back to the bottom every frame, so the
            // viewport looks right but the anchor is not a real row, and the
            // next growth is applied all at once. Normalising to a real anchor
            // at exactly the bottom is visually identical (the clamp already
            // put it there) and lets the follow ease from a stable position.
            self.list_state.scroll_by(px(max - offset));
            return;
        }
        let remaining = max - offset;
        if remaining <= 0.0 {
            // Already at the end, or the geometry shrank under us.
            self.list_state.scroll_by(px(max - offset));
            return;
        }
        // Close at most a fixed fraction of the gap per frame, and never more
        // than one viewport: a huge jump (long block first painted, a tool
        // result, a session switch) then reads as a fast but smooth slide,
        // while routine streaming growth (a few pixels) is fully applied in the
        // same frame so the last line never drifts below the bottom edge.
        let step = (remaining * FOLLOW_FRACTION).clamp(0.0, FOLLOW_MAX_STEP);
        if remaining - step <= FOLLOW_SNAP_EPSILON {
            self.list_state.scroll_by(px(remaining));
            return;
        }
        self.list_state.scroll_by(px(step));
        self.follow_frame_scheduled = true;
    }

    pub fn mark_scrolled(&mut self) {
        self.scroll_dirty = true;
    }

    pub fn scroll_to_bottom(&mut self) {
        // Prefer a pixel-exact move to the end over parking the anchor past the
        // last row. Both look identical, but an anchor past the end is only
        // clamped into place during layout, so the list has no real scroll
        // position to ease from and the next growth is applied in one jump.
        let max = f32::from(self.list_state.max_offset_for_scrollbar().height);
        if max > 0.0 {
            let offset = -f32::from(self.list_state.scroll_px_offset_for_scrollbar().y);
            let delta = max - offset;
            if delta.abs() > 0.01 {
                self.list_state.scroll_by(px(delta));
            }
            return;
        }
        // Nothing measured yet (cold open): park the anchor past the last row so
        // the first layout clamps to the tail.
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
        let max = f32::from(self.list_state.max_offset_for_scrollbar().height);
        let current = -f32::from(self.list_state.scroll_px_offset_for_scrollbar().y);
        let moved = self.glide.wheel(pixel_y, current, max);
        if pixel_y > 0.0 && moved {
            // Scrolling up always unpins, even before the first glide tick
            // moves the list: `sync_scroll` would otherwise force the viewport
            // back to the bottom with a stale `pinned` flag. When there is
            // nothing above (`moved == false`) the transcript stays pinned.
            self.pinned = false;
        }
        // Wheeling down to the very bottom re-arms the bottom chase inside the
        // glide; `tick_scroll` pins again once it settles there.
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
    /// Whether a wheel/target glide is still moving the viewport.
    pub fn glide_is_active(&self) -> bool {
        self.glide.active()
    }

    /// Re-derive the never-measured row height from the rows that *have* been
    /// measured.
    ///
    /// The first guess ([`ESTIMATED_ROW_HEIGHT`]) is only ever right for the
    /// kind of row it was tuned on. Every row the user scrolls past that is
    /// still holding a bad guess shifts the pixel coordinate system by the
    /// error — measured at up to 422px per frame on a 10k-message session,
    /// which moves the scrollbar and the pinned/follow logic even though the
    /// visible anchor is preserved. One pass over the height tree fixes the
    /// remaining rows; it is throttled because the walk is O(rows).
    fn calibrate_row_height(&mut self) {
        const REFRESH: Duration = Duration::from_millis(400);
        const TOLERANCE: f32 = 12.0;
        // Only while the viewport is standing still: re-seeding rewrites every
        // unmeasured height, which moves the pixel coordinate system by tens of
        // thousands of pixels. The row anchor is preserved, so an idle
        // transcript does not visibly move — but a glide in flight would keep
        // chasing a target expressed in the old coordinates.
        if self.glide.active() || self.row_calibration_off {
            return;
        }
        let now = Instant::now();
        if let Some(last) = self.estimate_refreshed_at
            && now.duration_since(last) < REFRESH
        {
            return;
        }
        self.estimate_refreshed_at = Some(now);
        let Some(mean) = self.list_state.measured_mean_height() else {
            return;
        };
        let mean = f32::from(mean);
        if (mean - self.seeded_row_height).abs() < TOLERANCE {
            return;
        }
        if std::env::var_os("EFFIBUDDY_ROW_CALIBRATE_TRACE").is_some() {
            println!(
                "[calibrate] 行高估算 {:.1} → {:.1}px（已测量均值）",
                self.seeded_row_height, mean
            );
        }
        self.seeded_row_height = mean;
        self.list_state.set_estimated_item_height(px(mean));
        self.list_state.reseed_unmeasured(px(mean));
    }

    /// Turn off the real-time row-height calibration (tests that drive their
    /// own clock).
    pub fn set_row_calibration_enabled(&mut self, enabled: bool) {
        self.row_calibration_off = !enabled;
    }

    pub fn tick_scroll(&mut self) -> bool {
        self.tick_scroll_at(Instant::now())
    }

    /// [`Self::tick_scroll`] with an injected clock. The mouse simulation uses
    /// it to replay a gesture at a fixed frame rate instead of real time (where
    /// a tight loop has `dt == 0` and the glide never moves).
    pub fn tick_scroll_at(&mut self, now: Instant) -> bool {
        if !self.glide.active() {
            return false;
        }
        let max = f32::from(self.list_state.max_offset_for_scrollbar().height);
        let real = -f32::from(self.list_state.scroll_px_offset_for_scrollbar().y);
        match self.glide.tick_at(now, real, max) {
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
    ///
    /// GPUI's wheel convention is `positive = up`; a `ScrollHandle` offset is
    /// negative while scrolled down, so the deltas add directly. (The previous
    /// subtraction happened to be unreachable: the overlay was laid out with
    /// zero height and never received the wheel.)
    fn apply_nested_scroll(&mut self, handle: &ScrollHandle, pixel_y: f32) {
        let max = handle.max_offset();
        let offset = handle.offset();
        let next_y = nested_scroll_y(f32::from(offset.y), pixel_y, f32::from(max.height));
        handle.set_offset(gpui::point(offset.x, px(next_y)));
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

    // --- prompt rail -------------------------------------------------------

    /// Cheap `Rc` clone of the query marks, in transcript order.
    pub fn prompt_marks(&self) -> Rc<Vec<PromptMark>> {
        Rc::clone(&self.prompt_marks)
    }

    /// Rail geometry for this frame, or `None` while there is nothing to
    /// navigate (no queries, first layout, or a conversation too narrow for
    /// the rail to sit beside the message column).
    pub fn prompt_rail_metrics(&self) -> Option<PromptRailMetrics> {
        if self.prompt_marks.is_empty() {
            return None;
        }
        let bounds = self.list_state.viewport_bounds();
        let body_height = f32::from(bounds.size.height);
        let width = f32::from(bounds.size.width);
        if width < PROMPT_RAIL_MIN_WIDTH || body_height <= PROMPT_RAIL_INSET * 2.0 + PROMPT_RAIL_ROW
        {
            return None;
        }
        let rail_height = body_height - PROMPT_RAIL_INSET * 2.0;
        let total = self.prompt_marks.len() as f32 * PROMPT_RAIL_ROW;
        let view_height = total.min(rail_height);
        Some(PromptRailMetrics {
            body_height,
            view_height,
            pad: ((rail_height - view_height) * 0.5).max(0.0),
            max_scroll: (total - view_height).max(0.0),
        })
    }

    /// Mark containing the top of the viewport. The scroll position is already
    /// a `(slot, offset)` pair, so this is one binary search over the marks —
    /// O(log n) per scroll frame, with no transcript scan.
    pub fn active_prompt_index(&self) -> Option<usize> {
        if self.prompt_marks.is_empty() {
            return None;
        }
        let ListOffset { item_ix, .. } = self.list_state.logical_scroll_top();
        let position = self.prompt_marks.partition_point(|mark| {
            self.first_slots
                .get(mark.item_ix)
                .is_none_or(|first| *first <= item_ix)
        });
        // Above the first query there is no owning mark; like ZCode's
        // navigator, fall back to the first entry instead of no highlight.
        Some(position.saturating_sub(1))
    }

    /// Jump to one query by index. `first_slots` was built with the current
    /// slot plan, so this is a single `scroll_to` (O(1)); no searching for the
    /// message, no layout of intermediate rows.
    pub fn jump_to_prompt(&mut self, mark_ix: usize) -> Option<SharedString> {
        // Copy the plan out before mutating the list: the mark borrows
        // `prompt_marks`, and `cancel_glide`/`scroll_to` need `&mut self`.
        let (slot_ix, id) = {
            let mark = self.prompt_marks.get(mark_ix)?;
            (*self.first_slots.get(mark.item_ix)?, mark.id.clone())
        };
        self.pinned = false;
        self.cancel_glide();
        self.list_state.scroll_to(ListOffset {
            item_ix: slot_ix,
            // Small negative offset leaves breathing room above the bubble.
            offset_in_item: px(-16.0),
        });
        // A programmatic scroll does not fire the list's scroll handler, so
        // clearing this keeps `sync_scroll` from recomputing `pinned` from
        // pre-jump geometry on the next frame.
        self.scroll_dirty = false;
        Some(id)
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
    // Byte-comparing every observation on every rebuild is O(transcript) and
    // measurable on a long session (tens of KB per result × thousands of
    // results). Length plus the cheap discriminators is enough: a tool result
    // is written once when the tool finishes and never edited in place, and
    // `message_signature` keys on the same triple, so a false "reuse" here
    // cannot hide a change the signature would have noticed.
    left.call_id == right.call_id
        && left.is_error == right.is_error
        && left.duration_ms == right.duration_ms
        && left.content.len() == right.content.len()
        && left.images.len() == right.images.len()
}

/// One rail mark per real user query, in transcript order. The reply preview
/// for a query spans every assistant message until the next query, so a
/// multi-step turn shows the same text the user remembers answering.
fn build_prompt_marks(
    items: &[Rc<ChatItem>],
    running: bool,
    previous: &[PromptMark],
) -> Vec<PromptMark> {
    // Marks are keyed by the user message id: a rebuild that only appended a
    // reply reuses every unchanged mark instead of re-flattening its bodies.
    let old_marks: HashMap<&str, &PromptMark> = previous
        .iter()
        .map(|mark| (mark.id.as_ref(), mark))
        .collect();

    let mut marks = Vec::with_capacity(previous.len() + 2);
    let mut index = 0;
    while index < items.len() {
        let ChatItemKind::User {
            text, attachments, ..
        } = &items[index].kind
        else {
            index += 1;
            continue;
        };
        // Walk to the next query once; each assistant body is visited by
        // exactly one mark, so building the entire rail stays O(items).
        //
        // The fingerprint deliberately keys on revisions and lengths instead of
        // hashing body text: a rebuild that only touched the tail must not pay
        // for re-reading every byte of a megabyte transcript.
        let mut end = index + 1;
        let mut signature = DefaultHasher::new();
        text.hash(&mut signature);
        (items[index].id.as_str(), attachments.len()).hash(&mut signature);
        while end < items.len() {
            match &items[end].kind {
                ChatItemKind::User { .. } | ChatItemKind::AgentMessage { .. } => break,
                ChatItemKind::Assistant(assistant) => {
                    assistant.message.revision.hash(&mut signature);
                    assistant.text.len().hash(&mut signature);
                }
                ChatItemKind::Turn(turn) => {
                    for member in &turn.members {
                        member.message.revision.hash(&mut signature);
                        member.text.len().hash(&mut signature);
                    }
                }
                _ => {}
            }
            end += 1;
        }
        let fingerprint = signature.finish();

        // Unchanged since the last rebuild: reuse the flattened previews
        // verbatim (the only field that can differ is `running`, set below).
        if let Some(existing) = old_marks
            .get(items[index].id.as_str())
            .filter(|mark| mark.fingerprint == fingerprint)
        {
            marks.push(PromptMark {
                item_ix: index,
                running: false,
                ..(**existing).clone()
            });
            index = end;
            continue;
        }

        let mut user_source = text.to_string();
        if user_source.trim().is_empty() && !attachments.is_empty() {
            user_source = format!(
                "附件：{}",
                attachments
                    .iter()
                    .map(crate::attachments::chip_title)
                    .collect::<Vec<_>>()
                    .join("、")
            );
        }
        let user_preview = preview_text(
            &[user_source.as_str()],
            "用户输入",
            PROMPT_PREVIEW_CHARS,
            PROMPT_PREVIEW_PARAGRAPHS,
        );

        // Only the first two paragraphs of the first few bodies can survive the
        // budget, so stop flattening once the preview is known to be full.
        let mut bodies: Vec<&str> = Vec::new();
        let mut start = index + 1;
        while start < end {
            match &items[start].kind {
                ChatItemKind::Assistant(assistant) => bodies.push(assistant.text.as_ref()),
                ChatItemKind::Turn(turn) => {
                    bodies.extend(turn.members.iter().map(|member| member.text.as_ref()))
                }
                _ => {}
            }
            start += 1;
        }
        let assistant_preview = if bodies.iter().any(|body| !body.trim().is_empty()) {
            preview_text(&bodies, "", PROMPT_PREVIEW_CHARS, PROMPT_PREVIEW_PARAGRAPHS)
        } else {
            String::new()
        };
        marks.push(PromptMark {
            id: items[index].id.clone().into(),
            item_ix: index,
            user_preview: user_preview.into(),
            assistant_placeholder: assistant_preview.is_empty(),
            assistant_preview: assistant_preview.into(),
            running: false,
            fingerprint,
        });
        index = end;
    }
    if running && let Some(last) = marks.last_mut() {
        last.running = true;
    }
    marks
}

/// Flatten preview source for the rail's hover card: whitespace runs collapse,
/// blank lines become paragraph breaks (at most `max_paragraphs`), and the scan
/// stops one character past the budget. A 300 KB answer therefore costs the
/// same as a short one, which keeps rebuilds proportional to query count.
fn preview_text(texts: &[&str], fallback: &str, max_chars: usize, max_paragraphs: usize) -> String {
    let max_chars = max_chars.max(8);
    let max_paragraphs = max_paragraphs.max(1);
    let mut out = String::with_capacity(max_chars + 1);
    let mut chars = 0usize;
    let mut paragraphs = 1usize;
    let mut pending_space = false;
    let mut newlines = 0usize;
    let mut overflow = false;
    'texts: for text in texts {
        if !out.is_empty() {
            pending_space = true;
            newlines = 2;
        }
        for ch in text.chars() {
            if ch.is_whitespace() {
                pending_space = true;
                if ch == '\n' {
                    newlines += 1;
                }
                continue;
            }
            if pending_space && !out.is_empty() {
                if newlines >= 2 {
                    if paragraphs >= max_paragraphs {
                        overflow = true;
                        break 'texts;
                    }
                    paragraphs += 1;
                    out.push('\n');
                } else {
                    out.push(' ');
                }
                chars += 1;
            }
            out.push(ch);
            chars += 1;
            pending_space = false;
            newlines = 0;
            if chars > max_chars {
                overflow = true;
                break 'texts;
            }
        }
    }
    if out.is_empty() {
        return fallback.to_string();
    }
    if overflow {
        let mut cut: String = out.chars().take(max_chars.saturating_sub(1)).collect();
        let trimmed = cut.trim_end().len();
        cut.truncate(trimmed);
        cut.push('…');
        return cut;
    }
    out
}

/// Derive the per-message assistant snapshot: joined text/reasoning, the
/// markdown source key, and the fence-safe chunk table for long bodies.
fn build_assistant_item(message: &Message, reasoning_duration: Option<Duration>) -> AssistantItem {
    let text = message.text();
    let reasoning = message.reasoning();
    let chunks = ChatList::split_chunks(&text);
    let tool_calls: Rc<[ToolCall]> = message
        .blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolCall { call } => Some(call.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .into();
    AssistantItem {
        message: MessageSource {
            id: message.id.clone(),
            revision: message.revision,
            interrupted: message.interrupted,
            created_at: message.created_at,
            tool_calls,
        },
        reasoning_duration,
        text_key: markdown_source_key(&text),
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
    // The revision alone identifies the message's own content (every mutator
    // bumps it), but two inputs live outside the message: the tool results its
    // calls resolve to, and the execution duration the UI prints. Those are
    // small (a hash lookup per call), while the text is not — hashing the
    // transcript used to dominate a rebuild of a long session.
    let mut hasher = DefaultHasher::new();
    message.revision.hash(&mut hasher);
    message.interrupted.hash(&mut hasher);
    message.mid_turn.hash(&mut hasher);
    reasoning_duration
        .map(|duration| duration.as_millis())
        .hash(&mut hasher);
    for block in &message.blocks {
        match block {
            ContentBlock::Text { text } | ContentBlock::Reasoning { text } => {
                // Cheap staleness guard for a message that was mutated without
                // going through a `Message` method (a reloaded transcript, or a
                // caller that reached into `blocks` directly).
                text.len().hash(&mut hasher);
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
    for attachment in &message.attachments {
        4u8.hash(&mut hasher);
        attachment.name.hash(&mut hasher);
        attachment.path.hash(&mut hasher);
        attachment.size.hash(&mut hasher);
        attachment.truncated.hash(&mut hasher);
        let kind = match attachment.kind {
            AttachmentKind::File => 0u8,
            AttachmentKind::Image => 1u8,
            AttachmentKind::Skill => 2u8,
        };
        kind.hash(&mut hasher);
        // Bodies are immutable after creation; the length is enough to notice
        // a reload that replaced them without hashing megabytes per rebuild.
        attachment
            .text
            .as_ref()
            .map(|text| text.len())
            .hash(&mut hasher);
    }
    hasher.finish()
}

fn hash_values(values: &impl Hash) -> u64 {
    let mut hasher = DefaultHasher::new();
    values.hash(&mut hasher);
    hasher.finish()
}

/// Milliseconds elapsed since `started`, for the env-gated rebuild trace.
fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
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
    reasoning_started: Option<Instant>,
    reasoning_elapsed: Option<Duration>,
    /// Timestamp of the previous `advance`, used to derive the real frame
    /// delta so the reveal speed does not depend on the refresh rate.
    last_advance: Option<Instant>,
}

impl StreamReveal {
    fn begin(&mut self, message_id: &str) {
        if self.message_id.as_deref() == Some(message_id) {
            return;
        }
        *self = Self {
            message_id: Some(message_id.to_string()),
            active: true,
            ..Self::default()
        };
        // A fresh stream must not inherit the previous stream's timestamp, or
        // the first dt would span the whole idle gap between sessions.
        self.last_advance = None;
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
        self.last_advance = None;
        if self.reasoning_started.is_some() && self.reasoning_elapsed.is_none() {
            self.reasoning_elapsed = self.reasoning_started.map(|start| start.elapsed());
        }
        self.active = false;
    }

    fn reasoning_duration(&self) -> Option<Duration> {
        if self.reasoning.borrow().is_empty() {
            return None;
        }
        self.reasoning_elapsed
            .or_else(|| self.reasoning_started.map(|start| start.elapsed()))
    }

    fn advance(&mut self) -> bool {
        let now = Instant::now();
        // Frame-rate independence: the previous version advanced a fixed
        // 4 + backlog/10 bytes per frame, so the visible speed was tied to the
        // display refresh rate (120 Hz drained twice as fast as 60 Hz) and a
        // stalled frame produced a visible stutter. Speed now derives from the
        // real elapsed time since the last tick.
        let dt = match self.last_advance {
            Some(last) => now.duration_since(last).as_secs_f32().clamp(0.0, 0.1),
            None => 1.0 / 60.0,
        };
        self.last_advance = Some(now);

        {
            let text = self.text.borrow_mut();
            let backlog = text.len().saturating_sub(self.shown_text);
            if backlog > 0 {
                let step = reveal_step(backlog, dt, TEXT_REVEAL_BASE, TEXT_REVEAL_TAU);
                if step > 0 {
                    self.shown_text = advance_chars(&text, self.shown_text, step);
                }
            }
        }
        {
            let reasoning = self.reasoning.borrow_mut();
            let backlog = reasoning.len().saturating_sub(self.shown_reasoning);
            if backlog > 0 {
                let step = reveal_step(backlog, dt, REASONING_REVEAL_BASE, REASONING_REVEAL_TAU);
                if step > 0 {
                    self.shown_reasoning = advance_chars(&reasoning, self.shown_reasoning, step);
                }
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

/// Characters revealed per second when the backlog is small. The rate rises
/// with the backlog so a large burst (a tool result dumping thousands of bytes
/// in one delta) still catches up instead of typing forever.
const TEXT_REVEAL_BASE: f32 = 900.0;
const REASONING_REVEAL_BASE: f32 = 600.0;
/// Backlog (bytes) at which the reveal rate roughly doubles.
const TEXT_REVEAL_TAU: f32 = 2200.0;
const REASONING_REVEAL_TAU: f32 = 2600.0;
/// Hard ceiling per frame so a huge backlog cannot dump the whole buffer in a
/// single repaint (which looks like an instant paste, not typing).
const REVEAL_STEP_CAP: usize = 640;

/// How many characters to reveal this frame.
///
/// `backlog` is the remaining byte count, `dt` the real seconds since the last
/// tick. The rate scales with the backlog (so it accelerates to catch up) and
/// is multiplied by `dt` (so the animation runs at the same speed regardless
/// of the display refresh rate). Always returns at least 1 while work remains,
/// otherwise a slow frame could round down to zero and stall the reveal.
fn reveal_step(backlog: usize, dt: f32, base: f32, tau: f32) -> usize {
    if backlog == 0 {
        return 0;
    }
    let scale = 1.0 + (backlog as f32) / tau;
    let chars = (base * scale * dt) as usize;
    chars.clamp(1, REVEAL_STEP_CAP)
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

/// Markdown click hook shared by every row: the app owns the menu, the
/// renderer only reports the target and the click position.
fn markdown_interaction(
    base_dir: &std::path::Path,
    entity: &WeakEntity<AppView>,
) -> MarkdownInteraction {
    let entity = entity.clone();
    MarkdownInteraction::new(
        base_dir.to_path_buf(),
        move |target, button, position, base_dir, window, cx| {
            let _ = entity.update(cx, |this, cx| {
                this.activate_markdown_target(target, button, position, base_dir, window, cx);
            });
        },
    )
}

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
    let flash_seq = view
        .jump_flash
        .as_ref()
        .filter(|flash| flash.message_id.as_ref() == item.id.as_str())
        .map(|flash| flash.seq);
    let content = match (&item.kind, slot.part) {
        (_, SlotPart::Whole) => match &item.kind {
            ChatItemKind::User {
                text,
                steered,
                attachments,
            } => render_user(
                view,
                theme,
                text,
                attachments,
                &item.id,
                *steered,
                view.running,
                view.actions_enabled,
                flash_seq,
                entity,
            ),
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
            render_assistant_chunk(turn_body(turn, view), view, chunk_ix, theme)
        }
        (ChatItemKind::Turn(turn), SlotPart::Tail) => render_turn_tail(turn, view, theme, entity),
        _ => div().into_any_element(),
    };

    let fresh = !reduce_motion()
        && !matches!(item.kind, ChatItemKind::RunStatus(_) | ChatItemKind::Empty)
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
        // ZCode rhythm: ~40px between turns, 9px between the chunk rows of
        // one message. The user hover-action strip rides inside its own row,
        // so it no longer needs a special case in the inter-item padding.
        .pb(px(if last_part { ITEM_GAP } else { CHUNK_GAP }))
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

/// Right-aligned user bubble plus the hover-revealed actions. The action row
/// always reserves its height, so hovering cannot shift the transcript.
fn render_user(
    view: &ChatViewState,
    theme: &Theme,
    text: &SharedString,
    attachments: &[UserAttachment],
    message_id: &str,
    steered: bool,
    running: bool,
    actions: bool,
    flash_seq: Option<u64>,
    entity: &WeakEntity<AppView>,
) -> AnyElement {
    let group = SharedString::from(format!("user-{message_id}"));
    let copy_id = SharedString::from(format!("user-copy-{message_id}"));
    let copy_text = text.clone();
    let edit_id = message_id.to_string();
    let edit_entity = entity.clone();
    let undo_id = message_id.to_string();
    let undo_entity = entity.clone();
    let edit_tip = if running {
        "任务运行结束后可编辑"
    } else {
        "编辑这条消息"
    };

    let mut container = div()
        .w_full()
        .flex()
        .flex_col()
        .items_end()
        .group(group.clone());
    if steered {
        container = container.child(
            div()
                .flex()
                .items_center()
                .gap(px(4.0))
                .mb(px(4.0))
                .text_size(px(10.0))
                .text_color(theme.text_faint)
                .child(
                    svg()
                        .path(icons::CLOCK)
                        .size(px(10.0))
                        .text_color(theme.text_faint),
                )
                .child("运行中插入"),
        );
    }
    // ZCode user bubble: 12px radius with a 2px top-right corner (the "tail"
    // pointing at the sender), 16/12 padding, capped at 576px.
    let mut bubble = div()
        .max_w(px(576.0))
        .min_w(px(0.0))
        .rounded(px(12.0))
        .rounded_tr(px(2.0))
        .border_1()
        .border_color(theme.border)
        .bg(theme.user_bubble)
        .px(px(16.0))
        .py(px(12.0))
        .text_size(px(14.0))
        .line_height(px(21.0))
        .text_color(theme.text);
    let image_attachments: Vec<&UserAttachment> = attachments
        .iter()
        .filter(|attachment| attachment.kind == AttachmentKind::Image)
        .collect();
    if !image_attachments.is_empty() {
        let mut thumbnails = div().flex().flex_wrap().items_end().gap(px(6.0));
        for (index, attachment) in image_attachments.into_iter().enumerate() {
            if let Some(image) = attachment_image(view, attachment) {
                let click_path = attachment.path.clone();
                let right_path = attachment.path.clone();
                let click_base = view.markdown.base_dir().to_path_buf();
                let right_base = view.markdown.base_dir().to_path_buf();
                let click_entity = entity.clone();
                let right_entity = entity.clone();
                let element_id = SharedString::from(format!("user-image-{message_id}-{index}"));
                thumbnails = thumbnails.child(
                    img(image)
                        .id(gpui::ElementId::Name(element_id))
                        .w(px(184.0))
                        .h(px(120.0))
                        .bg(theme.surface)
                        .object_fit(ObjectFit::Contain)
                        .rounded(px(8.0))
                        .border_1()
                        .border_color(theme.border)
                        .cursor_pointer()
                        .on_click(move |event, window, cx| {
                            let path = click_path.clone();
                            let _ = click_entity.update(cx, |this, cx| {
                                this.activate_markdown_target(
                                    effibuddy_ui::MarkdownTarget::File(path.into()),
                                    gpui::MouseButton::Left,
                                    event.position(),
                                    &click_base,
                                    window,
                                    cx,
                                );
                            });
                        })
                        .on_mouse_down(gpui::MouseButton::Right, move |event, window, cx| {
                            cx.stop_propagation();
                            let path = right_path.clone();
                            let _ = right_entity.update(cx, |this, cx| {
                                this.activate_markdown_target(
                                    effibuddy_ui::MarkdownTarget::File(path.into()),
                                    gpui::MouseButton::Right,
                                    event.position,
                                    &right_base,
                                    window,
                                    cx,
                                );
                            });
                        }),
                );
            }
        }
        if !text.is_empty() {
            thumbnails = thumbnails.mb(px(8.0));
        }
        bubble = bubble.child(thumbnails);
    }
    if !attachments.is_empty() {
        let mut chips = div().flex().flex_wrap().items_center().gap(px(6.0));
        if !text.is_empty() {
            chips = chips.mb(px(8.0));
        }
        for attachment in attachments {
            let icon = match attachment.kind {
                AttachmentKind::Skill => icons::SPARKLES,
                AttachmentKind::Image | AttachmentKind::File => icons::FILE,
            };
            let tint = match attachment.kind {
                AttachmentKind::Skill => theme.agent_purple,
                AttachmentKind::Image => theme.agent_green,
                AttachmentKind::File => theme.text_muted,
            };
            chips = chips.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .h(px(24.0))
                    .px(px(7.0))
                    .rounded(px(6.0))
                    .bg(theme.surface)
                    .border_1()
                    .border_color(theme.border)
                    .child(svg().path(icon).size(px(11.0)).text_color(tint))
                    .child(
                        div()
                            .max_w(px(220.0))
                            .truncate()
                            .text_size(px(11.0))
                            .text_color(theme.text_muted)
                            .child(crate::attachments::chip_title(attachment)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(9.5))
                            .text_color(theme.text_faint)
                            .child(crate::attachments::format_bytes(attachment.size)),
                    ),
            );
        }
        bubble = bubble.child(chips);
    }
    if !text.is_empty() {
        bubble = bubble.child(text.to_string());
    }
    // Rail jump landed here: a one-shot accent ring that holds briefly and
    // fades. It is an overlay (no layout, no hitbox), so the bubble never
    // shifts and the animation cannot swallow hover actions.
    if let Some(seq) = flash_seq {
        let ring = div()
            .absolute()
            .inset_0()
            .rounded(px(12.0))
            .rounded_tr(px(2.0))
            .border_1()
            .border_color(theme.accent);
        bubble = bubble.relative().child(if reduce_motion() {
            // Reduced motion keeps a static ring; it is replaced on the next
            // jump, so nothing needs to time it out.
            ring.opacity(0.55).into_any_element()
        } else {
            ring.with_animation(
                SharedString::from(format!("prompt-jump-flash-{seq}")),
                Animation::new(Duration::from_millis(1100)).with_easing(|delta| {
                    if delta < 0.55 {
                        1.0
                    } else {
                        (1.0 - delta) / 0.45
                    }
                }),
                |element, delta| element.opacity(delta),
            )
            .into_any_element()
        });
    }
    container = container.child(bubble);
    // Steered messages ride inside a turn owned by another user message, so
    // editing/undoing them would rewind checkpoints they never had.
    let actions = actions && !steered;
    if actions {
        container = container.child(
            div()
                .flex()
                .items_center()
                .gap(px(2.0))
                .h(px(22.0))
                .mt(px(4.0))
                .opacity(0.0)
                .group_hover(group, |style| style.opacity(1.0))
                .child(
                    IconButton::new(copy_id, icons::COPY, theme)
                        .size(22.0, 12.0)
                        .tooltip("复制")
                        .on_click(move |_, _, cx| {
                            if !copy_text.is_empty() {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    copy_text.to_string(),
                                ));
                            }
                        }),
                )
                .child(
                    IconButton::new(
                        SharedString::from(format!("user-edit-{message_id}")),
                        icons::EDIT,
                        theme,
                    )
                    .size(22.0, 12.0)
                    .tooltip(edit_tip)
                    .on_click(move |_, window, cx| {
                        let _ = edit_entity.update(cx, |this, cx| {
                            this.begin_edit_message(&edit_id, window, cx);
                        });
                    }),
                )
                .child(
                    IconButton::new(
                        SharedString::from(format!("user-undo-{message_id}")),
                        icons::UNDO,
                        theme,
                    )
                    .size(22.0, 12.0)
                    .tooltip("撤销这条消息并还原文件")
                    .on_click(move |_, window, cx| {
                        let _ = undo_entity.update(cx, |this, cx| {
                            this.request_undo_message(&undo_id, window, cx);
                        });
                    }),
                ),
        );
    }
    container.into_any_element()
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
    // ZCode keeps reasoning / body / tool groups as flow items with a clear
    // step between them; 12px holds that rhythm without disconnecting the
    // pieces the way its 20px would after the chunk split.
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(12.0))
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
            render_markdown_chunk_with(
                theme,
                &streaming,
                markdown_source_key(&streaming),
                hash_values(&(assistant.message.id.as_str(), "stream")),
                Some(&view.markdown),
            )
        } else {
            render_markdown_keyed_with(
                theme,
                &assistant.text,
                assistant.text_key,
                hash_values(&(assistant.message.id.as_str(), "body")),
                Some(&view.markdown),
            )
        };
        column = column.child(markdown);
    }

    let mut column = append_tool_rows(column, assistant, view, theme, entity, None);
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
            texts.active(),
            view.expanded_reasoning.contains(&assistant.message.id),
            view,
            theme,
            entity,
        ));
    }

    if let Some(chunk) = slot_chunk(assistant, view, 0) {
        column = column.child(render_markdown_chunk_with(
            theme,
            chunk.slice(texts.text()),
            chunk.key,
            hash_values(&(assistant.message.id.as_str(), "lead")),
            Some(&view.markdown),
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
        Some(chunk) => render_markdown_chunk_with(
            theme,
            chunk.slice(texts.text()),
            chunk.key,
            hash_values(&(assistant.message.id.as_str(), chunk_ix)),
            Some(&view.markdown),
        ),
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
            render_markdown_chunk_with(
                theme,
                &streaming,
                markdown_source_key(&streaming),
                hash_values(&(message_group.as_ref(), "tail-stream")),
                Some(&view.markdown),
            )
        } else {
            render_markdown_chunk_with(
                theme,
                chunk_text,
                chunk.key,
                hash_values(&(message_group.as_ref(), "tail", chunk.key)),
                Some(&view.markdown),
            )
        };
        column = column.child(markdown);
    }

    let mut column = append_tool_rows(column, assistant, view, theme, entity, None);
    if !texts.active() && !texts.text().trim().is_empty() {
        column = column.child(render_copy_footer(assistant, &message_group, theme));
    }
    column.into_any_element()
}

// --- turn blocks -----------------------------------------------------------

/// Which member renders as the turn's answer.
///
/// Normally the last member is the answer. But a turn often ends on a
/// tool-only step (or a reasoning-only step cut by the output limit), and the
/// model's last *text* is in an earlier member. Using the literal last member
/// then produces a header followed by nothing — the answer looks like it
/// vanished when the turn grew. Fall back to the last member that has text.
/// A live stream always owns the last member: its text is still arriving and
/// the typewriter snapshot is keyed by that message id.
fn turn_body_index(turn: &TurnItem, streaming_id: Option<&str>) -> usize {
    let last = turn.members.len() - 1;
    if streaming_id.is_some_and(|id| turn.members[last].message.id == id) {
        return last;
    }
    turn.members
        .iter()
        .rposition(|member| !member.text.trim().is_empty())
        .unwrap_or(last)
}

fn turn_body<'a>(turn: &'a TurnItem, view: &ChatViewState) -> &'a AssistantItem {
    let streaming_id = view
        .stream
        .as_ref()
        .map(|stream| stream.message_id.as_str());
    &turn.members[turn_body_index(turn, streaming_id)]
}

fn turn_body_index_in_view(turn: &TurnItem, view: &ChatViewState) -> usize {
    let streaming_id = view
        .stream
        .as_ref()
        .map(|stream| stream.message_id.as_str());
    turn_body_index(turn, streaming_id)
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
    turn_expanded_with(
        turn,
        &view.collapsed_turns,
        &view.expanded_turns,
        view.running,
        view.stream
            .as_ref()
            .map(|stream| stream.message_id.as_str()),
    )
}

/// `turn_expanded` 的参数化版本：`prepare_frame` 在构造 `ChatViewState` 之前
/// 也要判断展开态（做 slot key 指纹与自动展开计时）。
fn turn_expanded_with(
    turn: &TurnItem,
    collapsed_turns: &HashSet<String>,
    expanded_turns: &HashSet<String>,
    running: bool,
    streaming_id: Option<&str>,
) -> bool {
    if collapsed_turns.contains(&turn.id) {
        return false;
    }
    let live = running
        && streaming_id.is_some_and(|streaming_id| {
            turn.members
                .iter()
                .any(|member| member.message.id == streaming_id)
        });
    expanded_turns.contains(&turn.id) || live
}

/// 影响条目渲染高度的呈现状态指纹（折叠/展开面板、reasoning/tool 行）。
/// 混进 slot key：这些状态一改，前缀对齐就断在该条目、触发 splice，让
/// ListState 重新测量它——否则视口外的高度缓存失真，折叠展开后滚动边界
/// 会漂移，出现“滚不到上下全部内容”。
fn item_presentation(
    item: &ChatItem,
    expanded_reasoning: &HashSet<String>,
    expanded_tools: &HashSet<String>,
    expanded_turns: &HashSet<String>,
    collapsed_turns: &HashSet<String>,
    running: bool,
    streaming_id: Option<&str>,
) -> u64 {
    match &item.kind {
        ChatItemKind::Turn(turn) => {
            let mut flags = vec![turn_expanded_with(
                turn,
                collapsed_turns,
                expanded_turns,
                running,
                streaming_id,
            )];
            flags.extend(
                turn.members
                    .iter()
                    .map(|member| expanded_reasoning.contains(&member.message.id)),
            );
            hash_values(&flags)
        }
        ChatItemKind::Assistant(assistant) => {
            let mut flags = vec![expanded_reasoning.contains(&assistant.message.id)];
            flags.extend(
                assistant
                    .message
                    .tool_calls
                    .iter()
                    .map(|call| expanded_tools.contains(&call.id)),
            );
            hash_values(&flags)
        }
        _ => 0,
    }
}

/// 展开内容的渐显上下文：`fresh` 只在展开窗口内为真，窗口外的重新挂载
/// （例如滚动来回）直接全亮，不重放动画；`height_progress` 驱动高度从 0
/// 长到自然高度。
#[derive(Clone, Copy)]
struct TurnReveal<'a> {
    turn_id: &'a str,
    fresh: bool,
    height_progress: f32,
}

impl TurnReveal<'_> {
    /// 单段动画实现 stagger：整段时长 = 延迟 + 渐显，缓动把延迟窗口内的
    /// 进度钳在 0。
    ///
    /// 不要退回 `with_animations` 的「零时长首段 hold + 第二段渐显」写法：
    /// GPUI 在切换动画段的那一帧会以 `delta = 1.0` 渲染新段，内容先闪一帧
    /// 全亮、下一帧又退回透明再渐显，看起来就是反复「弹入」。
    ///
    /// `key` 必须按内容块稳定（例如 `reasoning-{message_id}`），不能用位置
    /// 序号：成员增长或分块渲染切换都会让序号漂移，元素 id 一变动画状态就
    /// 从 0 重来。
    fn apply(self, element: impl IntoElement, index: usize, key: &str) -> AnyElement {
        let element = element.into_any_element();
        if !self.fresh || reduce_motion() {
            return element;
        }
        let delay = TURN_EXPAND_STAGGER * index.min(TURN_EXPAND_STAGGER_STEPS) as u32;
        div()
            .w_full()
            .child(element)
            .with_animation(
                SharedString::from(format!("turn-reveal-{}-{key}", self.turn_id)),
                Animation::new(delay + TURN_EXPAND_FADE).with_easing(stagger_opacity(delay)),
                |element, delta| element.opacity(delta),
            )
            .into_any_element()
    }
}

/// stagger 的透明度曲线：`delta` 是整段（延迟 + 渐显）的线性进度，延迟
/// 窗口内固定 0，之后 180ms ease-out 到 1。首帧（`delta = 0`）也是 0，所以
/// 块不会先闪一帧再消失。
fn stagger_opacity(delay: Duration) -> impl Fn(f32) -> f32 + 'static {
    let total = (delay + TURN_EXPAND_FADE).as_secs_f32().max(f32::EPSILON);
    let hold = delay.as_secs_f32();
    let fade = TURN_EXPAND_FADE.as_secs_f32().max(f32::EPSILON);
    move |delta| ease_out_cubic(((delta * total - hold) / fade).clamp(0.0, 1.0))
}

/// 展开内容的高度渐显容器。
///
/// GPUI 没有 height transition，但列表每帧都会重新测量可见项，只要逐帧
/// 给出高度，下方内容就会跟着真实布局平滑让位。做法：外层裁剪容器高度取
/// 「进度 × 自然高度」，内层仍按自然高度布局（flex_none 不被压缩，超出的
/// 部分被裁掉——hitbox 与 content mask 求交，裁掉区域也不会接住点击）。
/// 内层里填充满块的 canvas 把自然高度写回缓存，下一帧据此计算显示高度。
/// 窗口结束后直接回到自然布局，不留包装。
fn reveal_height_block(
    block: impl IntoElement,
    reveal: TurnReveal,
    suffix: &str,
    heights: &Rc<RefCell<HashMap<String, f32>>>,
) -> AnyElement {
    if !reveal.fresh || reduce_motion() {
        return block.into_any_element();
    }
    let key = format!("{}#{suffix}", reveal.turn_id);
    let visible_height =
        heights.borrow().get(&key).copied().unwrap_or_default() * reveal.height_progress;
    let heights = Rc::clone(heights);
    div()
        .w_full()
        .h(px(visible_height))
        .overflow_hidden()
        .child(
            div().relative().w_full().flex_none().child(block).child(
                canvas(
                    move |bounds, _window, _cx| {
                        heights
                            .borrow_mut()
                            .insert(key.clone(), f32::from(bounds.size.height));
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .inset_0(),
            ),
        )
        .into_any_element()
}

/// 该 turn 当前是否需要播展开渐显（窗口由 `note_turn_expanded` 打点）。
fn turn_reveal_context<'a>(turn: &'a TurnItem, view: &ChatViewState) -> TurnReveal<'a> {
    let Some(at) = view.turn_expand_at.get(&turn.id) else {
        return TurnReveal {
            turn_id: &turn.id,
            fresh: false,
            height_progress: 1.0,
        };
    };
    let elapsed = at.elapsed();
    let height_progress = if reduce_motion() {
        1.0
    } else {
        ease_out_cubic((elapsed.as_secs_f32() / TURN_HEIGHT_REVEAL.as_secs_f32()).min(1.0))
    };
    TurnReveal {
        turn_id: &turn.id,
        fresh: elapsed < TURN_EXPAND_WINDOW,
        height_progress,
    }
}

/// Aggregate status bar of a turn block, ZCode-style: a full-width
/// 「已工作 {duration}」 strip above a bottom hairline, with the activity
/// counts trailing and the chevron revealed on hover. Clicking it records the
/// explicit fold choice.
fn render_turn_header(
    turn: &TurnItem,
    expanded: bool,
    view: &ChatViewState,
    theme: &Theme,
    entity: &WeakEntity<AppView>,
) -> AnyElement {
    let live = view.running && turn_is_live(turn, view);
    let unresolved = turn.members.iter().any(|member| {
        member
            .message
            .tool_calls
            .iter()
            .any(|call| !view.results.contains_key(&call.id))
    });

    let mut label = if live {
        if turn.total_ms > 0 {
            format!("工作中 · {}", format_duration_ms(turn.total_ms))
        } else {
            "工作中".to_string()
        }
    } else if turn.total_ms > 0 {
        format!("已工作 {}", format_duration_ms(turn.total_ms))
    } else {
        String::new()
    };
    if turn.thinking_count > 0 {
        if !label.is_empty() {
            label.push_str(" · ");
        }
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
        label.push_str(if live { "工作中" } else { "已完成" });
    }

    let turn_id = turn.id.clone();
    let toggle_entity = entity.clone();
    let row_group = SharedString::from(format!("turn-row-{}", turn.id));
    // 可展开只靠文字提亮暗示：静息用 muted，悬停留意行内文字变亮，
    // 不再铺一层背景色块。
    let label_hover = if turn.any_error {
        theme.danger_hover
    } else {
        theme.text
    };
    let label_element = if live && !unresolved {
        div()
            .min_w(px(0.0))
            .truncate()
            .text_size(px(13.0))
            .text_color(theme.text_muted)
            .group_hover(row_group.clone(), move |style| {
                style.text_color(label_hover)
            })
            .child(label)
            .with_animation(
                SharedString::from(format!("turn-pulse-{}", turn.id)),
                Animation::new(Duration::from_millis(1600)).repeat(),
                |element, delta| element.opacity(pulsating_between(0.45, 1.0)(delta)),
            )
            .into_any_element()
    } else {
        div()
            .min_w(px(0.0))
            .truncate()
            .text_size(px(13.0))
            .text_color(if turn.any_error {
                theme.danger
            } else {
                theme.text_muted
            })
            .group_hover(row_group.clone(), move |style| {
                style.text_color(label_hover)
            })
            .child(label)
            .into_any_element()
    };

    div()
        .w_full()
        .flex()
        .flex_col()
        // 悬停不铺背景：只有状态文字提亮，暗示整行可点开。
        .child(
            div()
                .id(SharedString::from(format!("turn-head-{}", turn.id)))
                .group(row_group.clone())
                .flex()
                .items_center()
                .gap(px(8.0))
                .py(px(4.0))
                .cursor_pointer()
                .on_click(move |_, _, cx| {
                    let turn_id = turn_id.clone();
                    let _ = toggle_entity.update(cx, |this, cx| {
                        this.toggle_turn(&turn_id, expanded);
                        cx.notify();
                    });
                })
                .when(turn.any_error, |element| {
                    element.child(
                        svg()
                            .path(icons::ALERT)
                            .size(px(13.0))
                            .text_color(theme.danger)
                            .flex_none(),
                    )
                })
                .when(!turn.any_error && (live || unresolved), |element| {
                    element.child(
                        div()
                            .flex_none()
                            .child(Spinner::new(theme.text_faint, 12.0)),
                    )
                })
                .child(label_element)
                // 折叠箭头跟在状态文字右侧（与左栏折叠尖一致），悬停才出现；
                // 之后才是弹性留白。
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .opacity(0.0)
                        .group_hover(row_group, |style| style.opacity(1.0))
                        .child(disclosure_chevron(
                            SharedString::from(format!("turn-chevron-{}", turn.id)),
                            expanded,
                            theme.text_muted,
                            12.0,
                        )),
                )
                .child(div().flex_1()),
        )
        .child(div().h(px(1.0)).w_full().mt(px(3.0)).bg(theme.border))
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
    reveal: TurnReveal,
    reveal_ix: &mut usize,
) -> Div {
    let mut column = column;
    if !member.reasoning.is_empty() {
        let reasoning = render_reasoning(
            &member.message.id,
            &member.reasoning,
            member.reasoning_duration,
            false,
            view.expanded_reasoning.contains(&member.message.id),
            view,
            theme,
            entity,
        );
        column = column.child(reveal.apply(
            reasoning,
            *reveal_ix,
            &format!("reasoning-{}", member.message.id),
        ));
        *reveal_ix += 1;
    }
    if !member.text.trim().is_empty() {
        let text_element = div().opacity(0.72).child(render_markdown_keyed_with(
            theme,
            &member.text,
            member.text_key,
            hash_values(&(member.message.id.as_str(), "member")),
            Some(&view.markdown),
        ));
        column = column.child(reveal.apply(
            text_element,
            *reveal_ix,
            &format!("text-{}", member.message.id),
        ));
        *reveal_ix += 1;
    }
    append_tool_rows(
        column,
        member,
        view,
        theme,
        entity,
        Some((reveal, *reveal_ix)),
    )
}

/// The body member's reasoning row; only mounted while the activity is
/// expanded so a folded turn costs one header row.
fn render_turn_body_reasoning(
    column: Div,
    body: &AssistantItem,
    view: &ChatViewState,
    theme: &Theme,
    entity: &WeakEntity<AppView>,
    reveal: TurnReveal,
    reveal_ix: &mut usize,
) -> Div {
    let texts = assistant_texts(body, view);
    if texts.reasoning().is_empty() {
        return column;
    }
    let reasoning = render_reasoning(
        &body.message.id,
        texts.reasoning(),
        body.reasoning_duration,
        texts.active(),
        view.expanded_reasoning.contains(&body.message.id),
        view,
        theme,
        entity,
    );
    let child = reveal.apply(
        reasoning,
        *reveal_ix,
        &format!("reasoning-{}", body.message.id),
    );
    *reveal_ix += 1;
    column.child(child)
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
    let body_ix = turn_body_index_in_view(turn, view);
    let body = &turn.members[body_ix];
    let texts = assistant_texts(body, view);
    let message_group = turn_message_group(turn);
    let mut column = assistant_column(&message_group);
    let reveal = turn_reveal_context(turn, view);
    let mut reveal_ix = 0;

    column = column.child(render_turn_header(turn, expanded, view, theme, entity));
    if expanded {
        // 活动栈整体放进高度渐显容器：展开时高度逐帧长高，答案文字与下方
        // 内容随真实布局平滑下移，而不是被瞬间撑开的一大截顶飞。
        let mut activity = div().w_full().flex().flex_col().gap(px(12.0));
        for (index, member) in turn.members.iter().enumerate() {
            if index == body_ix {
                continue;
            }
            activity = render_turn_member(
                activity,
                member,
                view,
                theme,
                entity,
                reveal,
                &mut reveal_ix,
            );
        }
        activity =
            render_turn_body_reasoning(activity, body, view, theme, entity, reveal, &mut reveal_ix);
        column = column.child(reveal_height_block(
            activity,
            reveal,
            "activity",
            &view.turn_block_heights,
        ));
    }

    let has_text = !texts.text().trim().is_empty();
    if has_text || texts.active() {
        let markdown = if texts.active() {
            let streaming = with_stream_caret(texts.text());
            render_markdown_streaming_with(
                theme,
                &streaming,
                hash_values(&body.message.id),
                Some(&view.markdown),
            )
        } else {
            render_markdown_keyed_with(
                theme,
                &body.text,
                body.text_key,
                hash_values(&(body.message.id.as_str(), "turn-body")),
                Some(&view.markdown),
            )
        };
        column = column.child(markdown);
    }

    if expanded {
        column = append_tool_rows(column, body, view, theme, entity, Some((reveal, reveal_ix)));
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
    let body_ix = turn_body_index_in_view(turn, view);
    let body = &turn.members[body_ix];
    let texts = assistant_texts(body, view);
    let message_group = turn_message_group(turn);
    let mut column = assistant_column(&message_group);
    let reveal = turn_reveal_context(turn, view);
    let mut reveal_ix = 0;

    column = column.child(render_turn_header(turn, expanded, view, theme, entity));
    if expanded {
        let mut activity = div().w_full().flex().flex_col().gap(px(12.0));
        for (index, member) in turn.members.iter().enumerate() {
            if index == body_ix {
                continue;
            }
            activity = render_turn_member(
                activity,
                member,
                view,
                theme,
                entity,
                reveal,
                &mut reveal_ix,
            );
        }
        activity =
            render_turn_body_reasoning(activity, body, view, theme, entity, reveal, &mut reveal_ix);
        column = column.child(reveal_height_block(
            activity,
            reveal,
            "activity",
            &view.turn_block_heights,
        ));
    }
    if let Some(chunk) = slot_chunk(body, view, 0) {
        column = column.child(render_markdown_chunk_with(
            theme,
            chunk.slice(texts.text()),
            chunk.key,
            hash_values(&(body.message.id.as_str(), "lead")),
            Some(&view.markdown),
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
    let body = turn_body(turn, view);
    let texts = assistant_texts(body, view);
    let message_group = turn_message_group(turn);
    let mut column = assistant_column(&message_group);
    let reveal = turn_reveal_context(turn, view);

    let last = slot_chunks(body, view).and_then(|chunks| chunks.last());
    if let Some(chunk) = last {
        let chunk_text = chunk.slice(texts.text());
        let markdown = if texts.active() {
            let streaming = with_stream_caret(chunk_text);
            render_markdown_chunk_with(
                theme,
                &streaming,
                markdown_source_key(&streaming),
                hash_values(&(message_group.as_ref(), "tail-stream")),
                Some(&view.markdown),
            )
        } else {
            render_markdown_chunk_with(
                theme,
                chunk_text,
                chunk.key,
                hash_values(&(message_group.as_ref(), "tail", chunk.key)),
                Some(&view.markdown),
            )
        };
        column = column.child(markdown);
    }

    if expanded {
        column = append_tool_rows(column, body, view, theme, entity, Some((reveal, 0)));
    }
    if !texts.active() && !texts.text().trim().is_empty() {
        column = column.child(render_copy_footer(body, &message_group, theme));
    }
    column.into_any_element()
}

/// Tool-call rows (agent cards, groups, singles) shared by every assistant
/// row variant. They stack into one tight group container (ZCode's `gap-1`
/// work-items rhythm) instead of inheriting the looser column spacing.
fn append_tool_rows(
    column: Div,
    assistant: &AssistantItem,
    view: &ChatViewState,
    theme: &Theme,
    entity: &WeakEntity<AppView>,
    reveal: Option<(TurnReveal, usize)>,
) -> Div {
    let message = &assistant.message;
    let calls: Vec<&ToolCall> = message.tool_calls.iter().collect();
    let mut rows: Vec<AnyElement> = Vec::new();
    let mut cursor = 0;
    while cursor < calls.len() {
        let call = calls[cursor];
        if call.name == "agent" {
            let result = view.results.get(&call.id).map(Rc::as_ref);
            let card = view.agents.get(&call.id);
            rows.push(render_agent_row(call, result, card, theme, entity));
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
            rows.push(render_tool_group(
                &group_id, run, view, expanded, theme, entity,
            ));
        } else {
            for call in run {
                let result = view.results.get(&call.id).map(Rc::as_ref);
                let expanded = view.expanded_tools.contains(&call.id);
                rows.push(render_tool_row(call, result, view, expanded, theme, entity));
            }
        }
    }
    if rows.is_empty() {
        return column;
    }
    let group = div().w_full().flex().flex_col().gap(px(4.0)).children(rows);
    match reveal {
        // 每个成员的工具组各有自己的自然高度：挂到同一个缓存键会让后画的
        // 组覆盖前一组，展开窗口内出现裁切/高度跳变。
        Some((reveal, index)) => {
            let key = format!("tools-{}", message.id);
            column.child(reveal.apply(
                reveal_height_block(group, reveal, &key, &view.turn_block_heights),
                index,
                &key,
            ))
        }
        None => column.child(group),
    }
}

/// Hover-revealed copy button plus the ZCode-style timestamp, shared by
/// assistant rows.
fn render_copy_footer(
    assistant: &AssistantItem,
    message_group: &SharedString,
    theme: &Theme,
) -> AnyElement {
    let message = &assistant.message;
    let copy_id = SharedString::from(format!("copy-{}", message.id));
    let copy_text = assistant.text.clone();
    let time_label = message_time_label(message.created_at);
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
                .text_size(px(11.5))
                .text_color(theme.text_faint)
                .child(time_label),
        )
        .when(message.interrupted, |element| {
            element.child(badge(theme, "已中断", BadgeTone::Warning))
        })
        .into_any_element()
}

/// ZCode message timestamp: local clock, `HH:mm` today, `M/D HH:mm` within
/// the year, and a full date once the year differs.
fn message_time_label(created_at: chrono::DateTime<chrono::Utc>) -> String {
    use chrono::{Datelike, Duration, Local, TimeZone};
    let local = Local.from_utc_datetime(&created_at.naive_utc());
    let now = Local::now();
    let today = now.date_naive();
    if local.date_naive() == today {
        local.format("%H:%M").to_string()
    } else if local.date_naive() == today - Duration::try_days(1).unwrap_or_default() {
        local.format("昨天 %H:%M").to_string()
    } else if local.year() == now.year() {
        local.format("%-m/%-d %H:%M").to_string()
    } else {
        local.format("%Y/%-m/%-d %H:%M").to_string()
    }
}

#[allow(clippy::too_many_arguments)]
fn render_reasoning(
    message_id: &str,
    reasoning: &str,
    duration: Option<Duration>,
    active: bool,
    expanded: bool,
    view: &ChatViewState,
    theme: &Theme,
    entity: &WeakEntity<AppView>,
) -> AnyElement {
    let live = active;
    // ZCode wording: 「正在思考」 while streaming (no clock in the row), the
    // measured duration once done, second-granularity like ZCode's ceil().
    let label = if live {
        "正在思考".to_string()
    } else if let Some(duration) = duration {
        let seconds = duration.as_secs().max(1);
        format!("思考 · 持续了 {seconds} 秒")
    } else {
        "思考".to_string()
    };

    let toggle_id = message_id.to_string();
    let toggle_entity = entity.clone();
    let row = div()
        .id(SharedString::from(format!("reasoning-{}", message_id)))
        .flex()
        .items_center()
        .gap(px(8.0))
        .py(px(2.0))
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
        .child(
            svg()
                .path(icons::SPARKLES)
                .size(px(13.0))
                .text_color(theme.text_faint)
                .flex_none(),
        )
        .child(if live {
            div()
                .flex_none()
                .text_size(px(13.0))
                .text_color(theme.text_muted)
                .child(label)
                .with_animation(
                    SharedString::from(format!("reasoning-pulse-{message_id}")),
                    Animation::new(Duration::from_millis(1600)).repeat(),
                    |element, delta| element.opacity(pulsating_between(0.45, 1.0)(delta)),
                )
                .into_any_element()
        } else {
            div()
                .flex_none()
                .text_size(px(13.0))
                .text_color(theme.text_muted)
                .child(label)
                .into_any_element()
        })
        .when(live && !expanded, |element| {
            element.child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(11.5))
                    .text_color(theme.text_faint)
                    .opacity(0.8)
                    .child(latest_reasoning_line(reasoning)),
            )
        })
        .when(!(live && !expanded), |element| {
            element.child(div().flex_1())
        })
        .child(disclosure_chevron(
            SharedString::from(format!("reasoning-chevron-{message_id}")),
            expanded,
            theme.text_faint,
            12.0,
        ));

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
            .relative()
            .ml(px(8.0))
            .pl(px(12.0))
            .border_l_1()
            .border_color(theme.border)
            .max_h(px(240.0))
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
            // The canvas must be absolutely laid over the body: as a plain
            // child it gets zero height at the bottom of the column, so
            // `bounds.contains` never matched and the wheel fell through to
            // the chat list instead of scrolling the reasoning text.
            .child(
                wheel_claim_overlay(scroll_handle, entity.clone())
                    .absolute()
                    .inset_0(),
            )
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

/// Tool call summary row, ZCode-style: one subtle line whose verb carries the
/// state (「正在读取」→「已读取」, pulsing while running), with the chevron
/// revealed on hover and the expandable detail carded below the row.
fn render_tool_row(
    call: &ToolCall,
    result: Option<&ToolResult>,
    view: &ChatViewState,
    expanded: bool,
    theme: &Theme,
    entity: &WeakEntity<AppView>,
) -> AnyElement {
    let running = view.running;
    let failed = result.is_some_and(|result| result.is_error);
    let pending = result.is_none() && running;
    let label = tool_action_label(&call.name, result, running);
    let detail = {
        let detail = tool_detail(call);
        match result {
            Some(result) if !result.images.is_empty() => {
                format!("{detail} · {} 张图片", result.images.len())
            }
            _ => detail,
        }
    };
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
    // File-tool rows double as artifact links: the path is visible and a
    // right-click opens the same open/save/copy/quote menu as a Markdown link.
    let file_path = matches!(
        call.name.as_str(),
        "read_file" | "write_file" | "edit_file" | "list_dir"
    )
    .then(|| {
        call.arguments
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    })
    .flatten();

    // ZCode keeps the tool icon static and expresses "running" through the
    // shimmering label; the pulse animation stands in for the shimmer here.
    let row_group = SharedString::from(format!("tool-row-{}", call.id));
    let label_element = if pending {
        div()
            .flex_none()
            .text_size(px(13.0))
            .text_color(theme.text_muted)
            .child(label)
            .with_animation(
                SharedString::from(format!("tool-pulse-{}", call.id)),
                Animation::new(Duration::from_millis(1600)).repeat(),
                |element, delta| element.opacity(pulsating_between(0.45, 1.0)(delta)),
            )
            .into_any_element()
    } else {
        div()
            .flex_none()
            .text_size(px(13.0))
            .text_color(if failed {
                theme.danger
            } else {
                theme.text_muted
            })
            .child(label)
            .into_any_element()
    };

    div()
        .w_full()
        .child(
            div()
                .id(SharedString::from(format!("tool-{}", call.id)))
                .group(row_group.clone())
                .flex()
                .items_center()
                .gap(px(8.0))
                .py(px(2.0))
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
                .when_some(file_path, |element, path| {
                    let entity = entity.clone();
                    let base_dir = view.markdown.base_dir().to_path_buf();
                    element.on_mouse_down(gpui::MouseButton::Right, move |event, window, cx| {
                        cx.stop_propagation();
                        let path = path.clone();
                        let _ = entity.update(cx, |this, cx| {
                            this.activate_markdown_target(
                                effibuddy_ui::MarkdownTarget::File(path.into()),
                                gpui::MouseButton::Right,
                                event.position,
                                &base_dir,
                                window,
                                cx,
                            );
                        });
                    })
                })
                .child(
                    svg()
                        .path(tool_icon(&call.name))
                        .size(px(13.0))
                        .text_color(theme.text_faint)
                        .flex_none(),
                )
                .child(label_element)
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(12.0))
                        .text_color(theme.text_faint)
                        .child(detail),
                )
                .child(
                    div()
                        .flex_none()
                        .opacity(0.0)
                        .group_hover(row_group, |style| style.opacity(1.0))
                        .child(disclosure_chevron(
                            SharedString::from(format!("tool-chevron-{}", call.id)),
                            expanded,
                            theme.text_faint,
                            12.0,
                        )),
                ),
        )
        .when(expanded, |element| {
            element.child(
                div()
                    .mt(px(2.0))
                    .mb(px(4.0))
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
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
                    })
                    .when_some(
                        result.filter(|result| !result.images.is_empty()),
                        |element, result| {
                            element.child(render_tool_images(theme, &call.id, result, view))
                        },
                    ),
            )
        })
        .into_any_element()
}

/// Consecutive tool calls collapse into one ZCode-style summary line
/// (「已读取 3 项，已执行 2 命令」). Expanding it reveals the individual rows
/// on a vertical rail, which keeps a long agent turn readable without hiding
/// evidence.
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
    let summary = tool_group_stats(calls, any_running);

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

    // Like single rows: a static icon, a stat line that pulses while any call
    // is still running, and a hover-revealed chevron.
    let row_group = SharedString::from(format!("tool-group-row-{group_id}"));
    let stat_element = if any_running && !any_error {
        div()
            .flex_none()
            .text_size(px(13.0))
            .text_color(theme.text_muted)
            .child(summary)
            .with_animation(
                SharedString::from(format!("tool-group-pulse-{group_id}")),
                Animation::new(Duration::from_millis(1600)).repeat(),
                |element, delta| element.opacity(pulsating_between(0.45, 1.0)(delta)),
            )
            .into_any_element()
    } else {
        div()
            .flex_none()
            .text_size(px(13.0))
            .text_color(if any_error {
                theme.danger
            } else {
                theme.text_muted
            })
            .child(summary)
            .into_any_element()
    };

    div()
        .w_full()
        .child(
            div()
                .id(SharedString::from(format!("tool-group-{group_id}")))
                .group(row_group.clone())
                .flex()
                .items_center()
                .gap(px(8.0))
                .py(px(2.0))
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
                .child(
                    svg()
                        .path(if any_error {
                            icons::ALERT
                        } else {
                            icons::WRENCH
                        })
                        .size(px(13.0))
                        .text_color(if any_error {
                            theme.danger
                        } else {
                            theme.text_faint
                        })
                        .flex_none(),
                )
                .child(stat_element)
                .child(div().flex_1())
                .child(
                    div()
                        .flex_none()
                        .opacity(0.0)
                        .group_hover(row_group, |style| style.opacity(1.0))
                        .child(disclosure_chevron(
                            SharedString::from(format!("tool-group-chevron-{group_id}")),
                            expanded,
                            theme.text_faint,
                            12.0,
                        )),
                ),
        )
        .when(expanded, |element| {
            element.child(
                div()
                    .mt(px(2.0))
                    .mb(px(4.0))
                    .ml(px(8.0))
                    .pl(px(12.0))
                    .border_l_1()
                    .border_color(theme.border)
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .children(rail),
            )
        })
        .into_any_element()
}

/// ZCode summary-group stats: per-verb counts joined with 「，」, past-tense
/// once nothing is pending (「查阅 3 项」→「已查阅 3 项」).
fn tool_group_stats(calls: &[&ToolCall], any_running: bool) -> String {
    let mut stats: Vec<(&'static str, &'static str, usize)> = Vec::new();
    for call in calls {
        let verb = tool_verb(&call.name);
        let unit = tool_unit(&call.name);
        if let Some(entry) = stats.iter_mut().find(|(v, u, _)| *v == verb && *u == unit) {
            entry.2 += 1;
        } else {
            stats.push((verb, unit, 1));
        }
    }
    let prefix = if any_running { "" } else { "已" };
    stats
        .into_iter()
        .map(|(verb, unit, count)| format!("{prefix}{verb} {count} {unit}"))
        .collect::<Vec<_>>()
        .join("，")
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

/// The left-edge query navigator: one short bar per user prompt, a widened
/// bar plus preview card on hover, and a click that jumps to that query.
///
/// Only rows intersecting the rail viewport are built, so a long session
/// renders O(visible) bars per frame; the scroll position comes from the list's
/// logical top and the jump target from `first_slots`, both O(1).
pub(crate) fn render_prompt_rail(
    theme: &Theme,
    marks: &[PromptMark],
    state: &PromptRailState,
    metrics: &PromptRailMetrics,
    active: usize,
    entity: &WeakEntity<AppView>,
) -> AnyElement {
    let scroll = state.scroll;
    let first = (((scroll / PROMPT_RAIL_ROW).floor() as isize) - 1).max(0) as usize;
    let last =
        ((((scroll + metrics.view_height) / PROMPT_RAIL_ROW).ceil() as usize) + 2).min(marks.len());

    let leave_entity = entity.clone();
    let mut rail = div()
        .id("prompt-rail")
        .absolute()
        .left(px(14.0))
        // The interactive root hugs the visible rows (centered when the
        // transcript has few queries); its hover hitbox must not cover the
        // empty space above/below the rail.
        .top(px(PROMPT_RAIL_INSET + metrics.pad))
        .w(px(PROMPT_RAIL_WIDTH))
        .h(px(metrics.view_height))
        .relative()
        // Row leave events are intentionally ignored: moving between rows must
        // not clear the newly entered one (enter/leave order is not ordered by
        // row). Only leaving the whole rail clears the hover state.
        .on_hover(move |hovered: &bool, _, cx| {
            if !*hovered {
                let _ = leave_entity.update(cx, |this, cx| {
                    this.prompt_rail_hover(None, cx);
                });
            }
        });

    for (index, mark) in marks.iter().enumerate().take(last).skip(first) {
        let y = index as f32 * PROMPT_RAIL_ROW - scroll;
        if y + PROMPT_RAIL_ROW <= 0.0 || y >= metrics.view_height {
            continue;
        }
        let hovered = state.hover == Some(index);
        let is_active = index == active;
        let color = if hovered {
            theme.text
        } else if is_active || mark.running {
            theme.text_muted
        } else {
            with_alpha(theme.text_faint, 0.55)
        };
        // Every bar rests at the same width; only hover widens one. Active and
        // running queries are distinguished by color/opacity instead.
        let idle = PROMPT_LINE_IDLE;
        let target = if hovered { PROMPT_LINE_HOVER } else { idle };
        // A row that was hovered last frame starts wide so leaving animates
        // back down; every other row starts and ends at its resting width.
        let start = if hovered {
            idle
        } else if state.previous_hover == Some(index) {
            PROMPT_LINE_HOVER
        } else {
            target
        };

        let line = div().h(px(2.0)).rounded(px(999.0)).bg(color).w(px(target));
        let line = if reduce_motion() {
            line.into_any_element()
        } else {
            line.with_animation(
                SharedString::from(format!("prompt-rail-line-{}-{}", mark.id, state.hover_seq)),
                Animation::new(PROMPT_LINE_MOTION).with_easing(ease_out_cubic),
                move |element, delta| element.w(px(start + (target - start) * delta)),
            )
            .into_any_element()
        };
        // The newest query of a live run breathes, matching the activity rows
        // in the transcript.
        let line = if mark.running && !reduce_motion() {
            div()
                .child(line)
                .with_animation(
                    SharedString::from(format!("prompt-rail-pulse-{}", mark.id)),
                    Animation::new(Duration::from_millis(1600)).repeat(),
                    |element, delta| element.opacity(pulsating_between(0.45, 1.0)(delta)),
                )
                .into_any_element()
        } else {
            line
        };

        let hover_entity = entity.clone();
        let click_entity = entity.clone();
        rail = rail.child(
            div()
                .id(SharedString::from(format!("prompt-rail-row-{}", mark.id)))
                .absolute()
                .left(px(0.0))
                .top(px(y))
                .w(px(PROMPT_RAIL_WIDTH))
                .h(px(PROMPT_RAIL_ROW))
                .flex()
                .items_center()
                .cursor_pointer()
                .on_hover(move |hovered: &bool, _, cx| {
                    if *hovered {
                        let _ = hover_entity.update(cx, |this, cx| {
                            this.prompt_rail_hover(Some(index), cx);
                        });
                    }
                })
                .on_click(move |_, _, cx| {
                    let _ = click_entity.update(cx, |this, cx| {
                        this.prompt_rail_click(index, cx);
                    });
                })
                .child(line),
        );
    }

    // Painted inside the rail so its capture listener runs before the chat
    // interceptor (which is painted after the rail); it claims the wheel only
    // when the rail can actually move.
    rail = rail.child(
        prompt_rail_wheel_overlay(entity.clone())
            .absolute()
            .inset_0(),
    );

    let mut wrapper = div().absolute().inset_0().child(rail);
    if let Some(card_ix) = state.card.filter(|index| *index < marks.len()) {
        wrapper = wrapper.child(render_prompt_rail_card(
            theme, marks, state, metrics, card_ix,
        ));
    }
    wrapper.into_any_element()
}

/// Hover card for one query: the user prompt (two lines) above the flattened
/// reply preview (three lines), anchored beside the hovered bar.
fn render_prompt_rail_card(
    theme: &Theme,
    marks: &[PromptMark],
    state: &PromptRailState,
    metrics: &PromptRailMetrics,
    card_ix: usize,
) -> AnyElement {
    let mark = &marks[card_ix];
    let row_center = PROMPT_RAIL_INSET + metrics.pad + card_ix as f32 * PROMPT_RAIL_ROW
        - state.scroll
        + PROMPT_RAIL_ROW * 0.5;
    let top = (row_center - PROMPT_RAIL_POPOVER_ESTIMATE * 0.5).clamp(
        8.0,
        (metrics.body_height - PROMPT_RAIL_POPOVER_ESTIMATE - 8.0).max(8.0),
    );
    let assistant = if mark.assistant_placeholder {
        if mark.running {
            "助手仍在工作"
        } else {
            "暂无助手正文"
        }
    } else {
        mark.assistant_preview.as_ref()
    };
    let card = div()
        .absolute()
        .left(px(56.0))
        .top(px(top))
        .w(px(PROMPT_RAIL_POPOVER_WIDTH))
        .p(px(12.0))
        .rounded(px(10.0))
        .border_1()
        .border_color(theme.border_strong)
        .bg(theme.elevated)
        .shadow(shadow_float(theme))
        // The card is informational, but it must not let a click fall through
        // to a hover action on the message underneath it.
        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(
            div()
                .w_full()
                .line_clamp(2)
                .text_size(px(13.0))
                .line_height(px(20.0))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.text)
                .child(mark.user_preview.clone()),
        )
        .child(
            div()
                .w_full()
                .mt(px(8.0))
                .line_clamp(3)
                .text_size(px(13.0))
                .line_height(px(20.0))
                .text_color(if mark.assistant_placeholder {
                    theme.text_faint
                } else {
                    theme.text_muted
                })
                .child(assistant.to_string()),
        );
    if reduce_motion() {
        card.into_any_element()
    } else {
        card.with_animation(
            SharedString::from(format!("prompt-rail-card-{}-{}", card_ix, state.hover_seq)),
            Animation::new(PROMPT_LINE_MOTION).with_easing(ease_out_cubic),
            |element, delta| element.opacity(delta),
        )
        .into_any_element()
    }
}

fn prompt_rail_wheel_overlay(entity: WeakEntity<AppView>) -> Canvas<&'static str> {
    wheel_capture_overlay(move |_pixel_x, pixel_y, _window, cx| {
        entity
            .update(cx, |this, cx| this.prompt_rail_wheel(pixel_y, cx))
            .unwrap_or(false)
    })
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

/// Next vertical offset for a nested scrollable from one wheel notch.
///
/// GPUI reports wheel deltas as "positive = up", while a `ScrollHandle` offset
/// is negative once the content is scrolled down, so the delta is added and
/// the result clamped to `[-max, 0]`.
fn nested_scroll_y(current_y: f32, pixel_y: f32, max: f32) -> f32 {
    (current_y + pixel_y).clamp(-max, 0.0)
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

fn tool_verb(name: &str) -> &'static str {
    match name {
        "read_file" => "读取",
        "write_file" => "写入",
        "edit_file" => "编辑",
        "list_dir" => "浏览",
        "grep" => "搜索",
        "webfetch" => "抓取",
        "shell" => "执行",
        "todo_write" => "计划",
        "run_js" => "运行",
        _ => "调用",
    }
}

/// Counting unit used by the ZCode-style group stats line ("已读取 3 项").
fn tool_unit(name: &str) -> &'static str {
    match name {
        "read_file" | "list_dir" => "项",
        "shell" => "命令",
        "write_file" | "edit_file" => "个文件",
        _ => "次",
    }
}

/// Status folded into the tool verb, ZCode-style: 「正在读取」→「已读取」,
/// failures degrade to the plain status word.
fn tool_action_label(name: &str, result: Option<&ToolResult>, running: bool) -> String {
    if result.is_some_and(|result| result.is_error) {
        return "执行失败".to_string();
    }
    let verb = tool_verb(name);
    if result.is_some() {
        return format!("已{verb}");
    }
    if running {
        return match verb {
            "执行" => "执行中".to_string(),
            other => format!("正在{other}"),
        };
    }
    "等待中".to_string()
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
        "webfetch" => icons::GLOBE,
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
        "webfetch" => arguments
            .get("url")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
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

/// A decoded raster the transcript may still paint.
///
/// `Image` only carries the *compressed* payload, so the number that matters
/// (the RGBA sprite the renderer uploads) has to come from the header. A 1080p
/// screenshot is ~8 MB decoded, and the cache used to be capped at 16
/// *entries* — i.e. it could pin ~128 MB of anonymous heap for pictures nobody
/// was looking at, with a FIFO eviction that had no idea how big anything was.
struct DecodedImage {
    image: Option<Arc<Image>>,
    /// Estimated decompressed footprint: width × height × 4.
    pixel_bytes: u64,
    /// Last use, for LRU eviction.
    used: u64,
}

/// Rasters above this are shown through a downscaled thumbnail instead of being
/// handed to the renderer at full size (2 MB ≈ 700×700).
const THUMBNAIL_MIN_PIXEL_BYTES: u64 = 2 * 1024 * 1024;
/// Accounting charge for a resident thumbnail: 512×512×4, its upper bound.
const THUMBNAIL_PIXEL_BYTES: u64 = 1024 * 1024;

/// Work queue for rasters that need a downscaled copy before they can be drawn.
///
/// The chat list can only take `&ChatViewState` while building rows, so it
/// records the request here (shared through an `Rc`, like the image cache) and
/// the host drains it in `render`, where a background task can be spawned.
#[derive(Default)]
pub(crate) struct ThumbnailState {
    /// Keys already requested, so a 300 Hz frame cannot queue the same work
    /// twice while the first one is still decoding.
    pending: HashSet<String>,
    requests: Vec<ThumbnailRequest>,
    /// Keys whose downscale failed (a format the image decoder refuses). Those
    /// render at full size, exactly like before thumbnails existed.
    unavailable: HashSet<String>,
}

pub(crate) struct ThumbnailRequest {
    pub(crate) session_id: String,
    pub(crate) key: String,
    /// Base64 payload, owned so the worker never touches the transcript.
    pub(crate) data: String,
}

enum Thumb {
    /// A cached thumbnail, ready to draw.
    Ready(Vec<u8>),
    /// Not built yet: keep this frame cheap and ask the host to build one.
    Pending,
    /// No thumbnail is possible; draw the raster itself.
    Unavailable,
}

impl ThumbnailState {
    fn thumbnail(&mut self, session_id: &str, key: &str, data: &str) -> Thumb {
        if self.unavailable.contains(key) {
            return Thumb::Unavailable;
        }
        if let Some(png) = crate::thumbs::cached(session_id, key) {
            return Thumb::Ready(png);
        }
        if self.pending.insert(key.to_string()) {
            self.requests.push(ThumbnailRequest {
                session_id: session_id.to_string(),
                key: key.to_string(),
                data: data.to_string(),
            });
        }
        Thumb::Pending
    }

    fn mark_ready(&mut self, key: &str) {
        self.pending.remove(key);
    }

    fn mark_unavailable(&mut self, key: &str) {
        self.pending.remove(key);
        self.unavailable.insert(key.to_string());
    }

    /// The transcript leaving the screen owns the queue's entries.
    fn cancel_pending(&mut self) {
        self.pending.clear();
        self.requests.clear();
    }
}

/// Budget for decoded rasters. Chosen so a couple of full-screen screenshots
/// plus a few thumbnails stay resident, but a long computer-use run cannot
/// accumulate dozens of them.
const MAX_DECODED_IMAGE_BYTES: u64 = 48 * 1024 * 1024;
/// Entry cap for the degenerate case of many tiny images.
const MAX_DECODED_IMAGES: usize = 24;
/// Assumed footprint when the header is unrecognized, so an unknown format
/// still competes for the budget instead of being free.
const UNKNOWN_IMAGE_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Default)]
struct ImageCacheInner {
    entries: HashMap<String, DecodedImage>,
    /// Monotonic tick, so `used` gives a total order.
    clock: u64,
    /// Sum of `pixel_bytes` currently resident.
    bytes: u64,
    /// Rasters this cache let go of since the last drain. GPUI's asset system
    /// keeps the decoded copy of every image it has painted and never evicts
    /// it, so the app hands these back to `ImageSource::remove_asset`, or a
    /// session with screenshots pays for its pixels until the process exits.
    dropped: Vec<Arc<Image>>,
}

/// Decoded raster cache, shared by every frame and released on session switch.
pub(crate) type ImageCache = Rc<RefCell<ImageCacheInner>>;

impl ImageCacheInner {
    fn get(&mut self, key: &str) -> Option<Option<Arc<Image>>> {
        self.clock += 1;
        let clock = self.clock;
        let entry = self.entries.get_mut(key)?;
        entry.used = clock;
        Some(entry.image.clone())
    }

    fn insert(&mut self, key: String, image: Option<Arc<Image>>, pixel_bytes: u64) {
        if let Some(previous) = self.entries.remove(&key) {
            self.bytes = self.bytes.saturating_sub(previous.pixel_bytes);
            self.dropped.extend(previous.image);
        }
        self.clock += 1;
        let used = self.clock;
        self.entries.insert(
            key,
            DecodedImage {
                image,
                pixel_bytes,
                used,
            },
        );
        self.bytes += pixel_bytes;
        self.evict();
    }

    /// Drop least-recently-used entries until both the byte budget and the
    /// entry cap are satisfied.
    fn evict(&mut self) {
        while self.bytes > MAX_DECODED_IMAGE_BYTES || self.entries.len() > MAX_DECODED_IMAGES {
            let Some(victim) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some(entry) = self.entries.remove(&victim) {
                self.bytes = self.bytes.saturating_sub(entry.pixel_bytes);
                self.dropped.extend(entry.image);
            }
        }
    }

    /// Let go of every decoded raster, queueing them for the asset drain.
    fn release_all(&mut self) {
        for (_, entry) in self.entries.drain() {
            self.dropped.extend(entry.image);
        }
        self.bytes = 0;
    }
}

fn cache_decoded_image(
    view: &ChatViewState,
    key: String,
    image: Option<Arc<Image>>,
    pixel_bytes: u64,
) {
    view.tool_images
        .borrow_mut()
        .insert(key, image, pixel_bytes);
}

/// Decompressed footprint of an encoded raster, read from its header.
///
/// Only the header is parsed — the bytes are never walked — so this is safe to
/// call on the decode path. Unrecognized formats report nothing and the caller
/// falls back to a conservative constant.
fn encoded_pixel_bytes(format: ImageFormat, bytes: &[u8]) -> Option<u64> {
    fn rgba(width: u64, height: u64) -> Option<u64> {
        (width > 0 && height > 0).then(|| width.saturating_mul(height).saturating_mul(4))
    }

    match format {
        ImageFormat::Png => {
            // 8-byte signature, 4-byte length, "IHDR", then width/height.
            let header = bytes.get(16..24)?;
            let width = u32::from_be_bytes(header[0..4].try_into().ok()?) as u64;
            let height = u32::from_be_bytes(header[4..8].try_into().ok()?) as u64;
            rgba(width, height)
        }
        ImageFormat::Jpeg => {
            // Walk the segment chain to the first start-of-frame marker.
            let mut index = 2usize;
            while index + 9 < bytes.len() {
                if bytes[index] != 0xFF {
                    index += 1;
                    continue;
                }
                let marker = bytes[index + 1];
                // SOF0..SOF15 minus DHT/JPG/DAC carry the frame dimensions.
                if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
                    let height =
                        u16::from_be_bytes(bytes[index + 5..index + 7].try_into().ok()?) as u64;
                    let width =
                        u16::from_be_bytes(bytes[index + 7..index + 9].try_into().ok()?) as u64;
                    return rgba(width, height);
                }
                let length =
                    u16::from_be_bytes(bytes.get(index + 2..index + 4)?.try_into().ok()?) as usize;
                index += 2 + length.max(2);
            }
            None
        }
        ImageFormat::Gif => {
            let header = bytes.get(6..10)?;
            let width = u16::from_le_bytes(header[0..2].try_into().ok()?) as u64;
            let height = u16::from_le_bytes(header[2..4].try_into().ok()?) as u64;
            rgba(width, height)
        }
        ImageFormat::Webp => {
            let chunk = bytes.get(12..16)?;
            match chunk {
                b"VP8X" => {
                    let header = bytes.get(24..30)?;
                    let width = 1 + u32::from_le_bytes([header[0], header[1], header[2], 0]) as u64;
                    let height =
                        1 + u32::from_le_bytes([header[3], header[4], header[5], 0]) as u64;
                    rgba(width, height)
                }
                b"VP8 " => {
                    let header = bytes.get(26..30)?;
                    let width = u16::from_le_bytes(header[0..2].try_into().ok()?) as u64 & 0x3FFF;
                    let height = u16::from_le_bytes(header[2..4].try_into().ok()?) as u64 & 0x3FFF;
                    rgba(width, height)
                }
                b"VP8L" => {
                    let header = bytes.get(21..25)?;
                    let bits = u32::from_le_bytes(header.try_into().ok()?);
                    let width = (bits & 0x3FFF) as u64 + 1;
                    let height = ((bits >> 14) & 0x3FFF) as u64 + 1;
                    rgba(width, height)
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Rasters attached by the user. Decoded once per path/size pair; the same
/// cache as tool screenshots (the keys are prefixed so they cannot collide).
fn attachment_image(view: &ChatViewState, attachment: &UserAttachment) -> Option<Arc<Image>> {
    let raster = attachment.image.as_ref()?;
    let key = format!("attachment:{}:{}", attachment.path, raster.data.len());
    let format = ImageFormat::from_mime_type(&raster.mime_type).unwrap_or(ImageFormat::Png);
    let pixel_bytes = raster_pixel_bytes(&raster.data, format).unwrap_or(UNKNOWN_IMAGE_BYTES);
    raster_image(view, key, &raster.data, format, pixel_bytes)
}

/// Footprint of a base64 raster, read from the header of a short prefix so a
/// frame never has to decode a megabyte of base64 just to size it. Falls back to
/// the caller's estimate when the format is not one the header parser knows.
fn raster_pixel_bytes(base64_data: &str, format: ImageFormat) -> Option<u64> {
    /// Enough for a PNG/GIF/WebP header and almost every JPEG frame header.
    const PREFIX_CHARS: usize = 8 * 1024;
    let end = base64_data.len().min(PREFIX_CHARS) & !3;
    let prefix = base64::engine::general_purpose::STANDARD
        .decode(base64_data.get(..end)?)
        .ok()?;
    encoded_pixel_bytes(format, &prefix)
}

/// One decoded or downscaled raster, shared by user attachments and tool
/// screenshots.
///
/// Rasters above [`THUMBNAIL_MIN_PIXEL_BYTES`] are not handed to the renderer
/// as they came: a 2560×1680 screenshot turns into a 4096² sprite-atlas page
/// plus a 16 MB decoded copy for a 184×120 card. They go through
/// [`crate::thumbs`] instead — the frame draws nothing until the downscale
/// lands, which keeps both the atlas page and the raster cache small.
fn raster_image(
    view: &ChatViewState,
    key: String,
    base64_data: &str,
    format: ImageFormat,
    pixel_bytes: u64,
) -> Option<Arc<Image>> {
    if let Some(cached) = view.tool_images.borrow_mut().get(&key) {
        return cached;
    }
    if pixel_bytes >= THUMBNAIL_MIN_PIXEL_BYTES {
        let decision = view
            .thumbs
            .borrow_mut()
            .thumbnail(&view.session_id, &key, base64_data);
        match decision {
            Thumb::Ready(png) => {
                let image = Arc::new(Image::from_bytes(ImageFormat::Png, png));
                cache_decoded_image(view, key, Some(image.clone()), THUMBNAIL_PIXEL_BYTES);
                return Some(image);
            }
            Thumb::Pending => return None,
            Thumb::Unavailable => {}
        }
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(base64_data)
        .ok()
        .map(|bytes| Arc::new(Image::from_bytes(format, bytes)));
    cache_decoded_image(view, key, decoded.clone(), pixel_bytes);
    decoded
}

/// Rasters attached to a tool result (usually computer-use screenshots).
/// Base64 decoding is cached per `call_id:index`; the map lives on the chat
/// list so a session switch releases the decoded pixels.
fn tool_image(
    view: &ChatViewState,
    call_id: &str,
    result: &ToolResult,
    index: usize,
) -> Option<Arc<Image>> {
    let key = format!("{call_id}:{index}");
    let source = result.images.get(index)?;
    let format = ImageFormat::from_mime_type(&source.mime_type).unwrap_or(ImageFormat::Png);
    let pixel_bytes = raster_pixel_bytes(&source.data, format).unwrap_or(UNKNOWN_IMAGE_BYTES);
    raster_image(view, key, &source.data, format, pixel_bytes)
}

fn render_tool_images(
    theme: &Theme,
    call_id: &str,
    result: &ToolResult,
    view: &ChatViewState,
) -> AnyElement {
    let mut row = div().flex().flex_wrap().gap(px(6.0));
    for index in 0..result.images.len() {
        if let Some(image) = tool_image(view, call_id, result, index) {
            row = row.child(
                img(image)
                    .w(px(420.0))
                    .h(px(280.0))
                    .bg(theme.surface)
                    .object_fit(ObjectFit::Contain)
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(theme.border),
            );
        }
    }
    row.into_any_element()
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
    // ZCode detail card: hairline border on the panel surface instead of a
    // bare dark slab.
    div()
        .w_full()
        .rounded(px(12.0))
        .border_1()
        .border_color(theme.border)
        .bg(theme.surface)
        .overflow_hidden()
        .child(
            div()
                .px(px(12.0))
                .pt(px(8.0))
                .pb(px(4.0))
                .text_size(px(10.5))
                .text_color(theme.text_faint)
                .child(title.to_string()),
        )
        .child(
            div()
                .id(SharedString::from(format!("code-{}", panel_key)))
                .relative()
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
                // Absolute, like the reasoning body: a plain zero-height
                // canvas child never contains the pointer.
                .child(
                    wheel_claim_overlay(scroll_handle, entity.clone())
                        .absolute()
                        .inset_0(),
                ),
        )
        .into_any_element()
}

// --- tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use effibuddy_agent_core::{SessionState, ToolCall, ToolResult};

    #[test]
    fn nested_wheel_follows_gpui_direction() {
        // Scrolled down 100px, wheel up: the offset moves toward zero.
        assert_eq!(nested_scroll_y(-100.0, 30.0, 400.0), -70.0);
        // Wheel down at the top scrolls down.
        assert_eq!(nested_scroll_y(0.0, -30.0, 400.0), -30.0);
        // Neither edge can overscroll.
        assert_eq!(nested_scroll_y(0.0, 30.0, 400.0), 0.0);
        assert_eq!(nested_scroll_y(-380.0, -50.0, 400.0), -400.0);
    }

    #[test]
    fn wheeling_down_to_the_end_re_pins_the_transcript() {
        let mut chat = ChatList::new();
        // Simulate a transcript the user scrolled away from.
        chat.pinned = false;
        chat.wheel(-120.0);
        // First tick arms the clock, the second settles at the bottom and
        // restores the follow-the-stream pin.
        assert!(chat.tick_scroll());
        assert!(!chat.tick_scroll());
        assert!(chat.pinned());
    }

    #[test]
    fn wheeling_up_with_no_scroll_room_stays_pinned() {
        let mut chat = ChatList::new();
        assert!(chat.pinned());
        // Nothing fits above the top: an upward wheel cannot unpin a
        // transcript that has nowhere to go.
        chat.wheel(120.0);
        assert!(chat.pinned());
    }

    /// The decoded image cache must be bounded by *memory*, not by count.
    ///
    /// It used to hold 16 entries with FIFO eviction: sixteen 1080p screenshots
    /// is ~128 MB of anon heap pinned for images that had long scrolled away,
    /// and the accounting reported the compressed payload so nothing noticed.
    #[test]
    fn decoded_image_cache_is_bounded_by_bytes_and_evicts_lru() {
        let mut cache = ImageCacheInner::default();
        // Eight 4 MB rasters: two of them blow the entry cap's byte budget.
        let page: u64 = 4 * 1024 * 1024;
        let capacity = (MAX_DECODED_IMAGE_BYTES / page) as usize;
        for index in 0..capacity {
            cache.insert(format!("img{index}"), None, page);
        }
        assert!(cache.bytes <= MAX_DECODED_IMAGE_BYTES);
        assert_eq!(cache.entries.len(), capacity);

        // Touch the oldest so it is no longer the eviction victim.
        assert!(cache.get("img0").is_some());
        cache.insert("fresh".to_string(), None, page);

        assert!(
            cache.bytes <= MAX_DECODED_IMAGE_BYTES,
            "cache still holds {} bytes after insert",
            cache.bytes
        );
        assert!(
            cache.entries.contains_key("img0"),
            "the recently viewed image was evicted instead of the stalest one"
        );
        assert!(
            !cache.entries.contains_key("img1"),
            "LRU victim was not the least recently used entry"
        );
    }

    /// Rasters the cache lets go of must reach the host, which is the only
    /// place that can unload GPUI's decoded copy. Dropping them silently is
    /// what pinned a pasted screenshot's pixels in the process for good.
    #[test]
    fn releasing_the_cache_queues_rasters_for_the_asset_drain() {
        let mut cache = ImageCacheInner::default();
        let raster = || {
            Arc::new(Image::from_bytes(
                ImageFormat::Png,
                b"not-a-real-png".to_vec(),
            ))
        };
        cache.insert("a".to_string(), Some(raster()), 4 * 1024 * 1024);
        cache.insert("b".to_string(), Some(raster()), 4 * 1024 * 1024);
        assert!(cache.dropped.is_empty(), "nothing dropped yet");

        cache.release_all();
        assert!(cache.entries.is_empty());
        assert_eq!(cache.bytes, 0);
        assert_eq!(cache.dropped.len(), 2, "both rasters queued for the drain");

        // A second release is a no-op and must not re-queue anything.
        cache.release_all();
        assert_eq!(cache.dropped.len(), 2);

        // Eviction queues the victim too, so an over-budget cache drains too.
        let mut cache = ImageCacheInner::default();
        for index in 0..3 {
            cache.insert(format!("img{index}"), Some(raster()), 48 * 1024 * 1024);
        }
        assert!(
            !cache.dropped.is_empty(),
            "an evicted raster was dropped without telling the host"
        );
    }

    /// Header parsing is what makes the budget meaningful; a wrong answer here
    /// would silently disable the byte bound for one format.
    #[test]
    fn encoded_pixel_bytes_reads_png_and_gif_headers() {
        // Minimal PNG signature + IHDR for a 1920×1080 raster.
        let mut png = Vec::new();
        png.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&1920u32.to_be_bytes());
        png.extend_from_slice(&1080u32.to_be_bytes());
        png.extend_from_slice(&[8, 6, 0, 0, 0]);
        assert_eq!(
            encoded_pixel_bytes(ImageFormat::Png, &png),
            Some(1920 * 1080 * 4)
        );

        let mut gif = Vec::new();
        gif.extend_from_slice(b"GIF89a");
        gif.extend_from_slice(&320u16.to_le_bytes());
        gif.extend_from_slice(&240u16.to_le_bytes());
        assert_eq!(
            encoded_pixel_bytes(ImageFormat::Gif, &gif),
            Some(320 * 240 * 4)
        );

        // Truncated input must not panic, just report nothing.
        assert_eq!(encoded_pixel_bytes(ImageFormat::Png, b"\x89PNG"), None);
        assert_eq!(encoded_pixel_bytes(ImageFormat::Jpeg, b"\xFF\xD8"), None);
    }

    /// Rebinding the same text must invalidate the cached assistant snapshot.
    ///
    /// The rebuild keys each assistant item on `Message::revision` (plus the
    /// external tool-result inputs), so a mutator that forgot to bump the
    /// revision would leave the UI showing stale text forever. This pins the
    /// contract from both ends: a no-op rebuild reuses the item, and any real
    /// mutation produces a fresh one.
    #[test]
    fn rebuild_reuses_snapshots_until_the_message_changes() {
        let mut session = SessionState::new(std::env::temp_dir(), "cache");
        session.push_message(Message::user("第一个问题"));
        let mut assistant = Message::assistant();
        assistant.push_reasoning("先想一想");
        assistant.set_text("第一段答案");
        session.push_message(assistant);
        session.push_message(Message::user("第二个问题"));

        let mut chat = ChatList::new();
        let run = RunSnapshot::default();
        chat.rebuild(&session, &run);
        let before = chat.items.len();
        let snapshot = match &chat.items[1].kind {
            ChatItemKind::Assistant(item) => item.clone(),
            _ => panic!("expected an assistant item"),
        };

        // Nothing changed: the exact same `Rc` snapshot must come back.
        chat.rebuild(&session, &run);
        assert_eq!(chat.items.len(), before);
        let reused = match &chat.items[1].kind {
            ChatItemKind::Assistant(item) => item.clone(),
            _ => panic!("expected an assistant item"),
        };
        assert!(
            Rc::ptr_eq(&snapshot, &reused),
            "an unchanged message was re-derived instead of reused"
        );

        // A real mutation must produce a new snapshot with the new body.
        let last = session.messages.len() - 2;
        session.messages[last].push_text("，补充下一段");
        chat.rebuild(&session, &run);
        let updated = match &chat.items[1].kind {
            ChatItemKind::Assistant(item) => item.clone(),
            _ => panic!("expected an assistant item"),
        };
        assert!(
            !Rc::ptr_eq(&snapshot, &updated),
            "the snapshot was reused after its message changed"
        );
        assert!(
            updated.text.ends_with("补充下一段"),
            "the rebuilt snapshot still shows the old body: {}",
            updated.text
        );
    }

    /// A finished turn often ends on a tool-only step; the answer text lives in
    /// an earlier member. The body must fall back to it instead of rendering a
    /// header with nothing below (the text used to disappear as the turn grew).
    #[test]
    fn turn_body_uses_the_last_member_with_text() {
        let make = |text: &str| {
            let mut message = Message::assistant();
            if !text.is_empty() {
                message.push_text(text);
            }
            Rc::new(build_assistant_item(&message, None))
        };
        let turn = TurnItem {
            id: "turn:test".to_string(),
            members: vec![make("first answer"), make(""), make("")],
            signatures: vec![1, 2, 3],
            thinking_count: 0,
            tool_calls: 0,
            agent_calls: 0,
            total_ms: 0,
            any_error: false,
        };
        assert_eq!(turn_body_index(&turn, None), 0);
        // A live stream always owns the last member: its text is still
        // arriving through the typewriter snapshot keyed by that id.
        let streaming = turn.members[2].message.id.clone();
        assert_eq!(turn_body_index(&turn, Some(&streaming)), 2);
    }

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
    fn auto_expand_reveal_is_armed_once_per_turn() {
        let mut session = SessionState::new(std::env::temp_dir(), "test");
        session.push_message(Message::user("hello"));
        let mut first = Message::assistant();
        first.set_text("working");
        session.push_message(first);
        let mut second = Message::assistant();
        second.set_text("done");
        session.push_message(second.clone());

        let mut chat = ChatList::new();
        let run = RunSnapshot {
            running: true,
            ..RunSnapshot::default()
        };
        chat.rebuild(&session, &run);
        // The turn's last message is the live streaming one.
        chat.begin_message(&second.id);
        let entity = WeakEntity::<AppView>::new_invalid();
        let none = HashSet::new();
        let mut prepare = |chat: &mut ChatList| {
            chat.prepare_frame(
                true,
                true,
                &none,
                &none,
                &none,
                &none,
                &HashMap::new(),
                &entity,
            )
        };
        let turn_id = chat.items[1].id.clone();

        prepare(&mut chat);
        assert!(chat.turn_expand_at.contains_key(&turn_id));
        let armed_at = chat.turn_expand_at[&turn_id];

        // Still live on the next frame: the timestamp must not move.
        prepare(&mut chat);
        assert_eq!(chat.turn_expand_at[&turn_id], armed_at);

        // `prepare_frame` prunes the expired window; the live turn is still
        // auto-expanded, but the reveal must not be re-armed — otherwise the
        // whole activity stack fades in again after every tool step.
        chat.turn_expand_at
            .insert(turn_id.clone(), Instant::now() - TURN_EXPAND_WINDOW * 2);
        let view = prepare(&mut chat);
        assert!(!view.turn_expand_at.contains_key(&turn_id));
        assert!(!chat.turn_expand_at.contains_key(&turn_id));

        // Manual expand stays a deliberate replay.
        chat.note_turn_expanded(&turn_id);
        let view = prepare(&mut chat);
        assert!(view.turn_expand_at.contains_key(&turn_id));
    }

    #[test]
    fn auto_expand_reveal_is_not_rearmed_by_step_rebuilds() {
        let mut session = SessionState::new(std::env::temp_dir(), "test");
        session.push_message(Message::user("hello"));
        let call = ToolCall::new("grep", serde_json::json!({"pattern": "x"}));
        let mut first = Message::assistant();
        first.set_text("working");
        first.push_tool_call(call.clone());
        session.push_message(first.clone());
        session.push_message(Message::from_tool_result(ToolResult::success(&call, "ok")));

        let mut chat = ChatList::new();
        let run = RunSnapshot {
            running: true,
            ..RunSnapshot::default()
        };
        let entity = WeakEntity::<AppView>::new_invalid();
        let none = HashSet::new();
        let mut prepare = |chat: &mut ChatList| {
            chat.prepare_frame(
                true,
                true,
                &none,
                &none,
                &none,
                &none,
                &HashMap::new(),
                &entity,
            )
        };

        // First step: a lone assistant message is not a turn block yet.
        chat.rebuild(&session, &run);
        prepare(&mut chat);
        assert!(chat.turn_expand_at.is_empty());

        // Second step: the run merges into a turn block and arms the reveal.
        let mut second = Message::assistant();
        second.set_text("reading");
        second.push_tool_call(call.clone());
        session.push_message(second.clone());
        chat.begin_message(&second.id);
        chat.rebuild(&session, &run);
        prepare(&mut chat);
        let turn_id = chat.items[1].id.clone();
        assert_eq!(turn_id, format!("turn:{}", first.id));
        assert!(chat.turn_expand_at.contains_key(&turn_id));

        // A long tool call lets the window expire; the next step rebuilds the
        // same turn (new signature, same id) and must not re-arm the stagger.
        chat.turn_expand_at
            .insert(turn_id.clone(), Instant::now() - TURN_EXPAND_WINDOW * 3);
        let mut third = Message::assistant();
        third.set_text("done");
        session.push_message(third.clone());
        chat.begin_message(&third.id);
        chat.rebuild(&session, &run);
        let view = prepare(&mut chat);
        assert_eq!(chat.items[1].id, turn_id);
        assert!(chat.revealed_turns.contains(&turn_id));
        assert!(!chat.turn_expand_at.contains_key(&turn_id));
        assert!(!view.turn_expand_at.contains_key(&turn_id));
    }

    #[test]
    fn stagger_holds_transparent_then_fades_without_a_flash() {
        let delay = TURN_EXPAND_STAGGER * 2;
        let total_ms = (delay + TURN_EXPAND_FADE).as_secs_f32() * 1000.0;
        let opacity_at = |elapsed_ms: f32| (stagger_opacity(delay))(elapsed_ms / total_ms);

        // Hold phase: transparent from the first frame until the delay ends.
        assert_eq!(opacity_at(0.0), 0.0);
        assert_eq!(opacity_at(1.0), 0.0);
        assert_eq!(opacity_at(delay.as_secs_f32() * 1000.0), 0.0);
        // The old two-phase `with_animations` hold rendered a full-brightness
        // frame exactly at the boundary, then fell back to transparent.
        assert!(opacity_at(delay.as_secs_f32() * 1000.0 + 1.0) < 0.05);
        assert!((opacity_at(total_ms) - 1.0).abs() < 1e-6);

        // No delay still starts from 0 instead of flashing on the first frame.
        assert_eq!((stagger_opacity(Duration::ZERO))(0.0), 0.0);
        assert!(((stagger_opacity(Duration::ZERO))(1.0) - 1.0).abs() < 1e-6);

        // Monotonic, and no frame paints a near-full-opacity jump.
        let mut previous = 0.0;
        for step in 0..=total_ms as u32 {
            let value = opacity_at(step as f32);
            assert!(
                value >= previous - 1e-6,
                "opacity went backwards at {step}ms"
            );
            assert!(value - previous < 0.1, "opacity jumped at {step}ms");
            previous = value;
        }
    }

    #[test]
    fn rebuild_forgets_reveals_of_removed_turns() {
        let mut session = SessionState::new(std::env::temp_dir(), "test");
        session.push_message(Message::user("hello"));
        let mut first = Message::assistant();
        first.set_text("working");
        session.push_message(first);
        let mut second = Message::assistant();
        second.set_text("done");
        session.push_message(second);

        let mut chat = ChatList::new();
        chat.rebuild(&session, &RunSnapshot::default());
        let turn_id = chat.items[1].id.clone();
        ChatList::note_auto_expanded(&mut chat.revealed_turns, &mut chat.turn_expand_at, &turn_id);
        assert!(chat.revealed_turns.contains(&turn_id));

        // Undo drops the turn from the transcript; the bookkeeping goes too.
        session.messages.truncate(2);
        chat.rebuild(&session, &RunSnapshot::default());
        assert!(!chat.revealed_turns.contains(&turn_id));
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

    #[test]
    fn prompt_rail_follow_centers_and_keeps_manual_scroll() {
        let metrics = PromptRailMetrics {
            body_height: 600.0,
            view_height: 100.0,
            pad: 0.0,
            max_scroll: 900.0,
        };
        let mut state = PromptRailState::default();
        assert!(state.follow(50, &metrics));
        assert!((state.scroll - 455.0).abs() < 0.1);
        // Same active query: no recenter, so a wheel scroll of the rail is not
        // snapped back while the user browses it.
        assert!(!state.follow(50, &metrics));
        state.scroll = 0.0;
        assert!(!state.follow(50, &metrics));
        assert_eq!(state.scroll, 0.0);
        // Moving to another query recenters...
        assert!(state.follow(51, &metrics));
        assert!((state.scroll - 465.0).abs() < 0.1);
        // ...and both ends clamp instead of scrolling past the list.
        state.follow(0, &metrics);
        assert_eq!(state.scroll, 0.0);
        state.follow(999, &metrics);
        assert_eq!(state.scroll, 900.0);
    }

    #[test]
    fn preview_text_collapses_whitespace_into_bounded_paragraphs() {
        assert_eq!(preview_text(&["  a   b\n\n c "], "", 220, 2), "a b\nc");
        // A third paragraph would exceed the two-paragraph budget; the preview
        // stops and marks the cut, matching ZCode's hover cards.
        assert_eq!(preview_text(&["a\n\nb\n\nc"], "", 220, 2), "a\nb…");
        let long = "字".repeat(400);
        let cut = preview_text(&[&long], "", 20, 2);
        assert_eq!(cut.chars().count(), 20);
        assert!(cut.ends_with('…'));
        assert_eq!(preview_text(&[" \n\t"], "用户输入", 220, 2), "用户输入");
    }

    #[test]
    fn prompt_marks_follow_queries_and_reply_segments() {
        let mut session = SessionState::new(std::env::temp_dir(), "test");
        session.push_message(Message::user("first question"));
        let mut first = Message::assistant();
        first.set_text("first answer");
        session.push_message(first);
        session.push_message(Message::user("second question"));
        let mut second = Message::assistant();
        second.set_text("second answer");
        session.push_message(second);

        let mut chat = ChatList::new();
        chat.rebuild(&session, &RunSnapshot::default());
        let marks = chat.prompt_marks();
        assert_eq!(marks.len(), 2);
        assert_eq!(marks[0].user_preview.as_ref(), "first question");
        assert_eq!(marks[0].assistant_preview.as_ref(), "first answer");
        assert!(!marks[0].assistant_placeholder);
        assert_eq!(marks[1].user_preview.as_ref(), "second question");
        assert_eq!(marks[1].assistant_preview.as_ref(), "second answer");
        assert!(!marks.iter().any(|mark| mark.running));

        // A live run flags exactly the newest query; an empty reply shows the
        // running placeholder instead of a stale body.
        session.push_message(Message::user("third question"));
        chat.rebuild(
            &session,
            &RunSnapshot {
                running: true,
                ..RunSnapshot::default()
            },
        );
        let marks = chat.prompt_marks();
        assert_eq!(marks.len(), 3);
        assert!(marks[2].running);
        assert!(marks[2].assistant_placeholder);
    }
}
