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
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui::{
    AnyElement, ClipboardItem, ElementId, FontStyle, FontWeight, HighlightStyle, IntoElement,
    MouseButton, ObjectFit, SharedString, StrikethroughStyle, StyledText, UnderlineStyle, Window,
    div, img, prelude::*, px, svg,
};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::assets::icons;
use crate::table::{TableCollector, TableData, render_table};
use crate::theme::{Theme, radius, text as text_size};

/// What a clickable run of markdown points at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MarkdownTarget {
    /// An external URL (web, mail, or another app scheme).
    Url(SharedString),
    /// A file-system path exactly as it appeared in the source. Relative paths
    /// are resolved against [`MarkdownInteraction::base_dir`] on click so the
    /// cached IR never bakes in a workspace.
    File(SharedString),
}

/// Click hooks for rendered markdown. The app owns the menu that opens; the
/// renderer only reports *what* was clicked and *where*.
#[derive(Clone)]
pub struct MarkdownInteraction {
    base_dir: PathBuf,
    activate: Rc<
        dyn Fn(
            MarkdownTarget,
            MouseButton,
            gpui::Point<gpui::Pixels>,
            &Path,
            &mut Window,
            &mut gpui::App,
        ),
    >,
}

impl MarkdownInteraction {
    pub fn new(
        base_dir: impl Into<PathBuf>,
        activate: impl Fn(
            MarkdownTarget,
            MouseButton,
            gpui::Point<gpui::Pixels>,
            &Path,
            &mut Window,
            &mut gpui::App,
        ) + 'static,
    ) -> Self {
        Self {
            base_dir: base_dir.into(),
            activate: Rc::new(activate),
        }
    }

    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    fn activate(
        &self,
        target: MarkdownTarget,
        button: MouseButton,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut gpui::App,
    ) {
        (self.activate)(target, button, position, &self.base_dir, window, cx);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum LinkTargetIr {
    Url(SharedString),
    File(SharedString),
}

/// A clickable byte range inside one inline buffer. Ranges are relative to the
/// text of the [`TextIr`]/[`ItemIr`] that owns them.
#[derive(Clone, Debug, PartialEq, Eq)]
struct LinkIr {
    range: Range<usize>,
    target: LinkTargetIr,
}

impl LinkIr {
    fn markdown_target(&self) -> MarkdownTarget {
        match &self.target {
            LinkTargetIr::Url(url) => MarkdownTarget::Url(url.clone()),
            LinkTargetIr::File(path) => MarkdownTarget::File(path.clone()),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ImageIr {
    alt: SharedString,
    src: SharedString,
}

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
    links: Rc<[LinkIr]>,
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
    links: Rc<[LinkIr]>,
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
    /// A `![alt](src)` image. Rendering resolves `src` against the interaction
    /// base directory; remote sources are loaded by the GPUI asset loader.
    Image(ImageIr),
    Table(TableData),
    Rule,
}

type Blocks = Rc<[Rc<BlockIr>]>;

#[derive(Clone, Default)]
struct InlineBuffer {
    text: String,
    highlights: Vec<(Range<usize>, HighlightStyle)>,
    links: Vec<LinkIr>,
    /// Ranges whose text must not be re-scanned for bare paths: explicit
    /// Markdown links and inline code already decided what is clickable.
    protected: Vec<Range<usize>>,
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
        self.links.clear();
        self.protected.clear();
    }

    fn add_link(&mut self, range: Range<usize>, target: LinkTargetIr, protect: bool) {
        if range.start >= range.end || range.end > self.text.len() {
            return;
        }
        if self.links.iter().any(|link| overlaps(&link.range, &range)) {
            return;
        }
        if protect {
            self.protected.push(range.clone());
        }
        self.links.push(LinkIr { range, target });
    }

    /// Resolve clickable targets for the finished buffer: inline code paths
    /// were added as they were emitted, explicit links on their end tag, and
    /// this pass adds bare paths/URLs in plain prose.
    fn finish_targets(&mut self) {
        if self.text.is_empty() {
            return;
        }
        for (range, target) in detect_plain_targets(&self.text) {
            if self
                .protected
                .iter()
                .any(|protected| overlaps(protected, &range))
                || self.links.iter().any(|link| overlaps(&link.range, &range))
            {
                continue;
            }
            self.links.push(LinkIr { range, target });
        }
        self.links.sort_by_key(|link| link.range.start);
    }
}

fn overlaps(left: &Range<usize>, right: &Range<usize>) -> bool {
    left.start < right.end && right.start < left.end
}

/// Extensions that make a bare token look like a file in prose. The list is
/// deliberately code/document/image-heavy: it keeps `example.com`, versions
/// (`v1.2.3`) and abbreviations (`e.g.`) from turning into links.
const KNOWN_EXTENSIONS: &[&str] = &[
    "rs",
    "md",
    "markdown",
    "mdx",
    "toml",
    "json",
    "jsonc",
    "json5",
    "yaml",
    "yml",
    "txt",
    "text",
    "log",
    "csv",
    "tsv",
    "sh",
    "bash",
    "zsh",
    "fish",
    "ps1",
    "bat",
    "cmd",
    "py",
    "pyi",
    "pyx",
    "js",
    "mjs",
    "cjs",
    "jsx",
    "ts",
    "mts",
    "cts",
    "tsx",
    "vue",
    "svelte",
    "astro",
    "css",
    "scss",
    "sass",
    "less",
    "styl",
    "html",
    "htm",
    "xhtml",
    "xml",
    "xsl",
    "xslt",
    "svg",
    "png",
    "jpg",
    "jpeg",
    "gif",
    "webp",
    "bmp",
    "ico",
    "avif",
    "tif",
    "tiff",
    "heic",
    "psd",
    "pdf",
    "doc",
    "docx",
    "xls",
    "xlsx",
    "ppt",
    "pptx",
    "odt",
    "ods",
    "odp",
    "rtf",
    "zip",
    "tar",
    "gz",
    "tgz",
    "bz2",
    "xz",
    "zst",
    "7z",
    "rar",
    "jar",
    "war",
    "lock",
    "cfg",
    "ini",
    "conf",
    "config",
    "env",
    "properties",
    "plist",
    "desktop",
    "service",
    "c",
    "h",
    "cc",
    "cpp",
    "cxx",
    "hpp",
    "hh",
    "hxx",
    "m",
    "mm",
    "swift",
    "kt",
    "kts",
    "java",
    "class",
    "go",
    "rb",
    "php",
    "pl",
    "pm",
    "lua",
    "r",
    "rmd",
    "sql",
    "db",
    "sqlite",
    "sqlite3",
    "proto",
    "graphql",
    "gql",
    "dart",
    "ex",
    "exs",
    "erl",
    "hrl",
    "hs",
    "lhs",
    "clj",
    "cljs",
    "cljc",
    "scala",
    "sc",
    "cs",
    "fs",
    "fsx",
    "vb",
    "asm",
    "s",
    "wat",
    "wasm",
    "ipynb",
    "tex",
    "bib",
    "org",
    "adoc",
    "rst",
    "diff",
    "patch",
    "puml",
    "plantuml",
    "drawio",
];

/// Extension-less files people reference by name.
const KNOWN_FILE_NAMES: &[&str] = &[
    "Dockerfile",
    "Containerfile",
    "Makefile",
    "GNUmakefile",
    "CMakeLists.txt",
    "LICENSE",
    "LICENCE",
    "README",
    "CHANGELOG",
    "CHANGES",
    "NOTICE",
    "AUTHORS",
    "Gemfile",
    "Rakefile",
    "Justfile",
    "Procfile",
    ".gitignore",
    ".dockerignore",
    ".npmignore",
    ".editorconfig",
    ".gitattributes",
    ".npmrc",
    ".prettierrc",
    ".eslintrc",
    ".babelrc",
    ".bashrc",
    ".zshrc",
    ".profile",
    ".env",
    ".python-version",
    ".tool-versions",
    ".nvmrc",
];

fn is_token_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric()
        || matches!(
            ch,
            '/' | '\\' | '.' | '_' | '-' | '~' | '@' | '+' | '=' | '%' | '#' | '?' | '&' | ':'
        )
}

fn is_path_char(ch: char) -> bool {
    // Inline code spans are explicitly delimited, so non-ASCII file names
    // (`docs/设计.md`) are safe here. Anything that would be prose punctuation
    // or shell syntax still disqualifies the span.
    !ch.is_whitespace()
        && !ch.is_control()
        && !matches!(
            ch,
            '"' | '\''
                | '<'
                | '>'
                | '|'
                | '?'
                | '*'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | ','
                | ';'
                | '`'
        )
}

/// Bare paths and URLs in plain prose. Conservative on purpose: plain text has
/// to carry an explicit relative prefix or a known extension, while inline
/// code is allowed to hold extension-less paths such as `crates/agent-core`.
fn detect_plain_targets(text: &str) -> Vec<(Range<usize>, LinkTargetIr)> {
    let mut out = Vec::new();
    let mut start = None;
    for (index, ch) in text.char_indices() {
        if is_token_char(ch) {
            if start.is_none() {
                start = Some(index);
            }
        } else if let Some(token_start) = start.take() {
            push_plain_candidate(text, token_start, index, &mut out);
        }
    }
    if let Some(token_start) = start {
        push_plain_candidate(text, token_start, text.len(), &mut out);
    }
    out
}

fn push_plain_candidate(
    text: &str,
    start: usize,
    end: usize,
    out: &mut Vec<(Range<usize>, LinkTargetIr)>,
) {
    let (start, end) = trim_token_bounds(text, start, end);
    if start >= end {
        return;
    }
    let token = &text[start..end];
    let range = start..end;
    if is_http_url(token) {
        out.push((range, LinkTargetIr::Url(token.to_string().into())));
        return;
    }
    if looks_like_plain_path(token) {
        out.push((range, LinkTargetIr::File(token.to_string().into())));
    }
}

fn trim_token_bounds(text: &str, mut start: usize, mut end: usize) -> (usize, usize) {
    while end > start {
        let ch = text[..end].chars().next_back().unwrap_or_default();
        if matches!(ch, '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"') {
            // A trailing slash or a path prefix alone is not a usable token
            // once its tail is punctuation.
            end -= ch.len_utf8();
        } else {
            break;
        }
    }
    while start < end {
        let ch = text[start..].chars().next().unwrap_or_default();
        if matches!(ch, '\'' | '"' | '(' | '[' | '{') {
            start += ch.len_utf8();
        } else {
            break;
        }
    }
    (start, end)
}

fn is_http_url(token: &str) -> bool {
    token.starts_with("http://") || token.starts_with("https://")
}

fn strip_line_suffix(token: &str) -> &str {
    if let Some(index) = token.rfind(':') {
        let suffix = &token[index + 1..];
        if !suffix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit()) {
            let head = &token[..index];
            if let Some(second) = head.rfind(':') {
                let line = &head[second + 1..];
                if !line.is_empty() && line.chars().all(|ch| ch.is_ascii_digit()) {
                    return &head[..second];
                }
            }
            return head;
        }
    }
    token
}

fn file_name_part(token: &str) -> &str {
    token
        .rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(token)
}

fn has_known_extension(token: &str) -> bool {
    let name = file_name_part(token);
    let Some((stem, extension)) = name.rsplit_once('.') else {
        return false;
    };
    !stem.is_empty()
        && !extension.is_empty()
        && extension.len() <= 12
        && extension.chars().all(|ch| ch.is_ascii_alphanumeric())
        && KNOWN_EXTENSIONS
            .iter()
            .any(|known| extension.eq_ignore_ascii_case(known))
}

fn is_known_file_name(token: &str) -> bool {
    let name = file_name_part(token);
    KNOWN_FILE_NAMES
        .iter()
        .any(|known| name.eq_ignore_ascii_case(known))
}

fn has_path_separator(token: &str) -> bool {
    token.contains('/') || token.contains('\\')
}

fn is_windows_absolute(token: &str) -> bool {
    let bytes = token.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/')
}

fn looks_like_plain_path(token: &str) -> bool {
    if token.len() > 260 || token.starts_with('-') {
        return false;
    }
    if token.contains("://") || token.starts_with("www.") {
        return false;
    }
    let token = token.strip_prefix('@').unwrap_or(token);
    let token = strip_line_suffix(token);
    if token.is_empty() || token.contains('@') || token.contains(' ') {
        return false;
    }
    if is_known_file_name(token) || has_known_extension(token) {
        return true;
    }
    // Extension-less paths are only auto-linked when the writer made the path
    // explicit; `and/or` must not become a link.
    token.starts_with("./")
        || token.starts_with("../")
        || token.starts_with("~/")
        || token.starts_with('/')
        || is_windows_absolute(token)
}

fn looks_like_code_path(content: &str) -> bool {
    let token = content.trim();
    if token.is_empty() || token.len() > 260 {
        return false;
    }
    if token.chars().any(|ch| !is_path_char(ch)) {
        return false;
    }
    if token.contains("://") || token.starts_with('-') || token.starts_with("www.") {
        return false;
    }
    let token = token.strip_prefix('@').unwrap_or(token);
    let token = strip_line_suffix(token);
    if token.is_empty() || token.contains('@') {
        return false;
    }
    is_known_file_name(token) || has_known_extension(token) || has_path_separator(token)
}

/// Classify a Markdown link destination. Anything that is not an external URL
/// is treated as a file reference; the click resolver decides whether it
/// exists and whether it is a directory.
fn link_target(dest: &str) -> Option<LinkTargetIr> {
    let dest = dest.trim();
    if dest.is_empty() || dest.starts_with('#') {
        return None;
    }
    let lower = dest.to_ascii_lowercase();
    if lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("mailto:")
        || lower.starts_with("tel:")
    {
        return Some(LinkTargetIr::Url(dest.to_string().into()));
    }
    if let Some(path) = dest.strip_prefix("file://") {
        return Some(LinkTargetIr::File(path.to_string().into()));
    }
    // Unknown schemes (`vscode://`, `zotero://`, …) still belong to the
    // browser/app branch; Windows drive letters are not schemes.
    if let Some(scheme_end) = dest.find(':') {
        let scheme = &dest[..scheme_end];
        if scheme.len() > 1
            && scheme
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.'))
        {
            return Some(LinkTargetIr::Url(dest.to_string().into()));
        }
    }
    Some(LinkTargetIr::File(dest.to_string().into()))
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
                // ZCode styles links with color alone (underline only on
                // hover, which plain highlights cannot express).
                highlight.color = Some(theme.link);
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
    /// Byte offset in `inline.text` where each open Markdown link started.
    link_starts: Vec<usize>,
    /// Destination of each open Markdown link, aligned with `link_starts`.
    link_dests: Vec<String>,
    lists: Vec<ListIrState>,
    item_marker: Option<String>,
    item_indent: usize,
    quote_depth: usize,
    code_block: Option<(Option<String>, String)>,
    heading_level: Option<u8>,
    in_table: bool,
    table: Option<TableCollector>,
    /// Inside an image tag: alt text is collected here instead of leaking into
    /// the surrounding paragraph.
    in_image: bool,
    pending_image: Option<ImageIr>,
    image_alt: String,
}

impl IrBuilder {
    fn new(theme: &Theme) -> Self {
        Self {
            theme: theme.clone(),
            blocks: Vec::new(),
            inline: InlineBuffer::default(),
            inline_kinds: Vec::new(),
            link_starts: Vec::new(),
            link_dests: Vec::new(),
            lists: Vec::new(),
            item_marker: None,
            item_indent: 0,
            quote_depth: 0,
            code_block: None,
            heading_level: None,
            in_table: false,
            table: None,
            in_image: false,
            pending_image: None,
            image_alt: String::new(),
        }
    }

    fn handle_event(&mut self, event: Event<'_>) {
        if self.in_image {
            match event {
                Event::Text(text) => self.image_alt.push_str(&text),
                Event::Code(text) => self.image_alt.push_str(&text),
                Event::SoftBreak | Event::HardBreak => self.image_alt.push(' '),
                Event::End(TagEnd::Image) => {
                    if let Some(mut image) = self.pending_image.take() {
                        let alt = self.image_alt.trim();
                        if !alt.is_empty() {
                            image.alt = alt.to_string().into();
                        }
                        self.blocks.push(Rc::new(BlockIr::Image(image)));
                    }
                    self.image_alt.clear();
                    self.in_image = false;
                }
                _ => {}
            }
            return;
        }
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
                let start = self.inline.text.len();
                self.inline.push(
                    &text,
                    Some(HighlightStyle {
                        background_color: Some(self.theme.code_bg),
                        color: Some(self.theme.text),
                        ..Default::default()
                    }),
                );
                let range = start..self.inline.text.len();
                if range.start < range.end {
                    self.inline.protected.push(range.clone());
                    if looks_like_code_path(&text) {
                        self.inline.add_link(
                            range,
                            LinkTargetIr::File(text.trim().to_string().into()),
                            false,
                        );
                    }
                }
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
            Tag::Link { dest_url, .. } => {
                self.inline_kinds.push(InlineKind::Link);
                self.link_starts.push(self.inline.text.len());
                self.link_dests.push(dest_url.to_string());
            }
            Tag::Image {
                dest_url, title, ..
            } => {
                // An image is a block of its own. Flush the text before it (and
                // close an open list item) so the paragraph keeps its shape.
                if self.item_marker.is_some() {
                    self.flush_list_item();
                } else {
                    self.flush_inline();
                }
                let alt = if title.is_empty() {
                    dest_url.to_string()
                } else {
                    title.to_string()
                };
                self.pending_image = Some(ImageIr {
                    alt: alt.into(),
                    src: dest_url.to_string().into(),
                });
                self.image_alt.clear();
                self.in_image = true;
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
            TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough => {
                self.inline_kinds.pop();
            }
            TagEnd::Link => {
                self.inline_kinds.pop();
                if let Some(start) = self.link_starts.pop() {
                    let range = start..self.inline.text.len();
                    if let Some(target) = self.link_dests.pop().and_then(|dest| link_target(&dest))
                    {
                        self.inline.add_link(range, target, true);
                    }
                }
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
        self.inline.finish_targets();
        let item = Rc::new(ItemIr {
            marker: marker.into(),
            indent: self.item_indent,
            text: self.inline.text.clone().into(),
            highlights: self.inline.highlights.clone().into(),
            links: self.inline.links.clone().into(),
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
        self.inline.finish_targets();
        let text = TextIr {
            text: self.inline.text.clone().into(),
            highlights: self.inline.highlights.clone().into(),
            links: self.inline.links.clone().into(),
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
        // ZCode heading scale: 18/16/14 with semibold weight, no bold jumps.
        let (size, weight) = match level {
            1 => (18.0, FontWeight::SEMIBOLD),
            2 => (16.0, FontWeight::SEMIBOLD),
            _ => (14.0, FontWeight::SEMIBOLD),
        };
        self.inline.finish_targets();
        let text = TextIr {
            text: self.inline.text.clone().into(),
            highlights: self.inline.highlights.clone().into(),
            links: self.inline.links.clone().into(),
            size,
            weight,
            heading: true,
            quote: false,
        };
        self.inline.clear();
        self.blocks.push(Rc::new(BlockIr::Text(text)));
    }

    fn finish(mut self) -> Blocks {
        // A malformed image without its end tag still renders as an image
        // rather than disappearing from the document.
        if let Some(image) = self.pending_image.take() {
            self.blocks.push(Rc::new(BlockIr::Image(image)));
        }
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
    for color in [
        theme.text,
        theme.text_muted,
        theme.accent,
        theme.code_bg,
        theme.link,
    ] {
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

/// Occupancy of the memoized markdown IR, for the memory diagnostics panel.
///
/// `*_bytes` sum the source text each entry was built from. The IR keeps its
/// own strings, so this is a lower bound on the cache's footprint — enough to
/// spot a cache that grew far beyond the transcript it serves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MarkdownCacheStats {
    pub documents: usize,
    pub document_bytes: u64,
    pub chunks: usize,
    pub chunk_bytes: u64,
}

pub fn markdown_cache_stats() -> MarkdownCacheStats {
    CACHE.with(|cache| {
        let cache = cache.borrow();
        MarkdownCacheStats {
            documents: cache.docs.len(),
            document_bytes: cache.docs.keys().map(|key| key.len as u64).sum(),
            chunks: cache.chunks.len(),
            chunk_bytes: cache.chunks.keys().map(|key| key.len as u64).sum(),
        }
    })
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
    render_markdown_keyed_with(theme, source, key, key, None)
}

/// [`render_markdown_keyed`] with clickable file/URL targets and images.
/// `element_key` only needs to be unique among rows that are on screen at the
/// same time; it keeps state and hit-testing separate for identical bodies.
pub fn render_markdown_keyed_with(
    theme: &Theme,
    source: &str,
    key: u64,
    element_key: u64,
    interaction: Option<&MarkdownInteraction>,
) -> AnyElement {
    let cache_key = DocKey {
        theme_key: theme_ir_key(theme),
        hash: key,
        len: source.len(),
    };
    let blocks = CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let MarkdownCache {
            docs, stamp, hits, ..
        } = &mut *cache;
        match cache_get(docs, &cache_key, stamp, hits) {
            Some(blocks) => blocks,
            None => {
                let blocks = build_blocks(source, theme);
                cache_put(docs, cache_key, blocks.clone(), stamp, MAX_CACHED_DOCS);
                blocks
            }
        }
    });
    render_parsed(theme, &blocks, interaction, element_key)
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
    render_markdown_streaming_with(theme, source, hash_source(source), None)
}

/// [`render_markdown_streaming`] with clickable targets. `doc_key` keeps
/// element identities stable while the tail of the stream grows.
pub fn render_markdown_streaming_with(
    theme: &Theme,
    source: &str,
    doc_key: u64,
    interaction: Option<&MarkdownInteraction>,
) -> AnyElement {
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
    render_parsed(theme, &Rc::from(blocks), interaction, doc_key)
}

/// Render one pre-split markdown block, memoized in the chunk cache under the
/// caller-supplied content hash. Virtual chat items that render a stable
/// slice of a longer document call this every frame; the hash makes the
/// per-frame cost an O(len) scan instead of a re-parse.
pub fn render_markdown_chunk(theme: &Theme, source: &str, hash: u64) -> AnyElement {
    render_markdown_chunk_with(theme, source, hash, hash, None)
}

/// [`render_markdown_chunk`] with clickable targets.
pub fn render_markdown_chunk_with(
    theme: &Theme,
    source: &str,
    hash: u64,
    element_key: u64,
    interaction: Option<&MarkdownInteraction>,
) -> AnyElement {
    render_parsed(
        theme,
        &Rc::from(cached_chunk_blocks(theme, source, hash)),
        interaction,
        element_key,
    )
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

fn render_parsed(
    theme: &Theme,
    blocks: &[Rc<BlockIr>],
    interaction: Option<&MarkdownInteraction>,
    doc_key: u64,
) -> AnyElement {
    let children = blocks
        .iter()
        .enumerate()
        .map(|(index, block)| render_block(theme, block, interaction, doc_key, index));
    div()
        .flex()
        .flex_col()
        .w_full()
        .gap_3()
        .children(children)
        .into_any_element()
}

fn render_block(
    theme: &Theme,
    block: &Rc<BlockIr>,
    interaction: Option<&MarkdownInteraction>,
    doc_key: u64,
    block_ix: usize,
) -> AnyElement {
    match block.as_ref() {
        BlockIr::Text(text) => render_text_block(theme, text, interaction, doc_key, block_ix),
        BlockIr::Item(item) => render_item_row(theme, item, interaction, doc_key, block_ix),
        BlockIr::List { items, quote } => {
            let mut container =
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(items.iter().enumerate().map(|(index, item)| {
                        render_item_row(theme, item, interaction, doc_key, block_ix + index)
                    }));
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
        BlockIr::Image(image) => render_image_block(theme, image, interaction, doc_key, block_ix),
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

/// Plain-prose paths get link color. A clickable range that already carries a
/// highlight (inline code) keeps its background and gains the link color, so
/// the run still reads as a file chip. Ranges are split at highlight/link
/// boundaries because `StyledText::with_highlights` requires sorted,
/// non-overlapping runs.
fn styled_inline_with_links(
    theme: &Theme,
    text: SharedString,
    highlights: &[(Range<usize>, HighlightStyle)],
    links: &[LinkIr],
) -> StyledText {
    if links.is_empty() {
        return styled_inline(text, highlights);
    }
    let mut points: Vec<usize> = Vec::with_capacity(highlights.len() * 2 + links.len() * 2 + 2);
    points.push(0);
    points.push(text.len());
    for (range, _) in highlights {
        points.push(range.start);
        points.push(range.end);
    }
    for link in links {
        points.push(link.range.start);
        points.push(link.range.end);
    }
    points.sort_unstable();
    points.dedup();

    let mut runs: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
    for pair in points.windows(2) {
        let (start, end) = (pair[0], pair[1]);
        if start >= end {
            continue;
        }
        let mut style = HighlightStyle::default();
        for (range, highlight) in highlights {
            if range.start <= start && end <= range.end {
                style = style.highlight(*highlight);
            }
        }
        if links
            .iter()
            .any(|link| link.range.start <= start && end <= link.range.end)
        {
            style.color = Some(theme.link);
        }
        if style == HighlightStyle::default() {
            continue;
        }
        if let Some((range, previous)) = runs.last_mut()
            && range.end == start
            && *previous == style
        {
            range.end = end;
            continue;
        }
        runs.push((start..end, style));
    }
    if runs.is_empty() {
        StyledText::new(text)
    } else {
        StyledText::new(text).with_highlights(runs)
    }
}

/// The inline element for one text run: a plain styled text when nothing is
/// clickable, an [`gpui::InteractiveText`] with per-target hit areas otherwise.
fn text_run_element(
    theme: &Theme,
    text: SharedString,
    highlights: &[(Range<usize>, HighlightStyle)],
    links: &[LinkIr],
    interaction: Option<&MarkdownInteraction>,
    id: SharedString,
) -> AnyElement {
    let styled = styled_inline_with_links(theme, text, highlights, links);
    let Some(interaction) = interaction.filter(|_| !links.is_empty()) else {
        return styled.into_any_element();
    };
    let ranges: Vec<Range<usize>> = links.iter().map(|link| link.range.clone()).collect();
    let targets: Vec<MarkdownTarget> = links.iter().map(LinkIr::markdown_target).collect();
    let click_interaction = interaction.clone();
    let click_targets = targets.clone();
    let right_interaction = interaction.clone();
    let right_targets = targets;
    let mut element = gpui::InteractiveText::new(ElementId::Name(id), styled);
    if !ranges.is_empty() {
        element = element.on_click(ranges.clone(), move |index, position, window, cx| {
            if let Some(target) = click_targets.get(index).cloned() {
                click_interaction.activate(target, MouseButton::Left, position, window, cx);
            }
        });
        element = element.on_right_click(ranges, move |index, position, window, cx| {
            if let Some(target) = right_targets.get(index).cloned() {
                right_interaction.activate(target, MouseButton::Right, position, window, cx);
            }
        });
    }
    element.into_any_element()
}

fn render_text_block(
    theme: &Theme,
    ir: &TextIr,
    interaction: Option<&MarkdownInteraction>,
    doc_key: u64,
    block_ix: usize,
) -> AnyElement {
    // ZCode body copy runs at a 1.75 line height; match it so streamed
    // markdown breathes the same way.
    let element = div()
        .w_full()
        .text_size(px(ir.size))
        .line_height(px(ir.size * 1.75))
        .font_weight(ir.weight)
        .text_color(theme.text)
        .child(text_run_element(
            theme,
            ir.text.clone(),
            &ir.highlights,
            &ir.links,
            interaction,
            SharedString::from(format!("md-{doc_key}-t{block_ix}")),
        ));
    let element = if ir.heading {
        div().w_full().pt(px(2.0)).child(element).into_any_element()
    } else {
        element.into_any_element()
    };
    let element = match render_linked_image_strip(theme, &ir.links, interaction, doc_key, block_ix)
    {
        Some(strip) => div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(element)
            .child(strip)
            .into_any_element(),
        None => element,
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

/// A text paragraph that references a local image path (usually inline code)
/// gets a thumbnail strip underneath, so an artifact is visible without
/// opening the link menu. Only images that actually exist are shown.
fn render_linked_image_strip(
    theme: &Theme,
    links: &[LinkIr],
    interaction: Option<&MarkdownInteraction>,
    doc_key: u64,
    block_ix: usize,
) -> Option<AnyElement> {
    let interaction = interaction?;
    let mut images: Vec<(SharedString, PathBuf)> = Vec::new();
    for link in links {
        let LinkTargetIr::File(src) = &link.target else {
            continue;
        };
        if !is_image_source(src) {
            continue;
        }
        let Some(path) = resolve_local_image(src, interaction.base_dir()) else {
            continue;
        };
        if images.iter().any(|(existing, _)| existing == src) {
            continue;
        }
        images.push((src.clone(), path));
        if images.len() >= 4 {
            break;
        }
    }
    if images.is_empty() {
        return None;
    }

    let mut row = div().flex().flex_wrap().gap(px(6.0));
    for (index, (src, path)) in images.into_iter().enumerate() {
        let click_interaction = interaction.clone();
        let click_target = MarkdownTarget::File(src.clone());
        let right_interaction = interaction.clone();
        let right_target = MarkdownTarget::File(src);
        row = row.child(
            img(path)
                .id(ElementId::named_usize(
                    SharedString::from(format!("md-{doc_key}-strip")),
                    block_ix * 8 + index,
                ))
                .w(px(220.0))
                .h(px(140.0))
                .bg(theme.surface)
                .object_fit(ObjectFit::Contain)
                .rounded(px(radius::SM))
                .border_1()
                .border_color(theme.border)
                .on_click(move |event, window, cx| {
                    click_interaction.activate(
                        click_target.clone(),
                        MouseButton::Left,
                        event.position(),
                        window,
                        cx,
                    );
                })
                .on_mouse_down(MouseButton::Right, move |event, window, cx| {
                    cx.stop_propagation();
                    right_interaction.activate(
                        right_target.clone(),
                        MouseButton::Right,
                        event.position,
                        window,
                        cx,
                    );
                }),
        );
    }
    Some(row.into_any_element())
}

fn is_image_source(src: &str) -> bool {
    let src = src.trim();
    if src.starts_with("http://") || src.starts_with("https://") || src.starts_with("data:") {
        return false;
    }
    let src = src.split(['#', '?']).next().unwrap_or(src);
    Path::new(src)
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
        .is_some_and(|extension| {
            matches!(
                extension.as_str(),
                "png"
                    | "jpg"
                    | "jpeg"
                    | "gif"
                    | "webp"
                    | "bmp"
                    | "ico"
                    | "avif"
                    | "tif"
                    | "tiff"
                    | "svg"
            )
        })
}

fn render_item_row(
    theme: &Theme,
    ir: &ItemIr,
    interaction: Option<&MarkdownInteraction>,
    doc_key: u64,
    item_ix: usize,
) -> AnyElement {
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
                .line_height(px(text_size::BASE * 1.75))
                .font_weight(FontWeight::NORMAL)
                .text_color(theme.text)
                .child(text_run_element(
                    theme,
                    ir.text.clone(),
                    &ir.highlights,
                    &ir.links,
                    interaction,
                    SharedString::from(format!("md-{doc_key}-i{item_ix}")),
                )),
        )
        .into_any_element()
}

fn render_image_block(
    theme: &Theme,
    image: &ImageIr,
    interaction: Option<&MarkdownInteraction>,
    doc_key: u64,
    block_ix: usize,
) -> AnyElement {
    let Some(interaction) = interaction else {
        return render_image_fallback(theme, image, None, doc_key, block_ix);
    };
    let src = image.src.as_ref();
    let target = if is_http_url(src) {
        MarkdownTarget::Url(image.src.clone())
    } else {
        MarkdownTarget::File(image.src.clone())
    };
    let source = if is_http_url(src) {
        Some(img(src.to_string()))
    } else {
        resolve_local_image(src, interaction.base_dir()).map(img)
    };
    let Some(base) = source else {
        return render_image_fallback(theme, image, Some(interaction), doc_key, block_ix);
    };

    let click_interaction = interaction.clone();
    let click_target = target.clone();
    let right_interaction = interaction.clone();
    let element = base
        .id(ElementId::Name(
            format!("md-{doc_key}-img{block_ix}").into(),
        ))
        .w_full()
        .max_w(px(640.0))
        .h(px(340.0))
        .bg(theme.surface)
        .object_fit(ObjectFit::Contain)
        .rounded(px(radius::MD))
        .border_1()
        .border_color(theme.border)
        .on_click(move |event, window, cx| {
            click_interaction.activate(
                click_target.clone(),
                MouseButton::Left,
                event.position(),
                window,
                cx,
            );
        })
        .on_mouse_down(MouseButton::Right, move |event, window, cx| {
            cx.stop_propagation();
            right_interaction.activate(
                target.clone(),
                MouseButton::Right,
                event.position,
                window,
                cx,
            );
        })
        .into_any_element();

    let mut column = div()
        .w_full()
        .flex()
        .flex_col()
        .items_start()
        .gap(px(4.0))
        .child(element);
    if !image.alt.is_empty() && image.alt.as_ref() != src {
        column = column.child(
            div()
                .text_size(px(10.5))
                .text_color(theme.text_faint)
                .child(image.alt.clone()),
        );
    }
    column.into_any_element()
}

fn render_image_fallback(
    theme: &Theme,
    image: &ImageIr,
    interaction: Option<&MarkdownInteraction>,
    doc_key: u64,
    block_ix: usize,
) -> AnyElement {
    let label = if image.alt.is_empty() || image.alt.as_ref() == image.src.as_ref() {
        image.src.clone()
    } else {
        SharedString::from(format!("{} · {}", image.alt, image.src))
    };
    let base = div()
        .id(ElementId::Name(
            format!("md-{doc_key}-imgfallback{block_ix}").into(),
        ))
        .flex()
        .items_center()
        .gap(px(7.0))
        .h(px(28.0))
        .px(px(9.0))
        .rounded(px(radius::SM))
        .border_1()
        .border_color(theme.border)
        .bg(theme.surface)
        .cursor_pointer()
        .hover(|style| style.bg(theme.hover))
        .child(
            svg()
                .path(icons::IMAGE)
                .size(px(12.0))
                .text_color(theme.text_faint),
        )
        .child(
            div()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(11.5))
                .text_color(theme.link)
                .child(label),
        );
    let Some(interaction) = interaction else {
        return base.into_any_element();
    };
    let target = if is_http_url(image.src.as_ref()) {
        MarkdownTarget::Url(image.src.clone())
    } else {
        MarkdownTarget::File(image.src.clone())
    };
    let click_interaction = interaction.clone();
    let click_target = target.clone();
    let right_interaction = interaction.clone();
    let right_target = target;
    base.on_click(move |event, window, cx| {
        click_interaction.activate(
            click_target.clone(),
            MouseButton::Left,
            event.position(),
            window,
            cx,
        );
    })
    .on_mouse_down(MouseButton::Right, move |event, window, cx| {
        cx.stop_propagation();
        right_interaction.activate(
            right_target.clone(),
            MouseButton::Right,
            event.position,
            window,
            cx,
        );
    })
    .into_any_element()
}

fn resolve_local_image(src: &str, base_dir: &Path) -> Option<PathBuf> {
    let src = src.trim();
    if src.is_empty() || src.starts_with("data:") {
        return None;
    }
    let src = src.strip_prefix("file://").unwrap_or(src);
    // Windows file URLs arrive as `/C:/dir/file`.
    #[cfg(target_os = "windows")]
    let src = src
        .strip_prefix('/')
        .filter(|rest| rest.as_bytes().get(1) == Some(&b':'))
        .unwrap_or(src);
    let src = src.split(['#', '?']).next().unwrap_or(src);
    let decoded = percent_decode(src);
    let path = if let Some(rest) = decoded.strip_prefix("~/") {
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(|home| PathBuf::from(home).join(rest))
            .unwrap_or_else(|| base_dir.join(rest))
    } else {
        let path = Path::new(&decoded);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            base_dir.join(path)
        }
    };
    path.is_file().then_some(path)
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
                out.push(high * 16 + low);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
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

    // ZCode frames code blocks with a hairline border on the card background;
    // the header sits on the same surface instead of a contrasting strip.
    div()
        .w_full()
        .rounded(px(radius::MD))
        .border_1()
        .border_color(theme.border)
        .bg(theme.code_bg)
        .overflow_hidden()
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .px_3()
                .py(px(6.0))
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

    #[test]
    fn inline_code_paths_and_markdown_links_become_targets() {
        let theme = theme();
        let blocks = build_blocks(
            "改 `crates/agent-core` 与 [README](README.md)，外加 src/main.rs:12 和 https://example.com/a?b=1。",
            &theme,
        );
        let BlockIr::Text(text) = blocks[0].as_ref() else {
            panic!("expected text block");
        };
        let targets: Vec<&LinkTargetIr> = text.links.iter().map(|link| &link.target).collect();
        assert_eq!(targets.len(), 4, "links: {targets:?}");
        assert!(matches!(targets[0], LinkTargetIr::File(path) if path == "crates/agent-core"));
        assert!(matches!(targets[1], LinkTargetIr::File(path) if path == "README.md"));
        assert!(matches!(targets[2], LinkTargetIr::File(path) if path == "src/main.rs:12"));
        assert!(matches!(targets[3], LinkTargetIr::Url(url) if url == "https://example.com/a?b=1"));
        // The clickable ranges cover exactly the visible token, not the
        // surrounding prose or the trailing punctuation.
        assert_eq!(&text.text[text.links[0].range.clone()], "crates/agent-core");
        assert_eq!(
            &text.text[text.links[3].range.clone()],
            "https://example.com/a?b=1"
        );
    }

    #[test]
    fn prose_is_not_mistaken_for_paths() {
        assert!(looks_like_plain_path("src/main.rs"));
        assert!(looks_like_plain_path("README.md"));
        assert!(looks_like_plain_path("./scripts"));
        assert!(!looks_like_plain_path("and/or"));
        assert!(!looks_like_plain_path("example.com"));
        assert!(!looks_like_plain_path("v1.2.3"));
        assert!(!looks_like_plain_path("e.g."));
        assert!(!looks_like_plain_path("foo@bar.com"));
        assert!(looks_like_code_path("crates/agent-core"));
        assert!(looks_like_code_path("Cargo.toml"));
        assert!(looks_like_code_path("docs/设计.md"));
        assert!(!looks_like_code_path("cargo test -p effibuddy-ui"));
        assert!(!looks_like_code_path("--features"));
    }

    #[test]
    fn markdown_images_become_image_blocks() {
        let theme = theme();
        let blocks = build_blocks(
            "before\n\n![dark chat](docs/screenshots/dark-chat.png)\n\nafter",
            &theme,
        );
        assert_eq!(blocks.len(), 3);
        let BlockIr::Image(image) = blocks[1].as_ref() else {
            panic!("expected image block, got {:?}", blocks[1]);
        };
        assert_eq!(image.src, "docs/screenshots/dark-chat.png");
        assert_eq!(image.alt, "dark chat");
    }

    #[test]
    fn inline_image_splits_the_paragraph_without_losing_text() {
        let theme = theme();
        let blocks = build_blocks("before ![shot](a.png) after", &theme);
        let texts: Vec<String> = blocks
            .iter()
            .filter_map(|block| match block.as_ref() {
                BlockIr::Text(text) => Some(text.text.trim().to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["before", "after"]);
        assert!(
            blocks
                .iter()
                .any(|block| matches!(block.as_ref(), BlockIr::Image(_)))
        );
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
                let element = render_parsed(&theme, &blocks, None, 0);
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
