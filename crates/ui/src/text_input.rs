//! Multiline text editor with IME support.
//!
//! This is a full `EntityInputHandler` implementation: selection, grapheme
//! aware cursor movement, soft wrapping, mouse selection, clipboard actions and
//! IME marked text. Enter submits; Shift+Enter inserts a newline (single-line
//! inputs treat both as submit).
//!
//! It intentionally renders its own text instead of using a platform text
//! field, because GPUI has no built-in text input widget and the composer needs
//! exact control over typography and layout.

use std::ops::Range;
use std::rc::Rc;

use gpui::{
    App, AvailableSpace, Bounds, ClipboardItem, ContentMask, Context, CursorStyle, Element,
    ElementId, ElementInputHandler, Entity, EntityInputHandler, FocusHandle, Focusable,
    GlobalElementId, Hsla, InspectorElementId, IntoElement, KeyBinding, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, Render,
    ScrollWheelEvent, SharedString, Style, Subscription, TextAlign, TextRun, UTF16Selection,
    Window, actions, div, fill, point, prelude::*, px, relative, size,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::scroll::{GlideTick, ScrollGlide};
use crate::theme::{Theme, radius};

actions!(
    effibuddy,
    [
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        SelectAll,
        Home,
        End,
        Paste,
        Cut,
        Copy,
        Submit,
        InsertNewline,
    ]
);

pub const INPUT_FONT_SIZE: f32 = 14.0;
pub const INPUT_LINE_HEIGHT: f32 = 22.0;
const INPUT_PADDING_X: f32 = 12.0;
const INPUT_PADDING_Y: f32 = 7.0;

#[derive(Clone, Debug)]
pub struct TextInputOptions {
    pub placeholder: SharedString,
    pub multiline: bool,
    pub min_lines: usize,
    pub max_lines: usize,
}

impl Default for TextInputOptions {
    fn default() -> Self {
        Self {
            placeholder: "输入消息…".into(),
            multiline: true,
            min_lines: 1,
            max_lines: 8,
        }
    }
}

impl TextInputOptions {
    pub fn placeholder(mut self, placeholder: impl Into<SharedString>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    pub fn multiline(mut self, multiline: bool) -> Self {
        self.multiline = multiline;
        self
    }

    pub fn lines(mut self, min_lines: usize, max_lines: usize) -> Self {
        self.min_lines = min_lines.max(1);
        self.max_lines = max_lines.max(self.min_lines);
        self
    }
}

#[derive(Clone, Default)]
struct InputLayout {
    lines: Vec<gpui::WrappedLine>,
    line_starts: Vec<usize>,
    visual_offsets: Vec<Pixels>,
    wrap_width: Pixels,
}

impl InputLayout {
    fn line_index_for_offset(&self, offset: usize) -> usize {
        if self.line_starts.is_empty() {
            return 0;
        }
        match self.line_starts.binary_search(&offset) {
            Ok(index) => index,
            Err(0) => 0,
            Err(index) => index - 1,
        }
    }

    /// Full content height in pixels, including every wrapped line even when
    /// the element's layout height is capped by `max_lines`.
    fn total_height(&self) -> Pixels {
        match self.lines.len() {
            0 => px(INPUT_LINE_HEIGHT),
            _ => {
                let last = self.lines.len() - 1;
                self.visual_offsets[last]
                    + px(INPUT_LINE_HEIGHT)
                        * (self.lines[last].wrap_boundaries().len() as f32 + 1.0)
            }
        }
    }

    fn point_for_offset(&self, offset: usize) -> Point<Pixels> {
        if self.lines.is_empty() {
            return point(px(0.0), px(0.0));
        }
        let line_index = self.line_index_for_offset(offset);
        let line_start = self.line_starts[line_index];
        let local = offset
            .saturating_sub(line_start)
            .min(self.lines[line_index].text.len());
        let position = self.lines[line_index]
            .position_for_index(local, px(INPUT_LINE_HEIGHT))
            .unwrap_or_else(|| point(px(0.0), px(0.0)));
        point(
            position.x,
            self.visual_offsets
                .get(line_index)
                .copied()
                .unwrap_or(px(0.0))
                + position.y,
        )
    }

    fn offset_for_point(&self, position: Point<Pixels>) -> usize {
        if self.lines.is_empty() {
            return 0;
        }
        let y = position.y.max(px(0.0));
        let mut line_index = 0;
        for (index, offset) in self.visual_offsets.iter().enumerate() {
            let height =
                px(INPUT_LINE_HEIGHT) * (self.lines[index].wrap_boundaries().len() as f32 + 1.0);
            if y < *offset + height {
                line_index = index;
                break;
            }
            line_index = index;
        }
        let local_y = (y - self.visual_offsets[line_index]).max(px(0.0));
        let local_x = position.x.max(px(0.0)).min(self.wrap_width);
        let local = point(local_x, local_y);
        let index = match self.lines[line_index].index_for_position(local, px(INPUT_LINE_HEIGHT)) {
            Ok(index) | Err(index) => index,
        };
        self.line_starts[line_index] + index.min(self.lines[line_index].text.len())
    }

    fn selection_quads(
        &self,
        range: Range<usize>,
        color: Hsla,
        origin: Point<Pixels>,
    ) -> Vec<PaintQuad> {
        let mut quads = Vec::new();
        if range.start >= range.end {
            return quads;
        }
        for (line_index, line) in self.lines.iter().enumerate() {
            let line_start = self.line_starts[line_index];
            let line_end = line_start + line.text.len();
            if line_end < range.start || line_start >= range.end {
                continue;
            }

            let mut segment_starts = vec![0usize];
            for boundary in line.wrap_boundaries() {
                if let Some(run) = line.runs().get(boundary.run_ix) {
                    if let Some(glyph) = run.glyphs.get(boundary.glyph_ix) {
                        segment_starts.push(glyph.index.min(line.text.len()));
                    }
                }
            }
            segment_starts.sort_unstable();
            segment_starts.dedup();

            for (visual_index, segment_start) in segment_starts.iter().enumerate() {
                let segment_end = segment_starts
                    .get(visual_index + 1)
                    .copied()
                    .unwrap_or(line.text.len());
                let absolute_start = line_start + segment_start;
                let absolute_end = line_start + segment_end;
                let selection_start = range.start.max(absolute_start);
                let selection_end = range.end.min(absolute_end);
                if selection_start >= selection_end {
                    continue;
                }

                let segment_position = line
                    .position_for_index(*segment_start, px(INPUT_LINE_HEIGHT))
                    .unwrap_or_else(|| point(px(0.0), px(0.0)));
                let start_position = line
                    .position_for_index(selection_start - line_start, px(INPUT_LINE_HEIGHT))
                    .unwrap_or_else(|| point(px(0.0), px(0.0)));
                let end_position = line
                    .position_for_index(selection_end - line_start, px(INPUT_LINE_HEIGHT))
                    .unwrap_or_else(|| point(px(0.0), px(0.0)));

                let x1 = (start_position.x - segment_position.x)
                    .max(px(0.0))
                    .min(self.wrap_width);
                let x2 = (end_position.x - segment_position.x)
                    .max(px(0.0))
                    .min(self.wrap_width);
                if x2 <= x1 {
                    continue;
                }
                let y =
                    self.visual_offsets[line_index] + px(INPUT_LINE_HEIGHT) * visual_index as f32;
                quads.push(fill(
                    Bounds::new(
                        point(origin.x + x1, origin.y + y),
                        size(x2 - x1, px(INPUT_LINE_HEIGHT)),
                    ),
                    color,
                ));
            }
        }
        quads
    }
}

pub struct TextInputState {
    focus_handle: FocusHandle,
    content: String,
    placeholder: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_layout: Option<InputLayout>,
    last_bounds: Option<Bounds<Pixels>>,
    is_selecting: bool,
    multiline: bool,
    min_lines: usize,
    max_lines: usize,
    /// Vertical scroll applied when the content exceeds `max_lines`.
    scroll_top: Pixels,
    /// Exponential glide that eases wheel scrolling once the editor is capped.
    glide: ScrollGlide,
    /// Guards against scheduling more than one glide frame at a time.
    glide_frame_scheduled: bool,
    /// Set whenever the caret or content moves so the next layout pass can
    /// scroll the caret back into view.
    pending_scroll_to_caret: bool,
    on_submit: Option<Rc<dyn Fn(String, &mut Window, &mut App)>>,
}

impl TextInputState {
    pub fn new(cx: &mut Context<Self>, options: TextInputOptions) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: String::new(),
            placeholder: options.placeholder,
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            is_selecting: false,
            multiline: options.multiline,
            min_lines: options.min_lines.max(1),
            max_lines: options.max_lines.max(options.min_lines.max(1)),
            scroll_top: px(0.0),
            glide: ScrollGlide::new(),
            glide_frame_scheduled: false,
            pending_scroll_to_caret: false,
            on_submit: None,
        }
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    /// Seed content before the view is first rendered (no window needed).
    pub fn with_content(mut self, content: impl Into<String>) -> Self {
        let mut content = content.into();
        if !self.multiline {
            content = sanitize_single_line(&content);
        }
        self.selected_range = content.len()..content.len();
        self.pending_scroll_to_caret = true;
        self.content = content;
        self
    }

    pub fn placeholder(&self) -> &SharedString {
        &self.placeholder
    }

    pub fn multiline(&self) -> bool {
        self.multiline
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    pub fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    pub fn selected_range(&self) -> Range<usize> {
        self.selected_range.clone()
    }

    pub fn set_content(
        &mut self,
        content: impl Into<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut content = content.into();
        if !self.multiline {
            content = sanitize_single_line(&content);
        }
        self.content = content;
        self.selected_range = self.content.len()..self.content.len();
        self.selection_reversed = false;
        self.marked_range = None;
        self.pending_scroll_to_caret = true;
        self.notify_change(window, cx);
        cx.notify();
    }

    /// Replace the content without a window. Used when a parent form is
    /// rehydrated from persisted state; the parent is responsible for
    /// notifying its own view.
    pub fn set_content_silent(&mut self, content: impl Into<String>) {
        let mut content = content.into();
        if !self.multiline {
            content = sanitize_single_line(&content);
        }
        self.content = content;
        self.selected_range = self.content.len()..self.content.len();
        self.selection_reversed = false;
        self.marked_range = None;
        self.pending_scroll_to_caret = true;
    }

    pub fn clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.content.is_empty() {
            return;
        }
        self.content.clear();
        self.selected_range = 0..0;
        self.selection_reversed = false;
        self.marked_range = None;
        self.scroll_top = px(0.0);
        self.pending_scroll_to_caret = false;
        self.notify_change(window, cx);
        cx.notify();
    }

    fn notify_change(&self, _window: &mut Window, _cx: &mut App) {
        // Auto-resize and parent affordances are handled by the view observing
        // this entity; nothing else to do yet.
    }

    /// Feed one wheel event into the smooth-scroll glide. Returns false when
    /// the content fits (the event should bubble to the surrounding page).
    fn wheel_scroll(
        &mut self,
        delta_y: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let (Some(bounds), Some(layout)) = (self.last_bounds.as_ref(), self.last_layout.as_ref())
        else {
            return false;
        };
        let max_scroll = (layout.total_height() - bounds.size.height).max(px(0.0));
        if max_scroll <= px(0.0) {
            return false;
        }
        self.pending_scroll_to_caret = false;
        let moved = self.glide.wheel(
            f32::from(delta_y),
            f32::from(self.scroll_top),
            f32::from(max_scroll),
        );
        if moved {
            self.schedule_glide_frame(window, cx);
        }
        moved
    }

    fn schedule_glide_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.glide_frame_scheduled {
            return;
        }
        self.glide_frame_scheduled = true;
        cx.on_next_frame(window, |state, window, cx| {
            state.glide_frame_scheduled = false;
            state.tick_scroll_glide(window, cx);
            cx.notify();
        });
    }

    fn tick_scroll_glide(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(bounds), Some(layout)) = (self.last_bounds.as_ref(), self.last_layout.as_ref())
        else {
            self.glide.cancel();
            return;
        };
        let max_scroll = (layout.total_height() - bounds.size.height).max(px(0.0));
        if max_scroll <= px(0.0) {
            self.glide.cancel();
            return;
        }
        let max = f32::from(max_scroll);
        let offset = f32::from(self.scroll_top).clamp(0.0, max);
        match self.glide.tick(offset, max) {
            GlideTick::Continue => self.schedule_glide_frame(window, cx),
            GlideTick::Step(delta) => {
                self.scroll_top = px((offset + delta).clamp(0.0, max));
                self.schedule_glide_frame(window, cx);
            }
            GlideTick::Done { delta, .. } => {
                self.scroll_top = px((offset + delta).clamp(0.0, max));
            }
            GlideTick::Cancelled => {}
        }
    }

    fn sanitize(&self, text: &str) -> String {
        if self.multiline {
            text.replace('\r', "")
        } else {
            sanitize_single_line(text)
        }
    }

    fn floor_boundary(&self, offset: usize) -> usize {
        let mut offset = offset.min(self.content.len());
        while offset > 0 && !self.content.is_char_boundary(offset) {
            offset -= 1;
        }
        offset
    }

    fn clamp_range(&self, range: Range<usize>) -> Range<usize> {
        let start = self.floor_boundary(range.start);
        let end = self.floor_boundary(range.end.max(start));
        start..end
    }

    fn replace_range(
        &mut self,
        range: Range<usize>,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = self.clamp_range(range);
        if range.start > range.end || range.end > self.content.len() {
            return;
        }
        let new_text = self.sanitize(new_text);
        self.content.replace_range(range.clone(), &new_text);
        let caret = range.start + new_text.len();
        self.selected_range = caret..caret;
        self.selection_reversed = false;
        self.marked_range = None;
        self.pending_scroll_to_caret = true;
        self.notify_change(window, cx);
        cx.notify();
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = self.floor_boundary(offset);
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        self.pending_scroll_to_caret = true;
        cx.notify();
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = self.floor_boundary(offset);
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        self.pending_scroll_to_caret = true;
        cx.notify();
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(index, _)| (index < offset).then_some(index))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(index, _)| (index > offset).then_some(index))
            .unwrap_or(self.content.len())
    }

    fn line_start(&self, offset: usize) -> usize {
        self.content[..offset]
            .rfind('\n')
            .map(|index| index + 1)
            .unwrap_or(0)
    }

    fn line_end(&self, offset: usize) -> usize {
        self.content[offset..]
            .find('\n')
            .map(|index| offset + index)
            .unwrap_or(self.content.len())
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        let (Some(bounds), Some(layout)) = (&self.last_bounds, &self.last_layout) else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }
        // Mouse positions are in view space; convert to content space by
        // adding the scroll offset.
        layout.offset_for_point(point(
            position.x - bounds.left(),
            position.y - bounds.top() + self.scroll_top,
        ))
    }

    fn vertical_move(&mut self, direction: i32, select: bool, cx: &mut Context<Self>) {
        let Some(layout) = self.last_layout.clone() else {
            return;
        };
        let current = layout.point_for_offset(self.cursor_offset());
        let target_y = current.y + px(INPUT_LINE_HEIGHT) * direction as f32;
        let target = layout.offset_for_point(point(current.x, target_y));
        if select {
            self.select_to(target, cx);
        } else {
            self.move_to(target, cx);
        }
    }

    // --- actions ---------------------------------------------------------

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let start = self.previous_boundary(self.cursor_offset());
            self.select_to(start, cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let end = self.next_boundary(self.cursor_offset());
            self.select_to(end, cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else if self.selection_reversed {
            self.move_to(self.selected_range.start, cx);
        } else {
            self.move_to(self.selected_range.end, cx);
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.cursor_offset()), cx);
        } else if self.selection_reversed {
            self.move_to(self.selected_range.start, cx);
        } else {
            self.move_to(self.selected_range.end, cx);
        }
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical_move(-1, false, cx);
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical_move(1, false, cx);
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical_move(-1, true, cx);
    }

    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical_move(1, true, cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.selected_range = 0..self.content.len();
        self.selection_reversed = false;
        self.pending_scroll_to_caret = true;
        cx.notify();
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        let offset = self.line_start(self.cursor_offset());
        self.move_to(offset, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        let offset = self.line_end(self.cursor_offset());
        self.move_to(offset, cx);
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace_text_in_range(None, &text, window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            self.replace_text_in_range(None, "", window, cx);
        }
    }

    fn submit(&mut self, _: &Submit, window: &mut Window, cx: &mut Context<Self>) {
        self.finish_submit(window, cx);
    }

    fn finish_submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(handler) = self.on_submit.clone() else {
            return;
        };
        let text = std::mem::take(&mut self.content);
        self.selected_range = 0..0;
        self.selection_reversed = false;
        self.marked_range = None;
        self.last_layout = None;
        self.scroll_top = px(0.0);
        self.pending_scroll_to_caret = false;
        cx.notify();
        handler(text, window, cx);
    }

    fn insert_newline(&mut self, _: &InsertNewline, window: &mut Window, cx: &mut Context<Self>) {
        if self.multiline {
            self.replace_text_in_range(None, "\n", window, cx);
        } else {
            self.finish_submit(window, cx);
        }
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.is_selecting = true;
        window.focus(&self.focus_handle);
        let offset = self.index_for_mouse_position(event.position);
        if event.modifiers.shift {
            self.select_to(offset, cx);
        } else {
            self.move_to(offset, cx);
        }
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            let offset = self.index_for_mouse_position(event.position);
            self.select_to(offset, cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    // --- UTF-16 helpers --------------------------------------------------

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }
        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }
        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }
}

fn sanitize_single_line(text: &str) -> String {
    text.chars()
        .map(|ch| if ch == '\r' || ch == '\n' { ' ' } else { ch })
        .collect()
}

impl Focusable for TextInputState {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EntityInputHandler for TextInputState {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        let range = self.clamp_range(range);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range| self.range_from_utf16(range))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone());
        let range = self.clamp_range(range);
        self.replace_range(range, new_text, window, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range| self.range_from_utf16(range))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone());
        let range = self.clamp_range(range);
        let new_text = self.sanitize(new_text);
        self.content.replace_range(range.clone(), &new_text);
        let insert_end = range.start + new_text.len();
        if new_text.is_empty() {
            self.marked_range = None;
        } else {
            self.marked_range = Some(range.start..insert_end);
        }
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|range| self.range_from_utf16(range))
            .map(|new_range| range.start + new_range.start..range.start + new_range.end)
            .unwrap_or(insert_end..insert_end);
        self.selection_reversed = false;
        self.pending_scroll_to_caret = true;
        self.notify_change(window, cx);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        let start = layout.point_for_offset(range.start);
        let end = layout.point_for_offset(range.end);
        Some(Bounds::from_corners(
            point(
                bounds.left() + start.x,
                bounds.top() + start.y - self.scroll_top,
            ),
            point(
                bounds.left() + end.x,
                bounds.top() + end.y + px(INPUT_LINE_HEIGHT) - self.scroll_top,
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        position: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let bounds = self.last_bounds?;
        let layout = self.last_layout.as_ref()?;
        let local = bounds.localize(&position)?;
        Some(
            self.offset_to_utf16(
                layout.offset_for_point(point(local.x, local.y + self.scroll_top)),
            ),
        )
    }
}

struct TextElement {
    input: Entity<TextInputState>,
}

struct PrepaintState {
    layout: InputLayout,
    selection_quads: Vec<PaintQuad>,
    cursor: Option<PaintQuad>,
    scroll: Pixels,
}

impl IntoElement for TextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        _cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let input = self.input.clone();
        let min_height = px(INPUT_LINE_HEIGHT) * 1.0;
        let mut style = Style::default();
        style.size.width = relative(1.0).into();

        let layout_id =
            window.request_measured_layout(style, move |known, available, window, cx| {
                let width = known
                    .width
                    .or_else(|| match available.width {
                        AvailableSpace::Definite(width) => Some(width),
                        _ => None,
                    })
                    .unwrap_or(px(560.0));
                let state = input.read(cx);
                let text = if state.content.is_empty() {
                    state.placeholder.to_string()
                } else {
                    state.content.clone()
                };
                let lines = shape_text(
                    &text,
                    width,
                    &Theme::get(cx),
                    if state.content.is_empty() {
                        Theme::get(cx).text_faint
                    } else {
                        Theme::get(cx).text
                    },
                    window,
                );
                let height = lines
                    .map(|lines| visual_line_count(&lines))
                    .unwrap_or(1)
                    .max(state.min_lines)
                    .min(state.max_lines) as f32
                    * INPUT_LINE_HEIGHT;
                size(width, px(height).max(min_height))
            });
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let theme = Theme::get(cx).clone();
        let (
            is_empty,
            text,
            color,
            selected_range,
            cursor_offset,
            pending_scroll,
            focused,
            mut scroll_top,
        ) = {
            let input = self.input.read(cx);
            let is_empty = input.content.is_empty();
            (
                is_empty,
                if is_empty {
                    input.placeholder.to_string()
                } else {
                    input.content.clone()
                },
                if is_empty {
                    theme.text_faint
                } else {
                    theme.text
                },
                input.selected_range.clone(),
                input.cursor_offset(),
                input.pending_scroll_to_caret,
                input.focus_handle.is_focused(window),
                input.scroll_top,
            )
        };
        let lines = shape_text(&text, bounds.size.width, &theme, color, window).unwrap_or_default();
        let line_starts = line_starts(&lines);
        let mut visual_offsets = Vec::with_capacity(lines.len());
        let mut y = px(0.0);
        for line in &lines {
            visual_offsets.push(y);
            y += px(INPUT_LINE_HEIGHT) * (line.wrap_boundaries().len() as f32 + 1.0);
        }
        let layout = InputLayout {
            lines,
            line_starts,
            visual_offsets,
            wrap_width: bounds.size.width,
        };

        // Clamp the scroll to the amount of hidden content, then honor a
        // pending scroll-to-caret request so typing and cursor moves keep the
        // caret visible once the editor is capped at `max_lines`.
        let max_scroll = (layout.total_height() - bounds.size.height).max(px(0.0));
        if pending_scroll {
            let caret_y = layout.point_for_offset(cursor_offset).y;
            let caret_bottom = caret_y + px(INPUT_LINE_HEIGHT);
            if caret_y < scroll_top {
                scroll_top = caret_y;
            } else if caret_bottom > scroll_top + bounds.size.height {
                scroll_top = caret_bottom - bounds.size.height;
            }
        }
        scroll_top = scroll_top.max(px(0.0)).min(max_scroll);
        self.input.update(cx, |state, _| {
            state.scroll_top = scroll_top;
            state.pending_scroll_to_caret = false;
        });

        let selection_quads = if is_empty {
            Vec::new()
        } else {
            layout.selection_quads(
                selected_range,
                theme.selection,
                point(bounds.origin.x, bounds.origin.y - scroll_top),
            )
        };
        let cursor = if focused {
            let position = layout.point_for_offset(cursor_offset);
            Some(fill(
                Bounds::new(
                    point(
                        bounds.left() + position.x,
                        bounds.top() + position.y - scroll_top,
                    ),
                    size(px(2.0), px(INPUT_LINE_HEIGHT)),
                ),
                theme.accent,
            ))
        } else {
            None
        };

        PrepaintState {
            layout,
            selection_quads,
            cursor,
            scroll: scroll_top,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );

        let scroll = prepaint.scroll;
        let layout = prepaint.layout.clone();
        let selection_quads = prepaint.selection_quads.drain(..).collect::<Vec<_>>();
        let mut cursor = prepaint.cursor.take();

        // Clip everything to the element bounds: the layout height is capped
        // at `max_lines`, so overflowing content must be scrolled and masked,
        // never painted past the editor (and its surrounding card).
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for quad in selection_quads {
                window.paint_quad(quad);
            }

            let mut y = bounds.top() - scroll;
            for line in &layout.lines {
                let height = px(INPUT_LINE_HEIGHT) * (line.wrap_boundaries().len() as f32 + 1.0);
                if y + height >= bounds.top() && y <= bounds.bottom() {
                    line.paint(
                        point(bounds.left(), y),
                        px(INPUT_LINE_HEIGHT),
                        TextAlign::Left,
                        Some(bounds),
                        window,
                        cx,
                    )
                    .ok();
                }
                y += height;
            }

            if let Some(cursor) = cursor.take() {
                window.paint_quad(cursor);
            }
        });

        self.input.update(cx, |input, _cx| {
            input.last_layout = Some(layout);
            input.last_bounds = Some(bounds);
        });
    }
}

fn shape_text(
    text: &str,
    width: Pixels,
    theme: &Theme,
    color: Hsla,
    window: &Window,
) -> Option<Vec<gpui::WrappedLine>> {
    if text.is_empty() {
        return Some(vec![gpui::WrappedLine::default()]);
    }
    let run = TextRun {
        len: text.len(),
        font: gpui::font(theme.ui_font()),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    window
        .text_system()
        .shape_text(
            text.to_string().into(),
            px(INPUT_FONT_SIZE),
            &[run],
            Some(width.max(px(16.0))),
            None,
        )
        .ok()
        .map(|lines| lines.into_iter().collect())
}

fn visual_line_count(lines: &[gpui::WrappedLine]) -> usize {
    lines
        .iter()
        .map(|line| line.wrap_boundaries().len() + 1)
        .sum()
}

fn line_starts(lines: &[gpui::WrappedLine]) -> Vec<usize> {
    let mut starts = Vec::with_capacity(lines.len());
    let mut offset = 0;
    for line in lines {
        starts.push(offset);
        offset += line.text.len() + 1;
    }
    starts
}

pub struct TextInputView {
    state: Entity<TextInputState>,
    on_submit: Option<Rc<dyn Fn(String, &mut Window, &mut App)>>,
    frameless: bool,
    _subscription: Option<Subscription>,
    _theme_subscription: Subscription,
}

impl TextInputView {
    pub fn new(state: Entity<TextInputState>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&state, |_, _, cx| cx.notify());
        let theme_subscription = cx.observe_global::<Theme>(|_, cx| cx.notify());
        Self {
            state,
            on_submit: None,
            frameless: false,
            _subscription: Some(subscription),
            _theme_subscription: theme_subscription,
        }
    }

    /// Remove the field's own background, border, radius and padding so the
    /// caller (for example a floating composer card) owns the chrome.
    pub fn frameless(mut self, frameless: bool) -> Self {
        self.frameless = frameless;
        self
    }

    pub fn on_submit(mut self, handler: impl Fn(String, &mut Window, &mut App) + 'static) -> Self {
        self.on_submit = Some(Rc::new(handler));
        self
    }

    pub fn state(&self) -> Entity<TextInputState> {
        self.state.clone()
    }
}

impl Render for TextInputView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let state = self.state.clone();
        let on_submit = self.on_submit.clone();
        state.update(cx, |state, _cx| {
            state.on_submit = on_submit;
        });

        let focus_handle = state.read(cx).focus_handle.clone();
        let focused = focus_handle.is_focused(window);
        let multiline = state.read(cx).multiline;
        let min_lines = state.read(cx).min_lines;
        let max_lines = state.read(cx).max_lines;
        let entity_id = state.entity_id();

        let mut element = if self.frameless {
            div()
                .id(ElementId::View(entity_id))
                .flex()
                .flex_col()
                .w_full()
                .cursor(CursorStyle::IBeam)
                .key_context("EffiTextInput")
                .track_focus(&focus_handle)
                .child(TextElement {
                    input: state.clone(),
                })
        } else {
            div()
                .id(ElementId::View(entity_id))
                .flex()
                .flex_col()
                .w_full()
                .min_h(px(36.0))
                .rounded(px(radius::MD))
                .border_1()
                .border_color(if focused {
                    theme.focus_ring
                } else {
                    theme.border
                })
                .bg(theme.surface_alt)
                .px(px(INPUT_PADDING_X))
                .py(px(INPUT_PADDING_Y))
                .overflow_hidden()
                .cursor(CursorStyle::IBeam)
                .key_context("EffiTextInput")
                .track_focus(&focus_handle)
                .child(TextElement {
                    input: state.clone(),
                })
        };

        element = bind_actions(element, &state);

        let state_down = state.clone();
        element = element.on_mouse_down(MouseButton::Left, move |event, window, cx| {
            state_down.update(cx, |state, cx| {
                window.focus(&state.focus_handle);
                state.on_mouse_down(event, window, cx);
            });
        });

        let state_move = state.clone();
        element = element.on_mouse_move(move |event, window, cx| {
            state_move.update(cx, |state, cx| state.on_mouse_move(event, window, cx));
        });

        let state_up = state.clone();
        element = element.on_mouse_up(MouseButton::Left, move |event, window, cx| {
            state_up.update(cx, |state, cx| state.on_mouse_up(event, window, cx));
        });

        let state_up_out = state.clone();
        element = element.on_mouse_up_out(MouseButton::Left, move |event, window, cx| {
            state_up_out.update(cx, |state, cx| state.on_mouse_up(event, window, cx));
        });

        // Wheel scrolling once the content is capped by `max_lines`. It eases
        // through the same exponential glide as the transcript; the event is
        // consumed only when the editor actually scrolls, so the wheel still
        // reaches surrounding scroll containers otherwise.
        let state_wheel = state.clone();
        element = element.on_scroll_wheel(move |event: &ScrollWheelEvent, window, cx| {
            state_wheel.update(cx, |state, cx| {
                let delta_y = event.delta.pixel_delta(px(INPUT_LINE_HEIGHT)).y;
                if delta_y == px(0.0) {
                    return;
                }
                if state.wheel_scroll(delta_y, window, cx) {
                    cx.stop_propagation();
                    cx.notify();
                }
            });
        });

        let _ = (multiline, min_lines, max_lines);
        element
    }
}

fn bind_actions(
    mut element: gpui::Stateful<gpui::Div>,
    state: &Entity<TextInputState>,
) -> gpui::Stateful<gpui::Div> {
    macro_rules! bind {
        ($element:expr, $action:ty, $method:ident) => {{
            let state = state.clone();
            $element.on_action(move |action: &$action, window: &mut Window, cx: &mut App| {
                state.update(cx, |input, cx| input.$method(action, window, cx));
            })
        }};
    }

    element = bind!(element, Backspace, backspace);
    element = bind!(element, Delete, delete);
    element = bind!(element, Left, left);
    element = bind!(element, Right, right);
    element = bind!(element, Up, up);
    element = bind!(element, Down, down);
    element = bind!(element, SelectLeft, select_left);
    element = bind!(element, SelectRight, select_right);
    element = bind!(element, SelectUp, select_up);
    element = bind!(element, SelectDown, select_down);
    element = bind!(element, SelectAll, select_all);
    element = bind!(element, Home, home);
    element = bind!(element, End, end);
    element = bind!(element, Paste, paste);
    element = bind!(element, Cut, cut);
    element = bind!(element, Copy, copy);
    element = bind!(element, Submit, submit);
    element = bind!(element, InsertNewline, insert_newline);
    element
}

/// Register editor keybindings. Call once during app startup.
pub fn install_keybindings(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, Some("EffiTextInput")),
        KeyBinding::new("delete", Delete, Some("EffiTextInput")),
        KeyBinding::new("left", Left, Some("EffiTextInput")),
        KeyBinding::new("right", Right, Some("EffiTextInput")),
        KeyBinding::new("up", Up, Some("EffiTextInput")),
        KeyBinding::new("down", Down, Some("EffiTextInput")),
        KeyBinding::new("shift-left", SelectLeft, Some("EffiTextInput")),
        KeyBinding::new("shift-right", SelectRight, Some("EffiTextInput")),
        KeyBinding::new("shift-up", SelectUp, Some("EffiTextInput")),
        KeyBinding::new("shift-down", SelectDown, Some("EffiTextInput")),
        KeyBinding::new("ctrl-a", SelectAll, Some("EffiTextInput")),
        KeyBinding::new("cmd-a", SelectAll, Some("EffiTextInput")),
        KeyBinding::new("ctrl-c", Copy, Some("EffiTextInput")),
        KeyBinding::new("cmd-c", Copy, Some("EffiTextInput")),
        KeyBinding::new("ctrl-x", Cut, Some("EffiTextInput")),
        KeyBinding::new("cmd-x", Cut, Some("EffiTextInput")),
        KeyBinding::new("ctrl-v", Paste, Some("EffiTextInput")),
        KeyBinding::new("cmd-v", Paste, Some("EffiTextInput")),
        KeyBinding::new("home", Home, Some("EffiTextInput")),
        KeyBinding::new("end", End, Some("EffiTextInput")),
        KeyBinding::new("enter", Submit, Some("EffiTextInput")),
        KeyBinding::new("shift-enter", InsertNewline, Some("EffiTextInput")),
    ]);
}
