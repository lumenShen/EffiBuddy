//! Accessibility-tree → `[ref]` snapshot shaping.
//!
//! The page-side script collects geometry + roles; this module prunes the tree,
//! assigns short `eN` handles and renders the compact text form the model reads.
//! Keeping shaping in Rust means the CDP backend and the fake backend produce
//! identical snapshots, which is what makes the tool testable without a browser.
//!
//! Why an AX tree at all: the default model has no vision, and raw HTML is
//! 100k+ tokens. A pruned interactive tree is 1–8k tokens and lets the model
//! act by `[ref]` instead of guessing coordinates.

use serde::Deserialize;

use crate::types::{AxNode, Rect, Snapshot, SnapshotMode};

/// Hard cap on nodes the page script collects; protects against pathological
/// pages before pruning even runs.
pub const MAX_RAW_NODES: usize = 4000;

/// Evaluated in the page. Returns `JSON.stringify(RawAxTree)`-compatible text.
///
/// Collection is deliberately generous (interactive nodes, headings, images,
/// live regions and text blocks); pruning by mode happens in [`build_snapshot`]
/// so one script serves all three snapshot modes.
pub const AX_TREE_JS: &str = r#"(() => {
  try {
    const MAX = 4000;
    const viewport = { x: 0, y: 0, width: window.innerWidth, height: window.innerHeight };
    const skipTags = new Set(['SCRIPT','STYLE','NOSCRIPT','META','LINK','HEAD','TITLE','TEMPLATE','BR','PATH','CIRCLE','RECT','LINE','POLYGON','DEFS','USE','G']);
    const blockTags = new Set(['P','LI','TD','TH','DD','DT','BLOCKQUOTE','PRE','ARTICLE','SECTION','H1','H2','H3','H4','H5','H6','LABEL','FIGCAPTION','SUMMARY']);
    const interactiveRoles = new Set(['button','link','checkbox','radio','tab','menuitem','menuitemcheckbox','menuitemradio','switch','combobox','textbox','searchbox','spinbutton','slider','listbox','option','treeitem']);

    const clip = (value, limit) => {
      if (value === null || value === undefined) return '';
      let text = String(value).replace(/\s+/g, ' ').trim();
      return text.length > limit ? text.slice(0, limit) + '…' : text;
    };

    const nameOf = (element) => {
      const aria = element.getAttribute && element.getAttribute('aria-label');
      if (aria) return clip(aria, 120);
      const labelledBy = element.getAttribute && element.getAttribute('aria-labelledby');
      if (labelledBy) {
        const parts = labelledBy.split(/\s+/)
          .map((id) => document.getElementById(id))
          .filter(Boolean)
          .map((node) => node.innerText || node.textContent || '');
        const joined = clip(parts.join(' '), 120);
        if (joined) return joined;
      }
      const text = element.innerText || element.textContent || '';
      if (text && text.trim()) return clip(text, 120);
      return clip(
        element.getAttribute && (element.getAttribute('placeholder')
          || element.getAttribute('alt')
          || element.getAttribute('title')
          || element.getAttribute('value')) || '',
        120,
      );
    };

    const roleOf = (element) => {
      const explicit = element.getAttribute && element.getAttribute('role');
      if (explicit) return explicit.toLowerCase();
      const tag = element.tagName.toLowerCase();
      if (tag === 'a') return element.hasAttribute('href') ? 'link' : 'generic';
      if (tag === 'button') return 'button';
      if (tag === 'select') return element.multiple ? 'listbox' : 'combobox';
      if (tag === 'textarea') return 'textbox';
      if (tag === 'option') return 'option';
      if (tag === 'summary') return 'button';
      if (tag === 'img') return 'img';
      if (/^h[1-6]$/.test(tag)) return 'heading';
      if (tag === 'input') {
        const type = (element.getAttribute('type') || 'text').toLowerCase();
        if (type === 'checkbox') return 'checkbox';
        if (type === 'radio') return 'radio';
        if (type === 'submit' || type === 'button' || type === 'reset') return 'button';
        if (type === 'range') return 'slider';
        if (type === 'number') return 'spinbutton';
        if (type === 'search') return 'searchbox';
        if (type === 'hidden') return 'generic';
        return 'textbox';
      }
      if (tag === 'nav') return 'navigation';
      if (tag === 'main') return 'main';
      if (tag === 'header') return 'banner';
      if (tag === 'footer') return 'contentinfo';
      if (tag === 'form') return 'form';
      if (tag === 'table') return 'table';
      if (tag === 'ul' || tag === 'ol') return 'list';
      if (tag === 'li') return 'listitem';
      return tag;
    };

    const depthOf = (element) => {
      let depth = 0;
      let node = element.parentElement;
      while (node && depth < 80) { depth += 1; node = node.parentElement; }
      return depth;
    };

    const isHidden = (element, style) => {
      if (style.display === 'none' || style.visibility === 'hidden' || style.visibility === 'collapse') return true;
      if (parseFloat(style.opacity) === 0) return true;
      return false;
    };

    // The accessibility tree is only cosmetic to us; we work on the DOM but
    // derive ARIA-ish fields, which is what makes `[ref]` actions reliable.
    const nodes = [];
    let truncated = false;
    const all = document.querySelectorAll('*');
    for (const element of all) {
      if (nodes.length >= MAX) { truncated = true; break; }
      const tag = element.tagName;
      if (skipTags.has(tag)) continue;
      if (element.namespaceURI && element.namespaceURI.includes('svg') && tag !== 'svg') {
        // SVG sub-nodes carry no useful semantics; the <svg> itself is kept as
        // an image when it has an accessible name.
        if (!(tag === 'SVG' && (element.getAttribute('aria-label') || element.getAttribute('title')))) continue;
      }

      let style;
      try { style = window.getComputedStyle(element); } catch (_) { style = null; }
      if (style && isHidden(element, style)) continue;

      const role = roleOf(element);
      const name = nameOf(element);
      let rect = { x: 0, y: 0, width: 0, height: 0 };
      try {
        const box = element.getBoundingClientRect();
        rect = { x: box.x, y: box.y, width: box.width, height: box.height };
      } catch (_) {}

      const interactive = interactiveRoles.has(role)
        || element.hasAttribute('onclick')
        || element.hasAttribute('tabindex')
        || (element.isContentEditable === true)
        || ['button','link','checkbox','radio','textbox','combobox','option','slider','switch','spinbutton','searchbox'].includes(role);
      const heading = role === 'heading' || /^h[1-6]$/.test(tag.toLowerCase());
      const image = role === 'img';
      const live = element.hasAttribute('aria-live')
        || ['alert','status','log'].includes((element.getAttribute('role') || '').toLowerCase());
      const hasText = (element.innerText || '').trim().length > 0;

      // Only interesting nodes plus text blocks: a plain <div> wrapper is noise.
      const interesting = interactive || heading || image || live;
      const textBlock = blockTags.has(tag) && hasText;
      if (!interesting && !textBlock) continue;

      const type = (element.getAttribute && (element.getAttribute('type') || '')).toLowerCase();
      let value = null;
      if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT') {
        value = type === 'password' ? '•••' : clip(element.value || '', 120);
      }
      let checked = null;
      if (tag === 'INPUT' && (type === 'checkbox' || type === 'radio')) {
        checked = element.indeterminate ? 'mixed' : (element.checked ? 'true' : 'false');
      } else if (element.getAttribute && element.getAttribute('aria-checked')) {
        checked = element.getAttribute('aria-checked');
      }
      let expanded = null;
      if (element.getAttribute && (element.getAttribute('aria-expanded') === 'true' || element.getAttribute('aria-expanded') === 'false')) {
        expanded = element.getAttribute('aria-expanded') === 'true';
      }
      let level = null;
      if (heading) {
        const aria = element.getAttribute && element.getAttribute('aria-level');
        level = aria ? parseInt(aria, 10) : (parseInt(tag.replace(/[^0-9]/g, ''), 10) || 2);
      }
      const disabled = element.disabled === true
        || (element.getAttribute && element.getAttribute('aria-disabled') === 'true');
      const offscreen = rect.width > 0 && rect.height > 0
        && (rect.x + rect.width <= 0 || rect.y + rect.height <= 0
          || rect.x >= viewport.width || rect.y >= viewport.height);

      // Stamp the element so the backend can resolve a ref back to this exact
      // node later without re-deriving it from text or geometry.
      const index = nodes.length;
      try { element.setAttribute('data-effi-i', String(index)); } catch (_) {}

      nodes.push({
        index,
        role,
        name,
        value,
        depth: depthOf(element),
        bounds: rect.width > 0 && rect.height > 0 ? rect : null,
        editable: !!element.isContentEditable
          || tag === 'TEXTAREA'
          || (tag === 'INPUT' && ['text','search','email','url','tel','password','number'].includes(type || 'text')),
        focusable: element.tabIndex >= 0 || interactive,
        disabled: !!disabled,
        checked,
        expanded,
        level,
        url: (role === 'link' && element.getAttribute) ? element.getAttribute('href') : null,
        offscreen: !!offscreen,
        text: (textBlock || heading) ? clip(element.innerText || '', 400) : null,
      });
    }

    return JSON.stringify({
      url: location.href,
      title: document.title,
      viewport,
      truncated,
      nodes,
    });
  } catch (error) {
    return JSON.stringify({
      url: location.href,
      title: document.title,
      viewport: { x: 0, y: 0, width: 0, height: 0 },
      nodes: [],
    });
  }
})()"#;

/// Evaluated in the page: readable text for `SnapshotMode::Text`.
pub const PAGE_TEXT_JS: &str = r#"(() => {
  try {
    const body = document.body ? document.body.innerText : '';
    if (body && body.trim().length > 20) return body;
    const root = document.documentElement ? document.documentElement.innerText : '';
    return root || body || '';
  } catch (error) {
    return '';
  }
})()"#;

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct RawAxTree {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub viewport: RawRect,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub nodes: Vec<RawAxNode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Deserialize)]
pub struct RawRect {
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
    #[serde(default)]
    pub width: f64,
    #[serde(default)]
    pub height: f64,
}

impl From<RawRect> for Rect {
    fn from(raw: RawRect) -> Self {
        Rect::new(raw.x, raw.y, raw.width, raw.height)
    }
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct RawAxNode {
    /// Page-side stamp (`data-effi-i`). Lets the backend re-find the exact
    /// element for a ref without guessing by role, name or geometry.
    #[serde(default)]
    pub index: Option<u32>,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub depth: u32,
    #[serde(default)]
    pub bounds: Option<RawRect>,
    #[serde(default)]
    pub editable: bool,
    #[serde(default)]
    pub focusable: bool,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub checked: Option<String>,
    #[serde(default)]
    pub expanded: Option<bool>,
    #[serde(default)]
    pub level: Option<u32>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub offscreen: bool,
    #[serde(default)]
    pub text: Option<String>,
}

/// Prune, tag and shape one snapshot. `id` must be unique per snapshot.
pub fn build_snapshot(
    raw: &RawAxTree,
    mode: SnapshotMode,
    max_nodes: usize,
    id: impl Into<String>,
) -> Snapshot {
    build_snapshot_with_stamps(raw, mode, max_nodes, id).0
}

/// Like [`build_snapshot`], also returning `ref -> page stamp`, which the CDP
/// backend needs to act on a ref without guessing by role, name or geometry.
pub fn build_snapshot_with_stamps(
    raw: &RawAxTree,
    mode: SnapshotMode,
    max_nodes: usize,
    id: impl Into<String>,
) -> (Snapshot, Vec<(String, u32)>) {
    let id = id.into();
    let viewport = Some(Rect::from(raw.viewport)).filter(|rect| rect.width > 0.0);
    let total_nodes = raw.nodes.len();

    if mode == SnapshotMode::Text {
        let text = render_text(raw);
        return (
            Snapshot {
                id,
                url: raw.url.clone(),
                title: raw.title.clone(),
                mode,
                nodes: Vec::new(),
                text: Some(text),
                total_nodes,
                truncated: raw.truncated,
                viewport,
            },
            Vec::new(),
        );
    }

    let mut kept: Vec<&RawAxNode> = Vec::new();
    for node in &raw.nodes {
        if mode == SnapshotMode::Interesting && !is_interesting(node) {
            continue;
        }
        if !has_identity(node) {
            continue;
        }
        let duplicate = kept.iter().any(|existing| same_place(existing, node));
        if duplicate {
            continue;
        }
        kept.push(node);
    }

    let limit = max_nodes.max(1);
    let mut truncated = raw.truncated;
    if kept.len() > limit {
        kept.truncate(limit);
        truncated = true;
    }

    let mut nodes: Vec<AxNode> = Vec::with_capacity(kept.len());
    let mut stamps: Vec<(String, u32)> = Vec::with_capacity(kept.len());
    for (index, raw_node) in kept.iter().enumerate() {
        let id = format!("e{}", index + 1);
        if let Some(stamp) = raw_node.index {
            stamps.push((id.clone(), stamp));
        }
        nodes.push(AxNode {
            id,
            role: normalize_role(&raw_node.role),
            name: squeeze(&raw_node.name, 120),
            value: raw_node.value.clone().filter(|value| !value.is_empty()),
            depth: raw_node.depth,
            bounds: raw_node.bounds.map(Rect::from).filter(Rect::is_visible),
            editable: raw_node.editable,
            focusable: raw_node.focusable,
            disabled: raw_node.disabled,
            checked: raw_node.checked.clone(),
            expanded: raw_node.expanded,
            level: raw_node.level,
            url: raw_node.url.clone(),
            offscreen: raw_node.offscreen,
        });
    }

    // Newlines are the renderer's job; names must never contain them.
    for node in &mut nodes {
        node.name = node.name.replace('\n', " ");
    }

    (
        Snapshot {
            id,
            url: raw.url.clone(),
            title: raw.title.clone(),
            mode,
            nodes,
            text: None,
            total_nodes,
            truncated,
            viewport,
        },
        stamps,
    )
}

/// Interactive, heading, image, live-region and named node detection.
fn is_interesting(node: &RawAxNode) -> bool {
    if node.editable || node.focusable || node.disabled {
        return true;
    }
    if node.checked.is_some() || node.expanded.is_some() || node.level.is_some() {
        return true;
    }
    if node.url.is_some() {
        return true;
    }
    matches!(
        normalize_role(&node.role).as_str(),
        "button"
            | "link"
            | "checkbox"
            | "radio"
            | "tab"
            | "menuitem"
            | "menuitemcheckbox"
            | "menuitemradio"
            | "switch"
            | "combobox"
            | "textbox"
            | "searchbox"
            | "spinbutton"
            | "slider"
            | "listbox"
            | "option"
            | "treeitem"
            | "heading"
            | "img"
            | "alert"
            | "status"
            | "log"
    )
}

/// A node with neither a name, a value nor a role worth reporting is noise.
fn has_identity(node: &RawAxNode) -> bool {
    if !node.name.trim().is_empty() {
        return true;
    }
    if node
        .value
        .as_deref()
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
    {
        return true;
    }
    // Nameless but actionable nodes still matter (icon buttons, bare inputs).
    node.editable
        || node.checked.is_some()
        || node.level.is_some()
        || matches!(normalize_role(&node.role).as_str(), "img" | "heading")
}

/// Two nodes that share role+name and overlap almost completely are the same
/// element reported twice (a nested span inside a button, typically).
fn same_place(left: &RawAxNode, right: &RawAxNode) -> bool {
    if normalize_role(&left.role) != normalize_role(&right.role) {
        return false;
    }
    if left.name.trim() != right.name.trim() || left.name.trim().is_empty() {
        return false;
    }
    let (Some(a), Some(b)) = (left.bounds.map(Rect::from), right.bounds.map(Rect::from)) else {
        return false;
    };
    let left_area = a.width * a.height;
    let right_area = b.width * b.height;
    if left_area <= 0.0 || right_area <= 0.0 {
        return false;
    }
    let overlap_w = (a.x + a.width).min(b.x + b.width) - a.x.max(b.x);
    let overlap_h = (a.y + a.height).min(b.y + b.height) - a.y.max(b.y);
    if overlap_w <= 0.0 || overlap_h <= 0.0 {
        return false;
    }
    let overlap = overlap_w * overlap_h;
    overlap / left_area.min(right_area) > 0.8
}

fn normalize_role(role: &str) -> String {
    role.trim().to_ascii_lowercase()
}

/// Collapse whitespace and truncate on a character boundary.
fn squeeze(value: &str, limit: usize) -> String {
    let mut out = String::new();
    let mut last_space = false;
    for ch in value.chars() {
        let is_space = ch.is_whitespace();
        if is_space {
            if last_space {
                continue;
            }
            out.push(' ');
        } else {
            out.push(ch);
        }
        last_space = is_space;
        if out.chars().count() >= limit {
            out.push('…');
            break;
        }
    }
    out.trim().to_string()
}

/// Build the readable text form from the collected blocks.
fn render_text(raw: &RawAxTree) -> String {
    let mut blocks: Vec<String> = Vec::new();
    for node in &raw.nodes {
        let role = normalize_role(&node.role);
        let text = node
            .text
            .as_deref()
            .or_else(|| {
                if role == "heading" || role == "listitem" {
                    Some(node.name.as_str())
                } else {
                    None
                }
            })
            .map(str::trim)
            .filter(|text| !text.is_empty());
        let Some(text) = text else { continue };
        let text = squeeze(text, 4000);
        if text.is_empty() {
            continue;
        }
        if blocks.last().map(|last| last == &text).unwrap_or(false) {
            continue;
        }
        blocks.push(text);
    }
    blocks.join("\n\n")
}

/// Compact indented rendering with `[ref]` tags, one element per line.
pub fn render_snapshot(snapshot: &Snapshot) -> String {
    if snapshot.mode == SnapshotMode::Text {
        return snapshot.text.clone().unwrap_or_default();
    }

    let mut out = String::new();
    out.push_str(&format!(
        "page: {} | {} | {} nodes",
        if snapshot.title.is_empty() {
            "(untitled)"
        } else {
            &snapshot.title
        },
        snapshot.url,
        snapshot.nodes.len()
    ));
    if snapshot.truncated {
        out.push_str(" (truncated)");
    }
    out.push('\n');

    // Normalize depth so the shortest node starts at column zero.
    let min_depth = snapshot
        .nodes
        .iter()
        .map(|node| node.depth)
        .min()
        .unwrap_or(0);

    for node in &snapshot.nodes {
        let indent = "  ".repeat(node.depth.saturating_sub(min_depth) as usize);
        out.push_str(&format!("{indent}[{}] {}", node.id, node.role));
        if !node.name.is_empty() {
            out.push_str(&format!(" \"{}\"", node.name));
        }
        if let Some(bounds) = node.bounds {
            out.push_str(&format!(
                " @ ({:.0},{:.0} {:.0}x{:.0})",
                bounds.x, bounds.y, bounds.width, bounds.height
            ));
        }
        if let Some(value) = &node.value {
            out.push_str(&format!(" value=\"{}\"", squeeze(value, 60)));
        }
        if node.editable {
            out.push_str(" editable");
        }
        if node.disabled {
            out.push_str(" disabled");
        }
        if let Some(checked) = &node.checked {
            out.push_str(&format!(" checked={checked}"));
        }
        if let Some(expanded) = node.expanded {
            out.push_str(&format!(" expanded={expanded}"));
        }
        if node.offscreen {
            out.push_str(" offscreen");
        }
        if let Some(url) = &node.url {
            out.push_str(&format!(" -> {}", squeeze(url, 60)));
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_node(role: &str, name: &str) -> RawAxNode {
        RawAxNode {
            role: role.to_string(),
            name: name.to_string(),
            ..RawAxNode::default()
        }
    }

    fn tree(nodes: Vec<RawAxNode>) -> RawAxTree {
        RawAxTree {
            url: "https://example.test/".to_string(),
            title: "Example".to_string(),
            viewport: RawRect {
                x: 0.0,
                y: 0.0,
                width: 1280.0,
                height: 800.0,
            },
            truncated: false,
            nodes,
        }
    }

    fn bounds(x: f64, y: f64, width: f64, height: f64) -> Option<RawRect> {
        Some(RawRect {
            x,
            y,
            width,
            height,
        })
    }

    #[test]
    fn refs_are_sequential_and_start_at_one() {
        let snapshot = build_snapshot(
            &tree(vec![
                raw_node("button", "Save"),
                raw_node("link", "Docs"),
                raw_node("textbox", "Email"),
            ]),
            SnapshotMode::Interesting,
            50,
            "n1",
        );
        let ids: Vec<_> = snapshot.nodes.iter().map(|node| node.id.as_str()).collect();
        assert_eq!(ids, vec!["e1", "e2", "e3"]);
        assert_eq!(snapshot.id, "n1");
        assert_eq!(snapshot.total_nodes, 3);
        assert!(!snapshot.truncated);
    }

    #[test]
    fn interesting_mode_drops_generic_wrappers_but_keeps_actions() {
        let snapshot = build_snapshot(
            &tree(vec![
                raw_node("div", ""),
                raw_node("generic", ""),
                raw_node("button", "Submit"),
            ]),
            SnapshotMode::Interesting,
            50,
            "n1",
        );
        assert_eq!(snapshot.nodes.len(), 1);
        assert_eq!(snapshot.nodes[0].name, "Submit");
    }

    #[test]
    fn full_mode_keeps_every_named_node() {
        let snapshot = build_snapshot(
            &tree(vec![raw_node("div", "wrapper"), raw_node("button", "Go")]),
            SnapshotMode::Full,
            50,
            "n1",
        );
        assert_eq!(snapshot.nodes.len(), 2);
    }

    #[test]
    fn max_nodes_truncates_and_flags_the_snapshot() {
        let nodes = (0..10)
            .map(|index| raw_node("button", &format!("b{index}")))
            .collect();
        let snapshot = build_snapshot(&tree(nodes), SnapshotMode::Interesting, 4, "n1");
        assert_eq!(snapshot.nodes.len(), 4);
        assert!(snapshot.truncated);
        assert_eq!(snapshot.total_nodes, 10);
    }

    #[test]
    fn text_mode_returns_readable_text_without_nodes() {
        let mut paragraph = raw_node("p", "");
        paragraph.text = Some("Hello world".to_string());
        let mut heading = raw_node("heading", "Title");
        heading.text = Some("Title".to_string());
        let snapshot = build_snapshot(
            &tree(vec![heading, paragraph]),
            SnapshotMode::Text,
            50,
            "n1",
        );
        assert!(snapshot.nodes.is_empty());
        let text = snapshot.text.unwrap();
        assert!(text.contains("Title"));
        assert!(text.contains("Hello world"));
    }

    #[test]
    fn duplicate_nested_reporting_collapses_to_one_ref() {
        let mut outer = raw_node("button", "Submit");
        outer.bounds = bounds(10.0, 10.0, 100.0, 40.0);
        let mut inner = raw_node("button", "Submit");
        inner.bounds = bounds(12.0, 12.0, 96.0, 36.0);
        let snapshot = build_snapshot(&tree(vec![outer, inner]), SnapshotMode::Full, 50, "n1");
        assert_eq!(snapshot.nodes.len(), 1);
    }

    #[test]
    fn distinct_buttons_are_not_deduplicated() {
        let mut left = raw_node("button", "Submit");
        left.bounds = bounds(10.0, 10.0, 100.0, 40.0);
        let mut right = raw_node("button", "Submit");
        right.bounds = bounds(400.0, 10.0, 100.0, 40.0);
        let snapshot = build_snapshot(&tree(vec![left, right]), SnapshotMode::Full, 50, "n1");
        assert_eq!(snapshot.nodes.len(), 2);
    }

    #[test]
    fn nameless_actionable_nodes_survive_pruning() {
        let mut input = raw_node("textbox", "");
        input.editable = true;
        let snapshot = build_snapshot(&tree(vec![input]), SnapshotMode::Interesting, 50, "n1");
        assert_eq!(snapshot.nodes.len(), 1);
    }

    #[test]
    fn bounds_are_carried_over_and_zero_rects_dropped() {
        let mut button = raw_node("button", "Go");
        button.bounds = bounds(5.0, 6.0, 70.0, 30.0);
        let mut flat = raw_node("button", "Flat");
        flat.bounds = bounds(0.0, 0.0, 0.0, 0.0);
        let snapshot = build_snapshot(&tree(vec![button, flat]), SnapshotMode::Full, 50, "n1");
        assert_eq!(
            snapshot.nodes[0].bounds,
            Some(Rect::new(5.0, 6.0, 70.0, 30.0))
        );
        assert_eq!(snapshot.nodes[1].bounds, None);
    }

    #[test]
    fn viewport_is_reported_and_omitted_when_empty() {
        let snapshot = build_snapshot(&tree(vec![]), SnapshotMode::Interesting, 50, "n1");
        assert_eq!(snapshot.viewport, Some(Rect::new(0.0, 0.0, 1280.0, 800.0)));
        let mut empty = tree(vec![]);
        empty.viewport = RawRect::default();
        let snapshot = build_snapshot(&empty, SnapshotMode::Interesting, 50, "n1");
        assert!(snapshot.viewport.is_none());
    }

    #[test]
    fn render_lists_refs_roles_and_attributes() {
        let mut button = raw_node("button", "Save");
        button.bounds = bounds(12.0, 480.0, 80.0, 32.0);
        button.disabled = true;
        let mut input = raw_node("textbox", "Email");
        input.value = Some("a@b.test".to_string());
        input.editable = true;
        let mut link = raw_node("link", "Docs");
        link.url = Some("https://docs.test/guide".to_string());
        let mut checkbox = raw_node("checkbox", "Remember");
        checkbox.checked = Some("true".to_string());
        let snapshot = build_snapshot(
            &tree(vec![button, input, link, checkbox]),
            SnapshotMode::Interesting,
            50,
            "n1",
        );
        let rendered = render_snapshot(&snapshot);
        assert!(rendered.starts_with("page: Example | https://example.test/ | 4 nodes"));
        assert!(rendered.contains("[e1] button \"Save\" @ (12,480 80x32) disabled"));
        assert!(rendered.contains("[e2] textbox \"Email\" value=\"a@b.test\" editable"));
        assert!(rendered.contains("[e3] link \"Docs\" -> https://docs.test/guide"));
        assert!(rendered.contains("[e4] checkbox \"Remember\" checked=true"));
    }

    #[test]
    fn render_is_flat_when_all_nodes_share_a_depth() {
        let mut a = raw_node("button", "A");
        a.depth = 7;
        let mut b = raw_node("button", "B");
        b.depth = 7;
        let snapshot = build_snapshot(&tree(vec![a, b]), SnapshotMode::Interesting, 50, "n1");
        let rendered = render_snapshot(&snapshot);
        assert!(rendered.contains("\n[e1] button \"A\""));
        assert!(!rendered.contains("  [e1]"));
    }

    #[test]
    fn render_preserves_nesting_relative_to_the_shallowest_node() {
        let mut outer = raw_node("list", "Menu");
        outer.depth = 3;
        outer.bounds = bounds(0.0, 100.0, 200.0, 100.0);
        let mut inner = raw_node("button", "Item");
        inner.depth = 5;
        inner.bounds = bounds(10.0, 110.0, 100.0, 30.0);
        let snapshot = build_snapshot(&tree(vec![outer, inner]), SnapshotMode::Full, 50, "n1");
        let rendered = render_snapshot(&snapshot);
        assert!(rendered.contains("\n[e1] list \"Menu\""));
        assert!(rendered.contains("\n    [e2] button \"Item\""));
    }

    #[test]
    fn render_text_mode_returns_the_text_body() {
        let snapshot = Snapshot {
            id: "n1".into(),
            url: String::new(),
            title: String::new(),
            mode: SnapshotMode::Text,
            nodes: Vec::new(),
            text: Some("just text".into()),
            total_nodes: 0,
            truncated: false,
            viewport: None,
        };
        assert_eq!(render_snapshot(&snapshot), "just text");
    }

    #[test]
    fn render_truncated_snapshot_is_marked() {
        let nodes = (0..5)
            .map(|index| raw_node("button", &format!("b{index}")))
            .collect();
        let snapshot = build_snapshot(&tree(nodes), SnapshotMode::Interesting, 2, "n1");
        assert!(render_snapshot(&snapshot).contains("(truncated)"));
    }

    #[test]
    fn empty_snapshot_renders_only_the_header() {
        let snapshot = build_snapshot(&tree(vec![]), SnapshotMode::Interesting, 50, "n1");
        let rendered = render_snapshot(&snapshot);
        assert!(rendered.contains("0 nodes"));
        assert_eq!(rendered.lines().count(), 1);
    }

    #[test]
    fn whitespace_in_names_is_collapsed_and_truncated() {
        let mut button = raw_node("button", "Save   and\n\nclose");
        button.bounds = bounds(0.0, 0.0, 10.0, 10.0);
        let snapshot = build_snapshot(&tree(vec![button]), SnapshotMode::Interesting, 50, "n1");
        assert_eq!(snapshot.nodes[0].name, "Save and close");
    }

    #[test]
    fn long_names_are_truncated_with_an_ellipsis() {
        let mut button = raw_node("button", &"x".repeat(200));
        button.bounds = bounds(0.0, 0.0, 10.0, 10.0);
        let snapshot = build_snapshot(&tree(vec![button]), SnapshotMode::Interesting, 50, "n1");
        let name = &snapshot.nodes[0].name;
        assert!(name.chars().count() <= 121, "{}", name.chars().count());
        assert!(name.ends_with('…'));
    }

    #[test]
    fn raw_tree_deserializes_from_the_page_script_shape() {
        let json = r#"{
            "url": "https://example.test/",
            "title": "Example",
            "viewport": {"x": 0, "y": 0, "width": 800, "height": 600},
            "truncated": false,
            "nodes": [{
                "role": "button",
                "name": "Save",
                "value": null,
                "depth": 3,
                "bounds": {"x": 1, "y": 2, "width": 3, "height": 4},
                "editable": false,
                "focusable": true,
                "disabled": false,
                "checked": null,
                "expanded": null,
                "level": null,
                "url": null,
                "offscreen": false,
                "text": null
            }]
        }"#;
        let raw: RawAxTree = serde_json::from_str(json).unwrap();
        assert_eq!(raw.nodes.len(), 1);
        assert_eq!(raw.nodes[0].role, "button");
        assert_eq!(raw.viewport.width, 800.0);
    }

    #[test]
    fn squeeze_handles_unicode_on_char_boundaries() {
        let value = "你好 世界".to_string();
        assert_eq!(squeeze(&value, 100), "你好 世界");
        let long = "é".repeat(200);
        let squeezed = squeeze(&long, 10);
        assert_eq!(squeezed.chars().count(), 11);
    }

    #[test]
    fn offscreen_flag_survives_shaping() {
        let mut button = raw_node("button", "Below");
        button.bounds = bounds(0.0, 2000.0, 50.0, 20.0);
        button.offscreen = true;
        let snapshot = build_snapshot(&tree(vec![button]), SnapshotMode::Interesting, 50, "n1");
        assert!(snapshot.nodes[0].offscreen);
        assert!(render_snapshot(&snapshot).contains("offscreen"));
    }

    #[test]
    fn page_scripts_are_non_empty_and_wrapped_in_iife() {
        assert!(AX_TREE_JS.contains("getBoundingClientRect"));
        assert!(AX_TREE_JS.contains("aria-label"));
        assert!(AX_TREE_JS.trim_start().starts_with("(() =>"));
        assert!(PAGE_TEXT_JS.contains("innerText"));
    }
}
