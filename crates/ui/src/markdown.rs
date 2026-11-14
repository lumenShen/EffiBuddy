//! Markdown rendering for assistant messages.
//!
//! A deliberately small renderer: paragraphs, headings, emphasis, inline code,
//! fenced code blocks (with copy), lists, quotes, rules and tables.
//!
//! Rendering is two-stage: pulldown-cmark events are first reduced into a
//! theme-independent IR ([`BlockIr`]), and the IR is turned into GPUI
//! elements on every frame. GPUI cannot reuse elements across frames, so the
//! IR — not the elements — is what gets memoized:
//!
//! * Complete documents are cached whole ([`render_markdown`],
//!   [`render_markdown_keyed`]) keyed by content hash, length and the theme
//!   fields that leak into highlights. A historical message costs one hash
//!   lookup per frame instead of a re-parse.
//! * Streaming documents are split into fence-safe chunks ([`chunk_ranges`])
//!   and cached per chunk ([`render_markdown_streaming`]): every frame only
//!   re-parses the chunk that is currently being typed.
//!
//! Malformed input degrades to plain text rather than failing the view.

use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::rc::Rc;

use gpui::{
    AnyElement, ClipboardItem, FontStyle, FontWeight, HighlightStyle, IntoElement, SharedString,
    StrikethroughStyle, StyledText, UnderlineStyle, div, prelude::*, px, svg,
};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::assets::icons;
use crate::table::{TableCollector, TableData, render_table};
use crate::theme::{Theme, radius, text as text_size};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InlineKind {
    Strong,
    Emphasis,
    Strikethrough,
    Link,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TextIr {
    text: SharedString,
    highlights: Rc<[(Range<usize>, HighlightStyle)]>,
    size: f32,
    weight: FontWeight,
    /// Headings keep the small top padding the renderer has always applied.
    heading: bool,
    /// Blockquote-wrapped paragraphs get the border-left treatment.
    quote: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ItemIr {
    marker: SharedString,
    indent: usize,
    text: SharedString,
    highlights: Rc<[(Range<usize>, HighlightStyle)]>,
}

/// One parsed block, independent of the theme and of GPUI elements so it can
/// be cached across frames.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum BlockIr {
    Text(TextIr),
    /// A list row produced outside any open list container.
    Item(Rc<ItemIr>),
    List {
        items: Rc<[Rc<ItemIr>]>,
        quote: bool,
    },
    Code {
        language: Option<SharedString>,
        code: SharedString,
        code_hash: u64,
    },
    Table(TableData),
    Rule,
}

type Blocks = Rc<[Rc<BlockIr>]>;

#[derive(Clone, Default)]
struct InlineBuffer {
    text: String,
    highlights: Vec<(Range<usize>, HighlightStyle)>,
}

impl InlineBuffer {
    fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }

    fn push(&mut self, text: &str, highlight: Option<HighlightStyle>) {
        if text.is_empty() {
            return;
        }
        let start = self.text.len();
        self.text.push_str(text);
        if let Some(highlight) = highlight {
            self.highlights.push((start..start + text.len(), highlight));
        }
    }

    fn clear(&mut self) {
        self.text.clear();
        self.highlights.clear();
    }
}

/// Merges the currently open inline kinds into one [`HighlightStyle`].
fn combined_highlight(kinds: &[InlineKind], theme: &Theme) -> Option<HighlightStyle> {
    if kinds.is_empty() {
        return None;
    }
    let mut highlight = HighlightStyle::default();
    for kind in kinds {
        match kind {
            InlineKind::Strong => {
                highlight.font_weight = Some(FontWeight::SEMIBOLD);
            }
            InlineKind::Emphasis => {
                highlight.font_style = Some(FontStyle::Italic);
            }
            InlineKind::Strikethrough => {
                highlight.strikethrough = Some(StrikethroughStyle {
                    thickness: px(1.0),
                    color: Some(theme.text_muted),
                });
            }
            InlineKind::Link => {
                highlight.color = Some(theme.accent);
                highlight.underline = Some(UnderlineStyle {
                    color: Some(theme.accent),
                    thickness: px(1.0),
                    wavy: false,
                });
            }
        }
    }
    Some(highlight)
}

struct ListIrState {
    ordered: bool,
    next: u64,
    items: Vec<Rc<ItemIr>>,
}

/// Walks pulldown events and collects [`BlockIr`] values. The state machine is
/// a faithful port of the previous element-building builder; only the leaf
/// pushes changed from "build element" to "record IR".
struct IrBuilder {
    theme: Theme,
    blocks: Vec<Rc<BlockIr>>,
    inline: InlineBuffer,
    inline_kinds: Vec<InlineKind>,
    lists: Vec<ListIrState>,
    item_marker: Option<String>,
    item_indent: usize,
    quote_depth: usize,
    code_block: Option<(Option<String>, String)>,
    heading_level: Option<u8>,
    in_table: bool,
    table: Option<TableCollector>,
}

impl IrBuilder {
    fn new(theme: &Theme) -> Self {
        Self {
            theme: theme.clone(),
            blocks: Vec::new(),
            inline: InlineBuffer::default(),
            inline_kinds: Vec::new(),
            lists: Vec::new(),
            item_marker: None,
            item_indent: 0,
            quote_depth: 0,
            code_block: None,
            heading_level: None,
            in_table: false,
            table: None,
        }
    }

    fn handle_event(&mut self, event: Event<'_>) {
        if self.in_table {
            let Some(collector) = self.table.as_mut() else {
                self.in_table = false;
                return;
            };
            if let Some(data) = collector.handle_event(event) {
                self.in_table = false;
                self.table = None;
                self.blocks.push(Rc::new(BlockIr::Table(data)));
            }
            return;
        }

        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(text) => {
                if let Some((_, code)) = &mut self.code_block {
                    code.push_str(&text);
                } else {
                    let highlight = combined_highlight(&self.inline_kinds, &self.theme);
                    self.inline.push(&text, highlight);
                }
            }
            Event::Code(text) => {
                self.inline.push(
                    &text,
                    Some(HighlightStyle {
                        background_color: Some(self.theme.code_bg),
                        color: Some(self.theme.text),
                        ..Default::default()
                    }),
                );
            }
            Event::InlineMath(text) | Event::DisplayMath(text) => {
                let highlight = combined_highlight(&self.inline_kinds, &self.theme);
                self.inline.push(&text, highlight);
            }
            Event::SoftBreak => {
                let highlight = combined_highlight(&self.inline_kinds, &self.theme);
                self.inline.push(" ", highlight);
            }
            Event::HardBreak => {
                let highlight = combined_highlight(&self.inline_kinds, &self.theme);
                self.inline.push("\n", highlight);
            }
            Event::Rule => {
                self.flush_inline();
                self.blocks.push(Rc::new(BlockIr::Rule));
            }
            Event::Html(text) | Event::InlineHtml(text) => {
                if text.contains("<br") {
                    self.inline.push("\n", None);
                }
            }
            Event::TaskListMarker(checked) => {
                self.inline.push(
                    if checked { "☑ " } else { "☐ " },
                    Some(HighlightStyle {
                        color: Some(self.theme.text_muted),
                        ..Default::default()
                    }),
                );
            }
            Event::FootnoteReference(reference) => {
                self.inline.push(
                    &format!("[{reference}]"),
                    Some(HighlightStyle {
                        color: Some(self.theme.accent),
                        ..Default::default()
                    }),
                );
            }
        }
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                if self.in_list_item() && !self.inline.is_empty() {
                    self.inline.push("\n", None);
                }
            }
            Tag::Heading { level, .. } => {
                self.heading_level = Some(heading_level(level));
            }
            Tag::BlockQuote(_) => {
                self.flush_inline();
                self.quote_depth += 1;
            }
            Tag::CodeBlock(kind) => {
                self.flush_inline();
                let language = match kind {
                    CodeBlockKind::Fenced(language) => {
                        let language = language.trim();
                        (!language.is_empty()).then(|| language.to_string())
                    }
                    CodeBlockKind::Indented => None,
                };
                self.code_block = Some((language, String::new()));
            }
            Tag::List(start) => {
                if self.in_list_item() && !self.inline.is_empty() {
                    self.flush_list_item();
                }
                self.lists.push(ListIrState {
                    ordered: start.is_some(),
                    next: start.unwrap_or(1),
                    items: Vec::new(),
                });
            }
            Tag::Item => {
                let marker = match self.lists.last_mut() {
                    Some(list) if list.ordered => {
                        let marker = format!("{}.", list.next);
                        list.next += 1;
                        marker
                    }
                    _ => "•".to_string(),
                };
                self.item_marker = Some(marker);
                self.item_indent = self.lists.len().saturating_sub(1);
                self.inline.clear();
            }
            Tag::Table(alignments) => {
                self.flush_inline();
                self.in_table = true;
                self.table = Some(TableCollector::new(alignments));
            }
            Tag::Strong => self.inline_kinds.push(InlineKind::Strong),
            Tag::Emphasis => self.inline_kinds.push(InlineKind::Emphasis),
            Tag::Strikethrough => self.inline_kinds.push(InlineKind::Strikethrough),
            Tag::Link { .. } => self.inline_kinds.push(InlineKind::Link),
            Tag::Image {
                dest_url, title, ..
            } => {
                let label = if title.is_empty() {
                    dest_url.to_string()
                } else {
                    title.to_string()
                };
                self.inline.push(
                    &format!("[{label}]"),
                    Some(HighlightStyle {
                        color: Some(self.theme.accent),
                        underline: Some(UnderlineStyle {
                            color: Some(self.theme.accent),
                            thickness: px(1.0),
                            wavy: false,
                        }),
                        ..Default::default()
                    }),
                );
            }
            Tag::HtmlBlock
            | Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition => {}
            _ => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                if !self.in_list_item() {
                    self.flush_inline();
                }
            }
            TagEnd::Heading(_) => {
                let level = self.heading_level.take().unwrap_or(1);
                self.flush_heading(level);
            }
            TagEnd::BlockQuote(_) => {
                self.flush_inline();
                self.quote_depth = self.quote_depth.saturating_sub(1);
            }
            TagEnd::CodeBlock => {
                if let Some((language, code)) = self.code_block.take() {
                    let block = BlockIr::Code {
                        language: language.map(Into::into),
                        code_hash: hash_source(&code),
                        code: code.into(),
                    };
                    self.blocks.push(Rc::new(block));
                }
            }
            TagEnd::List(_) => {
                if let Some(list) = self.lists.pop() {
                    if self.item_marker.is_some() {
                        self.flush_list_item();
                    }
                    if !list.items.is_empty() {
                        let block = BlockIr::List {
                            items: list.items.into(),
                            quote: self.quote_depth > 0,
                        };
                        self.blocks.push(Rc::new(block));
                    }
                }
            }
            TagEnd::Item => {
                self.flush_list_item();
            }
            TagEnd::Table => {}
            TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough | TagEnd::Link => {
                self.inline_kinds.pop();
            }
            _ => {}
        }
    }

    fn in_list_item(&self) -> bool {
        self.item_marker.is_some()
    }

    fn flush_list_item(&mut self) {
        let Some(marker) = self.item_marker.take() else {
            return;
        };
        if self.inline.is_empty() {
            return;
        }
        let item = Rc::new(ItemIr {
            marker: marker.into(),
            indent: self.item_indent,
            text: self.inline.text.clone().into(),
            highlights: self.inline.highlights.clone().into(),
        });
        self.inline.clear();
        if let Some(list) = self.lists.last_mut() {
            list.items.push(item);
        } else {
            self.blocks.push(Rc::new(BlockIr::Item(item)));
        }
    }

    fn flush_inline(&mut self) {
        if self.inline.is_empty() {
            self.inline.clear();
            return;
        }
        if self.item_marker.is_some() {
            return;
        }
        let text = TextIr {
            text: self.inline.text.clone().into(),
            highlights: self.inline.highlights.clone().into(),
            size: text_size::BASE,
            weight: FontWeight::NORMAL,
            heading: false,
            quote: self.quote_depth > 0,
        };
        self.inline.clear();
        self.blocks.push(Rc::new(BlockIr::Text(text)));
    }

    fn flush_heading(&mut self, level: u8) {
        if self.inline.is_empty() {
            self.inline.clear();
            return;
        }
        let (size, weight) = match level {
            1 => (22.0, FontWeight::BOLD),
            2 => (17.0, FontWeight::SEMIBOLD),
            3 => (15.0, FontWeight::SEMIBOLD),
            _ => (14.5, FontWeight::SEMIBOLD),
        };
        let text = TextIr {
            text: self.inline.text.clone().into(),
            highlights: self.inline.highlights.clone().into(),
            size,
            weight,
            heading: true,
            quote: false,
        };
        self.inline.clear();
        self.blocks.push(Rc::new(BlockIr::Text(text)));
    }

    fn finish(mut self) -> Blocks {
        self.flush_inline();
        self.blocks.into()
    }
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn parse_options() -> Options {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_TASKLISTS);
    options
}

fn build_blocks(source: &str, theme: &Theme) -> Blocks {
    let mut builder = IrBuilder::new(theme);
    for event in Parser::new_ext(source, parse_options()) {
        builder.handle_event(event);
    }
    builder.finish()
}

// --- caching ---------------------------------------------------------------

/// How many complete documents stay memoized (transcript-length answers).
const MAX_CACHED_DOCS: usize = 64;
/// How many markdown chunks stay memoized. Chunk-split transcript items touch
/// every stable chunk on every frame, so the LRU stamps keep them resident
/// while the streaming caret chunk churns.
const MAX_CACHED_CHUNKS: usize = 2048;

/// Cache identity: the theme fields that leak into cached highlights, plus
/// the source content hash and length.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct DocKey {
    theme_key: u64,
    hash: u64,
    len: usize,
}

#[derive(Default)]
struct MarkdownCache {
    docs: HashMap<DocKey, (u64, Blocks)>,
    chunks: HashMap<DocKey, (u64, Blocks)>,
    stamp: u64,
    hits: u64,
}

thread_local! {
    /// Rendering only happens on the UI thread, so a thread-local memo keeps
    /// the API signature-free (`&Theme, &str`) instead of threading `cx`
    /// through every render call. It stores no element state, only IR.
    static CACHE: RefCell<MarkdownCache> = RefCell::new(MarkdownCache::default());
}

fn cache_get(
    map: &mut HashMap<DocKey, (u64, Blocks)>,
    key: &DocKey,
    stamp: &mut u64,
    hits: &mut u64,
) -> Option<Blocks> {
    let entry = map.get_mut(key)?;
    *stamp += 1;
    entry.0 = *stamp;
    *hits += 1;
    Some(entry.1.clone())
}

fn cache_put(
    map: &mut HashMap<DocKey, (u64, Blocks)>,
    key: DocKey,
    blocks: Blocks,
    stamp: &mut u64,
    cap: usize,
) {
    *stamp += 1;
    map.insert(key, (*stamp, blocks));
    if map.len() > cap {
        let victim = map
            .iter()
            .min_by_key(|(_, (entry_stamp, _))| *entry_stamp)
            .map(|(key, _)| *key);
        if let Some(key) = victim {
            map.remove(&key);
        }
    }
}

/// Hash of the theme fields cached IR embeds (highlight colors), so a theme
/// switch can never serve stale styling.
fn theme_ir_key(theme: &Theme) -> u64 {
    let mut hasher = DefaultHasher::new();
    for color in [theme.text, theme.text_muted, theme.accent, theme.code_bg] {
        for channel in [color.h, color.s, color.l, color.a] {
            channel.to_bits().hash(&mut hasher);
        }
    }
    hasher.finish()
}

/// Content hash used for cache keys. Exported so callers that hash the same
/// text at rebuild time can reuse the value instead of re-hashing per frame.
pub fn markdown_source_key(source: &str) -> u64 {
    hash_source(source)
}

/// Number of cache hits since process start; the `EFFIBUDDY_PERF`
/// instrumentation reports the delta to show memoization effectiveness.
pub fn markdown_cache_hits() -> u64 {
    CACHE.with(|cache| cache.borrow().hits)
}

fn hash_source(source: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    source.hash(&mut hasher);
    hasher.finish()
}

// --- public rendering API --------------------------------------------------

pub fn render_markdown(theme: &Theme, source: &str) -> AnyElement {
    render_markdown_keyed(theme, source, hash_source(source))
}

/// [`render_markdown`] with a caller-supplied content hash, for render paths
/// that already know the hash (e.g. computed at rebuild time) and must not
/// re-hash a potentially megabyte-sized document every frame.
pub fn render_markdown_keyed(theme: &Theme, source: &str, key: u64) -> AnyElement {
    let key = DocKey {
        theme_key: theme_ir_key(theme),
        hash: key,
        len: source.len(),
    };
    let blocks = CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let MarkdownCache {
            docs, stamp, hits, ..
        } = &mut *cache;
        match cache_get(docs, &key, stamp, hits) {
            Some(blocks) => blocks,
            None => {
                let blocks = build_blocks(source, theme);
                cache_put(docs, key, blocks.clone(), stamp, MAX_CACHED_DOCS);
                blocks
            }
        }
    });
    render_parsed(theme, &blocks)
}

/// Like [`render_markdown`], but keeps a block cursor at the end of the text.
/// The cursor is appended to the Markdown source so it flows inline with the
/// last sentence (a new paragraph, list item or code block when appropriate).
///
/// The source is split into fence-safe chunks ([`chunk_ranges`]); each chunk
/// is memoized by content hash, so a frame during streaming re-parses only the
/// chunk that gained characters. Chunk-wise parsing renders identically to
/// whole-document parsing because cuts that would tear a list or an indented
/// continuation in half are merged back together.
pub fn render_markdown_streaming(theme: &Theme, source: &str) -> AnyElement {
    let mut streaming = String::with_capacity(source.len() + 2);
    streaming.push_str(source);
    if !streaming.is_empty() {
        streaming.push(' ');
    }
    streaming.push('▍');

    let mut blocks: Vec<Rc<BlockIr>> = Vec::new();
    for range in chunk_ranges(&streaming) {
        let chunk = &streaming[range];
        blocks.extend(
            cached_chunk_blocks(theme, chunk, hash_source(chunk))
                .iter()
                .cloned(),
        );
    }
    render_parsed(theme, &Rc::from(blocks))
}

/// Render one pre-split markdown block, memoized in the chunk cache under the
/// caller-supplied content hash. Virtual chat items that render a stable
/// slice of a longer document call this every frame; the hash makes the
/// per-frame cost an O(len) scan instead of a re-parse.
pub fn render_markdown_chunk(theme: &Theme, source: &str, hash: u64) -> AnyElement {
    render_parsed(theme, &Rc::from(cached_chunk_blocks(theme, source, hash)))
}

/// Chunk-cache lookup keyed by theme + content hash + length.
fn cached_chunk_blocks(theme: &Theme, source: &str, hash: u64) -> Blocks {
    let key = DocKey {
        theme_key: theme_ir_key(theme),
        hash,
        len: source.len(),
    };
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let MarkdownCache {
            chunks,
            stamp,
            hits,
            ..
        } = &mut *cache;
        match cache_get(chunks, &key, stamp, hits) {
            Some(blocks) => blocks,
            None => {
                let blocks = build_blocks(source, theme);
                cache_put(chunks, key, blocks.clone(), stamp, MAX_CACHED_CHUNKS);
                blocks
            }
        }
    })
}

pub fn markdown_inline(theme: &Theme, source: &str, size: f32, color: gpui::Hsla) -> AnyElement {
    let inline = collect_inline(source, theme);
    div()
        .w_full()
        .text_size(px(size))
        .line_height(px(size * 1.58))
        .text_color(color)
        .child(styled_inline(inline.text.into(), &inline.highlights))
        .into_any_element()
}

/// Inline markdown (emphasis, inline code, links, task markers) flattened into
/// a single buffer. Block constructs contribute only their inline text.
fn collect_inline(source: &str, theme: &Theme) -> InlineBuffer {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    let mut inline = InlineBuffer::default();
    let mut kinds: Vec<InlineKind> = Vec::new();
    let mut skip_depth = 0usize;
    for event in Parser::new_ext(source, options) {
        match event {
            Event::Start(Tag::CodeBlock(_)) | Event::Start(Tag::Table(_)) => skip_depth += 1,
            Event::End(TagEnd::CodeBlock) | Event::End(TagEnd::Table) => {
                skip_depth = skip_depth.saturating_sub(1);
            }
            _ if skip_depth > 0 => {}
            Event::Text(text) => {
                let highlight = combined_highlight(&kinds, theme);
                inline.push(&text, highlight);
            }
            Event::Code(text) => {
                inline.push(
                    &text,
                    Some(HighlightStyle {
                        background_color: Some(theme.code_bg),
                        color: Some(theme.text),
                        ..Default::default()
                    }),
                );
            }
            Event::InlineMath(text) | Event::DisplayMath(text) => {
                let highlight = combined_highlight(&kinds, theme);
                inline.push(&text, highlight);
            }
            Event::SoftBreak => {
                let highlight = combined_highlight(&kinds, theme);
                inline.push(" ", highlight);
            }
            Event::HardBreak => {
                let highlight = combined_highlight(&kinds, theme);
                inline.push("\n", highlight);
            }
            Event::Start(tag) => match tag {
                Tag::Strong => kinds.push(InlineKind::Strong),
                Tag::Emphasis => kinds.push(InlineKind::Emphasis),
                Tag::Strikethrough => kinds.push(InlineKind::Strikethrough),
                Tag::Link { .. } => kinds.push(InlineKind::Link),
                Tag::Image {
                    dest_url, title, ..
                } => {
                    let label = if title.is_empty() {
                        dest_url.to_string()
                    } else {
                        title.to_string()
                    };
                    inline.push(
                        &format!("[{label}]"),
                        Some(HighlightStyle {
                            color: Some(theme.accent),
                            underline: Some(UnderlineStyle {
                                color: Some(theme.accent),
                                thickness: px(1.0),
                                wavy: false,
                            }),
                            ..Default::default()
                        }),
                    );
                }
                _ => {}
            },
            Event::End(
                TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough | TagEnd::Link,
            ) => {
                kinds.pop();
            }
            Event::TaskListMarker(checked) => {
                inline.push(
                    if checked { "☑ " } else { "☐ " },
                    Some(HighlightStyle {
                        color: Some(theme.text_muted),
                        ..Default::default()
                    }),
                );
            }
            Event::FootnoteReference(reference) => {
                inline.push(
                    &format!("[{reference}]"),
                    Some(HighlightStyle {
                        color: Some(theme.accent),
                        ..Default::default()
                    }),
                );
            }
            Event::Html(text) | Event::InlineHtml(text) if text.contains("<br") => {
                inline.push("\n", None);
            }
            _ => {}
        }
    }
    inline
}

// --- chunking (streaming) --------------------------------------------------

/// Byte ranges of fence-safe chunks. Blank lines outside fenced code blocks
/// separate chunks; cuts that would tear a list or an indented continuation
/// in half are merged back so chunk-wise parsing renders identically to
/// whole-document parsing.
/// Fence-safe split of a markdown document into self-contained chunks: cuts
/// land on blank lines outside code fences, and list continuations are merged
/// back so each chunk parses to the same blocks it would inside the whole
/// document. Prefix-stable under appends: growing the source only ever
/// extends (or adds) trailing chunks, which lets virtual item lists reuse
/// stable chunk entries across frames.
pub fn chunk_ranges(source: &str) -> Vec<Range<usize>> {
    let mut cuts: Vec<usize> = Vec::new();
    let mut fence: Option<(u8, usize)> = None;
    let mut line_start = 0usize;
    while line_start < source.len() {
        let line_end = source[line_start..]
            .find('\n')
            .map(|index| line_start + index + 1)
            .unwrap_or(source.len());
        let content = source[line_start..line_end].trim_end_matches(['\r', '\n']);
        let indent = content.len() - content.trim_start_matches(' ').len();
        let stripped = content.trim_start_matches(' ');
        match fence {
            None => {
                if let Some((marker, length)) = fence_open(stripped) {
                    fence = Some((marker, length));
                } else if content.trim().is_empty() && line_end < source.len() {
                    cuts.push(line_end);
                }
            }
            Some((marker, length)) => {
                let closing = stripped.trim();
                if indent <= 3
                    && closing.len() >= length
                    && closing.bytes().all(|byte| byte == marker)
                {
                    fence = None;
                }
            }
        }
        line_start = line_end;
    }

    let mut ranges: Vec<Range<usize>> = Vec::new();
    let mut cursor = 0usize;
    for cut in cuts {
        if cut > cursor {
            ranges.push(cursor..cut);
            cursor = cut;
        }
    }
    if cursor < source.len() {
        ranges.push(cursor..source.len());
    }

    let mut merged: Vec<Range<usize>> = Vec::new();
    for range in ranges {
        let join = merged
            .last()
            .is_some_and(|previous| should_merge(source, previous, &range));
        if join {
            merged.last_mut().expect("checked above").end = range.end;
        } else {
            merged.push(range);
        }
    }
    merged
}

/// Opening fence: three or more backticks/tilde chars (≤3 spaces of indent
/// are handled by the caller). The rest of the line is an info string.
fn fence_open(stripped: &str) -> Option<(u8, usize)> {
    let bytes = stripped.as_bytes();
    if bytes.len() < 3 {
        return None;
    }
    let marker = bytes[0];
    if marker != b'`' && marker != b'~' {
        return None;
    }
    let count = bytes.iter().take_while(|&&byte| byte == marker).count();
    if count < 3 {
        return None;
    }
    // A backtick info string may not contain backticks, in which case this
    // line is not a fence at all.
    if marker == b'`' && stripped[count..].contains('`') {
        return None;
    }
    Some((marker, count))
}

fn should_merge(source: &str, previous: &Range<usize>, next: &Range<usize>) -> bool {
    let next_text = &source[next.clone()];
    let Some(next_first) = next_text.lines().next() else {
        return false;
    };
    let indent = next_first.len() - next_first.trim_start_matches(' ').len();
    if indent >= 2 {
        // Indented or nested continuation of whatever the previous chunk was
        // building (list item text, nested list, indented code).
        return true;
    }
    if !is_list_marker_line(next_first) {
        return false;
    }
    let previous_text = &source[previous.clone()];
    previous_text
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .is_some_and(is_list_marker_line)
}

fn is_list_marker_line(line: &str) -> bool {
    let bytes = line.trim_start_matches(' ').as_bytes();
    match bytes.first() {
        Some(b'-' | b'*' | b'+') => bytes.len() > 1 && (bytes[1] == b' ' || bytes[1] == b'\t'),
        Some(b'0'..=b'9') => {
            let digits = bytes
                .iter()
                .take_while(|&&byte| byte.is_ascii_digit())
                .count();
            digits <= 9
                && matches!(bytes.get(digits), Some(b'.' | b')'))
                && matches!(bytes.get(digits + 1), None | Some(b' ' | b'\t'))
        }
        _ => false,
    }
}

// --- IR → elements ----------------------------------------------------------

fn render_parsed(theme: &Theme, blocks: &[Rc<BlockIr>]) -> AnyElement {
    let children = blocks.iter().map(|block| render_block(theme, block));
    div()
        .flex()
        .flex_col()
        .w_full()
        .gap_3()
        .children(children)
        .into_any_element()
}

fn render_block(theme: &Theme, block: &Rc<BlockIr>) -> AnyElement {
    match block.as_ref() {
        BlockIr::Text(text) => render_text_block(theme, text),
        BlockIr::Item(item) => render_item_row(theme, item),
        BlockIr::List { items, quote } => {
            let mut container = div()
                .w_full()
                .flex()
                .flex_col()
                .gap_1()
                .children(items.iter().map(|item| render_item_row(theme, item)));
            if *quote {
                container = container
                    .border_l_2()
                    .border_color(theme.border_strong)
                    .pl_3();
            }
            container.into_any_element()
        }
        BlockIr::Code {
            language,
            code,
            code_hash,
        } => render_code_block(
            theme,
            language.as_ref().map(|name| name.as_str()),
            code,
            *code_hash,
        ),
        BlockIr::Table(data) => render_table(theme, data),
        BlockIr::Rule => div()
            .w_full()
            .h(px(1.0))
            .bg(theme.border)
            .into_any_element(),
    }
}

fn styled_inline(text: SharedString, highlights: &[(Range<usize>, HighlightStyle)]) -> StyledText {
    if highlights.is_empty() {
        StyledText::new(text)
    } else {
        StyledText::new(text).with_highlights(highlights.iter().cloned())
    }
}

fn render_text_block(theme: &Theme, ir: &TextIr) -> AnyElement {
    let element = div()
        .w_full()
        .text_size(px(ir.size))
        .line_height(px(ir.size * 1.58))
        .font_weight(ir.weight)
        .text_color(theme.text)
        .child(styled_inline(ir.text.clone(), &ir.highlights));
    let element = if ir.heading {
        div().w_full().pt(px(2.0)).child(element).into_any_element()
    } else {
        element.into_any_element()
    };
    if ir.quote {
        div()
            .w_full()
            .border_l_2()
            .border_color(theme.border_strong)
            .pl_3()
            .text_color(theme.text_muted)
            .child(element)
            .into_any_element()
    } else {
        element
    }
}

fn render_item_row(theme: &Theme, ir: &ItemIr) -> AnyElement {
    div()
        .w_full()
        .flex()
        .items_start()
        .gap_2()
        .pl(px(ir.indent as f32 * 16.0))
        .child(
            div()
                .flex_none()
                .w(px(18.0))
                .pt(px(1.0))
                .text_size(px(text_size::BASE))
                .text_color(theme.text_faint)
                .child(ir.marker.clone()),
        )
        .child(
            div()
                .w_full()
                .text_size(px(text_size::BASE))
                .line_height(px(text_size::BASE * 1.58))
                .font_weight(FontWeight::NORMAL)
                .text_color(theme.text)
                .child(styled_inline(ir.text.clone(), &ir.highlights)),
        )
        .into_any_element()
}

fn render_code_block(
    theme: &Theme,
    language: Option<&str>,
    code: &SharedString,
    code_hash: u64,
) -> AnyElement {
    // SharedString clone: a refcount bump, not a copy, even for large code.
    let copy_code = code.clone();
    let label = language.unwrap_or("code");

    div()
        .w_full()
        .rounded(px(radius::MD))
        .bg(theme.code_bg)
        .overflow_hidden()
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .px_3()
                .py(px(6.0))
                .bg(theme.surface)
                .child(
                    div()
                        .text_size(px(text_size::XS))
                        .font_family(theme.mono_font())
                        .text_color(theme.text_faint)
                        .child(label.to_string()),
                )
                .child(
                    div()
                        .id(("copy-code", code_hash))
                        .flex()
                        .items_center()
                        .gap_1()
                        .px(px(6.0))
                        .py(px(3.0))
                        .rounded(px(radius::SM))
                        .cursor_pointer()
                        .text_color(theme.text_faint)
                        .hover(|style| style.bg(theme.hover).text_color(theme.text))
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy_code.to_string()));
                        })
                        .child(
                            svg()
                                .path(icons::COPY)
                                .size(px(12.0))
                                .text_color(theme.text_faint),
                        )
                        .child(div().text_size(px(text_size::XS)).child("复制".to_string())),
                ),
        )
        .child(
            div()
                .id(("code-scroll", code_hash))
                .w_full()
                .overflow_x_scroll()
                .child(
                    div()
                        .p_3()
                        .font_family(theme.mono_font())
                        .text_size(px(12.0))
                        .line_height(px(18.0))
                        .text_color(theme.text)
                        .whitespace_nowrap()
                        .child(code.clone()),
                ),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> Theme {
        Theme::default()
    }

    fn chunk_texts(source: &str) -> Vec<String> {
        chunk_ranges(source)
            .into_iter()
            .map(|range| source[range].to_string())
            .collect()
    }

    #[test]
    fn blank_lines_outside_fences_split_chunks() {
        let chunks = chunk_texts("one\n\ntwo\n\nthree");
        assert_eq!(chunks, vec!["one\n\n", "two\n\n", "three"]);
    }

    #[test]
    fn blank_lines_inside_fences_stay_in_one_chunk() {
        let source = "```python\na = 1\n\nb = 2\n```\n\nafter";
        let chunks = chunk_texts(source);
        assert_eq!(chunks, vec!["```python\na = 1\n\nb = 2\n```\n\n", "after"]);
    }

    #[test]
    fn loose_list_stays_one_chunk_so_numbering_survives() {
        let chunks = chunk_texts("1. first\n\n2. second\n\n3. third");
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn list_followed_by_paragraph_splits() {
        let chunks = chunk_texts("1. item\n\nplain text");
        assert_eq!(chunks, vec!["1. item\n\n", "plain text"]);
    }

    #[test]
    fn indented_continuation_merges() {
        let chunks = chunk_texts("para\n\n  continued");
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn streaming_ir_matches_whole_document_parse() {
        let source = "# Title\n\nHello **world** and `code`.\n\n```rust\nfn main() {}\n```\n\n> quoted text\n\n- tight\n- items\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n---\n";
        let theme = theme();
        let direct = build_blocks(source, &theme);
        let chunked: Vec<Rc<BlockIr>> = chunk_ranges(source)
            .into_iter()
            .flat_map(|range| {
                let blocks = build_blocks(&source[range], &theme);
                blocks.iter().cloned().collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(direct.as_ref(), chunked.as_slice());
    }

    #[test]
    fn document_cache_hits_on_second_render() {
        let theme = theme();
        let source = "cached **document**\n\nsecond paragraph";
        let key = DocKey {
            theme_key: theme_ir_key(&theme),
            hash: hash_source(source),
            len: source.len(),
        };
        CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            // Fresh thread-local: ensure the document is absent first.
            cache.docs.remove(&key);
            let before = cache.hits;
            let MarkdownCache {
                docs, stamp, hits, ..
            } = &mut *cache;
            assert!(cache_get(docs, &key, stamp, hits).is_none());
            let blocks = build_blocks(source, &theme);
            cache_put(docs, key, blocks, stamp, MAX_CACHED_DOCS);
            let hit = cache_get(docs, &key, stamp, hits);
            assert!(hit.is_some());
            assert_eq!(*hits, before + 1);
        });
    }

    #[test]
    fn streaming_chunks_hit_cache_on_repeat() {
        let theme = theme();
        let stable = "intro paragraph\n\n- item one\n- item two\n\n";
        let with_tail = format!("{stable}tail being typed");
        render_markdown_streaming(&theme, &with_tail);
        let hits_after_first = CACHE.with(|cache| cache.borrow().hits);
        render_markdown_streaming(&theme, &with_tail);
        let hits_after_second = CACHE.with(|cache| cache.borrow().hits);
        assert!(
            hits_after_second > hits_after_first,
            "every chunk should hit the cache on the second identical call"
        );
    }

    #[test]
    fn theme_change_invalidates_cache_key() {
        let mut theme = theme();
        let key_a = theme_ir_key(&theme);
        // Any palette change must alter the key; nudge one embedded color.
        theme.accent.h += 0.01;
        let key_b = theme_ir_key(&theme);
        assert_ne!(key_a, key_b);
    }

    #[test]
    fn ir_preserves_inline_highlights_and_code() {
        let theme = theme();
        let blocks = build_blocks("say **bold** and `mono`", &theme);
        assert_eq!(blocks.len(), 1);
        let BlockIr::Text(text) = blocks[0].as_ref() else {
            panic!("expected text block");
        };
        assert_eq!(text.text, "say bold and mono");
        assert_eq!(text.highlights.len(), 2);
        let code = build_blocks("```js\nlet x = 1;\n```", &theme);
        assert!(matches!(code[0].as_ref(), BlockIr::Code { .. }));
    }

    /// Manual benchmark: simulates a typewriter streaming a ~30 KB document
    /// over 60 frames and prints the per-frame render cost, first frame vs
    /// steady state. Run with:
    /// `cargo test -p effibuddy-ui bench_streaming --release -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_streaming_frame_cost() {
        let theme = theme();
        let paragraph = "这是一段**流式输出**的正文，包含 `inline code`、[链接](https://example.com) 和 *强调*。\n\n";
        let mut document = String::new();
        for index in 1..=60 {
            document.push_str(&format!("### 第 {index} 节\n\n{paragraph}"));
        }
        // Reveal 60 times in ~512-char steps: a full pass at ~60 fps pacing.
        let mut shown = 0usize;
        let mut frames: Vec<f64> = Vec::new();
        while shown < document.len() {
            shown = (shown + 512).min(document.len());
            // Land on a UTF-8 boundary.
            while !document.is_char_boundary(shown) {
                shown += 1;
            }
            let text = &document[..shown];
            let started = std::time::Instant::now();
            let element = render_markdown_streaming(&theme, text);
            frames.push(started.elapsed().as_secs_f64() * 1000.0);
            drop(element);
        }
        let first = frames.first().copied().unwrap_or_default();
        let tail = &frames[frames.len().saturating_sub(10)..];
        let tail_average = tail.iter().sum::<f64>() / tail.len().max(1) as f64;

        // Steady-state comparison at the FINAL document size: 20 frames of
        // the cached streaming path vs the pre-cache behavior (full reparse
        // + IR rebuild + element build every frame).
        let full = document.as_str();
        let warm = render_markdown_streaming(&theme, full);
        drop(warm);
        let steady_cached: Vec<f64> = (0..20)
            .map(|_| {
                let started = std::time::Instant::now();
                let element = render_markdown_streaming(&theme, full);
                let elapsed = started.elapsed().as_secs_f64() * 1000.0;
                drop(element);
                elapsed
            })
            .collect();
        let steady_uncached: Vec<f64> = (0..20)
            .map(|_| {
                let started = std::time::Instant::now();
                let blocks = build_blocks(full, &theme);
                let element = render_parsed(&theme, &blocks);
                let elapsed = started.elapsed().as_secs_f64() * 1000.0;
                drop(element);
                elapsed
            })
            .collect();
        let average = |samples: &[f64]| samples.iter().sum::<f64>() / samples.len() as f64;
        let cached_steady = average(&steady_cached);
        let uncached_steady = average(&steady_uncached);
        println!(
            "streaming {frames_len} frames over {total} KB: first frame {first:.2}ms, last-10 avg {tail_average:.2}ms; steady state at full size: cached {cached_steady:.2}ms vs pre-cache {uncached_steady:.2}ms ({speedup:.1}x faster)",
            frames_len = frames.len(),
            total = document.len() / 1024,
            speedup = uncached_steady / cached_steady.max(0.001),
        );
    }
}
