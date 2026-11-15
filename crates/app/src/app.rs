//! The main application view.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, Timelike, Utc};
use effibuddy_agent_core::tool::{ModeApprovalGate, PermissionMode};
use effibuddy_agent_core::{
    AgentHandle, AgentHarness, AgentVisibility, ApprovalDecision, ApprovalRequest,
    BrokerApprovalGate, ContentBlock, DemoProvider, HarnessConfig, HarnessEvent, Message,
    ModelProvider, ProfileDiagnostic, RunOutcome, RunStatus, SharedSession, SubagentColor,
    SubagentProfile, SubagentReport, SubagentSpawner, SubagentStatus, TodoStatus, ToolCall,
    ToolResult, UserAttachment, lock_session, new_shared_session,
};
use effibuddy_agent_tools::{ComputerTool, builtin_tools_with_roots};
use effibuddy_computer::{ComputerUseMode, OverlayEvent, OverlayEventKind};
use effibuddy_ui::{
    BadgeTone, Button, ButtonSize, ButtonVariant, CONTROL_MOTION, IconButton, MotionController,
    MotionSpec, PaintShift, ProgressRing, Spinner, TextInputOptions, TextInputState, TextInputView,
    Theme, disclosure_chevron, divider, ease_out_cubic, empty_state, icons, images, kbd,
    motion_item, overlay_motion_duration, radius, reduce_motion, shadow_float, small_badge,
};
use gpui::{
    Animation, AnimationExt, AnyElement, App, Bounds, ClipboardItem, Context, DispatchPhase, Div,
    Entity, IntoElement, MouseButton, MouseMoveEvent, MouseUpEvent, PathPromptOptions, Pixels,
    Point, Render, SharedString, Stateful, WeakEntity, Window, WindowBounds, WindowHandle,
    WindowKind, WindowOptions, actions, anchored, div, img, linear_color_stop, linear_gradient,
    list, point, prelude::*, px, size, svg, transparent_black,
};
use serde_json::json;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use crate::chat::{
    AgentCard, ChatList, JumpFlash, PromptRailState, RunSnapshot, agent_color, render_chat_item,
    render_prompt_rail as render_prompt_rail_element, render_scroll_to_bottom, render_scrollbar,
};
use crate::checkpoint::{RestoreAction, RestorePlan};
use crate::composer_menu::ComposerMenuState;
use crate::computer::{ComputerApprovalGate, ComputerOverlayView, composer_hint};
use crate::config::{AppConfig, MAX_UI_ZOOM, MIN_UI_ZOOM};
use crate::draft_store::{DraftStore, TASK_SCOPE};
use crate::events::{AppEvent, SubagentSpawnedInfo};
use crate::graph::{GraphView, ViewMode, persist as graph_persist};
use crate::model_config::ModelConfigStore;
use crate::model_picker::ModelMenuState;
use crate::perf;
use crate::project_store::{ProjectStore, normalize_root, project_default_name, project_group_key};
use crate::settings::{SettingsSection, SettingsState};
use crate::side_pane::{
    AgentView, GitFile, SidePaneState, SidePaneTab, SidePaneTabKind, format_untracked_file,
    parse_git_status,
};
use crate::smooth::{SmoothScrollers, smooth_overlay};
use crate::store::{PendingSave, SessionStore, SessionSummary};
use crate::subagent::SubagentService;

/// Expanded left sidebar width. The titlebar brand block mirrors it so the
/// two form one continuous column; when collapsed the sidebar slides out over
/// [`PANE_SLIDE_SECS`] and is then removed from the tree.
const SIDEBAR_WIDTH: f32 = 272.0;

/// Dwell before the query rail opens its preview card. The bar widens
/// immediately; only the floating card waits, so sweeping the rail does not
/// strobe cards.
const PROMPT_RAIL_CARD_DELAY: Duration = Duration::from_millis(110);

/// One in-flight slide of the left sidebar or the right side pane. Progress
/// follows `MotionSpec::LAYOUT`'s 220ms ease-out so a collapse reads like the
/// app's other layout motion.
#[derive(Clone, Copy)]
struct PaneSlide {
    start: Instant,
    from: f32,
    to: f32,
}

/// Slide duration in seconds, mirroring `MotionSpec::LAYOUT`.
const PANE_SLIDE_SECS: f32 = 0.22;

/// Advance a panel slide toward `target` (0 = hidden, 1 = shown); returns
/// whether the slide still needs frames. The first frame that sees an unmet
/// target starts a slide from the current progress, and a mid-flight retarget
/// restarts from wherever the panel currently is, so rapid toggles never jump.
fn advance_pane_slide(slide: &mut Option<PaneSlide>, progress: &mut f32, target: f32) -> bool {
    if reduce_motion() {
        *slide = None;
        *progress = target;
        return false;
    }
    if *progress == target && slide.is_none() {
        return false;
    }
    let now = Instant::now();
    let state = match slide {
        Some(state) if state.to == target => *state,
        _ => PaneSlide {
            start: now,
            from: *progress,
            to: target,
        },
    };
    let t = (now.saturating_duration_since(state.start).as_secs_f32() / PANE_SLIDE_SECS).min(1.0);
    if t >= 1.0 {
        *progress = state.to;
        *slide = None;
        return false;
    }
    *progress = state.from + (state.to - state.from) * ease_out_cubic(t);
    *slide = Some(state);
    true
}

actions!(
    effibuddy_app,
    [
        NewSession,
        ToggleTheme,
        ToggleSettings,
        ToggleSidebar,
        FocusComposer,
        StopRun,
        OpenCommandPalette,
        ToggleGraph,
        CyclePermissionMode,
        ZoomIn,
        ZoomOut,
        ZoomReset
    ]
);

/// True when `--stress` was passed (feature `stress` only; always false
/// otherwise). Used to scale the agent runtime for 1000-stream runs.
fn stress_requested() -> bool {
    #[cfg(feature = "stress")]
    {
        std::env::args().any(|argument| argument == "--stress")
    }
    #[cfg(not(feature = "stress"))]
    {
        false
    }
}

#[derive(Clone)]
enum PaletteAction {
    NewSession,
    ToggleTheme,
    OpenSettings,
    OpenModels,
    TogglePanel,
    ToggleSidebar,
    FocusComposer,
    StopRun,
    OpenAgents,
    OpenGraph,
    SelectSession(String),
}

impl PaletteAction {
    fn shortcut(&self) -> Option<&'static str> {
        match self {
            PaletteAction::NewSession => Some("Ctrl N"),
            PaletteAction::ToggleTheme => Some("Ctrl ⇧ T"),
            PaletteAction::OpenSettings => Some("Ctrl ⇧ S"),
            PaletteAction::ToggleSidebar => Some("Ctrl B"),
            PaletteAction::FocusComposer => Some("Ctrl L"),
            PaletteAction::StopRun => Some("Esc"),
            PaletteAction::OpenGraph => Some("Ctrl ⇧ G"),
            PaletteAction::OpenModels
            | PaletteAction::TogglePanel
            | PaletteAction::OpenAgents
            | PaletteAction::SelectSession(_) => None,
        }
    }
}

/// Palette rows render under a section header, like ZCode's QuickPick groups.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PaletteGroup {
    Recent,
    Suggested,
    Panel,
    Session,
}

impl PaletteGroup {
    fn label(self) -> &'static str {
        match self {
            PaletteGroup::Recent => "最近使用",
            PaletteGroup::Suggested => "建议",
            PaletteGroup::Panel => "面板",
            PaletteGroup::Session => "会话",
        }
    }
}

#[derive(Clone)]
struct PaletteItem {
    id: String,
    title: String,
    subtitle: String,
    icon: &'static str,
    group: PaletteGroup,
    /// English aliases so both languages match the same row.
    keywords: &'static [&'static str],
    /// Set for entries that sit in the MRU `palette_recent` list.
    recent: bool,
    action: PaletteAction,
}

pub(crate) struct ActiveSession {
    pub(crate) shared: SharedSession,
}

pub(crate) struct ActiveRun {
    run_id: Option<String>,
    handle: AgentHandle,
    approval: Arc<BrokerApprovalGate>,
    started_at: Instant,
    /// Bound to the user message that owns this run once `RunStarted` lands.
    checkpoint: Option<Arc<crate::checkpoint::CheckpointRecorder>>,
}

/// Delivery lane for a message typed while a run is active. `Queue` waits for
/// the turn to finish and then starts a new one; `Steer` is handed to the
/// harness and injected at the next step boundary inside the current turn.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PendingInputKind {
    Queue,
    Steer,
}

#[derive(Clone)]
pub(crate) struct PendingInput {
    pub(crate) id: String,
    pub(crate) text: String,
    pub(crate) kind: PendingInputKind,
    /// Composer attachments staged when the message was admitted. They ride
    /// with the text through queue → run, so a queued file drop is not lost.
    pub(crate) attachments: Vec<UserAttachment>,
}

/// One parent turn's user text plus its composer attachments.
#[derive(Clone)]
pub(crate) struct RunInput {
    pub(crate) text: String,
    pub(crate) attachments: Vec<UserAttachment>,
}

#[derive(Clone)]
pub(crate) struct PendingApproval {
    pub(crate) agent_id: Option<String>,
    pub(crate) request: ApprovalRequest,
}

/// Composer-side editing state for a user message. Sending truncates the
/// transcript at `message_id` and (optionally) restores this turn's files.
#[derive(Clone)]
pub(crate) struct EditMessageState {
    pub message_id: String,
    pub plan: RestorePlan,
    pub include_files: bool,
}

/// Second-step confirmation before a user message and its files are removed.
#[derive(Clone)]
pub(crate) struct UndoDialogState {
    pub message_id: String,
    pub excerpt: String,
    pub plan: RestorePlan,
}

/// Shown when the edited turn has files that cannot be restored safely. The
/// conversation is not trimmed until the user chooses an explicit action.
#[derive(Clone)]
pub(crate) struct EditConflictState {
    pub message_id: String,
    pub text: String,
    pub plan: RestorePlan,
    /// Attachments staged for the edited message; an edit re-sends them.
    pub attachments: Vec<UserAttachment>,
}

/// One project row in the sidebar. Sessions group by project id; a legacy
/// group without a registry entry (its folder vanished) still renders so
/// history never disappears.
#[derive(Clone)]
pub(crate) struct ProjectGroup {
    /// `None` for a synthesized legacy group.
    pub id: Option<String>,
    /// Stable sidebar/collapse key: `project:<id>` or `project:legacy:<path>`.
    pub key: String,
    pub name: String,
    /// Ordered folders, primary first.
    pub roots: Vec<PathBuf>,
}

impl ProjectGroup {
    fn primary(&self) -> Option<&Path> {
        self.roots.first().map(PathBuf::as_path)
    }
}

/// Popover anchored to a project's "more" button in the sidebar.
#[derive(Default)]
pub(crate) struct ProjectMenuState {
    pub target: Option<ProjectGroup>,
    pub position: Option<Point<Pixels>>,
}

/// Popover anchored to a conversation row's "more" button. Deleting is the
/// only action for now and stays two-step: the first click arms the row, the
/// second removes the transcript.
#[derive(Default)]
pub(crate) struct SessionMenuState {
    pub target: Option<String>,
    pub position: Option<Point<Pixels>>,
    pub confirm_delete: bool,
}

/// Rename dialog for one conversation.
#[derive(Clone)]
pub(crate) struct SessionRenameState {
    pub id: String,
    pub error: Option<String>,
}

/// Draft state of the project folder editor dialog.
#[derive(Clone)]
pub(crate) struct ProjectEditorState {
    pub id: String,
    pub name: String,
    pub roots: Vec<PathBuf>,
    pub primary: Option<PathBuf>,
    pub error: Option<String>,
}

/// UI-side projection of one subagent run. The child transcript is held via
/// `shared` so the read-only pane can render it without a store round-trip.
#[allow(dead_code)]
pub(crate) struct AgentTask {
    pub agent_id: String,
    pub name: String,
    pub profile: String,
    pub color: SubagentColor,
    pub description: String,
    pub status: SubagentStatus,
    pub background: bool,
    pub started_at: Instant,
    pub finished_at: Option<Instant>,
    pub child_session_id: String,
    pub parent_call_id: String,
    pub parent_session_id: String,
    pub parent_agent_id: Option<String>,
    pub parent_agent_name: Option<String>,
    pub group_id: Option<String>,
    pub group_name: Option<String>,
    pub depth: usize,
    pub visibility: AgentVisibility,
    pub read_only: bool,
    pub can_spawn: bool,
    pub task_prompt: String,
    pub output_schema: Option<serde_json::Value>,
    pub provider_id: Option<String>,
    pub model: String,
    pub shared: SharedSession,
    pub report: Option<String>,
    pub error: Option<String>,
    pub data: Option<serde_json::Value>,
    pub tool_uses: usize,
    pub total_tokens: u64,
    pub duration_ms: u64,
    pub waiting_approval: bool,
    /// True when rebuilt from disk on session reopen; restored tasks are
    /// read-only history and never counted as live work.
    pub restored: bool,
}

pub struct AppView {
    pub(crate) config: AppConfig,
    pub(crate) models: ModelConfigStore,
    pub(crate) store: SessionStore,
    /// Composer drafts and the single unsent conversation owned by each
    /// project/task scope. Survives session switches and app restarts.
    pub(crate) drafts: DraftStore,
    /// Named multi-folder projects. Sessions group by project id, never path.
    pub(crate) projects: ProjectStore,
    pub(crate) runtime: Option<tokio::runtime::Runtime>,
    pub(crate) app_tx: UnboundedSender<AppEvent>,

    sessions: Vec<SessionSummary>,
    /// Conversations archived from the sidebar; listed in Settings → 归档.
    pub(crate) archived_sessions: Vec<SessionSummary>,
    pub(crate) active: Option<ActiveSession>,
    pub(crate) active_run: Option<ActiveRun>,
    pub(crate) pending_approvals: Vec<PendingApproval>,
    /// User message currently being edited in the composer. The next submit
    /// trims the transcript at this message and optionally restores files.
    pub(crate) edit_message: Option<EditMessageState>,
    /// Secondary confirmation for the hover “撤销” action.
    pub(crate) undo_dialog: Option<UndoDialogState>,
    /// Edit conflict dialog: the conversation is not trimmed until resolved.
    pub(crate) edit_conflict: Option<EditConflictState>,
    /// Update check state (see `update.rs`); results arrive via `AppEvent`.
    pub(crate) update: crate::update::UpdateState,

    /// Shared computer-use runtime: consent, frames, pause/stop and the
    /// platform backend. One per app; only the main agent gets the tool.
    pub(crate) computer: Arc<effibuddy_computer::ComputerRuntime>,
    /// Floating top-center control bar, open only while computer access is in
    /// progress.
    pub(crate) computer_overlay: Option<WindowHandle<ComputerOverlayView>>,
    /// Weak self used by the overlay's stop button to interrupt the run.
    pub(crate) self_weak: WeakEntity<AppView>,
    /// Keeps the window-closed observer alive. If the user closes the floating
    /// control bar with the window manager, computer control is stopped.
    _window_closed_subscription: gpui::Subscription,

    pub(crate) subagent_tasks: Vec<AgentTask>,
    /// `agent_id → index into subagent_tasks`. Tasks are append-only (cleared
    /// on session reset), so the map stays valid without rebuilds; per-event
    /// lookups must be O(1) once 1000 children stream concurrently.
    subagent_task_index: HashMap<String, usize>,
    /// How many terminal (history) agent rows the side pane shows before the
    /// “加载更多” button; live rows are never cut.
    pub(crate) subagent_history_limit: usize,
    subagent_services: Vec<SubagentService>,
    pub(crate) side: SidePaneState,
    pub(crate) view_mode: ViewMode,
    pub(crate) graph: GraphView,
    /// Bumped on every viewport change; a debounced save only runs when this
    /// still matches, so continuous wheel-zoom writes disk once per gesture.
    pub(crate) graph_save_generation: u64,
    pending_subagent_notifications: Vec<String>,
    /// Messages typed while a run was active, in admission order. Queue items
    /// auto-drain one per finished turn; steer items vanish when the harness
    /// reports them drained. In-memory only (like ZCode's queue): a crash
    /// loses them and the user re-sends.
    pending_inputs: Vec<PendingInput>,
    /// Set when a run ends in Interrupted/Failed with the queue still full;
    /// ZCode calls this error-paused. A held queue does not auto-drain — not
    /// even after an unrelated background-notification run — until the user
    /// sends from the bar or clears it.
    queue_held: bool,
    agent_ticker_active: bool,
    /// Staged session saves, drained one at a time off the UI thread.
    persist_tx: UnboundedSender<PendingSave>,
    /// Wall clock of the last completed persist staging; enforces the minimum
    /// interval between background serializations of a (potentially huge)
    /// session.
    last_persist: Option<Instant>,
    /// Set while a trailing persist flush is scheduled after a throttled one.
    persist_flush_scheduled: bool,
    /// Set while a debounced `drafts.json` write is pending. Draft writes are
    /// tiny and coalesce keystrokes into one file write.
    draft_save_scheduled: bool,
    /// `--bench` deadline; the bench loop refills the storm until it passes.
    bench_until: Option<Instant>,
    /// The stress streaming message, so the bench loop can refill its backlog.
    bench_stream: Option<Message>,
    /// Guards background session scans: a stale listing never overwrites a
    /// newer one.
    refresh_generation: u64,
    /// Displayed-second watermarks for the elapsed clocks; the tickers only
    /// notify when a displayed second actually rolled over.
    run_tick_stamp: u64,
    agent_tick_stamp: u64,
    /// Stress lab (feature `stress`): the active/finished stress run.
    #[cfg(feature = "stress")]
    pub(crate) stress: Option<crate::stress::StressState>,
    /// Heartbeat guard for the stress dashboard ticker.
    #[cfg(feature = "stress")]
    pub(crate) stress_ticker_active: bool,
    /// Cached subagent catalog for the settings Agents section. The page used
    /// to rescan the agents directory on every render — i.e. per keystroke,
    /// since the section's inputs notify the whole view.
    pub(crate) agents_catalog: Option<Rc<(Vec<SubagentProfile>, Vec<ProfileDiagnostic>)>>,

    palette_open: bool,
    palette_query: Entity<TextInputState>,
    palette_input: Entity<TextInputView>,
    /// Keyboard-selected row in the palette (index into the filtered list).
    palette_selected: usize,
    /// Query the selection index was set for; a changed query resets to 0.
    palette_selected_for: String,
    /// Scroll handle of the palette list so keyboard selection keeps the
    /// highlighted row visible (the list can hold more rows than the 380px
    /// viewport shows).
    palette_scroll: gpui::ScrollHandle,
    /// Keeps the global up/down keystroke interceptor alive while the palette
    /// can be open.
    _palette_keys_subscription: gpui::Subscription,

    pub(crate) composer_state: Entity<TextInputState>,
    composer: Entity<TextInputView>,
    _composer_subscription: gpui::Subscription,
    /// `/`, `$` and `@` autocomplete state (trigger, filtered items, selection).
    pub(crate) composer_menu: ComposerMenuState,
    /// Files, images and skills staged for the next message. Cleared on send;
    /// kept in place while editing an existing message.
    pub(crate) composer_attachments: Vec<UserAttachment>,
    /// Up/Down/Enter/Tab/Esc interception while the composer menu is open.
    _composer_menu_keys_subscription: gpui::Subscription,

    pub(crate) settings_open: bool,
    /// Sidebar donation dialog (reward QR code); closed by Esc or backdrop.
    pub(crate) reward_open: bool,
    pub(crate) settings: SettingsState,
    pub(crate) model_menu: ModelMenuState,
    pub(crate) project_menu: ProjectMenuState,
    /// Conversation-row "more" popover (`None` while closed).
    pub(crate) session_menu: SessionMenuState,
    /// Rename dialog for one conversation (`None` while closed).
    pub(crate) session_rename: Option<SessionRenameState>,
    session_rename_name: Entity<TextInputState>,
    session_rename_name_view: Entity<TextInputView>,
    _session_rename_name_subscription: gpui::Subscription,
    /// Folder editor dialog for one project (`None` while closed).
    pub(crate) project_editor: Option<ProjectEditorState>,
    /// Name input inside the project editor dialog.
    project_editor_name: Entity<TextInputState>,
    project_editor_name_view: Entity<TextInputView>,
    _project_editor_name_subscription: gpui::Subscription,
    _palette_subscription: gpui::Subscription,

    pub(crate) theme: Theme,
    pub(crate) expanded_reasoning: HashSet<String>,
    pub(crate) expanded_tools: HashSet<String>,
    /// Turn blocks the user explicitly expanded / collapsed; both override
    /// the auto fold rule (live turns expand, finished ones collapse).
    pub(crate) expanded_turns: HashSet<String>,
    pub(crate) collapsed_turns: HashSet<String>,
    pub(crate) chat: ChatList,
    /// Set when the transcript structure changed; `render_chat` rebuilds the
    /// virtual-list snapshot once per change instead of once per frame.
    pub(crate) chat_dirty: bool,
    /// Left-edge query navigator state (hover card, rail scroll) for the main
    /// chat. Cleared together with `chat` on session switches.
    prompt_rail: PromptRailState,
    /// User bubble the last rail jump landed on; paints a one-shot ring.
    jump_flash: Option<JumpFlash>,
    jump_flash_seq: u64,
    /// Guards the extra frame requested while the rail's list viewport is not
    /// measured yet (first frame after a session opens or the window resizes).
    prompt_rail_frame_scheduled: bool,
    /// Smooth-scroll handles for every overflow region outside the transcript.
    pub(crate) scrolls: SmoothScrollers,
    /// Keyed FLIP state for the sidebar session list. Reorders are cheap paint
    /// offsets, not per-frame re-layout.
    pub(crate) motion: MotionController,
    /// Set by pin/unpin: the raw session order may not change (pinning does
    /// not touch `updated_at`), but the rendered grouping does. The next
    /// `refresh_sessions` consumes this to start the FLIP at assignment time.
    sidebar_grouping_dirty: bool,
    /// Guards against scheduling more than one smooth-scroll frame at a time.
    scroll_frame_scheduled: bool,
    /// Guards against scheduling more than one panel-slide frame at a time.
    pane_frame_scheduled: bool,
    /// Slide progress (0 = hidden, 1 = shown) of the left sidebar and the
    /// right side pane, advanced per frame like the scroll glides.
    sidebar_slide: Option<PaneSlide>,
    sidebar_p: f32,
    side_pane_slide: Option<PaneSlide>,
    side_p: f32,
    run_ticker_active: bool,
    pub(crate) last_error: Option<String>,
    pub(crate) notice: Option<String>,
    pub(crate) preview_mode: bool,
    /// `--preview-sidebar-hover` forces hover affordances on one project
    /// header and one session row so screenshots can verify them.
    preview_sidebar_hover: bool,
    /// `--preview-context-tip` 在输入框旁常驻渲染上下文 tips，供截图验收
    /// （Wayland 无法模拟真实 hover）。
    preview_context_tip: bool,
    /// 左栏会话行跑马灯的行内状态，key = session id。渲染（&self）需要按行
    /// 创建/推进状态，因此套 RefCell；行内再套一层供 hover 事件短借。
    sidebar_marquees: RefCell<HashMap<String, Rc<RefCell<SessionMarquee>>>>,
    /// 跑马灯播放/收尾期间的逐帧重绘泵标记（与 typewriter 同款）。
    sidebar_marquee_scheduled: bool,
}

impl AppView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut config = AppConfig::load();
        let theme = if std::env::args().any(|argument| argument == "--dark") {
            Theme::dark()
        } else if std::env::args().any(|argument| argument == "--light") {
            Theme::light()
        } else if config.theme == "light" {
            Theme::light()
        } else {
            Theme::dark()
        };
        theme.clone().set_global(cx);

        let models = ModelConfigStore::new(&config);
        let store = SessionStore::new();
        let summaries = store.summaries();
        let drafts = DraftStore::new();
        let (persist_tx, mut persist_rx) = unbounded_channel::<PendingSave>();
        let preview_launch = std::env::args().any(|argument| {
            argument == "--preview"
                || argument == "--preview-stress"
                || argument == "--bench"
                || argument == "--preview-empty"
                || argument == "--preview-agents"
                || argument == "--preview-subagent"
                || argument == "--preview-palette"
                || argument == "--preview-graph"
                || argument == "--preview-models"
                || argument == "--preview-sidepane-files"
                || argument == "--preview-sidepane-changes"
                || argument == "--preview-computer"
        });
        let state = if preview_launch {
            models.new_session_state(config.workspace())
        } else {
            // Reopen what the user last had in front of them. A missing or
            // archived transcript is skipped; an empty session only counts
            // when the draft registry still owns it (otherwise it is a stale
            // pre-draft artifact and should stay invisible).
            let last_active = drafts
                .last_active()
                .and_then(|id| store.load(id))
                .filter(|session| !session.archived)
                .filter(|session| {
                    !session.messages.is_empty() || drafts.is_unsent(&session.id)
                });
            last_active
                .or_else(|| {
                    summaries
                        .first()
                        .and_then(|summary| store.load(&summary.id))
                        .filter(|session| !session.archived)
                })
                .map(|mut session| {
                    models.normalize_session(&mut session);
                    session
                })
                .unwrap_or_else(|| {
                    // No history: reopen the 任务 section's unsent draft when
                    // there is one, so a restart never loses a half-written
                    // prompt. Otherwise start a fresh task draft.
                    if let Some(mut session) = drafts
                        .scope_session(TASK_SCOPE)
                        .and_then(|id| store.load(id))
                        .filter(|session| session.messages.is_empty() && !session.archived)
                    {
                        models.normalize_session(&mut session);
                        return session;
                    }
                    let mut session = models.new_session_state(config.workspace());
                    session.permission_mode = config.effective_default_mode();
                    let _ = store.save(&session);
                    session
                })
        };
        let startup_cwd = state.cwd.clone();
        let active = Some(ActiveSession {
            shared: new_shared_session(state),
        });

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(if stress_requested() {
                // Stress lab: 1000 concurrent streams starve a 2-worker
                // runtime; scale it, but keep the default app untouched.
                std::thread::available_parallelism()
                    .map(|n| n.get().min(8))
                    .unwrap_or(4)
            } else {
                2
            })
            .enable_all()
            .thread_name("effibuddy-agent")
            .build()
            .ok();

        let (app_tx, mut app_rx) = unbounded_channel::<AppEvent>();

        // Computer use shares the same foreground pump as harness events so
        // consent cards, tool results and overlay updates keep their order.
        let self_weak = cx.entity().downgrade();
        let computer = Arc::new(effibuddy_computer::ComputerRuntime::with_artifact_dir(
            effibuddy_computer::create_backend(),
            crate::config::base_dir().join("computer"),
        ));
        computer.set_mode(config.computer_use_mode);
        {
            let tx = app_tx.clone();
            computer.set_overlay_emitter(Arc::new(move |event| {
                let _ = tx.send(AppEvent::ComputerOverlay(event));
            }));
        }
        let window_closed_subscription = cx.on_window_closed({
            let weak = self_weak.clone();
            move |cx| {
                let _ = weak.update(cx, |this, cx| {
                    let Some(handle) = this.computer_overlay else {
                        return;
                    };
                    if handle.update(cx, |_, _, _| {}).is_ok() {
                        return;
                    }
                    // The floating bar was closed by the user/window manager.
                    this.computer_overlay = None;
                    if let Some(session_id) = this
                        .active
                        .as_ref()
                        .map(|active| lock_session(&active.shared).id.clone())
                    {
                        this.computer.stop(&session_id);
                    }
                    this.interrupt(cx);
                });
            }
        });

        let composer_state = cx.new(|cx| {
            TextInputState::new(
                cx,
                TextInputOptions::default()
                    .multiline(true)
                    .lines(2, 8)
                    .placeholder("给 EffiBuddy 一个任务…"),
            )
        });
        let weak = cx.entity().downgrade();
        let composer = cx.new(|cx| {
            let weak_alt = weak.clone();
            TextInputView::new(composer_state.clone(), cx)
                .frameless(true)
                .on_submit(move |text, window, cx| {
                    let _ = weak.update(cx, |this, cx| this.send_message(text, false, window, cx));
                })
                .on_submit_alternate(move |text, window, cx| {
                    let _ =
                        weak_alt.update(cx, |this, cx| this.send_message(text, true, window, cx));
                })
        });
        let composer_subscription = cx.observe(&composer_state, |this, state, cx| {
            this.composer_draft_changed(state.read(cx).content().to_string(), cx);
            this.sync_composer_menu(cx);
            cx.notify();
        });

        let sessions = store.summaries();
        let archived_sessions = store.archived_summaries();

        // Project registry: migrate the legacy single-folder registries once,
        // then reconcile session folders on every launch so a lost
        // projects.json never hides conversations from the sidebar.
        let projects_file_existed = ProjectStore::path().exists();
        let mut projects = ProjectStore::new();
        let legacy_project_paths: Vec<PathBuf> = sessions
            .iter()
            .chain(archived_sessions.iter())
            .filter_map(|summary| summary.project.clone())
            .collect();
        if !projects_file_existed {
            let mapping = projects.migrate_legacy(&config.projects, &legacy_project_paths);
            let mut config_changed = false;
            if !mapping.is_empty() {
                for group in &mut config.sidebar_collapsed_sections {
                    let Some(rest) = group.strip_prefix("project:") else {
                        continue;
                    };
                    let normalized = normalize_root(Path::new(rest));
                    if let Some((_, id)) = mapping
                        .iter()
                        .find(|(candidate, _)| candidate == &normalized)
                    {
                        *group = project_group_key(id);
                        config_changed = true;
                    }
                }
            }
            if !config.projects.is_empty() {
                config.projects.clear();
                config_changed = true;
            }
            if config_changed {
                let _ = config.save();
            }
            let _ = projects.save();
        } else if projects.reconcile(&legacy_project_paths) {
            let _ = projects.save();
        }
        let project_editor_name = cx.new(|cx| {
            TextInputState::new(
                cx,
                TextInputOptions::default()
                    .multiline(false)
                    .lines(1, 1)
                    .placeholder("项目名称"),
            )
        });
        let project_editor_name_subscription =
            cx.observe(&project_editor_name, |_, _, cx| cx.notify());
        let project_editor_name_view =
            cx.new(|cx| TextInputView::new(project_editor_name.clone(), cx));

        let session_rename_name = cx.new(|cx| {
            TextInputState::new(
                cx,
                TextInputOptions::default()
                    .multiline(false)
                    .lines(1, 1)
                    .placeholder("会话名称"),
            )
        });
        let session_rename_name_subscription =
            cx.observe(&session_rename_name, |this, _, cx| {
                if let Some(state) = this.session_rename.as_mut() {
                    state.error = None;
                }
                cx.notify();
            });
        let session_rename_weak = cx.entity().downgrade();
        let session_rename_name_view = cx.new(|cx| {
            TextInputView::new(session_rename_name.clone(), cx).on_submit(
                move |text, window, cx| {
                    let _ = session_rename_weak.update(cx, |this, cx| {
                        this.save_session_rename(Some(text), window, cx)
                    });
                },
            )
        });

        let palette_query = cx.new(|cx| {
            TextInputState::new(
                cx,
                TextInputOptions::default()
                    .multiline(false)
                    .lines(1, 1)
                    .placeholder("搜索会话或命令…"),
            )
        });
        let palette_weak = cx.entity().downgrade();
        let palette_input = cx.new(|cx| {
            TextInputView::new(palette_query.clone(), cx)
                .frameless(true)
                .on_submit(move |text, window, cx| {
                    let _ = palette_weak.update(cx, |this, cx| {
                        this.run_palette_selection(&text, window, cx);
                    });
                })
        });
        let palette_subscription = cx.observe(&palette_query, |_, _, cx| cx.notify());

        // Up/Down inside the palette must move the selection, not the text
        // cursor. GPUI dispatches matched actions before raw key listeners and
        // an action listener consumes the event by default, so neither a
        // backdrop `on_key_down` nor an ancestor `on_action` ever sees Up/Down
        // while the palette input is focused — and key bindings in ancestor
        // key contexts lose to the input's own `EffiTextInput` bindings
        // (deepest context wins). A global keystroke interceptor runs before
        // all of that and `stop_propagation` keeps the input out of the loop.
        let palette_keys_weak = cx.entity().downgrade();
        let palette_keys_subscription = cx.intercept_keystrokes(
            move |event: &gpui::KeystrokeEvent, _window, cx: &mut gpui::App| {
                let key = event.keystroke.key.as_str();
                if key != "up" && key != "down" {
                    return;
                }
                let open = palette_keys_weak
                    .read_with(cx, |this, _| this.palette_open)
                    .unwrap_or(false);
                if !open {
                    return;
                }
                let delta: i32 = if key == "up" { -1 } else { 1 };
                let _ = palette_keys_weak.update(cx, |this, cx| {
                    this.palette_move_selection(delta, cx);
                });
                cx.stop_propagation();
            },
        );

        let side = SidePaneState::new(cx, &config, startup_cwd);
        let settings = SettingsState::new(cx, &config, &models);

        // The composer's `/` and `@` menu owns the arrow keys, Enter/Tab and
        // Escape while it is open — the text input's own bindings would
        // otherwise move the caret or submit the half-typed token. Like the
        // palette, this global interceptor runs before action dispatch.
        let menu_keys_weak = cx.entity().downgrade();
        let composer_menu_keys_subscription = cx.intercept_keystrokes(
            move |event: &gpui::KeystrokeEvent, window, cx: &mut gpui::App| {
                let key = event.keystroke.key.as_str();
                let modifiers = event.keystroke.modifiers;
                let state = menu_keys_weak
                    .read_with(cx, |this, _| {
                        let composer = this.composer_state.read(cx);
                        let focused = composer.focus_handle().is_focused(window);
                        (
                            this.composer_menu.open,
                            this.composer_menu.items.len(),
                            focused,
                            composer.is_composing(),
                        )
                    })
                    .unwrap_or((false, 0, false, false));
                let (open, count, focused, composing) = state;
                if !open || !focused || composing {
                    return;
                }
                match key {
                    "up" | "down" if count > 0 => {
                        let delta = if key == "up" { -1 } else { 1 };
                        let _ = menu_keys_weak.update(cx, |this, cx| {
                            this.move_composer_menu_selection(delta, cx);
                        });
                        cx.stop_propagation();
                    }
                    "escape" => {
                        let _ = menu_keys_weak.update(cx, |this, cx| {
                            this.dismiss_composer_menu(cx);
                        });
                        cx.stop_propagation();
                    }
                    "enter"
                        if !modifiers.shift
                            && !modifiers.control
                            && !modifiers.platform
                            && count > 0 =>
                    {
                        let _ = menu_keys_weak.update(cx, |this, cx| {
                            let index = this
                                .composer_menu
                                .selected
                                .min(this.composer_menu.items.len().saturating_sub(1));
                            this.accept_composer_menu_item(index, window, cx);
                        });
                        cx.stop_propagation();
                    }
                    "tab" if count > 0 => {
                        let _ = menu_keys_weak.update(cx, |this, cx| {
                            let index = this
                                .composer_menu
                                .selected
                                .min(this.composer_menu.items.len().saturating_sub(1));
                            this.accept_composer_menu_item(index, window, cx);
                        });
                        cx.stop_propagation();
                    }
                    _ => {}
                }
            },
        );

        // Read the collapse flags before `config` moves into the struct.
        let sidebar_collapsed = config.sidebar_collapsed;
        let side_bar_open = config.side_bar_open;
        let mut view = Self {
            config,
            models,
            store,
            drafts,
            projects,
            runtime,
            app_tx,
            sessions,
            archived_sessions,
            active,
            active_run: None,
            pending_approvals: Vec::new(),
            edit_message: None,
            undo_dialog: None,
            edit_conflict: None,
            update: Default::default(),
            computer,
            computer_overlay: None,
            self_weak,
            _window_closed_subscription: window_closed_subscription,
            subagent_tasks: Vec::new(),
            subagent_task_index: HashMap::new(),
            subagent_history_limit: 30,
            subagent_services: Vec::new(),
            side,
            view_mode: ViewMode::Chat,
            graph: GraphView::new(),
            graph_save_generation: 0,
            pending_subagent_notifications: Vec::new(),
            pending_inputs: Vec::new(),
            queue_held: false,
            agent_ticker_active: false,
            persist_tx,
            last_persist: None,
            persist_flush_scheduled: false,
            draft_save_scheduled: false,
            bench_until: None,
            bench_stream: None,
            run_tick_stamp: 0,
            agent_tick_stamp: 0,
            refresh_generation: 0,
            agents_catalog: None,
            palette_open: false,
            palette_query,
            palette_input,
            palette_selected: 0,
            palette_selected_for: String::new(),
            palette_scroll: gpui::ScrollHandle::new(),
            _palette_keys_subscription: palette_keys_subscription,
            _palette_subscription: palette_subscription,
            composer_state,
            composer,
            _composer_subscription: composer_subscription,
            composer_menu: ComposerMenuState::default(),
            composer_attachments: Vec::new(),
            _composer_menu_keys_subscription: composer_menu_keys_subscription,
            settings_open: false,
            reward_open: false,
            settings,
            model_menu: ModelMenuState::default(),
            project_menu: ProjectMenuState::default(),
            session_menu: SessionMenuState::default(),
            session_rename: None,
            session_rename_name,
            session_rename_name_view,
            _session_rename_name_subscription: session_rename_name_subscription,
            project_editor: None,
            project_editor_name,
            project_editor_name_view,
            _project_editor_name_subscription: project_editor_name_subscription,
            theme,
            expanded_reasoning: HashSet::new(),
            expanded_tools: HashSet::new(),
            expanded_turns: HashSet::new(),
            collapsed_turns: HashSet::new(),
            chat: ChatList::new(),
            chat_dirty: true,
            prompt_rail: PromptRailState::default(),
            jump_flash: None,
            jump_flash_seq: 0,
            prompt_rail_frame_scheduled: false,
            scrolls: SmoothScrollers::new(),
            motion: MotionController::new(),
            sidebar_grouping_dirty: false,
            scroll_frame_scheduled: false,
            pane_frame_scheduled: false,
            sidebar_slide: None,
            sidebar_p: if sidebar_collapsed { 0.0 } else { 1.0 },
            side_pane_slide: None,
            side_p: if side_bar_open { 1.0 } else { 0.0 },
            run_ticker_active: false,
            last_error: None,
            notice: None,
            preview_mode: preview_launch,
            preview_sidebar_hover: false,
            preview_context_tip: false,
            sidebar_marquees: RefCell::new(HashMap::new()),
            sidebar_marquee_scheduled: false,
            #[cfg(feature = "stress")]
            stress: None,
            #[cfg(feature = "stress")]
            stress_ticker_active: false,
        };

        view.restore_composer_draft(cx);
        view.remember_active_draft(cx);
        view.chat.install_scroll_handler(cx.entity().downgrade());

        // Session persistence happens off the UI thread: the foreground only
        // stages a snapshot (clone + serialize are already done), and this
        // worker writes staged saves one at a time in send order.
        cx.spawn(async move |_weak, cx| {
            while let Some(save) = persist_rx.recv().await {
                let _ = cx
                    .background_executor()
                    .spawn(async move { save.write() })
                    .await;
            }
        })
        .detach();

        if view.runtime.is_none() {
            view.last_error = Some("无法创建后台运行时；Agent 功能不可用。".to_string());
        }

        // Reopen-time restore: project the active session's persisted children
        // into the agent catalog before anything renders.
        view.restore_subagent_history();

        if std::env::args().any(|argument| argument == "--bench") {
            let seconds = std::env::args()
                .skip_while(|argument| argument != "--bench")
                .nth(1)
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(8);
            view.seed_preview(cx);
            view.seed_stress_transcript(cx);
            view.start_bench(seconds, cx);
        }
        if std::env::args().any(|argument| argument == "--preview-stress") {
            view.seed_preview(cx);
            view.seed_stress_transcript(cx);
        }
        if std::env::args().any(|argument| argument == "--preview")
            || std::env::args().any(|argument| argument == "--preview-graph")
        {
            view.seed_preview(cx);
        }
        if std::env::args().any(|argument| argument == "--preview-graph") {
            view.seed_graph_preview(cx);
        }

        // Stress lab: `--stress [N]` launches the in-app concurrency run after
        // every other startup step (feature `stress` only).
        #[cfg(feature = "stress")]
        if let Some(launch) = crate::stress::parse_stress_args() {
            view.begin_stress(launch, cx);
        }
        if std::env::args().any(|argument| argument == "--preview-empty") {
            view.preview_mode = true;
        }
        // `--preview-palette` opens the command palette on its own (it used to
        // live inside `seed_preview`, which only runs for `--preview` and
        // `--preview-graph`, making the standalone flag a dead branch).
        if std::env::args().any(|argument| argument == "--preview-palette") {
            view.palette_open = true;
            cx.notify();
        }
        if std::env::args().any(|argument| argument == "--settings") {
            view.settings_open = true;
            cx.notify();
        }
        if std::env::args()
            .any(|argument| argument == "--settings-models" || argument == "--preview-models")
        {
            view.settings.section = SettingsSection::Models;
            view.settings_open = true;
            if std::env::args().any(|argument| argument == "--preview-models") {
                view.settings.selected_provider = Some("openai".to_string());
                view.settings.load_provider_form(&view.models, cx);
                view.settings.provider_status = Some((
                    true,
                    "预览：未配置 Key 时仍可使用离线 Demo，填写 Key 后即可请求真实模型。"
                        .to_string(),
                ));
            }
            cx.notify();
        }
        if std::env::args().any(|argument| argument == "--preview-sidepane-files") {
            view.side.open_tab(SidePaneTab::files());
            view.on_side_tab_activated(cx);
        }
        if std::env::args().any(|argument| argument == "--preview-sidepane-changes") {
            view.side.open_tab(SidePaneTab::changes());
            view.on_side_tab_activated(cx);
        }
        if std::env::args().any(|argument| argument == "--preview-sidebar-hover") {
            view.preview_mode = true;
            view.preview_sidebar_hover = true;
            cx.notify();
        }
        if std::env::args().any(|argument| argument == "--preview-context-tip") {
            view.preview_mode = true;
            view.preview_context_tip = true;
            // 灌入会话内容与用量，让进度环和 tips 有真实占比可看。
            if let Some(shared) = view.active_shared() {
                let mut session = lock_session(&shared);
                for index in 0..3 {
                    session.push_message(Message::user(format!(
                        "预览消息 {index}：帮我梳理这段日志里的报错模式，并挑出前三类高频问题。"
                    )));
                    let mut assistant = Message::assistant();
                    assistant.set_text(format!(
                        "预览回复 {index}：日志里主要是超时与限流两类问题；建议先从重试策略入手，\
                         再检查网关的并发上限配置，最后核对客户端的退避参数。"
                    ));
                    session.push_message(assistant);
                }
                session.usage.prompt_tokens = 121_400;
                session.usage.completion_tokens = 8_900;
                session.usage.total_tokens = 130_300;
                session.usage.cached_tokens = 96_500;
                session.usage.last_prompt_tokens = 91_800;
            }
            cx.notify();
        }
        if std::env::args().any(|argument| argument == "--preview-project-menu") {
            view.preview_mode = true;
            let target = view
                .projects
                .projects()
                .first()
                .map(|project| ProjectGroup {
                    id: Some(project.id.clone()),
                    key: project_group_key(&project.id),
                    name: project.name.clone(),
                    roots: project.root_paths(),
                })
                .or_else(|| {
                    view.sessions
                        .iter()
                        .find_map(|summary| view.project_group_for_summary(summary))
                })
                .unwrap_or_else(|| {
                    let workspace = view.config.workspace();
                    ProjectGroup {
                        id: None,
                        key: format!("project:legacy:{}", workspace.display()),
                        name: project_default_name(&workspace),
                        roots: vec![workspace],
                    }
                });
            view.project_menu.target = Some(target);
            view.project_menu.position = Some(point(px(240.0), px(300.0)));
            cx.notify();
        }
        if std::env::args().any(|argument| argument == "--settings-archive") {
            view.settings.section = SettingsSection::Archived;
            view.settings_open = true;
            cx.notify();
        }

        // A single foreground pump receives every harness event. After each
        // wake it drains everything that queued up while it was waiting, so a
        // token burst costs one entity update (and one notify) per frame
        // instead of one per delta.
        cx.spawn(async move |weak, cx| {
            loop {
                let Some(first) = app_rx.recv().await else {
                    break;
                };
                let updated = weak.update(cx, |this, cx| {
                    this.handle_app_event(first, cx);
                    while let Ok(event) = app_rx.try_recv() {
                        this.handle_app_event(event, cx);
                    }
                });
                if updated.is_err() {
                    break;
                }
            }
        })
        .detach();

        // One silent update check per launch, results via the same pump.
        if view.config.update_check_enabled {
            view.start_update_check(cx);
        }

        view
    }

    pub fn composer_focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.composer_state.read(cx).focus_handle()
    }

    pub(crate) fn active_resolved_model(&self) -> effibuddy_agent_core::ResolvedModel {
        match self.active_shared() {
            Some(shared) => {
                let session = lock_session(&shared);
                self.models.resolve_session(&session)
            }
            None => self.models.resolved_for(None),
        }
    }

    /// `--preview` seeds a representative conversation so the visual system can
    /// be inspected without making a live model call. Preview sessions are
    /// never written to disk.
    /// `--preview-stress`: a transcript of long markdown turns plus a live
    /// reveal backlog, so the render pipeline can be measured under the exact
    /// conditions that used to stall it (typewriter storm over huge bodies).
    fn seed_stress_transcript(&mut self, cx: &mut Context<Self>) {
        let Some(active) = &self.active else {
            return;
        };
        let mut session = lock_session(&active.shared);
        for turn in 0..STRESS_TURNS {
            let mut message = Message::assistant();
            message.set_text(stress_markdown_turn(turn));
            session.push_message(message);
        }
        let mut stream_message = Message::assistant();
        // The live answer is sized like a real long reply (~70KB): big enough
        // that a whole-message layout cost would dwarf the per-chunk cost.
        let mut full_text = String::with_capacity(80 * 1024);
        for turn in 0..12 {
            full_text.push_str(&stress_markdown_turn(STRESS_TURNS + turn));
        }
        stream_message.set_text(&full_text);
        session.push_message(stream_message.clone());
        session.usage.total_tokens = 570_600;
        session.touch();
        drop(session);

        // A live reveal backlog: the typewriter drains at its per-frame cap
        // while every frame re-lays-out the visible chunks. The bench loop
        // refills the cursor whenever the backlog runs low, so a whole bench
        // window stays saturated.
        self.chat_dirty = true;
        self.chat.prime_stream_backlog(&stream_message, 100);
        self.bench_stream = Some(stream_message.clone());
        self.chat.pin_to_bottom();
        self.graph.invalidate();
        cx.notify();
    }

    fn seed_preview(&mut self, cx: &mut Context<Self>) {
        self.preview_mode = true;
        let mut session = self.models.new_session_state(self.config.workspace());
        session.title = "Agent Core 结构梳理".to_string();
        session.steps = 5;

        session.push_message(Message::user(
            "先帮我确认 crates/agent-core 的结构，然后用 PTC 统计各 crate 的 Rust 文件数量。",
        ));

        let list_call = ToolCall::new("list_dir", json!({"path": "crates", "depth": 1}));
        let js_call = ToolCall::new(
            "run_js",
            json!({
                "code": "const crates = tools.list_dir({ path: \"crates\" }).entries;\nconst counts = {};\nfor (const c of crates) {\n  const files = tools.list_dir({ path: c.path, depth: 3 }).entries;\n  counts[c.name] = files.filter((f) => f.name.endsWith(\".rs\")).length;\n}\nreturn counts;"
            }),
        );
        let shell_call = ToolCall::new(
            "shell",
            json!({"command": "cargo test -p effibuddy-agent-core", "timeout_secs": 60}),
        );
        let agent_call = ToolCall::new(
            "agent",
            json!({
                "description": "审查路径沙箱",
                "prompt": "只读审查 crates/agent-tools/src/workspace.rs 的路径沙箱实现，找出越界写入与符号链接绕过风险。",
                "subagent_type": "reviewer"
            }),
        );

        let mut assistant = Message::assistant();
        assistant.push_reasoning(
            "用户要我先了解仓库结构，再用 PTC 编排多次工具调用。先列出 crates/，然后让脚本聚合各目录下的 Rust 文件数量。",
        );
        assistant.set_text(
            "我会先读取目录，再用一段 JavaScript 把多次列表调用合并成一个结果；最后跑一次核心测试确认环境。",
        );
        assistant.push_tool_call(list_call.clone());
        assistant.push_tool_call(js_call.clone());
        assistant.push_tool_call(shell_call.clone());
        assistant.push_tool_call(agent_call.clone());
        let assistant_id = assistant.id.clone();
        session.push_message(assistant);

        session.push_message(Message::from_tool_result(ToolResult::success(
            &list_call,
            serde_json::to_string_pretty(&json!({
                "path": "crates",
                "entries": [
                    {"name": "agent-core", "path": "crates/agent-core", "kind": "dir"},
                    {"name": "agent-tools", "path": "crates/agent-tools", "kind": "dir"},
                    {"name": "agent-js", "path": "crates/agent-js", "kind": "dir"},
                    {"name": "app", "path": "crates/app", "kind": "dir"},
                    {"name": "ui", "path": "crates/ui", "kind": "dir"}
                ],
                "truncated": false
            }))
            .unwrap(),
        )));
        let js_result = json!({
            "ok": true,
            "value": {"agent-core": 9, "agent-tools": 8, "agent-js": 1, "app": 4, "ui": 6},
            "logs": ["scanned 5 crates"],
            "tool_calls": [
                {"name": "list_dir", "ok": true, "duration_ms": 2},
                {"name": "list_dir", "ok": true, "duration_ms": 1}
            ],
            "duration_ms": 18
        });
        session.push_message(Message::from_tool_result(ToolResult::success(
            &js_call,
            serde_json::to_string_pretty(&js_result).unwrap(),
        )));
        session.push_message(Message::from_tool_result(ToolResult::success(
            &shell_call,
            "exit_code: 0\n\n--- stdout ---\nrunning 15 tests\ntest result: ok. 15 passed; 0 failed\n",
        )));
        session.push_message(Message::from_tool_result(
            ToolResult::success(
                &agent_call,
                "[Subagent hand-back] The text below is the final report of a subagent…\n\n  ## 审查结论\n  - `resolve_for_write` 先规范化父目录，再拼接文件名，逻辑正确。\n  - 建议为 `..` 组件增加显式拒绝，避免 Windows 短路径绕过。\n\nagentId: agent_preview",
            )
            .with_metadata(json!({
                "agent_id": "agent_preview",
                "profile": "reviewer",
                "background": false,
                "status": "completed"
            })),
        ));

        let mut final_message = Message::assistant();
        final_message.set_text(
            "## 结论\n\n`crates/` 下分为 5 个 crate。Rust 文件数量如下：\n\n| crate | .rs 文件 |\n|---|---|\n| agent-core | 9 |\n| agent-tools | 8 |\n| agent-js | 1 |\n| app | 4 |\n| ui | 6 |\n\n核心测试已通过：\n\n```text\ntest result: ok. 15 passed; 0 failed\n```\n\n**下一步**：需要我把这些数字写进 README 的架构章节吗？",
        );
        session.push_message(final_message);

        // A second, short multi-step turn: stays folded (no manual expand),
        // so the preview shows the collapsed summary + answer body state,
        // including the aggregate error icon for the failed edit.
        session.push_message(Message::user("顺手把统计补进 README 的架构章节。"));
        let read_call = ToolCall::new("read_file", json!({"path": "README.md"}));
        let edit_call = ToolCall::new(
            "edit_file",
            json!({"path": "docs/missing.md", "old_text": "## 架构", "new_text": "## 架构\n\n| crate | .rs 文件 |"}),
        );
        let mut step_one = Message::assistant();
        step_one.push_reasoning("先读取 README 的架构章节，找到合适的插入点。");
        step_one.set_text("先看一下 README 现在的结构。");
        step_one.push_tool_call(read_call.clone());
        step_one.push_tool_call(edit_call.clone());
        session.push_message(step_one);
        session.push_message(Message::from_tool_result(ToolResult::success(
            &read_call,
            "# EffiBuddy\n\n## 架构\n\n（待补充）\n",
        )));
        session.push_message(Message::from_tool_result(ToolResult::failure(
            &edit_call,
            "错误：docs/missing.md 不存在，目标文件应为 README.md。",
        )));
        let mut step_two = Message::assistant();
        step_two.push_reasoning("README 里已经有架构章节，直接补在末尾即可。");
        step_two.set_text("已把各 crate 的 Rust 文件数量补进 README 的架构章节。");
        session.push_message(step_two);

        session.apply_todos(vec![
            effibuddy_agent_core::TodoItem::new("确认 crates/ 目录结构", TodoStatus::Completed),
            effibuddy_agent_core::TodoItem::new("用 PTC 聚合 Rust 文件数量", TodoStatus::Completed),
            effibuddy_agent_core::TodoItem::new("运行 agent-core 测试", TodoStatus::Completed),
            effibuddy_agent_core::TodoItem::new("补充 README 架构说明", TodoStatus::Pending),
        ]);
        session.usage.prompt_tokens = 18_420;
        session.usage.completion_tokens = 2_140;
        session.usage.total_tokens = 20_560;
        // 预览里让占用环 / 概览的上下文构成有真实数据可画。
        session.usage.last_prompt_tokens = 18_420;
        session.touch();

        self.active = Some(ActiveSession {
            shared: new_shared_session(session.clone()),
        });
        self.chat.reset_session();
        self.prompt_rail = PromptRailState::default();
        self.jump_flash = None;
        self.jump_flash_seq = 0;
        self.chat_dirty = true;
        self.chat.pin_to_bottom();
        self.graph.invalidate();
        self.graph.clear_selection();
        self.sessions = vec![SessionSummary {
            id: session.id.clone(),
            title: session.title.clone(),
            updated_at: session.updated_at,
            message_count: session.messages.len(),
            project: session.project.clone(),
            project_id: session.project_id.clone(),
            pinned: session.pinned,
            unread: session.unread,
        }];
        self.expanded_reasoning.insert(assistant_id.clone());
        self.expanded_tools.insert(js_call.id.clone());
        // The fixture's tool step + final answer merge into one turn block;
        // pre-expand it so the activity rows are visible in the preview.
        self.expanded_turns.insert(format!("turn:{assistant_id}"));

        // A finished reviewer subagent plus its child transcript.
        let mut child = self.models.new_session_state(self.config.workspace());
        child.title = "reviewer · 审查路径沙箱".to_string();
        child.parent_id = Some(session.id.clone());
        child.agent_name = Some("reviewer".to_string());
        child.agent_description = Some("审查路径沙箱".to_string());
        child.push_message(Message::user(
            "只读审查 crates/agent-tools/src/workspace.rs 的路径沙箱实现，找出越界写入与符号链接绕过风险。",
        ));
        let mut child_assistant = Message::assistant();
        child_assistant.push_reasoning(
            "先看 Workspace::new 与 resolve_for_write 的规范化顺序，再检查测试是否覆盖 .. 和符号链接。",
        );
        child_assistant.set_text(
            "## 审查结论\n\n读取路径与写入路径都先经过 `canonicalize`，边界判断在真实路径上进行，整体方向正确。\n\n**需要补强**\n\n1. Windows 短路径（8.3 名称）可能让 `starts_with` 判断出现偏差，建议对每个组件用 `Component::Normal` 过滤。\n2. 写入新文件时父目录不存在会直接报错，错误信息可以提示更具体的相对路径。\n\n**已验证**\n\n- `../outside.txt` 被拒绝。\n- 符号链接指向工作区外时被拒绝。",
        );
        child.push_message(child_assistant);
        child.push_message(Message::from_tool_result(ToolResult::success(
            &ToolCall::new(
                "read_file",
                json!({"path": "crates/agent-tools/src/workspace.rs"}),
            ),
            "1 | use std::path::{Component, Path, PathBuf};\n…",
        )));
        child.usage.total_tokens = 8_420;
        child.usage.prompt_tokens = 7_900;
        child.usage.completion_tokens = 520;
        child.touch();
        let child_shared = new_shared_session(child.clone());
        self.push_subagent_task(AgentTask {
            agent_id: "agent_preview".to_string(),
            name: "path-reviewer".to_string(),
            profile: "reviewer".to_string(),
            color: SubagentColor::Purple,
            description: "审查路径沙箱".to_string(),
            status: SubagentStatus::Completed,
            background: false,
            started_at: Instant::now() - Duration::from_secs(46),
            finished_at: Some(Instant::now() - Duration::from_secs(11)),
            child_session_id: child.id.clone(),
            parent_call_id: agent_call.id.clone(),
            parent_session_id: session.id.clone(),
            parent_agent_id: Some("main".to_string()),
            parent_agent_name: Some("main".to_string()),
            group_id: None,
            group_name: None,
            depth: 1,
            visibility: AgentVisibility::Public,
            read_only: true,
            can_spawn: true,
            task_prompt: "只读审查 crates/agent-tools/src/workspace.rs 的路径沙箱实现，找出越界写入与符号链接绕过风险。".to_string(),
            output_schema: None,
            provider_id: Some(self.models.resolved_for(None).provider.id.clone()),
            model: self.models.resolved_for(None).model.id.clone(),
            shared: child_shared,
            report: Some(
                "审查结论：路径规范化与边界判断正确，建议补强 Windows 短路径处理。".to_string(),
            ),
            error: None,
            data: None,
            tool_uses: 3,
            total_tokens: 8_420,
            duration_ms: 35_000,
            waiting_approval: false,
            restored: false,
        });

        if std::env::args().any(|argument| argument == "--preview-agents")
            || std::env::args().any(|argument| argument == "--preview-graph")
        {
            let mut running_child = self.models.new_session_state(self.config.workspace());
            running_child.title = "explore · 搜索工具注册点".to_string();
            running_child.parent_id = Some(session.id.clone());
            running_child.agent_name = Some("explore".to_string());
            running_child.push_message(Message::user(
                "找出所有工具注册点，并确认哪些工具会修改工作区。",
            ));
            let mut running_assistant = Message::assistant();
            running_assistant.push_reasoning(
                "先用 grep 搜索 ToolRegistry::register，再逐个确认 approval 策略。",
            );
            running_assistant.set_text("正在搜索 `ToolRegistry::register` 的调用点…");
            running_child.push_message(running_assistant);
            self.push_subagent_task(AgentTask {
                agent_id: "agent_preview_running".to_string(),
                name: "tool-scout".to_string(),
                profile: "explore".to_string(),
                color: SubagentColor::Green,
                description: "搜索工具注册点".to_string(),
                status: SubagentStatus::Running,
                background: true,
                started_at: Instant::now() - Duration::from_secs(12),
                finished_at: None,
                child_session_id: running_child.id.clone(),
                parent_call_id: "call_preview_running".to_string(),
                parent_session_id: session.id.clone(),
                parent_agent_id: Some("main".to_string()),
                parent_agent_name: Some("main".to_string()),
                group_id: None,
                group_name: None,
                depth: 1,
                visibility: AgentVisibility::Public,
                read_only: true,
                can_spawn: true,
                task_prompt: "找出所有工具注册点，并确认哪些工具会修改工作区。".to_string(),
                output_schema: None,
                provider_id: Some(self.models.resolved_for(None).provider.id.clone()),
                model: self.models.resolved_for(None).model.id.clone(),
                shared: new_shared_session(running_child),
                report: None,
                error: None,
                data: None,
                tool_uses: 2,
                total_tokens: 0,
                duration_ms: 0,
                waiting_approval: false,
                restored: false,
            });
            self.side.open_tab(SidePaneTab::agents());
            self.ensure_agent_ticker(cx);
        }

        if std::env::args().any(|argument| argument == "--preview-subagent") {
            self.open_agent_tab("agent_preview", cx);
        }

        if std::env::args().any(|argument| argument == "--preview-approval") {
            let approval_call = ToolCall::new(
                "write_file",
                json!({
                    "path": "crates/app/src/theme_preview.rs",
                    "content": "// generated preview\n"
                }),
            );
            self.pending_approvals.push(PendingApproval {
                agent_id: None,
                request: ApprovalRequest::new(
                    &approval_call,
                    "写入 1,240 字节到 `crates/app/src/theme_preview.rs`",
                ),
            });
        }
        self.side.open = true;
        cx.notify();
    }

    /// `--preview-graph` visual fixture: two public agents inside one group,
    /// a typed hand-off message and a persisted group frame.
    fn seed_graph_preview(&mut self, cx: &mut Context<Self>) {
        self.preview_mode = true;
        self.view_mode = ViewMode::Graph;
        let group_id = "group_preview".to_string();
        for task in &mut self.subagent_tasks {
            if task.agent_id == "agent_preview_running" || task.agent_id == "agent_preview" {
                task.group_id = Some(group_id.clone());
                task.group_name = Some("调研 → 验收".to_string());
            }
        }
        let Some(shared) = self.active_shared() else {
            return;
        };
        {
            let mut session = lock_session(&shared);
            session.title = "智能体关系图预览".to_string();
            let now = Utc::now();
            session
                .graph
                .upsert_node(effibuddy_agent_core::AgentGraphNodeRecord {
                    id: group_id.clone(),
                    name: "调研 → 验收".to_string(),
                    description: "探索工具注册点，再由 reviewer 验收".to_string(),
                    kind: effibuddy_agent_core::AgentNodeKind::Group,
                    profile: "group".to_string(),
                    parent_id: Some("main".to_string()),
                    parent_name: Some("main".to_string()),
                    owner_id: Some("main".to_string()),
                    owner_name: Some("main".to_string()),
                    visibility: AgentVisibility::Public,
                    group_id: None,
                    group_name: None,
                    depth: 1,
                    status: "运行中".to_string(),
                    session_id: None,
                    tool_uses: 0,
                    total_tokens: 0,
                    task: Some("从调研到验收的全流程".to_string()),
                    output: None,
                    data: None,
                    input: Some(json!({
                        "topic": "EffiBuddy 工具注册点",
                        "requirements": ["定位注册点", "确认写权限"]
                    })),
                    input_schema: Some(json!({
                        "type": "object",
                        "required": ["topic"],
                        "properties": {
                            "topic": {"type": "string"},
                            "requirements": {"type": "array", "items": {"type": "string"}}
                        }
                    })),
                    output_schema: Some(json!({
                        "type": "object",
                        "required": ["verdict"],
                        "properties": {
                            "verdict": {"enum": ["pass", "fail"]},
                            "notes": {"type": "string"}
                        }
                    })),
                    started_at: Some(now - chrono::Duration::seconds(46)),
                    finished_at: None,
                });
            let message = effibuddy_agent_core::AgentMessage {
                id: "msg_preview_handoff".to_string(),
                from_id: "agent_preview_running".to_string(),
                from_name: "tool-scout".to_string(),
                to_id: "agent_preview".to_string(),
                to_name: "path-reviewer".to_string(),
                kind: effibuddy_agent_core::AgentMessageKind::Data,
                text: "工具注册点已定位，附上需要复查的写权限清单。".to_string(),
                data: Some(json!({
                    "registrations": ["read_file", "write_file", "edit_file", "shell"],
                    "mutating": ["write_file", "edit_file", "shell"]
                })),
                created_at: now,
            };
            graph_persist::record_message(&mut session.graph, &message, false, true);
            // Fit the demo (main + group + inspector) into the 1280px preview
            // window; the canvas stays freely pannable/zoomable from here.
            session.graph.zoom = 0.62;
            session.graph.pan = effibuddy_agent_core::GraphPoint { x: 12.0, y: 12.0 };
            session.touch();
        }
        self.graph.invalidate();
        self.graph.select_node("agent_preview_running");
        self.ensure_agent_ticker(cx);
        cx.notify();
    }

    fn active_id(&self) -> Option<String> {
        self.active
            .as_ref()
            .map(|active| lock_session(&active.shared).id.clone())
    }

    /// Drop edit/undo UI when the transcript under it changes identity.
    fn clear_message_editing(&mut self) {
        self.edit_message = None;
        self.undo_dialog = None;
        self.edit_conflict = None;
    }

    /// A rewind mutates the transcript and the workspace, so it must wait for
    /// the main run *and* every background child to stop.
    fn message_rewind_blocked(&mut self, cx: &mut Context<Self>) -> bool {
        if self.active_run.is_some() {
            self.notice = Some("任务运行中，结束后才能编辑或撤销消息。".to_string());
            self.chat_dirty = true;
            cx.notify();
            return true;
        }
        if self
            .subagent_tasks
            .iter()
            .any(|task| task.status == SubagentStatus::Running)
        {
            self.notice = Some("子代理仍在运行，请先停止后再编辑或撤销消息。".to_string());
            self.chat_dirty = true;
            cx.notify();
            return true;
        }
        false
    }

    pub(crate) fn active_shared(&self) -> Option<SharedSession> {
        self.active.as_ref().map(|active| active.shared.clone())
    }

    /// Sidebar grouping key for the conversation's project (or 任务).
    fn draft_scope_for_session(session: &effibuddy_agent_core::SessionState) -> String {
        crate::draft_store::scope_key(session.project_id.as_deref(), session.project.as_deref())
    }

    /// True when the active conversation is still the unsent draft of `scope`.
    /// Repeated “新建任务” clicks must land here instead of replacing it.
    fn active_is_unsent_draft_for(&self, scope: &str) -> bool {
        let Some(active) = &self.active else {
            return false;
        };
        let session = lock_session(&active.shared);
        session.messages.is_empty() && Self::draft_scope_for_session(&session) == scope
    }

    /// Composer observer: keep the in-memory draft current so a switch or a
    /// crash-free shutdown never loses the typed text, then debounce the tiny
    /// `drafts.json` write.
    pub(crate) fn composer_draft_changed(&mut self, text: String, cx: &mut Context<Self>) {
        if self.preview_mode {
            return;
        }
        let Some(id) = self.active_id() else {
            return;
        };
        if self.drafts.set_text(&id, text) {
            self.schedule_draft_save(cx);
        }
    }

    fn schedule_draft_save(&mut self, cx: &mut Context<Self>) {
        if self.preview_mode || self.draft_save_scheduled {
            return;
        }
        self.draft_save_scheduled = true;
        cx.spawn(async move |weak, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(350))
                .await;
            let _ = weak.update(cx, |this, _| {
                this.draft_save_scheduled = false;
                let _ = this.drafts.save();
            });
        })
        .detach();
    }

    fn save_drafts_now(&mut self) {
        if self.preview_mode {
            return;
        }
        let _ = self.drafts.save();
    }

    /// Seed the composer from the active conversation's saved draft. Silent:
    /// switching must not re-enter the change observer for the target session.
    fn restore_composer_draft(&mut self, cx: &mut Context<Self>) {
        if self.preview_mode {
            return;
        }
        let Some(id) = self.active_id() else {
            return;
        };
        let text = self.drafts.text(&id).to_string();
        self.composer_attachments = self.drafts.attachments(&id);
        // The menu belongs to the outgoing conversation's workspace.
        self.composer_menu.close();
        self.composer_state
            .update(cx, |state, _| state.set_content_silent(text.clone()));
        // A restored draft may still reference `$skill`; make its chip (and the
        // attached body) match what the text says.
        self.sync_skill_attachments(&text, cx);
    }

    /// Persist the staged file/image/skill chips for the active conversation.
    /// They live in their own per-session file, so text typing never
    /// re-serializes a multi-megabyte image payload.
    pub(crate) fn persist_composer_attachments(&mut self, cx: &mut Context<Self>) {
        let _ = cx;
        if self.preview_mode {
            return;
        }
        let Some(id) = self.active_id() else {
            return;
        };
        if let Err(error) = self
            .drafts
            .set_attachments(&id, &self.composer_attachments)
        {
            self.last_error = Some(error);
        }
    }

    /// Remember the active conversation as its scope's unsent draft (when it
    /// still has no messages) and as the app's last-open session.
    fn remember_active_draft(&mut self, cx: &mut Context<Self>) {
        let _ = cx;
        if self.preview_mode {
            return;
        }
        let Some(active) = &self.active else {
            return;
        };
        let (id, empty, scope) = {
            let session = lock_session(&active.shared);
            (
                session.id.clone(),
                session.messages.is_empty(),
                Self::draft_scope_for_session(&session),
            )
        };
        let mut changed = self.drafts.set_last_active(&id);
        if empty {
            if self.drafts.scope_session(&scope) != Some(id.as_str()) {
                self.drafts.bind_scope(&scope, &id);
                changed = true;
            }
        } else if self.drafts.is_unsent(&id) {
            // A crash between sending the first message and the RunStarted
            // event can leave the binding behind; the transcript is history
            // now, so stop offering it as an invisible draft. Keep any text
            // typed for the next turn.
            self.drafts.unbind_session(&id);
            changed = true;
        }
        if changed {
            self.save_drafts_now();
        }
    }

    /// First user message sent: the conversation stops being a draft and must
    /// appear in the sidebar immediately. Write it synchronously (the session
    /// is one message long at this point) so the refresh that follows sees it.
    fn publish_active_draft(&mut self, cx: &mut Context<Self>) -> bool {
        let _ = cx;
        if self.preview_mode {
            return false;
        }
        let Some(shared) = self.active_shared() else {
            return false;
        };
        let (id, has_messages) = {
            let session = lock_session(&shared);
            (session.id.clone(), !session.messages.is_empty())
        };
        if !has_messages || !self.drafts.unbind_session(&id) {
            return false;
        }
        let _ = self.drafts.save();
        if let Err(error) = self.save_active_metadata(&shared) {
            self.last_error = Some(error);
        }
        true
    }

    /// Minimum wall-clock spacing between session serializations. A chatty
    /// run fires a persist per tool boundary; serializing a megabyte-scale
    /// session that often — even off-thread — hammers the (mounted) disk.
    const PERSIST_MIN_INTERVAL: Duration = Duration::from_millis(800);

    /// Persist the active session. Cloning and JSON serialization run on the
    /// background executor: they used to run on the UI thread, which stalled
    /// the frame for megabyte-scale transcripts. Throttled with a trailing
    /// flush so bursty runs coalesce into one write per quiet period.
    /// Non-stress builds never show the stress dashboard; the render call site
    /// is compiled either way.
    #[cfg(not(feature = "stress"))]
    pub(crate) fn stress_dashboard_active(&self) -> bool {
        false
    }

    /// Stress dashboard content (feature `stress`).
    #[cfg(feature = "stress")]
    fn stress_content(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        self.render_stress(theme, cx)
    }

    /// Non-stress builds never reach this: [`Self::stress_dashboard_active`]
    /// is always false, so an empty element is a safe placeholder.
    #[cfg(not(feature = "stress"))]
    fn stress_content(&mut self, _theme: &Theme, _cx: &mut Context<Self>) -> AnyElement {
        div().into_any_element()
    }

    pub(crate) fn persist_active(&mut self, cx: &mut Context<Self>) {
        if self.preview_mode {
            return;
        }
        // Stress lab: the active session must keep its pre-stress disk state;
        // stress children persist through the swapped RAM store instead.
        #[cfg(feature = "stress")]
        if self.stress.is_some() {
            return;
        }
        let Some(active) = &self.active else {
            return;
        };
        let now = Instant::now();
        if let Some(last) = self.last_persist
            && now.duration_since(last) < Self::PERSIST_MIN_INTERVAL
        {
            if !self.persist_flush_scheduled {
                self.persist_flush_scheduled = true;
                cx.spawn(async move |weak, cx| {
                    cx.background_executor()
                        .timer(Self::PERSIST_MIN_INTERVAL)
                        .await;
                    let _ = weak.update(cx, |this, cx| {
                        this.persist_flush_scheduled = false;
                        this.persist_active(cx);
                    });
                })
                .detach();
            }
            return;
        }
        self.last_persist = Some(now);
        let shared = active.shared.clone();
        let store = self.store.clone();
        let persist_tx = self.persist_tx.clone();
        cx.background_executor()
            .spawn(async move {
                let snapshot = lock_session(&shared).clone();
                if let Ok(save) = store.stage(&snapshot) {
                    let _ = persist_tx.send(save);
                }
            })
            .detach();
    }

    /// Re-list sessions on a background thread; the newest scan wins.
    pub(crate) fn refresh_sessions(&mut self, cx: &mut Context<Self>) {
        self.refresh_generation += 1;
        let generation = self.refresh_generation;
        let store = self.store.clone();
        cx.spawn(async move |weak, cx| {
            let summaries = cx
                .background_executor()
                .spawn(async move { store.summaries() })
                .await;
            weak.update(cx, |this, cx| {
                if this.refresh_generation == generation {
                    // Only open a transition when the sidebar ordering really
                    // changes. A session's title/updated_at metadata often
                    // changes without moving it; starting a 220ms frame loop
                    // for those refreshes would re-render the whole view for
                    // no visible reason. Pin/unpin sets `sidebar_grouping_dirty`
                    // because it changes the rendered groups without changing
                    // the raw updated-at order.
                    let grouping_dirty = std::mem::take(&mut this.sidebar_grouping_dirty);
                    let order_changed = this.sessions.len() != summaries.len()
                        || this
                            .sessions
                            .iter()
                            .zip(&summaries)
                            .any(|(old, new)| old.id != new.id);
                    if grouping_dirty {
                        // Pin/unpin crosses sidebar groups: a long move that
                        // users often repeat rapidly — give it the critically
                        // damped spring so interruption keeps momentum.
                        this.motion.begin(MotionSpec::SPRING_LAYOUT);
                    } else if order_changed {
                        // Snapshot the last painted row positions before the
                        // new ordering lands; `MotionItem` turns the delta into
                        // a paint-time FLIP offset.
                        this.motion.begin(MotionSpec::LAYOUT);
                    }
                    this.sessions = summaries;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn send_message(
        &mut self,
        text: String,
        steer: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = text.trim().to_string();
        // A `$skill` reference may have been typed rather than picked from the
        // menu; stage/reconcile its attachment before deciding the message is
        // empty.
        self.sync_skill_attachments(&text, cx);
        if text.is_empty() && self.composer_attachments.is_empty() {
            return;
        }
        if self.active_run.is_some() {
            if self.edit_message.is_some() {
                // Stay in edit mode; the staged attachments remain staged.
                self.restore_composer_text(text, window, cx);
                return;
            }
            let attachments = std::mem::take(&mut self.composer_attachments);
            self.persist_composer_attachments(cx);
            // Running: never reject. Enter queues for a fresh turn; Ctrl+Enter
            // hands the message to the harness for the next step boundary.
            if steer {
                let steer_id = self
                    .active_run
                    .as_ref()
                    .expect("checked active_run above")
                    .handle
                    .steer_with_attachments(text.clone(), attachments.clone());
                self.pending_inputs.push(PendingInput {
                    id: steer_id,
                    text,
                    kind: PendingInputKind::Steer,
                    attachments,
                });
                self.notice = Some("将在下一个步骤边界插入当前回合。".to_string());
            } else {
                self.pending_inputs.push(PendingInput {
                    id: effibuddy_agent_core::ids::new_id(),
                    text,
                    kind: PendingInputKind::Queue,
                    attachments,
                });
                self.notice = Some("已加入队列，当前回合结束后自动发送。".to_string());
            }
            cx.notify();
            return;
        }
        let attachments = std::mem::take(&mut self.composer_attachments);
        self.persist_composer_attachments(cx);
        if self.edit_message.is_some() {
            self.commit_edit_message(text, attachments, window, cx);
            return;
        }
        self.start_run(
            Some(RunInput {
                text,
                attachments,
            }),
            cx,
        );
    }

    /// Drop one pending input from the queue bar.
    pub(crate) fn remove_pending_input(&mut self, id: &str, cx: &mut Context<Self>) {
        self.pending_inputs.retain(|input| input.id != id);
        if self.pending_inputs.is_empty() {
            self.queue_held = false;
        }
        cx.notify();
    }

    /// "Send now" on a queued item. While a run is active this converts it
    /// into a steer (jumps ahead of the remaining queue); when idle it starts
    /// the run immediately — the manual disposition for a held queue.
    pub(crate) fn send_pending_input_now(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(position) = self.pending_inputs.iter().position(|input| input.id == id) else {
            return;
        };
        let Some(input) = self.pending_inputs.get(position) else {
            return;
        };
        if input.kind == PendingInputKind::Steer {
            return; // already handed to the harness; injection is pending
        }
        let text = input.text.clone();
        let attachments = input.attachments.clone();
        let running = self.active_run.is_some();
        self.pending_inputs.remove(position);
        self.queue_held = false;
        if running {
            let steer_id = self
                .active_run
                .as_ref()
                .expect("running checked above")
                .handle
                .steer_with_attachments(text.clone(), attachments.clone());
            self.pending_inputs.push(PendingInput {
                id: steer_id,
                text,
                kind: PendingInputKind::Steer,
                attachments,
            });
            self.notice = Some("将在下一个步骤边界插入当前回合。".to_string());
        } else {
            self.notice = None;
            self.start_run(Some(RunInput { text, attachments }), cx);
            return;
        }
        cx.notify();
    }

    /// Enter edit mode for a user message. The composer receives the original
    /// text; submitting trims the transcript at this message and, by default,
    /// restores every file the turn changed.
    pub(crate) fn begin_edit_message(
        &mut self,
        message_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.message_rewind_blocked(cx) {
            return;
        }
        let Some(shared) = self.active_shared() else {
            return;
        };
        let root = crate::checkpoint::checkpoint_root();
        let (text, plan, attachments) = {
            let session = lock_session(&shared);
            let Some(message) = session
                .messages
                .iter()
                .find(|message| message.id == message_id)
            else {
                self.notice = Some("这条消息已不在当前会话中。".to_string());
                self.chat_dirty = true;
                cx.notify();
                return;
            };
            if message.role != effibuddy_agent_core::Role::User
                || message.synthetic
                || message.agent_message.is_some()
            {
                self.notice = Some("只能编辑用户输入的消息。".to_string());
                self.chat_dirty = true;
                cx.notify();
                return;
            }
            let Ok(plan) = crate::checkpoint::plan_restore_in(
                &session,
                message_id,
                &root,
                &self.project_roots_for_session_state(&session),
            ) else {
                self.notice = Some("无法读取这轮消息的文件检查点。".to_string());
                self.chat_dirty = true;
                cx.notify();
                return;
            };
            (message.text(), plan, message.attachments.clone())
        };
        let include_files = plan.has_file_changes();
        self.edit_message = Some(EditMessageState {
            message_id: message_id.to_string(),
            plan,
            include_files,
        });
        self.undo_dialog = None;
        self.edit_conflict = None;
        // Editing re-opens the message the way it was sent: its files/images
        // come back as composer chips.
        self.composer_attachments = attachments;
        self.persist_composer_attachments(cx);
        self.composer_state
            .update(cx, |state, cx| state.set_content(text, window, cx));
        self.composer_focus_handle(cx).focus(window);
        cx.notify();
    }

    pub(crate) fn cancel_edit_message(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_message = None;
        self.edit_conflict = None;
        self.composer_attachments.clear();
        self.persist_composer_attachments(cx);
        self.composer_state
            .update(cx, |state, cx| state.clear(window, cx));
        self.composer_focus_handle(cx).focus(window);
        cx.notify();
    }

    pub(crate) fn toggle_edit_files(&mut self, cx: &mut Context<Self>) {
        if let Some(edit) = &mut self.edit_message {
            edit.include_files = !edit.include_files;
            cx.notify();
        }
    }

    /// Submit from edit mode. The plan is recomputed here so a file changed
    /// while the composer was open cannot be clobbered silently.
    fn commit_edit_message(
        &mut self,
        text: String,
        attachments: Vec<UserAttachment>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(mut edit) = self.edit_message.take() else {
            self.composer_attachments = attachments;
            self.persist_composer_attachments(cx);
            return;
        };
        if self.message_rewind_blocked(cx) {
            self.edit_message = Some(edit);
            self.composer_attachments = attachments;
            self.restore_composer_text(text, window, cx);
            return;
        }
        if !self.run_ready(cx) {
            self.edit_message = Some(edit);
            self.composer_attachments = attachments;
            self.restore_composer_text(text, window, cx);
            return;
        }
        let Some(shared) = self.active_shared() else {
            self.edit_message = Some(edit);
            self.composer_attachments = attachments;
            self.persist_composer_attachments(cx);
            return;
        };
        let root = crate::checkpoint::checkpoint_root();
        let plan = {
            let session = lock_session(&shared);
            match crate::checkpoint::plan_restore_in(
                &session,
                &edit.message_id,
                &root,
                &self.project_roots_for_session_state(&session),
            ) {
                Ok(plan) => plan,
                Err(error) => {
                    self.edit_message = Some(edit);
                    self.last_error = Some(error);
                    self.composer_attachments = attachments;
                    self.restore_composer_text(text, window, cx);
                    return;
                }
            }
        };
        edit.plan = plan;

        if edit.include_files && !edit.plan.can_restore() {
            // Conflict dialogs are rare; clone the plan so the edit state can
            // be restored if the user cancels.
            let plan = edit.plan.clone();
            let message_id = edit.message_id.clone();
            self.edit_message = Some(edit);
            self.edit_conflict = Some(EditConflictState {
                message_id,
                text,
                plan,
                attachments,
            });
            window.blur();
            cx.notify();
            return;
        }

        let restore = edit.include_files && edit.plan.has_file_changes() && edit.plan.can_restore();
        match self.trim_conversation(&edit.message_id, restore.then_some(&edit.plan), cx) {
            Ok(restored) => {
                self.edit_message = None;
                self.edit_conflict = None;
                self.composer_state
                    .update(cx, |state, cx| state.clear(window, cx));
                self.last_error = None;
                self.start_run(
                    Some(RunInput {
                        text,
                        attachments,
                    }),
                    cx,
                );
                self.composer_focus_handle(cx).focus(window);
                if self.active_run.is_some() && restored > 0 {
                    self.notice = Some(format!("已还原 {restored} 个文件，正在发送…"));
                    cx.notify();
                }
            }
            Err(error) => {
                self.edit_message = Some(edit);
                self.last_error = Some(error);
                self.composer_attachments = attachments;
                self.restore_composer_text(text, window, cx);
            }
        }
    }

    fn restore_composer_text(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.composer_state
            .update(cx, |state, cx| state.set_content(text, window, cx));
        self.persist_composer_attachments(cx);
        self.composer_focus_handle(cx).focus(window);
        self.edit_conflict = None;
        self.chat_dirty = true;
        cx.notify();
    }

    /// Open the secondary confirmation for undoing a user message.
    pub(crate) fn request_undo_message(
        &mut self,
        message_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.message_rewind_blocked(cx) {
            return;
        }
        if self.edit_message.is_some() {
            self.notice = Some("请先取消当前的消息编辑。".to_string());
            self.chat_dirty = true;
            cx.notify();
            return;
        }
        let Some(shared) = self.active_shared() else {
            return;
        };
        let root = crate::checkpoint::checkpoint_root();
        let (excerpt, plan) = {
            let session = lock_session(&shared);
            let Some(message) = session
                .messages
                .iter()
                .find(|message| message.id == message_id)
            else {
                self.notice = Some("这条消息已不在当前会话中。".to_string());
                self.chat_dirty = true;
                cx.notify();
                return;
            };
            if message.role != effibuddy_agent_core::Role::User
                || message.synthetic
                || message.agent_message.is_some()
            {
                self.notice = Some("只能撤销用户输入的消息。".to_string());
                self.chat_dirty = true;
                cx.notify();
                return;
            }
            let excerpt = preview_text(&message.text(), 120);
            let Ok(plan) = crate::checkpoint::plan_restore_in(
                &session,
                message_id,
                &root,
                &self.project_roots_for_session_state(&session),
            ) else {
                self.notice = Some("无法读取这轮消息的文件检查点。".to_string());
                self.chat_dirty = true;
                cx.notify();
                return;
            };
            (excerpt, plan)
        };
        self.undo_dialog = Some(UndoDialogState {
            message_id: message_id.to_string(),
            excerpt,
            plan,
        });
        self.edit_conflict = None;
        // The dialog has no text input; drop composer focus so typing cannot
        // edit the transcript draft behind the modal.
        window.blur();
        cx.notify();
    }

    pub(crate) fn close_undo_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.undo_dialog = None;
        self.composer_focus_handle(cx).focus(window);
        cx.notify();
    }

    /// Cancel the edit-conflict dialog and hand the drafted text back to the
    /// composer, still in edit mode.
    pub(crate) fn dismiss_edit_conflict(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(conflict) = self.edit_conflict.take() {
            self.composer_state.update(cx, |state, cx| {
                state.set_content(conflict.text, window, cx);
            });
            self.composer_attachments = conflict.attachments;
            self.persist_composer_attachments(cx);
            self.composer_focus_handle(cx).focus(window);
        }
        cx.notify();
    }

    /// Resolve an edit conflict by trimming only the conversation and sending
    /// the edited text; the conflicting files are left untouched.
    pub(crate) fn send_edit_without_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conflict) = self.edit_conflict.take() else {
            return;
        };
        if self.message_rewind_blocked(cx) {
            self.edit_conflict = Some(conflict);
            return;
        }
        if !self.run_ready(cx) {
            self.edit_conflict = Some(conflict);
            return;
        }
        match self.trim_conversation(&conflict.message_id, None, cx) {
            Ok(_) => {
                self.edit_message = None;
                self.composer_state
                    .update(cx, |state, cx| state.clear(window, cx));
                self.last_error = None;
                self.start_run(
                    Some(RunInput {
                        text: conflict.text,
                        attachments: conflict.attachments,
                    }),
                    cx,
                );
                self.composer_focus_handle(cx).focus(window);
                if self.active_run.is_some() {
                    self.notice = Some("已重置对话；冲突文件未改动。".to_string());
                    cx.notify();
                }
            }
            Err(error) => {
                self.edit_conflict = Some(conflict);
                self.last_error = Some(error);
                self.chat_dirty = true;
                cx.notify();
            }
        }
    }

    pub(crate) fn confirm_undo(
        &mut self,
        restore_files: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(dialog) = self.undo_dialog.take() else {
            return;
        };
        if self.message_rewind_blocked(cx) {
            self.undo_dialog = Some(dialog);
            return;
        }
        let mut succeeded = false;
        let restore = restore_files && dialog.plan.can_restore() && dialog.plan.has_file_changes();
        match self.trim_conversation(&dialog.message_id, restore.then_some(&dialog.plan), cx) {
            Ok(restored) => {
                succeeded = true;
                self.last_error = None;
                let shell_note = if dialog.plan.shell_commands.is_empty() {
                    ""
                } else {
                    "；shell 改动无法追踪"
                };
                self.notice = Some(if restored > 0 {
                    format!("已撤销这条消息，并还原 {restored} 个文件{shell_note}。")
                } else if dialog.plan.has_file_changes() {
                    format!("已撤销这条消息；文件改动未还原{shell_note}。")
                } else if !dialog.plan.shell_commands.is_empty() {
                    "已撤销这条消息；shell 命令造成的改动无法追踪。".to_string()
                } else {
                    "已撤销这条消息。".to_string()
                });
                self.chat.pin_to_bottom();
            }
            Err(error) => {
                self.last_error = Some(error);
                // Re-run the preflight so the dialog shows the new on-disk
                // state (for example a file that changed during confirmation)
                // instead of repeating a doomed restore with a stale plan.
                let message_id = dialog.message_id.clone();
                let excerpt = dialog.excerpt.clone();
                let root = crate::checkpoint::checkpoint_root();
                let refreshed = self.active_shared().and_then(|shared| {
                    let session = lock_session(&shared);
                    crate::checkpoint::plan_restore_in(
                        &session,
                        &message_id,
                        &root,
                        &self.project_roots_for_session_state(&session),
                    )
                    .ok()
                });
                self.undo_dialog = Some(UndoDialogState {
                    message_id,
                    excerpt,
                    plan: refreshed.unwrap_or(dialog.plan),
                });
                self.chat_dirty = true;
            }
        }
        if succeeded {
            // The dialog had blurred the composer; restore focus so keyboard
            // shortcuts and the next draft behave normally.
            self.composer_focus_handle(cx).focus(window);
        }
        cx.notify();
    }

    /// Validate that starting a run will not immediately fail. Used before a
    /// destructive trim so a missing API key cannot cost the user their
    /// transcript.
    fn run_ready(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(shared) = self.active_shared() else {
            self.last_error = Some("没有活动会话。".to_string());
            self.chat_dirty = true;
            cx.notify();
            return false;
        };
        let workspace = lock_session(&shared).cwd.clone();
        if !workspace.is_dir() {
            self.last_error = Some(format!("工作区不可访问：{}", workspace.display()));
            self.chat_dirty = true;
            cx.notify();
            return false;
        }
        if self.runtime.is_none() {
            self.last_error = Some("后台运行时不可用。".to_string());
            self.chat_dirty = true;
            cx.notify();
            return false;
        }
        let resolved = self.active_resolved_model();
        if self.models.demo_mode(self.config.use_demo, &resolved) {
            return true;
        }
        if !resolved.live {
            self.last_error = Some(format!(
                "供应商「{}」尚未配置 API Key。请在设置 → 模型中填写，或切换到其他供应商。",
                resolved.provider_label()
            ));
            self.chat_dirty = true;
            cx.notify();
            return false;
        }
        if let Err(error) = resolved.build_provider() {
            self.last_error = Some(format!("无法创建模型客户端：{error}"));
            self.chat_dirty = true;
            cx.notify();
            return false;
        }
        true
    }

    /// Trim the transcript at `message_id`, optionally restoring files first.
    /// File restoration is verified and atomic per file; the transcript is
    /// only trimmed after the restore succeeded.
    fn trim_conversation(
        &mut self,
        message_id: &str,
        restore: Option<&RestorePlan>,
        cx: &mut Context<Self>,
    ) -> Result<usize, String> {
        let Some(shared) = self.active_shared() else {
            return Err("没有活动会话。".to_string());
        };
        let cwd = {
            let session = lock_session(&shared);
            if session.message_position(message_id).is_none() {
                return Err("这条消息已不在当前会话中。".to_string());
            }
            session.cwd.clone()
        };
        let restored = match restore {
            Some(plan) => crate::checkpoint::apply_restore_at(plan, &cwd)?,
            None => 0,
        };

        let root = crate::checkpoint::checkpoint_root();
        {
            let mut session = lock_session(&shared);
            let Some(cut_index) = session.message_position(message_id) else {
                return Err("这条消息已不在当前会话中。".to_string());
            };
            session.truncate_from(cut_index);
            let live: HashSet<String> = session
                .messages
                .iter()
                .map(|message| message.id.clone())
                .collect();
            let stale: Vec<String> = session
                .checkpoints
                .iter()
                .filter(|checkpoint| !live.contains(&checkpoint.message_id))
                .map(|checkpoint| checkpoint.message_id.clone())
                .collect();
            crate::checkpoint::drop_turn_checkpoints(&mut session, &stale, &root);
            // No run or child is alive at this point; clear a stale busy flag
            // left by an interrupted harness so the edited turn can restart.
            session.set_status(RunStatus::Idle);
            session.touch();
        }
        // Undo/edit is a deliberate structural change; save synchronously so
        // the sidebar and a reopened session see the trimmed transcript even
        // when the throttled background writer would still hold the old JSON.
        // Preview sessions are never written to disk.
        if !self.preview_mode {
            let _ = self.save_active_metadata(&shared);
        }
        // A completed background child may still be waiting for its
        // `<task-notification>` to be flushed. That turn no longer exists, so
        // drop the notification instead of waking the model for removed work.
        self.pending_subagent_notifications.clear();
        // Queued user messages referenced the pre-trim conversation; drop
        // them too so they cannot run against a rewound transcript.
        self.pending_inputs.clear();
        self.chat_dirty = true;
        self.graph.invalidate();
        self.chat.pin_to_bottom();
        // Created files may have been deleted and restored files rewritten;
        // keep the file pane honest without touching Git.
        self.refresh_file_tree();
        self.refresh_sessions(cx);
        Ok(restored)
    }

    /// Start one parent turn. `input = None` resumes an existing transcript
    /// (used for background-task notifications that were already appended).
    fn start_run(&mut self, input: Option<RunInput>, cx: &mut Context<Self>) {
        if self.active_run.is_some() {
            self.notice = Some("当前任务仍在运行，请先停止。".to_string());
            cx.notify();
            return;
        }
        let Some(active) = &self.active else {
            self.last_error = Some("没有活动会话。".to_string());
            cx.notify();
            return;
        };
        let Some(runtime_handle) = self.runtime.as_ref().map(|runtime| runtime.handle()) else {
            self.last_error = Some("后台运行时不可用。".to_string());
            cx.notify();
            return;
        };

        let shared = active.shared.clone();
        let (workspace, additional_workspaces) = self.workspace_roots_for_session(&shared);
        let session_project_id = lock_session(&shared).project_id.clone();
        let permission_mode = lock_session(&shared).permission_mode;

        let (resolved, session_id, model) = {
            let mut session = lock_session(&shared);
            let resolved = self.models.resolve_session(&session);
            let selection = resolved.selection();
            session.set_model_selection(
                selection.provider_id,
                selection.model_id,
                selection.reasoning_level,
            );
            session.last_error = None;
            (resolved, session.id.clone(), session.model.clone())
        };
        let demo_mode = self.models.demo_mode(self.config.use_demo, &resolved);
        let provider: Arc<dyn ModelProvider> = if demo_mode {
            Arc::new(DemoProvider::new())
        } else if !resolved.live {
            self.last_error = Some(format!(
                "供应商「{}」尚未配置 API Key。请在设置 → 模型中填写，或切换到其他供应商。",
                resolved.provider_label()
            ));
            cx.notify();
            return;
        } else {
            match resolved.build_provider_for(Some(&session_id)) {
                Ok(provider) => provider,
                Err(error) => {
                    self.last_error = Some(format!("无法创建模型客户端：{error}"));
                    cx.notify();
                    return;
                }
            }
        };
        let mut tools = match builtin_tools_with_roots(&workspace, &additional_workspaces) {
            Ok(tools) => tools,
            Err(error) => {
                self.last_error = Some(format!("无法打开工作区 {}：{error}", workspace.display()));
                cx.notify();
                return;
            }
        };
        // Computer use is a main-agent-only capability: child_tools() builds a
        // fresh registry from builtin_tools(), so subagents never inherit it.
        if self.config.computer_use_mode.is_enabled() {
            tools.register(ComputerTool::new(
                self.computer.clone(),
                resolved.model.capabilities.vision,
            ));
        }
        let computer_session_id = session_id.clone();

        // Project instructions belong to the session's own workspace, not the
        // global task default: a project session must see its own AGENTS.md.
        let project_instructions = crate::config::project_instructions_for(&workspace);
        let system_prompt_extra = if permission_mode == PermissionMode::Plan {
            format!("{}\n\n{}", project_instructions, PLAN_MODE_SYSTEM_PROMPT)
        } else {
            project_instructions.clone()
        };
        let checkpoint_recorder =
            Arc::new(crate::checkpoint::CheckpointRecorder::new(shared.clone()));
        let harness_config = HarnessConfig {
            max_tokens: Some(resolved.max_output_tokens()),
            temperature: self.config.temperature,
            reasoning_effort: resolved.reasoning_level.clone(),
            context_window_override: if demo_mode {
                None
            } else {
                Some(resolved.context_window())
            },
            auto_approve_tools: false,
            compaction: self.config.compaction.clone(),
            system_prompt_extra,
            additional_workspaces: additional_workspaces.clone(),
            project_id: session_project_id.clone(),
            tool_timeout: Duration::from_secs(180),
            // `RunStarted` binds this to the user message that owns the turn;
            // child harnesses inherit the same sink through the cloned config,
            // so subagent edits land in the parent turn.
            checkpoint_sink: Some(checkpoint_recorder.clone()),
            ..HarnessConfig::default()
        };

        let service = SubagentService::new(
            provider.clone(),
            session_id,
            resolved.provider.id.clone(),
            model,
            resolved.reasoning_level.clone(),
            harness_config.clone(),
            project_instructions,
            self.store.clone(),
            self.app_tx.clone(),
            runtime_handle.clone(),
            workspace.clone(),
            std::sync::Arc::new(self.models.file().clone()),
        );

        let (harness_tx, mut harness_rx) = unbounded_channel::<HarnessEvent>();
        let approval = Arc::new(BrokerApprovalGate::new(
            harness_tx.clone(),
            Duration::from_secs(600),
        ));
        // The gate re-reads the session's permission mode on every request, so
        // a Shift+Tab mid-run takes effect on the next tool call. The UI keeps
        // responding to the inner broker gate; locally decided calls never
        // reach it and never show an approval card.
        let permission_mode_provider: Arc<dyn Fn() -> PermissionMode + Send + Sync> = {
            let shared = shared.clone();
            Arc::new(move || lock_session(&shared).permission_mode)
        };
        let mode_gate = Arc::new(ModeApprovalGate::new(
            approval.clone(),
            permission_mode_provider.clone(),
        ));
        // Computer consent is layered *outside* the mode gate: FullAccess must
        // not silently grant screen control, and Plan mode must block mutating
        // computer actions even if the user would approve the card.
        let approval_gate = Arc::new(ComputerApprovalGate::new(
            mode_gate,
            approval.clone(),
            self.computer.clone(),
            computer_session_id,
            permission_mode_provider,
        ));
        let harness = AgentHarness::new(
            shared.clone(),
            provider,
            Arc::new(tools),
            approval_gate,
            harness_config,
            harness_tx,
        )
        .with_subagents(Arc::new(service.clone()));
        let handle = harness.handle();
        let harness = Arc::new(harness);

        let forward_tx = self.app_tx.clone();
        runtime_handle.spawn(async move {
            while let Some(event) = harness_rx.recv().await {
                if forward_tx.send(AppEvent::Main(event)).is_err() {
                    break;
                }
            }
        });
        let error_tx = self.app_tx.clone();
        let error_session = shared.clone();
        runtime_handle.spawn(async move {
            let result = match input {
                Some(input) => {
                    harness
                        .run_turn_with_attachments(input.text, input.attachments)
                        .await
                }
                None => harness.run_pending().await,
            };
            if let Err(error) = result {
                lock_session(&error_session).set_status(RunStatus::Idle);
                let _ = error_tx.send(AppEvent::Main(HarnessEvent::RunFinished {
                    run_id: String::new(),
                    outcome: RunOutcome::Failed(error.to_string()),
                }));
            }
        });

        self.subagent_services.push(service);
        if self.subagent_services.len() > 8 {
            self.subagent_services.remove(0);
        }
        self.active_run = Some(ActiveRun {
            run_id: None,
            handle,
            approval,
            started_at: Instant::now(),
            checkpoint: Some(checkpoint_recorder),
        });
        self.pending_approvals
            .retain(|pending| pending.agent_id.is_some());
        self.last_error = None;
        self.notice = None;
        self.chat.pin_to_bottom();
        self.chat_dirty = true;
        self.ensure_run_ticker(cx);
        cx.notify();
    }

    /// Keep the "工作中 · 1 分 16 秒" footer moving even while a tool is
    /// blocking and no deltas arrive. The elapsed text is second-granular, so
    /// the ticker only wakes the UI when a displayed second rolls over.
    fn ensure_run_ticker(&mut self, cx: &mut Context<Self>) {
        if self.run_ticker_active {
            return;
        }
        self.run_ticker_active = true;
        cx.spawn(async move |weak, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                let keep = weak
                    .update(cx, |this, cx| {
                        let Some(run) = &this.active_run else {
                            this.run_ticker_active = false;
                            return false;
                        };
                        let seconds = run.started_at.elapsed().as_secs();
                        if this.run_tick_stamp != seconds {
                            this.run_tick_stamp = seconds;
                            cx.notify();
                        }
                        true
                    })
                    .unwrap_or(false);
                if !keep {
                    break;
                }
            }
        })
        .detach();
    }

    pub(crate) fn interrupt(&mut self, cx: &mut Context<Self>) {
        let running = self.active_run.is_some()
            || self
                .subagent_tasks
                .iter()
                .any(|task| task.status == SubagentStatus::Running);
        if !running {
            return;
        }
        if let Some(run) = &self.active_run {
            run.handle.interrupt();
            run.approval.deny_all();
        }
        if let Some(session_id) = self
            .active
            .as_ref()
            .map(|active| lock_session(&active.shared).id.clone())
        {
            self.computer.stop(&session_id);
            if let Some(runtime) = &self.runtime {
                let computer = self.computer.clone();
                runtime.spawn(async move {
                    computer.finish_session(&session_id).await;
                });
            }
        }
        for pending in self.pending_approvals.drain(..) {
            if let Some(agent_id) = &pending.agent_id {
                for service in &self.subagent_services {
                    service.respond_approval(agent_id, &pending.request.id, ApprovalDecision::Deny);
                }
            }
        }
        for service in &self.subagent_services {
            service.stop_all();
        }
        for task in &mut self.subagent_tasks {
            if task.status == SubagentStatus::Running {
                task.waiting_approval = false;
            }
        }
        self.notice = Some("正在停止…".to_string());
        self.chat_dirty = true;
        cx.notify();
    }

    fn respond_approval(&mut self, decision: ApprovalDecision, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_approvals.first().cloned() else {
            return;
        };
        match &pending.agent_id {
            Some(agent_id) => {
                for service in &self.subagent_services {
                    if service.respond_approval(agent_id, &pending.request.id, decision) {
                        break;
                    }
                }
                if let Some(task) = self
                    .subagent_tasks
                    .iter_mut()
                    .find(|task| &task.agent_id == agent_id)
                {
                    task.waiting_approval = false;
                }
            }
            None => {
                if let Some(run) = &self.active_run {
                    run.approval.respond(&pending.request.id, decision);
                }
            }
        }
        self.pending_approvals.remove(0);
        self.chat_dirty = true;
        cx.notify();
    }

    fn handle_event(&mut self, event: HarnessEvent, cx: &mut Context<Self>) {
        let mut structural = false;
        match &event {
            HarnessEvent::RunStarted { run_id, .. } => {
                let turn_id = self.active_shared().and_then(|shared| {
                    let session = lock_session(&shared);
                    session.latest_user_turn_id().map(str::to_string)
                });
                if let Some(run) = &mut self.active_run {
                    run.run_id = Some(run_id.clone());
                    if let (Some(recorder), Some(turn_id)) = (&run.checkpoint, turn_id) {
                        recorder.bind_turn(turn_id);
                    }
                }
                // The first user message turns an invisible draft into real
                // history; write it before the sidebar scan so the new row
                // appears at once.
                self.publish_active_draft(cx);
                self.persist_active(cx);
                self.refresh_sessions(cx);
                structural = true;
            }
            HarnessEvent::StepStarted { .. } => {
                structural = true;
            }
            HarnessEvent::AssistantStarted { message_id } => {
                self.chat.begin_message(message_id);
                structural = true;
            }
            HarnessEvent::TextDelta { message_id, delta } => {
                self.chat.push_text(message_id, delta);
            }
            HarnessEvent::ReasoningDelta { message_id, delta } => {
                self.chat.push_reasoning(message_id, delta);
            }
            HarnessEvent::AssistantFinished { message_id, .. } => {
                let message = self.active_shared().and_then(|shared| {
                    let session = lock_session(&shared);
                    session
                        .messages
                        .iter()
                        .find(|message| &message.id == message_id)
                        .cloned()
                });
                if let Some(message) = message {
                    self.chat.finish_message(&message);
                }
                self.persist_active(cx);
                structural = true;
            }
            HarnessEvent::ApprovalRequested { request } => {
                self.pending_approvals.push(PendingApproval {
                    agent_id: None,
                    request: request.clone(),
                });
                structural = true;
            }
            HarnessEvent::ApprovalResolved { request_id, .. } => {
                self.pending_approvals
                    .retain(|pending| &pending.request.id != request_id);
                structural = true;
            }
            HarnessEvent::ToolCallStarted { .. } => {
                structural = true;
            }
            HarnessEvent::ToolCallFinished { result } => {
                self.record_workflow_result(result, cx);
                self.persist_active(cx);
                structural = true;
            }
            HarnessEvent::SteerDrained { steer_id, .. } => {
                // The steer is now a real transcript user message; drop the
                // queue-bar entry that was waiting for injection.
                self.pending_inputs
                    .retain(|input| input.id != steer_id.as_str());
                // Steered messages intentionally have no edit/undo actions:
                // they ride inside the turn checkpoint owned by the prompt
                // that started the run.
                self.persist_active(cx);
                structural = true;
            }
            HarnessEvent::RunFinished { outcome, .. } => {
                // Steers that never reached a step boundary (interrupt, hard
                // failure) come back to the host. Bar entries still marked
                // Steer are exactly those messages, so normalize them instead
                // of re-pushing the reclaimed text.
                if let Some(run) = &self.active_run {
                    run.handle.take_pending_steers();
                }
                for input in &mut self.pending_inputs {
                    input.kind = PendingInputKind::Queue;
                }
                self.active_run = None;
                // A failed or interrupted turn pauses the queue (ZCode's
                // error-paused state): the messages stay visible and the user
                // decides whether to send or drop them.
                self.queue_held =
                    !matches!(outcome, RunOutcome::Completed) && !self.pending_inputs.is_empty();
                self.pending_approvals
                    .retain(|pending| pending.agent_id.is_some());
                // Hide the control bar and restore parked windows/workspace.
                // Consent is retained for the conversation unless the user hit
                // Stop (which revokes it in runtime.stop()).
                if let Some(session_id) = self
                    .active
                    .as_ref()
                    .map(|active| lock_session(&active.shared).id.clone())
                {
                    if let Some(runtime) = &self.runtime {
                        let computer = self.computer.clone();
                        runtime.spawn(async move {
                            computer.finish_session(&session_id).await;
                        });
                    }
                }
                self.chat.end_stream();
                self.persist_active(cx);
                self.refresh_sessions(cx);
                structural = true;
                match outcome {
                    RunOutcome::Failed(error) => {
                        self.last_error = Some(error.clone());
                    }
                    RunOutcome::Interrupted => {
                        self.notice = Some("已停止当前任务。".to_string());
                    }
                    RunOutcome::Completed => {
                        self.notice = None;
                    }
                }
            }
            HarnessEvent::TodosUpdated { .. } | HarnessEvent::ContextCompacted { .. } => {
                self.persist_active(cx);
            }
            HarnessEvent::SessionChanged => {
                structural = true;
            }
            HarnessEvent::UsageUpdated { .. } => {}
        }
        if structural {
            self.chat_dirty = true;
            self.graph.invalidate();
        }
        if let HarnessEvent::RunFinished { outcome, .. } = &event {
            if outcome.is_success() {
                self.resume_after_run(cx);
            }
        }
        // Token deltas are picked up by the typewriter reveal loop, which
        // already keeps a frame scheduled while it has backlog; notifying per
        // delta would only queue redundant full-window renders. Structural
        // events always wake the view.
        if structural || !self.chat.reveal_frame_scheduled {
            cx.notify();
        }
    }

    // --- subagent events --------------------------------------------------

    fn handle_app_event(&mut self, event: AppEvent, cx: &mut Context<Self>) {
        // Stress lab observes (and may fully consume) pump traffic first.
        #[cfg(feature = "stress")]
        if self.stress_observe(&event) {
            return;
        }
        match event {
            AppEvent::Main(event) => self.handle_event(event, cx),
            AppEvent::SubagentSpawned(info) => self.handle_subagent_spawned(info, cx),
            AppEvent::SubagentHarness { agent_id, event } => {
                self.handle_subagent_event(&agent_id, event, cx)
            }
            AppEvent::SubagentFinished(report) => self.handle_subagent_finished(report, cx),
            AppEvent::AgentMessageToMain(message) => self.handle_agent_message_to_main(message, cx),
            AppEvent::AgentMessageDelivered(info) => self.handle_agent_message_delivered(info, cx),
            AppEvent::ComputerOverlay(event) => self.handle_computer_overlay(event, cx),
            AppEvent::UpdateChecked(result) => self.handle_update_checked(result, cx),
            // Stress deltas are consumed by `stress_observe` above; the arm
            // only keeps the match exhaustive.
            #[cfg(feature = "stress")]
            AppEvent::StressDelta { .. } => {}
        }
    }

    // --- update check -------------------------------------------------------

    /// Kick off an update check on the shared tokio runtime. The result rides
    /// the same `AppEvent` pump as harness events, so no extra locking; with
    /// no runtime (creation failed) this silently does nothing.
    pub(crate) fn start_update_check(&mut self, cx: &mut Context<Self>) {
        if self.update.checking {
            return;
        }
        let Some(runtime) = self.runtime.as_ref() else {
            return;
        };
        let client = match crate::update::build_client() {
            Ok(client) => client,
            Err(error) => {
                self.update.error = Some(error);
                cx.notify();
                return;
            }
        };
        self.update.checking = true;
        self.update.error = None;
        self.update.dismissed = false;
        let tx = self.app_tx.clone();
        runtime.spawn(async move {
            let result = crate::update::fetch_latest(&client).await;
            let _ = tx.send(AppEvent::UpdateChecked(result));
        });
        cx.notify();
    }

    fn handle_update_checked(
        &mut self,
        result: Result<Option<crate::update::UpdateInfo>, String>,
        cx: &mut Context<Self>,
    ) {
        self.update.checking = false;
        match result {
            Ok(latest) => {
                self.update.latest = latest;
                self.update.error = None;
            }
            Err(error) => self.update.error = Some(error),
        }
        cx.notify();
    }

    /// The release the banner should offer, if any: strictly newer than the
    /// running build and not the version the user chose to skip.
    pub(crate) fn available_update(&self) -> Option<&crate::update::UpdateInfo> {
        let info = self.update.latest.as_ref()?;
        if crate::update::is_newer(crate::update::current_version(), &info.version)
            && self.config.update_skip_version.as_deref() != Some(info.version.as_str())
        {
            Some(info)
        } else {
            None
        }
    }

    pub(crate) fn skip_update(&mut self, cx: &mut Context<Self>) {
        if let Some(info) = self.available_update() {
            self.config.update_skip_version = Some(info.version.clone());
            let _ = self.config.save();
        }
        cx.notify();
    }

    // --- computer use overlay ---------------------------------------------

    fn handle_computer_overlay(&mut self, event: OverlayEvent, cx: &mut Context<Self>) {
        let active_session = self
            .active
            .as_ref()
            .map(|active| lock_session(&active.shared).id.clone());
        match &event.kind {
            OverlayEventKind::Hide | OverlayEventKind::CaptureHide => {
                self.close_computer_overlay(cx);
            }
            OverlayEventKind::Show { .. } | OverlayEventKind::Update { .. } => {
                if active_session.as_deref() != Some(event.session_id.as_str()) {
                    return;
                }
                let Some(handle) = self.ensure_computer_overlay(cx) else {
                    return;
                };
                if handle
                    .update(cx, |view, _, cx| view.apply(&event, cx))
                    .is_err()
                {
                    // The user may have closed the bar with the window
                    // manager; drop the stale handle and reopen once.
                    self.computer_overlay = None;
                    if let Some(handle) = self.ensure_computer_overlay(cx) {
                        let _ = handle.update(cx, |view, _, cx| view.apply(&event, cx));
                    }
                }
            }
        }
    }

    fn ensure_computer_overlay(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<WindowHandle<ComputerOverlayView>> {
        if let Some(handle) = self.computer_overlay {
            return Some(handle);
        }
        // GPUI's Wayland backend reports no primary display and may not have
        // populated outputs yet; Wayland ignores the requested origin anyway.
        // X11/Windows still get a true top-center position.
        let width = px(500.0);
        let height = px(64.0);
        let origin = match cx
            .primary_display()
            .or_else(|| cx.displays().into_iter().next())
        {
            Some(display) => {
                let display_bounds = display.bounds();
                let centered_offset = (display_bounds.size.width - width) * 0.5;
                point(
                    display_bounds.origin.x + centered_offset,
                    display_bounds.origin.y + px(24.0),
                )
            }
            None => point(px(0.0), px(24.0)),
        };
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                origin,
                size(width, height),
            ))),
            titlebar: None,
            focus: false,
            is_movable: false,
            is_resizable: false,
            is_minimizable: false,
            kind: WindowKind::PopUp,
            app_id: Some("effibuddy-computer".to_string()),
            ..WindowOptions::default()
        };

        let runtime = self.computer.clone();
        let app = self.self_weak.clone();
        let theme = self.theme.clone();
        let handle = match cx.open_window(options, move |window, cx| {
            window.set_window_title("EffiBuddy Computer Use");
            window.set_app_id("effibuddy-computer");
            cx.new(|_| ComputerOverlayView::new(runtime, app, theme))
        }) {
            Ok(handle) => handle,
            Err(error) => {
                self.notice = Some(format!("无法显示电脑操作悬浮条：{error}"));
                cx.notify();
                return None;
            }
        };
        self.computer_overlay = Some(handle);

        // Topmost/sticky is platform-specific; do it after the native window
        // exists, and don't block the UI thread.
        if let Some(runtime) = &self.runtime {
            let computer = self.computer.clone();
            runtime.spawn(async move {
                // The native window may not be mapped yet; retry once after
                // the compositor/WM has had time to see it.
                tokio::time::sleep(Duration::from_millis(150)).await;
                computer.pin_overlay("EffiBuddy Computer Use").await;
                tokio::time::sleep(Duration::from_millis(500)).await;
                computer.pin_overlay("EffiBuddy Computer Use").await;
            });
        }
        Some(handle)
    }

    fn close_computer_overlay(&mut self, cx: &mut Context<Self>) {
        if let Some(handle) = self.computer_overlay.take() {
            let _ = handle.update(cx, |_, window, _| window.remove_window());
        }
    }

    /// Leaving a conversation ends its computer session: parked windows and
    /// the isolated workspace are restored, and the one-time consent is
    /// revoked so another conversation cannot inherit it.
    fn retire_computer_session(&mut self, cx: &mut Context<Self>) {
        self.close_computer_overlay(cx);
        let Some(session_id) = self
            .active
            .as_ref()
            .map(|active| lock_session(&active.shared).id.clone())
        else {
            return;
        };
        let computer = self.computer.clone();
        if let Some(runtime) = &self.runtime {
            runtime.spawn(async move {
                computer.restore_isolation(&session_id).await;
                computer.forget_consent(&session_id);
            });
        } else {
            computer.forget_consent(&session_id);
        }
    }

    /// A child (or grandchild) addressed the main agent. The message becomes a
    /// synthetic user turn so the main model sees it, but the UI renders it as
    /// an inter-agent card. If the main agent is idle it is woken.
    fn handle_agent_message_to_main(
        &mut self,
        message: effibuddy_agent_core::AgentMessage,
        cx: &mut Context<Self>,
    ) {
        let Some(shared) = self.active_shared() else {
            return;
        };
        {
            let mut session = lock_session(&shared);
            session.push_message(Message::from_agent_message(&message));
            graph_persist::record_message(&mut session.graph, &message, false, true);
            session.touch();
        }
        self.persist_active(cx);
        self.graph.invalidate();
        self.chat_dirty = true;
        if self.active_run.is_none() {
            self.notice = Some(format!("收到来自 {} 的消息", message.from_name));
            self.start_run(None, cx);
        }
        cx.notify();
    }

    fn handle_agent_message_delivered(
        &mut self,
        info: crate::events::AgentMessageDeliveredInfo,
        cx: &mut Context<Self>,
    ) {
        if let Some(shared) = self.active_shared() {
            let mut session = lock_session(&shared);
            graph_persist::record_message(
                &mut session.graph,
                &info.message,
                info.queued,
                info.woke,
            );
            session.touch();
        }
        self.persist_active(cx);
        self.graph.invalidate();
        self.chat_dirty = true;
        cx.notify();
    }

    fn record_workflow_result(&mut self, result: &ToolResult, cx: &mut Context<Self>) {
        let Some(metadata) = &result.metadata else {
            return;
        };
        if metadata.get("workflow").is_none() {
            return;
        }
        let Some(shared) = self.active_shared() else {
            return;
        };
        {
            let mut session = lock_session(&shared);
            graph_persist::record_workflow_metadata(&mut session.graph, metadata);
            session.touch();
        }
        self.persist_active(cx);
        self.graph.invalidate();
        self.chat_dirty = true;
    }

    fn handle_subagent_spawned(&mut self, info: SubagentSpawnedInfo, cx: &mut Context<Self>) {
        if self.subagent_task_index.contains_key(&info.agent_id) {
            return;
        }
        if let Some(shared) = self.active_shared() {
            let mut session = lock_session(&shared);
            graph_persist::record_spawn(&mut session.graph, &info);
            session.touch();
        }
        self.push_subagent_task(AgentTask {
            agent_id: info.agent_id,
            name: info.name,
            profile: info.profile,
            color: info.color,
            description: info.description,
            status: SubagentStatus::Running,
            background: info.background,
            started_at: info.started_at,
            finished_at: None,
            child_session_id: info.child_session_id,
            parent_call_id: info.parent_call_id,
            parent_session_id: info.parent_session_id,
            parent_agent_id: info.parent_agent_id,
            parent_agent_name: info.parent_agent_name,
            group_id: info.group_id,
            group_name: info.group_name,
            depth: info.depth,
            visibility: info.visibility,
            read_only: info.read_only,
            can_spawn: info.can_spawn,
            task_prompt: info.task_prompt,
            output_schema: info.output_schema,
            provider_id: info.provider_id,
            model: info.model,
            shared: info.shared,
            report: None,
            error: None,
            data: None,
            tool_uses: 0,
            total_tokens: 0,
            duration_ms: 0,
            waiting_approval: false,
            restored: false,
        });
        self.persist_active(cx);
        self.graph.invalidate();
        self.chat_dirty = true;
        self.ensure_agent_ticker(cx);
        cx.notify();
    }

    fn handle_subagent_event(
        &mut self,
        agent_id: &str,
        event: HarnessEvent,
        cx: &mut Context<Self>,
    ) {
        let viewing = self.side.agent_views.contains_key(agent_id);
        let mut structural = false;
        // Only topology/status changes redraw the graph. Per-step churn
        // (steps, assistant messages, tool calls, todos) invalidates nothing:
        // under a 1000-child stress run that would rebuild the projection and
        // re-layout every frame.
        let mut graph_relevant = false;
        match &event {
            HarnessEvent::RunStarted { .. } => {
                if let Some(task) = self.subagent_task_mut(agent_id) {
                    task.status = SubagentStatus::Running;
                }
                structural = true;
                graph_relevant = true;
            }
            HarnessEvent::StepStarted { .. } => structural = true,
            HarnessEvent::AssistantStarted { message_id } => {
                if let Some(view) = self.side.agent_views.get_mut(agent_id) {
                    view.chat.begin_message(message_id);
                }
                structural = true;
            }
            HarnessEvent::TextDelta { message_id, delta } => {
                if let Some(view) = self.side.agent_views.get_mut(agent_id) {
                    view.chat.push_text(message_id, delta);
                }
            }
            HarnessEvent::ReasoningDelta { message_id, delta } => {
                if let Some(view) = self.side.agent_views.get_mut(agent_id) {
                    view.chat.push_reasoning(message_id, delta);
                }
            }
            HarnessEvent::AssistantFinished { message_id, .. } => {
                if viewing {
                    let message = self
                        .subagent_tasks
                        .iter()
                        .find(|task| task.agent_id == agent_id)
                        .and_then(|task| {
                            let session = lock_session(&task.shared);
                            session
                                .messages
                                .iter()
                                .find(|message| &message.id == message_id)
                                .cloned()
                        });
                    if let Some(message) = message {
                        if let Some(view) = self.side.agent_views.get_mut(agent_id) {
                            view.chat.finish_message(&message);
                        }
                    }
                }
                structural = true;
            }
            HarnessEvent::ApprovalRequested { request } => {
                if !self
                    .pending_approvals
                    .iter()
                    .any(|pending| pending.request.id == request.id)
                {
                    self.pending_approvals.push(PendingApproval {
                        agent_id: Some(agent_id.to_string()),
                        request: request.clone(),
                    });
                }
                if let Some(task) = self.subagent_task_mut(agent_id) {
                    task.waiting_approval = true;
                }
                structural = true;
                graph_relevant = true;
            }
            HarnessEvent::ApprovalResolved { request_id, .. } => {
                self.pending_approvals
                    .retain(|pending| &pending.request.id != request_id);
                if let Some(task) = self.subagent_task_mut(agent_id) {
                    task.waiting_approval = false;
                }
                structural = true;
                graph_relevant = true;
            }
            HarnessEvent::ToolCallStarted { .. } => {
                structural = true;
            }
            HarnessEvent::ToolCallFinished { result } => {
                self.record_workflow_result(result, cx);
                structural = true;
            }
            HarnessEvent::RunFinished { .. } => {
                if let Some(view) = self.side.agent_views.get_mut(agent_id) {
                    view.chat.end_stream();
                }
                structural = true;
            }
            HarnessEvent::TodosUpdated { .. }
            | HarnessEvent::ContextCompacted { .. }
            | HarnessEvent::SessionChanged
            | HarnessEvent::SteerDrained { .. }
            | HarnessEvent::UsageUpdated { .. } => {
                structural = true;
            }
        }
        if structural {
            self.chat_dirty = true;
            if graph_relevant {
                self.graph.invalidate();
            }
            if let Some(view) = self.side.agent_views.get_mut(agent_id) {
                view.dirty = true;
            }
        }
        // Deltas for a subagent whose pane is closed buffer nothing and
        // change nothing visible; skip the full-window notify. When the pane
        // is open the delta notify kicks the reveal loop, which then drives
        // its own frames.
        if structural || viewing {
            cx.notify();
        }
    }

    fn handle_subagent_finished(&mut self, report: SubagentReport, cx: &mut Context<Self>) {
        let agent_id = report.agent_id.clone();
        let active_id = self.active_id();
        let mut belongs_to_active = false;
        let mut known = false;
        if let Some(task) = self.subagent_task_mut(&agent_id) {
            known = true;
            belongs_to_active =
                active_id.as_deref() == Some(task.parent_session_id.as_str()) || task.depth > 0;
            task.name = report.name.clone();
            task.status = report.status;
            task.report = Some(report.report.clone());
            task.error = report.error.clone();
            task.data = report.data.clone();
            task.tool_uses = report.tool_uses;
            task.total_tokens = report.usage.total_tokens;
            task.duration_ms = report.duration_ms;
            task.finished_at = Some(Instant::now());
            task.waiting_approval = false;
        }
        if let Some(shared) = self.active_shared() {
            let mut session = lock_session(&shared);
            graph_persist::record_finish(&mut session.graph, &report);
            session.touch();
        }
        self.graph.invalidate();
        self.persist_active(cx);
        self.pending_approvals
            .retain(|pending| pending.agent_id.as_deref() != Some(agent_id.as_str()));
        if let Some(view) = self.side.agent_views.get_mut(&agent_id) {
            view.chat.end_stream();
            view.dirty = true;
        }
        self.chat_dirty = true;

        if known {
            self.notice = match report.status {
                SubagentStatus::Completed => {
                    Some(format!("子代理「{}」已完成", report.description))
                }
                SubagentStatus::Failed => Some(format!("子代理「{}」失败", report.description)),
                SubagentStatus::Cancelled => {
                    Some(format!("子代理「{}」已停止", report.description))
                }
                // Lost is derived from disk on restore, never from a live run.
                SubagentStatus::Running | SubagentStatus::Lost => None,
            };
        }

        if belongs_to_active
            && report.background
            && !report.wake
            && matches!(
                report.status,
                SubagentStatus::Completed | SubagentStatus::Failed
            )
        {
            self.pending_subagent_notifications
                .push(format_task_notification(&report));
            self.maybe_flush_subagent_notifications(cx);
        }
        self.ensure_agent_ticker(cx);
        cx.notify();
    }

    fn maybe_flush_subagent_notifications(&mut self, cx: &mut Context<Self>) {
        if self.active_run.is_some() || self.pending_subagent_notifications.is_empty() {
            return;
        }
        let Some(active) = &self.active else {
            return;
        };
        let notifications = std::mem::take(&mut self.pending_subagent_notifications);
        let text = notifications.join("\n\n");
        {
            let mut session = lock_session(&active.shared);
            session.push_message(Message::synthetic_user(text));
            session.touch();
        }
        self.persist_active(cx);
        self.chat_dirty = true;
        self.notice = Some("正在汇总后台子代理的结果…".to_string());
        self.start_run(None, cx);
    }

    /// Continuation after a successful turn: auto-drain the queue head (one
    /// message per turn, like DeepSeek Harness claiming a single `next-turn`
    /// item) and fold pending background notifications into the same new turn,
    /// ahead of the queued user message. A held queue (interrupt/failure) is
    /// never resumed from here — not even by an unrelated notification run —
    /// only by the user acting on the queue bar.
    fn resume_after_run(&mut self, cx: &mut Context<Self>) {
        if self.active_run.is_some() {
            return;
        }
        let notifications = std::mem::take(&mut self.pending_subagent_notifications);
        let head = if self.queue_held {
            None
        } else {
            self.pending_inputs
                .iter()
                .position(|input| input.kind == PendingInputKind::Queue)
        };
        if notifications.is_empty() && head.is_none() {
            return;
        }
        let head = head
            .map(|index| self.pending_inputs.remove(index))
            .map(|input| RunInput {
                text: input.text,
                attachments: input.attachments,
            });
        if head.is_some() {
            self.queue_held = false;
        }
        if !notifications.is_empty() {
            let Some(active) = &self.active else {
                return;
            };
            {
                let mut session = lock_session(&active.shared);
                session.push_message(Message::synthetic_user(notifications.join("\n\n")));
                session.touch();
            }
            self.persist_active(cx);
            self.chat_dirty = true;
        }
        self.start_run(head, cx);
    }

    fn ensure_agent_ticker(&mut self, cx: &mut Context<Self>) {
        if self.agent_ticker_active {
            return;
        }
        self.agent_ticker_active = true;
        cx.spawn(async move |weak, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                let keep = weak
                    .update(cx, |this, cx| {
                        let running = this
                            .subagent_tasks
                            .iter()
                            .any(|task| task.status == SubagentStatus::Running);
                        if !running {
                            this.agent_ticker_active = false;
                            return false;
                        }
                        // Sum of the running tasks' displayed seconds: it only
                        // changes when some card's elapsed text rolls over.
                        let stamp: u64 = this
                            .subagent_tasks
                            .iter()
                            .filter(|task| task.status == SubagentStatus::Running)
                            .map(|task| task.started_at.elapsed().as_secs())
                            .sum();
                        if this.agent_tick_stamp != stamp {
                            this.agent_tick_stamp = stamp;
                            cx.notify();
                        }
                        true
                    })
                    .unwrap_or(false);
                if !keep {
                    break;
                }
            }
        })
        .detach();
    }

    pub(crate) fn open_agent_tab(&mut self, agent_id: &str, cx: &mut Context<Self>) {
        let Some(task) = self
            .subagent_tasks
            .iter()
            .find(|task| task.agent_id == agent_id)
        else {
            return;
        };
        let tab = SidePaneTab::agent(agent_id, &task.profile, &task.description);
        if !self.side.agent_views.contains_key(agent_id) {
            let mut chat = ChatList::new();
            let run = RunSnapshot {
                running: task.status == SubagentStatus::Running,
                started_at: Some(task.started_at),
                waiting_approval: task.waiting_approval,
                approval_tool: None,
                last_error: task.error.clone(),
                notice: None,
            };
            {
                let session = lock_session(&task.shared);
                chat.rebuild(&session, &run);
                if task.status == SubagentStatus::Running {
                    if let Some(message) = session.messages.iter().rev().find(|message| {
                        message.role == effibuddy_agent_core::Role::Assistant
                            && !message.text().trim().is_empty()
                    }) {
                        chat.prime_stream(message);
                    }
                }
            }
            chat.install_agent_scroll_handler(agent_id.to_string(), cx.entity().downgrade());
            chat.pin_to_bottom();
            self.side
                .agent_views
                .insert(agent_id.to_string(), AgentView { chat, dirty: false });
        }
        self.side.open_tab(tab);
        self.side.open = true;
        self.persist_side_pane();
        cx.notify();
    }

    pub(crate) fn close_side_tab(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(tab) = self.side.tab(id).cloned() {
            if let SidePaneTabKind::Agent(agent_id) = tab.kind {
                self.side.agent_views.remove(&agent_id);
            }
        }
        self.side.close_tab(id);
        self.persist_side_pane();
        cx.notify();
    }

    pub(crate) fn open_sidebar_kind(&mut self, tab: SidePaneTab, cx: &mut Context<Self>) {
        self.side.open_tab(tab);
        self.on_side_tab_activated(cx);
        self.persist_side_pane();
        cx.notify();
    }

    pub(crate) fn refresh_file_tree(&mut self) {
        if let Some(shared) = self.active_shared() {
            let cwd = lock_session(&shared).cwd.clone();
            self.side.file_tree.set_root(cwd);
        }
        self.side.file_tree.refresh();
    }

    pub(crate) fn refresh_git(&mut self, cx: &mut Context<Self>) {
        if self.side.git.loading {
            return;
        }
        let Some(shared) = self.active_shared() else {
            return;
        };
        let workspace = lock_session(&shared).cwd.clone();
        let Some(runtime) = &self.runtime else {
            self.side.git.error = Some("后台运行时不可用。".to_string());
            cx.notify();
            return;
        };
        self.side.git.loading = true;
        self.side.git.error = None;
        self.side.git.selected = None;
        self.side.git.diff = None;
        let (sender, receiver) = tokio::sync::oneshot::channel();
        runtime.spawn_blocking(move || {
            let result = git_status(&workspace);
            let _ = sender.send(result);
        });
        cx.spawn(async move |weak, cx| {
            if let Ok(result) = receiver.await {
                let _ = weak.update(cx, |this, cx| {
                    this.side.git.loading = false;
                    match result {
                        Ok((branch, files)) => {
                            this.side.git.branch = branch;
                            this.side.git.files = files;
                            this.side.git.error = None;
                        }
                        Err(error) => {
                            this.side.git.branch = None;
                            this.side.git.files.clear();
                            this.side.git.error = Some(error);
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
        cx.notify();
    }

    pub(crate) fn open_git_diff(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(file) = self.side.git.files.get(index).cloned() else {
            return;
        };
        let Some(shared) = self.active_shared() else {
            return;
        };
        let workspace = lock_session(&shared).cwd.clone();
        let Some(runtime) = &self.runtime else {
            self.side.git.error = Some("后台运行时不可用。".to_string());
            cx.notify();
            return;
        };
        self.side.git.selected = Some(index);
        self.side.git.diff = Some("正在加载 diff…".to_string());
        self.side.git.diff_loading = true;
        let (sender, receiver) = tokio::sync::oneshot::channel();
        runtime.spawn_blocking(move || {
            let result = git_diff(&workspace, &file);
            let _ = sender.send(result);
        });
        cx.spawn(async move |weak, cx| {
            if let Ok(result) = receiver.await {
                let _ = weak.update(cx, |this, cx| {
                    this.side.git.diff_loading = false;
                    this.side.git.diff = Some(result);
                    cx.notify();
                });
            }
        })
        .detach();
        cx.notify();
    }

    pub(crate) fn close_agent_view(&mut self, cx: &mut Context<Self>) {
        if let Some(active) = self.side.active().cloned() {
            if let SidePaneTabKind::Agent(agent_id) = active.kind {
                let id = format!("agent:{agent_id}");
                self.close_side_tab(&id, cx);
                return;
            }
        }
        cx.notify();
    }

    pub(crate) fn stop_agent(&mut self, agent_id: &str, cx: &mut Context<Self>) {
        for service in &self.subagent_services {
            service.stop(agent_id);
        }
        if let Some(task) = self
            .subagent_tasks
            .iter_mut()
            .find(|task| task.agent_id == agent_id)
        {
            task.waiting_approval = false;
        }
        self.notice = Some("正在停止子代理…".to_string());
        self.chat_dirty = true;
        cx.notify();
    }

    fn sync_side_pane_workspace(&mut self, cx: &mut Context<Self>) {
        if let Some(shared) = self.active_shared() {
            let cwd = lock_session(&shared).cwd.clone();
            self.side.ensure_file_root(cwd);
            self.side.file_tree.refresh();
            self.side.git = Default::default();
            if self
                .side
                .active()
                .is_some_and(|tab| matches!(tab.kind, SidePaneTabKind::Changes))
            {
                self.refresh_git(cx);
            }
        }
    }

    /// Rebuild read-only agent tasks for the active session's children from
    /// disk. Children without a terminal record are marked `Lost`: the process
    /// exited before they finished, mirroring ZCode's rule that persisted
    /// metadata is the only cross-restart terminal state.
    fn restore_subagent_history(&mut self) {
        let Some(session_id) = self.active_id() else {
            return;
        };
        for child in self.store.child_sessions(&session_id) {
            let record = child.child_run.clone();
            let status = record
                .as_ref()
                .map(|record| record.status)
                .unwrap_or(SubagentStatus::Lost);
            let color = record
                .as_ref()
                .map(|record| record.color)
                .unwrap_or(SubagentColor::Blue);
            let profile = record
                .as_ref()
                .map(|record| record.profile.clone())
                .unwrap_or_else(|| "general-purpose".to_string());
            let tool_uses = child
                .messages
                .iter()
                .flat_map(|message| &message.blocks)
                .filter(|block| matches!(block, ContentBlock::ToolCall { .. }))
                .count();
            let duration_ms = record
                .as_ref()
                .map(|record| {
                    (record.ended_at - child.created_at)
                        .num_milliseconds()
                        .max(0) as u64
                })
                .unwrap_or(0);
            let total_tokens = record
                .as_ref()
                .map(|record| record.usage.total_tokens)
                .unwrap_or(child.usage.total_tokens);
            let started_at = Instant::now()
                - Duration::from_millis(
                    (Utc::now() - child.created_at).num_milliseconds().max(0) as u64
                );
            let finished_at = record.as_ref().map(|record| {
                Instant::now()
                    - Duration::from_millis(
                        (Utc::now() - record.ended_at).num_milliseconds().max(0) as u64,
                    )
            });
            let agent_id = record
                .as_ref()
                .map(|record| record.agent_id.clone())
                .unwrap_or_else(|| format!("restored_{}", child.id));
            let name = child
                .agent_name
                .clone()
                .unwrap_or_else(|| "agent".to_string());
            let description = child.agent_description.clone().unwrap_or_default();
            let child_session_id = child.id.clone();
            let model = child.model.clone();
            let provider_id = child.provider_id.clone();
            let shared = new_shared_session(child);
            self.push_subagent_task(AgentTask {
                agent_id,
                name,
                profile,
                color,
                description,
                status,
                background: false,
                started_at,
                finished_at,
                child_session_id,
                parent_call_id: String::new(),
                parent_session_id: session_id.clone(),
                parent_agent_id: Some("main".to_string()),
                parent_agent_name: Some("main".to_string()),
                group_id: None,
                group_name: None,
                depth: record.as_ref().map(|record| record.depth).unwrap_or(1),
                visibility: AgentVisibility::Public,
                read_only: false,
                can_spawn: false,
                task_prompt: String::new(),
                output_schema: None,
                provider_id,
                model,
                shared,
                report: None,
                error: None,
                data: None,
                tool_uses,
                total_tokens,
                duration_ms,
                waiting_approval: false,
                restored: true,
            });
        }
    }

    fn reset_subagents(&mut self) {
        for service in &self.subagent_services {
            service.stop_all();
        }
        self.subagent_services.clear();
        self.subagent_tasks.clear();
        self.subagent_task_index.clear();
        self.side.agent_views.clear();
        let ids: Vec<String> = self
            .side
            .tabs
            .iter()
            .filter(|tab| matches!(tab.kind, SidePaneTabKind::Agent(_)))
            .map(|tab| tab.id.clone())
            .collect();
        for id in ids {
            self.side.close_tab(&id);
        }
        self.pending_subagent_notifications.clear();
        self.pending_inputs.clear();
        self.pending_approvals
            .retain(|pending| pending.agent_id.is_none());
    }

    /// Append a task and register its index. All task inserts go through
    /// here so [`Self::subagent_task_mut`] stays O(1).
    fn push_subagent_task(&mut self, task: AgentTask) {
        self.subagent_task_index
            .insert(task.agent_id.clone(), self.subagent_tasks.len());
        self.subagent_tasks.push(task);
    }

    fn subagent_task_mut(&mut self, agent_id: &str) -> Option<&mut AgentTask> {
        let index = *self.subagent_task_index.get(agent_id)?;
        self.subagent_tasks.get_mut(index)
    }

    fn agent_cards(&self) -> HashMap<String, AgentCard> {
        self.subagent_tasks
            .iter()
            // Group members are represented by the workflow tool's group card
            // and the graph, not as independent cards under the same call id.
            .filter(|task| task.group_id.is_none())
            .map(|task| {
                (
                    task.parent_call_id.clone(),
                    AgentCard {
                        agent_id: task.agent_id.clone(),
                        profile: task.profile.clone(),
                        color: task.color,
                        description: task.description.clone(),
                        status: task.status,
                        background: task.background,
                        started_at: task.started_at,
                        finished_at: task.finished_at,
                        tool_uses: task.tool_uses,
                        waiting_approval: task.waiting_approval,
                    },
                )
            })
            .collect()
    }

    pub(crate) fn toggle_reasoning(&mut self, id: &str) {
        if !self.expanded_reasoning.remove(id) {
            self.expanded_reasoning.insert(id.to_string());
        }
    }

    pub(crate) fn toggle_tool(&mut self, id: &str) {
        if !self.expanded_tools.remove(id) {
            self.expanded_tools.insert(id.to_string());
        }
    }

    /// Record the explicit fold choice for a turn block so it wins over the
    /// auto rule (live turns expand, finished ones collapse).
    pub(crate) fn toggle_turn(&mut self, id: &str, now_expanded: bool) {
        if now_expanded {
            self.collapsed_turns.insert(id.to_string());
            self.expanded_turns.remove(id);
        } else {
            self.expanded_turns.insert(id.to_string());
            self.collapsed_turns.remove(id);
            // 手动展开也打点：展开内容按 50ms stagger 渐显。
            self.chat.note_turn_expanded(id);
        }
    }

    /// `--bench`: keep the stress storm saturated for `seconds`, then print
    /// one `[bench]` summary line and quit — the CLI-invokable perf lab.
    fn start_bench(&mut self, seconds: u64, cx: &mut Context<Self>) {
        self.bench_until = Some(Instant::now() + Duration::from_secs(seconds));
        let Some(stream) = self.bench_stream.clone() else {
            eprintln!("[bench] no stress stream seeded");
            cx.quit();
            return;
        };
        cx.spawn(async move |weak, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(
                        std::env::var("EFFIBUDDY_BENCH_TICK_MS")
                            .ok()
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(50),
                    ))
                    .await;
                let done = weak
                    .update(cx, |this, cx| {
                        let Some(until) = this.bench_until else {
                            return true;
                        };
                        if Instant::now() >= until {
                            eprintln!("{}", perf::bench_summary());
                            cx.quit();
                            return true;
                        }
                        // Refill before the backlog drains (the drain budget
                        // per 50ms tick is ~3KB even at 120fps), and wake the
                        // render loop — the reveal only self-sustains while a
                        // frame is already scheduled.
                        if this.chat.stream_backlog_bytes() < 4096 {
                            this.chat.refill_stream_backlog(&stream, 100);
                        }
                        cx.notify();
                        false
                    })
                    .unwrap_or(true);
                if done {
                    break;
                }
            }
        })
        .detach();
    }

    /// True while a floating layer (command palette, model menu) covers the
    /// main content and owns wheel input; smooth-scroll overlays underneath
    /// must not claim the event.
    pub(crate) fn modal_blocks_scroll(&self) -> bool {
        self.palette_open
            || self.model_menu.open
            || self.project_menu.target.is_some()
            || self.session_menu.target.is_some()
            || self.session_rename.is_some()
            || self.project_editor.is_some()
            || self.undo_dialog.is_some()
            || self.edit_conflict.is_some()
            || self.reward_open
    }

    pub(crate) fn scroll_chat_to_bottom(&mut self) {
        // Animated glide; the glide snaps and pins itself when it arrives.
        if self.chat.list_state.max_offset_for_scrollbar().height > px(0.0) {
            self.chat.glide_to_bottom();
        } else {
            self.chat.pin_to_bottom();
        }
    }

    fn new_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.new_session_in_project(None, None, window, cx);
    }

    /// Resolve a session's project row: registry project by id, else legacy
    /// path as a root, else a synthesized legacy group, else a plain task.
    pub(crate) fn project_group(
        &self,
        project_id: Option<&str>,
        legacy: Option<&Path>,
    ) -> Option<ProjectGroup> {
        if let Some(project) = self.projects.project_for_session(project_id, legacy) {
            return Some(ProjectGroup {
                id: Some(project.id.clone()),
                key: project_group_key(&project.id),
                name: project.name.clone(),
                roots: project.root_paths(),
            });
        }
        legacy.map(|path| ProjectGroup {
            id: None,
            key: format!("project:legacy:{}", path.display()),
            name: project_default_name(path),
            roots: vec![path.to_path_buf()],
        })
    }

    fn project_group_for_summary(&self, summary: &SessionSummary) -> Option<ProjectGroup> {
        self.project_group(summary.project_id.as_deref(), summary.project.as_deref())
    }

    /// Session cwd plus the project's other folders (in project order).
    fn workspace_roots_for_session(&self, shared: &SharedSession) -> (PathBuf, Vec<PathBuf>) {
        let (cwd, project_id, legacy) = {
            let session = lock_session(shared);
            (
                session.cwd.clone(),
                session.project_id.clone(),
                session.project.clone(),
            )
        };
        let additional = self
            .projects
            .project_for_session(project_id.as_deref(), legacy.as_deref())
            .map(|project| project.additional_roots(&cwd))
            .unwrap_or_default();
        (cwd, additional)
    }

    /// All folders of the project owning this snapshot (empty for tasks).
    fn project_roots_for_session_state(
        &self,
        session: &effibuddy_agent_core::SessionState,
    ) -> Vec<PathBuf> {
        self.projects
            .project_for_session(session.project_id.as_deref(), session.project.as_deref())
            .map(|project| project.root_paths())
            .unwrap_or_default()
    }

    /// The active session's workspace: its project folder for project
    /// sessions, the configured workspace for tasks.
    pub(crate) fn active_workspace(&self) -> PathBuf {
        self.active
            .as_ref()
            .map(|active| lock_session(&active.shared).cwd.clone())
            .unwrap_or_else(|| self.config.workspace())
    }

    /// Label and tooltip for the sidebar footer: the active project's name and
    /// primary folder, else the configured workspace.
    fn sidebar_workspace_label(&self) -> (String, String) {
        if let Some(active) = &self.active {
            let (project_id, legacy) = {
                let session = lock_session(&active.shared);
                (session.project_id.clone(), session.project.clone())
            };
            if let Some(project) = self
                .projects
                .project_for_session(project_id.as_deref(), legacy.as_deref())
            {
                let path = project
                    .primary()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default();
                return (project.name.clone(), path);
            }
        }
        let workspace = self.config.workspace();
        let label = workspace
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| workspace.display().to_string());
        (label, workspace.display().to_string())
    }

    /// Create a conversation. `project_id = None` without an override goes to
    /// the sidebar's 任务 section and runs in the configured workspace.
    /// `project_id = Some(id)` opens it in that project's primary folder;
    /// `workspace_override` serves synthesized legacy groups.
    ///
    /// Each project/task scope owns at most one *unsent* conversation. Clicking
    /// “新建任务” again returns to that draft (text, model and workspace
    /// intact) instead of spawning another invisible transcript.
    fn new_session_in_project(
        &mut self,
        project_id: Option<String>,
        workspace_override: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.active_run.is_some() {
            self.notice = Some("当前任务仍在运行，请先停止。".to_string());
            cx.notify();
            return;
        }
        let (workspace, legacy_project, project_id) = match project_id {
            Some(id) => {
                let primary = self
                    .projects
                    .by_id(&id)
                    .and_then(|project| project.primary())
                    .map(Path::to_path_buf);
                let workspace = primary
                    .or(workspace_override)
                    .unwrap_or_else(|| self.config.workspace());
                (workspace.clone(), Some(workspace), Some(id))
            }
            None => match workspace_override {
                Some(path) => (path.clone(), Some(path), None),
                None => (self.config.workspace(), None, None),
            },
        };
        let scope = crate::draft_store::scope_key(
            project_id.as_deref(),
            legacy_project.as_deref(),
        );

        // Pressing the same scope's “+” again while its draft is open must not
        // replace the draft or clear the half-written prompt.
        if self.active_is_unsent_draft_for(&scope) {
            self.project_menu = ProjectMenuState::default();
            self.project_editor = None;
            self.settings_open = false;
            if let Some(id) = self.active_id() {
                let mut changed = self.drafts.set_last_active(&id);
                if self.drafts.scope_session(&scope) != Some(id.as_str()) {
                    self.drafts.bind_scope(&scope, &id);
                    changed = true;
                }
                if changed {
                    self.save_drafts_now();
                }
            }
            self.composer_focus_handle(cx).focus(window);
            cx.notify();
            return;
        }

        // A scope may already own an unsent conversation that is not active
        // (the user looked at another session meanwhile). Reopen it rather
        // than creating a second one.
        if let Some(id) = self.drafts.scope_session(&scope).map(str::to_string) {
            let still_unsent = self
                .store
                .load(&id)
                .is_some_and(|state| state.messages.is_empty() && !state.archived);
            if still_unsent {
                self.select_session(&id, window, cx);
                if self.active_id().as_deref() == Some(id.as_str()) {
                    self.project_menu = ProjectMenuState::default();
                    self.project_editor = None;
                    self.notice = None;
                    self.composer_focus_handle(cx).focus(window);
                    cx.notify();
                    return;
                }
            } else {
                // The transcript vanished, was archived or already received
                // its first message: drop the stale binding but keep the
                // composer text in case it belongs to a follow-up turn.
                self.drafts.unbind_session(&id);
                self.save_drafts_now();
            }
        }

        // Leaving a different unsent conversation behind: keep it bound to its
        // own scope so its text and model come back later.
        self.remember_active_draft(cx);
        self.persist_active(cx);
        self.retire_computer_session(cx);
        let mut session = self.models.new_session_state(workspace);
        session.permission_mode = self.config.effective_default_mode();
        session.project_id = project_id;
        session.project = legacy_project;
        let id = session.id.clone();
        let _ = self.store.save(&session);
        self.drafts.bind_scope(&scope, &id);
        self.drafts.set_last_active(&id);
        self.save_drafts_now();
        self.active = Some(ActiveSession {
            shared: new_shared_session(session),
        });
        self.project_menu = ProjectMenuState::default();
        self.project_editor = None;
        self.settings_open = false;
        self.reset_subagents();
        self.sync_side_pane_workspace(cx);
        self.last_error = None;
        self.notice = None;
        self.expanded_reasoning.clear();
        self.expanded_tools.clear();
        self.expanded_turns.clear();
        self.collapsed_turns.clear();
        self.clear_message_editing();
        self.chat.reset_session();
        self.prompt_rail = PromptRailState::default();
        self.jump_flash = None;
        self.jump_flash_seq = 0;
        self.chat_dirty = true;
        self.chat.pin_to_bottom();
        self.graph.invalidate();
        self.graph.clear_selection();
        self.composer_attachments.clear();
        // The composer menu belongs to the outgoing conversation/workspace.
        self.composer_menu.close();
        self.composer_state
            .update(cx, |state, _| state.set_content_silent(String::new()));
        self.refresh_sessions(cx);
        self.composer_focus_handle(cx).focus(window);
        cx.notify();
    }

    /// Native multi-select folder picker behind the projects header's "+".
    /// The selected folders become ONE project whose first folder is primary.
    fn pick_project_folders(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: true,
            prompt: Some("新建项目（可多选文件夹）".into()),
        });
        cx.spawn(async move |weak, cx| {
            let Ok(result) = receiver.await else {
                return;
            };
            let _ = weak.update(cx, |this, cx| match result {
                Ok(Some(paths)) => this.create_project_from_folders(paths, cx),
                Ok(None) => {}
                Err(error) => {
                    this.last_error = Some(format!("无法打开文件夹选择器：{error}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn create_project_from_folders(&mut self, folders: Vec<PathBuf>, cx: &mut Context<Self>) {
        let mut fresh: Vec<PathBuf> = Vec::new();
        let mut duplicates: Vec<String> = Vec::new();
        for folder in folders {
            let folder = normalize_root(&folder);
            if fresh.iter().any(|existing| existing == &folder) {
                continue;
            }
            match self.projects.project_for_path(&folder) {
                Some(existing) => duplicates.push(existing.name.clone()),
                None => fresh.push(folder),
            }
        }
        if fresh.is_empty() {
            if let Some(name) = duplicates.first() {
                self.notice = Some(format!("所选文件夹已在项目「{name}」中。"));
            }
            cx.notify();
            return;
        }
        match self.projects.create(&fresh) {
            Ok(project) => {
                if let Err(error) = self.projects.save() {
                    self.last_error = Some(error);
                }
                // A freshly added project should be visible immediately.
                self.config.sidebar_collapsed_sections.retain(|group| {
                    group != "projects" && group != &project_group_key(&project.id)
                });
                let _ = self.config.save();
                let mut message = if project.roots.len() > 1 {
                    format!(
                        "已新建项目「{}」：{} 个文件夹（主文件夹 {}）。",
                        project.name,
                        project.roots.len(),
                        project
                            .primary()
                            .map(|path| path.display().to_string())
                            .unwrap_or_default()
                    )
                } else {
                    format!("已新建项目「{}」。悬停项目可新建任务。", project.name)
                };
                if !duplicates.is_empty() {
                    message.push_str(" 部分文件夹已在其他项目中，已跳过。");
                }
                self.notice = Some(message);
            }
            Err(error) => self.last_error = Some(error),
        }
        cx.notify();
    }

    // --- project folder editor -------------------------------------------

    fn open_project_editor(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.projects.by_id(id) else {
            self.last_error = Some("项目不存在。".to_string());
            cx.notify();
            return;
        };
        let state = ProjectEditorState {
            id: project.id.clone(),
            name: project.name.clone(),
            roots: project.root_paths(),
            primary: project.primary().map(Path::to_path_buf),
            error: None,
        };
        self.project_menu = ProjectMenuState::default();
        self.project_editor_name.update(cx, |input, cx| {
            input.set_content(&state.name, window, cx);
        });
        self.project_editor = Some(state);
        cx.notify();
    }

    fn close_project_editor(&mut self, cx: &mut Context<Self>) {
        self.project_editor = None;
        cx.notify();
    }

    fn project_editor_add_folders(&mut self, cx: &mut Context<Self>) {
        if self.project_editor.is_none() {
            return;
        }
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: true,
            prompt: Some("添加文件夹".into()),
        });
        cx.spawn(async move |weak, cx| {
            let Ok(result) = receiver.await else {
                return;
            };
            let _ = weak.update(cx, |this, cx| {
                let outcome = match result {
                    Ok(Some(paths)) => {
                        if let Some(editor) = this.project_editor.as_mut() {
                            for path in paths {
                                let path = normalize_root(&path);
                                if !editor.roots.iter().any(|root| root == &path) {
                                    editor.roots.push(path);
                                }
                            }
                            if editor.primary.is_none() {
                                editor.primary = editor.roots.first().cloned();
                            }
                            editor.error = None;
                        }
                        Ok(())
                    }
                    Ok(None) => Ok(()),
                    Err(error) => Err(format!("无法打开文件夹选择器：{error}")),
                };
                if let Err(error) = outcome {
                    if let Some(editor) = this.project_editor.as_mut() {
                        editor.error = Some(error);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn project_editor_remove_root(&mut self, root: &Path, cx: &mut Context<Self>) {
        let Some(editor) = self.project_editor.as_mut() else {
            return;
        };
        editor.roots.retain(|candidate| candidate != root);
        if editor.primary.as_deref() == Some(root) {
            editor.primary = editor.roots.first().cloned();
        }
        cx.notify();
    }

    fn project_editor_make_primary(&mut self, root: &Path, cx: &mut Context<Self>) {
        let Some(editor) = self.project_editor.as_mut() else {
            return;
        };
        if editor.roots.iter().any(|candidate| candidate == root) {
            editor.primary = Some(root.to_path_buf());
            cx.notify();
        }
    }

    fn save_project_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.project_editor.clone() else {
            return;
        };
        let name = self.project_editor_name.read(cx).content().to_string();
        let result = self
            .projects
            .update(&editor.id, &name, &editor.roots, editor.primary.as_deref())
            .and_then(|()| self.projects.save());
        match result {
            Ok(()) => {
                self.project_editor = None;
                self.notice = Some("项目文件夹已更新。".to_string());
            }
            Err(error) => {
                if let Some(editor) = self.project_editor.as_mut() {
                    editor.error = Some(error);
                }
            }
        }
        window.focus(&self.composer_state.read(cx).focus_handle());
        cx.notify();
    }

    fn select_session(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_id().as_deref() == Some(id) {
            // Clicking the active row still counts as reading it.
            self.mark_session_read(id, cx);
            return;
        }
        if self.active_run.is_some() {
            self.notice = Some("任务运行中，先停止后再切换会话。".to_string());
            cx.notify();
            return;
        }
        // Capture the outgoing composer text and unsent-draft binding before
        // the active session changes.
        self.remember_active_draft(cx);
        self.persist_active(cx);
        self.retire_computer_session(cx);
        let Some(mut state) = self.store.load(id) else {
            self.last_error = Some("无法读取该会话。".to_string());
            cx.notify();
            return;
        };
        self.models.normalize_session(&mut state);
        self.active = Some(ActiveSession {
            shared: new_shared_session(state),
        });
        self.settings_open = false;
        self.reset_subagents();
        self.restore_subagent_history();
        self.sync_side_pane_workspace(cx);
        self.last_error = None;
        self.notice = None;
        self.expanded_reasoning.clear();
        self.expanded_tools.clear();
        self.expanded_turns.clear();
        self.collapsed_turns.clear();
        self.clear_message_editing();
        self.chat.reset_session();
        self.prompt_rail = PromptRailState::default();
        self.jump_flash = None;
        self.jump_flash_seq = 0;
        self.chat_dirty = true;
        self.chat.pin_to_bottom();
        self.graph.invalidate();
        self.graph.clear_selection();
        // Bring back whatever was typed here before the user switched away.
        self.restore_composer_draft(cx);
        self.remember_active_draft(cx);
        self.mark_session_read(id, cx);
        self.composer_focus_handle(cx).focus(window);
        cx.notify();
    }

    /// Toggle a session's pinned flag. Pinned conversations move into the
    /// sidebar's 置顶 section and out of their project/task group.
    fn toggle_pin(&mut self, id: &str, cx: &mut Context<Self>) {
        let is_active = self.active_id().as_deref() == Some(id);
        let (result, pinned) = if is_active {
            let Some(shared) = self.active.as_ref().map(|active| active.shared.clone()) else {
                return;
            };
            let pinned = {
                let mut session = lock_session(&shared);
                session.pinned = !session.pinned;
                session.pinned
            };
            (self.save_active_metadata(&shared), pinned)
        } else {
            match self.store.load_for_update(id) {
                Some(mut session) => {
                    session.pinned = !session.pinned;
                    (self.store.save(&session), session.pinned)
                }
                None => (Err("无法读取该会话。".to_string()), false),
            }
        };
        if let Err(error) = result {
            self.last_error = Some(error);
        } else {
            self.notice = Some(if pinned {
                "已置顶。".to_string()
            } else {
                "已取消置顶。".to_string()
            });
            // Pinning changes the rendered groups even when the raw order is
            // unchanged; let the next summary assignment start the FLIP.
            self.sidebar_grouping_dirty = true;
        }
        self.refresh_sessions(cx);
        cx.notify();
    }

    /// Persist a metadata-only edit to the active session. The synchronous
    /// save makes the new flags visible to the immediately following session
    /// scans (e.g. the 归档 page), then the same snapshot is queued behind any
    /// already-staged writes so an older transcript cannot win afterwards.
    pub(crate) fn save_active_metadata(&mut self, shared: &SharedSession) -> Result<(), String> {
        let snapshot = lock_session(shared).clone();
        // Serialize once. Write the JSON synchronously so the immediately
        // following session scan sees the metadata, then queue the same
        // payload behind older staged writes so an older transcript cannot
        // win afterwards. The previous `save` + `stage` pair serialized the
        // whole session twice on the UI thread.
        let save = self.store.stage(&snapshot)?;
        save.write()?;
        let _ = self.persist_tx.send(save);
        Ok(())
    }

    /// Archive a conversation: it leaves the sidebar and is kept in
    /// Settings → 归档 until restored or deleted. Archiving is the "this work
    /// is finished" action, so it replaces the old per-row delete button.
    fn archive_session(&mut self, id: &str, cx: &mut Context<Self>) {
        let is_active = self.active_id().as_deref() == Some(id);
        if is_active && self.active_run.is_some() {
            self.notice = Some("任务运行中，不能归档当前会话。".to_string());
            cx.notify();
            return;
        }
        let result = if is_active {
            let Some(shared) = self.active.as_ref().map(|active| active.shared.clone()) else {
                return;
            };
            {
                let mut session = lock_session(&shared);
                session.archived = true;
                session.touch();
            }
            self.save_active_metadata(&shared)
        } else {
            match self.store.load_for_update(id) {
                Some(mut session) => {
                    session.archived = true;
                    session.touch();
                    self.store.save(&session)
                }
                None => Err("无法读取该会话。".to_string()),
            }
        };
        if let Err(error) = result {
            self.last_error = Some(error);
            cx.notify();
            return;
        }

        if is_active {
            self.activate_session_after_removal(id, cx);
        }

        self.notice = Some("会话已归档，可在 设置 → 归档 中恢复。".to_string());
        self.refresh_sessions(cx);
        self.refresh_archived_sessions(cx);
        cx.notify();
    }

    /// Replace an active conversation that just left the sidebar (archived or
    /// deleted). Picks the next history row, else the task scope's unsent
    /// draft, else a fresh task draft — and restores that session's composer
    /// draft text so both removal paths behave like a normal switch.
    fn activate_session_after_removal(&mut self, removed_id: &str, cx: &mut Context<Self>) {
        self.active = None;
        self.settings_open = false;
        if let Some(summary) = self
            .sessions
            .iter()
            .find(|summary| summary.id != removed_id)
            .cloned()
            && let Some(mut state) = self.store.load(&summary.id)
        {
            self.models.normalize_session(&mut state);
            self.active = Some(ActiveSession {
                shared: new_shared_session(state),
            });
        }
        if self.active.is_none() {
            let mut task_draft = self
                .drafts
                .scope_session(TASK_SCOPE)
                .and_then(|id| self.store.load(id))
                .filter(|session| session.messages.is_empty() && !session.archived);
            if let Some(state) = task_draft.as_mut() {
                self.models.normalize_session(state);
                self.active = Some(ActiveSession {
                    shared: new_shared_session(state.clone()),
                });
            } else {
                let mut session = self.models.new_session_state(self.config.workspace());
                session.permission_mode = self.config.effective_default_mode();
                let id = session.id.clone();
                let _ = self.store.save(&session);
                self.drafts.bind_scope(TASK_SCOPE, &id);
                self.active = Some(ActiveSession {
                    shared: new_shared_session(session),
                });
            }
        }
        self.reset_subagents();
        self.sync_side_pane_workspace(cx);
        self.expanded_reasoning.clear();
        self.expanded_tools.clear();
        self.expanded_turns.clear();
        self.collapsed_turns.clear();
        self.clear_message_editing();
        self.chat.reset_session();
        self.prompt_rail = PromptRailState::default();
        self.jump_flash = None;
        self.jump_flash_seq = 0;
        self.chat_dirty = true;
        self.chat.pin_to_bottom();
        self.graph.invalidate();
        self.graph.clear_selection();
        self.restore_composer_draft(cx);
        self.remember_active_draft(cx);
        if let Some(id) = self.active_id() {
            self.mark_session_read(&id, cx);
        }
    }

    /// Permanently remove a conversation from the sidebar's “more” menu. The
    /// menu arms the confirmation first; this performs the second click.
    fn delete_session(&mut self, id: &str, cx: &mut Context<Self>) {
        let is_active = self.active_id().as_deref() == Some(id);
        if is_active && self.active_run.is_some() {
            self.notice = Some("任务运行中，不能删除当前会话。".to_string());
            self.session_menu = SessionMenuState::default();
            cx.notify();
            return;
        }
        self.session_menu = SessionMenuState::default();
        if let Err(error) = self.store.delete(id) {
            self.last_error = Some(error);
            cx.notify();
            return;
        }
        crate::checkpoint::remove_session_checkpoints(id);
        self.drafts.remove_session(id);
        self.save_drafts_now();
        if is_active {
            self.activate_session_after_removal(id, cx);
        }
        self.notice = Some("会话已删除。".to_string());
        self.refresh_sessions(cx);
        // A conversation can also be deleted while its archived page is open.
        self.refresh_archived_sessions(cx);
        cx.notify();
    }

    /// Restore an archived conversation back into the sidebar.
    pub(crate) fn restore_archived_session(&mut self, id: &str, cx: &mut Context<Self>) {
        let result = match self.store.load_for_update(id) {
            Some(mut session) => {
                session.archived = false;
                session.touch();
                self.store.save(&session)
            }
            None => Err("无法读取该会话。".to_string()),
        };
        match result {
            Ok(()) => {
                self.settings.status = Some((true, "已恢复到侧栏。".to_string()));
                self.settings.archived_delete_confirm = None;
                self.refresh_sessions(cx);
            }
            Err(error) => self.settings.status = Some((false, error)),
        }
        self.refresh_archived_sessions(cx);
        cx.notify();
    }

    /// Permanently delete an archived transcript. Two-step, like the old
    /// sidebar delete button: the first click arms, the second removes.
    pub(crate) fn delete_archived_session(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.settings.archived_delete_confirm.as_deref() != Some(id) {
            self.settings.archived_delete_confirm = Some(id.to_string());
            cx.notify();
            return;
        }
        self.settings.archived_delete_confirm = None;
        match self.store.delete(id) {
            Ok(()) => {
                crate::checkpoint::remove_session_checkpoints(id);
                self.drafts.remove_session(id);
                self.save_drafts_now();
                self.settings.status = Some((true, "已删除归档会话。".to_string()));
            }
            Err(error) => self.settings.status = Some((false, error)),
        }
        self.refresh_sessions(cx);
        self.refresh_archived_sessions(cx);
        cx.notify();
    }

    fn sidebar_section_collapsed(&self, group: &str) -> bool {
        self.config
            .sidebar_collapsed_sections
            .iter()
            .any(|entry| entry == group)
    }

    fn toggle_sidebar_section(&mut self, group: &str, cx: &mut Context<Self>) {
        let expanding = if let Some(index) = self
            .config
            .sidebar_collapsed_sections
            .iter()
            .position(|entry| entry == group)
        {
            self.config.sidebar_collapsed_sections.remove(index);
            true
        } else {
            self.config
                .sidebar_collapsed_sections
                .push(group.to_string());
            false
        };
        let _ = self.config.save();
        // Rows below the section move by its full height; a paint-time
        // transition turns that layout jump into motion.
        //
        // 展开用临界阻尼 spring（长距离下落、连续点按不丢速度）；收回改用
        // 220ms ease-out，动作敏捷不拖尾。新的行在渲染端按 50ms stagger 渐
        // 显，与下方行的位移互不重叠。Start after the disk write so the
        // animation budget is not eaten by config persistence.
        self.motion.begin(if expanding {
            MotionSpec::SPRING_LAYOUT
        } else {
            MotionSpec::LAYOUT
        });
        cx.notify();
    }

    fn refresh_archived_sessions(&mut self, cx: &mut Context<Self>) {
        let store = self.store.clone();
        cx.spawn(async move |weak, cx| {
            let summaries = cx
                .background_executor()
                .spawn(async move { store.archived_summaries() })
                .await;
            weak.update(cx, |this, cx| {
                this.archived_sessions = summaries;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn open_project_menu(
        &mut self,
        target: ProjectGroup,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.project_menu.target = Some(target);
        self.project_menu.position = Some(position);
        cx.notify();
    }

    pub(crate) fn close_project_menu(&mut self, cx: &mut Context<Self>) {
        self.project_menu = ProjectMenuState::default();
        cx.notify();
    }

    /// Open the conversation-row “more” popover. Anchored to the click point,
    /// with the project menu closed so only one popover owns the backdrop.
    fn open_session_menu(&mut self, id: &str, position: Point<Pixels>, cx: &mut Context<Self>) {
        self.session_menu = SessionMenuState {
            target: Some(id.to_string()),
            position: Some(position),
            confirm_delete: false,
        };
        self.project_menu = ProjectMenuState::default();
        self.model_menu.open = false;
        cx.notify();
    }

    pub(crate) fn close_session_menu(&mut self, cx: &mut Context<Self>) {
        if self.session_menu.target.is_none() {
            return;
        }
        self.session_menu = SessionMenuState::default();
        cx.notify();
    }

    /// Set or clear the sidebar's unread marker. This is UI state only, so it
    /// never touches `updated_at` and never reorders the list.
    fn set_session_unread(&mut self, id: &str, unread: bool, cx: &mut Context<Self>) {
        self.session_menu = SessionMenuState::default();
        let is_active = self.active_id().as_deref() == Some(id);
        let result = if is_active {
            let Some(shared) = self.active_shared() else {
                return;
            };
            {
                let mut session = lock_session(&shared);
                session.unread = unread;
            }
            self.save_active_metadata(&shared)
        } else {
            match self.store.load_for_update(id) {
                Some(mut session) => {
                    session.unread = unread;
                    self.store.save(&session)
                }
                None => Err("无法读取该会话。".to_string()),
            }
        };
        match result {
            Ok(()) => {
                if let Some(summary) = self.sessions.iter_mut().find(|summary| summary.id == id) {
                    summary.unread = unread;
                }
                self.notice = Some(
                    if unread {
                        "已标为未读。"
                    } else {
                        "已标为已读。"
                    }
                    .to_string(),
                );
            }
            Err(error) => self.last_error = Some(error),
        }
        cx.notify();
    }

    /// Opening a conversation clears its unread marker.
    fn mark_session_read(&mut self, id: &str, cx: &mut Context<Self>) {
        let was_unread = self
            .sessions
            .iter()
            .find(|summary| summary.id == id)
            .is_some_and(|summary| summary.unread);
        if !was_unread {
            return;
        }
        if let Some(summary) = self.sessions.iter_mut().find(|summary| summary.id == id) {
            summary.unread = false;
        }
        let is_active = self.active_id().as_deref() == Some(id);
        let result = if is_active {
            let Some(shared) = self.active_shared() else {
                return;
            };
            lock_session(&shared).unread = false;
            self.save_active_metadata(&shared)
        } else {
            match self.store.load_for_update(id) {
                Some(mut session) => {
                    session.unread = false;
                    self.store.save(&session)
                }
                None => Ok(()),
            }
        };
        if let Err(error) = result {
            self.last_error = Some(error);
        }
        cx.notify();
    }

    fn open_session_rename(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(summary) = self.sessions.iter().find(|summary| summary.id == id).cloned() else {
            return;
        };
        self.session_menu = SessionMenuState::default();
        let name = if summary.title.trim().is_empty() {
            "新的会话".to_string()
        } else {
            summary.title
        };
        self.session_rename_name
            .update(cx, |input, cx| input.set_content(name, window, cx));
        self.session_rename = Some(SessionRenameState {
            id: id.to_string(),
            error: None,
        });
        window.focus(&self.session_rename_name.read(cx).focus_handle());
        cx.notify();
    }

    pub(crate) fn close_session_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session_rename.take().is_none() {
            return;
        }
        window.focus(&self.composer_state.read(cx).focus_handle());
        cx.notify();
    }

    fn save_session_rename(
        &mut self,
        submitted: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(rename) = self.session_rename.clone() else {
            return;
        };
        // Enter submits through `TextInputView`, which takes the content out
        // before invoking the handler; put it back first so the dialog keeps
        // its value if validation or I/O fails.
        let name = match submitted {
            Some(text) => {
                self.session_rename_name.update(cx, |state, cx| {
                    state.set_content(text.clone(), window, cx)
                });
                text
            }
            None => self.session_rename_name.read(cx).content().to_string(),
        };
        let name: String = name.trim().chars().take(120).collect();
        if name.is_empty() {
            if let Some(state) = self.session_rename.as_mut() {
                state.error = Some("会话名称不能为空。".to_string());
            }
            cx.notify();
            return;
        }
        let is_active = self.active_id().as_deref() == Some(rename.id.as_str());
        let result = if is_active {
            let Some(shared) = self.active_shared() else {
                return;
            };
            {
                let mut session = lock_session(&shared);
                session.title = name.clone();
            }
            self.save_active_metadata(&shared)
        } else {
            match self.store.load_for_update(&rename.id) {
                Some(mut session) => {
                    session.title = name.clone();
                    self.store.save(&session)
                }
                None => Err("无法读取该会话。".to_string()),
            }
        };
        match result {
            Ok(()) => {
                self.session_rename = None;
                if let Some(summary) = self
                    .sessions
                    .iter_mut()
                    .find(|summary| summary.id == rename.id)
                {
                    summary.title = name;
                }
                self.notice = Some("会话名称已更新。".to_string());
                self.refresh_sessions(cx);
            }
            Err(error) => {
                if let Some(state) = self.session_rename.as_mut() {
                    state.error = Some(error);
                }
            }
        }
        window.focus(&self.composer_state.read(cx).focus_handle());
        cx.notify();
    }

    /// Ask where to save, then bundle the structured transcript tree and the
    /// files it referenced into one zip.
    fn export_session(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(summary) = self.sessions.iter().find(|summary| summary.id == id).cloned() else {
            return;
        };
        self.session_menu = SessionMenuState::default();
        let directory = dirs::download_dir()
            .or_else(dirs::document_dir)
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| self.config.workspace());
        let suggested = export_file_name(&summary.title);
        let receiver = cx.prompt_for_new_path(&directory, Some(suggested.as_str()));
        self.notice = Some("选择导出位置…".to_string());
        cx.notify();
        cx.spawn(async move |weak, cx| {
            let path = match receiver.await {
                Ok(Ok(Some(path))) => path,
                Ok(Ok(None)) => {
                    let _ = weak.update(cx, |this, cx| {
                        if this.notice.as_deref() == Some("选择导出位置…") {
                            this.notice = None;
                        }
                        cx.notify();
                    });
                    return;
                }
                Ok(Err(error)) => {
                    let _ = weak.update(cx, |this, cx| {
                        this.notice = None;
                        this.last_error = Some(format!("无法打开保存对话框：{error}"));
                        cx.notify();
                    });
                    return;
                }
                Err(_) => {
                    let _ = weak.update(cx, |this, cx| {
                        if this.notice.as_deref() == Some("选择导出位置…") {
                            this.notice = None;
                        }
                        cx.notify();
                    });
                    return;
                }
            };
            let _ = weak.update(cx, |this, cx| {
                this.begin_session_export(&summary.id, normalize_export_path(path), cx);
            });
        })
        .detach();
    }

    fn begin_session_export(&mut self, id: &str, destination: PathBuf, cx: &mut Context<Self>) {
        let active_shared = if self.active_id().as_deref() == Some(id) {
            self.active_shared()
        } else {
            None
        };
        // Resolve the project's extra folders from the summary so the session
        // clone/load can happen entirely off the UI thread.
        let project_roots = self
            .sessions
            .iter()
            .find(|summary| summary.id == id)
            .and_then(|summary| self.project_group_for_summary(summary))
            .map(|group| group.roots)
            .unwrap_or_default();
        let store = self.store.clone();
        let id = id.to_string();
        self.notice = Some("正在导出会话…".to_string());
        self.last_error = None;
        cx.notify();
        cx.spawn(async move |weak, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let main = match active_shared {
                        Some(shared) => lock_session(&shared).clone(),
                        None => store
                            .load_raw(&id)
                            .ok_or_else(|| format!("无法读取会话 {id}"))?,
                    };
                    let children = crate::session_export::descendants(&store, &main.id);
                    crate::session_export::export(
                        &main,
                        &children,
                        &project_roots,
                        &destination,
                    )
                })
                .await;
            let _ = weak.update(cx, |this, cx| {
                match result {
                    Ok(report) => {
                        let missing = if report.missing > 0 {
                            format!("，{} 个文件缺失", report.missing)
                        } else {
                            String::new()
                        };
                        this.notice = Some(format!(
                            "已导出 {} 个子代理、{} 个文件{} → {}",
                            report.subagents,
                            report.files,
                            missing,
                            report.path.display()
                        ));
                    }
                    Err(error) => {
                        this.notice = None;
                        this.last_error = Some(format!("导出失败：{error}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn reveal_project_folder(&mut self, project: &Path, cx: &mut Context<Self>) {
        if let Err(error) = open_in_file_manager(project) {
            self.last_error = Some(format!("无法打开文件夹：{error}"));
        }
        self.project_menu = ProjectMenuState::default();
        cx.notify();
    }

    pub(crate) fn toggle_theme(&mut self, cx: &mut Context<Self>) {
        self.theme = self.theme.toggle();
        self.config.theme = if self.theme.is_dark() {
            "dark".to_string()
        } else {
            "light".to_string()
        };
        self.theme.clone().set_global(cx);
        let _ = self.config.save();
        cx.notify();
    }

    /// Step the whole-app UI zoom (see `Window::set_ui_zoom`) by `delta`,
    /// snapping to 0.1 so repeated shortcuts land on clean percentages.
    pub(crate) fn zoom_by(&mut self, delta: f32, window: &mut Window, cx: &mut Context<Self>) {
        let next = ((self.config.ui_zoom + delta) * 10.0).round() / 10.0;
        self.zoom_to(next, window, cx);
    }

    /// Set the whole-app UI zoom. Persisted app-wide so the shell reopens at
    /// the zoom the user left it at.
    pub(crate) fn zoom_to(&mut self, zoom: f32, window: &mut Window, cx: &mut Context<Self>) {
        let zoom = zoom.clamp(MIN_UI_ZOOM, MAX_UI_ZOOM);
        if (zoom - self.config.ui_zoom).abs() < 1e-4 {
            return;
        }
        self.config.ui_zoom = zoom;
        window.set_ui_zoom(zoom);
        let _ = self.config.save();
        cx.notify();
    }

    /// Hide the entire left sidebar (or restore it), leaving only the main
    /// column. Persisted app-wide so the shell reopens in the chosen layout.
    pub(crate) fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.config.sidebar_collapsed = !self.config.sidebar_collapsed;
        if self.config.sidebar_collapsed {
            // The project popover is anchored to a sidebar row that no longer
            // exists once the column is gone.
            self.project_menu = ProjectMenuState::default();
        }
        let _ = self.config.save();
        cx.notify();
    }

    /// The active session's permission mode; follows the session like the
    /// model selection does.
    pub(crate) fn permission_mode(&self) -> PermissionMode {
        self.active_shared()
            .map(|shared| lock_session(&shared).permission_mode)
            .unwrap_or(PermissionMode::Default)
    }

    /// Shift+Tab (or a chip click) cycles the active session's mode. The mode
    /// lives on the session, so it persists with the transcript and the
    /// approval gate picks it up on the next tool call.
    pub(crate) fn cycle_permission_mode(&mut self, cx: &mut Context<Self>) {
        let Some(shared) = self.active_shared() else {
            return;
        };
        {
            let mut session = lock_session(&shared);
            session.permission_mode = session.permission_mode.next();
            session.touch();
        }
        // Persist immediately: the mode must survive an app close that races
        // the throttled background writer, just like the model selection.
        if let Err(error) = self.save_active_metadata(&shared) {
            self.last_error = Some(error);
        }
        cx.notify();
    }

    fn fill_composer(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.composer_state
            .update(cx, |state, cx| state.set_content(text, window, cx));
        self.composer_focus_handle(cx).focus(window);
        cx.notify();
    }

    /// Seed the composer with long, wrapping content for visual overflow and
    /// scroll checks (`--preview-composer`).
    pub fn seed_composer_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.preview_mode = true;
        let text = [
            "打磨一下输入框里 overflow 的问题：先复现，再修。",
            "这段文本有十四行，混合中英文、长短句和代码片段，",
            "用来检查 composer 超过 max_lines 之后的裁剪与滚动。",
            "const MAX_LINES: usize = 8; // 编辑器封顶八行，超出后滚动",
            "for (item) in 置顶 { right_align(fold_icon, plus) }",
            "for (program) in 项目s { for (session) in program { pin } }",
            "七、这是第七行 —— 应该已经开始滚动了，顶部行被裁掉。",
            "八、第八行在可视边界附近；光标必须始终停留在可视范围内。",
            "九、第九行及以下只在滚动后可见，绝不允许画到卡片外面。",
            "十、修剪 selection 高亮、光标和 hit-test 都要跟随滚动偏移。",
            "十一、滚轮只在内容溢出时消费事件，否则透传给消息列表。",
            "十二、Enter 发送、Shift+Enter 换行，行为不应有任何变化。",
            "十三、IME 组字窗口的位置也要扣除滚动偏移后再上报。",
            "十四、最后一行：光标停在这里，卡片和提示行保持干净。",
        ]
        .join("\n");
        self.fill_composer(&text, window, cx);
    }

    /// Stage one real workspace file and open the `@` menu
    /// (`--preview-composer-menu`). Interactive check for the mention menu and
    /// the attachment chip row; the file index fills in after the first frame.
    pub fn seed_composer_menu_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.preview_mode = true;
        let workspace = self.composer_workspace();
        for candidate in ["README.md", "Cargo.toml"] {
            let path = workspace.join(candidate);
            if path.is_file() {
                self.attach_paths(vec![path], cx);
                break;
            }
        }
        self.fill_composer("@", window, cx);
    }

    /// Show the floating computer-use bar and the composer hint without
    /// touching the real desktop (`--preview-computer`).
    pub fn seed_computer_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.preview_mode = true;
        self.config.computer_use_mode = ComputerUseMode::Always;
        self.computer.set_mode(ComputerUseMode::Always);
        let Some(session_id) = self
            .active
            .as_ref()
            .map(|active| lock_session(&active.shared).id.clone())
        else {
            return;
        };
        self.computer.grant(&session_id);
        self.handle_computer_overlay(
            OverlayEvent {
                session_id,
                kind: OverlayEventKind::Show {
                    message: "正在读取窗口「示例编辑器」…".to_string(),
                    paused: false,
                },
            },
            cx,
        );
        self.composer_focus_handle(cx).focus(window);
        cx.notify();
    }

    // --- command palette --------------------------------------------------

    fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.composer_menu.close();
        self.palette_open = true;
        self.palette_selected = 0;
        self.palette_selected_for.clear();
        self.palette_scroll
            .set_offset(gpui::point(px(0.0), px(0.0)));
        self.palette_state_clear(window, cx);
        self.palette_query.read(cx).focus_handle().focus(window);
        cx.notify();
    }

    fn palette_state_clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette_query
            .update(cx, |state, cx| state.clear(window, cx));
    }

    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.palette_open {
            return;
        }
        self.palette_open = false;
        self.palette_state_clear(window, cx);
        self.composer_focus_handle(cx).focus(window);
        cx.notify();
    }

    fn palette_items(&self, query: &str) -> Vec<PaletteItem> {
        let mut items = vec![
            PaletteItem {
                id: "cmd-new-session".to_string(),
                title: "新建任务".to_string(),
                subtitle: "Ctrl N".to_string(),
                icon: icons::PLUS,
                group: PaletteGroup::Suggested,
                keywords: &["new session", "create"],
                recent: false,
                action: PaletteAction::NewSession,
            },
            PaletteItem {
                id: "cmd-theme".to_string(),
                title: "切换主题".to_string(),
                subtitle: if self.theme.is_dark() {
                    "深色 → 浅色"
                } else {
                    "浅色 → 深色"
                }
                .to_string(),
                icon: if self.theme.is_dark() {
                    icons::SUN
                } else {
                    icons::MOON
                },
                group: PaletteGroup::Suggested,
                keywords: &["theme", "dark", "light", "appearance"],
                recent: false,
                action: PaletteAction::ToggleTheme,
            },
            PaletteItem {
                id: "cmd-agents".to_string(),
                title: "打开子代理面板".to_string(),
                subtitle: format!("{} 个子代理", self.subagent_tasks.len()),
                icon: icons::SPARKLES,
                group: PaletteGroup::Panel,
                keywords: &["agents", "subagent"],
                recent: false,
                action: PaletteAction::OpenAgents,
            },
            PaletteItem {
                id: "cmd-graph".to_string(),
                title: "打开智能体关系图".to_string(),
                subtitle: format!("{} 个节点 · Ctrl Shift G", self.subagent_tasks.len() + 1),
                icon: icons::BRANCH,
                group: PaletteGroup::Panel,
                keywords: &["graph", "agent graph", "map"],
                recent: false,
                action: PaletteAction::OpenGraph,
            },
            PaletteItem {
                id: "cmd-panel".to_string(),
                title: "显示 / 隐藏右侧面板".to_string(),
                subtitle: "计划、子代理、文件与变更".to_string(),
                icon: icons::PANEL_RIGHT,
                group: PaletteGroup::Panel,
                keywords: &["panel", "right panel", "side pane"],
                recent: false,
                action: PaletteAction::TogglePanel,
            },
            PaletteItem {
                id: "cmd-sidebar".to_string(),
                title: if self.config.sidebar_collapsed {
                    "显示左侧栏".to_string()
                } else {
                    "隐藏左侧栏".to_string()
                },
                subtitle: if self.config.sidebar_collapsed {
                    "恢复会话侧栏 · Ctrl B"
                } else {
                    "整个左栏收起，只保留主窗口 · Ctrl B"
                }
                .to_string(),
                icon: icons::PANEL_LEFT,
                group: PaletteGroup::Panel,
                keywords: &["sidebar", "left sidebar", "collapse", "会话", "侧栏"],
                recent: false,
                action: PaletteAction::ToggleSidebar,
            },
            PaletteItem {
                id: "cmd-settings".to_string(),
                title: "设置".to_string(),
                subtitle: "权限、工作区、外观".to_string(),
                icon: icons::SETTINGS,
                group: PaletteGroup::Suggested,
                keywords: &["settings", "preferences"],
                recent: false,
                action: PaletteAction::OpenSettings,
            },
            PaletteItem {
                id: "cmd-models".to_string(),
                title: "模型设置".to_string(),
                subtitle: format!(
                    "{} 个供应商 · 当前 {}",
                    self.models.file().providers.len(),
                    self.active_resolved_model().model_label()
                ),
                icon: icons::SPARKLES,
                group: PaletteGroup::Suggested,
                keywords: &["models", "provider"],
                recent: false,
                action: PaletteAction::OpenModels,
            },
            PaletteItem {
                id: "cmd-focus".to_string(),
                title: "聚焦输入框".to_string(),
                subtitle: "Ctrl L".to_string(),
                icon: icons::EDIT,
                group: PaletteGroup::Suggested,
                keywords: &["focus", "composer", "input"],
                recent: false,
                action: PaletteAction::FocusComposer,
            },
            PaletteItem {
                id: "cmd-stop".to_string(),
                title: "停止当前任务".to_string(),
                subtitle: "Esc".to_string(),
                icon: icons::STOP,
                group: PaletteGroup::Suggested,
                keywords: &["stop", "interrupt", "cancel"],
                recent: false,
                action: PaletteAction::StopRun,
            },
        ];
        for summary in self.sessions.iter().take(12) {
            items.push(PaletteItem {
                id: format!("session-{}", summary.id),
                title: summary.title.clone(),
                subtitle: format!(
                    "会话 · {} · {} 条消息",
                    relative_time(summary.updated_at),
                    summary.message_count
                ),
                icon: icons::CHAT,
                group: PaletteGroup::Session,
                keywords: &[],
                recent: false,
                action: PaletteAction::SelectSession(summary.id.clone()),
            });
        }
        order_palette_items(items, query, &self.config.palette_recent)
    }

    /// Move the keyboard selection by `delta`, wrapping at both ends. The
    /// selection resets to the first row whenever the query changed. The
    /// highlighted row is scrolled into view so keyboard navigation can never
    /// land on an entry the user cannot see.
    fn palette_move_selection(&mut self, delta: i32, cx: &mut Context<Self>) {
        let query = self.palette_query.read(cx).content().to_string();
        let items = self.palette_items(&query);
        let count = items.len();
        if count == 0 {
            return;
        }
        if self.palette_selected_for != query {
            self.palette_selected = 0;
            self.palette_scroll
                .set_offset(gpui::point(px(0.0), px(0.0)));
        }
        let next = (self.palette_selected as i32 + delta).rem_euclid(count as i32);
        self.palette_selected = next as usize;
        self.palette_selected_for = query;
        self.palette_scroll
            .scroll_to_item(palette_row_index(&items, self.palette_selected));
        cx.notify();
    }

    /// Index of the row Enter should execute; 0 for a stale or unset
    /// selection so a freshly typed query always starts at the top.
    fn palette_selection_index(&self, query: &str) -> usize {
        if self.palette_selected_for != query {
            return 0;
        }
        self.palette_selected
    }

    fn run_palette_selection(&mut self, query: &str, window: &mut Window, cx: &mut Context<Self>) {
        let items = self.palette_items(query);
        let index = self
            .palette_selection_index(query)
            .min(items.len().saturating_sub(1));
        let Some(item) = items.into_iter().nth(index) else {
            return;
        };
        self.execute_palette_item(item, window, cx);
    }

    fn execute_palette_item(
        &mut self,
        item: PaletteItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.palette_open = false;
        self.palette_selected = 0;
        self.palette_selected_for.clear();
        self.palette_state_clear(window, cx);
        self.config.record_palette_use(&item.id);
        let _ = self.config.save();
        match item.action {
            PaletteAction::NewSession => self.new_session(window, cx),
            PaletteAction::ToggleTheme => self.toggle_theme(cx),
            PaletteAction::OpenSettings => self.open_settings(window, cx),
            PaletteAction::OpenModels => self.open_settings_models(window, cx),
            PaletteAction::TogglePanel => {
                self.side.open = !self.side.open;
                self.persist_side_pane();
                cx.notify();
            }
            PaletteAction::ToggleSidebar => self.toggle_sidebar(cx),
            PaletteAction::FocusComposer => {
                self.composer_focus_handle(cx).focus(window);
            }
            PaletteAction::StopRun => self.interrupt(cx),
            PaletteAction::OpenAgents => {
                self.open_sidebar_kind(SidePaneTab::agents(), cx);
            }
            PaletteAction::OpenGraph => {
                self.set_view_mode(ViewMode::Graph, cx);
            }
            PaletteAction::SelectSession(id) => self.select_session(&id, window, cx),
        }
    }

    // --- rendering -------------------------------------------------------

    fn render_titlebar(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = &self.theme;
        let maximized = window.is_maximized();
        let chat_active = self.view_mode == ViewMode::Chat;
        let graph_active = self.view_mode == ViewMode::Graph;
        let running_agents = self
            .subagent_tasks
            .iter()
            .filter(|task| task.status == SubagentStatus::Running)
            .count();
        div()
            .h(px(52.0))
            .flex()
            .flex_none()
            .items_center()
            .bg(theme.bg)
            .border_b_1()
            .border_color(theme.border)
            .when(self.sidebar_p > 0.001, |element| {
                element.child(
                    div()
                        .w(px(SIDEBAR_WIDTH * self.sidebar_p))
                        .h_full()
                        .flex_none()
                        .when(self.sidebar_p < 1.0, |clip| clip.overflow_hidden())
                        .bg(theme.sidebar)
                        .border_r_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .w(px(SIDEBAR_WIDTH))
                                .h_full()
                                .flex()
                                .items_center()
                                .gap_2()
                                .px_4()
                                .child(
                                    svg()
                                        .path(icons::SPARKLES)
                                        .size(px(16.0))
                                        .text_color(theme.text),
                                )
                                .child(
                                    div()
                                        .text_size(px(13.5))
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .text_color(theme.text)
                                        .child("EffiBuddy"),
                                )
                                .child(div().flex_1())
                                .child(
                                    IconButton::new("sidebar-toggle", icons::PANEL_LEFT, theme)
                                        .size(26.0, 13.0)
                                        .tooltip("隐藏左侧栏 (Ctrl B)")
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.toggle_sidebar(cx)),
                                        ),
                                ),
                        ),
                )
            })
            .when(self.sidebar_p < 0.999, |element| {
                element.child(
                    div().ml(px(8.0)).opacity(1.0 - self.sidebar_p).child(
                        IconButton::new("sidebar-expand", icons::PANEL_LEFT, theme)
                            .size(28.0, 14.0)
                            .tooltip("显示左侧栏 (Ctrl B)")
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_sidebar(cx))),
                    ),
                )
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(2.0))
                    .h(px(32.0))
                    .p(px(3.0))
                    .ml(px(12.0))
                    .rounded(px(9.0))
                    .bg(theme.surface_alt)
                    .child(
                        div()
                            .id("view-mode-chat")
                            .h(px(26.0))
                            .px(px(9.0))
                            .rounded(px(radius::XS))
                            .border_1()
                            .flex()
                            .items_center()
                            .gap(px(5.0))
                            .cursor_pointer()
                            .when(chat_active, |element| {
                                element
                                    .bg(theme.surface)
                                    .border_color(theme.border)
                                    .shadow_sm()
                            })
                            .when(!chat_active, |element| {
                                element
                                    .border_color(transparent_black())
                                    .hover(|style| style.bg(theme.hover))
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_view_mode(ViewMode::Chat, cx);
                            }))
                            .child(svg().path(icons::CHAT).size(px(12.0)).text_color(
                                if chat_active {
                                    theme.text
                                } else {
                                    theme.text_faint
                                },
                            ))
                            .child(
                                div()
                                    .text_size(px(11.5))
                                    .font_weight(if chat_active {
                                        gpui::FontWeight::MEDIUM
                                    } else {
                                        gpui::FontWeight::NORMAL
                                    })
                                    .text_color(if chat_active {
                                        theme.text
                                    } else {
                                        theme.text_muted
                                    })
                                    .child("主智能体"),
                            ),
                    )
                    .child(
                        div()
                            .id("view-mode-graph")
                            .h(px(26.0))
                            .px(px(9.0))
                            .rounded(px(radius::XS))
                            .border_1()
                            .flex()
                            .items_center()
                            .gap(px(5.0))
                            .cursor_pointer()
                            .when(graph_active, |element| {
                                element
                                    .bg(theme.surface)
                                    .border_color(theme.border)
                                    .shadow_sm()
                            })
                            .when(!graph_active, |element| {
                                element
                                    .border_color(transparent_black())
                                    .hover(|style| style.bg(theme.hover))
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_view_mode(ViewMode::Graph, cx);
                            }))
                            .child(svg().path(icons::BRANCH).size(px(12.0)).text_color(
                                if graph_active {
                                    theme.text
                                } else {
                                    theme.text_faint
                                },
                            ))
                            .child(
                                div()
                                    .text_size(px(11.5))
                                    .font_weight(if graph_active {
                                        gpui::FontWeight::MEDIUM
                                    } else {
                                        gpui::FontWeight::NORMAL
                                    })
                                    .text_color(if graph_active {
                                        theme.text
                                    } else {
                                        theme.text_muted
                                    })
                                    .child("智能体关系图"),
                            )
                            .when(running_agents > 0, |element| {
                                element.child(small_badge(
                                    theme,
                                    running_agents.to_string(),
                                    BadgeTone::Accent,
                                ))
                            }),
                    ),
            )
            .child(div().flex_1().h_full().on_mouse_down(
                MouseButton::Left,
                move |event, window, _| {
                    if event.click_count == 2 {
                        window.titlebar_double_click();
                    } else {
                        window.start_window_move();
                    }
                },
            ))
            .when(self.sidebar_p < 0.999, |element| {
                element.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(1.0))
                        .mr(px(6.0))
                        .opacity(1.0 - self.sidebar_p)
                        .child(
                            IconButton::new(
                                "titlebar-theme-toggle",
                                if theme.is_dark() {
                                    icons::SUN
                                } else {
                                    icons::MOON
                                },
                                theme,
                            )
                            .size(28.0, 14.0)
                            .tooltip("切换主题 (Ctrl+Shift+T)")
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_theme(cx))),
                        )
                        .child(
                            IconButton::new("titlebar-settings", icons::SETTINGS, theme)
                                .size(28.0, 14.0)
                                .tooltip("设置 (Ctrl+Shift+S)")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_settings(window, cx)
                                })),
                        ),
                )
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .mr(px(8.0))
                    .child(
                        self.window_button(icons::MINIMIZE, "最小化", false, |window| {
                            window.minimize_window()
                        }),
                    )
                    .child(self.window_button(
                        if maximized {
                            icons::MAXIMIZE
                        } else {
                            icons::MAXIMIZE
                        },
                        if maximized { "还原" } else { "最大化" },
                        false,
                        |window| window.zoom_window(),
                    ))
                    .child(
                        self.window_button(icons::X, "关闭", true, |window| {
                            window.remove_window()
                        }),
                    ),
            )
            .into_any_element()
    }

    fn window_button(
        &self,
        icon: &'static str,
        label: &'static str,
        danger: bool,
        action: impl Fn(&mut Window) + 'static,
    ) -> AnyElement {
        let theme = self.theme.clone();
        let hover_theme = theme.clone();
        let group = SharedString::from(format!("window-button-group-{icon}"));
        // Icons rest at text_muted like the other titlebar buttons; the danger
        // button only flips to on_accent on hover, on top of the danger hover
        // bg. (SVGs don't inherit text color, hence group_hover on the icon.)
        let mut icon_element = svg().path(icon).size(px(12.0)).text_color(theme.text_muted);
        if danger {
            let hover_icon_theme = theme.clone();
            icon_element = icon_element.group_hover(group.clone(), move |style| {
                style.text_color(hover_icon_theme.on_accent)
            });
        }
        div()
            .id(SharedString::from(format!("window-button-{icon}")))
            .group(group)
            .flex()
            .items_center()
            .justify_center()
            .w(px(46.0))
            .h(px(32.0))
            .rounded(px(radius::XS))
            .cursor_pointer()
            .hover(move |style| {
                if danger {
                    style.bg(hover_theme.danger_hover)
                } else {
                    style.bg(hover_theme.hover)
                }
            })
            .tooltip(effibuddy_ui::tooltip(&self.theme, label))
            .on_click(move |_, window, _| action(window))
            .child(icon_element)
            .into_any_element()
    }

    /// Popover for a conversation row's “more” button. Deleting is
    /// intentionally two-step: the row arms, the second click removes the
    /// transcript (and any draft held for it).
    fn render_session_menu(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let id = self.session_menu.target.clone()?;
        let position = self.session_menu.position?;
        let summary = self
            .sessions
            .iter()
            .find(|summary| summary.id == id)
            .cloned()?;
        let theme = self.theme.clone();
        let confirm = self.session_menu.confirm_delete;
        let unread = summary.unread;
        let id_for_export = id.clone();
        let id_for_rename = id.clone();
        let id_for_unread = id.clone();
        let id_for_delete = id.clone();
        let title = summary.title.clone();
        let meta = format!(
            "{} · {} 条消息",
            relative_time(summary.updated_at),
            summary.message_count
        );
        Some(
            div()
                .id("session-menu-backdrop")
                .absolute()
                .top(px(0.0))
                .left(px(0.0))
                .size_full()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.close_session_menu(cx)),
                )
                .child(
                    anchored()
                        .position(position)
                        .offset(point(px(-148.0), px(6.0)))
                        .snap_to_window_with_margin(px(12.0))
                        .child(
                            div()
                                .id("session-menu")
                                .w(px(184.0))
                                .rounded(px(radius::MD))
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.elevated)
                                .shadow(shadow_float(&theme))
                                .p(px(4.0))
                                .flex()
                                .flex_col()
                                .gap(px(1.0))
                                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                    cx.stop_propagation()
                                })
                                .child(
                                    div()
                                        .px(px(7.0))
                                        .py(px(5.0))
                                        .flex()
                                        .flex_col()
                                        .gap(px(1.0))
                                        .child(
                                            div()
                                                .truncate()
                                                .text_size(px(11.5))
                                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                                .text_color(theme.text)
                                                .child(title),
                                        )
                                        .child(
                                            div()
                                                .text_size(px(9.5))
                                                .text_color(theme.text_faint)
                                                .child(meta),
                                        ),
                                )
                                .child(div().h(px(1.0)).bg(theme.border).my(px(2.0)))
                                .child(
                                    session_menu_row(
                                        &theme,
                                        "session-menu-export",
                                        icons::DOWNLOAD,
                                        "导出会话…",
                                        false,
                                    )
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.export_session(&id_for_export, cx);
                                    })),
                                )
                                .child(
                                    session_menu_row(
                                        &theme,
                                        "session-menu-rename",
                                        icons::EDIT,
                                        "修改会话名…",
                                        false,
                                    )
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.open_session_rename(&id_for_rename, window, cx);
                                    })),
                                )
                                .child(
                                    session_menu_row(
                                        &theme,
                                        "session-menu-unread",
                                        icons::MAIL,
                                        if unread { "标为已读" } else { "标为未读" },
                                        false,
                                    )
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.set_session_unread(&id_for_unread, !unread, cx);
                                    })),
                                )
                                .child(div().h(px(1.0)).bg(theme.border).my(px(2.0)))
                                .child(
                                    session_menu_row(
                                        &theme,
                                        "session-menu-delete",
                                        icons::TRASH,
                                        if confirm {
                                            "确认删除会话"
                                        } else {
                                            "删除会话"
                                        },
                                        true,
                                    )
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        if this.session_menu.confirm_delete {
                                            this.delete_session(&id_for_delete, cx);
                                        } else {
                                            this.session_menu.confirm_delete = true;
                                            cx.notify();
                                        }
                                    })),
                                )
                                .when(confirm, |element| {
                                    element.child(
                                        div()
                                            .px(px(8.0))
                                            .pb(px(5.0))
                                            .text_size(px(9.5))
                                            .text_color(theme.text_faint)
                                            .child("删除后无法恢复。"),
                                    )
                                })
                                .with_animation(
                                    "session-menu-enter",
                                    Animation::new(overlay_motion_duration())
                                        .with_easing(ease_out_cubic),
                                    |element, delta| {
                                        element.opacity(delta).mt(px(4.0 * (1.0 - delta)))
                                    },
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    /// Centered rename dialog opened from the conversation's “more” menu.
    fn render_session_rename(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let state = self.session_rename.as_ref()?;
        let theme = self.theme.clone();
        let input = self.session_rename_name_view.clone();
        let error_row = state.error.clone().map(|error| {
            div()
                .text_size(px(11.0))
                .text_color(theme.danger)
                .child(error)
                .into_any_element()
        });
        Some(
            div()
                .id("session-rename-backdrop")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(transparent_black())
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| this.close_session_rename(window, cx)),
                )
                .child(
                    div()
                        .id("session-rename")
                        .w(px(420.0))
                        .rounded(px(radius::LG))
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.elevated)
                        .shadow(shadow_float(&theme))
                        .p(px(16.0))
                        .flex()
                        .flex_col()
                        .gap(px(12.0))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(
                            div()
                                .text_size(px(14.0))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(theme.text)
                                .child("重命名会话"),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(4.0))
                                .child(effibuddy_ui::section_label(&theme, "名称"))
                                .child(
                                    div()
                                        .h(px(34.0))
                                        .px(px(8.0))
                                        .rounded(px(radius::SM))
                                        .border_1()
                                        .border_color(theme.border)
                                        .bg(theme.surface)
                                        .flex()
                                        .items_center()
                                        .child(input),
                                )
                                .child(
                                    div()
                                        .text_size(px(10.0))
                                        .text_color(theme.text_faint)
                                        .child("最多 120 个字符；不会改变会话的排序时间。"),
                                ),
                        )
                        .children(error_row)
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_end()
                                .gap(px(8.0))
                                .child(
                                    Button::new("session-rename-cancel", "取消", &theme)
                                        .variant(ButtonVariant::Ghost)
                                        .size(ButtonSize::Sm)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.close_session_rename(window, cx);
                                        })),
                                )
                                .child(
                                    Button::new("session-rename-save", "保存", &theme)
                                        .variant(ButtonVariant::Primary)
                                        .size(ButtonSize::Sm)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.save_session_rename(None, window, cx);
                                        })),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    /// Popover for a project's "more" button: new task, folder management,
    /// per-root reveal, collapse/expand and removal.
    fn render_project_menu(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let target = self.project_menu.target.clone()?;
        let position = self.project_menu.position?;
        let theme = self.theme.clone();
        let name = target.name.clone();
        let initial = name
            .chars()
            .next()
            .map(|character| character.to_uppercase().to_string())
            .unwrap_or_default();
        let color = project_color(&theme, &target.key);
        let key = target.key.clone();
        let collapsed = self.sidebar_section_collapsed(&key);
        let primary = target
            .primary()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        let path_text = primary.clone();
        let path_for_copy = primary;
        let project_in_use = self
            .sessions
            .iter()
            .chain(self.archived_sessions.iter())
            .any(|summary| {
                self.project_group_for_summary(summary)
                    .is_some_and(|group| group.key == key)
            });
        let project_id_for_new = target.id.clone();
        let project_primary_for_new = target.primary().map(Path::to_path_buf);
        let project_id_for_edit = target.id.clone();
        let project_id_for_remove = target.id.clone();
        let project_name = name.clone();

        // One reveal row per folder so multi-root projects open the right one.
        let mut reveal_rows: Vec<AnyElement> = Vec::new();
        for (index, root) in target.roots.iter().enumerate() {
            let label = if target.roots.len() > 1 {
                format!("打开：{}", project_default_name(root))
            } else {
                "在文件管理器中打开".to_string()
            };
            let root = root.clone();
            reveal_rows.push(
                div()
                    .id(SharedString::from(format!("project-menu-reveal-{index}")))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .h(px(30.0))
                    .px(px(7.0))
                    .rounded(px(radius::XS))
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.hover))
                    .child(
                        svg()
                            .path(icons::FOLDER)
                            .size(px(12.0))
                            .text_color(theme.text_muted),
                    )
                    .child(
                        div()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(12.0))
                            .text_color(theme.text)
                            .child(label),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.reveal_project_folder(&root, cx);
                    }))
                    .into_any_element(),
            );
        }

        Some(
            div()
                .id("project-menu-backdrop")
                .absolute()
                .top(px(0.0))
                .left(px(0.0))
                .size_full()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.close_project_menu(cx)),
                )
                .child(
                    anchored()
                        .position(position)
                        .offset(point(px(-174.0), px(6.0)))
                        .snap_to_window_with_margin(px(12.0))
                        .child(
                            div()
                                .id("project-menu")
                                .w(px(214.0))
                                .rounded(px(radius::MD))
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.elevated)
                                .shadow(shadow_float(&theme))
                                .p(px(4.0))
                                .flex()
                                .flex_col()
                                .gap(px(1.0))
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(7.0))
                                        .px(px(6.0))
                                        .py(px(5.0))
                                        .child(
                                            div()
                                                .flex_none()
                                                .size(px(18.0))
                                                .rounded(px(5.0))
                                                .bg(color.opacity(0.16))
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .child(
                                                    div()
                                                        .text_size(px(9.5))
                                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                                        .text_color(color)
                                                        .child(initial),
                                                ),
                                        )
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w(px(0.0))
                                                .flex()
                                                .flex_col()
                                                .gap(px(1.0))
                                                .child(
                                                    div()
                                                        .truncate()
                                                        .text_size(px(12.0))
                                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                                        .text_color(theme.text)
                                                        .child(name),
                                                )
                                                .child(
                                                    div()
                                                        .truncate()
                                                        .text_size(px(9.5))
                                                        .text_color(theme.text_faint)
                                                        .child(if target.roots.len() > 1 {
                                                            format!(
                                                                "{}  ·  {} 个文件夹",
                                                                path_text,
                                                                target.roots.len()
                                                            )
                                                        } else {
                                                            path_text.clone()
                                                        }),
                                                ),
                                        ),
                                )
                                .child(div().h(px(1.0)).bg(theme.border).my(px(2.0)))
                                .child(
                                    project_menu_row(
                                        &theme,
                                        "project-menu-new",
                                        icons::PLUS,
                                        "新建任务",
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, window, cx| {
                                            let project_id = project_id_for_new.clone();
                                            let primary = project_primary_for_new.clone();
                                            this.project_menu = ProjectMenuState::default();
                                            this.new_session_in_project(
                                                project_id, primary, window, cx,
                                            );
                                        },
                                    )),
                                )
                                .when_some(project_id_for_edit, |element, id| {
                                    element.child(
                                        project_menu_row(
                                            &theme,
                                            "project-menu-edit",
                                            icons::EDIT,
                                            "管理文件夹…",
                                        )
                                        .on_click(
                                            cx.listener(move |this, _, window, cx| {
                                                this.open_project_editor(&id, window, cx);
                                            }),
                                        ),
                                    )
                                })
                                .children(reveal_rows)
                                .child(
                                    project_menu_row(
                                        &theme,
                                        "project-menu-copy",
                                        icons::COPY,
                                        "复制主文件夹路径",
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                path_for_copy.clone(),
                                            ));
                                            this.project_menu = ProjectMenuState::default();
                                            this.notice = Some("已复制项目路径。".to_string());
                                            cx.notify();
                                        },
                                    )),
                                )
                                .child(div().h(px(1.0)).bg(theme.border).my(px(2.0)))
                                .child(
                                    project_menu_row(
                                        &theme,
                                        "project-menu-collapse",
                                        if collapsed {
                                            icons::CHEVRON_RIGHT
                                        } else {
                                            icons::CHEVRON_DOWN
                                        },
                                        if collapsed {
                                            "展开项目"
                                        } else {
                                            "折叠项目"
                                        },
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            this.project_menu = ProjectMenuState::default();
                                            this.toggle_sidebar_section(&key, cx);
                                        },
                                    )),
                                )
                                .when(!project_in_use, |element| {
                                    element
                                        .child(div().h(px(1.0)).bg(theme.border).my(px(2.0)))
                                        .child(
                                            project_menu_row(
                                                &theme,
                                                "project-menu-remove",
                                                icons::TRASH,
                                                "从项目列表移除",
                                            )
                                            .on_click(
                                                cx.listener(move |this, _, _, cx| {
                                                    if let Some(id) = &project_id_for_remove {
                                                        if this.projects.remove(id) {
                                                            let _ = this.projects.save();
                                                        }
                                                        let id_key = project_group_key(id);
                                                        this.config
                                                            .sidebar_collapsed_sections
                                                            .retain(|group| group != &id_key);
                                                        let _ = this.config.save();
                                                    }
                                                    this.project_menu = ProjectMenuState::default();
                                                    this.notice = Some(format!(
                                                        "已从项目列表移除：{}",
                                                        project_name
                                                    ));
                                                    cx.notify();
                                                }),
                                            ),
                                        )
                                })
                                .with_animation(
                                    "project-menu-enter",
                                    Animation::new(overlay_motion_duration())
                                        .with_easing(ease_out_cubic),
                                    |element, delta| {
                                        element.opacity(delta).mt(px(4.0 * (1.0 - delta)))
                                    },
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    /// Centered folder editor for one project: rename, add/remove folders and
    /// choose the primary folder that new sessions start in.
    fn render_project_editor(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let editor = self.project_editor.clone()?;
        let theme = self.theme.clone();
        let mut root_rows: Vec<AnyElement> = Vec::new();
        for (index, root) in editor.roots.iter().enumerate() {
            let is_primary = editor.primary.as_deref() == Some(root.as_path());
            let root_for_remove = root.clone();
            let root_for_primary = root.clone();
            root_rows.push(
                div()
                    .id(SharedString::from(format!("project-editor-root-{index}")))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .min_h(px(38.0))
                    .px(px(8.0))
                    .py(px(4.0))
                    .rounded(px(radius::XS))
                    .bg(theme.sidebar)
                    .child(
                        svg()
                            .path(icons::FOLDER)
                            .size(px(13.0))
                            .text_color(theme.text_faint),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .gap(px(1.0))
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(12.0))
                                    .text_color(theme.text)
                                    .child(project_default_name(root)),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(9.5))
                                    .text_color(theme.text_faint)
                                    .child(root.display().to_string()),
                            ),
                    )
                    .when(is_primary, |element| {
                        element.child(
                            div()
                                .flex_none()
                                .rounded(px(999.0))
                                .bg(theme.accent_soft)
                                .px(px(6.0))
                                .py(px(1.0))
                                .text_size(px(9.0))
                                .text_color(theme.accent)
                                .child("主文件夹"),
                        )
                    })
                    .when(!is_primary, |element| {
                        element.child(
                            sidebar_icon_button(
                                &theme,
                                SharedString::from(format!("project-editor-primary-{index}")),
                                icons::PIN,
                                theme.text_faint,
                                "设为主文件夹",
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.project_editor_make_primary(&root_for_primary, cx);
                                },
                            )),
                        )
                    })
                    .child(
                        sidebar_icon_button(
                            &theme,
                            SharedString::from(format!("project-editor-remove-{index}")),
                            icons::X,
                            theme.text_faint,
                            "移除文件夹",
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.project_editor_remove_root(&root_for_remove, cx);
                        })),
                    )
                    .into_any_element(),
            );
        }
        let name_input = self.project_editor_name_view.clone();
        let error_row = editor.error.clone().map(|error| {
            div()
                .text_size(px(11.0))
                .text_color(theme.danger)
                .child(error)
                .into_any_element()
        });
        Some(
            div()
                .id("project-editor-backdrop")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(transparent_black())
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.close_project_editor(cx)),
                )
                .child(
                    div()
                        .id("project-editor")
                        .w(px(460.0))
                        .max_h(px(560.0))
                        .rounded(px(radius::LG))
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.elevated)
                        .shadow(shadow_float(&theme))
                        .p(px(16.0))
                        .flex()
                        .flex_col()
                        .gap(px(12.0))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(
                            div()
                                .text_size(px(14.0))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(theme.text)
                                .child("项目文件夹"),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(4.0))
                                .child(effibuddy_ui::section_label(&theme, "名称"))
                                .child(
                                    div()
                                        .h(px(34.0))
                                        .px(px(8.0))
                                        .rounded(px(radius::SM))
                                        .border_1()
                                        .border_color(theme.border)
                                        .bg(theme.surface)
                                        .flex()
                                        .items_center()
                                        .child(name_input),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(4.0))
                                .child(effibuddy_ui::section_label(&theme, "文件夹"))
                                .child(div().flex().flex_col().gap(px(4.0)).children(root_rows)),
                        )
                        .child(
                            project_menu_row(
                                &theme,
                                "project-editor-add",
                                icons::PLUS,
                                "添加文件夹…",
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.project_editor_add_folders(cx);
                            })),
                        )
                        .children(error_row)
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_end()
                                .gap(px(8.0))
                                .child(
                                    Button::new("project-editor-cancel", "取消", &theme)
                                        .variant(ButtonVariant::Ghost)
                                        .size(ButtonSize::Sm)
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.close_project_editor(cx);
                                        })),
                                )
                                .child(
                                    Button::new("project-editor-save", "保存", &theme)
                                        .variant(ButtonVariant::Primary)
                                        .size(ButtonSize::Sm)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.save_project_editor(window, cx);
                                        })),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    /// 取（或创建）某会话行的跑马灯状态。
    fn sidebar_marquee(&self, id: &str) -> Rc<RefCell<SessionMarquee>> {
        self.sidebar_marquees
            .borrow_mut()
            .entry(id.to_string())
            .or_default()
            .clone()
    }

    /// 指针进出会话行：推进跑马灯状态机并触发重绘。
    fn sidebar_marquee_hover(&mut self, id: &str, entered: bool, cx: &mut Context<Self>) {
        if reduce_motion() {
            return;
        }
        let marquee = self.sidebar_marquee(id);
        if entered {
            marquee.borrow_mut().pointer_enter();
        } else {
            marquee.borrow_mut().pointer_leave();
        }
        cx.notify();
    }

    /// 是否有会话行处于播放/收尾，需要逐帧重绘。
    fn sidebar_marquee_animating(&self) -> bool {
        self.sidebar_marquees
            .borrow()
            .values()
            .any(|marquee| marquee.borrow().is_animating())
    }

    /// 计算一行会话名本帧的跑马灯绘制参数，同时推进状态机时钟。
    fn sidebar_marquee_frame(
        &self,
        window: &Window,
        theme: &Theme,
        summary: &SessionSummary,
        active: bool,
    ) -> SessionMarqueeFrame {
        let mut frame = SessionMarqueeFrame {
            overflow: false,
            offset: 0.0,
            left_fade: 0.0,
        };
        let title_w = measured_line_width(
            window,
            &summary.title,
            theme.ui_font(),
            if active {
                gpui::FontWeight::MEDIUM
            } else {
                gpui::FontWeight::NORMAL
            },
            SESSION_TITLE_SIZE,
        );
        frame.overflow = title_w > session_title_viewport_w();
        if !frame.overflow || reduce_motion() {
            // 不再溢出（或遵循减弱动效）：回静止态，避免帧泵空转。
            self.sidebar_marquee(&summary.id).borrow_mut().reset();
            return frame;
        }
        let distance = title_w + SESSION_MARQUEE_GAP;
        let marquee = self.sidebar_marquee(&summary.id);
        let mut marquee = marquee.borrow_mut();
        marquee.distance.set(distance);
        marquee.advance(distance, Instant::now());
        match marquee.frame(distance, Instant::now()) {
            MarqueeTick::None => {}
            MarqueeTick::Play { offset } => {
                frame.offset = offset;
                frame.left_fade = SESSION_TITLE_FADE_W;
            }
            MarqueeTick::Retract { progress } => {
                frame.left_fade = SESSION_TITLE_FADE_W * (1.0 - ease_out_cubic(progress));
            }
        }
        frame
    }

    fn render_sidebar(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = &self.theme;
        let motion = self.motion.clone();
        let scroll_entity = cx.entity().downgrade();
        let active_id = self.active_id();
        let running = self.active_run.is_some();
        let (workspace_label, workspace_path) = self.sidebar_workspace_label();
        // Partition once; groups are keyed by project id (legacy session paths
        // resolve through the project roots). Projects appear in order of their
        // most recent conversation; registered projects without sessions stay
        // visible after them.
        let mut pinned: Vec<&SessionSummary> = Vec::new();
        let mut projects: Vec<(ProjectGroup, Vec<&SessionSummary>)> = Vec::new();
        let mut tasks: Vec<&SessionSummary> = Vec::new();
        for summary in &self.sessions {
            if summary.pinned {
                pinned.push(summary);
                continue;
            }
            match self.project_group_for_summary(summary) {
                Some(group) => {
                    if let Some((_, members)) = projects
                        .iter_mut()
                        .find(|(candidate, _)| candidate.key == group.key)
                    {
                        members.push(summary);
                    } else {
                        projects.push((group, vec![summary]));
                    }
                }
                None => tasks.push(summary),
            }
        }
        for project in self.projects.projects() {
            let key = project_group_key(&project.id);
            if !projects.iter().any(|(group, _)| group.key == key) {
                projects.push((
                    ProjectGroup {
                        id: Some(project.id.clone()),
                        key,
                        name: project.name.clone(),
                        roots: project.root_paths(),
                    },
                    Vec::new(),
                ));
            }
        }

        let pinned_collapsed = self.sidebar_section_collapsed("pinned");
        let projects_collapsed = self.sidebar_section_collapsed("projects");
        let tasks_collapsed = self.sidebar_section_collapsed("tasks");
        // `--preview-sidebar-hover` exercises both hover affordances without
        // a real pointer: project 1's header and project 2's first row.
        let mut preview_header_pending = self.preview_sidebar_hover;
        let mut preview_row_pending = self.preview_sidebar_hover;
        let add_workspace = sidebar_icon_button(
            theme,
            SharedString::from("project-add-workspace"),
            icons::PLUS,
            theme.text_faint,
            "新建项目（可多选文件夹）",
        )
        .on_click(cx.listener(|this, _, _, cx| {
            cx.stop_propagation();
            this.pick_project_folders(cx);
        }))
        .into_any_element();

        let mut rows: Vec<AnyElement> =
            Vec::with_capacity(self.sessions.len() + projects.len() * 2 + 4);
        if !pinned.is_empty() {
            rows.push(
                motion_item(
                    &motion,
                    SharedString::from("sidebar-section-pinned"),
                    sidebar_section_header(
                        theme,
                        "pinned",
                        "置顶",
                        pinned.len(),
                        pinned_collapsed,
                        None,
                        cx,
                    ),
                )
                .into_any_element(),
            );
            if !pinned_collapsed {
                for (stagger_ix, summary) in pinned.iter().enumerate() {
                    rows.push(motion_session_row(
                        &motion,
                        theme,
                        summary,
                        active_id.as_deref() == Some(summary.id.as_str()),
                        true,
                        false,
                        self.sidebar_marquee_frame(
                            window,
                            theme,
                            summary,
                            active_id.as_deref() == Some(summary.id.as_str()),
                        ),
                        stagger_ix,
                        cx,
                    ));
                }
            }
        }
        rows.push(
            motion_item(
                &motion,
                SharedString::from("sidebar-section-projects"),
                sidebar_section_header(
                    theme,
                    "projects",
                    "全部项目",
                    projects.len(),
                    projects_collapsed,
                    Some(add_workspace),
                    cx,
                ),
            )
            .into_any_element(),
        );
        if !projects_collapsed {
            if projects.is_empty() {
                rows.push(
                    motion_item(
                        &motion,
                        SharedString::from("sidebar-empty-projects"),
                        div()
                            .px(px(9.0))
                            .py(px(5.0))
                            .text_size(px(10.5))
                            .text_color(theme.text_faint)
                            .child("暂无项目，点击右上角 + 新建项目"),
                    )
                    .into_any_element(),
                );
            }
            for (project, group) in &projects {
                let project_key = project.key.clone();
                let collapsed = self.sidebar_section_collapsed(&project_key);
                let header_hover = preview_header_pending;
                preview_header_pending = false;
                rows.push(
                    motion_item(
                        &motion,
                        SharedString::from(format!("sidebar-project-{project_key}")),
                        sidebar_project_header(theme, project, collapsed, header_hover, cx),
                    )
                    .into_any_element(),
                );
                if !collapsed {
                    if group.is_empty() {
                        rows.push(
                            motion_item(
                                &motion,
                                SharedString::from(format!("sidebar-empty-{project_key}")),
                                div()
                                    .px(px(9.0))
                                    .py(px(5.0))
                                    .text_size(px(10.5))
                                    .text_color(theme.text_faint)
                                    .child("暂无任务，悬停项目可新建"),
                            )
                            .into_any_element(),
                        );
                    }
                    for (stagger_ix, summary) in group.iter().enumerate() {
                        let row_hover = preview_row_pending && !header_hover;
                        if row_hover {
                            preview_row_pending = false;
                        }
                        rows.push(motion_session_row(
                            &motion,
                            theme,
                            summary,
                            active_id.as_deref() == Some(summary.id.as_str()),
                            false,
                            row_hover,
                            self.sidebar_marquee_frame(
                                window,
                                theme,
                                summary,
                                active_id.as_deref() == Some(summary.id.as_str()),
                            ),
                            stagger_ix,
                            cx,
                        ));
                    }
                }
            }
        }
        rows.push(
            motion_item(
                &motion,
                SharedString::from("sidebar-section-tasks"),
                sidebar_section_header(
                    theme,
                    "tasks",
                    "任务",
                    tasks.len(),
                    tasks_collapsed,
                    None,
                    cx,
                ),
            )
            .into_any_element(),
        );
        if !tasks_collapsed {
            if tasks.is_empty() {
                rows.push(
                    motion_item(
                        &motion,
                        SharedString::from("sidebar-empty-tasks"),
                        div()
                            .px(px(9.0))
                            .py(px(5.0))
                            .text_size(px(10.5))
                            .text_color(theme.text_faint)
                            .child("暂无任务"),
                    )
                    .into_any_element(),
                );
            } else {
                let mut preview_task_pending = self.preview_sidebar_hover && projects.is_empty();
                for (stagger_ix, summary) in tasks.iter().enumerate() {
                    let row_hover = preview_task_pending;
                    preview_task_pending = false;
                    rows.push(motion_session_row(
                        &motion,
                        theme,
                        summary,
                        active_id.as_deref() == Some(summary.id.as_str()),
                        false,
                        row_hover,
                        self.sidebar_marquee_frame(
                            window,
                            theme,
                            summary,
                            active_id.as_deref() == Some(summary.id.as_str()),
                        ),
                        stagger_ix,
                        cx,
                    ));
                }
            }
        }

        div()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex()
            .flex_none()
            .flex_col()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .px(px(10.0))
                    .pt(px(10.0))
                    .pb(px(6.0))
                    .child(
                        div()
                            .id("new-session-row")
                            .flex_1()
                            .min_w(px(0.0))
                            .h(px(32.0))
                            .px(px(7.0))
                            .rounded(px(radius::SM))
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.surface)
                            .flex()
                            .items_center()
                            .gap(px(7.0))
                            .cursor_pointer()
                            .hover(|style| style.bg(theme.surface_alt))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.new_session(window, cx)),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .size(px(18.0))
                                    .rounded(px(radius::XS))
                                    .bg(theme.accent_soft)
                                    .child(
                                        svg()
                                            .path(icons::PLUS)
                                            .size(px(11.0))
                                            .text_color(theme.text),
                                    ),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .truncate()
                                    .text_size(px(12.5))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child("新建任务"),
                            )
                            .child(kbd(theme, "Ctrl N")),
                    )
                    .child(
                        IconButton::new("sidebar-search", icons::SEARCH, theme)
                            .size(28.0, 14.0)
                            .tooltip("搜索任务与命令 (Ctrl K)")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_palette(window, cx);
                            })),
                    ),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h(px(0.0))
                    .child(
                        div()
                            .id("session-list")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(self.scrolls.sessions.handle())
                            .px(px(10.0))
                            .pb_2()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .children(rows),
                    )
                    // Painted last: its capture listener claims the wheel and
                    // feeds the glide instead of the built-in jump scroll.
                    .when(!self.modal_blocks_scroll(), |element| {
                        element.child(
                            smooth_overlay(self.scrolls.sessions.clone(), scroll_entity.clone())
                                .absolute()
                                .inset_0(),
                        )
                    }),
            )
            .child(divider(theme))
            .child(
                div()
                    .px_4()
                    .py_3()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .id("sidebar-workspace")
                            .flex()
                            .items_center()
                            .gap_2()
                            .h(px(26.0))
                            .text_size(px(11.5))
                            .text_color(theme.text_muted)
                            .tooltip(effibuddy_ui::tooltip(theme, workspace_path))
                            .child(
                                svg()
                                    .path(icons::FOLDER)
                                    .size(px(13.0))
                                    .text_color(theme.text_faint),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .truncate()
                                    .child(workspace_label),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                IconButton::new(
                                    "theme-toggle",
                                    if theme.is_dark() {
                                        icons::SUN
                                    } else {
                                        icons::MOON
                                    },
                                    theme,
                                )
                                .size(28.0, 14.0)
                                .tooltip("切换主题 (Ctrl+Shift+T)")
                                .on_click(cx.listener(|this, _, _, cx| this.toggle_theme(cx))),
                            )
                            .child(
                                Button::new("settings", "设置", theme)
                                    .icon(icons::SETTINGS)
                                    .size(ButtonSize::Sm)
                                    .variant(ButtonVariant::Ghost)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_settings(window, cx)
                                    })),
                            )
                            // Spacer pins the donation entry to the far right,
                            // so the running spinner never pushes it around.
                            .child(div().flex_1())
                            .when(running, |element| {
                                element.child(Spinner::new(theme.accent, 12.0))
                            })
                            .child(
                                IconButton::new("reward", icons::HEART, theme)
                                    .size(28.0, 14.0)
                                    .tooltip("为开发者打赏")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.reward_open = true;
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// Thin one-line update banner above the transcript. Hidden while
    /// checking, on failure, when the user dismissed it this session, or when
    /// the release is not newer / was skipped.
    fn render_update_banner(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let info = self.available_update()?.clone();
        let theme = self.theme.clone();
        let url = info.html_url.clone();
        let banner = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(10.0))
            .h(px(36.0))
            .px(px(32.0))
            .bg(theme.accent_soft)
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .size(px(6.0))
                    .flex_none()
                    .rounded(px(999.0))
                    .bg(theme.accent),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(11.5))
                    .text_color(theme.text_muted)
                    .child(format!("新版本 {} 可用", info.version)),
            )
            .child(
                Button::new("update-open-release", "查看发布页", &theme)
                    .variant(ButtonVariant::Secondary)
                    .size(ButtonSize::Sm)
                    .on_click(cx.listener(move |_this, _, _, cx| {
                        cx.open_url(&url);
                    })),
            )
            .child(
                Button::new("update-skip", "跳过此版本", &theme)
                    .variant(ButtonVariant::Secondary)
                    .size(ButtonSize::Sm)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.skip_update(cx);
                    })),
            )
            .child(
                IconButton::new("update-dismiss", icons::X, &theme).on_click(cx.listener(
                    |this, _, _, cx| {
                        this.update.dismissed = true;
                        cx.notify();
                    },
                )),
            )
            // Rare banner: fade the contents in so the bar does not pop; the
            // height push itself is not interpolated (size never tweens here).
            .with_animation(
                "update-banner-enter",
                Animation::new(overlay_motion_duration()).with_easing(ease_out_cubic),
                |element, delta| element.opacity(delta),
            )
            .into_any_element();
        Some(banner)
    }

    fn render_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme.clone();
        let shared = match self.active_shared() {
            Some(shared) => shared,
            None => {
                return div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(empty_state(
                        &theme,
                        "没有活动会话",
                        "点击左上角 + 新建任务",
                        icons::CHAT,
                    ))
                    .into_any_element();
            }
        };

        // Smooth-scroll glide: apply this frame's step before the list lays
        // out, and keep a frame scheduled while the glide is moving.
        if self.chat.tick_scroll() && !self.chat.glide_frame_scheduled {
            self.chat.glide_frame_scheduled = true;
            cx.on_next_frame(window, |this, _, cx| {
                this.chat.glide_frame_scheduled = false;
                cx.notify();
            });
        }

        // Typewriter reveal: advance once per frame and keep requesting frames
        // while characters are still hidden.
        if self.chat.tick() && !self.chat.reveal_frame_scheduled {
            self.chat.reveal_frame_scheduled = true;
            cx.on_next_frame(window, |this, _, cx| {
                this.chat.reveal_frame_scheduled = false;
                cx.notify();
            });
        }

        // Turn 展开高度渐显：进度在渲染期按时刻计算，需要逐帧重绘（主会话
        // 与侧栏子代理会话共用同一个泵标记）。
        let turn_height_animating = self.chat.turn_height_animating()
            || self
                .side
                .agent_views
                .values()
                .any(|view| view.chat.turn_height_animating());
        if turn_height_animating && !self.chat.turn_height_frame_scheduled {
            self.chat.turn_height_frame_scheduled = true;
            cx.on_next_frame(window, |this, _, cx| {
                this.chat.turn_height_frame_scheduled = false;
                cx.notify();
            });
        }

        // 会话名跑马灯：播放/收尾期间逐帧重绘；顺带清理已不存在会话的行状态。
        self.sidebar_marquees
            .borrow_mut()
            .retain(|id, _| self.sessions.iter().any(|summary| summary.id.as_str() == id));
        if self.sidebar_marquee_animating() && !self.sidebar_marquee_scheduled {
            self.sidebar_marquee_scheduled = true;
            cx.on_next_frame(window, |this, _, cx| {
                this.sidebar_marquee_scheduled = false;
                cx.notify();
            });
        }

        if self.chat_dirty {
            let approval = self.pending_approvals.first();
            let run = RunSnapshot {
                running: self.active_run.is_some(),
                started_at: self.active_run.as_ref().map(|run| run.started_at),
                waiting_approval: approval.is_some(),
                approval_tool: approval.map(|pending| match &pending.agent_id {
                    Some(_) => format!("子代理 · {}", pending.request.tool_name),
                    None => pending.request.tool_name.clone(),
                }),
                last_error: self.last_error.clone(),
                notice: self.notice.clone(),
            };
            {
                let session = lock_session(&shared);
                self.chat.rebuild(&session, &run);
            }
            self.chat_dirty = false;
        }
        self.chat.sync_scroll();

        let (title, message_count, current_todo) = {
            let session = lock_session(&shared);
            (
                session.display_title(),
                session.messages.len(),
                session
                    .todos
                    .iter()
                    .find(|todo| todo.status == TodoStatus::InProgress)
                    .map(|todo| todo.content.clone()),
            )
        };
        let show_empty = message_count == 0 && self.last_error.is_none() && self.notice.is_none();

        let running_agents = self
            .subagent_tasks
            .iter()
            .filter(|task| task.status == SubagentStatus::Running)
            .count();
        let waiting_main = self
            .pending_approvals
            .iter()
            .any(|pending| pending.agent_id.is_none());
        // 标题栏只保留输入框没有的状态（子代理 / 批准 / 当前任务）；上下文
        // 占用与模型选择统一收敛到输入框，避免两处重复展示。
        let capsule = if running_agents > 0 {
            Some((
                SidePaneTab::agents(),
                icons::SPARKLES,
                format!("{running_agents} 个子代理运行中"),
                theme.agent_blue,
            ))
        } else if waiting_main {
            Some((
                SidePaneTab::overview(),
                icons::ALERT,
                "等待你的批准".to_string(),
                theme.warning,
            ))
        } else {
            current_todo.map(|todo| (SidePaneTab::overview(), icons::CLOCK, todo, theme.text_muted))
        };

        let update_banner = self.render_update_banner(cx);
        let header = div()
            .h(px(52.0))
            .flex()
            .flex_none()
            .items_center()
            .justify_between()
            .px(px(32.0))
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.bg)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .min_w(px(0.0))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(15.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child(title),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(11.0))
                            .text_color(theme.text_faint)
                            .child(format!("{message_count} 条消息")),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .when_some(capsule, |element, (target, icon, text, tone)| {
                        element.child(
                            div()
                                .id("status-capsule")
                                .flex()
                                .items_center()
                                .gap(px(6.0))
                                .h(px(26.0))
                                .px(px(9.0))
                                .rounded(px(999.0))
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.surface)
                                .cursor_pointer()
                                .hover(|style| style.bg(theme.surface_alt))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_sidebar_kind(target.clone(), cx);
                                }))
                                .child(svg().path(icon).size(px(12.0)).text_color(tone))
                                .child(
                                    div()
                                        .max_w(px(190.0))
                                        .truncate()
                                        .text_size(px(11.0))
                                        .text_color(theme.text_muted)
                                        .child(text),
                                ),
                        )
                    })
                    .child(
                        IconButton::new("toggle-panel", icons::PANEL_RIGHT, &theme)
                            .size(28.0, 15.0)
                            .tooltip("显示或隐藏右侧面板")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.side.open = !this.side.open;
                                this.persist_side_pane();
                                cx.notify();
                            })),
                    ),
            );

        let body = if show_empty {
            div()
                .id("chat-empty-scroll")
                .flex_1()
                .min_h(px(0.0))
                .overflow_y_scroll()
                .child(self.render_empty_chat(cx))
                .into_any_element()
        } else {
            let agent_cards = self.agent_cards();
            let mut view = self.chat.prepare_frame(
                self.active_run.is_some(),
                true,
                &self.expanded_reasoning,
                &self.expanded_tools,
                &self.expanded_turns,
                &self.collapsed_turns,
                &agent_cards,
            );
            view.jump_flash = self.jump_flash.clone();
            let view = Rc::new(view);
            let state = self.chat.list_state.clone();
            let entity = cx.entity().downgrade();
            let item_theme = theme.clone();
            let rows = list(state, move |index, _window, _cx| {
                render_chat_item(index, &view, &item_theme, &entity)
            });

            let mut container = div()
                .flex_1()
                .min_h(px(0.0))
                .relative()
                .child(rows.size_full());
            // The rail is painted before the wheel interceptor so its own
            // capture-phase wheel claim runs first; clicks still land on the
            // rows because the interceptor has no hitbox.
            if let Some(rail) = self.render_prompt_rail(window, cx) {
                container = container.child(rail);
            }
            // Painted last so its capture-phase wheel listener runs after the
            // nested claim overlays: anything the reasoning bodies and code
            // panels don't claim becomes glide input for the chat list.
            let wheel_entity = cx.entity().downgrade();
            container = container.child(
                crate::chat::chat_wheel_interceptor(wheel_entity)
                    .absolute()
                    .inset_0(),
            );
            if !self.chat.pinned() {
                container = container.child(render_scroll_to_bottom(
                    &theme,
                    self.chat.new_content(),
                    cx.entity().downgrade(),
                ));
            }
            if let Some(scrollbar) = render_scrollbar(
                &theme,
                self.chat.scrollbar_geometry(),
                self.chat.list_state.clone(),
                cx.entity().downgrade(),
            ) {
                container = container.child(scrollbar);
            }
            container.into_any_element()
        };

        let drop_highlight = theme.accent_soft.opacity(0.15);
        div()
            .size_full()
            .flex()
            .flex_col()
            // The whole conversation accepts OS file drops, not only the
            // composer card; the card's own handler wins when the pointer is
            // over it (`stop_propagation`).
            .drag_over::<gpui::ExternalPaths>(move |style, _, _, _| {
                style.bg(drop_highlight)
            })
            .on_drop(cx.listener(
                |this, paths: &gpui::ExternalPaths, window, cx| {
                    this.attach_paths(paths.paths().to_vec(), cx);
                    this.composer_focus_handle(cx).focus(window);
                },
            ))
            .child(header)
            .children(update_banner)
            .child(body)
            .child(self.render_composer(window, cx))
            .into_any_element()
    }

    /// Build the left-edge query rail for this frame, or `None` when the
    /// transcript has no queries / the viewport is not measured yet.
    fn render_prompt_rail(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let marks = self.chat.prompt_marks();
        let Some(metrics) = self.chat.prompt_rail_metrics() else {
            // The list viewport is measured during layout, so the first frame
            // of a session (and the first after a resize) has no bounds yet.
            // Ask for exactly one more frame instead of waiting for unrelated
            // input to repaint the rail into existence.
            if !marks.is_empty() && !self.prompt_rail_frame_scheduled {
                let bounds = self.chat.list_state.viewport_bounds();
                if bounds.size.height <= px(0.0) || bounds.size.width <= px(0.0) {
                    self.prompt_rail_frame_scheduled = true;
                    cx.on_next_frame(window, |this, _, cx| {
                        this.prompt_rail_frame_scheduled = false;
                        cx.notify();
                    });
                }
            }
            return None;
        };
        let active = self.chat.active_prompt_index().unwrap_or(0);
        if self.prompt_rail.follow(active, &metrics) {
            // The rail moved under a possibly stationary pointer; its old row
            // no longer sits under the cursor, so drop the hover/card instead
            // of leaving them attached to a bar that scrolled away.
            self.prompt_rail.previous_hover = None;
            self.prompt_rail.hover = None;
            self.prompt_rail.card = None;
            self.prompt_rail.hover_seq = self.prompt_rail.hover_seq.wrapping_add(1);
        }
        let entity = cx.entity().downgrade();
        Some(render_prompt_rail_element(
            &self.theme,
            &marks,
            &self.prompt_rail,
            &metrics,
            active,
            &entity,
        ))
    }

    /// Hovering a rail row widens its bar immediately; the preview card waits
    /// for a short dwell so sweeping down the rail does not flicker cards.
    pub(crate) fn prompt_rail_hover(&mut self, index: Option<usize>, cx: &mut Context<Self>) {
        if self.prompt_rail.hover == index {
            return;
        }
        self.prompt_rail.previous_hover = self.prompt_rail.hover;
        self.prompt_rail.hover = index;
        self.prompt_rail.hover_seq = self.prompt_rail.hover_seq.wrapping_add(1);
        let Some(index) = index else {
            self.prompt_rail.card = None;
            cx.notify();
            return;
        };
        let seq = self.prompt_rail.hover_seq;
        cx.spawn(async move |weak, cx| {
            cx.background_executor().timer(PROMPT_RAIL_CARD_DELAY).await;
            let _ = weak.update(cx, |this, cx| {
                if this.prompt_rail.hover == Some(index) && this.prompt_rail.hover_seq == seq {
                    this.prompt_rail.card = Some(index);
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// Jump to a query. `ChatList::jump_to_prompt` already resolved the slot,
    /// so this is a single `scroll_to`; the flash ring confirms the landing.
    pub(crate) fn prompt_rail_click(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(message_id) = self.chat.jump_to_prompt(index) else {
            return;
        };
        self.jump_flash_seq = self.jump_flash_seq.wrapping_add(1);
        let seq = self.jump_flash_seq;
        self.jump_flash = Some(JumpFlash {
            message_id,
            seq,
        });
        // The ring fades out on its own; drop the state one beat later so a
        // reduced-motion static ring (and the removed element) cannot linger.
        cx.spawn(async move |weak, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1200))
                .await;
            let _ = weak.update(cx, |this, cx| {
                if this
                    .jump_flash
                    .as_ref()
                    .is_some_and(|flash| flash.seq == seq)
                {
                    this.jump_flash = None;
                    cx.notify();
                }
            });
        })
        .detach();
        self.prompt_rail.card = None;
        self.prompt_rail.hover_seq = self.prompt_rail.hover_seq.wrapping_add(1);
        cx.notify();
    }

    /// Wheel over the rail scrolls the rail itself (claiming the event before
    /// the chat interceptor) and drops the hover card, like ZCode's navigator.
    pub(crate) fn prompt_rail_wheel(&mut self, pixel_y: f32, cx: &mut Context<Self>) -> bool {
        let Some(metrics) = self.chat.prompt_rail_metrics() else {
            return false;
        };
        if metrics.max_scroll <= 0.0 {
            return false;
        }
        let next = (self.prompt_rail.scroll - pixel_y).clamp(0.0, metrics.max_scroll);
        if (next - self.prompt_rail.scroll).abs() < 0.01 {
            return false;
        }
        self.prompt_rail.scroll = next;
        self.prompt_rail.previous_hover = self.prompt_rail.hover;
        self.prompt_rail.hover = None;
        self.prompt_rail.hover_seq = self.prompt_rail.hover_seq.wrapping_add(1);
        self.prompt_rail.card = None;
        cx.notify();
        true
    }

    fn render_empty_chat(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = &self.theme;
        let prompts = [
            (
                "浏览项目结构",
                "先看目录，再总结主要模块",
                "帮我看看这个项目的目录结构，并总结主要模块。",
                icons::FOLDER,
            ),
            (
                "制定实现计划",
                "阅读仓库，给出可执行步骤",
                "请阅读这个仓库并给我一份可以执行的实现计划。",
                icons::CLOCK,
            ),
            (
                "解释一个模块",
                "从入口开始讲清 agent loop",
                "解释一下 crates/agent-core 里的 agent loop 是怎么工作的。",
                icons::CODE,
            ),
        ];
        div()
            .w_full()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(22.0))
            .py(px(64.0))
            .px(px(24.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(48.0))
                    .rounded(px(radius::LG))
                    .bg(theme.accent_soft)
                    .child(
                        svg()
                            .path(icons::SPARKLES)
                            .size(px(22.0))
                            .text_color(theme.text),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .text_size(px(22.0))
                            .line_height(px(30.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child(format!("{}，想做点什么？", day_greeting())),
                    )
                    .child(
                        div()
                            .max_w(px(480.0))
                            .text_center()
                            .text_size(px(13.0))
                            .line_height(px(21.0))
                            .text_color(theme.text_muted)
                            .child("EffiBuddy 会先读代码、再动手。它可以读写文件、运行命令、用 JavaScript 编排工具（PTC），也可以派子代理并行探索或审查。"),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .max_w(px(640.0))
                    .flex()
                    .flex_wrap()
                    .justify_center()
                    .gap(px(8.0))
                    .children(prompts.iter().map(|(label, description, prompt, icon)| {
                        let prompt = prompt.to_string();
                        div()
                            .id(SharedString::from(format!("prompt-{label}")))
                            .w(px(196.0))
                            .flex_none()
                            .flex()
                            .flex_col()
                            .gap(px(6.0))
                            .px(px(14.0))
                            .py(px(12.0))
                            .rounded(px(radius::MD))
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.surface)
                            .cursor_pointer()
                            .hover(|style| {
                                style.bg(theme.surface_alt).border_color(theme.border_strong)
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.fill_composer(&prompt, window, cx);
                            }))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        svg()
                                            .path(*icon)
                                            .size(px(14.0))
                                            .text_color(theme.text_faint),
                                    )
                                    .child(
                                        svg()
                                            .path(icons::CHEVRON_RIGHT)
                                            .size(px(12.0))
                                            .text_color(theme.text_faint)
                                            .opacity(0.55),
                                    ),
                            )
                            .child(
                                div()
                                    .text_size(px(12.5))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child(*label),
                            )
                            .child(
                                div()
                                    .text_size(px(11.5))
                                    .line_height(px(17.0))
                                    .text_color(theme.text_faint)
                                    .child(*description),
                            )
                    })),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .mt_1()
                    .child(feature_chip(theme, icons::WRENCH, "9 个内置工具"))
                    .child(feature_chip(theme, icons::SPARKLES, "子代理 · explore / reviewer"))
                    .child(feature_chip(theme, icons::CODE, "PTC · Boa")),
            )
            .into_any_element()
    }

    fn render_composer(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = &self.theme;
        let running = self.active_run.is_some();
        let empty = self.composer_state.read(cx).content().trim().is_empty()
            && !self.composer_has_attachments();
        let focused = self
            .composer_state
            .read(cx)
            .focus_handle()
            .is_focused(window);

        let resolved = self.active_resolved_model();
        let demo_mode = self.models.demo_mode(self.config.use_demo, &resolved);
        let context_window = if demo_mode {
            64_000
        } else {
            resolved.context_window()
        };
        let context_usage = self
            .active_shared()
            .map(|shared| crate::context_meter::collect(&shared, context_window))
            .unwrap_or_else(|| crate::context_meter::ContextUsage::empty(context_window));
        let context_pct = context_usage.percent();

        let permission_mode = self.permission_mode();
        let (permission_label, permission_color, permission_bg, permission_icon) =
            match permission_mode {
                PermissionMode::Default => {
                    ("默认询问", theme.warning, theme.warning_soft, icons::WRENCH)
                }
                PermissionMode::AcceptEdits => {
                    ("自动编辑", theme.accent, theme.accent_soft, icons::EDIT)
                }
                PermissionMode::Plan => {
                    ("计划模式", theme.warning, theme.warning_soft, icons::SEARCH)
                }
                PermissionMode::FullAccess => {
                    ("完全访问", theme.success, theme.success_soft, icons::CHECK)
                }
            };

        let send = if running {
            div()
                .id("composer-stop")
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .size(px(34.0))
                .rounded(px(999.0))
                .bg(theme.danger_soft)
                .cursor_pointer()
                .tooltip(effibuddy_ui::tooltip(theme, "停止当前任务 (Esc)"))
                .hover(|style| style.bg(theme.active))
                .on_click(cx.listener(|this, _, _, cx| this.interrupt(cx)))
                .child(
                    svg()
                        .path(icons::STOP)
                        .size(px(12.0))
                        .text_color(theme.danger),
                )
        } else if empty {
            div()
                .id("composer-send-disabled")
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .size(px(34.0))
                .rounded(px(999.0))
                .bg(theme.disabled_bg)
                .tooltip(effibuddy_ui::tooltip(theme, "输入消息后发送"))
                .child(
                    svg()
                        .path(icons::SEND)
                        .size(px(14.0))
                        .text_color(theme.disabled_fg),
                )
        } else {
            div()
                .id("composer-send")
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .size(px(34.0))
                .rounded(px(999.0))
                .bg(theme.accent)
                .cursor_pointer()
                .tooltip(effibuddy_ui::tooltip(theme, "发送 (Enter)"))
                .hover(|style| style.bg(theme.accent_hover))
                .on_click(cx.listener(|this, _, window, cx| {
                    let text = this.composer_state.read(cx).content().to_string();
                    this.composer_state
                        .update(cx, |state, cx| state.clear(window, cx));
                    this.send_message(text, false, window, cx);
                }))
                .child(
                    svg()
                        .path(icons::SEND)
                        .size(px(14.0))
                        .text_color(theme.on_accent),
                )
        };

        let drop_theme = theme.clone();
        let mut composer_card =
            div()
                .w_full()
                .max_w(px(760.0))
                .rounded(px(radius::XL))
                .border_1()
                .border_color(if focused {
                    theme.focus_ring
                } else {
                    theme.border
                })
                .bg(theme.surface)
                .shadow(shadow_float(theme))
                // Dropping OS files anywhere on the card stages them for the
                // next message. `drag_over` is re-evaluated every frame from
                // the active drag, so the highlight cannot stick.
                .drag_over::<gpui::ExternalPaths>(move |style, _, _, _| {
                    style.bg(drop_theme.accent_soft).border_color(drop_theme.accent)
                })
                .on_drop(cx.listener(
                    |this, paths: &gpui::ExternalPaths, window, cx| {
                        this.attach_paths(paths.paths().to_vec(), cx);
                        this.composer_focus_handle(cx).focus(window);
                    },
                ))
                .pt(px(14.0))
                .px(px(16.0))
                .pb(px(12.0))
                .flex()
                .flex_col()
                .gap(px(12.0))
                .on_mouse_down(MouseButton::Left, {
                    let state = self.composer_state.clone();
                    move |_, window, cx| {
                        let _ = state.update(cx, |state, _| {
                            window.focus(&state.focus_handle());
                        });
                    }
                });
        if let Some(chips) = self.render_composer_attachments(cx) {
            composer_card = composer_card.child(chips);
        }
        let composer_card = composer_card
                .child(self.composer.clone())
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .h(px(34.0))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.0))
                                .child(
                                    IconButton::new("composer-add", icons::PLUS, theme)
                                        .size(26.0, 13.0)
                                        .tooltip("添加文件到会话（也可直接拖入）")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.pick_composer_files(cx);
                                        })),
                                )
                                .child(
                                    div()
                                        .id("composer-permission")
                                        .flex()
                                        .items_center()
                                        .gap(px(5.0))
                                        .h(px(26.0))
                                        .px(px(9.0))
                                        .rounded(px(999.0))
                                        .bg(permission_bg)
                                        .text_color(permission_color)
                                        .text_size(px(11.0))
                                        .cursor_pointer()
                                        .tooltip(effibuddy_ui::tooltip(
                                            theme,
                                            "权限模式 · Shift+Tab 切换",
                                        ))
                                        .hover(|style| style.bg(theme.active))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.cycle_permission_mode(cx)
                                        }))
                                        .child(
                                            svg()
                                                .path(permission_icon)
                                                .size(px(11.0))
                                                .text_color(permission_color),
                                        )
                                        .child(permission_label.to_string())
                                        .child(
                                            svg()
                                                .path(icons::CHEVRON_DOWN)
                                                .size(px(9.0))
                                                .text_color(permission_color),
                                        ),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.0))
                                .child(
                                    div()
                                        .id("composer-context")
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .size(px(26.0))
                                        .rounded(px(999.0))
                                        .bg(theme.surface_alt)
                                        .cursor_pointer()
                                        .tooltip(crate::context_meter::context_tooltip(
                                            theme,
                                            context_usage.clone(),
                                        ))
                                        .hover(|style| style.bg(theme.active))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.open_sidebar_kind(SidePaneTab::overview(), cx);
                                        }))
                                        // 静止时只给进度环不给百分比；占用构成与累计
                                        // 消耗都在悬停 tips 里展开。
                                        .child(
                                            ProgressRing::new(context_usage.fraction(), 16.0)
                                                .stroke_width(2.2)
                                                .track(theme.border_strong)
                                                .progress(if context_pct > 75.0 {
                                                    theme.warning
                                                } else {
                                                    theme.success
                                                }),
                                        ),
                                )
                                .child(
                                    div()
                                        .id("composer-model")
                                        .flex()
                                        .items_center()
                                        .gap(px(6.0))
                                        .h(px(26.0))
                                        .px(px(9.0))
                                        .rounded(px(999.0))
                                        .bg(theme.surface_alt)
                                        .text_size(px(11.0))
                                        .text_color(theme.text_muted)
                                        .cursor_pointer()
                                        .tooltip(effibuddy_ui::tooltip(theme, "切换模型"))
                                        .hover(|style| style.bg(theme.active))
                                        .on_click(cx.listener(
                                            |this, event: &gpui::ClickEvent, _, cx| {
                                                this.open_model_menu(event.position(), cx);
                                            },
                                        ))
                                        .child(div().size(px(6.0)).rounded(px(999.0)).bg(
                                            if demo_mode {
                                                theme.warning
                                            } else if resolved.live {
                                                theme.success
                                            } else {
                                                theme.warning
                                            },
                                        ))
                                        .child(resolved.model_label().to_string())
                                        .child(
                                            svg()
                                                .path(icons::CHEVRON_DOWN)
                                                .size(px(10.0))
                                                .text_color(theme.text_faint),
                                        ),
                                )
                                .child(send),
                        ),
                );

        let mut column = div()
            .w_full()
            .flex()
            .flex_col()
            .items_center()
            .px(px(32.0))
            .pt(px(8.0))
            .pb(px(20.0))
            .gap(px(10.0));

        let computer_mode = self.config.computer_use_mode;
        let computer_granted = self
            .active_shared()
            .map(|shared| {
                let session = lock_session(&shared);
                self.computer.is_granted(&session.id)
            })
            .unwrap_or(false);
        let computer_vision = resolved.model.capabilities.vision;
        let computer_hint = composer_hint(computer_mode, computer_granted, computer_vision);

        let mut footer_left = div().flex().items_center().gap(px(8.0)).child(
            if permission_mode == PermissionMode::Plan {
                "计划模式：写操作将被拒绝，先出计划（Shift+Tab 切换）"
            } else if self.active_run.is_some() {
                "任务运行中：Enter 排队 · Ctrl+Enter 插入当前回合 · Shift+Enter 换行"
            } else {
                "Enter 发送 · Shift+Enter 换行"
            },
        );
        if let Some((label, tooltip_text)) = computer_hint {
            let (hint_color, hint_bg) = if !computer_vision {
                (theme.warning, theme.warning_soft)
            } else if computer_granted || computer_mode == ComputerUseMode::Always {
                (theme.success, theme.success_soft)
            } else {
                (theme.text_muted, theme.surface_alt)
            };
            footer_left = footer_left
                .child(div().w(px(1.0)).h(px(10.0)).bg(theme.border))
                .child(
                    div()
                        .id("composer-computer")
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .h(px(20.0))
                        .px(px(6.0))
                        .rounded(px(999.0))
                        .bg(hint_bg)
                        .text_size(px(10.0))
                        .text_color(hint_color)
                        .cursor_pointer()
                        .tooltip(effibuddy_ui::tooltip(theme, tooltip_text))
                        .hover(|style| style.bg(theme.active))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_settings_section(SettingsSection::General, window, cx)
                        }))
                        .child(
                            svg()
                                .path(icons::MOUSE)
                                .size(px(10.0))
                                .text_color(hint_color),
                        )
                        .child(label),
                );
        }

        if let Some(pending) = self.pending_approvals.first() {
            column = column.child(self.render_approval(
                &pending.request,
                pending.agent_id.as_deref(),
                cx,
            ));
        }

        if let Some(edit) = &self.edit_message {
            column = column.child(self.render_edit_banner(edit, cx));
        }

        if !self.pending_inputs.is_empty() {
            column = column.child(self.render_queue_bar(cx));
        }

        if self.preview_context_tip {
            // 预览：把 hover tips 常驻渲染在输入框上方（对齐 context 环一侧），
            // Wayland 截图验收用；真实交互仍走 tooltip 闭包。
            column = column.child(
                div()
                    .w_full()
                    .max_w(px(760.0))
                    .flex()
                    .justify_end()
                    .child(cx.new(|_cx| {
                        crate::context_meter::ContextUsageTip::new(
                            theme.clone(),
                            context_usage.clone(),
                        )
                    })),
            );
        }

        if let Some(menu) = self.render_composer_menu(window, cx) {
            column = column.child(menu);
        }

        column
            .child(composer_card)
            .child(
                div()
                    .w_full()
                    .max_w(px(760.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(2.0))
                    .text_size(px(11.0))
                    .text_color(if permission_mode == PermissionMode::Plan {
                        theme.warning
                    } else {
                        theme.text_faint
                    })
                    .child(footer_left)
                    .child("Ctrl K 命令 · / 技能·工具 · @ 文件"),
            )
            .into_any_element()
    }

    /// Composer banner shown while a user message is being edited. It states
    /// how much conversation will be removed and whether files are restored.
    /// Pending-message bar above the composer: queue items (wait for the
    /// current turn, then start a new one) and steers (waiting for the next
    /// step boundary). Queue rows can be sent immediately — while running
    /// that promotes them ahead of the rest of the queue, when idle it starts
    /// the run, which is the manual disposition for a held queue.
    fn render_queue_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = &self.theme;
        let running = self.active_run.is_some();
        let mut rows = div().w_full().flex().flex_col().gap(px(4.0));
        for input in &self.pending_inputs {
            let (chip_label, chip_color, chip_bg) = match input.kind {
                PendingInputKind::Queue => ("排队", theme.text_muted, theme.surface_alt),
                PendingInputKind::Steer => ("待注入", theme.accent, theme.accent_soft),
            };
            let mut row = div()
                .w_full()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .h(px(20.0))
                        .px(px(7.0))
                        .rounded(px(999.0))
                        .bg(chip_bg)
                        .text_size(px(10.0))
                        .text_color(chip_color)
                        .child(
                            svg()
                                .path(icons::CLOCK)
                                .size(px(10.0))
                                .text_color(chip_color),
                        )
                        .child(chip_label),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(px(12.0))
                        .text_color(theme.text_muted)
                        .truncate()
                        .child(input.text.replace('\n', " ")),
                );
            if input.kind == PendingInputKind::Queue {
                let send_id = SharedString::from(format!("queue-send-{}", input.id));
                let send_entity = cx.entity().downgrade();
                let send_target = input.id.clone();
                row = row.child(
                    IconButton::new(send_id, icons::SEND, theme)
                        .size(24.0, 14.0)
                        .tooltip(if running {
                            "跳过队列，插入当前回合"
                        } else {
                            "立即发送"
                        })
                        .on_click(move |_, _, cx| {
                            let _ = send_entity.update(cx, |this, cx| {
                                this.send_pending_input_now(&send_target, cx);
                            });
                        }),
                );
            }
            let remove_id = SharedString::from(format!("queue-remove-{}", input.id));
            let remove_entity = cx.entity().downgrade();
            let remove_target = input.id.clone();
            row = row.child(
                IconButton::new(remove_id, icons::TRASH, theme)
                    .size(24.0, 14.0)
                    .tooltip("移除")
                    .on_click(move |_, _, cx| {
                        let _ = remove_entity.update(cx, |this, cx| {
                            this.remove_pending_input(&remove_target, cx);
                        });
                    }),
            );
            rows = rows.child(row);
        }

        div()
            .w_full()
            .max_w(px(760.0))
            .rounded(px(radius::LG))
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface_alt)
            .px(px(12.0))
            .py(px(8.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .text_size(px(10.0))
                    .text_color(theme.text_faint)
                    .child(if running {
                        "消息队列 · 当前回合结束后按顺序发送"
                    } else if self.queue_held {
                        "消息队列 · 已暂停，点击发送图标立即执行"
                    } else {
                        "消息队列 · 空闲中，点击发送图标立即执行"
                    }),
            )
            .child(rows)
            .into_any_element()
    }

    fn render_edit_banner(&self, edit: &EditMessageState, cx: &mut Context<Self>) -> AnyElement {
        let theme = &self.theme;
        let plan = &edit.plan;
        let file_count = plan.files.len();
        let conflict_count = plan.conflicts.len();
        let has_changes = plan.has_file_changes();
        let toggle_label = if !has_changes {
            "本轮没有可还原的文件改动".to_string()
        } else if !edit.include_files {
            "不还原文件".to_string()
        } else if conflict_count > 0 {
            if file_count == 0 {
                format!("{conflict_count} 个文件无法安全还原")
            } else {
                format!("还原 {file_count} 个文件（{conflict_count} 个无法还原）")
            }
        } else {
            format!("发送时还原 {file_count} 个文件")
        };
        let toggle_color = if conflict_count > 0 {
            theme.warning
        } else if edit.include_files {
            theme.accent
        } else {
            theme.text_faint
        };

        let mut banner = div()
            .w_full()
            .max_w(px(760.0))
            .rounded(px(radius::LG))
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface_alt)
            .px(px(12.0))
            .py(px(9.0))
            .flex()
            .flex_col()
            .gap(px(5.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        svg()
                            .path(icons::EDIT)
                            .size(px(13.0))
                            .text_color(theme.accent),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .gap(px(1.0))
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme.text)
                                    .child("正在编辑用户消息"),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(10.5))
                                    .text_color(theme.text_faint)
                                    .child(format!(
                                        "发送后将重置到这条消息之前，共移除 {} 条消息（{} 轮对话）",
                                        plan.removed_messages, plan.removed_user_turns
                                    )),
                            ),
                    )
                    .when(has_changes, |element| {
                        element.child(
                            div()
                                .id("edit-file-toggle")
                                .flex()
                                .flex_none()
                                .items_center()
                                .gap(px(4.0))
                                .h(px(22.0))
                                .px(px(8.0))
                                .rounded(px(999.0))
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.surface)
                                .text_size(px(10.5))
                                .text_color(toggle_color)
                                .cursor_pointer()
                                .tooltip(effibuddy_ui::tooltip(
                                    theme,
                                    "发送时是否把本轮文件改动还原到编辑前",
                                ))
                                .hover(|style| style.bg(theme.hover))
                                .on_click(cx.listener(|this, _, _, cx| this.toggle_edit_files(cx)))
                                .child(
                                    svg()
                                        .path(if conflict_count > 0 {
                                            icons::ALERT
                                        } else if edit.include_files {
                                            icons::CHECK
                                        } else {
                                            icons::FILE
                                        })
                                        .size(px(11.0))
                                        .text_color(toggle_color),
                                )
                                .child(toggle_label.clone()),
                        )
                    })
                    .child(
                        Button::new("edit-message-cancel", "取消", theme)
                            .size(ButtonSize::Sm)
                            .variant(ButtonVariant::Ghost)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.cancel_edit_message(window, cx)
                            })),
                    ),
            );
        if conflict_count > 0 {
            banner = banner.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .pl(px(21.0))
                    .text_size(px(10.5))
                    .text_color(theme.warning)
                    .child(
                        svg()
                            .path(icons::ALERT)
                            .size(px(11.0))
                            .text_color(theme.warning),
                    )
                    .child(format!(
                        "{conflict_count} 个文件无法安全还原，发送前会再次确认；也可以关闭“还原文件”。"
                    )),
            );
        }
        if !plan.shell_commands.is_empty() {
            banner = banner.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .pl(px(21.0))
                    .text_size(px(10.5))
                    .text_color(theme.warning)
                    .child(
                        svg()
                            .path(icons::TERMINAL)
                            .size(px(11.0))
                            .text_color(theme.warning),
                    )
                    .child("本轮运行过 shell 命令，其文件改动无法追踪，也不会被还原。"),
            );
        }
        banner.into_any_element()
    }

    pub(crate) fn render_approval(
        &self,
        request: &ApprovalRequest,
        agent_id: Option<&str>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = &self.theme;
        let origin = agent_id.and_then(|agent_id| {
            self.subagent_tasks
                .iter()
                .find(|task| task.agent_id == agent_id)
                .map(|task| (task.profile.clone(), task.color))
        });
        let mut warning_border = theme.warning;
        warning_border.a = if theme.is_dark() { 0.40 } else { 0.30 };
        let mut warning_tile = theme.warning;
        warning_tile.a = if theme.is_dark() { 0.18 } else { 0.12 };
        div()
            .w_full()
            .max_w(px(760.0))
            .rounded(px(radius::LG))
            .border_1()
            .border_color(warning_border)
            .bg(theme.warning_soft)
            .p(px(14.0))
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(26.0))
                            .rounded(px(radius::SM))
                            .bg(warning_tile)
                            .child(
                                svg()
                                    .path(icons::ALERT)
                                    .size(px(14.0))
                                    .text_color(theme.warning),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(13.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child(if origin.is_some() {
                                "子代理需要你的批准"
                            } else {
                                "需要你的批准"
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .h(px(22.0))
                            .px(px(7.0))
                            .rounded(px(radius::XS))
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.surface)
                            .font_family(theme.mono_font())
                            .text_size(px(10.5))
                            .text_color(theme.text_muted)
                            .child(request.tool_name.clone()),
                    )
                    .when_some(origin.clone(), |element, (profile, color)| {
                        let color = agent_color(theme, color);
                        let mut soft = color;
                        soft.a = if theme.is_dark() { 0.22 } else { 0.12 };
                        element.child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(4.0))
                                .h(px(22.0))
                                .px(px(8.0))
                                .rounded(px(999.0))
                                .bg(soft)
                                .text_size(px(11.0))
                                .text_color(color)
                                .child(svg().path(icons::SPARKLES).size(px(10.0)).text_color(color))
                                .child(format!("子代理 · {profile}")),
                        )
                    }),
            )
            .child(
                div()
                    .text_size(px(12.5))
                    .line_height(px(19.0))
                    .text_color(theme.text_muted)
                    .child(request.summary.clone()),
            )
            .when_some(
                // Subagent harnesses run on the bare broker gate: the main
                // session's permission mode neither auto-approves nor
                // auto-denies their requests, so the mode hint would contradict
                // the card's actual behavior.
                if origin.is_some() {
                    Some("子代理请求不受主会话权限模式影响：不会被自动批准或拒绝，仍需你确认。")
                } else {
                    permission_mode_hint(self.permission_mode())
                },
                |element, hint| {
                    element.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(5.0))
                            .text_size(px(11.0))
                            .text_color(theme.text_faint)
                            .child(
                                svg()
                                    .path(icons::SPARKLES)
                                    .size(px(11.0))
                                    .text_color(theme.text_faint),
                            )
                            .child(hint.to_string()),
                    )
                },
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("deny-tool", "拒绝", theme)
                            .size(ButtonSize::Sm)
                            .variant(ButtonVariant::Ghost)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.respond_approval(ApprovalDecision::Deny, cx)
                            })),
                    )
                    .child(
                        Button::new("approve-once", "允许本次", theme)
                            .size(ButtonSize::Sm)
                            .variant(ButtonVariant::Secondary)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.respond_approval(ApprovalDecision::Approve, cx)
                            })),
                    )
                    .child(
                        Button::new("approve-session", "本会话允许", theme)
                            .size(ButtonSize::Sm)
                            .variant(ButtonVariant::Primary)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.respond_approval(ApprovalDecision::ApproveForSession, cx)
                            })),
                    ),
            )
            .with_animation(
                SharedString::from(format!("approval-enter-{}", request.id)),
                Animation::new(overlay_motion_duration())
                    .with_easing(|delta| 1.0 - (1.0 - delta).powi(3)),
                |element, delta| element.opacity(delta),
            )
            .into_any_element()
    }

    /// Secondary confirmation for message undo, and the conflict dialog for
    /// edited turns whose files cannot be restored safely. Both list every
    /// affected path and state explicitly that EffiBuddy cannot recover the
    /// change afterwards (no Git snapshots are involved).
    fn render_restore_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = &self.theme;
        let (title, subtitle, excerpt, plan, undo) = if let Some(dialog) = &self.undo_dialog {
            (
                "撤销这条用户消息？",
                format!(
                    "将移除从这条消息开始的 {} 条消息（{} 轮对话）。",
                    dialog.plan.removed_messages, dialog.plan.removed_user_turns
                ),
                Some(dialog.excerpt.clone()),
                &dialog.plan,
                true,
            )
        } else if let Some(conflict) = &self.edit_conflict {
            (
                "文件无法安全重置",
                "对话尚未裁剪。请检查下列冲突文件，然后仅重置对话并发送，或取消。".to_string(),
                None,
                &conflict.plan,
                false,
            )
        } else {
            return div().into_any_element();
        };

        let file_count = plan.files.len();
        let can_restore = plan.can_restore();
        let has_files = plan.has_file_changes();
        let backdrop_entity = cx.entity().downgrade();
        let card = div()
            .id("restore-dialog")
            .w(px(620.0))
            .max_h(px(600.0))
            .rounded(px(radius::XL))
            .border_1()
            .border_color(theme.border)
            .bg(theme.elevated)
            .shadow(shadow_float(theme))
            .p(px(20.0))
            .flex()
            .flex_col()
            .gap(px(14.0))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap(px(10.0))
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(30.0))
                            .rounded(px(radius::MD))
                            .bg(if undo {
                                theme.danger_soft
                            } else {
                                theme.warning_soft
                            })
                            .child(
                                svg()
                                    .path(if undo { icons::UNDO } else { icons::ALERT })
                                    .size(px(15.0))
                                    .text_color(if undo { theme.danger } else { theme.warning }),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .gap(px(3.0))
                            .child(
                                div()
                                    .text_size(px(15.0))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme.text)
                                    .child(title),
                            )
                            .child(
                                div()
                                    .text_size(px(11.5))
                                    .line_height(px(17.0))
                                    .text_color(theme.text_muted)
                                    .child(subtitle),
                            ),
                    ),
            )
            .when_some(excerpt, |element, excerpt| {
                element.child(
                    div()
                        .rounded(px(radius::MD))
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.surface_alt)
                        .px(px(11.0))
                        .py(px(8.0))
                        .text_size(px(11.5))
                        .line_height(px(17.0))
                        .text_color(theme.text_muted)
                        .child(excerpt),
                )
            })
            .child(
                div()
                    .id("restore-dialog-scroll")
                    .max_h(px(220.0))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .child(render_restore_report(theme, plan)),
            )
            .when(has_files && !can_restore, |element| {
                element.child(
                    div()
                        .px(px(2.0))
                        .text_size(px(11.0))
                        .line_height(px(16.0))
                        .text_color(theme.warning)
                        .child(
                            "存在无法安全还原的文件，因此本次不会写入任何文件；\
                             冲突文件会原样保留。",
                        ),
                )
            })
            .child(
                div()
                    .rounded(px(radius::MD))
                    .border_1()
                    .border_color(theme.danger)
                    .bg(theme.danger_soft)
                    .px(px(11.0))
                    .py(px(9.0))
                    .flex()
                    .items_start()
                    .gap(px(7.0))
                    .child(
                        svg()
                            .path(icons::ALERT)
                            .size(px(13.0))
                            .flex_none()
                            .text_color(theme.danger),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .line_height(px(16.0))
                            .text_color(theme.danger)
                            .child(
                                "此操作不可撤销：这条消息及其后的对话会被删除，工作区文件会被写回旧内容或删除。\
                                 EffiBuddy 不会使用 Git，也无法从版本库找回这些内容。",
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("restore-dialog-cancel", "取消", theme)
                            .size(ButtonSize::Sm)
                            .variant(ButtonVariant::Ghost)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if this.edit_conflict.is_some() {
                                    this.dismiss_edit_conflict(window, cx);
                                } else {
                                    this.close_undo_dialog(window, cx);
                                }
                            })),
                    )
                    .when(undo && has_files, |element| {
                        element.child(
                            Button::new("restore-dialog-conversation", "仅撤销对话", theme)
                                .size(ButtonSize::Sm)
                                .variant(ButtonVariant::Secondary)
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.confirm_undo(false, window, cx)),
                                ),
                        )
                    })
                    .when(!undo, |element| {
                        element.child(
                            Button::new("restore-dialog-send-only", "仅重置对话并发送", theme)
                                .size(ButtonSize::Sm)
                                .variant(ButtonVariant::Primary)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.send_edit_without_files(window, cx)
                                })),
                        )
                    })
                    .when(undo, |element| {
                        let label = if has_files {
                            if can_restore {
                                format!("撤销并还原 {file_count} 个文件")
                            } else {
                                "无法安全还原文件".to_string()
                            }
                        } else {
                            "撤销消息".to_string()
                        };
                        element.child(
                            Button::new("restore-dialog-confirm", label, theme)
                                .size(ButtonSize::Sm)
                                .variant(ButtonVariant::Danger)
                                .disabled(has_files && !can_restore)
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.confirm_undo(true, window, cx)),
                                ),
                        )
                    }),
            );

        // Entrance: the backdrop fades while the card settles down 4px, on
        // the same 180ms beat as the menus and approval cards. Exit stays
        // instant — cancelling must feel immediate.
        let card = card.with_animation(
            "restore-dialog-enter",
            Animation::new(overlay_motion_duration()).with_easing(ease_out_cubic),
            |element, delta| element.mt(px(4.0 * (1.0 - delta))),
        );

        div()
            .id("restore-dialog-backdrop")
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .bg(theme.overlay)
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                let _ = backdrop_entity.update(cx, |this, cx| {
                    if this.edit_conflict.is_some() {
                        this.dismiss_edit_conflict(window, cx);
                    } else {
                        this.close_undo_dialog(window, cx);
                    }
                });
            })
            .child(card)
            .with_animation(
                "restore-dialog-backdrop-enter",
                Animation::new(overlay_motion_duration()).with_easing(ease_out_cubic),
                |element, delta| element.opacity(delta),
            )
            .into_any_element()
    }

    /// Donation entry behind the sidebar heart. The QR code is a raster asset
    /// in the design system's `AssetSource`, so `img()` loads it by path.
    fn render_reward_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = &self.theme;
        let backdrop_entity = cx.entity().downgrade();
        let card = div()
            .id("reward-dialog")
            .w(px(396.0))
            .rounded(px(radius::XL))
            .border_1()
            .border_color(theme.border)
            .bg(theme.elevated)
            .shadow(shadow_float(theme))
            .p(px(18.0))
            .flex()
            .flex_col()
            .gap(px(14.0))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(30.0))
                            .rounded(px(radius::MD))
                            .bg(theme.accent_soft)
                            .child(svg().path(icons::HEART).size(px(15.0)).text_color(theme.text)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .text_size(px(15.0))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme.text)
                                    .child("为开发者打赏"),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(theme.text_faint)
                                    .child("微信扫一扫，金额随意"),
                            ),
                    )
                    .child(
                        IconButton::new("reward-close", icons::X, theme)
                            .size(24.0, 13.0)
                            .tooltip("关闭 (Esc)")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.reward_open = false;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                // A QR code needs its quiet white field; the image already has
                // one, the wrapper only rounds the corners.
                div()
                    .rounded(px(radius::MD))
                    .overflow_hidden()
                    .bg(gpui::white())
                    .child(img(images::REWARD_QR).w(px(360.0)).h(px(360.0))),
            )
            .child(
                div()
                    .w_full()
                    .text_center()
                    .text_size(px(10.5))
                    .text_color(theme.text_faint)
                    .child("感谢你支持 EffiBuddy 与 rein 的开发 · gozaoo"),
            )
            .with_animation(
                "reward-dialog-enter",
                Animation::new(overlay_motion_duration()).with_easing(ease_out_cubic),
                |element, delta| element.mt(px(4.0 * (1.0 - delta))),
            );

        div()
            .id("reward-dialog-backdrop")
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .bg(theme.overlay)
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                let _ = backdrop_entity.update(cx, |this, cx| {
                    this.reward_open = false;
                    cx.notify();
                });
            })
            .child(card)
            .with_animation(
                "reward-dialog-backdrop-enter",
                Animation::new(overlay_motion_duration()).with_easing(ease_out_cubic),
                |element, delta| element.opacity(delta),
            )
            .into_any_element()
    }

    fn render_palette(&self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = &self.theme;
        let query = self.palette_query.read(cx).content().to_string();
        let items = self.palette_items(&query);
        let selected = self.palette_selection_index(&query);
        let mut rows: Vec<AnyElement> = Vec::with_capacity(items.len() * 2);
        let mut current_group: Option<PaletteGroup> = None;
        for (index, item) in items.into_iter().enumerate() {
            if current_group != Some(item.group) {
                current_group = Some(item.group);
                rows.push(
                    div()
                        .pt(px(8.0))
                        .pb(px(2.0))
                        .px(px(10.0))
                        .text_size(px(10.5))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text_faint)
                        .child(item.group.label().to_string())
                        .into_any_element(),
                );
            }
            let action_item = item.clone();
            let hint = item.action.shortcut();
            let is_selected = index == selected;
            let is_recent = item.recent;
            rows.push(
                div()
                    .id(SharedString::from(item.id.clone()))
                    .flex()
                    .items_center()
                    .gap_3()
                    .h(px(40.0))
                    .px(px(10.0))
                    .rounded(px(radius::SM))
                    .cursor_pointer()
                    .when(is_selected, |element| element.bg(theme.accent_soft))
                    .when(!is_selected, |element| {
                        element.hover(|style| style.bg(theme.hover))
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.execute_palette_item(action_item.clone(), window, cx);
                    }))
                    .child(
                        svg()
                            .path(item.icon)
                            .size(px(14.0))
                            .text_color(if is_selected {
                                theme.text
                            } else {
                                theme.text_muted
                            })
                            .flex_none(),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .gap(px(1.0))
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(13.0))
                                    .font_weight(if is_selected {
                                        gpui::FontWeight::MEDIUM
                                    } else {
                                        gpui::FontWeight::NORMAL
                                    })
                                    .text_color(theme.text)
                                    .child(item.title),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(10.5))
                                    .text_color(theme.text_faint)
                                    .child(item.subtitle),
                            ),
                    )
                    .when(is_recent, |element| {
                        element.child(small_badge(theme, "最近", BadgeTone::Neutral))
                    })
                    .when_some(hint, |element, hint| element.child(kbd(theme, hint)))
                    .into_any_element(),
            );
        }

        div()
            .id("command-palette-backdrop")
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .bg(theme.overlay)
            .flex()
            .justify_center()
            .items_start()
            .pt(px(88.0))
            .on_mouse_down(MouseButton::Left, {
                let entity = cx.entity().downgrade();
                move |_, window, cx| {
                    let _ = entity.update(cx, |this, cx| this.close_palette(window, cx));
                }
            })
            .child(
                div()
                    .id("command-palette")
                    .w(px(560.0))
                    .rounded(px(radius::XL))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.elevated)
                    .shadow(shadow_float(theme))
                    .p(px(10.0))
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .h(px(40.0))
                            .px(px(12.0))
                            .rounded(px(radius::MD))
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.surface_alt)
                            .child(
                                svg()
                                    .path(icons::SEARCH)
                                    .size(px(14.0))
                                    .text_color(theme.text_faint),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .child(self.palette_input.clone()),
                            )
                            .child(kbd(theme, "Esc")),
                    )
                    .child(
                        div()
                            .id("command-palette-list")
                            .max_h(px(380.0))
                            .overflow_y_scroll()
                            .track_scroll(&self.palette_scroll)
                            .flex()
                            .flex_col()
                            .gap(px(1.0))
                            .when(rows.is_empty(), |element| {
                                element.child(
                                    div()
                                        .py_4()
                                        .text_center()
                                        .text_size(px(12.0))
                                        .text_color(theme.text_faint)
                                        .child("没有匹配的命令或会话"),
                                )
                            })
                            .children(rows),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .px(px(9.0))
                            .pt(px(4.0))
                            .text_size(px(10.5))
                            .text_color(theme.text_faint)
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(5.0))
                                    .child(kbd(theme, "↑↓"))
                                    .child("选择")
                                    .child(kbd(theme, "Enter"))
                                    .child("执行"),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(5.0))
                                    .child(kbd(theme, "Ctrl K"))
                                    .child("打开")
                                    .child(kbd(theme, "Esc"))
                                    .child("关闭"),
                            ),
                    ),
            )
            .into_any_element()
    }
}

impl AppView {
    /// A full-window overlay whose paint phase registers the side-pane drag
    /// listeners. `window.on_mouse_event` is only legal during paint (it
    /// registers for a single frame), so the listeners ride on this canvas,
    /// which GPUI repaints — and re-registers — every frame. The canvas has no
    /// hitbox, so it never intercepts clicks.
    fn side_resize_listener_layer(&self, cx: &Context<Self>) -> impl IntoElement {
        let move_entity = cx.entity().downgrade();
        let up_entity = cx.entity().downgrade();
        gpui::canvas(
            |_, _, _| {},
            move |_, _, window, _| {
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                    if phase != DispatchPhase::Bubble || !event.dragging() {
                        return;
                    }
                    let viewport = f32::from(window.viewport_size().width);
                    let max_width = (viewport - 420.0).clamp(
                        crate::side_pane::MIN_SIDE_PANE_WIDTH,
                        crate::side_pane::MAX_SIDE_PANE_WIDTH,
                    );
                    let width = (viewport - f32::from(event.position.x))
                        .clamp(crate::side_pane::MIN_SIDE_PANE_WIDTH, max_width);
                    let _ = move_entity.update(cx, |this, cx| {
                        if !this.side.resizing {
                            return;
                        }
                        if (this.side.width - width).abs() > 0.5 {
                            this.side.width = width;
                            cx.notify();
                        }
                    });
                });

                window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                    if phase != DispatchPhase::Bubble || event.button != MouseButton::Left {
                        return;
                    }
                    let _ = up_entity.update(cx, |this, cx| {
                        if this.side.resizing {
                            this.side.resizing = false;
                            this.persist_side_pane();
                            cx.notify();
                        }
                    });
                });
            },
        )
        .absolute()
        .inset_0()
        .size_full()
    }
}

impl Render for AppView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // EFFIBUDDY_PERF=1 measures the element-tree build cost per frame.
        let perf_frame = perf::frame_start();
        let theme = self.theme.clone();

        // Smooth-scroll glide: apply this frame's step before the scroll
        // containers lay out, and keep a frame scheduled while any of them is
        // still moving.
        if self.scrolls.tick() && !self.scroll_frame_scheduled {
            self.scroll_frame_scheduled = true;
            cx.on_next_frame(window, |this, _, cx| {
                this.scroll_frame_scheduled = false;
                cx.notify();
            });
        }

        // Panel slides: advance the sidebar / side-pane collapse glides before
        // the tree builds, then keep one frame scheduled while either is still
        // moving (same pattern as the scroll glides above).
        let sidebar_target = if self.config.sidebar_collapsed {
            0.0
        } else {
            1.0
        };
        let side_pane_target =
            if self.side.open && !self.settings_open && self.view_mode == ViewMode::Chat {
                1.0
            } else {
                0.0
            };
        let slides_animating =
            advance_pane_slide(&mut self.sidebar_slide, &mut self.sidebar_p, sidebar_target)
                | advance_pane_slide(
                    &mut self.side_pane_slide,
                    &mut self.side_p,
                    side_pane_target,
                );
        if slides_animating && !self.pane_frame_scheduled {
            self.pane_frame_scheduled = true;
            cx.on_next_frame(window, |this, _, cx| {
                this.pane_frame_scheduled = false;
                cx.notify();
            });
        }

        let content = if self.settings_open {
            self.render_settings(window, cx)
        } else if self.view_mode == ViewMode::Graph {
            self.render_graph(window, cx)
        } else if self.stress_dashboard_active() {
            // Stress lab owns the main area while a run is active. The helper
            // exists in both cfg states so the dispatch itself is ungated.
            self.stress_content(&theme, cx)
        } else {
            self.render_chat(window, cx)
        };

        let mut tree = div()
            .key_context("EffiBuddy")
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.bg)
            .text_color(theme.text)
            .font_family(theme.ui_font())
            .text_size(px(14.0))
            .on_action(cx.listener(|this, _: &NewSession, window, cx| this.new_session(window, cx)))
            .on_action(cx.listener(|this, _: &ToggleTheme, _, cx| this.toggle_theme(cx)))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| this.toggle_sidebar(cx)))
            .on_action(cx.listener(|this, _: &ToggleSettings, window, cx| {
                if this.settings_open {
                    this.settings_open = false;
                    cx.notify();
                } else {
                    this.open_settings(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &FocusComposer, window, cx| {
                this.composer_focus_handle(cx).focus(window);
            }))
            .on_action(cx.listener(|this, _: &StopRun, window, cx| {
                if this.edit_conflict.is_some() {
                    this.dismiss_edit_conflict(window, cx);
                } else if this.undo_dialog.is_some() {
                    this.close_undo_dialog(window, cx);
                } else if this.reward_open {
                    this.reward_open = false;
                    cx.notify();
                } else if this.edit_message.is_some() {
                    this.cancel_edit_message(window, cx);
                } else if this.palette_open {
                    this.close_palette(window, cx);
                } else if this.model_menu.open {
                    this.close_model_menu(cx);
                } else if this.project_menu.target.is_some() {
                    this.close_project_menu(cx);
                } else if this.session_menu.target.is_some() {
                    this.close_session_menu(cx);
                } else if this.session_rename.is_some() {
                    this.close_session_rename(window, cx);
                } else if this.project_editor.is_some() {
                    this.close_project_editor(cx);
                } else if this.settings.model_editor.open {
                    this.close_model_editor(cx);
                } else if this.view_mode == ViewMode::Graph && this.graph.selected.is_some() {
                    this.graph.clear_selection();
                    cx.notify();
                } else if this.side.overlay.is_some() {
                    this.side.overlay = None;
                    cx.notify();
                } else if this
                    .side
                    .active()
                    .is_some_and(|tab| matches!(tab.kind, SidePaneTabKind::Agent(_)))
                {
                    this.close_agent_view(cx);
                } else if this.settings_open {
                    this.settings_open = false;
                    cx.notify();
                } else {
                    this.interrupt(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &OpenCommandPalette, window, cx| {
                this.open_palette(window, cx);
            }))
            .on_action(cx.listener(|this, _: &CyclePermissionMode, _, cx| {
                this.cycle_permission_mode(cx);
            }))
            .on_action(cx.listener(|this, _: &ZoomIn, window, cx| {
                this.zoom_by(0.1, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ZoomOut, window, cx| {
                this.zoom_by(-0.1, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ZoomReset, window, cx| {
                this.zoom_to(1.0, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleGraph, _, cx| {
                let next = if this.view_mode == ViewMode::Graph {
                    ViewMode::Chat
                } else {
                    ViewMode::Graph
                };
                this.set_view_mode(next, cx);
            }))
            .child(self.render_titlebar(window, cx))
            .child({
                div()
                    .flex()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .child(self.side_resize_listener_layer(cx))
                    // While a slide is in flight the panel renders inside a
                    // clipping wrapper at the interpolated width; once settled
                    // it goes in bare so nothing can clip its edges.
                    .when(self.sidebar_p > 0.001, |element| {
                        if self.sidebar_p >= 1.0 {
                            element.child(self.render_sidebar(window, cx))
                        } else {
                            element.child(
                                div()
                                    .w(px(SIDEBAR_WIDTH * self.sidebar_p))
                                    .h_full()
                                    .flex_none()
                                    .overflow_hidden()
                                    .border_r_1()
                                    .border_color(theme.border)
                                    .child(self.render_sidebar(window, cx)),
                            )
                        }
                    })
                    .child(div().flex_1().min_w(px(0.0)).h_full().child(content))
                    .when(self.side_p > 0.001, |element| {
                        if self.side_p >= 1.0 {
                            element.child(self.render_side_pane(window, cx))
                        } else {
                            element.child(
                                div()
                                    .w(px(self.side.width * self.side_p))
                                    .h_full()
                                    .flex_none()
                                    .overflow_hidden()
                                    .child(self.render_side_pane(window, cx)),
                            )
                        }
                    })
            })
            .when_some(self.render_side_overlay(window, cx), |element, overlay| {
                element.child(overlay)
            })
            .when_some(self.render_project_menu(window, cx), |element, menu| {
                element.child(menu)
            })
            .when_some(self.render_session_menu(window, cx), |element, menu| {
                element.child(menu)
            })
            .when_some(self.render_project_editor(window, cx), |element, editor| {
                element.child(editor)
            })
            .when_some(self.render_session_rename(window, cx), |element, dialog| {
                element.child(dialog)
            })
            .when_some(self.render_model_menu(window, cx), |element, menu| {
                element.child(menu)
            })
            .when_some(self.render_model_editor(window, cx), |element, editor| {
                element.child(editor)
            })
            .when(self.palette_open, |element| {
                element.child(self.render_palette(window, cx))
            })
            .when(self.reward_open, |element| {
                element.child(self.render_reward_dialog(cx))
            })
            .when(
                self.undo_dialog.is_some() || self.edit_conflict.is_some(),
                |element| element.child(self.render_restore_dialog(cx)),
            );
        if perf::hud_snapshot().is_some() {
            tree = tree.child(render_perf_hud(&theme));
        }
        if let Some(started) = perf_frame {
            perf::frame_end(started);
        }
        tree
    }
}

/// Live performance overlay (`EFFIBUDDY_PERF_HUD=1`): fps, frame pacing and
/// render cost in one glance, plus the active degradation-mode badges.
fn render_perf_hud(theme: &Theme) -> AnyElement {
    let Some(hud) = perf::hud_snapshot() else {
        return div().into_any_element();
    };
    let line = |text: String| {
        div()
            .child(text)
            .text_size(px(10.0))
            .line_height(px(14.0))
            .text_color(theme.text_muted)
    };
    div()
        .absolute()
        .top(px(10.0))
        .right(px(14.0))
        .flex()
        .flex_col()
        .gap_0p5()
        .rounded(px(6.0))
        .bg(gpui::black().opacity(0.72))
        .px_2()
        .py_1()
        .font_family(theme.mono_font())
        .child(line(format!(
            "{:.0} fps · p50 {:.1} · p95 {:.1} ms",
            hud.fps, hud.interval_p50_ms, hud.interval_p95_ms
        )))
        .child(line(format!(
            "render avg {:.2} · p95 {:.2} ms · hits {}/s",
            hud.render_avg_ms, hud.render_p95_ms, hud.cache_hits_per_s
        )))
        .child(line(perf::mode_badges()))
        .into_any_element()
}

impl Drop for AppView {
    fn drop(&mut self) {
        if let Some(run) = &self.active_run {
            run.handle.interrupt();
            run.approval.deny_all();
        }
        // Teardown path: no executor context here, so serialize inline — the
        // process is exiting either way.
        if !self.preview_mode
            && let Some(active) = &self.active
        {
            let snapshot = lock_session(&active.shared).clone();
            if let Ok(save) = self.store.stage(&snapshot) {
                let _ = self.persist_tx.send(save);
            }
            // Flush the composer draft synchronously: the debounced writer may
            // not get another executor turn before the process exits.
            let _ = self.drafts.save();
        }
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(Duration::from_millis(400));
        }
    }
}

/// Model-facing instruction injected while the session is in plan mode: the
/// run stays read-only and the model should produce a plan instead of acting.
const PLAN_MODE_SYSTEM_PROMPT: &str = "## Permission mode: plan\n\
You are in plan mode. Use read-only tools (read_file, list_dir, grep, todo_write) to research, \
and never try to edit files or run commands: file edits and command execution are blocked and \
will be denied in this mode. Produce a complete, step-by-step implementation plan and stop. \
Wait for the user to switch the permission mode (Shift+Tab) before making any change.";

/// One-line explanation shown on approval cards. `None` for the default mode,
/// whose cards need no extra context.
fn permission_mode_hint(mode: PermissionMode) -> Option<&'static str> {
    match mode {
        PermissionMode::Default => None,
        PermissionMode::AcceptEdits => {
            Some("自动编辑模式：文件编辑已自动批准，shell 等命令仍需确认。")
        }
        PermissionMode::Plan => {
            Some("计划模式：编辑与命令会被自动拒绝。请先让 EffiBuddy 输出计划，再切换模式执行。")
        }
        PermissionMode::FullAccess => {
            Some("完全访问模式只对主会话生效；来自子代理的请求仍会请求批准。")
        }
    }
}

/// File/conflict report shared by the undo confirmation and the edit-conflict
/// dialog. Every row the user is about to change is listed here; shell commands
/// are called out separately because their file effects cannot be tracked.
fn render_restore_report(theme: &Theme, plan: &RestorePlan) -> Div {
    let mut report = div().flex().flex_col().gap(px(8.0));
    if !plan.has_file_changes() && plan.shell_commands.is_empty() {
        return report.child(
            div()
                .py(px(8.0))
                .text_size(px(11.5))
                .text_color(theme.text_faint)
                .child("本轮没有可还原的文件改动。"),
        );
    }

    if !plan.files.is_empty() {
        report = report.child(restore_section_label(
            if plan.can_restore() {
                format!("将还原 {} 个文件", plan.files.len())
            } else {
                format!("可安全还原 {} 个文件", plan.files.len())
            },
            theme.success,
        ));
        let mut rows = div().flex().flex_col().gap(px(1.0));
        for file in plan.files.iter().take(crate::checkpoint::MAX_LISTED_FILES) {
            let tools = if file.tool_names.is_empty() {
                String::new()
            } else {
                format!(" · {}", file.tool_names.join(" / "))
            };
            rows = rows.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .min_h(px(24.0))
                    .px(px(4.0))
                    .rounded(px(radius::XS))
                    .hover(|style| style.bg(theme.hover))
                    .child(
                        svg()
                            .path(icons::FILE)
                            .size(px(12.0))
                            .flex_none()
                            .text_color(theme.text_faint),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .font_family(theme.mono_font())
                            .text_size(px(11.0))
                            .text_color(theme.text)
                            .child(file.path.clone()),
                    )
                    .child(small_badge(
                        theme,
                        file.action.label(),
                        match file.action {
                            RestoreAction::Restore => BadgeTone::Neutral,
                            RestoreAction::Delete => BadgeTone::Warning,
                        },
                    ))
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(10.0))
                            .text_color(theme.text_faint)
                            .child(format!("{} 次修改{tools}", file.operations)),
                    ),
            );
        }
        if plan.files.len() > crate::checkpoint::MAX_LISTED_FILES {
            rows = rows.child(
                div()
                    .py(px(3.0))
                    .text_size(px(10.5))
                    .text_color(theme.text_faint)
                    .child(format!(
                        "还有 {} 个文件未列出…",
                        plan.files.len() - crate::checkpoint::MAX_LISTED_FILES
                    )),
            );
        }
        report = report.child(rows);
    }

    if !plan.conflicts.is_empty() {
        report = report.child(restore_section_label(
            format!("无法安全还原 {} 个文件", plan.conflicts.len()),
            theme.warning,
        ));
        let mut rows = div().flex().flex_col().gap(px(1.0));
        for conflict in plan
            .conflicts
            .iter()
            .take(crate::checkpoint::MAX_LISTED_FILES)
        {
            rows = rows.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .min_h(px(24.0))
                    .px(px(4.0))
                    .rounded(px(radius::XS))
                    .hover(|style| style.bg(theme.hover))
                    .child(
                        svg()
                            .path(icons::ALERT)
                            .size(px(12.0))
                            .flex_none()
                            .text_color(theme.warning),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .font_family(theme.mono_font())
                            .text_size(px(11.0))
                            .text_color(theme.text)
                            .child(conflict.path.clone()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(10.0))
                            .text_color(theme.warning)
                            .child(conflict.reason.label()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(10.0))
                            .text_color(theme.text_faint)
                            .child(format!(
                                "{} 次修改{}",
                                conflict.operations,
                                if conflict.tool_names.is_empty() {
                                    String::new()
                                } else {
                                    format!(" · {}", conflict.tool_names.join(" / "))
                                }
                            )),
                    ),
            );
        }
        if plan.conflicts.len() > crate::checkpoint::MAX_LISTED_FILES {
            rows = rows.child(
                div()
                    .py(px(3.0))
                    .text_size(px(10.5))
                    .text_color(theme.text_faint)
                    .child(format!(
                        "还有 {} 个文件未列出…",
                        plan.conflicts.len() - crate::checkpoint::MAX_LISTED_FILES
                    )),
            );
        }
        report = report.child(rows);
    }

    if !plan.shell_commands.is_empty() {
        report = report.child(restore_section_label(
            "无法追踪的改动".to_string(),
            theme.warning,
        ));
        let mut commands = div().flex().flex_col().gap(px(2.0));
        for command in plan.shell_commands.iter().take(4) {
            let command = command.replace(['\r', '\n'], " ");
            let command = preview_text(&command, 160);
            commands = commands.child(
                div()
                    .truncate()
                    .px(px(4.0))
                    .font_family(theme.mono_font())
                    .text_size(px(10.5))
                    .text_color(theme.text_muted)
                    .child(format!("$ {command}")),
            );
        }
        if plan.shell_commands.len() > 4 {
            commands = commands.child(
                div()
                    .px(px(4.0))
                    .text_size(px(10.0))
                    .text_color(theme.text_faint)
                    .child(format!("还有 {} 条命令…", plan.shell_commands.len() - 4)),
            );
        }
        report = report.child(commands).child(
            div()
                .px(px(4.0))
                .text_size(px(10.5))
                .line_height(px(15.0))
                .text_color(theme.warning)
                .child("这些命令造成的文件改动无法追踪，不会被还原。"),
        );
    }

    report
}

fn restore_section_label(label: String, color: gpui::Hsla) -> Div {
    div()
        .px(px(4.0))
        .text_size(px(10.5))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(color)
        .child(label)
}

/// Short excerpt for dialogs. Counts characters, not bytes, so a cut never
/// lands inside a multi-byte glyph.
fn preview_text(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        out.push('…');
    }
    out
}

/// Row cap of the command palette, recents included.
const PALETTE_MAX_ITEMS: usize = 14;
/// How many most-recently-used rows may be pinned above the scored list.
const PALETTE_MAX_RECENT: usize = 6;

/// Row index of palette item `selected` inside the rendered row list: group
/// headers are emitted as their own rows, so the highlighted item's row is
/// its item index plus the number of headers emitted up to and including it.
fn palette_row_index(items: &[PaletteItem], selected: usize) -> usize {
    let mut headers = 0usize;
    let mut current_group: Option<PaletteGroup> = None;
    for item in items.iter().take(selected + 1) {
        if current_group != Some(item.group) {
            current_group = Some(item.group);
            headers += 1;
        }
    }
    selected + headers
}

/// Filter, pin and sort palette rows for `query`.
///
/// Order: matching MRU entries first (their group is switched to "recent"),
/// then the rest by fuzzy score, stable for equal scores. Empty queries keep
/// the natural order. Mirrors ZCode's QuickPick recent pinning on top of the
/// mention-search three-tier scoring model.
fn order_palette_items(
    mut items: Vec<PaletteItem>,
    query: &str,
    recent: &[String],
) -> Vec<PaletteItem> {
    let query = query.trim();
    if !query.is_empty() {
        items.retain(|item| palette_score(query, item).is_some());
    }
    for item in &mut items {
        item.recent = recent.iter().any(|id| id == &item.id);
    }
    if recent.is_empty() {
        if !query.is_empty() {
            items.sort_by_key(|item| std::cmp::Reverse(palette_score(query, item).unwrap_or(0)));
        }
        items.truncate(PALETTE_MAX_ITEMS);
        return items;
    }

    let mut pinned: Vec<PaletteItem> = items
        .iter()
        .filter(|item| item.recent)
        .take(PALETTE_MAX_RECENT)
        .cloned()
        .collect();
    // MRU order: the config list already holds ids from most to least recent.
    pinned.sort_by_key(|item| {
        recent
            .iter()
            .position(|id| id == &item.id)
            .unwrap_or(usize::MAX)
    });
    for item in &mut pinned {
        item.group = PaletteGroup::Recent;
    }
    let pinned_ids: HashSet<String> = pinned.iter().map(|item| item.id.clone()).collect();
    let mut rest: Vec<PaletteItem> = items
        .into_iter()
        .filter(|item| !pinned_ids.contains(&item.id))
        .collect();
    if !query.is_empty() {
        rest.sort_by_key(|item| std::cmp::Reverse(palette_score(query, item).unwrap_or(0)));
    }
    pinned.extend(rest);
    pinned.truncate(PALETTE_MAX_ITEMS);
    pinned
}

/// Bilingual fuzzy score for one palette row: title prefix > title substring >
/// keyword prefix/substring > subtitle substring > subsequence. Queries that
/// contain Han characters skip the subsequence tier, which over-matches CJK.
/// Returns `None` when the row should not be shown.
fn palette_score(query: &str, item: &PaletteItem) -> Option<i32> {
    const SCORE_TITLE_PREFIX: i32 = 120;
    const SCORE_TITLE_SUBSTRING: i32 = 100;
    const SCORE_KEYWORD_PREFIX: i32 = 90;
    const SCORE_KEYWORD_SUBSTRING: i32 = 80;
    const SCORE_SUBTITLE_SUBSTRING: i32 = 70;
    const SCORE_TITLE_SUBSEQUENCE: i32 = 50;
    const SCORE_KEYWORD_SUBSEQUENCE: i32 = 40;

    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Some(0);
    }
    let title = item.title.to_lowercase();
    if title.starts_with(&query) {
        return Some(SCORE_TITLE_PREFIX);
    }
    if title.contains(&query) {
        return Some(SCORE_TITLE_SUBSTRING);
    }
    let mut keyword_best: Option<i32> = None;
    for keyword in item.keywords {
        let keyword = keyword.to_lowercase();
        let score = if keyword.starts_with(&query) {
            SCORE_KEYWORD_PREFIX
        } else if keyword.contains(&query) {
            SCORE_KEYWORD_SUBSTRING
        } else {
            continue;
        };
        keyword_best = Some(score.max(keyword_best.unwrap_or(0)));
    }
    if let Some(score) = keyword_best {
        return Some(score);
    }
    if item.subtitle.to_lowercase().contains(&query) {
        return Some(SCORE_SUBTITLE_SUBSTRING);
    }
    if has_han(&query) {
        return None;
    }
    if is_subsequence(&query, &title) {
        return Some(SCORE_TITLE_SUBSEQUENCE);
    }
    if item
        .keywords
        .iter()
        .any(|keyword| is_subsequence(&query, &keyword.to_lowercase()))
    {
        return Some(SCORE_KEYWORD_SUBSEQUENCE);
    }
    None
}

/// True when `needle`'s characters appear in `haystack` in order.
fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut chars = haystack.chars();
    needle
        .chars()
        .all(|expected| chars.any(|found| found == expected))
}

/// True when the text contains CJK Unified Ideographs (Han characters).
fn has_han(text: &str) -> bool {
    text.chars()
        .any(|ch| ('\u{4E00}'..='\u{9FFF}').contains(&ch))
}

fn feature_chip(theme: &Theme, icon_path: &'static str, label: &str) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(px(5.0))
        .h(px(26.0))
        .px(px(8.0))
        .rounded(px(999.0))
        .bg(theme.surface_alt)
        .child(
            svg()
                .path(icon_path)
                .size(px(11.0))
                .text_color(theme.text_faint),
        )
        .child(
            div()
                .text_size(px(10.5))
                .text_color(theme.text_faint)
                .child(label.to_string()),
        )
        .into_any_element()
}

/// One row in the project popover.
fn project_menu_row(
    theme: &Theme,
    id: &'static str,
    icon: &'static str,
    label: &'static str,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(8.0))
        .h(px(30.0))
        .px(px(7.0))
        .rounded(px(radius::XS))
        .cursor_pointer()
        .hover(|style| style.bg(theme.hover))
        .child(svg().path(icon).size(px(12.0)).text_color(theme.text_muted))
        .child(
            div()
                .text_size(px(12.0))
                .text_color(theme.text)
                .child(label),
        )
}

/// One row in a conversation's “more” popover.
fn session_menu_row(
    theme: &Theme,
    id: &'static str,
    icon: &'static str,
    label: &'static str,
    danger: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(8.0))
        .h(px(30.0))
        .px(px(7.0))
        .rounded(px(radius::XS))
        .cursor_pointer()
        .hover(|style| style.bg(if danger { theme.danger_soft } else { theme.hover }))
        .child(
            svg()
                .path(icon)
                .size(px(12.0))
                .text_color(if danger { theme.danger } else { theme.text_muted }),
        )
        .child(
            div()
                .text_size(px(12.0))
                .text_color(if danger { theme.danger } else { theme.text })
                .child(label),
        )
}

/// Deterministic per-project accent, mirroring ZCode's colored program icons.
fn project_color(theme: &Theme, seed: &str) -> gpui::Hsla {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in seed.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    // A stable hue per path; lightness is tuned per theme so the letter avatar
    // stays readable on both the light and dark sidebar.
    let hue = (hash % 360) as f32 / 360.0;
    gpui::hsla(hue, 0.55, if theme.is_dark() { 0.66 } else { 0.42 }, 1.0)
}

/// A tiny 18px icon action used by sidebar rows. The caller attaches the
/// click handler so it can capture the right session/project id.
fn sidebar_icon_button(
    theme: &Theme,
    id: SharedString,
    icon: &'static str,
    color: gpui::Hsla,
    label: &'static str,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .size(px(18.0))
        .rounded(px(4.0))
        .cursor_pointer()
        .hover(|style| style.bg(theme.active))
        .tooltip(effibuddy_ui::tooltip(theme, label))
        .child(svg().path(icon).size(px(11.0)).text_color(color))
}

/// Collapsible section header (`置顶` / `全部项目` / `任务`). `trailing` holds
/// an optional action such as the projects section's "添加工作区" button.
///
/// 折叠箭头跟在文字右侧（而非行首）；除置顶外，箭头仅在行悬停时出现。
/// 箭头用 opacity 显隐，占位不变，悬停时行内不会抖动。
fn sidebar_section_header(
    theme: &Theme,
    group: &'static str,
    label: &'static str,
    count: usize,
    collapsed: bool,
    trailing: Option<AnyElement>,
    cx: &mut Context<AppView>,
) -> AnyElement {
    let row_group = SharedString::from(format!("sidebar-section-{group}"));
    let chevron_hover_gated = group != "pinned";
    div()
        .id(row_group.clone())
        .group(row_group.clone())
        .flex()
        .items_center()
        .gap(px(4.0))
        .h(px(26.0))
        .px(px(6.0))
        .mt(px(6.0))
        .rounded(px(radius::XS))
        .cursor_pointer()
        .hover(|style| style.bg(theme.hover))
        .on_click(cx.listener(move |this, _, _, cx| {
            this.toggle_sidebar_section(group, cx);
        }))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .items_center()
                .gap(px(4.0))
                .child(
                    div()
                        .flex_none()
                        .text_size(px(10.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme.text_faint)
                        .child(label),
                )
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .opacity(if chevron_hover_gated { 0.0 } else { 1.0 })
                        .when(chevron_hover_gated, |element| {
                            element.group_hover(row_group, |style| style.opacity(1.0))
                        })
                        .child(disclosure_chevron(
                            SharedString::from(format!("sidebar-chevron-{group}")),
                            !collapsed,
                            theme.text_faint,
                            10.0,
                        )),
                ),
        )
        .child(
            div()
                .text_size(px(10.0))
                .text_color(theme.text_faint)
                .child(count.to_string()),
        )
        .when_some(trailing, |element, trailing| element.child(trailing))
        .into_any_element()
}

/// Project header: colored avatar + name, with more/new-task actions
/// revealed on hover. Clicking the row collapses the project.
fn sidebar_project_header(
    theme: &Theme,
    project: &ProjectGroup,
    collapsed: bool,
    force_hover: bool,
    cx: &mut Context<AppView>,
) -> AnyElement {
    let key = project.key.clone();
    let row_group = SharedString::from(format!("project-row:{key}"));
    let name = project.name.clone();
    let initial = name
        .chars()
        .next()
        .map(|character| character.to_uppercase().to_string())
        .unwrap_or_default();
    let color = project_color(theme, &project.key);
    let key_for_click = key.clone();
    let project_for_menu = project.clone();
    let project_for_add = project.clone();
    let tooltip_text = project
        .roots
        .iter()
        .map(|root| root.display().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let additional = project.roots.len().saturating_sub(1);
    let multi_root = project.roots.len() > 1;

    div()
        .id(SharedString::from(format!("project-row-{key}")))
        .group(row_group.clone())
        .flex()
        .items_center()
        .gap(px(6.0))
        .h(px(30.0))
        .px(px(6.0))
        .rounded(px(radius::XS))
        .cursor_pointer()
        .hover(|style| style.bg(theme.hover))
        .when(force_hover, |element| element.bg(theme.hover))
        .tooltip(effibuddy_ui::tooltip(theme, tooltip_text))
        .on_click(cx.listener(move |this, _, _, cx| {
            this.toggle_sidebar_section(&key_for_click, cx);
        }))
        .child(
            div()
                .flex_none()
                .size(px(18.0))
                .rounded(px(5.0))
                .bg(color.opacity(0.16))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .text_size(px(9.5))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(color)
                        .child(initial),
                ),
        )
        // 项目头折叠其会话列表：箭头同样跟在名字文字右侧；名字过长时收缩
        // 截断，箭头始终可见。
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .items_center()
                .gap(px(4.0))
                .child(
                    div()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(12.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text_muted)
                        .child(name),
                )
                .child(disclosure_chevron(
                    SharedString::from(format!("project-chevron-{key}")),
                    !collapsed,
                    theme.text_faint,
                    10.0,
                )),
        )
        .when(multi_root, |element| {
            element.child(
                div()
                    .flex_none()
                    .rounded(px(999.0))
                    .bg(theme.active)
                    .px(px(5.0))
                    .py(px(1.0))
                    .text_size(px(9.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text_muted)
                    .child(format!("+{additional}")),
            )
        })
        .child(
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(1.0))
                .opacity(if force_hover { 1.0 } else { 0.0 })
                .group_hover(row_group, |style| style.opacity(1.0))
                .child(
                    sidebar_icon_button(
                        theme,
                        SharedString::from(format!("project-more-{key}")),
                        icons::MORE,
                        theme.text_faint,
                        "项目操作",
                    )
                    .on_click(cx.listener(
                        move |this, event: &gpui::ClickEvent, _, cx| {
                            cx.stop_propagation();
                            this.open_project_menu(project_for_menu.clone(), event.position(), cx);
                        },
                    )),
                )
                .child(
                    sidebar_icon_button(
                        theme,
                        SharedString::from(format!("project-add-{key}")),
                        icons::PLUS,
                        theme.text_faint,
                        "在项目中新建任务",
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        let project_id = project_for_add.id.clone();
                        let primary = project_for_add.primary().map(Path::to_path_buf);
                        this.new_session_in_project(project_id, primary, window, cx);
                    })),
                ),
        )
        .into_any_element()
}

/// 展开时新行的渐显 stagger：每行延迟 50ms，封顶 10 行。
const SESSION_ENTER_STAGGER: Duration = Duration::from_millis(50);
const SESSION_ENTER_STAGGER_STEPS: usize = 10;
/// 会话名跑马灯速度（px/s）。视口约 176px，整循环约 5s 起，节奏平缓。
const SESSION_MARQUEE_SPEED: f32 = 55.0;
/// 双副本文本条带的副本间隔（px）：循环一周后文本头部在这里停留。
const SESSION_MARQUEE_GAP: f32 = 24.0;
/// 会话名遮罩淡出宽度：0.5em @ 12.5px。
const SESSION_TITLE_FADE_W: f32 = 6.25;
/// 会话名字号（测量与渲染共用）。
const SESSION_TITLE_SIZE: f32 = 12.5;

/// 会话名视口宽度。常量与 `sidebar_session_row` 的行布局一一对应：
/// 侧栏 272 − 列表左右 10×2 − 行左右 4×2 − 置顶钮 18 − 两个行 gap 5×2 −
/// 时间列 40。改动行布局时同步这里。
fn session_title_viewport_w() -> f32 {
    SIDEBAR_WIDTH - 2.0 * 10.0 - 2.0 * 4.0 - 18.0 - 2.0 * 5.0 - 40.0
}

/// 单次循环时长（秒）：距离 = 文本宽 + 副本间隔。
fn marquee_cycle_duration(distance: f32) -> f32 {
    if distance <= 0.0 {
        0.0
    } else {
        distance / SESSION_MARQUEE_SPEED
    }
}

/// 一帧的跑马灯绘制参数（随帧变化，渲染期算出）。
#[derive(Clone, Copy, Debug)]
struct SessionMarqueeFrame {
    /// 会话名溢出视口：右缘淡出常显，跑马灯可用。
    overflow: bool,
    /// 文本条带的绘制期左移量；0 表示静止。
    offset: f32,
    /// 左缘遮罩宽度；0 表示不绘制。
    left_fade: f32,
}

/// 状态机某帧的输出。
#[derive(Clone, Copy)]
enum MarqueeTick {
    None,
    /// 播放中：条带绘制期左移 offset（px）。
    Play { offset: f32 },
    /// 收尾：偏移已回 0，左缘遮罩按 progress（0..1）收回。
    Retract { progress: f32 },
}

/// 会话行跑马灯状态机。
///
/// 状态只在两个时机变更：指针进/出（hover 事件）与 `advance`（每帧渲染
/// 推进 Finish→Retract→Idle 的时钟）。偏移一律绘制期平移（PaintShift），
/// 不触碰布局。打断策略：进入播放中的行沿用原周期时钟（偏移连续、不跳
/// 变）；离开后播完当前周期，回到 left=0（与静止态对齐）再收回遮罩。
#[derive(Debug, Default)]
struct SessionMarquee {
    phase: MarqueePhase,
    /// 最近一帧的循环距离（文本宽 + 副本间隔）；0 = 未溢出/未测量。
    distance: Cell<f32>,
}

#[derive(Debug, Default)]
enum MarqueePhase {
    /// 静止：溢出时仅右缘淡出。
    #[default]
    Idle,
    /// 循环播放中。`cycle_start` 是本轮循环起点。
    Playing { cycle_start: Instant },
    /// 指针已离开：继续播到下一个 left=0 边界再收尾。
    Finish { cycle_start: Instant, left_at: Instant },
    /// 偏移已回 0，左缘遮罩正在收回（150ms ease-out）。
    Retract { start: Instant },
}

impl SessionMarquee {
    fn is_animating(&self) -> bool {
        !matches!(self.phase, MarqueePhase::Idle)
    }

    /// 回到静止态（标题不再溢出 / 减弱动效）。
    fn reset(&mut self) {
        self.phase = MarqueePhase::Idle;
    }

    /// 指针进入。播放中/收尾等待时延续原时钟（不打断当前周期）；其余从
    /// 0 起新周期，与静止态的文本位置对齐。
    fn pointer_enter(&mut self) {
        self.phase = match self.phase {
            MarqueePhase::Playing { cycle_start } | MarqueePhase::Finish { cycle_start, .. } => {
                MarqueePhase::Playing { cycle_start }
            }
            _ => MarqueePhase::Playing {
                cycle_start: Instant::now(),
            },
        };
    }

    /// 指针离开：播到下一个 left=0 边界再收尾。重复离开只刷新 `left_at`，
    /// 不重置时钟。
    fn pointer_leave(&mut self) {
        if let MarqueePhase::Playing { cycle_start } = self.phase {
            self.phase = MarqueePhase::Finish {
                cycle_start,
                left_at: Instant::now(),
            };
        }
    }

    /// 每帧由渲染调用：把 Finish/Retract 推进到下一相位。
    fn advance(&mut self, distance: f32, now: Instant) {
        match self.phase {
            MarqueePhase::Finish {
                cycle_start,
                left_at,
            } => {
                let duration = marquee_cycle_duration(distance);
                if duration <= 0.0 {
                    return;
                }
                // 下一个整数倍周期边界即 left=0。deadline 回溯到边界时刻，
                // 帧率抖动时收回动画也从正确位置起步。
                let k = ((left_at - cycle_start).as_secs_f32() / duration).floor() + 1.0;
                let deadline = cycle_start + Duration::from_secs_f32(duration * k);
                if now >= deadline {
                    self.phase = MarqueePhase::Retract { start: deadline };
                }
            }
            MarqueePhase::Retract { start } if now - start >= CONTROL_MOTION => {
                self.phase = MarqueePhase::Idle;
            }
            _ => {}
        }
    }

    /// 本帧的输出。
    fn frame(&self, distance: f32, now: Instant) -> MarqueeTick {
        match self.phase {
            MarqueePhase::Idle => MarqueeTick::None,
            MarqueePhase::Playing { cycle_start } | MarqueePhase::Finish { cycle_start, .. } => {
                let duration = marquee_cycle_duration(distance);
                if duration <= 0.0 {
                    return MarqueeTick::None;
                }
                let t = ((now - cycle_start).as_secs_f32() / duration).fract();
                MarqueeTick::Play {
                    offset: -distance * t,
                }
            }
            MarqueePhase::Retract { start } => {
                let t = ((now - start).as_secs_f32() / CONTROL_MOTION.as_secs_f32()).clamp(0.0, 1.0);
                MarqueeTick::Retract { progress: t }
            }
        }
    }
}

/// 用文本系统测量单行宽度；`layout_line` 按帧缓存，同一帧内重复测量近似免费。
fn measured_line_width(
    window: &Window,
    text: &str,
    font_family: &'static str,
    weight: gpui::FontWeight,
    font_size: f32,
) -> f32 {
    let font = gpui::Font {
        weight,
        ..gpui::font(font_family)
    };
    let run = gpui::TextRun {
        len: text.len(),
        font,
        color: gpui::black(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    window
        .text_system()
        .layout_line(text, px(font_size), &[run], None)
        .width
        .into()
}

/// One conversation row: title + relative time, with pin and archive actions
/// revealed on hover. Pinned rows live in the 置顶 section and show the pin
/// affordance in its active color.
#[allow(clippy::too_many_arguments)]
fn motion_session_row(
    motion: &MotionController,
    theme: &Theme,
    summary: &SessionSummary,
    active: bool,
    pinned_group: bool,
    force_hover: bool,
    marquee: SessionMarqueeFrame,
    stagger: usize,
    cx: &mut Context<AppView>,
) -> AnyElement {
    let motion_id = SharedString::from(summary.id.clone());
    let mut row =
        sidebar_session_row(theme, summary, active, pinned_group, force_hover, marquee, cx);
    // 展开时新行按 50ms stagger 渐显。只延迟透明度：下方已存在的行仍然
    // 立刻位移（不延迟 FLIP），所以新行淡入时不会和滑动中的内容重叠；
    // stagger 封顶 10 行，长列表末行不会等太久。
    let delay = SESSION_ENTER_STAGGER * stagger.min(SESSION_ENTER_STAGGER_STEPS) as u32;
    if let Some(progress) = motion.enter_progress_with_delay(&motion_id, delay) {
        row = row.opacity(progress);
    }
    motion_item(motion, motion_id, row)
        .enter_offset(point(px(0.0), px(6.0)))
        .into_any_element()
}

fn sidebar_session_row(
    theme: &Theme,
    summary: &SessionSummary,
    active: bool,
    pinned_group: bool,
    force_hover: bool,
    marquee: SessionMarqueeFrame,
    cx: &mut Context<AppView>,
) -> Stateful<Div> {
    let id = summary.id.clone();
    let title = summary.title.clone();
    let time = relative_time(summary.updated_at);
    let unread = summary.unread;
    let row_group = SharedString::from(format!("session-row-{}", id));
    let id_for_click = id.clone();
    let id_for_hover = id.clone();
    let id_for_pin = id.clone();
    let id_for_more = id.clone();
    let id_for_archive = id.clone();

    div()
        .id(SharedString::from(format!("session-{}", id)))
        .group(row_group.clone())
        .flex()
        .items_center()
        .gap(px(5.0))
        .h(px(30.0))
        .px(px(4.0))
        .rounded(px(radius::XS))
        .bg(if active {
            theme.accent_soft
        } else if force_hover {
            theme.hover
        } else {
            transparent_black()
        })
        .when(!active, |element| {
            element.hover(|style| style.bg(theme.hover))
        })
        .cursor_pointer()
        .on_click(cx.listener(move |this, _, window, cx| {
            this.select_session(&id_for_click, window, cx);
        }))
        // 会话名跑马灯：进入播放，离开播完当前周期再收尾。
        .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
            this.sidebar_marquee_hover(&id_for_hover, *hovered, cx);
        }))
        .child(
            sidebar_icon_button(
                theme,
                SharedString::from(format!("pin-{id_for_pin}")),
                icons::PIN,
                if pinned_group {
                    theme.accent
                } else {
                    theme.text_faint
                },
                if pinned_group {
                    "取消置顶"
                } else {
                    "置顶"
                },
            )
            .opacity(if pinned_group || force_hover {
                1.0
            } else {
                0.0
            })
            .group_hover(row_group.clone(), |style| style.opacity(1.0))
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.toggle_pin(&id_for_pin, cx);
            })),
        )
        .child(session_title(
            theme,
            &title,
            active,
            &row_group,
            marquee,
        ))
        .child(
            div()
                .relative()
                .flex_none()
                .w(px(40.0))
                .flex()
                .items_center()
                .justify_end()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_end()
                        .when(force_hover, |element| element.opacity(0.0))
                        .group_hover(row_group.clone(), |style| style.opacity(0.0))
                        .child(if unread {
                            div()
                                .id(SharedString::from(format!("unread-dot-{id}")))
                                .size(px(7.0))
                                .rounded(px(999.0))
                                .bg(theme.accent)
                                .tooltip(effibuddy_ui::tooltip(
                                    theme,
                                    format!("未读 · {time}"),
                                ))
                                .into_any_element()
                        } else {
                            div()
                                .text_size(px(10.0))
                                .text_color(theme.text_faint)
                                .child(time)
                                .into_any_element()
                        }),
                )
                .child(
                    div()
                        .absolute()
                        .right(px(0.0))
                        .flex()
                        .items_center()
                        .gap(px(1.0))
                        .opacity(if force_hover { 1.0 } else { 0.0 })
                        .group_hover(row_group, |style| style.opacity(1.0))
                        .child(
                            sidebar_icon_button(
                                theme,
                                SharedString::from(format!("more-{id_for_more}")),
                                icons::MORE,
                                theme.text_faint,
                                "更多",
                            )
                            .on_click(cx.listener(
                                move |this, event: &gpui::ClickEvent, _, cx| {
                                    cx.stop_propagation();
                                    this.open_session_menu(
                                        &id_for_more,
                                        event.position(),
                                        cx,
                                    );
                                },
                            )),
                        )
                        .child(
                            sidebar_icon_button(
                                theme,
                                SharedString::from(format!("archive-{id_for_archive}")),
                                icons::ARCHIVE,
                                theme.text_faint,
                                "归档",
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.archive_session(&id_for_archive, cx);
                                },
                            )),
                        ),
                ),
        )
}

/// 会话名视口。溢出时右缘常显 0.5em 淡出（GPUI 没有 mask-image，用与行
/// 背景同色的渐变盖片等效实现）；跑马灯播放时双副本条带经 PaintShift 左
/// 移循环、两端遮罩；离开后条带播回 left=0，左缘遮罩收回。
fn session_title(
    theme: &Theme,
    title: &str,
    active: bool,
    row_group: &SharedString,
    frame: SessionMarqueeFrame,
) -> Div {
    let text_style = |element: Div| {
        element
            .text_size(px(SESSION_TITLE_SIZE))
            .font_weight(if active {
                gpui::FontWeight::MEDIUM
            } else {
                gpui::FontWeight::NORMAL
            })
            .text_color(if active { theme.text } else { theme.text_muted })
    };
    if !frame.overflow {
        return text_style(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .child(title.to_string()),
        );
    }

    // 遮罩渐变基色必须与行背景一致：静止=侧栏底色，行悬停时经 group_hover
    // 换成 hover 色，激活行恒为 accent_soft。
    let fade_base = if active { theme.accent_soft } else { theme.sidebar };
    let fade_gradient = |base: gpui::Hsla, inward: bool| {
        let (left, right) = if inward {
            (base.opacity(0.0), base)
        } else {
            (base, base.opacity(0.0))
        };
        linear_gradient(90.0, linear_color_stop(left, 0.0), linear_color_stop(right, 1.0))
    };

    div()
        .relative()
        .flex_1()
        .min_w(px(0.0))
        .flex()
        .items_center()
        .overflow_hidden()
        .child(PaintShift::new(
            point(px(frame.offset), px(0.0)),
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(SESSION_MARQUEE_GAP))
                .child(text_style(div().flex_none()).child(title.to_string()))
                .child(text_style(div().flex_none()).child(title.to_string())),
        ))
        .child(
            // 右缘淡出：静止与播放时都显示。
            div()
                .absolute()
                .top(px(0.0))
                .bottom(px(0.0))
                .right(px(0.0))
                .w(px(SESSION_TITLE_FADE_W))
                .bg(fade_gradient(fade_base, true))
                .when(!active, |element| {
                    element.group_hover(row_group.clone(), |style| {
                        style.bg(fade_gradient(theme.hover, true))
                    })
                }),
        )
        .when(frame.left_fade > 0.05, |element| {
            element.child(
                // 左缘淡出：播放中全宽，收尾时收回。
                div()
                    .absolute()
                    .top(px(0.0))
                    .bottom(px(0.0))
                    .left(px(0.0))
                    .w(px(frame.left_fade))
                    .bg(fade_gradient(fade_base, false))
                    .when(!active, |element| {
                        element.group_hover(row_group.clone(), |style| {
                            style.bg(fade_gradient(theme.hover, false))
                        })
                    }),
            )
        })
}

/// Open a project folder in the platform file manager.
fn open_in_file_manager(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = std::process::Command::new("explorer.exe");
        command.arg(path);
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = std::process::Command::new("open");
        command.arg(path);
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(path);
        command
    };
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn day_greeting() -> &'static str {
    match Local::now().hour() {
        5..=8 => "早上好",
        9..=11 => "上午好",
        12..=13 => "中午好",
        14..=17 => "下午好",
        18..=22 => "晚上好",
        _ => "夜深了",
    }
}

/// Suggested file name for an export. Titles can contain characters that are
/// illegal on some platforms, so keep only the readable part.
fn export_file_name(title: &str) -> String {
    let mut safe = String::new();
    for ch in title.chars().take(40) {
        if ch.is_control() || matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
            safe.push('_');
        } else {
            safe.push(ch);
        }
    }
    let safe = safe.trim().trim_matches('.').to_string();
    let safe = if safe.is_empty() {
        "session".to_string()
    } else {
        safe
    };
    format!(
        "EffiBuddy-{safe}-{}.zip",
        Local::now().format("%Y%m%d-%H%M")
    )
}

fn normalize_export_path(mut path: PathBuf) -> PathBuf {
    if !path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
    {
        path.set_extension("zip");
    }
    path
}

pub(crate) fn relative_time(time: DateTime<Utc>) -> String {
    let delta = Utc::now().signed_duration_since(time);
    if delta.num_minutes() < 1 {
        "刚刚".to_string()
    } else if delta.num_minutes() < 60 {
        format!("{} 分钟", delta.num_minutes())
    } else if delta.num_hours() < 24 {
        format!("{} 小时", delta.num_hours())
    } else if delta.num_days() < 7 {
        format!("{} 天", delta.num_days())
    } else {
        time.with_timezone(&Local).format("%m-%d").to_string()
    }
}

/// Model-visible wake-up for a finished background subagent. The UI renders
/// the synthetic message as a system notice, never as a user bubble.
fn format_task_notification(report: &SubagentReport) -> String {
    let status = match report.status {
        SubagentStatus::Completed => "completed",
        SubagentStatus::Failed => "failed",
        SubagentStatus::Cancelled => "cancelled",
        SubagentStatus::Lost => "lost",
        SubagentStatus::Running => "running",
    };
    let summary = match report.status {
        SubagentStatus::Completed => {
            format!(
                "Agent \"{}\" ({}) finished",
                report.name, report.description
            )
        }
        SubagentStatus::Failed => format!(
            "Agent \"{}\" ({}) failed: {}",
            report.name,
            report.description,
            report.error.as_deref().unwrap_or("unknown error")
        ),
        SubagentStatus::Lost => format!(
            "Agent \"{}\" ({}) was lost after a restart",
            report.name, report.description
        ),
        SubagentStatus::Cancelled => {
            format!(
                "Agent \"{}\" ({}) was stopped",
                report.name, report.description
            )
        }
        SubagentStatus::Running => {
            format!(
                "Agent \"{}\" ({}) is running",
                report.name, report.description
            )
        }
    };
    let mut result = report.report.clone();
    if result.chars().count() > 20_000 {
        result = result.chars().take(20_000).collect::<String>();
        result.push_str("\n\n[report truncated]");
    }
    if let Some(data) = &report.data {
        result.push_str("\n\n<agent_data>\n");
        result.push_str(&serde_json::to_string_pretty(data).unwrap_or_else(|_| data.to_string()));
        result.push_str("\n</agent_data>");
    }
    let escape = |text: &str| {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    format!(
        "<task-notification>\n\
<task-id>{agent_id}</task-id>\n\
<status>{status}</status>\n\
<summary>{summary}</summary>\n\
<note>This background subagent result was delivered by the runtime, not typed by the user. \
The user can open its full transcript from the Agents panel. Do not start a new task from it \
unless it is directly relevant to what the user asked.</note>\n\
<result>\n{result}\n</result>\n\
<usage>subagent_tokens: {tokens}; tool_uses: {tool_uses}; duration_ms: {duration_ms}</usage>\n\
</task-notification>",
        agent_id = escape(&report.agent_id),
        status = status,
        summary = escape(&summary),
        result = escape(&result),
        tokens = report.usage.total_tokens,
        tool_uses = report.tool_uses,
        duration_ms = report.duration_ms,
    )
}

fn git_status(workspace: &std::path::Path) -> Result<(Option<String>, Vec<GitFile>), String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["status", "--porcelain=v1", "-b", "--untracked-files=all"])
        .output()
        .map_err(|error| format!("无法执行 git：{error}"))?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if message.contains("not a git repository") {
            return Err("当前工作区不是 Git 仓库。".to_string());
        }
        return Err(if message.is_empty() {
            "git status 失败。".to_string()
        } else {
            message
        });
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(parse_git_status(&text))
}

fn git_diff(workspace: &std::path::Path, file: &GitFile) -> String {
    if file.is_untracked() {
        return format_untracked_file(&file.path, workspace);
    }
    let run = |cached: bool| -> Result<String, String> {
        let mut command = std::process::Command::new("git");
        command
            .arg("-C")
            .arg(workspace)
            .arg("diff")
            .arg("--no-color");
        if cached {
            command.arg("--cached");
        }
        command.arg("--").arg(&file.path);
        let output = command
            .output()
            .map_err(|error| format!("无法执行 git diff：{error}"))?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    };

    match run(false) {
        Ok(diff) if !diff.trim().is_empty() => diff,
        Ok(_) => match run(true) {
            Ok(diff) if !diff.trim().is_empty() => diff,
            Ok(_) => format!(
                "diff --git a/{} b/{}\n(没有可显示的文本 diff)\n",
                file.path, file.path
            ),
            Err(error) if !error.is_empty() => error,
            Err(_) => "没有可显示的 diff。".to_string(),
        },
        Err(error) if !error.is_empty() => error,
        Err(_) => "没有可显示的 diff。".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(
        id: &str,
        title: &str,
        group: PaletteGroup,
        keywords: &'static [&'static str],
    ) -> PaletteItem {
        PaletteItem {
            id: id.to_string(),
            title: title.to_string(),
            subtitle: String::new(),
            icon: icons::PLUS,
            group,
            keywords,
            recent: false,
            action: PaletteAction::NewSession,
        }
    }

    #[test]
    fn palette_score_ranks_prefix_above_substring_above_subsequence() {
        let row = item("cmd", "新建会话", PaletteGroup::Suggested, &[]);
        assert_eq!(palette_score("新建", &row), Some(120));
        assert_eq!(palette_score("建会", &row), Some(100));
        // Subsequence over latin titles.
        let latin = item("cmd", "New Session", PaletteGroup::Suggested, &[]);
        assert_eq!(palette_score("nwssn", &latin), Some(50));
        assert_eq!(palette_score("zzz", &latin), None);
    }

    #[test]
    fn palette_score_matches_english_keywords() {
        let row = item("cmd", "切换主题", PaletteGroup::Suggested, &["theme"]);
        assert_eq!(palette_score("theme", &row), Some(90));
        assert_eq!(palette_score("them", &row), Some(90));
        assert_eq!(palette_score("heme", &row), Some(80));
    }

    #[test]
    fn palette_score_skips_subsequence_for_han_queries() {
        let latin = item("cmd", "Settings", PaletteGroup::Suggested, &[]);
        // "stigs" is a latin subsequence and matches.
        assert_eq!(palette_score("stigs", &latin), Some(50));
        // A Han query must not subsequence-match latin titles…
        assert_eq!(palette_score("设计", &latin), None);
        // …but still matches by substring when the query appears verbatim.
        let cjk = item("cmd", "模型设置", PaletteGroup::Suggested, &[]);
        assert_eq!(palette_score("设置", &cjk), Some(100));
    }

    #[test]
    fn palette_score_empty_query_matches_everything() {
        let row = item("cmd", "设置", PaletteGroup::Suggested, &[]);
        assert_eq!(palette_score("", &row), Some(0));
        assert_eq!(palette_score("   ", &row), Some(0));
    }

    #[test]
    fn order_palette_items_pins_recents_above_natural_order() {
        let items = vec![
            item(
                "cmd-a",
                "新建会话",
                PaletteGroup::Suggested,
                &["new session"],
            ),
            item("cmd-b", "切换主题", PaletteGroup::Suggested, &["theme"]),
            item("cmd-c", "模型设置", PaletteGroup::Suggested, &[]),
        ];
        let ordered = order_palette_items(items, "", &["cmd-c".to_string()]);
        assert_eq!(ordered[0].id, "cmd-c");
        assert!(ordered[0].recent);
        assert_eq!(ordered[0].group, PaletteGroup::Recent);
        assert_eq!(ordered[1].id, "cmd-a");
        assert_eq!(ordered[2].id, "cmd-b");
        assert!(!ordered[1].recent);
    }

    #[test]
    fn order_palette_items_uses_mru_order_for_pins() {
        let items = vec![
            item("cmd-a", "新建会话", PaletteGroup::Suggested, &[]),
            item("cmd-b", "切换主题", PaletteGroup::Suggested, &[]),
        ];
        let ordered = order_palette_items(items, "", &["cmd-b".to_string(), "cmd-a".to_string()]);
        assert_eq!(ordered[0].id, "cmd-b");
        assert_eq!(ordered[1].id, "cmd-a");
        assert_eq!(ordered[1].group, PaletteGroup::Recent);
    }

    #[test]
    fn order_palette_items_sorts_by_score_for_queries() {
        let items = vec![
            item(
                "cmd-a",
                "新建会话",
                PaletteGroup::Suggested,
                &["new session"],
            ),
            item("cmd-d", "New Session", PaletteGroup::Suggested, &[]),
        ];
        // "session" substring-matches the keyword (80) but prefix-ish substring
        // matches the latin title (100), so the title row comes first.
        let ordered = order_palette_items(items, "session", &[]);
        assert_eq!(ordered[0].id, "cmd-d");
        assert_eq!(ordered[1].id, "cmd-a");
    }

    #[test]
    fn order_palette_items_drops_recent_that_does_not_match_query() {
        let items = vec![
            item(
                "cmd-a",
                "新建会话",
                PaletteGroup::Suggested,
                &["new session"],
            ),
            item("cmd-b", "切换主题", PaletteGroup::Suggested, &["theme"]),
        ];
        let ordered = order_palette_items(items, "theme", &["cmd-a".to_string()]);
        assert_eq!(ordered.len(), 1);
        assert_eq!(ordered[0].id, "cmd-b");
    }

    #[test]
    fn order_palette_items_caps_pinned_recents() {
        let items: Vec<PaletteItem> = (0..8)
            .map(|index| {
                item(
                    &format!("cmd-{index}"),
                    &format!("命令 {index}"),
                    PaletteGroup::Suggested,
                    &[],
                )
            })
            .collect();
        let recent: Vec<String> = (0..8).map(|index| format!("cmd-{index}")).collect();
        let ordered = order_palette_items(items, "", &recent);
        assert_eq!(ordered.len(), 8);
        let pinned = ordered
            .iter()
            .take_while(|row| row.group == PaletteGroup::Recent)
            .count();
        assert_eq!(pinned, PALETTE_MAX_RECENT);
    }

    #[test]
    fn han_detection_and_subsequence_helpers() {
        assert!(has_han("新建"));
        assert!(has_han("mixed 设"));
        assert!(!has_han("settings"));
        assert!(is_subsequence("nws", "new session"));
        assert!(!is_subsequence("nxs", "new session"));
    }
}

/// Stress preview knobs: number of finished long turns, and the byte budget
/// of the live streaming backlog message.
const STRESS_TURNS: usize = 8;

/// Deterministic ~6KB markdown body (CJK paragraphs, lists, table, fences) so
/// stress runs are reproducible and chunk boundaries stay interesting.
fn stress_markdown_turn(turn: usize) -> String {
    let mut out = String::with_capacity(8 * 1024);
    out.push_str(&format!("## 压测回合 {turn}\n\n"));
    for section in 0..6 {
        out.push_str(&format!("### 小节 {turn}.{section}\n\n"));
        out.push_str(
            "系统在渲染管线上以每帧一次的方式重建虚拟列表，只有视口附近的块会被重新排版；分块按空白行切分并保持代码围栏完整，已定稿的块在帧间复用排版高度。".repeat(5).as_str(),
        );
        out.push_str("\n\n");
        out.push_str(&format!(
            "- 要点 {section}.1：打字机循环保持每帧调度，不降帧\n- 要点 {section}.2：平滑滚动以指数缓动追赶目标偏移\n- 要点 {section}.3：工具卡片仅在展开时构建 JSON\n\n"
        ));
        out.push_str(&format!(
            "```rust\nfn frame_{turn}_{section}(state: &mut State) {{\n    state.advance_typewriter(160);\n    state.glide_scroll(dt);\n    state.rebuild_visible_chunks();\n}}\n```\n\n"
        ));
    }
    out.push_str("| 项目 | 数值 |\n|---|---|\n| 帧预算 | 8.3ms |\n| 缓动时间常数 | 85ms |\n\n");
    out
}
