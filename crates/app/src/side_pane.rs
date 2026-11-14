//! The right-hand tabbed side pane.
//!
//! Mirrors ZCode's side pane: a horizontal tab strip with closable tabs, a
//! launcher, a searchable overview of open/recently closed tabs, a resizable
//! panel and several content panes (plan, agents, subagent transcripts, files
//! and git changes). The main transcript never gets replaced by a subagent.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Instant;

use effibuddy_agent_core::{AgentVisibility, SubagentStatus, TodoStatus, Usage, lock_session};
use effibuddy_ui::{
    BadgeTone, Button, ButtonSize, ButtonVariant, IconButton, Spinner, TextInputOptions,
    TextInputState, TextInputView, Theme, badge, disclosure_chevron, divider, ease_out_cubic,
    empty_state, icons, overlay_motion_duration, radius, section_label, shadow_float, small_badge,
};
use gpui::{
    Animation, AnimationExt, AnyElement, Context, Entity, HighlightStyle, IntoElement, MouseButton,
    Point, SharedString, StyledText, Window, anchored, div, point, prelude::*, px, svg,
    transparent_black, uniform_list,
};
use similar::{ChangeTag, TextDiff};

use crate::app::{AgentTask, AppView, PendingApproval};
use crate::chat::{
    AgentCard, ChatList, RunSnapshot, agent_color, format_elapsed, render_chat_item,
};
use crate::config::AppConfig;
use crate::smooth::smooth_overlay;

pub(crate) const MIN_SIDE_PANE_WIDTH: f32 = 260.0;
pub(crate) const MAX_SIDE_PANE_WIDTH: f32 = 620.0;

/// 渲染变更页签 diff 时最多保留的原始行数，超出部分截断并提示。
const MAX_DIFF_RENDER_LINES: usize = 6_000;

/// 折叠连续未修改行时，每侧保留的上下文行数。
const FOLD_CONTEXT_LINES: usize = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SidePaneTabKind {
    Plan,
    Agents,
    Files,
    Changes,
    Agent(String),
}

#[derive(Clone, Debug)]
pub(crate) struct SidePaneTab {
    pub id: String,
    pub kind: SidePaneTabKind,
    pub title: String,
    pub icon: &'static str,
    pub closable: bool,
}

impl SidePaneTab {
    pub fn plan() -> Self {
        Self {
            id: "plan".to_string(),
            kind: SidePaneTabKind::Plan,
            title: "计划".to_string(),
            icon: icons::CLOCK,
            closable: true,
        }
    }

    pub fn agents() -> Self {
        Self {
            id: "agents".to_string(),
            kind: SidePaneTabKind::Agents,
            title: "子代理".to_string(),
            icon: icons::SPARKLES,
            closable: true,
        }
    }

    pub fn files() -> Self {
        Self {
            id: "files".to_string(),
            kind: SidePaneTabKind::Files,
            title: "文件".to_string(),
            icon: icons::FOLDER,
            closable: true,
        }
    }

    pub fn changes() -> Self {
        Self {
            id: "changes".to_string(),
            kind: SidePaneTabKind::Changes,
            title: "变更".to_string(),
            icon: icons::BRANCH,
            closable: true,
        }
    }

    pub fn agent(agent_id: &str, profile: &str, description: &str) -> Self {
        let title = if description.trim().is_empty() {
            profile.to_string()
        } else {
            description.to_string()
        };
        Self {
            id: format!("agent:{agent_id}"),
            kind: SidePaneTabKind::Agent(agent_id.to_string()),
            title,
            icon: icons::SPARKLES,
            closable: true,
        }
    }

    /// Only shell tabs survive a restart. Live subagent transcripts are tied
    /// to a running harness and are intentionally not restored.
    pub fn persist_key(&self) -> Option<&'static str> {
        match self.kind {
            SidePaneTabKind::Plan => Some("plan"),
            SidePaneTabKind::Agents => Some("agents"),
            SidePaneTabKind::Files => Some("files"),
            SidePaneTabKind::Changes => Some("changes"),
            SidePaneTabKind::Agent(_) => None,
        }
    }
}

pub(crate) fn tab_from_key(key: &str) -> Option<SidePaneTab> {
    match key {
        "plan" => Some(SidePaneTab::plan()),
        "agents" => Some(SidePaneTab::agents()),
        "files" => Some(SidePaneTab::files()),
        "changes" => Some(SidePaneTab::changes()),
        _ => None,
    }
}

pub(crate) struct AgentView {
    pub chat: ChatList,
    pub dirty: bool,
}

#[derive(Clone, Debug)]
pub(crate) enum SideOverlay {
    AddMenu(Point<gpui::Pixels>),
    Overview(Point<gpui::Pixels>),
}

#[derive(Clone, Debug)]
pub(crate) struct FileEntry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
}

pub(crate) struct FilePreview {
    pub path: PathBuf,
    /// Precomputed line slice (capped at [`FILE_PREVIEW_MAX_LINES`]) so the
    /// virtualized preview can render arbitrary visible rows without
    /// re-splitting the file on every frame.
    pub lines: Rc<Vec<SharedString>>,
    pub truncated: bool,
    pub binary: bool,
}

pub(crate) struct FileTreeState {
    pub root: PathBuf,
    pub expanded: HashSet<PathBuf>,
    pub entries: HashMap<PathBuf, Vec<FileEntry>>,
    pub preview: Option<FilePreview>,
    pub error: Option<String>,
    /// Cached flattened rows; rebuilt whenever the tree changes.
    rows_cache: Option<Rc<Vec<FileRow>>>,
}

/// File previews show at most this many lines.
const FILE_PREVIEW_MAX_LINES: usize = 4_000;

impl FileTreeState {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            expanded: HashSet::new(),
            entries: HashMap::new(),
            preview: None,
            error: None,
            rows_cache: None,
        }
    }

    pub fn set_root(&mut self, root: PathBuf) {
        if self.root == root {
            return;
        }
        self.root = root;
        self.expanded.clear();
        self.entries.clear();
        self.preview = None;
        self.error = None;
        self.rows_cache = None;
    }

    pub fn refresh(&mut self) {
        self.entries.clear();
        self.preview = None;
        self.error = None;
        self.rows_cache = None;
        self.load_dir(self.root.clone());
    }

    pub fn load_dir(&mut self, path: PathBuf) {
        let mut entries = Vec::new();
        match std::fs::read_dir(&path) {
            Ok(read_dir) => {
                for entry in read_dir.flatten().take(600) {
                    let entry_path = entry.path();
                    let name = entry.file_name().to_string_lossy().to_string();
                    let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
                    entries.push(FileEntry {
                        path: entry_path,
                        name,
                        is_dir,
                    });
                }
                entries.sort_by(|left, right| {
                    right
                        .is_dir
                        .cmp(&left.is_dir)
                        .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
                });
                self.error = None;
            }
            Err(error) => {
                self.error = Some(format!("无法读取 {}：{error}", path.display()));
            }
        }
        self.entries.insert(path, entries);
        self.rows_cache = None;
    }

    pub fn toggle_dir(&mut self, path: &Path) {
        let path = path.to_path_buf();
        if !self.expanded.remove(&path) {
            self.expanded.insert(path.clone());
            if !self.entries.contains_key(&path) {
                self.load_dir(path);
            } else {
                self.rows_cache = None;
            }
        } else {
            self.rows_cache = None;
        }
    }

    pub fn open_file(&mut self, path: &Path) {
        let mut file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) => {
                self.error = Some(format!("无法打开 {}：{error}", path.display()));
                return;
            }
        };
        let limit = 256 * 1024;
        let mut buffer = vec![0u8; limit + 1];
        let read = match file.read(&mut buffer) {
            Ok(read) => read,
            Err(error) => {
                self.error = Some(format!("无法读取 {}：{error}", path.display()));
                return;
            }
        };
        buffer.truncate(read);
        let truncated = buffer.len() > limit;
        if truncated {
            buffer.truncate(limit);
        }
        let binary = buffer.iter().take(8_192).any(|byte| *byte == 0);
        let text = if binary {
            "二进制文件不提供预览。".to_string()
        } else {
            String::from_utf8_lossy(&buffer).to_string()
        };
        let lines = Rc::new(
            text.lines()
                .take(FILE_PREVIEW_MAX_LINES)
                .map(|line| SharedString::from(line.to_string()))
                .collect::<Vec<_>>(),
        );
        self.preview = Some(FilePreview {
            path: path.to_path_buf(),
            lines,
            truncated,
            binary,
        });
        self.error = None;
    }

    /// Flattened visible rows, cached so the virtualized tree renders without
    /// re-walking the whole directory state every frame.
    pub fn visible_rows(&mut self) -> Rc<Vec<FileRow>> {
        if let Some(rows) = &self.rows_cache {
            return rows.clone();
        }
        let mut rows = Vec::new();
        self.push_rows(self.root.clone(), 0, &mut rows);
        let rows = Rc::new(rows);
        self.rows_cache = Some(Rc::clone(&rows));
        rows
    }

    fn push_rows(&self, path: PathBuf, depth: usize, rows: &mut Vec<FileRow>) {
        let Some(entries) = self.entries.get(&path) else {
            return;
        };
        for entry in entries {
            let expanded = entry.is_dir && self.expanded.contains(&entry.path);
            rows.push(FileRow {
                path: entry.path.clone(),
                name: entry.name.clone(),
                is_dir: entry.is_dir,
                depth,
                expanded,
            });
            if expanded {
                self.push_rows(entry.path.clone(), depth + 1, rows);
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct FileRow {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub depth: usize,
    pub expanded: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct GitFile {
    pub path: String,
    pub index_status: char,
    pub worktree_status: char,
}

impl GitFile {
    pub fn status_label(&self) -> String {
        if self.index_status == '?' {
            return "??".to_string();
        }
        if self.index_status == '!' {
            return "!!".to_string();
        }
        let mut value = String::new();
        if self.index_status != ' ' {
            value.push(self.index_status);
        }
        if self.worktree_status != ' ' {
            value.push(self.worktree_status);
        }
        if value.is_empty() {
            "M".to_string()
        } else {
            value
        }
    }

    pub fn is_untracked(&self) -> bool {
        self.index_status == '?' && self.worktree_status == '?'
    }
}

#[derive(Default)]
pub(crate) struct GitState {
    pub loading: bool,
    pub error: Option<String>,
    pub branch: Option<String>,
    pub files: Vec<GitFile>,
    pub selected: Option<usize>,
    pub diff: Option<String>,
    pub diff_loading: bool,
    /// Cached parse of the shown diff, keyed by content hash. The changes
    /// pane used to re-run parsing + word-level diffs over up to
    /// [`MAX_DIFF_RENDER_LINES`] lines on every frame.
    pub diff_ir_cache: Option<(u64, Rc<DiffIr>)>,
}

pub(crate) struct SidePaneState {
    pub open: bool,
    pub width: f32,
    pub tabs: Vec<SidePaneTab>,
    pub active_id: String,
    pub recent_closed: Vec<SidePaneTab>,
    pub overlay: Option<SideOverlay>,
    pub agent_views: HashMap<String, AgentView>,
    pub file_tree: FileTreeState,
    pub git: GitState,
    /// 变更页签里已展开的折叠区间键（文件路径 + 段/hunk/区间索引）。
    pub expanded_hunks: HashSet<String>,
    pub search_state: Entity<TextInputState>,
    pub search_view: Entity<TextInputView>,
    pub resizing: bool,
}

impl SidePaneState {
    pub fn new(cx: &mut Context<AppView>, config: &AppConfig, workspace: PathBuf) -> Self {
        let mut tabs = Vec::new();
        for key in &config.side_bar_tabs {
            if let Some(tab) = tab_from_key(key) {
                if !tabs.iter().any(|item: &SidePaneTab| item.id == tab.id) {
                    tabs.push(tab);
                }
            }
        }
        if tabs.is_empty() {
            tabs.push(SidePaneTab::plan());
        }
        let active_id = if tabs.iter().any(|tab| tab.id == config.side_bar_active) {
            config.side_bar_active.clone()
        } else {
            tabs[0].id.clone()
        };

        let search_state = cx.new(|cx| {
            TextInputState::new(
                cx,
                TextInputOptions::default()
                    .multiline(false)
                    .lines(1, 1)
                    .placeholder("搜索页签…"),
            )
        });
        let search_view = cx.new(|cx| TextInputView::new(search_state.clone(), cx).frameless(true));

        let mut file_tree = FileTreeState::new(workspace);
        file_tree.load_dir(file_tree.root.clone());

        Self {
            open: config.side_bar_open,
            width: config
                .side_bar_width
                .clamp(MIN_SIDE_PANE_WIDTH, MAX_SIDE_PANE_WIDTH),
            tabs,
            active_id,
            recent_closed: Vec::new(),
            overlay: None,
            agent_views: HashMap::new(),
            file_tree,
            git: GitState::default(),
            expanded_hunks: HashSet::new(),
            search_state,
            search_view,
            resizing: false,
        }
    }

    pub fn active(&self) -> Option<&SidePaneTab> {
        self.tabs.iter().find(|tab| tab.id == self.active_id)
    }

    pub fn tab(&self, id: &str) -> Option<&SidePaneTab> {
        self.tabs.iter().find(|tab| tab.id == id)
    }

    pub fn has_tab(&self, id: &str) -> bool {
        self.tabs.iter().any(|tab| tab.id == id)
    }

    pub fn open_tab(&mut self, tab: SidePaneTab) {
        let id = tab.id.clone();
        if !self.has_tab(&id) {
            self.tabs.push(tab);
        }
        self.active_id = id;
        self.open = true;
    }

    pub fn activate(&mut self, id: &str) {
        if self.has_tab(id) {
            self.active_id = id.to_string();
            self.open = true;
        }
    }

    pub fn close_tab(&mut self, id: &str) {
        let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else {
            return;
        };
        let removed = self.tabs.remove(index);
        self.recent_closed.retain(|tab| tab.id != removed.id);
        self.recent_closed.insert(0, removed);
        self.recent_closed.truncate(10);
        if self.active_id == id {
            if let Some(tab) = self.tabs.get(index).or_else(|| self.tabs.last()) {
                self.active_id = tab.id.clone();
            } else {
                self.active_id.clear();
            }
        }
    }

    pub fn ensure_file_root(&mut self, workspace: PathBuf) {
        self.file_tree.set_root(workspace);
    }

    pub fn persist_into(&self, config: &mut AppConfig) {
        config.side_bar_open = self.open;
        config.side_bar_width = self.width.clamp(MIN_SIDE_PANE_WIDTH, MAX_SIDE_PANE_WIDTH);
        config.side_bar_tabs = self
            .tabs
            .iter()
            .filter_map(|tab| tab.persist_key().map(str::to_string))
            .collect();
        config.side_bar_active = self.active_id.clone();
    }
}

// --- git helpers -----------------------------------------------------------

pub(crate) fn parse_git_status(output: &str) -> (Option<String>, Vec<GitFile>) {
    let mut branch = None;
    let mut files = Vec::new();
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            let name = rest
                .split("...")
                .next()
                .unwrap_or(rest)
                .split(' ')
                .next()
                .unwrap_or(rest)
                .trim();
            if !name.is_empty() && name != "No" {
                branch = Some(name.to_string());
            }
            continue;
        }
        if line.len() < 4 {
            continue;
        }
        let mut chars = line.chars();
        let index_status = chars.next().unwrap_or(' ');
        let worktree_status = chars.next().unwrap_or(' ');
        let path = line[3..].trim().to_string();
        if path.is_empty() {
            continue;
        }
        let path = path
            .split_once(" -> ")
            .map(|(_, new_path)| new_path)
            .unwrap_or(&path)
            .trim()
            .to_string();
        files.push(GitFile {
            path,
            index_status,
            worktree_status,
        });
    }
    (branch, files)
}

pub(crate) fn format_untracked_file(path: &str, workspace: &Path) -> String {
    let full = workspace.join(path);
    let Ok(file) = std::fs::File::open(&full) else {
        return format!("diff --git a/{path} b/{path}\n(new file, unable to read)\n");
    };
    let mut buffer = String::new();
    let _ = file.take(256 * 1024).read_to_string(&mut buffer);
    let mut result = format!(
        "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n"
    );
    for line in buffer.lines().take(4_000) {
        result.push('+');
        result.push_str(line);
        result.push('\n');
    }
    result
}

// --- unified diff 解析与词级分段 ---------------------------------------------

/// 行内一个着色片段的类别。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WordTag {
    /// 与配对行相同的部分。
    Equal,
    /// 相对配对行新增/删除的部分，需要强调。
    Changed,
}

/// 行内内容的一个着色片段。
#[derive(Clone, Debug, PartialEq, Eq)]
struct WordSegment {
    tag: WordTag,
    text: String,
}

/// 一行内容的词级信息。
#[derive(Clone, Debug, PartialEq, Eq)]
enum LineDiff {
    /// 无词级信息（独立增删或整行替换），整行着色。
    Plain,
    /// 行内按词分段着色。
    Words(Vec<WordSegment>),
}

/// unified diff 行类别。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiffRowKind {
    Delete,
    Insert,
    Context,
    /// `\ No newline at end of file` 等备注行。
    Note,
}

/// 解析后的 diff 行，带双侧行号。
#[derive(Clone, Debug, PartialEq, Eq)]
struct DiffRow {
    kind: DiffRowKind,
    text: String,
    old_no: Option<u32>,
    new_no: Option<u32>,
}

impl DiffRow {
    fn delete(old_no: u32, text: &str) -> Self {
        Self {
            kind: DiffRowKind::Delete,
            text: text.to_string(),
            old_no: Some(old_no),
            new_no: None,
        }
    }

    fn insert(new_no: u32, text: &str) -> Self {
        Self {
            kind: DiffRowKind::Insert,
            text: text.to_string(),
            old_no: None,
            new_no: Some(new_no),
        }
    }

    fn context(old_no: u32, new_no: u32, text: &str) -> Self {
        Self {
            kind: DiffRowKind::Context,
            text: text.to_string(),
            old_no: Some(old_no),
            new_no: Some(new_no),
        }
    }

    fn note(text: &str) -> Self {
        Self {
            kind: DiffRowKind::Note,
            text: text.to_string(),
            old_no: None,
            new_no: None,
        }
    }
}

/// hunk 内的渲染单元：一行，或折叠的连续未修改区间。
#[derive(Clone, Debug, PartialEq, Eq)]
enum HunkItem {
    Row(DiffRow, LineDiff),
    /// 折叠的未修改区间：count 为折叠行数，hidden 为展开时渲染的内容。
    Folded {
        count: u32,
        hidden: Vec<(DiffRow, LineDiff)>,
    },
}

/// 一个文件段的可缓存渲染单元：头部行 + 每个 hunk 的「头 + 折叠后渲染单元」。
/// 展开状态（[`SidePaneState::expanded_hunks`]）在渲染时应用，不参与缓存键。
#[derive(Clone)]
struct DiffSectionIr {
    header: Vec<String>,
    hunks: Vec<(String, Vec<HunkItem>)>,
}

/// 整个 diff 的缓存解析结果。
#[derive(Clone)]
pub(crate) struct DiffIr {
    sections: Vec<DiffSectionIr>,
    /// diff 超过 [`MAX_DIFF_RENDER_LINES`] 行被截断。
    truncated: bool,
}

/// 一个 hunk；`header` 为空表示无 `@@` 头的伪 hunk（如未跟踪文件的合成 diff）。
#[derive(Clone, Debug, PartialEq, Eq)]
struct DiffHunk {
    header: String,
    rows: Vec<DiffRow>,
}

/// 单个文件的 diff 段：头部行 + hunk 列表。
#[derive(Clone, Debug, PartialEq, Eq)]
struct DiffFileSection {
    /// `diff --git`/`index`/`---`/`+++` 等头部行。
    header: Vec<String>,
    /// 二进制文件（Binary files differ / GIT binary patch）。
    binary: bool,
    hunks: Vec<DiffHunk>,
}

/// 一个文件段在解析过程中的附加状态。
struct SectionParse {
    rendered: DiffFileSection,
    /// 是否已出现过 `+++ ` 头（其后同前缀行视作合成 diff 正文）。
    saw_plus_header: bool,
    /// 是否出现过 `@@` hunk 头（决定散行按新增正文还是备注处理）。
    saw_hunk_header: bool,
}

impl SectionParse {
    /// 无文件头的兜底段：承载 diff 命令失败时的错误/提示文本。
    fn fallback() -> Self {
        Self {
            rendered: DiffFileSection {
                header: Vec::new(),
                binary: false,
                hunks: Vec::new(),
            },
            saw_plus_header: false,
            saw_hunk_header: false,
        }
    }

    /// 把 hunk 内无法归类的散行归入当前段（无 `@@` 头的合成 diff 全部按
    /// 新增行处理，其余按备注行处理）。
    fn push_stray_row(
        &mut self,
        line: &str,
        hunk: &mut Option<DiffHunk>,
        old_no: &mut u32,
        new_no: &mut u32,
    ) {
        if hunk.is_none() {
            *old_no = 1;
            *new_no = 1;
            *hunk = Some(DiffHunk {
                header: String::new(),
                rows: Vec::new(),
            });
        }
        let Some(hunk) = hunk.as_mut() else {
            return;
        };
        if self.saw_plus_header && !self.saw_hunk_header {
            let text = line.strip_prefix('+').unwrap_or(line);
            hunk.rows.push(DiffRow::insert(*new_no, text));
            *new_no += 1;
        } else {
            hunk.rows.push(DiffRow::note(line));
        }
    }
}

/// 解析 unified diff 文本为按文件分段的 hunk 列表。
///
/// 支持三类输入：标准 `git diff`（带 `@@` 头与上下文行）、无 `@@` 头的合成
/// diff（如未跟踪文件的全文新增）、以及错误/提示文本（渲染为无行号备注）。
/// 二进制文件只保留头部行并标记 `binary`。
fn parse_unified_hunks(diff: &str) -> Vec<DiffFileSection> {
    let mut sections: Vec<DiffFileSection> = Vec::new();
    let mut section: Option<SectionParse> = None;
    let mut hunk: Option<DiffHunk> = None;
    let mut old_no = 1u32;
    let mut new_no = 1u32;

    let flush_hunk = |section: &mut Option<SectionParse>, hunk: &mut Option<DiffHunk>| {
        if let Some(hunk) = hunk.take()
            && let Some(section) = section.as_mut()
        {
            section.rendered.hunks.push(hunk);
        }
    };

    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            flush_hunk(&mut section, &mut hunk);
            if let Some(section) = section.take() {
                sections.push(section.rendered);
            }
            section = Some(SectionParse {
                rendered: DiffFileSection {
                    header: vec![line.to_string()],
                    binary: false,
                    hunks: Vec::new(),
                },
                saw_plus_header: false,
                saw_hunk_header: false,
            });
            continue;
        }
        if let Some((old_start, new_start)) = parse_hunk_header(line) {
            flush_hunk(&mut section, &mut hunk);
            let section = section.get_or_insert_with(SectionParse::fallback);
            section.saw_hunk_header = true;
            old_no = old_start;
            new_no = new_start;
            hunk = Some(DiffHunk {
                header: line.to_string(),
                rows: Vec::new(),
            });
            continue;
        }
        if hunk.is_some() {
            let handled = match line.as_bytes().first() {
                Some(b'\\') => {
                    if let Some(hunk) = hunk.as_mut() {
                        hunk.rows.push(DiffRow::note(line));
                    }
                    true
                }
                Some(b'+') => {
                    if let Some(hunk) = hunk.as_mut() {
                        hunk.rows.push(DiffRow::insert(new_no, &line[1..]));
                    }
                    new_no += 1;
                    true
                }
                Some(b'-') => {
                    if let Some(hunk) = hunk.as_mut() {
                        hunk.rows.push(DiffRow::delete(old_no, &line[1..]));
                    }
                    old_no += 1;
                    true
                }
                Some(b' ') | None => {
                    let text = if line.is_empty() { "" } else { &line[1..] };
                    if let Some(hunk) = hunk.as_mut() {
                        hunk.rows.push(DiffRow::context(old_no, new_no, text));
                    }
                    old_no += 1;
                    new_no += 1;
                    true
                }
                _ => false,
            };
            if handled {
                continue;
            }
            flush_hunk(&mut section, &mut hunk);
        }
        match section.as_mut() {
            Some(section) => {
                if is_diff_header_line(line, section.saw_plus_header) {
                    if line.starts_with("+++ ") {
                        section.saw_plus_header = true;
                    }
                    if line.starts_with("Binary files ") || line == "GIT binary patch" {
                        section.rendered.binary = true;
                    }
                    section.rendered.header.push(line.to_string());
                } else {
                    section.push_stray_row(line, &mut hunk, &mut old_no, &mut new_no);
                }
            }
            None => {
                // 文件段之前的散行（如 diff 命令的错误提示）。
                let mut fallback = SectionParse::fallback();
                fallback.push_stray_row(line, &mut hunk, &mut old_no, &mut new_no);
                if let Some(hunk) = hunk.take() {
                    fallback.rendered.hunks.push(hunk);
                }
                sections.push(fallback.rendered);
            }
        }
    }
    flush_hunk(&mut section, &mut hunk);
    if let Some(section) = section.take() {
        sections.push(section.rendered);
    }
    sections
}

/// 判断无 hunk 时的行是否属于文件头。`+++ ` 只在首次出现时算头部，其后
/// 同前缀行是未跟踪文件合成 diff 的正文。
fn is_diff_header_line(line: &str, saw_plus_header: bool) -> bool {
    const HEADER_PREFIXES: [&str; 12] = [
        "index ",
        "old mode ",
        "new mode ",
        "new file mode ",
        "deleted file mode ",
        "similarity index ",
        "dissimilarity index ",
        "rename from ",
        "rename to ",
        "copy from ",
        "copy to ",
        "--- ",
    ];
    if line.starts_with("+++ ") {
        return !saw_plus_header;
    }
    if line.starts_with("Binary files ") || line == "GIT binary patch" || line.starts_with('\\') {
        return true;
    }
    HEADER_PREFIXES
        .iter()
        .any(|prefix| line.starts_with(prefix))
}

/// 解析 `@@ -a,b +c,d @@` 头，返回 (旧起始行, 新起始行)。
fn parse_hunk_header(line: &str) -> Option<(u32, u32)> {
    let body = line.strip_prefix("@@")?.trim_start();
    let body = body.split_once("@@").map_or(body, |(ranges, _)| ranges);
    let mut tokens = body.split_whitespace();
    let old = parse_hunk_range(tokens.next()?, '-')?;
    let new = parse_hunk_range(tokens.next()?, '+')?;
    Some((old.0, new.0))
}

/// 解析 `-a,b` / `+c,d` 区间，缺省计数为 1。
fn parse_hunk_range(token: &str, sign: char) -> Option<(u32, u32)> {
    let body = token.strip_prefix(sign)?;
    let (start, count) = body.split_once(',').unwrap_or((body, "1"));
    Some((start.parse().ok()?, count.parse().ok()?))
}

/// 为 hunk 行计算词级分段：相邻的删除/新增行块做行内 diff；相似行按词
/// 强调，整行替换或独立的增删行整行着色。
fn annotate_hunk_rows(rows: &[DiffRow]) -> Vec<(DiffRow, LineDiff)> {
    let mut out: Vec<(DiffRow, LineDiff)> = rows
        .iter()
        .map(|row| (row.clone(), LineDiff::Plain))
        .collect();
    let mut index = 0;
    while index < rows.len() {
        if rows[index].kind != DiffRowKind::Delete {
            index += 1;
            continue;
        }
        let delete_start = index;
        while index < rows.len() && rows[index].kind == DiffRowKind::Delete {
            index += 1;
        }
        let insert_start = index;
        while index < rows.len() && rows[index].kind == DiffRowKind::Insert {
            index += 1;
        }
        let insert_end = index;
        if insert_start == insert_end {
            continue;
        }
        let deleted: Vec<&str> = rows[delete_start..insert_start]
            .iter()
            .map(|row| row.text.as_str())
            .collect();
        let inserted: Vec<&str> = rows[insert_start..insert_end]
            .iter()
            .map(|row| row.text.as_str())
            .collect();
        let (delete_lines, insert_lines) = word_diff_segments(&deleted, &inserted);
        for (offset, line) in delete_lines.into_iter().enumerate() {
            if let Some(line) = line {
                out[delete_start + offset].1 = line;
            }
        }
        for (offset, line) in insert_lines.into_iter().enumerate() {
            if let Some(line) = line {
                out[insert_start + offset].1 = line;
            }
        }
    }
    out
}

/// 对一对相邻的删除/新增行块计算词级分段。
///
/// 行块整体交给 `similar` 的行级 diff，再做第二层行内 diff；返回值与输入
/// 行一一对应，行级 diff 判定为“相等”的行返回 `None`（渲染时保持整行
/// 着色）。
fn word_diff_segments(
    deleted: &[&str],
    inserted: &[&str],
) -> (Vec<Option<LineDiff>>, Vec<Option<LineDiff>>) {
    let mut delete_out: Vec<Option<LineDiff>> = vec![None; deleted.len()];
    let mut insert_out: Vec<Option<LineDiff>> = vec![None; inserted.len()];
    if deleted.is_empty() {
        let insert_out = inserted.iter().map(|_| Some(LineDiff::Plain)).collect();
        return (delete_out, insert_out);
    }
    if inserted.is_empty() {
        let delete_out = deleted.iter().map(|_| Some(LineDiff::Plain)).collect();
        return (delete_out, insert_out);
    }
    let mut old_text = deleted.join("\n");
    old_text.push('\n');
    let mut new_text = inserted.join("\n");
    new_text.push('\n');
    let diff = TextDiff::from_lines(&old_text, &new_text);
    for op in diff.ops() {
        for change in diff.iter_inline_changes(op) {
            let mut segments: Vec<WordSegment> = Vec::new();
            for (emphasized, value) in change.iter_strings_lossy() {
                let text = value.trim_end_matches(['\n', '\r']).to_string();
                if text.is_empty() {
                    continue;
                }
                let tag = if emphasized {
                    WordTag::Changed
                } else {
                    WordTag::Equal
                };
                match segments.last_mut() {
                    Some(last) if last.tag == tag => last.text.push_str(&text),
                    _ => segments.push(WordSegment { tag, text }),
                }
            }
            if segments.is_empty() {
                continue;
            }
            let has_equal = segments.iter().any(|segment| segment.tag == WordTag::Equal);
            let has_changed = segments
                .iter()
                .any(|segment| segment.tag == WordTag::Changed);
            let line = if has_equal && has_changed {
                LineDiff::Words(segments)
            } else {
                // 全等（独立增删行）或全变（整行替换）都整行着色。
                LineDiff::Plain
            };
            match change.tag() {
                ChangeTag::Delete => {
                    if let Some(index) = change.old_index()
                        && index < delete_out.len()
                    {
                        delete_out[index] = Some(line);
                    }
                }
                ChangeTag::Insert => {
                    if let Some(index) = change.new_index()
                        && index < insert_out.len()
                    {
                        insert_out[index] = Some(line);
                    }
                }
                ChangeTag::Equal => {}
            }
        }
    }
    (delete_out, insert_out)
}

/// 把 hunk 行折叠为渲染单元：连续未修改行超过两侧保留之和时，中间部分
/// 折叠为「⋯ N 行未修改」，点击可展开。
fn fold_hunk_rows(rows: &[(DiffRow, LineDiff)]) -> Vec<HunkItem> {
    let mut items: Vec<HunkItem> = Vec::new();
    let mut run: Vec<(DiffRow, LineDiff)> = Vec::new();
    for (row, diff) in rows {
        if row.kind == DiffRowKind::Context {
            run.push((row.clone(), diff.clone()));
            continue;
        }
        flush_context_run(&mut run, &mut items);
        items.push(HunkItem::Row(row.clone(), diff.clone()));
    }
    flush_context_run(&mut run, &mut items);
    items
}

/// 结束一段连续未修改行：过长时中段折叠，两侧各保留 FOLD_CONTEXT_LINES 行。
fn flush_context_run(run: &mut Vec<(DiffRow, LineDiff)>, items: &mut Vec<HunkItem>) {
    if run.is_empty() {
        return;
    }
    let mut rows = std::mem::take(run);
    if rows.len() > FOLD_CONTEXT_LINES * 2 {
        let hidden_count = rows.len() - FOLD_CONTEXT_LINES * 2;
        let tail = rows.split_off(hidden_count + FOLD_CONTEXT_LINES);
        let hidden = rows.split_off(FOLD_CONTEXT_LINES);
        items.extend(rows.into_iter().map(|(row, diff)| HunkItem::Row(row, diff)));
        items.push(HunkItem::Folded {
            count: hidden.len() as u32,
            hidden,
        });
        items.extend(tail.into_iter().map(|(row, diff)| HunkItem::Row(row, diff)));
    } else {
        items.extend(rows.into_iter().map(|(row, diff)| HunkItem::Row(row, diff)));
    }
}

/// 折叠区间的展开状态键：文件路径 + 文件段/hunk/区间索引。
fn diff_fold_key(path: &str, section: usize, hunk: usize, item: usize) -> String {
    format!("{path}:{section}:{hunk}:{item}")
}

/// 切换折叠区间的展开状态（未展开则展开，已展开则收起）。
fn toggle_expanded_hunk(expanded: &mut HashSet<String>, key: &str) {
    if !expanded.remove(key) {
        expanded.insert(key.to_string());
    }
}

/// Stable content hash for diff cache keys.
fn content_key(text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

// --- rendering -------------------------------------------------------------

/// 渲染一行 diff：行号 gutter + 词级底色。
///
/// 整行用单个文本节点 + 高亮区间渲染，保持与旧版一致的块级布局与横向
/// 滚动宽度；行级底色铺满整行，词级底色只覆盖变化的词。
fn render_diff_row(theme: &Theme, row: &DiffRow, diff: &LineDiff) -> AnyElement {
    let (soft, base_color) = match row.kind {
        DiffRowKind::Insert => (Some(theme.success_soft), theme.success),
        DiffRowKind::Delete => (Some(theme.danger_soft), theme.danger),
        DiffRowKind::Context => (None, theme.text_muted),
        DiffRowKind::Note => (None, theme.text_faint),
    };
    let plain = matches!(diff, LineDiff::Plain);
    let gutter = format!(
        "{:>4} {:>4}",
        row.old_no.map_or(String::new(), |no| no.to_string()),
        row.new_no.map_or(String::new(), |no| no.to_string()),
    );

    let mut full = String::new();
    let mut highlights: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
    full.push_str(&gutter);
    highlights.push((
        0..gutter.len(),
        HighlightStyle {
            color: Some(theme.text_faint),
            ..Default::default()
        },
    ));
    let parts: Vec<(&str, HighlightStyle)> = match diff {
        LineDiff::Plain => vec![(
            row.text.as_str(),
            HighlightStyle {
                color: Some(base_color),
                ..Default::default()
            },
        )],
        LineDiff::Words(segments) => segments
            .iter()
            .map(|segment| {
                let emphasized = segment.tag == WordTag::Changed;
                (
                    segment.text.as_str(),
                    HighlightStyle {
                        color: Some(base_color),
                        background_color: emphasized.then_some(soft).flatten(),
                        font_weight: emphasized.then_some(gpui::FontWeight::SEMIBOLD),
                        ..Default::default()
                    },
                )
            })
            .collect(),
    };
    for (part, style) in parts {
        let start = full.len();
        full.push_str(part);
        highlights.push((start..full.len(), style));
    }

    div()
        .whitespace_nowrap()
        .when_some(plain.then_some(soft).flatten(), |style, bg| style.bg(bg))
        .child(StyledText::new(full).with_highlights(highlights))
        .into_any_element()
}

/// 渲染「⋯ N 行未修改」折叠行，点击展开/收起。
fn render_fold_row(
    theme: &Theme,
    count: u32,
    key: String,
    cx: &mut Context<AppView>,
) -> AnyElement {
    div()
        .id(SharedString::from(format!("diff-fold-{}", key)))
        .flex()
        .items_center()
        .gap_2()
        .px_2()
        .my(px(1.0))
        .rounded(px(4.0))
        .cursor_pointer()
        .text_color(theme.text_faint)
        .hover(|style| style.bg(theme.hover))
        .on_click(cx.listener(move |this, _, _, cx| {
            toggle_expanded_hunk(&mut this.side.expanded_hunks, &key);
            cx.notify();
        }))
        .child(
            svg()
                .path(icons::CHEVRON_RIGHT)
                .size(px(10.0))
                .text_color(theme.text_faint),
        )
        .child(format!("⋯ {count} 行未修改"))
        .into_any_element()
}

impl AppView {
    pub(crate) fn render_side_pane(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.theme.clone();
        let tab_bar = self.render_side_tab_bar(&theme, cx);
        let body: AnyElement = match self.side.active().cloned() {
            Some(active) => self.render_side_tab_content(&active, window, cx),
            None => div()
                .flex_1()
                .min_h(px(0.0))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.text_faint)
                        .child("没有打开的页签"),
                )
                .child(
                    Button::new("side-open-plan", "打开计划", &theme)
                        .variant(ButtonVariant::Secondary)
                        .size(ButtonSize::Sm)
                        .icon(icons::CLOCK)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.side.open_tab(SidePaneTab::plan());
                            this.persist_side_pane();
                            cx.notify();
                        })),
                )
                .into_any_element(),
        };

        let mut panel = div()
            .id("side-pane")
            .w(px(self.side.width))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .bg(theme.bg)
            .border_l_1()
            .border_color(theme.border)
            .child(tab_bar)
            .child(body);

        panel = panel.child(
            div()
                .id("side-pane-resize")
                .absolute()
                .left(px(-3.0))
                .top(px(0.0))
                .bottom(px(0.0))
                .w(px(6.0))
                .cursor(gpui::CursorStyle::ResizeLeftRight)
                .hover(|style| style.bg(theme.active))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.side.resizing = true;
                        cx.notify();
                    }),
                ),
        );
        panel.into_any_element()
    }

    fn render_side_tab_bar(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let running_agents = self
            .subagent_tasks
            .iter()
            .filter(|task| task.status == SubagentStatus::Running)
            .count();
        let tabs: Vec<AnyElement> = self
            .side
            .tabs
            .iter()
            .map(|tab| {
                let active = tab.id == self.side.active_id;
                let tab_id = tab.id.clone();
                let close_id = tab.id.clone();
                let badge_count = match &tab.kind {
                    SidePaneTabKind::Agents => running_agents,
                    SidePaneTabKind::Agent(agent_id) => self
                        .subagent_tasks
                        .iter()
                        .find(|task| &task.agent_id == agent_id)
                        .filter(|task| task.status == SubagentStatus::Running)
                        .map(|_| 1)
                        .unwrap_or(0),
                    _ => 0,
                };
                div()
                    .id(SharedString::from(format!("side-tab-{}", tab.id)))
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(6.0))
                    .h(px(28.0))
                    .max_w(px(170.0))
                    .px(px(8.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(transparent_black())
                    .cursor_pointer()
                    .when(active, |element| {
                        element
                            .bg(theme.surface)
                            .border_color(theme.border)
                            .shadow_sm()
                    })
                    .when(!active, |element| {
                        element.hover(|style| style.bg(theme.hover))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.side.activate(&tab_id);
                        this.on_side_tab_activated(cx);
                        cx.notify();
                    }))
                    .child(
                        svg()
                            .path(tab.icon)
                            .size(px(12.0))
                            .text_color(if active { theme.text } else { theme.text_faint })
                            .flex_none(),
                    )
                    .child(
                        div()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(11.5))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(if active { theme.text } else { theme.text_faint })
                            .child(tab.title.clone()),
                    )
                    .when(badge_count > 0, |element| {
                        element.child(small_badge(
                            theme,
                            badge_count.to_string(),
                            BadgeTone::Accent,
                        ))
                    })
                    .when(tab.closable, |element| {
                        element.child(
                            div()
                                .id(SharedString::from(format!("side-tab-close-{}", tab.id)))
                                .flex()
                                .items_center()
                                .justify_center()
                                .size(px(18.0))
                                .rounded(px(radius::XS))
                                .hover(|style| style.bg(theme.active))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.close_side_tab(&close_id, cx);
                                }))
                                .child(
                                    svg()
                                        .path(icons::X)
                                        .size(px(9.0))
                                        .text_color(theme.text_faint),
                                ),
                        )
                    })
                    .into_any_element()
            })
            .collect();

        div()
            .h(px(52.0))
            .flex_none()
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .id("side-tab-strip")
                    .flex()
                    .items_center()
                    .gap_1()
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_x_scroll()
                    .children(tabs),
            )
            .child(
                IconButton::new("side-tab-add", icons::PLUS, theme)
                    .size(24.0, 12.0)
                    .tooltip("添加页签")
                    .on_click(cx.listener(|this, event: &gpui::ClickEvent, _, cx| {
                        this.side.overlay = Some(SideOverlay::AddMenu(event.position()));
                        cx.notify();
                    })),
            )
            .child(
                IconButton::new("side-tab-overview", icons::MORE, theme)
                    .size(24.0, 12.0)
                    .tooltip("查看所有页签")
                    .on_click(cx.listener(|this, event: &gpui::ClickEvent, window, cx| {
                        this.side.overlay = Some(SideOverlay::Overview(event.position()));
                        this.side
                            .search_state
                            .update(cx, |state, _| state.set_content_silent(""));
                        let handle = this.side.search_state.read(cx).focus_handle();
                        window.focus(&handle);
                        cx.notify();
                    })),
            )
            .child(
                IconButton::new("side-pane-close", icons::X, theme)
                    .size(24.0, 12.0)
                    .tooltip("关闭右侧面板")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.side.open = false;
                        this.persist_side_pane();
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    fn render_side_tab_content(
        &mut self,
        tab: &SidePaneTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match &tab.kind {
            SidePaneTabKind::Plan => self.render_plan_pane(cx),
            SidePaneTabKind::Agents => self.render_agents_pane(cx),
            SidePaneTabKind::Files => self.render_files_pane(cx),
            SidePaneTabKind::Changes => self.render_changes_pane(cx),
            SidePaneTabKind::Agent(agent_id) => self.render_agent_pane(agent_id, window, cx),
        }
    }

    fn render_plan_pane(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = &self.theme;
        let scroll_entity = cx.entity().downgrade();
        let shared = self.active_shared();
        let (todos, usage, message_count, steps, model, provider, context_window, compacted) =
            if let Some(shared) = &shared {
                let session = lock_session(shared);
                let resolved = self.models.resolve_session(&session);
                (
                    session.todos.clone(),
                    session.usage.clone(),
                    session.messages.len(),
                    session.steps,
                    resolved.model_label().to_string(),
                    resolved.provider_label().to_string(),
                    resolved.context_window(),
                    session
                        .compaction
                        .as_ref()
                        .map(|record| record.compacted_messages)
                        .unwrap_or(0),
                )
            } else {
                (
                    Vec::new(),
                    Usage::default(),
                    0,
                    0,
                    String::new(),
                    String::new(),
                    128_000,
                    0,
                )
            };

        let todo_rows: Vec<AnyElement> = todos
            .iter()
            .map(|todo| {
                let marker: AnyElement = match todo.status {
                    TodoStatus::Completed => div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(18.0))
                        .rounded(px(radius::FULL))
                        .bg(theme.success_soft)
                        .child(
                            svg()
                                .path(icons::CHECK)
                                .size(px(10.0))
                                .text_color(theme.success),
                        )
                        .into_any_element(),
                    TodoStatus::InProgress => div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(18.0))
                        .rounded(px(radius::FULL))
                        .bg(theme.accent_soft)
                        .child(Spinner::new(theme.accent, 10.0))
                        .into_any_element(),
                    TodoStatus::Pending => div()
                        .flex_none()
                        .size(px(18.0))
                        .rounded(px(radius::FULL))
                        .border_1()
                        .border_color(theme.border_strong)
                        .into_any_element(),
                };
                div()
                    .flex()
                    .items_start()
                    .gap_2()
                    .py(px(4.0))
                    .child(marker)
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(12.0))
                            .line_height(px(18.0))
                            .text_color(if todo.status == TodoStatus::Completed {
                                theme.text_faint
                            } else {
                                theme.text_muted
                            })
                            .child(todo.content.clone()),
                    )
                    .into_any_element()
            })
            .collect();

        div()
            .relative()
            .flex_1()
            .min_h(px(0.0))
            .child(
                div()
                    .id("side-plan-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(self.scrolls.side_plan.handle())
                    .px_4()
                    .py_4()
                    .flex()
                    .flex_col()
                    .gap(px(20.0))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(section_label(theme, "计划"))
                    .when(todo_rows.is_empty(), |element| {
                        element.child(
                            div()
                                .text_size(px(11.0))
                                .text_color(theme.text_faint)
                                .py_2()
                                .child("暂无任务。让 EffiBuddy 制定计划后会显示在这里"),
                        )
                    })
                    .children(todo_rows),
            )
            .child(divider(theme))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(section_label(theme, "上下文"))
                    .child(stat_row(theme, "供应商", &provider))
                    .child(stat_row(theme, "模型", &model))
                    .child(stat_row(
                        theme,
                        "上下文窗口",
                        &format_context_window(context_window),
                    ))
                    .child(stat_row(theme, "消息", &message_count.to_string()))
                    .child(stat_row(theme, "当前步数", &steps.to_string()))
                    .child(stat_row(
                        theme,
                        "当前占用",
                        &format_tokens(usage.last_prompt_tokens),
                    ))
                    .child(stat_row(
                        theme,
                        "累计 tokens",
                        &format_tokens(usage.total_tokens),
                    ))
                    .child(stat_row(theme, "已压缩消息", &compacted.to_string())),
            )
            .child(divider(theme))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(section_label(theme, "能力"))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_1()
                            .children(
                                [
                                    "read_file",
                                    "write_file",
                                    "edit_file",
                                    "list_dir",
                                    "grep",
                                    "shell",
                                    "todo_write",
                                    "run_js",
                                    "agent",
                                ]
                                .iter()
                                .map(|name| badge(theme, *name, BadgeTone::Neutral)),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(10.5))
                            .line_height(px(16.0))
                            .text_color(theme.text_faint)
                            .child("run_js 使用 Boa 引擎，在同步 JS 中调用 tools.*，每次嵌套调用仍会经过审批。agent 工具可启动只读 explore / reviewer 或通用子代理。"),
                    ),
            )
            )
            .when(!self.modal_blocks_scroll() && self.side.overlay.is_none(), |element| {
                element.child(
                    smooth_overlay(
                        self.scrolls.side_plan.clone(),
                        scroll_entity.clone(),
                    )
                        .absolute()
                        .inset_0(),
                )
            })
            .into_any_element()
    }

    fn render_agents_pane(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = &self.theme;
        let scroll_entity = cx.entity().downgrade();
        let mut tasks: Vec<&AgentTask> = self.subagent_tasks.iter().collect();
        tasks.sort_by(|left, right| {
            let left_running = left.status == SubagentStatus::Running;
            let right_running = right.status == SubagentStatus::Running;
            right_running
                .cmp(&left_running)
                .then_with(|| right.started_at.cmp(&left.started_at))
        });
        // Live rows are always shown; history rows (restored or finished) are
        // paginated. A simple visible-limit stands in for ZCode's keyset
        // cursor — the catalog is an in-memory sorted list, not a database.
        let live_count = tasks
            .iter()
            .filter(|task| task.status == SubagentStatus::Running || task.waiting_approval)
            .count();
        let ended_start = live_count.min(tasks.len());
        let visible_ended = self.subagent_history_limit.min(tasks.len() - ended_start);
        let hidden_history = tasks.len() - ended_start - visible_ended;
        let mut rows: Vec<AnyElement> = Vec::new();
        for task in &tasks[..ended_start + visible_ended] {
            rows.push(self.render_agent_row(task, theme, cx));
        }
        if ended_start > 0 && visible_ended > 0 {
            rows.insert(
                ended_start,
                div()
                    .mt(px(2.0))
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().h(px(1.0)).bg(theme.border))
                    .child(
                        div()
                            .text_size(px(9.5))
                            .text_color(theme.text_faint)
                            .child("历史子代理"),
                    )
                    .child(div().flex_1().h(px(1.0)).bg(theme.border))
                    .into_any_element(),
            );
        }
        if hidden_history > 0 {
            rows.push(
                div()
                    .id("side-agent-history-more")
                    .w_full()
                    .flex()
                    .justify_center()
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(theme.border)
                    .py(px(6.0))
                    .text_size(px(11.0))
                    .text_color(theme.text_muted)
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.surface_alt))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.subagent_history_limit += 30;
                        cx.notify();
                    }))
                    .child(format!("加载更多（还有 {hidden_history} 条历史子代理）"))
                    .into_any_element(),
            );
        }

        div()
            .relative()
            .flex_1()
            .min_h(px(0.0))
            .child(
                div()
                    .id("side-agents-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(self.scrolls.side_agents.handle())
                    .px_4()
                    .py_4()
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .when(rows.is_empty(), |element| {
                        element.child(
                            div()
                                .rounded(px(10.0))
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.surface)
                                .p(px(12.0))
                                .flex()
                                .flex_col()
                                .gap_2()
                                .child(
                                    div()
                                        .text_size(px(12.0))
                                        .text_color(theme.text_muted)
                                        .child("还没有子代理"),
                                )
                                .child(
                                    div()
                                        .text_size(px(10.5))
                                        .line_height(px(16.0))
                                        .text_color(theme.text_faint)
                                        .child("当主代理需要大范围搜索、独立审查或并行工作时，会通过 agent 工具启动子代理。运行状态、耗时与完整对话都会显示在这里。"),
                                ),
                        )
                    })
                    .children(rows),
            )
            .when(!self.modal_blocks_scroll() && self.side.overlay.is_none(), |element| {
                element.child(
                    smooth_overlay(
                        self.scrolls.side_agents.clone(),
                        scroll_entity.clone(),
                    )
                        .absolute()
                        .inset_0(),
                )
            })
            .into_any_element()
    }

    /// One row in the subagent catalog list.
    fn render_agent_row(
        &self,
        task: &AgentTask,
        theme: &effibuddy_ui::Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let color = agent_color(theme, task.color);
        let status_text = if task.waiting_approval {
            "等待批准".to_string()
        } else {
            task.status.label().to_string()
        };
        let elapsed = task
            .finished_at
            .unwrap_or_else(Instant::now)
            .saturating_duration_since(task.started_at);
        let open_id = task.agent_id.clone();
        let running = task.status == SubagentStatus::Running;
        let group_label = task.group_name.clone();
        let status_icon: AnyElement = if task.waiting_approval {
            svg()
                .path(icons::ALERT)
                .size(px(12.0))
                .text_color(theme.warning)
                .into_any_element()
        } else {
            match task.status {
                SubagentStatus::Running => Spinner::new(color, 12.0).into_any_element(),
                SubagentStatus::Completed => svg()
                    .path(icons::CHECK)
                    .size(px(12.0))
                    .text_color(theme.success)
                    .into_any_element(),
                SubagentStatus::Failed => svg()
                    .path(icons::ALERT)
                    .size(px(12.0))
                    .text_color(theme.danger)
                    .into_any_element(),
                SubagentStatus::Cancelled => svg()
                    .path(icons::STOP)
                    .size(px(10.0))
                    .text_color(theme.text_faint)
                    .into_any_element(),
                SubagentStatus::Lost => svg()
                    .path(icons::CLOCK)
                    .size(px(12.0))
                    .text_color(theme.text_faint)
                    .into_any_element(),
            }
        };
        div()
            .id(SharedString::from(format!("side-agent-{}", task.agent_id)))
            .w_full()
            .rounded(px(radius::MD))
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface)
            .p(px(10.0))
            .flex()
            .items_start()
            .gap_2()
            .cursor_pointer()
            .hover(|style| {
                style
                    .bg(theme.surface_alt)
                    .border_color(theme.border_strong)
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.open_agent_tab(&open_id, cx);
            }))
            .child(div().flex_none().mt(px(1.0)).child(status_icon))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(px(11.5))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child(task.name.clone()),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(px(9.5))
                                    .text_color(color)
                                    .child(task.profile.clone()),
                            )
                            .child(small_badge(
                                theme,
                                if task.visibility == AgentVisibility::Public {
                                    "公开"
                                } else {
                                    "私有"
                                },
                                BadgeTone::Neutral,
                            ))
                            .when(task.group_id.is_some(), {
                                let group_label = group_label.clone();
                                let theme_owned: effibuddy_ui::Theme = theme.clone();
                                move |element| {
                                    element.child(small_badge(
                                        &theme_owned,
                                        group_label.clone().unwrap_or_else(|| "组".to_string()),
                                        BadgeTone::Accent,
                                    ))
                                }
                            })
                            .when(task.background, |element| {
                                element.child(small_badge(theme, "后台", BadgeTone::Neutral))
                            })
                            .when(task.waiting_approval, |element| {
                                element.child(small_badge(theme, "等待批准", BadgeTone::Warning))
                            }),
                    )
                    .child(
                        div()
                            .text_size(px(11.5))
                            .line_height(px(17.0))
                            .text_color(theme.text_muted)
                            .child(task.description.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_size(px(10.0))
                            .text_color(theme.text_faint)
                            .child(status_text)
                            .child(format_elapsed(elapsed))
                            .when(task.tool_uses > 0, |element| {
                                element.child(format!("{} 次工具", task.tool_uses))
                            }),
                    ),
            )
            .when(running, |element| {
                element.child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(18.0))
                        .rounded(px(5.0))
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.bg)
                        .hover(|style| style.bg(theme.danger_soft))
                        .child(
                            svg()
                                .path(icons::X)
                                .size(px(9.0))
                                .text_color(theme.text_faint),
                        ),
                )
            })
            .into_any_element()
    }

    fn render_agent_pane(
        &mut self,
        agent_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.theme.clone();
        let Some(task) = self
            .subagent_tasks
            .iter()
            .find(|task| task.agent_id == agent_id)
        else {
            return empty_state(&theme, "子代理已结束", "会话记录不再可用", icons::SPARKLES)
                .into_any_element();
        };
        let profile = task.profile.clone();
        let name = task.name.clone();
        let visibility = task.visibility;
        let group_name = task.group_name.clone();
        let color = agent_color(&theme, task.color);
        let description = task.description.clone();
        let status = task.status;
        let started_at = task.started_at;
        let finished_at = task.finished_at;
        let tool_uses = task.tool_uses;
        let error = task.error.clone();
        let waiting = task.waiting_approval;
        let model = task.model.clone();
        let shared = task.shared.clone();
        let running = status == SubagentStatus::Running;
        let agent_id = agent_id.to_string();

        if let Some(view) = self.side.agent_views.get_mut(&agent_id) {
            if view.chat.tick_scroll() && !view.chat.glide_frame_scheduled {
                view.chat.glide_frame_scheduled = true;
                let frame_agent = agent_id.clone();
                cx.on_next_frame(window, move |this, _, cx| {
                    if let Some(view) = this.side.agent_views.get_mut(&frame_agent) {
                        view.chat.glide_frame_scheduled = false;
                    }
                    cx.notify();
                });
            }
            if view.chat.tick() && !view.chat.reveal_frame_scheduled {
                view.chat.reveal_frame_scheduled = true;
                let frame_agent = agent_id.clone();
                cx.on_next_frame(window, move |this, _, cx| {
                    if let Some(view) = this.side.agent_views.get_mut(&frame_agent) {
                        view.chat.reveal_frame_scheduled = false;
                    }
                    cx.notify();
                });
            }
            if view.dirty {
                let run = RunSnapshot {
                    running,
                    started_at: Some(started_at),
                    waiting_approval: waiting,
                    approval_tool: None,
                    last_error: error.clone(),
                    notice: None,
                };
                {
                    let session = lock_session(&shared);
                    view.chat.rebuild(&session, &run);
                }
                view.dirty = false;
            }
            view.chat.sync_scroll();
        }

        let body = if let Some(view) = self.side.agent_views.get_mut(&agent_id) {
            let cards: HashMap<String, AgentCard> = HashMap::new();
            let view_state = Rc::new(view.chat.prepare_frame(
                running,
                &self.expanded_reasoning,
                &self.expanded_tools,
                &self.expanded_turns,
                &self.collapsed_turns,
                &cards,
            ));
            let state = view.chat.list_state.clone();
            let entity = cx.entity().downgrade();
            let item_theme = theme.clone();
            let rows = gpui::list(state, move |index, _window, _cx| {
                render_chat_item(index, &view_state, &item_theme, &entity)
            });
            let wheel_entity = cx.entity().downgrade();
            let wheel_agent = agent_id.clone();
            let mut container = div()
                .flex_1()
                .min_h(px(0.0))
                .relative()
                .child(rows.size_full());
            if self.side.overlay.is_none() {
                container = container.child(
                    crate::chat::agent_chat_wheel_interceptor(wheel_agent, wheel_entity)
                        .absolute()
                        .inset_0(),
                );
            }
            if !view.chat.pinned() {
                container = container.child(
                    div()
                        .id("side-agent-scroll-bottom")
                        .absolute()
                        .right(px(12.0))
                        .bottom(px(12.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(26.0))
                        .rounded(px(999.0))
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.surface)
                        .shadow(shadow_float(&theme))
                        .cursor_pointer()
                        .hover(|style| style.bg(theme.surface_alt))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(active) = this.side.active().cloned() {
                                if let SidePaneTabKind::Agent(agent_id) = active.kind {
                                    if let Some(view) = this.side.agent_views.get_mut(&agent_id) {
                                        view.chat.pin_to_bottom();
                                    }
                                }
                            }
                            cx.notify();
                        }))
                        .child(
                            svg()
                                .path(icons::CHEVRON_DOWN)
                                .size(px(12.0))
                                .text_color(theme.text_muted),
                        ),
                );
            }
            container.into_any_element()
        } else {
            div().into_any_element()
        };

        let elapsed = finished_at
            .unwrap_or_else(Instant::now)
            .saturating_duration_since(started_at);
        let status_text = if waiting {
            "等待批准".to_string()
        } else {
            status.label().to_string()
        };
        let status_color = if waiting {
            theme.warning
        } else {
            match status {
                SubagentStatus::Completed => theme.success,
                SubagentStatus::Failed => theme.danger,
                SubagentStatus::Cancelled | SubagentStatus::Lost => theme.text_faint,
                SubagentStatus::Running => color,
            }
        };
        let error_banner = error
            .filter(|_| status == SubagentStatus::Failed)
            .map(|error| {
                div()
                    .mx_3()
                    .mt_3()
                    .rounded(px(10.0))
                    .border_1()
                    .border_color(theme.danger)
                    .bg(theme.danger_soft)
                    .px(px(12.0))
                    .py(px(8.0))
                    .text_size(px(11.0))
                    .text_color(theme.danger)
                    .child(error)
            });
        let approval_card = self
            .pending_approvals
            .iter()
            .find(|pending: &&PendingApproval| {
                pending.agent_id.as_deref() == Some(agent_id.as_str())
            })
            .map(|pending| pending.request.clone())
            .map(|request| {
                div()
                    .px_3()
                    .pt_2()
                    .child(self.render_approval(&request, Some(&agent_id), cx))
            });
        let stop_id = agent_id.clone();

        let header = div()
            .flex_none()
            .flex()
            .flex_col()
            .gap_1()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(22.0))
                            .rounded(px(7.0))
                            .bg({
                                let mut soft = color;
                                soft.a = if theme.is_dark() { 0.20 } else { 0.10 };
                                soft
                            })
                            .child(svg().path(icons::SPARKLES).size(px(12.0)).text_color(color)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(12.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child(format!("{name} · {profile}")),
                    )
                    .child(badge(
                        &theme,
                        if visibility == AgentVisibility::Public {
                            "公开"
                        } else {
                            "私有"
                        },
                        BadgeTone::Neutral,
                    ))
                    .when_some(group_name, |element, group| {
                        element.child(badge(&theme, group, BadgeTone::Accent))
                    })
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(status_color)
                            .child(status_text),
                    )
                    .when(running, |element| {
                        element.child(
                            Button::new("side-agent-stop", "停止", &theme)
                                .variant(ButtonVariant::Ghost)
                                .size(ButtonSize::Sm)
                                .icon(icons::STOP)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.stop_agent(&stop_id, cx);
                                })),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_size(px(10.0))
                    .text_color(theme.text_faint)
                    .child(description)
                    .child(format!("{} 次工具", tool_uses))
                    .child(format_elapsed(elapsed))
                    .child(badge(&theme, model, BadgeTone::Neutral)),
            );

        div()
            .relative()
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .child(header)
            .when_some(error_banner, |element, banner| element.child(banner))
            .child(body)
            .when_some(approval_card, |element, card| element.child(card))
            .into_any_element()
    }

    fn render_files_pane(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme.clone();
        let scroll_entity = cx.entity().downgrade();
        if let Some(preview) = self.side.file_tree.preview.as_ref() {
            let path = preview.path.clone();
            let display = path
                .strip_prefix(&self.side.file_tree.root)
                .unwrap_or(&path)
                .display()
                .to_string();
            let truncated = preview.truncated;
            let binary = preview.binary;
            let lines = Rc::clone(&preview.lines);
            let line_count = lines.len();
            let row_theme = theme.clone();
            return div()
                .flex_1()
                .min_h(px(0.0))
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_2()
                        .border_b_1()
                        .border_color(theme.border)
                        .child(
                            IconButton::new("file-preview-back", icons::CHEVRON_LEFT, &theme)
                                .size(24.0, 12.0)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.side.file_tree.preview = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .truncate()
                                .text_size(px(11.0))
                                .font_family(theme.mono_font())
                                .text_color(theme.text_muted)
                                .child(display),
                        )
                        .when(truncated, |element| {
                            element.child(badge(&theme, "已截断", BadgeTone::Warning))
                        })
                        .when(binary, |element| {
                            element.child(badge(&theme, "二进制", BadgeTone::Neutral))
                        }),
                )
                // Virtualized: only visible lines are built per frame; the
                // old branch materialized up to 4_000 row elements every time
                // anything notified.
                .child(
                    div()
                        .relative()
                        .flex_1()
                        .min_h(px(0.0))
                        .child(
                            uniform_list(
                                "file-preview-lines",
                                line_count,
                                move |range, _window, _cx| {
                                    let range_start = range.start;
                                    lines[range]
                                        .iter()
                                        .enumerate()
                                        .map(|(offset, line)| {
                                            let index = range_start + offset;
                                            div()
                                                .flex()
                                                .h(px(16.0))
                                                .gap_2()
                                                .child(
                                                    div()
                                                        .w(px(34.0))
                                                        .flex_none()
                                                        .text_right()
                                                        .text_color(row_theme.text_faint)
                                                        .child((index + 1).to_string()),
                                                )
                                                .child(
                                                    div()
                                                        .min_w(px(0.0))
                                                        .whitespace_nowrap()
                                                        .child(line.clone()),
                                                )
                                                .into_any_element()
                                        })
                                        .collect()
                                },
                            )
                            .track_scroll(self.scrolls.file_preview_list.clone())
                            .size_full()
                            .with_horizontal_sizing_behavior(
                                gpui::ListHorizontalSizingBehavior::Unconstrained,
                            )
                            .px_3()
                            .py_2()
                            .font_family(theme.mono_font())
                            .text_size(px(10.5))
                            .line_height(px(16.0))
                            .text_color(theme.text),
                        )
                        .when(
                            !self.modal_blocks_scroll() && self.side.overlay.is_none(),
                            |element| {
                                element.child(
                                    smooth_overlay(
                                        self.scrolls.file_preview.clone(),
                                        scroll_entity.clone(),
                                    )
                                    .absolute()
                                    .inset_0(),
                                )
                            },
                        ),
                )
                .into_any_element();
        }

        let root_name = self
            .side
            .file_tree
            .root
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| self.side.file_tree.root.display().to_string());
        // Virtualized: visible rows only, backed by the cached flattened rows.
        let rows = self.side.file_tree.visible_rows();
        let row_count = rows.len();
        let view = cx.entity();
        let row_theme = theme.clone();
        let rows_for_items = Rc::clone(&rows);
        let tree = uniform_list("file-tree-rows", row_count, move |range, _window, _cx| {
            let range_start = range.start;
            rows_for_items[range]
                .iter()
                .enumerate()
                .map(|(offset, row)| {
                    let index = range_start + offset;
                    let path = row.path.clone();
                    let chevron_id =
                        SharedString::from(format!("file-tree-chevron-{}", path.display()));
                    let is_dir = row.is_dir;
                    let expanded = row.expanded;
                    let view = view.clone();
                    div()
                        .id(("file-row", index))
                        .flex()
                        .items_center()
                        .gap_1()
                        .h(px(24.0))
                        .pl(px(8.0 + row.depth as f32 * 12.0))
                        .pr_2()
                        .rounded(px(6.0))
                        .cursor_pointer()
                        .hover(|style| style.bg(row_theme.hover))
                        .on_click(move |_, _, cx| {
                            view.update(cx, |this, cx| {
                                if is_dir {
                                    this.side.file_tree.toggle_dir(&path);
                                } else {
                                    this.side.file_tree.open_file(&path);
                                }
                                cx.notify();
                            });
                        })
                        .child(if is_dir {
                            disclosure_chevron(chevron_id, expanded, row_theme.text_muted, 10.0)
                                .into_any_element()
                        } else {
                            svg()
                                .path(icons::FILE)
                                .size(px(12.0))
                                .text_color(row_theme.text_faint)
                                .flex_none()
                                .into_any_element()
                        })
                        .child(
                            div()
                                .min_w(px(0.0))
                                .truncate()
                                .text_size(px(11.0))
                                .text_color(if is_dir {
                                    row_theme.text
                                } else {
                                    row_theme.text_muted
                                })
                                .child(row.name.clone()),
                        )
                        .into_any_element()
                })
                .collect()
        })
        .track_scroll(self.scrolls.file_tree_list.clone())
        .flex_1()
        .min_h(px(0.0))
        .px_1()
        .py_1();

        div()
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        svg()
                            .path(icons::FOLDER)
                            .size(px(12.0))
                            .text_color(theme.text_muted),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(11.5))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(root_name),
                    )
                    .child(
                        IconButton::new("file-tree-refresh", icons::LOADER, &theme)
                            .size(24.0, 12.0)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.refresh_file_tree();
                                cx.notify();
                            })),
                    ),
            )
            .when_some(self.side.file_tree.error.clone(), |element, error| {
                element.child(
                    div()
                        .px_3()
                        .py_2()
                        .text_size(px(10.5))
                        .text_color(theme.danger)
                        .child(error),
                )
            })
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h(px(0.0))
                    .child(tree.size_full())
                    .when(
                        !self.modal_blocks_scroll() && self.side.overlay.is_none(),
                        |element| {
                            element.child(
                                smooth_overlay(
                                    self.scrolls.file_tree.clone(),
                                    scroll_entity.clone(),
                                )
                                .absolute()
                                .inset_0(),
                            )
                        },
                    ),
            )
            .into_any_element()
    }

    fn render_changes_pane(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme.clone();
        let scroll_entity = cx.entity().downgrade();
        if self.side.git.selected.is_some() {
            let file = self
                .side
                .git
                .selected
                .and_then(|index| self.side.git.files.get(index))
                .cloned();
            let diff: &str = self.side.git.diff.as_deref().unwrap_or_default();
            let diff_key = content_key(diff);
            // Parse + annotate + fold once per diff content; the per-frame
            // render only rebuilds elements from the cached units.
            let ir = match &self.side.git.diff_ir_cache {
                Some((cached_key, cached)) if *cached_key == diff_key => cached.clone(),
                _ => {
                    let truncated = diff.lines().count() > MAX_DIFF_RENDER_LINES;
                    let visible = diff
                        .lines()
                        .take(MAX_DIFF_RENDER_LINES)
                        .collect::<Vec<_>>()
                        .join("\n");
                    let parsed = parse_unified_hunks(&visible);
                    let sections = parsed
                        .into_iter()
                        .map(|section| DiffSectionIr {
                            header: section.header,
                            hunks: section
                                .hunks
                                .into_iter()
                                .map(|hunk| {
                                    let annotated = annotate_hunk_rows(&hunk.rows);
                                    (hunk.header, fold_hunk_rows(&annotated))
                                })
                                .collect(),
                        })
                        .collect();
                    let ir = Rc::new(DiffIr {
                        sections,
                        truncated,
                    });
                    self.side.git.diff_ir_cache = Some((diff_key, Rc::clone(&ir)));
                    ir
                }
            };
            let selected_path = file
                .as_ref()
                .map(|file| file.path.as_str())
                .unwrap_or_default();
            let mut lines: Vec<AnyElement> = Vec::new();
            for (section_index, section) in ir.sections.iter().enumerate() {
                for header_line in &section.header {
                    lines.push(
                        div()
                            .whitespace_nowrap()
                            .text_color(theme.text_faint)
                            .child(header_line.clone())
                            .into_any_element(),
                    );
                }
                for (hunk_index, (hunk_header, items)) in section.hunks.iter().enumerate() {
                    if !hunk_header.is_empty() {
                        lines.push(
                            div()
                                .whitespace_nowrap()
                                .text_color(theme.warning)
                                .child(hunk_header.clone())
                                .into_any_element(),
                        );
                    }
                    for (item_index, item) in items.iter().enumerate() {
                        match item {
                            HunkItem::Row(row, line_diff) => {
                                lines.push(render_diff_row(&theme, row, line_diff));
                            }
                            HunkItem::Folded { count, hidden } => {
                                let key = diff_fold_key(
                                    selected_path,
                                    section_index,
                                    hunk_index,
                                    item_index,
                                );
                                if self.side.expanded_hunks.contains(&key) {
                                    for (row, line_diff) in hidden {
                                        lines.push(render_diff_row(&theme, row, line_diff));
                                    }
                                } else {
                                    lines.push(render_fold_row(&theme, *count, key, cx));
                                }
                            }
                        }
                    }
                }
            }
            if ir.truncated {
                lines.push(
                    div()
                        .whitespace_nowrap()
                        .text_color(theme.text_faint)
                        .child(format!(
                            "⋯ diff 过长，已截断（仅显示前 {MAX_DIFF_RENDER_LINES} 行）"
                        ))
                        .into_any_element(),
                );
            }
            let title = file.map(|file| file.path).unwrap_or_default();
            return div()
                .flex_1()
                .min_h(px(0.0))
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_2()
                        .border_b_1()
                        .border_color(theme.border)
                        .child(
                            IconButton::new("git-diff-back", icons::CHEVRON_LEFT, &theme)
                                .size(24.0, 12.0)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.side.git.selected = None;
                                    this.side.git.diff = None;
                                    this.side.expanded_hunks.clear();
                                    cx.notify();
                                })),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .truncate()
                                .text_size(px(11.0))
                                .font_family(theme.mono_font())
                                .text_color(theme.text_muted)
                                .child(title),
                        ),
                )
                .child(
                    div()
                        .relative()
                        .flex_1()
                        .min_h(px(0.0))
                        .child(
                            div()
                                .id("git-diff-scroll")
                                .size_full()
                                .overflow_scroll()
                                .track_scroll(self.scrolls.side_git_diff.handle())
                                .px_3()
                                .py_2()
                                .font_family(theme.mono_font())
                                .text_size(px(10.5))
                                .line_height(px(16.0))
                                .children(lines),
                        )
                        .when(
                            !self.modal_blocks_scroll() && self.side.overlay.is_none(),
                            |element| {
                                element.child(
                                    smooth_overlay(
                                        self.scrolls.side_git_diff.clone(),
                                        scroll_entity.clone(),
                                    )
                                    .absolute()
                                    .inset_0(),
                                )
                            },
                        ),
                )
                .into_any_element();
        }

        let branch = self.side.git.branch.clone();
        let loading = self.side.git.loading;
        let error = self.side.git.error.clone();
        let files = self.side.git.files.clone();
        let rows: Vec<AnyElement> = files
            .iter()
            .enumerate()
            .map(|(index, file)| {
                let status = file.status_label();
                let color = if file.is_untracked() {
                    theme.text_faint
                } else if file.index_status == 'D' || file.worktree_status == 'D' {
                    theme.danger
                } else if file.index_status == 'A'
                    || file.worktree_status == 'A'
                    || file.is_untracked()
                {
                    theme.success
                } else {
                    theme.warning
                };
                let path = file.path.clone();
                div()
                    .id(SharedString::from(format!("git-file-{}", index)))
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(26.0))
                    .px_2()
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_git_diff(index, cx);
                    }))
                    .child(
                        div()
                            .w(px(22.0))
                            .flex_none()
                            .text_size(px(10.0))
                            .font_family(theme.mono_font())
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(color)
                            .child(status),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(11.0))
                            .text_color(theme.text_muted)
                            .child(path),
                    )
                    .into_any_element()
            })
            .collect();

        div()
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        svg()
                            .path(icons::BRANCH)
                            .size(px(12.0))
                            .text_color(theme.text_muted),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(11.5))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(branch.unwrap_or_else(|| "工作区变更".to_string())),
                    )
                    .when(loading, |element| {
                        element.child(Spinner::new(theme.text_faint, 12.0))
                    })
                    .child(
                        IconButton::new("git-refresh", icons::LOADER, &theme)
                            .size(24.0, 12.0)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.refresh_git(cx);
                            })),
                    ),
            )
            .when_some(error, |element, error| {
                element.child(
                    div()
                        .px_3()
                        .py_2()
                        .text_size(px(10.5))
                        .line_height(px(16.0))
                        .text_color(theme.danger)
                        .child(error),
                )
            })
            .when(rows.is_empty() && !loading, |element| {
                element.child(
                    div()
                        .px_3()
                        .py_4()
                        .text_size(px(11.0))
                        .text_color(theme.text_faint)
                        .child("工作区没有未提交的变更。"),
                )
            })
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h(px(0.0))
                    .child(
                        div()
                            .id("git-files-scroll")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(self.scrolls.side_git_files.handle())
                            .px_1()
                            .py_1()
                            .children(rows),
                    )
                    .when(
                        !self.modal_blocks_scroll() && self.side.overlay.is_none(),
                        |element| {
                            element.child(
                                smooth_overlay(
                                    self.scrolls.side_git_files.clone(),
                                    scroll_entity.clone(),
                                )
                                .absolute()
                                .inset_0(),
                            )
                        },
                    ),
            )
            .into_any_element()
    }

    pub(crate) fn render_side_overlay(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let overlay = self.side.overlay.clone()?;
        let theme = self.theme.clone();
        let position = match &overlay {
            SideOverlay::AddMenu(position) | SideOverlay::Overview(position) => *position,
        };
        let is_overview = matches!(overlay, SideOverlay::Overview(_));

        let menu = if is_overview {
            self.render_side_overview(&theme, cx)
        } else {
            self.render_side_add_menu(&theme, cx)
        };

        Some(
            div()
                .id("side-overlay-backdrop")
                .absolute()
                .top(px(0.0))
                .left(px(0.0))
                .size_full()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.side.overlay = None;
                        cx.notify();
                    }),
                )
                .child(
                    anchored()
                        .position(position)
                        .offset(point(px(-236.0), px(6.0)))
                        .snap_to_window_with_margin(px(10.0))
                        .child(
                            div()
                                .id("side-overlay-menu")
                                .w(px(260.0))
                                .rounded(px(12.0))
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.elevated)
                                .shadow(shadow_float(&theme))
                                .p(px(6.0))
                                .flex()
                                .flex_col()
                                .gap(px(2.0))
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .child(menu)
                                .with_animation(
                                    "side-overlay-menu-enter",
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

    fn render_side_add_menu(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let mut rows: Vec<AnyElement> = Vec::new();
        for tab in [
            SidePaneTab::plan(),
            SidePaneTab::agents(),
            SidePaneTab::files(),
            SidePaneTab::changes(),
        ] {
            if self.side.has_tab(&tab.id) {
                continue;
            }
            let open = tab.clone();
            rows.push(self.side_menu_row(
                theme,
                tab.icon,
                &tab.title,
                None,
                cx,
                move |this, cx| {
                    this.side.open_tab(open.clone());
                    this.side.overlay = None;
                    this.on_side_tab_activated(cx);
                    cx.notify();
                },
            ));
        }
        for task in &self.subagent_tasks {
            let id = format!("agent:{}", task.agent_id);
            if self.side.has_tab(&id) {
                continue;
            }
            let agent_id = task.agent_id.clone();
            rows.push(self.side_menu_row(
                theme,
                icons::SPARKLES,
                &format!("{} · {}", task.name, task.profile),
                Some(&task.profile),
                cx,
                move |this, cx| {
                    this.open_agent_tab(&agent_id, cx);
                    this.side.overlay = None;
                    cx.notify();
                },
            ));
        }
        if rows.is_empty() {
            rows.push(
                div()
                    .px_2()
                    .py_2()
                    .text_size(px(11.0))
                    .text_color(theme.text_faint)
                    .child("所有页签都已打开。")
                    .into_any_element(),
            );
        }
        ClassifiedRows(rows).into_element(theme)
    }

    fn render_side_overview(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let scroll_entity = cx.entity().downgrade();
        let query = self
            .side
            .search_state
            .read(cx)
            .content()
            .trim()
            .to_lowercase();
        let mut sections: Vec<AnyElement> = Vec::new();
        let mut open_rows = Vec::new();
        for tab in &self.side.tabs {
            if !query.is_empty() && !tab.title.to_lowercase().contains(&query) {
                continue;
            }
            let tab_id = tab.id.clone();
            let close_id = tab.id.clone();
            let active = tab.id == self.side.active_id;
            open_rows.push(
                div()
                    .id(SharedString::from(format!("overview-tab-{}", tab.id)))
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(30.0))
                    .px_2()
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .when(active, |element| element.bg(theme.active))
                    .hover(|style| style.bg(theme.hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.side.activate(&tab_id);
                        this.side.overlay = None;
                        this.on_side_tab_activated(cx);
                        cx.notify();
                    }))
                    .child(
                        svg()
                            .path(tab.icon)
                            .size(px(12.0))
                            .text_color(theme.text_faint),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(11.0))
                            .text_color(theme.text)
                            .child(tab.title.clone()),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("overview-close-{}", tab.id)))
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(16.0))
                            .rounded(px(4.0))
                            .hover(|style| style.bg(theme.active))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.close_side_tab(&close_id, cx);
                                cx.notify();
                            }))
                            .child(
                                svg()
                                    .path(icons::X)
                                    .size(px(9.0))
                                    .text_color(theme.text_faint),
                            ),
                    )
                    .into_any_element(),
            );
        }
        if !open_rows.is_empty() {
            sections.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(section_label(theme, "已打开"))
                    .children(open_rows)
                    .into_any_element(),
            );
        }

        let mut recent_rows = Vec::new();
        for tab in &self.side.recent_closed {
            if !query.is_empty() && !tab.title.to_lowercase().contains(&query) {
                continue;
            }
            let tab = tab.clone();
            let tab_icon = tab.icon;
            let tab_title = tab.title.clone();
            recent_rows.push(
                div()
                    .id(SharedString::from(format!("recent-tab-{}", tab.id)))
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(28.0))
                    .px_2()
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let SidePaneTabKind::Agent(agent_id) = &tab.kind {
                            this.open_agent_tab(agent_id, cx);
                        } else {
                            this.side.open_tab(tab.clone());
                            this.on_side_tab_activated(cx);
                        }
                        this.side.overlay = None;
                        cx.notify();
                    }))
                    .child(
                        svg()
                            .path(tab_icon)
                            .size(px(12.0))
                            .text_color(theme.text_faint),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(11.0))
                            .text_color(theme.text_muted)
                            .child(tab_title),
                    )
                    .child(
                        div()
                            .text_size(px(9.5))
                            .text_color(theme.text_faint)
                            .child("最近关闭"),
                    )
                    .into_any_element(),
            );
        }
        if !recent_rows.is_empty() {
            sections.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(section_label(theme, "最近关闭"))
                    .children(recent_rows)
                    .into_any_element(),
            );
        }
        if sections.is_empty() {
            sections.push(
                div()
                    .px_2()
                    .py_3()
                    .text_size(px(11.0))
                    .text_color(theme.text_faint)
                    .child("没有匹配的页签。")
                    .into_any_element(),
            );
        }

        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .rounded(px(7.0))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.surface)
                    .px_2()
                    .py_1()
                    .child(self.side.search_view.clone()),
            )
            .child(
                div()
                    .relative()
                    .child(
                        div()
                            .id("side-overview-scroll")
                            .max_h(px(340.0))
                            .overflow_y_scroll()
                            .track_scroll(self.scrolls.side_overview.handle())
                            .flex()
                            .flex_col()
                            .gap_2()
                            .children(sections),
                    )
                    .when(!self.modal_blocks_scroll(), |element| {
                        element.child(
                            smooth_overlay(
                                self.scrolls.side_overview.clone(),
                                scroll_entity.clone(),
                            )
                            .absolute()
                            .inset_0(),
                        )
                    }),
            )
            .into_any_element()
    }

    fn side_menu_row(
        &self,
        theme: &Theme,
        icon: &'static str,
        title: &str,
        hint: Option<&str>,
        cx: &mut Context<Self>,
        handler: impl Fn(&mut AppView, &mut Context<AppView>) + 'static,
    ) -> AnyElement {
        div()
            .id(SharedString::from(format!("side-menu-{title}")))
            .flex()
            .items_center()
            .gap_2()
            .h(px(30.0))
            .px_2()
            .rounded(px(7.0))
            .cursor_pointer()
            .hover(|style| style.bg(theme.hover))
            .on_click(cx.listener(move |this, _, _, cx| handler(this, cx)))
            .child(svg().path(icon).size(px(12.0)).text_color(theme.text_muted))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(11.5))
                    .text_color(theme.text)
                    .child(title.to_string()),
            )
            .when_some(hint.map(str::to_string), |element, hint| {
                element.child(badge(theme, hint, BadgeTone::Neutral))
            })
            .into_any_element()
    }

    pub(crate) fn on_side_tab_activated(&mut self, cx: &mut Context<Self>) {
        if self.side.active().map(|tab| &tab.kind) == Some(&SidePaneTabKind::Files) {
            if let Some(shared) = self.active_shared() {
                let cwd = lock_session(&shared).cwd.clone();
                if self.side.file_tree.root != cwd {
                    self.side.ensure_file_root(cwd);
                    self.side.file_tree.refresh();
                }
            }
        }
        if self.side.active().map(|tab| &tab.kind) == Some(&SidePaneTabKind::Changes) {
            self.refresh_git(cx);
        }
    }

    pub(crate) fn persist_side_pane(&mut self) {
        self.side.persist_into(&mut self.config);
        let _ = self.config.save();
    }
}

/// Small helper so `render_side_add_menu` can return a list of rows without
/// exposing a second element type.
struct ClassifiedRows(Vec<AnyElement>);

impl ClassifiedRows {
    fn into_element(self, _theme: &Theme) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .children(self.0)
            .into_any_element()
    }
}

fn stat_row(theme: &Theme, label: &str, value: &str) -> AnyElement {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_2()
        .child(
            div()
                .text_size(px(11.5))
                .text_color(theme.text_faint)
                .child(label.to_string()),
        )
        .child(
            div()
                .max_w(px(170.0))
                .truncate()
                .font_family(theme.mono_font())
                .text_size(px(11.0))
                .text_color(theme.text_muted)
                .child(value.to_string()),
        )
        .into_any_element()
}

fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}m", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn format_context_window(tokens: usize) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}m", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{}k", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_status_parser_handles_branch_and_renames() {
        let output = "## main...origin/main\n M crates/app/src/app.rs\n?? new file.txt\nR  old.rs -> new.rs\n";
        let (branch, files) = parse_git_status(output);
        assert_eq!(branch.as_deref(), Some("main"));
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].status_label(), "M");
        assert!(files[1].is_untracked());
        assert_eq!(files[2].path, "new.rs");
        assert_eq!(files[2].status_label(), "R");
    }

    #[test]
    fn closing_tab_selects_neighbor_and_tracks_recent() {
        let mut tabs = vec![
            SidePaneTab::plan(),
            SidePaneTab::agents(),
            SidePaneTab::files(),
        ];
        let active = tabs[1].id.clone();
        // Pure equivalent of SidePaneState::close_tab without GPUI entities.
        let index = tabs.iter().position(|tab| tab.id == active).unwrap();
        let removed = tabs.remove(index);
        assert_eq!(removed.id, "agents");
        let next = tabs.get(index).or_else(|| tabs.last()).unwrap();
        assert_eq!(next.id, "files");
    }

    #[test]
    fn persisted_keys_skip_agent_tabs() {
        assert_eq!(SidePaneTab::plan().persist_key(), Some("plan"));
        assert_eq!(
            SidePaneTab::agent("a1", "explore", "search").persist_key(),
            None
        );
    }

    #[test]
    fn parses_multi_file_diff_with_hunks_and_line_numbers() {
        let diff = "\
diff --git a/src/lib.rs b/src/lib.rs
index 83db48f..bf269f4 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,4 +1,5 @@
 fn main() {
-    println!(\"hello\");
+    println!(\"hello, world\");
+    println!(\"bye\");
     let x = 1;
 }
diff --git a/old.rs b/new.rs
similarity index 90%
rename from old.rs
rename to new.rs
@@ -3 +3 @@
-old line
+new line
";
        let sections = parse_unified_hunks(diff);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].header.len(), 4);
        assert_eq!(sections[0].hunks.len(), 1);
        assert!(!sections[0].binary);
        let rows = &sections[0].hunks[0].rows;
        assert_eq!(rows.len(), 6);
        assert_eq!(rows[0].kind, DiffRowKind::Context);
        assert_eq!(rows[0].old_no, Some(1));
        assert_eq!(rows[0].new_no, Some(1));
        assert_eq!(rows[1].kind, DiffRowKind::Delete);
        assert_eq!(rows[1].text, "    println!(\"hello\");");
        assert_eq!(rows[1].old_no, Some(2));
        assert_eq!(rows[2].kind, DiffRowKind::Insert);
        assert_eq!(rows[2].new_no, Some(2));
        assert_eq!(rows[3].kind, DiffRowKind::Insert);
        assert_eq!(rows[3].new_no, Some(3));
        assert_eq!(rows[4].kind, DiffRowKind::Context);
        assert_eq!(rows[4].old_no, Some(3));
        assert_eq!(rows[4].new_no, Some(4));
        assert_eq!(rows[5].old_no, Some(4));
        assert_eq!(rows[5].new_no, Some(5));
        // 第二个文件段：重命名头 + 无计数的 @@ 头。
        assert_eq!(sections[1].header.len(), 4);
        let rows = &sections[1].hunks[0].rows;
        assert_eq!(rows[0].kind, DiffRowKind::Delete);
        assert_eq!(rows[0].old_no, Some(3));
        assert_eq!(rows[1].kind, DiffRowKind::Insert);
        assert_eq!(rows[1].new_no, Some(3));
    }

    #[test]
    fn parses_untracked_synthetic_diff_as_inserts() {
        let diff = "\
diff --git a/notes.md b/notes.md
new file mode 100644
--- /dev/null
+++ b/notes.md
+# Title
+body
+++ extra
";
        let sections = parse_unified_hunks(diff);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].hunks.len(), 1);
        let rows = &sections[0].hunks[0].rows;
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].kind, DiffRowKind::Insert);
        assert_eq!(rows[0].text, "# Title");
        assert_eq!(rows[0].new_no, Some(1));
        assert_eq!(rows[1].text, "body");
        // `+++ ` 头之后出现的同前缀行是正文，不是新的文件头。
        assert_eq!(rows[2].kind, DiffRowKind::Insert);
        assert_eq!(rows[2].text, "++ extra");
        assert_eq!(rows[2].new_no, Some(3));
    }

    #[test]
    fn parses_binary_files_and_error_text() {
        let diff = "\
diff --git a/logo.png b/logo.png
index 1234567..89abcde 100644
Binary files a/logo.png and b/logo.png differ
";
        let sections = parse_unified_hunks(diff);
        assert_eq!(sections.len(), 1);
        assert!(sections[0].binary);
        assert!(sections[0].hunks.is_empty());

        let sections = parse_unified_hunks("没有可显示的 diff。");
        assert_eq!(sections.len(), 1);
        assert!(sections[0].header.is_empty());
        assert_eq!(sections[0].hunks.len(), 1);
        assert_eq!(sections[0].hunks[0].rows[0].kind, DiffRowKind::Note);
    }

    #[test]
    fn folds_long_context_runs_keeping_edges() {
        // 10 行连续上下文：前 3 行 + 折叠 4 行 + 后 3 行 = 7 个渲染单元。
        let rows: Vec<DiffRow> = (0..10)
            .map(|no| DiffRow::context(no + 1, no + 1, "unchanged"))
            .collect();
        let items = fold_hunk_rows(&annotate_hunk_rows(&rows));
        assert_eq!(items.len(), 7);
        match &items[3] {
            HunkItem::Folded { count, hidden } => {
                assert_eq!(*count, 4);
                assert_eq!(hidden.len(), 4);
            }
            _ => panic!("expected folded item"),
        }
        let HunkItem::Row(head_first, _) = &items[0] else {
            panic!("expected row item");
        };
        let HunkItem::Row(head_last, _) = &items[2] else {
            panic!("expected row item");
        };
        let HunkItem::Row(tail_first, _) = &items[4] else {
            panic!("expected row item");
        };
        let HunkItem::Row(tail_last, _) = &items[6] else {
            panic!("expected row item");
        };
        assert_eq!(head_first.old_no, Some(1));
        assert_eq!(head_last.old_no, Some(3));
        assert_eq!(tail_first.old_no, Some(8));
        assert_eq!(tail_last.old_no, Some(10));

        // 6 行以内的连续上下文不折叠（git 默认上下文即此量级）。
        let rows: Vec<DiffRow> = (0..6)
            .map(|no| DiffRow::context(no + 1, no + 1, "unchanged"))
            .collect();
        let items = fold_hunk_rows(&annotate_hunk_rows(&rows));
        assert_eq!(items.len(), 6);
        assert!(items.iter().all(|item| matches!(item, HunkItem::Row(..))));
    }

    #[test]
    fn fold_resumes_after_change_rows() {
        let rows = vec![
            DiffRow::delete(1, "old"),
            DiffRow::insert(1, "new"),
            DiffRow::context(2, 2, "a"),
            DiffRow::context(3, 3, "b"),
            DiffRow::insert(3, "x"),
        ];
        let items = fold_hunk_rows(&annotate_hunk_rows(&rows));
        assert_eq!(items.len(), 5);
        assert!(items.iter().all(|item| matches!(item, HunkItem::Row(..))));
    }

    #[test]
    fn word_diff_highlights_changed_words() {
        let (deleted, inserted) = word_diff_segments(
            &["let total = calculate(1, 2);"],
            &["let total = calculate(1, 3);"],
        );
        let deleted = deleted[0].as_ref().unwrap();
        let inserted = inserted[0].as_ref().unwrap();
        for (line, marker) in [(deleted, "2"), (inserted, "3")] {
            let LineDiff::Words(segments) = line else {
                panic!("expected word segments, got {line:?}");
            };
            assert!(
                segments
                    .iter()
                    .any(|segment| segment.tag == WordTag::Changed && segment.text.contains(marker))
            );
            let equal_text: String = segments
                .iter()
                .filter(|segment| segment.tag == WordTag::Equal)
                .map(|segment| segment.text.as_str())
                .collect();
            assert_eq!(equal_text, "let total = calculate(1, ");
        }
    }

    #[test]
    fn word_diff_plain_for_whole_line_replacement_and_lone_blocks() {
        // 整行替换：两侧都整行着色。
        let (deleted, inserted) = word_diff_segments(&["foo"], &["bar baz"]);
        assert_eq!(deleted[0].as_ref(), Some(&LineDiff::Plain));
        assert_eq!(inserted[0].as_ref(), Some(&LineDiff::Plain));

        // 纯新增块 / 纯删除块：整行着色。
        let (deleted, inserted) = word_diff_segments(&[], &["a", "b"]);
        assert!(deleted.is_empty());
        assert!(inserted.iter().all(|line| line.is_some()));
        let (deleted, inserted) = word_diff_segments(&["a"], &[]);
        assert!(deleted.iter().all(|line| line.is_some()));
        assert!(inserted.is_empty());
    }

    #[test]
    fn word_diff_handles_empty_line_boundaries() {
        // 旧块 2 行、新块 3 行：行级 diff 判定为相等的行不产词级信息。
        let (deleted, inserted) = word_diff_segments(&["a", ""], &["a", "", "b"]);
        assert_eq!(deleted.len(), 2);
        assert_eq!(inserted.len(), 3);
        assert!(deleted[0].is_none());
        assert_eq!(inserted[2].as_ref(), Some(&LineDiff::Plain));
    }

    #[test]
    fn expanded_hunk_keys_toggle() {
        let key = diff_fold_key("src/lib.rs", 0, 1, 2);
        assert_eq!(key, "src/lib.rs:0:1:2");
        let mut expanded = HashSet::new();
        toggle_expanded_hunk(&mut expanded, &key);
        assert!(expanded.contains(&key));
        toggle_expanded_hunk(&mut expanded, &key);
        assert!(expanded.is_empty());
    }
}
