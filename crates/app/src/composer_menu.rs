//! Composer autocomplete: the `/` command / skill / tool / agent menu, the
//! `@` file / subagent mention menu, and the attachment chip row.
//!
//! The model, adapted from ZCode's prompt editor:
//!
//! - A trigger only matches at the start of the text or after whitespace
//!   (`@` also after CJK text, so `看@src/main.rs` works), and its query never
//!   contains whitespace. That keeps `src/main.rs`, `https://…` and `a@b.com`
//!   from opening a menu.
//! - `/` lists skills, agent types and tools in that order; `$` lists skills
//!   only; `@` lists workspace files then agent types.
//! - Selecting an item inserts the canonical compact token (`/tool`, `$skill`,
//!   `@agent`, `@path`) and the send path interprets it. Skills additionally
//!   become an attachment so the whole skill body reaches the model, no matter
//!   where the SKILL.md lives.
//! - Workspace file search runs off the UI thread over a bounded index. The
//!   index is rebuilt when the workspace changes or after a short TTL.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use effibuddy_agent_core::{
    AttachmentKind, SubagentProfile, UserAttachment, merged_subagent_profiles,
};
use effibuddy_agent_tools::builtin_tools_with_roots;
use effibuddy_ui::{IconButton, Theme, icons, radius, shadow_float};
use gpui::prelude::*;
use gpui::{AnyElement, Context, IntoElement, MouseButton, SharedString, Window, div, px, svg};

use crate::app::AppView;
use crate::attachments::{self, MAX_ATTACHMENTS};
use crate::skills::{self, Skill};

/// The workspace walk is capped so a huge checkout cannot stall the search.
const MAX_INDEXED_FILES: usize = 20_000;
const MAX_WALK_DEPTH: usize = 12;
/// Files shown while the query is empty vs. after typing.
const FILE_PREVIEW_LIMIT: usize = 12;
const FILE_QUERY_LIMIT: usize = 60;
const GROUP_LIMIT: usize = 30;
const INDEX_TTL: Duration = Duration::from_secs(60);
const MENU_MAX_HEIGHT: f32 = 320.0;

const IGNORED_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "target",
    "node_modules",
    "dist",
    "build",
    ".next",
    ".turbo",
    "__pycache__",
    ".venv",
    "venv",
];

// --- trigger detection ------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ComposerTrigger {
    Command,
    Mention,
    Skill,
}

impl ComposerTrigger {
    fn from_sigil(sigil: char) -> Option<Self> {
        match sigil {
            '/' => Some(Self::Command),
            '@' => Some(Self::Mention),
            '$' => Some(Self::Skill),
            _ => None,
        }
    }

    pub(crate) fn sigil(self) -> char {
        match self {
            Self::Command => '/',
            Self::Mention => '@',
            Self::Skill => '$',
        }
    }

    fn empty_hint(self) -> &'static str {
        match self {
            Self::Command => "技能 / 智能体 / 工具",
            Self::Mention => "工作区文件 / 子智能体",
            Self::Skill => "技能",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ActiveTrigger {
    pub trigger: ComposerTrigger,
    /// Byte offset of the sigil.
    pub start: usize,
    /// Byte offset of the caret (end of the query).
    pub end: usize,
    pub query: String,
}

impl ActiveTrigger {
    pub(crate) fn token_range(&self) -> Range<usize> {
        self.start..self.end
    }

    fn signature(&self) -> String {
        format!("{}{}", self.trigger.sigil(), self.query)
    }
}

/// Return the trigger whose token ends at `cursor`, if any.
pub(crate) fn active_trigger(text: &str, cursor: usize) -> Option<ActiveTrigger> {
    let cursor = cursor.min(text.len());
    if !text.is_char_boundary(cursor) {
        return None;
    }
    let before = &text[..cursor];
    for (index, ch) in before.char_indices().rev() {
        if ch.is_whitespace() {
            return None;
        }
        let Some(trigger) = ComposerTrigger::from_sigil(ch) else {
            if is_query_char(ch) {
                continue;
            }
            return None;
        };
        let previous = before[..index].chars().next_back();
        if !trigger_allowed(trigger, previous) {
            // A slash inside a mention path (`@src/ma`) is not a command
            // trigger: keep scanning for the `@` that opened the token.
            if trigger == ComposerTrigger::Command {
                continue;
            }
            return None;
        }
        // Commands and skills are single identifiers; a slash means the token
        // is really a path (or plain prose).
        if trigger != ComposerTrigger::Mention && before[index + ch.len_utf8()..].contains('/') {
            return None;
        }
        return Some(ActiveTrigger {
            trigger,
            start: index,
            end: cursor,
            query: before[index + ch.len_utf8()..].to_string(),
        });
    }
    None
}

/// A sigil starts a token only at a word boundary. `@` additionally accepts
/// CJK text directly before it (`看@src/main.rs`), matching ZCode.
fn trigger_allowed(trigger: ComposerTrigger, previous: Option<char>) -> bool {
    match previous {
        None => true,
        Some(previous) if previous.is_whitespace() => true,
        Some(previous) if trigger == ComposerTrigger::Mention => !previous.is_ascii(),
        Some(_) => false,
    }
}

/// Characters that may appear inside a query. `/`, `.`, `-`, `_` and `\` are
/// included so paths can be typed; punctuation that ends a token is not.
fn is_query_char(ch: char) -> bool {
    !ch.is_whitespace()
        && !matches!(
            ch,
            ',' | ';'
                | ':'
                | '"'
                | '\''
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '<'
                | '>'
                | '|'
                | '`'
                | '。'
                | '，'
                | '；'
                | '：'
                | '（'
                | '）'
                | '【'
                | '】'
                | '《'
                | '》'
                | '、'
        )
}

/// Stable identity of the staged skill attachments. Used to avoid
/// re-serializing multi-megabyte file/image payloads when a send only
/// reconciled `$skill` tokens that were already staged.
fn staged_skill_names(attachments: &[UserAttachment]) -> Vec<String> {
    let mut names: Vec<String> = attachments
        .iter()
        .filter(|attachment| attachment.kind == AttachmentKind::Skill)
        .map(|attachment| skills::normalize_name(&attachment.name))
        .collect();
    names.sort_unstable();
    names
}

/// Every `$skill` token in a sent message. Manual typing must work, not only
/// menu insertions.
pub(crate) fn skill_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut start = None;
    for (index, ch) in text.char_indices() {
        match start {
            Some(token_start) => {
                if is_query_char(ch) && !matches!(ch, '/' | '@' | '$') {
                    continue;
                }
                if index > token_start {
                    tokens.push(text[token_start..index].to_string());
                }
                start = None;
                if ch == '$' {
                    start = Some(index + 1);
                }
            }
            None => {
                if ch == '$'
                    && trigger_allowed(ComposerTrigger::Skill, text[..index].chars().next_back())
                {
                    start = Some(index + 1);
                }
            }
        }
    }
    if let Some(token_start) = start {
        tokens.push(text[token_start..].to_string());
    }
    tokens
        .into_iter()
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
        .collect()
}

// --- catalog ----------------------------------------------------------------

#[derive(Clone)]
struct ToolEntry {
    name: String,
    description: String,
}

#[derive(Clone)]
struct AgentEntry {
    name: String,
    description: String,
    read_only: bool,
    source: &'static str,
}

#[derive(Clone)]
pub(crate) struct FileEntry {
    pub name: String,
    /// Workspace-relative, `/` separated.
    pub path: String,
}

#[derive(Clone)]
struct ComposerCatalog {
    workspace: PathBuf,
    skills: Vec<Skill>,
    tools: Vec<ToolEntry>,
    agents: Vec<AgentEntry>,
    files: Vec<FileEntry>,
    files_ready: bool,
    files_ready_at: Option<Instant>,
}

impl ComposerCatalog {
    fn load(workspace: &Path, include_computer: bool) -> Self {
        let skills = skills::load_skills(workspace);
        let mut tools = Vec::new();
        if let Ok(registry) = builtin_tools_with_roots(workspace, &[]) {
            tools.extend(registry.specs().into_iter().map(|spec| ToolEntry {
                name: spec.name,
                description: spec.description,
            }));
        }
        if include_computer {
            tools.push(ToolEntry {
                name: "computer".to_string(),
                description: "控制用户的桌面：读取屏幕截图并模拟鼠标键盘。".to_string(),
            });
        }
        tools.sort_by(|left, right| left.name.cmp(&right.name));
        let (profiles, _) =
            merged_subagent_profiles(&crate::config::base_dir().join("agents"), Some(workspace));
        let agents = profiles.into_iter().map(agent_entry).collect();
        Self {
            workspace: workspace.to_path_buf(),
            skills,
            tools,
            agents,
            files: Vec::new(),
            files_ready: false,
            files_ready_at: None,
        }
    }
}

fn agent_entry(profile: SubagentProfile) -> AgentEntry {
    let source = match profile.source {
        effibuddy_agent_core::ProfileSource::BuiltIn => "内置",
        effibuddy_agent_core::ProfileSource::User => "用户",
        effibuddy_agent_core::ProfileSource::Project => "项目",
    };
    AgentEntry {
        name: profile.name,
        description: profile.description,
        read_only: profile.read_only,
        source,
    }
}

/// Hidden directories that are implementation state, never user content.
/// Other dot-directories (`.github`, `.config`, `.vscode`) stay searchable.
const IGNORED_DOT_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".next",
    ".turbo",
    ".venv",
    ".cache",
    ".gradle",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".DS_Store",
    // App state (skills, agent profiles, image copies), not project content.
    ".effibuddy",
];

/// Bounded, symlink-safe depth-first walk. Build directories and VCS internals
/// are skipped; everything else (including `.github` and dotfiles) stays
/// searchable.
fn walk_workspace_files(root: &Path) -> Vec<FileEntry> {
    let mut files = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((directory, depth)) = stack.pop() {
        if files.len() >= MAX_INDEXED_FILES {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            if files.len() >= MAX_INDEXED_FILES {
                break;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                if depth >= MAX_WALK_DEPTH
                    || IGNORED_DIRS.contains(&name.as_str())
                    || (name.starts_with('.') && IGNORED_DOT_DIRS.contains(&name.as_str()))
                {
                    continue;
                }
                stack.push((entry.path(), depth + 1));
            } else if file_type.is_file()
                && let Ok(relative) = entry.path().strip_prefix(root)
            {
                files.push(FileEntry {
                    name,
                    path: attachments::normalize_relative(relative),
                });
            }
        }
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    files
}

// --- menu items -------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MenuGroup {
    Skill,
    Tool,
    Agent,
    File,
}

impl MenuGroup {
    fn title(self, trigger: ComposerTrigger) -> &'static str {
        match self {
            Self::Skill => "技能",
            Self::Tool => "工具",
            Self::Agent if trigger == ComposerTrigger::Mention => "子智能体",
            Self::Agent => "子智能体 · 要求使用",
            Self::File => "工作区文件",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::Skill => icons::SPARKLES,
            Self::Tool => icons::WRENCH,
            Self::Agent => icons::BRANCH,
            Self::File => icons::FILE,
        }
    }

    fn tint(self, theme: &Theme) -> gpui::Hsla {
        match self {
            Self::Skill => theme.agent_purple,
            Self::Tool => theme.text_muted,
            Self::Agent => theme.agent_blue,
            Self::File => theme.text_faint,
        }
    }
}

#[derive(Clone)]
pub(crate) struct MenuItem {
    pub group: MenuGroup,
    /// Text inserted into the composer (without the trailing space).
    pub insert: String,
    pub description: String,
    pub detail: String,
    /// Present for skills so selection can stage the body immediately.
    skill: Option<Skill>,
}

impl MenuItem {
    fn title(&self) -> String {
        match self.group {
            MenuGroup::File | MenuGroup::Agent => format!("@{}", self.insert),
            MenuGroup::Tool => format!("/{}", self.insert),
            MenuGroup::Skill => format!("${}", self.insert),
        }
    }
}

fn build_items(catalog: &ComposerCatalog, trigger: ComposerTrigger, query: &str) -> Vec<MenuItem> {
    let mut items = Vec::new();
    match trigger {
        ComposerTrigger::Skill => {
            append_skills(&mut items, &catalog.skills, query, &catalog.workspace);
        }
        ComposerTrigger::Command => {
            append_skills(&mut items, &catalog.skills, query, &catalog.workspace);
            append_tools(&mut items, &catalog.tools, query);
            append_agents(&mut items, &catalog.agents, query);
        }
        ComposerTrigger::Mention => {
            let limit = if query.is_empty() {
                FILE_PREVIEW_LIMIT
            } else {
                FILE_QUERY_LIMIT
            };
            items.extend(ranked_files(&catalog.files, query, limit));
            append_agents(&mut items, &catalog.agents, query);
        }
    }
    items
}

fn append_skills(items: &mut Vec<MenuItem>, catalog: &[Skill], query: &str, workspace: &Path) {
    let mut ranked: Vec<(u32, &Skill)> = catalog
        .iter()
        .filter_map(|skill| {
            score_item(query, &skill.name, &skill.description).map(|score| (score, skill))
        })
        .collect();
    ranked.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.name.cmp(&right.1.name))
    });
    for (_, skill) in ranked.into_iter().take(GROUP_LIMIT) {
        items.push(MenuItem {
            group: MenuGroup::Skill,
            insert: skill.name.clone(),
            description: truncate(&skill.description, 90),
            detail: format!(
                "{} · {}",
                skill.scope.label(),
                skill.display_path(workspace)
            ),
            skill: Some(skill.clone()),
        });
    }
}

fn append_tools(items: &mut Vec<MenuItem>, catalog: &[ToolEntry], query: &str) {
    let mut ranked: Vec<(u32, &ToolEntry)> = catalog
        .iter()
        .filter_map(|tool| {
            score_item(query, &tool.name, &tool.description).map(|score| (score, tool))
        })
        .collect();
    ranked.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.name.cmp(&right.1.name))
    });
    for (_, tool) in ranked.into_iter().take(GROUP_LIMIT) {
        items.push(MenuItem {
            group: MenuGroup::Tool,
            insert: tool.name.clone(),
            description: truncate(&tool.description, 90),
            detail: String::new(),
            skill: None,
        });
    }
}

fn append_agents(items: &mut Vec<MenuItem>, catalog: &[AgentEntry], query: &str) {
    let mut ranked: Vec<(u32, &AgentEntry)> = catalog
        .iter()
        .filter_map(|agent| {
            score_item(query, &agent.name, &agent.description).map(|score| (score, agent))
        })
        .collect();
    ranked.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.name.cmp(&right.1.name))
    });
    for (_, agent) in ranked.into_iter().take(GROUP_LIMIT) {
        items.push(MenuItem {
            group: MenuGroup::Agent,
            insert: agent.name.clone(),
            description: truncate(&agent.description, 90),
            detail: format!(
                "{} · {}{}",
                agent.source,
                if agent.read_only { "只读" } else { "可写" },
                ""
            ),
            skill: None,
        });
    }
}

fn ranked_files(files: &[FileEntry], query: &str, limit: usize) -> Vec<MenuItem> {
    let mut ranked: Vec<(u32, &FileEntry)> = files
        .iter()
        .filter_map(|file| file_score(query, file).map(|score| (score, file)))
        .collect();
    ranked.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            // Shallow files first: the empty-query preview should show the
            // files people actually mention, not whatever sorts first.
            .then_with(|| path_depth(&left.1.path).cmp(&path_depth(&right.1.path)))
            .then_with(|| left.1.path.cmp(&right.1.path))
    });
    ranked
        .into_iter()
        .take(limit)
        .map(|(_, file)| MenuItem {
            group: MenuGroup::File,
            insert: file.path.clone(),
            description: String::new(),
            detail: parent_dir(&file.path),
            skill: None,
        })
        .collect()
}

fn path_depth(path: &str) -> usize {
    path.matches('/').count()
}

fn parent_dir(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((parent, _)) => parent.to_string(),
        None => String::new(),
    }
}

fn score_item(query: &str, label: &str, description: &str) -> Option<u32> {
    if query.is_empty() {
        return Some(0);
    }
    let mut best = fuzzy_score(query, label);
    if let Some(score) = fuzzy_score(query, description) {
        best = Some(best.map_or(score + 120, |best| best.min(score + 120)));
    }
    best
}

fn file_score(query: &str, file: &FileEntry) -> Option<u32> {
    if query.is_empty() {
        return Some(0);
    }
    let name = fuzzy_score(query, &file.name).map(|score| score + 20);
    let path = fuzzy_score(query, &file.path);
    match (name, path) {
        (Some(name), Some(path)) => Some(name.min(path)),
        (Some(score), None) | (None, Some(score)) => Some(score),
        (None, None) => None,
    }
}

/// Prefix < substring < ordered subsequence, with the same rough weights as
/// ZCode's prompt input. CJK queries never use subsequence matching: it turns
/// almost any Chinese string into a hit.
pub(crate) fn fuzzy_score(query: &str, target: &str) -> Option<u32> {
    if query.is_empty() {
        return Some(0);
    }
    let query = query.to_lowercase();
    let target_lower = target.to_lowercase();
    if let Some(prefix) = target_lower.strip_prefix(&query) {
        return Some((target_lower.len() - prefix.len()) as u32);
    }
    if let Some(index) = target_lower.find(&query) {
        return Some(100 + index as u32);
    }
    if query.chars().any(|ch| !ch.is_ascii()) {
        return None;
    }
    let mut chars = query.chars();
    let mut wanted = chars.next();
    let mut gaps = 0u32;
    for ch in target_lower.chars() {
        let Some(needle) = wanted else {
            return Some(200 + gaps);
        };
        if ch == needle {
            wanted = chars.next();
        } else if wanted.is_some() {
            gaps += 1;
        }
    }
    wanted.is_none().then_some(200 + gaps)
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let mut out: String = value.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

// --- state ------------------------------------------------------------------

pub(crate) struct ComposerMenuState {
    pub open: bool,
    pub trigger: ComposerTrigger,
    pub query: String,
    /// Byte range of the active token, replaced on selection.
    pub token_range: Range<usize>,
    pub items: Vec<MenuItem>,
    pub selected: usize,
    /// Signature (sigil + query) the selection belongs to.
    selected_for: String,
    /// Dismissed token signature; the menu stays closed until it changes.
    dismissed: Option<String>,
    catalog: Option<Rc<ComposerCatalog>>,
    file_generation: u64,
    file_loading: bool,
    pub scroll: gpui::ScrollHandle,
    /// Index (items only, no group headers) of the first visible row, for the
    /// "N results" footer.
    pub loading_hint: bool,
}

impl Default for ComposerMenuState {
    fn default() -> Self {
        Self {
            open: false,
            trigger: ComposerTrigger::Command,
            query: String::new(),
            token_range: 0..0,
            items: Vec::new(),
            selected: 0,
            selected_for: String::new(),
            dismissed: None,
            catalog: None,
            file_generation: 0,
            file_loading: false,
            scroll: gpui::ScrollHandle::new(),
            loading_hint: false,
        }
    }
}

impl ComposerMenuState {
    pub(crate) fn close(&mut self) {
        self.open = false;
        self.items.clear();
        self.query.clear();
        self.token_range = 0..0;
        self.loading_hint = false;
    }
}

// --- AppView integration ----------------------------------------------------

impl AppView {
    pub(crate) fn composer_workspace(&self) -> PathBuf {
        self.active
            .as_ref()
            .map(|active| {
                effibuddy_agent_core::lock_session(&active.shared)
                    .cwd
                    .clone()
            })
            .unwrap_or_else(|| self.config.workspace())
    }

    /// Recompute the menu from the composer's text and caret. Called from the
    /// composer observer on every edit.
    pub(crate) fn sync_composer_menu(&mut self, cx: &mut Context<Self>) {
        let (text, cursor, selection_empty, composing) = {
            let state = self.composer_state.read(cx);
            (
                state.content().to_string(),
                state.cursor_offset(),
                state.selected_range().is_empty(),
                state.is_composing(),
            )
        };
        if composing {
            // IME marked text is not a finished token; stay out of its way.
            if self.composer_menu.open {
                self.composer_menu.close();
            }
            return;
        }
        // A staged skill stops being active as soon as its `$name` token is
        // edited away. Prune live so the chip row cannot lie about what will
        // be sent; manually typed tokens are staged at send time instead.
        if self
            .composer_attachments
            .iter()
            .any(|attachment| attachment.kind == AttachmentKind::Skill)
        {
            let tokens: Vec<String> = skill_tokens(&text)
                .into_iter()
                .map(|token| skills::normalize_name(&token))
                .collect();
            let before = self.composer_attachments.len();
            self.composer_attachments.retain(|attachment| {
                attachment.kind != AttachmentKind::Skill
                    || tokens.contains(&skills::normalize_name(&attachment.name))
            });
            if self.composer_attachments.len() != before {
                self.persist_composer_attachments(cx);
                cx.notify();
            }
        }
        let active = if selection_empty {
            active_trigger(&text, cursor)
        } else {
            None
        };
        let Some(active) = active else {
            if self.composer_menu.open {
                // A dismissed token is irrelevant once the trigger is gone.
                self.composer_menu.close();
                self.composer_menu.dismissed = None;
            }
            return;
        };
        if self.composer_menu.dismissed.as_deref() == Some(active.signature().as_str()) {
            if self.composer_menu.open {
                self.composer_menu.close();
            }
            return;
        }
        let workspace = self.composer_workspace();
        self.ensure_composer_catalog(&workspace, cx);
        let trigger = active.trigger;
        let signature = active.signature();
        if self.composer_menu.selected_for != signature {
            self.composer_menu.selected = 0;
            self.composer_menu.selected_for = signature;
        }
        let items = match self.composer_menu.catalog.as_ref() {
            Some(catalog) => build_items(catalog, trigger, &active.query),
            None => Vec::new(),
        };
        self.composer_menu.open = true;
        self.composer_menu.trigger = trigger;
        self.composer_menu.token_range = active.token_range();
        self.composer_menu.query = active.query;
        self.composer_menu.loading_hint = self
            .composer_menu
            .catalog
            .as_ref()
            .map(|catalog| !catalog.files_ready)
            .unwrap_or(false)
            && matches!(trigger, ComposerTrigger::Mention);
        if self.composer_menu.selected >= items.len() {
            self.composer_menu.selected = items.len().saturating_sub(1);
        }
        self.composer_menu.items = items;
    }

    fn ensure_composer_catalog(&mut self, workspace: &Path, cx: &mut Context<Self>) {
        let rebuild = self
            .composer_menu
            .catalog
            .as_ref()
            .map(|catalog| catalog.workspace != workspace)
            .unwrap_or(true);
        if rebuild {
            self.composer_menu.file_generation += 1;
            let generation = self.composer_menu.file_generation;
            self.composer_menu.catalog = Some(Rc::new(ComposerCatalog::load(
                workspace,
                self.config.computer_use_mode.is_enabled(),
            )));
            self.composer_menu.file_loading = false;
            self.spawn_file_index(workspace.to_path_buf(), generation, cx);
            return;
        }
        let needs_index = self
            .composer_menu
            .catalog
            .as_ref()
            .map(|catalog| {
                !catalog.files_ready
                    || catalog
                        .files_ready_at
                        .map(|at| at.elapsed() > INDEX_TTL)
                        .unwrap_or(true)
            })
            .unwrap_or(true);
        if needs_index && !self.composer_menu.file_loading {
            let generation = self.composer_menu.file_generation;
            self.spawn_file_index(workspace.to_path_buf(), generation, cx);
        }
    }

    fn spawn_file_index(&mut self, workspace: PathBuf, generation: u64, cx: &mut Context<Self>) {
        self.composer_menu.file_loading = true;
        cx.spawn(async move |weak, cx| {
            let root = workspace.clone();
            let files = cx
                .background_executor()
                .spawn(async move { walk_workspace_files(&root) })
                .await;
            let _ = weak.update(cx, |this, cx| {
                if this.composer_menu.file_generation != generation {
                    return;
                }
                let Some(catalog) = this.composer_menu.catalog.as_ref() else {
                    return;
                };
                if catalog.workspace != workspace {
                    return;
                }
                let mut next = (**catalog).clone();
                next.files = files;
                next.files_ready = true;
                next.files_ready_at = Some(Instant::now());
                this.composer_menu.catalog = Some(Rc::new(next));
                this.composer_menu.file_loading = false;
                this.refresh_menu_items(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn refresh_menu_items(&mut self, _cx: &mut Context<Self>) {
        if !self.composer_menu.open {
            return;
        }
        let Some(catalog) = self.composer_menu.catalog.as_ref() else {
            return;
        };
        let items = build_items(
            catalog,
            self.composer_menu.trigger,
            &self.composer_menu.query,
        );
        let selected = self
            .composer_menu
            .selected
            .min(items.len().saturating_sub(1));
        self.composer_menu.items = items;
        self.composer_menu.selected = selected;
    }

    pub(crate) fn move_composer_menu_selection(&mut self, delta: i32, cx: &mut Context<Self>) {
        let count = self.composer_menu.items.len();
        if count == 0 {
            return;
        }
        let next = (self.composer_menu.selected as i32 + delta).rem_euclid(count as i32);
        self.composer_menu.selected = next as usize;
        self.composer_menu.scroll.scroll_to_item(menu_row_index(
            &self.composer_menu.items,
            self.composer_menu.selected,
        ));
        cx.notify();
    }

    pub(crate) fn dismiss_composer_menu(&mut self, cx: &mut Context<Self>) {
        if !self.composer_menu.open {
            return;
        }
        self.composer_menu.dismissed = Some(format!(
            "{}{}",
            self.composer_menu.trigger.sigil(),
            self.composer_menu.query
        ));
        self.composer_menu.close();
        cx.notify();
    }

    pub(crate) fn accept_composer_menu_item(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = self.composer_menu.items.get(index).cloned() else {
            return;
        };
        let range = self.composer_menu.token_range.clone();
        if range.end > self.composer_state.read(cx).content().len() {
            self.composer_menu.close();
            return;
        }
        let insert = format!("{} ", item.insert);
        self.composer_state.update(cx, |state, cx| {
            state.replace_byte_range(range, &insert, window, cx);
        });
        self.composer_menu.dismissed = None;
        self.composer_menu.close();
        if let Some(skill) = item.skill.clone() {
            self.stage_skill(&skill, cx);
        }
        self.composer_focus_handle(cx).focus(window);
        self.sync_composer_menu(cx);
        cx.notify();
    }

    /// Stage an activated skill as an attachment so its body reaches the model.
    fn stage_skill(&mut self, skill: &Skill, cx: &mut Context<Self>) {
        let workspace = self.composer_workspace();
        if self.composer_attachments.iter().any(|attachment| {
            attachment.kind == AttachmentKind::Skill
                && skills::normalize_name(&attachment.name) == skills::normalize_name(&skill.name)
        }) {
            return;
        }
        if self.composer_attachments.len() >= MAX_ATTACHMENTS {
            self.notice = Some(format!("最多附加 {MAX_ATTACHMENTS} 个附件。"));
            return;
        }
        match attachments::skill_attachment(skill, &workspace) {
            Ok(attachment) => {
                self.composer_attachments.push(attachment);
                self.persist_composer_attachments(cx);
                cx.notify();
            }
            Err(error) => {
                self.notice = Some(error);
                cx.notify();
            }
        }
    }

    /// Keep staged skills in sync with the `$skill` tokens that survived in the
    /// text, and stage manually typed ones. Called right before sending.
    pub(crate) fn sync_skill_attachments(&mut self, text: &str, cx: &mut Context<Self>) {
        let tokens: Vec<String> = skill_tokens(text)
            .into_iter()
            .map(|token| skills::normalize_name(&token))
            .collect();
        let before = staged_skill_names(&self.composer_attachments);
        self.composer_attachments.retain(|attachment| {
            attachment.kind != AttachmentKind::Skill
                || tokens.contains(&skills::normalize_name(&attachment.name))
        });
        if tokens.is_empty() {
            if staged_skill_names(&self.composer_attachments) != before {
                self.persist_composer_attachments(cx);
            }
            return;
        }
        let workspace = self.composer_workspace();
        let catalog = skills::load_skills(&workspace);
        for token in tokens {
            if self.composer_attachments.iter().any(|attachment| {
                attachment.kind == AttachmentKind::Skill
                    && skills::normalize_name(&attachment.name) == token
            }) {
                continue;
            }
            let Some(skill) = catalog
                .iter()
                .find(|skill| skills::normalize_name(&skill.name) == token)
            else {
                continue;
            };
            if self.composer_attachments.len() >= MAX_ATTACHMENTS {
                break;
            }
            match attachments::skill_attachment(skill, &workspace) {
                Ok(attachment) => self.composer_attachments.push(attachment),
                Err(error) => self.notice = Some(error),
            }
        }
        if staged_skill_names(&self.composer_attachments) != before {
            self.persist_composer_attachments(cx);
        }
        cx.notify();
    }

    pub(crate) fn attach_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let workspace = self.composer_workspace();
        let resolved = self.active_resolved_model();
        let accepts_images = resolved.model.capabilities.vision
            || self.models.demo_mode(self.config.use_demo, &resolved);
        let mut added = 0usize;
        let mut errors: Vec<String> = Vec::new();
        for path in paths {
            if self.composer_attachments.len() >= MAX_ATTACHMENTS {
                errors.push(format!("最多附加 {MAX_ATTACHMENTS} 个附件。"));
                break;
            }
            match attachments::attachment_from_path(&path, &workspace) {
                Ok(attachment) => {
                    if attachment.kind == AttachmentKind::Image && !accepts_images {
                        errors.push(format!("当前模型不支持图片，未添加 {}。", attachment.name));
                        continue;
                    }
                    // Compare the resulting path: external images dedupe onto
                    // their content-addressed workspace copy, not the source.
                    if self
                        .composer_attachments
                        .iter()
                        .any(|existing| existing.path == attachment.path)
                    {
                        continue;
                    }
                    self.composer_attachments.push(attachment);
                    added += 1;
                }
                Err(error) => errors.push(error),
            }
        }
        if added > 0 {
            self.notice = Some(format!("已添加 {added} 个文件。"));
            self.persist_composer_attachments(cx);
        }
        if let Some(error) = errors.first() {
            self.last_error = Some(error.clone());
        }
        cx.notify();
    }

    /// Stage clipboard content that the editor cannot type: a screenshot or a
    /// copied file. `Ctrl+V` in the composer routes here, so pasting a
    /// screenshot behaves like dropping the same image.
    pub(crate) fn paste_media(
        &mut self,
        media: effibuddy_ui::text_input::PastedMedia,
        cx: &mut Context<Self>,
    ) {
        match media {
            effibuddy_ui::text_input::PastedMedia::Paths(paths) => self.attach_paths(paths, cx),
            effibuddy_ui::text_input::PastedMedia::Image { mime_type, bytes } => {
                let resolved = self.active_resolved_model();
                let accepts_images = resolved.model.capabilities.vision
                    || self.models.demo_mode(self.config.use_demo, &resolved);
                if !accepts_images {
                    self.last_error = Some(
                        "当前模型不支持图片，剪贴板中的图片未添加。请在设置里切换到支持图片的模型。"
                            .to_string(),
                    );
                    cx.notify();
                    return;
                }
                if self.composer_attachments.len() >= MAX_ATTACHMENTS {
                    self.last_error = Some(format!("最多附加 {MAX_ATTACHMENTS} 个附件。"));
                    cx.notify();
                    return;
                }
                let workspace = self.composer_workspace();
                match attachments::attachment_from_image_bytes(&mime_type, bytes, &workspace) {
                    Ok(attachment) => {
                        // Pasting the same screenshot twice should not add a
                        // second identical chip.
                        if self
                            .composer_attachments
                            .iter()
                            .any(|existing| existing.path == attachment.path)
                        {
                            return;
                        }
                        self.composer_attachments.push(attachment);
                        self.persist_composer_attachments(cx);
                        self.notice = Some("已添加剪贴板图片。".to_string());
                        cx.notify();
                    }
                    Err(error) => {
                        self.last_error = Some(error);
                        cx.notify();
                    }
                }
            }
        }
    }

    pub(crate) fn remove_composer_attachment(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.composer_attachments.len() {
            self.composer_attachments.remove(index);
            self.persist_composer_attachments(cx);
            cx.notify();
        }
    }

    pub(crate) fn pick_composer_files(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("添加文件到会话".into()),
        });
        cx.spawn(async move |weak, cx| {
            let Ok(result) = receiver.await else {
                return;
            };
            let _ = weak.update(cx, |this, cx| match result {
                Ok(Some(paths)) => this.attach_paths(paths, cx),
                Ok(None) => {}
                Err(error) => {
                    this.last_error = Some(format!("无法打开文件选择器：{error}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(crate) fn composer_has_attachments(&self) -> bool {
        !self.composer_attachments.is_empty()
    }

    // --- rendering ----------------------------------------------------------

    pub(crate) fn render_composer_menu(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.composer_menu.open {
            return None;
        }
        if !self
            .composer_state
            .read(cx)
            .focus_handle()
            .is_focused(window)
        {
            return None;
        }
        let theme = &self.theme;
        let trigger = self.composer_menu.trigger;
        let items = &self.composer_menu.items;
        let selected = self
            .composer_menu
            .selected
            .min(items.len().saturating_sub(1));
        let mut rows: Vec<AnyElement> = Vec::with_capacity(items.len() + 4);
        let mut current: Option<MenuGroup> = None;
        for (index, item) in items.iter().enumerate() {
            if current != Some(item.group) {
                current = Some(item.group);
                rows.push(
                    div()
                        .pt(px(8.0))
                        .pb(px(2.0))
                        .px(px(10.0))
                        .text_size(px(10.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text_faint)
                        .child(item.group.title(trigger))
                        .into_any_element(),
                );
            }
            let is_selected = index == selected;
            let title = item.title();
            rows.push(
                div()
                    .id(SharedString::from(format!("composer-menu-{index}")))
                    .flex()
                    .items_center()
                    .gap(px(9.0))
                    .h(px(36.0))
                    .px(px(10.0))
                    .rounded(px(radius::SM))
                    .cursor_pointer()
                    .when(is_selected, |element| element.bg(theme.accent_soft))
                    .when(!is_selected, |element| {
                        element.hover(|style| style.bg(theme.hover))
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            this.accept_composer_menu_item(index, window, cx);
                        }),
                    )
                    .child(
                        svg()
                            .path(item.group.icon())
                            .size(px(13.0))
                            .text_color(item.group.tint(theme))
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
                                    .text_size(px(12.5))
                                    .font_weight(if is_selected {
                                        gpui::FontWeight::MEDIUM
                                    } else {
                                        gpui::FontWeight::NORMAL
                                    })
                                    .text_color(theme.text)
                                    .child(title),
                            )
                            .when(!item.description.is_empty(), |element| {
                                element.child(
                                    div()
                                        .truncate()
                                        .text_size(px(10.5))
                                        .text_color(theme.text_faint)
                                        .child(item.description.clone()),
                                )
                            }),
                    )
                    .when(!item.detail.is_empty(), |element| {
                        element.child(
                            div()
                                .flex_none()
                                .max_w(px(220.0))
                                .truncate()
                                .text_size(px(10.0))
                                .text_color(theme.text_faint)
                                .child(item.detail.clone()),
                        )
                    })
                    .into_any_element(),
            );
        }

        let skills_hint = matches!(trigger, ComposerTrigger::Command | ComposerTrigger::Skill)
            && self
                .composer_menu
                .catalog
                .as_ref()
                .is_some_and(|catalog| catalog.skills.is_empty());
        let footer = div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(8.0))
            .px(px(10.0))
            .pt(px(6.0))
            .text_size(px(10.0))
            .text_color(theme.text_faint)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(if items.is_empty() {
                        "没有匹配项"
                    } else {
                        trigger.empty_hint()
                    })
                    .when(self.composer_menu.loading_hint, |element| {
                        element.child("· 正在索引工作区文件…")
                    })
                    .when(skills_hint, |element| {
                        element.child("· 未发现技能（.effibuddy/skills）")
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child("↑↓ 选择")
                    .child("Enter/Tab 插入")
                    .child("Esc 关闭"),
            );

        Some(
            div()
                .w_full()
                .max_w(px(760.0))
                .max_h(px(MENU_MAX_HEIGHT))
                .rounded(px(radius::XL))
                .border_1()
                .border_color(theme.border)
                .bg(theme.elevated)
                .shadow(shadow_float(theme))
                .p(px(6.0))
                .flex()
                .flex_col()
                .child(
                    div()
                        .id("composer-menu-list")
                        .flex_1()
                        .min_h(px(0.0))
                        .overflow_y_scroll()
                        .track_scroll(&self.composer_menu.scroll)
                        .flex()
                        .flex_col()
                        .gap(px(1.0))
                        .children(rows),
                )
                .child(footer)
                .into_any_element(),
        )
    }

    pub(crate) fn render_composer_attachments(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.composer_attachments.is_empty() {
            return None;
        }
        let theme = &self.theme;
        let mut chips: Vec<AnyElement> = Vec::with_capacity(self.composer_attachments.len());
        for (index, attachment) in self.composer_attachments.iter().enumerate() {
            let title = attachments::chip_title(attachment);
            let icon = match attachment.kind {
                AttachmentKind::Skill => icons::SPARKLES,
                AttachmentKind::Image => icons::FILE,
                AttachmentKind::File => icons::FILE,
            };
            let tint = match attachment.kind {
                AttachmentKind::Skill => theme.agent_purple,
                AttachmentKind::Image => theme.agent_green,
                AttachmentKind::File => theme.text_muted,
            };
            let detail = format!(
                "{} · {}{}",
                attachments::kind_label(attachment.kind),
                attachments::format_bytes(attachment.size),
                if attachment.truncated {
                    " · 已截断"
                } else {
                    ""
                }
            );
            chips.push(
                div()
                    .id(SharedString::from(format!("composer-attachment-{index}")))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .h(px(28.0))
                    .pl(px(8.0))
                    .pr(px(3.0))
                    .rounded(px(radius::SM))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.surface_alt)
                    .child(svg().path(icon).size(px(12.0)).text_color(tint).flex_none())
                    .child(
                        div()
                            .max_w(px(190.0))
                            .truncate()
                            .text_size(px(11.5))
                            .text_color(theme.text)
                            .child(title),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(10.0))
                            .text_color(theme.text_faint)
                            .child(detail),
                    )
                    .child(
                        IconButton::new(
                            SharedString::from(format!("composer-attachment-remove-{index}")),
                            icons::X,
                            theme,
                        )
                        .size(20.0, 10.0)
                        .tooltip("移除附件")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.remove_composer_attachment(index, cx);
                        })),
                    )
                    .into_any_element(),
            );
        }
        Some(
            div()
                .w_full()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(6.0))
                .children(chips)
                .into_any_element(),
        )
    }
}

/// Row index of the selected item inside the scroll container, counting the
/// group headers that precede it. `ScrollHandle::scroll_to_item` addresses
/// direct children, not model items.
fn menu_row_index(items: &[MenuItem], selected: usize) -> usize {
    let mut row = 0usize;
    let mut current: Option<MenuGroup> = None;
    for (index, item) in items.iter().enumerate() {
        if current != Some(item.group) {
            current = Some(item.group);
            row += 1;
        }
        if index == selected {
            return row;
        }
        row += 1;
    }
    row.saturating_sub(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> ComposerCatalog {
        ComposerCatalog {
            workspace: PathBuf::from("/w"),
            skills: vec![
                Skill {
                    name: "review".to_string(),
                    description: "Strict review".to_string(),
                    path: PathBuf::from("/w/.effibuddy/skills/review/SKILL.md"),
                    scope: crate::skills::SkillScope::Project,
                },
                Skill {
                    name: "lint".to_string(),
                    description: "Lint code".to_string(),
                    path: PathBuf::from("/w/.effibuddy/skills/lint/SKILL.md"),
                    scope: crate::skills::SkillScope::Project,
                },
            ],
            tools: vec![ToolEntry {
                name: "read_file".to_string(),
                description: "Read a file".to_string(),
            }],
            agents: vec![AgentEntry {
                name: "explore".to_string(),
                description: "Search the repo".to_string(),
                read_only: true,
                source: "内置",
            }],
            files: vec![
                FileEntry {
                    name: "app.rs".to_string(),
                    path: "crates/app/src/app.rs".to_string(),
                },
                FileEntry {
                    name: "README.md".to_string(),
                    path: "README.md".to_string(),
                },
            ],
            files_ready: true,
            files_ready_at: None,
        }
    }

    #[test]
    fn command_menu_lists_skills_tools_and_agents() {
        let items = build_items(&catalog(), ComposerTrigger::Command, "");
        let groups: Vec<MenuGroup> = items.iter().map(|item| item.group).collect();
        assert_eq!(
            groups,
            vec![
                MenuGroup::Skill,
                MenuGroup::Skill,
                MenuGroup::Tool,
                MenuGroup::Agent
            ]
        );
        assert_eq!(items[0].insert, "lint");
        assert_eq!(items[0].title(), "$lint");
        assert_eq!(items[2].title(), "/read_file");
        assert_eq!(items[3].title(), "@explore");
        assert!(items[0].skill.is_some());
    }

    #[test]
    fn mention_menu_filters_files_and_agents_by_query() {
        let items = build_items(&catalog(), ComposerTrigger::Mention, "read");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].group, MenuGroup::File);
        assert_eq!(items[0].insert, "README.md");
        assert_eq!(items[0].title(), "@README.md");

        let items = build_items(&catalog(), ComposerTrigger::Mention, "exp");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].group, MenuGroup::Agent);
        assert_eq!(items[0].insert, "explore");
    }

    #[test]
    fn empty_mention_preview_prefers_shallow_files() {
        let items = build_items(&catalog(), ComposerTrigger::Mention, "");
        assert_eq!(items[0].insert, "README.md");
        assert_eq!(items[1].insert, "crates/app/src/app.rs");
    }

    #[test]
    fn triggers_only_fire_at_word_boundaries() {
        let text = "please /rev";
        let active = active_trigger(text, text.len()).unwrap();
        assert_eq!(active.trigger, ComposerTrigger::Command);
        assert_eq!(active.query, "rev");
        assert_eq!(active.start, 7);

        let text = "看看 @src/ma";
        let active = active_trigger(text, text.len()).unwrap();
        assert_eq!(active.trigger, ComposerTrigger::Mention);
        assert_eq!(active.query, "src/ma");

        let text = "看@src/ma";
        assert_eq!(active_trigger(text, text.len()).unwrap().query, "src/ma");

        assert!(active_trigger("src/main.rs", 11).is_none());
        assert!(active_trigger("https://example.com", 19).is_none());
        assert!(active_trigger("foo@bar.com", 11).is_none());
        assert!(active_trigger("done /rev ", 10).is_none());
        assert!(active_trigger("$skill", 6).unwrap().trigger == ComposerTrigger::Skill);
    }

    #[test]
    fn slashes_inside_mentions_are_part_of_the_query() {
        // `@src/ma` must survive the `/` that belongs to the path.
        assert_eq!(active_trigger("@src/ma", 7).unwrap().query, "src/ma");
        assert_eq!(
            active_trigger("看 @crates/app/src", "看 @crates/app/src".len())
                .unwrap()
                .query,
            "crates/app/src"
        );
        // A command/skill name never contains a slash.
        assert!(active_trigger("$foo/bar", 8).is_none());
        assert!(active_trigger("please /rev", 11).is_some());
    }

    #[test]
    fn trigger_query_stops_at_whitespace_and_punctuation() {
        let text = "/read_file src";
        assert_eq!(active_trigger(text, 10).unwrap().query, "read_file");
        let text = "看 @main.rs，";
        assert!(active_trigger(text, text.len()).is_none());
    }

    #[test]
    fn skill_tokens_include_manual_and_menu_insertions() {
        assert_eq!(
            skill_tokens("用 $review 看看 src/main.rs"),
            vec!["review".to_string()]
        );
        assert_eq!(
            skill_tokens("$lint\n$review"),
            vec!["lint".to_string(), "review".to_string()]
        );
        assert!(skill_tokens("a$b").is_empty());
    }

    #[test]
    fn fuzzy_scoring_prefers_prefix_then_substring() {
        assert_eq!(fuzzy_score("app", "app.rs"), Some(3));
        assert!(fuzzy_score("app", "src/app.rs").unwrap() > 100);
        assert!(fuzzy_score("xx", "app.rs").is_none());
        assert_eq!(fuzzy_score("src", "src/main.rs"), Some(3));
        assert!(fuzzy_score("源", "资源文件").is_some());
        assert!(fuzzy_score("源", "src/app.rs").is_none());
    }

    #[test]
    fn menu_row_index_counts_headers() {
        let items = vec![
            MenuItem {
                group: MenuGroup::Skill,
                insert: "a".into(),
                description: String::new(),
                detail: String::new(),
                skill: None,
            },
            MenuItem {
                group: MenuGroup::Tool,
                insert: "b".into(),
                description: String::new(),
                detail: String::new(),
                skill: None,
            },
        ];
        assert_eq!(menu_row_index(&items, 0), 1);
        assert_eq!(menu_row_index(&items, 1), 3);
    }
}
