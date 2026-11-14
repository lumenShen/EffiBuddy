//! Real table rendering for Markdown tables.
//!
//! Assistant messages frequently carry comparison/statistics tables. Instead
//! of flattening them into a pseudo code block, the Markdown renderer collects
//! [`TableData`] from the parser events and hands it to [`render_table`],
//! which draws a column-aligned table (header row, 1px grid lines, striped
//! rows, horizontal scrolling) with a hover toolbar that copies the table as
//! Markdown or CSV. CSV export guards against formula injection the same way
//! ZCode's `buildCsvTableText` does: cells starting with `=`, `+`, `-` or `@`
//! are prefixed with an apostrophe so spreadsheets treat them as text.

use std::hash::{Hash, Hasher};

use gpui::{
    AnyElement, ClipboardItem, FontWeight, IntoElement, TextAlign, div, linear_color_stop,
    linear_gradient, prelude::*, px, svg,
};
use pulldown_cmark::{Alignment, Event, Tag, TagEnd};
use unicode_width::UnicodeWidthStr;

use crate::assets::icons;
use crate::theme::{Theme, radius, text as text_size};

/// Beyond this many columns per-cell measurement stops helping (columns get
/// too narrow to read), so the table falls back to equal column weights.
pub const MAX_COLUMNS: usize = 8;
/// Display-width units a single column may occupy before it is capped
/// (oversized cells ellipsize instead of dominating the layout).
const MAX_COLUMN_UNITS: usize = 40;
/// Floor for a measured column, so tiny cells still get readable width.
const MIN_COLUMN_UNITS: usize = 6;
/// Pixels per display-width unit for the minimum column width.
const CHAR_UNIT_PX: f32 = 7.5;
/// Absolute floor for a column's minimum width, in pixels.
const MIN_COLUMN_PX: f32 = 56.0;
/// Show the right-edge scroll fade once the table is wider than this.
/// The actual container width is unknown while building the element, so this
/// is a best-effort hint rather than an overflow measurement.
const SCROLL_HINT_THRESHOLD_PX: f32 = 640.0;

/// Structural data for one Markdown table: header cells, body rows and the
/// per-column text alignment declared by the separator row.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TableData {
    pub alignments: Vec<Alignment>,
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

impl TableData {
    /// Column count across header and rows; ragged tables report the widest.
    pub fn column_count(&self) -> usize {
        self.headers
            .len()
            .max(self.rows.iter().map(Vec::len).max().unwrap_or(0))
    }

    /// True when the table carries neither headers nor rows.
    pub fn is_empty(&self) -> bool {
        self.headers.iter().all(String::is_empty) && self.rows.is_empty()
    }

    /// Pads/truncates headers and every row to [`Self::column_count`] so
    /// rendering and serialization never index out of bounds.
    pub fn normalized(self) -> Self {
        let columns = self.column_count();
        let mut normalized = self;
        normalized.headers.resize(columns, String::new());
        for row in &mut normalized.rows {
            row.resize(columns, String::new());
        }
        normalized
    }
}

/// Collects parser events between `Start(Table)` and `End(Table)` into a
/// [`TableData`]. Cell contents are flattened to plain text: inline text and
/// code are kept verbatim, soft/hard breaks become spaces, `<br>` becomes a
/// newline, and other inline events (emphasis wrappers, math) are dropped.
pub(crate) struct TableCollector {
    alignments: Vec<Alignment>,
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
    in_head: bool,
    row: Vec<String>,
    cell: String,
}

impl TableCollector {
    pub(crate) fn new(alignments: Vec<Alignment>) -> Self {
        Self {
            alignments,
            headers: Vec::new(),
            rows: Vec::new(),
            in_head: false,
            row: Vec::new(),
            cell: String::new(),
        }
    }

    /// Feeds one parser event. Returns the finished [`TableData`] exactly
    /// once, when the closing `End(Table)` arrives.
    pub(crate) fn handle_event(&mut self, event: Event<'_>) -> Option<TableData> {
        match event {
            Event::Start(Tag::TableHead) => {
                self.in_head = true;
                self.row = Vec::new();
            }
            Event::Start(Tag::TableRow) => {
                self.in_head = false;
                self.row = Vec::new();
            }
            Event::Start(Tag::TableCell) => self.cell = String::new(),
            Event::Text(text) | Event::Code(text) => self.cell.push_str(&text),
            Event::SoftBreak | Event::HardBreak => self.cell.push(' '),
            // GFM cells are single-line, so authored line breaks arrive as
            // inline `<br>`; keep them as newlines so `table_to_markdown`
            // can round-trip the cell losslessly.
            Event::InlineHtml(html) | Event::Html(html) if html.contains("<br") => {
                self.cell.push('\n');
            }
            Event::End(TagEnd::TableCell) => self.row.push(std::mem::take(&mut self.cell)),
            Event::End(TagEnd::TableRow) => {
                let row = std::mem::take(&mut self.row);
                if self.in_head {
                    self.headers = row;
                    self.in_head = false;
                } else {
                    self.rows.push(row);
                }
            }
            Event::End(TagEnd::TableHead) => {
                self.headers = std::mem::take(&mut self.row);
                self.in_head = false;
            }
            Event::End(TagEnd::Table) => return Some(self.finish()),
            _ => {}
        }
        None
    }

    fn finish(&mut self) -> TableData {
        let mut data = TableData {
            alignments: std::mem::take(&mut self.alignments),
            headers: std::mem::take(&mut self.headers),
            rows: std::mem::take(&mut self.rows),
        };
        if data.headers.is_empty() && !data.rows.is_empty() {
            // Degenerate table without a header row: promote the first row.
            data.headers = data.rows.remove(0);
        }
        data.normalized()
    }
}

/// Per-column flex weights in display-width units (CJK characters count
/// double). Oversized cells are capped, tiny cells get a floor, and tables
/// wider than [`MAX_COLUMNS`] fall back to equal weights.
pub fn measure_columns(data: &TableData) -> Vec<f32> {
    let columns = data.column_count();
    if columns == 0 {
        return Vec::new();
    }
    if columns > MAX_COLUMNS {
        return vec![1.0; columns];
    }
    let mut units = vec![MIN_COLUMN_UNITS as f32; columns];
    let mut consider = |index: usize, cell: &str| {
        if index < columns {
            let width = cell.trim().width().min(MAX_COLUMN_UNITS) as f32;
            units[index] = units[index].max(width);
        }
    };
    for (index, header) in data.headers.iter().enumerate() {
        consider(index, header);
    }
    for row in &data.rows {
        for (index, cell) in row.iter().enumerate() {
            consider(index, cell);
        }
    }
    units
}

/// Serializes the table back into GitHub-Flavored Markdown. Pipes and newlines
/// inside cells are escaped so the text round-trips through the parser.
pub fn table_to_markdown(data: &TableData) -> String {
    let columns = data.column_count();
    if columns == 0 {
        return String::new();
    }
    let mut out = String::new();
    push_markdown_row(&mut out, &data.headers, columns);
    let separator: Vec<String> = (0..columns)
        .map(|index| match data.alignments.get(index).copied() {
            Some(Alignment::Left) => ":---",
            Some(Alignment::Center) => ":---:",
            Some(Alignment::Right) => "---:",
            _ => "---",
        })
        .map(str::to_string)
        .collect();
    push_markdown_row(&mut out, &separator, columns);
    for row in &data.rows {
        push_markdown_row(&mut out, row, columns);
    }
    out
}

fn push_markdown_row(out: &mut String, cells: &[String], columns: usize) {
    out.push('|');
    for index in 0..columns {
        let cell = cells.get(index).map(String::as_str).unwrap_or("");
        out.push(' ');
        out.push_str(&markdown_cell(cell));
        out.push_str(" |");
    }
    out.push('\n');
}

fn markdown_cell(cell: &str) -> String {
    cell.replace('\r', "")
        .replace('|', "\\|")
        .replace('\n', "<br>")
}

/// Serializes the table as CSV (comma-separated, RFC-4180 quoting). Cells
/// starting with `=`, `+`, `-` or `@` get a leading apostrophe so spreadsheet
/// applications do not interpret them as formulas.
pub fn table_to_csv(data: &TableData) -> String {
    let columns = data.column_count();
    if columns == 0 {
        return String::new();
    }
    let mut out = String::new();
    push_csv_row(&mut out, &data.headers, columns);
    for row in &data.rows {
        push_csv_row(&mut out, row, columns);
    }
    out
}

fn push_csv_row(out: &mut String, cells: &[String], columns: usize) {
    let fields: Vec<String> = (0..columns)
        .map(|index| csv_field(cells.get(index).map(String::as_str).unwrap_or("")))
        .collect();
    out.push_str(&fields.join(","));
    out.push('\n');
}

fn csv_field(cell: &str) -> String {
    let guarded = match cell.chars().next() {
        Some('=' | '+' | '-' | '@') => format!("'{cell}"),
        _ => cell.to_string(),
    };
    if guarded.contains(&[',', '"', '\n', '\r'][..]) {
        format!("\"{}\"", guarded.replace('"', "\"\""))
    } else {
        guarded
    }
}

/// Renders the table as a column-aligned element: bold header, 1px grid lines,
/// striped body rows, horizontal scrolling for wide tables and a right-edge
/// fade hinting that more columns are available.
pub fn render_table(theme: &Theme, data: &TableData) -> AnyElement {
    if data.is_empty() {
        return div().into_any_element();
    }
    let units = measure_columns(data);
    let widths: Vec<f32> = units
        .iter()
        .map(|unit| (unit * CHAR_UNIT_PX).max(MIN_COLUMN_PX))
        .collect();
    let total_width: f32 = widths.iter().sum();
    let markdown = table_to_markdown(data);
    let csv = table_to_csv(data);

    // Content hash keeps element ids stable across identical tables and
    // distinct between different ones, mirroring the code block copy button.
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    markdown.hash(&mut hasher);
    let table_hash = hasher.finish();

    div()
        .w_full()
        .rounded(px(radius::MD))
        .border_1()
        .border_color(theme.border)
        .bg(theme.surface)
        .overflow_hidden()
        .child(table_toolbar(theme, markdown, csv, table_hash))
        .child(
            div()
                .relative()
                .w_full()
                .child(
                    div()
                        .id(("table-scroll", table_hash))
                        .w_full()
                        .overflow_x_scroll()
                        .child(table_body(theme, data, &units, &widths, total_width)),
                )
                .when(total_width > SCROLL_HINT_THRESHOLD_PX, |container| {
                    container.child(scroll_fade(theme))
                }),
        )
        .into_any_element()
}

fn table_toolbar(theme: &Theme, markdown: String, csv: String, table_hash: u64) -> AnyElement {
    div()
        .flex()
        .items_center()
        .justify_between()
        .px_3()
        .py(px(6.0))
        .bg(theme.surface)
        .border_b_1()
        .border_color(theme.border)
        .child(
            div()
                .text_size(px(text_size::XS))
                .font_family(theme.mono_font())
                .text_color(theme.text_faint)
                .child("表格"),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .child(copy_button(
                    theme,
                    ("table-copy-md", table_hash),
                    markdown,
                    "复制 Markdown",
                ))
                .child(copy_button(
                    theme,
                    ("table-copy-csv", table_hash),
                    csv,
                    "复制 CSV",
                )),
        )
        .into_any_element()
}

fn copy_button(
    theme: &Theme,
    id: (&'static str, u64),
    payload: String,
    label: &'static str,
) -> AnyElement {
    div()
        .id(id)
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
            cx.write_to_clipboard(ClipboardItem::new_string(payload.clone()));
        })
        .child(
            svg()
                .path(icons::COPY)
                .size(px(12.0))
                .text_color(theme.text_faint),
        )
        .child(div().text_size(px(text_size::XS)).child(label.to_string()))
        .into_any_element()
}

fn table_body(
    theme: &Theme,
    data: &TableData,
    units: &[f32],
    widths: &[f32],
    total_width: f32,
) -> AnyElement {
    let mut body = div().flex().flex_col().w_full().min_w(px(total_width));
    body = body.child(table_row(
        theme,
        data,
        &data.headers,
        units,
        widths,
        true,
        0,
    ));
    for (row_index, row) in data.rows.iter().enumerate() {
        body = body.child(table_row(theme, data, row, units, widths, false, row_index));
    }
    body.into_any_element()
}

fn table_row(
    theme: &Theme,
    data: &TableData,
    cells: &[String],
    units: &[f32],
    widths: &[f32],
    is_header: bool,
    row_index: usize,
) -> AnyElement {
    let columns = widths.len();
    // Every row resolves the same per-column [basis, grow, min] triple, so
    // columns line up across the header and all body rows.
    let mut row = div()
        .flex()
        .w_full()
        .min_w(px(widths.iter().sum()))
        .border_b_1()
        .border_color(if is_header {
            theme.border_strong
        } else {
            theme.border
        })
        .bg(if is_header {
            theme.surface
        } else if row_index % 2 == 1 {
            theme.surface_alt
        } else {
            theme.surface
        });
    for index in 0..columns {
        row = row.child(table_cell(
            theme,
            cells
                .get(index)
                .cloned()
                .unwrap_or_else(|| String::from("")),
            units[index],
            widths[index],
            column_alignment(data, index),
            is_header,
            index + 1 == columns,
        ));
    }
    row.into_any_element()
}

fn table_cell(
    theme: &Theme,
    content: String,
    unit: f32,
    width: f32,
    alignment: TextAlign,
    is_header: bool,
    is_last: bool,
) -> AnyElement {
    // Zero-width basis plus a grow weight makes the final column width depend
    // only on the measurement (not on the cell text), which keeps every row's
    // columns aligned; `min_w` floors it when the table runs out of space.
    let mut cell = div()
        .w(px(0.0))
        .min_w(px(width))
        .px(px(10.0))
        .py(px(6.0))
        .overflow_hidden()
        .border_color(theme.border);
    if !is_last {
        cell = cell.border_r_1();
    }
    cell.style().flex_grow = Some(unit.max(1.0));
    let mut label = div()
        .whitespace_nowrap()
        .text_ellipsis()
        .text_size(px(text_size::SM))
        .line_height(px(18.0))
        .text_align(alignment)
        .text_color(theme.text)
        .child(content);
    if is_header {
        label = label.font_weight(FontWeight::SEMIBOLD);
    }
    cell.child(label).into_any_element()
}

fn column_alignment(data: &TableData, index: usize) -> TextAlign {
    match data.alignments.get(index).copied() {
        Some(Alignment::Left) => TextAlign::Left,
        Some(Alignment::Center) => TextAlign::Center,
        Some(Alignment::Right) => TextAlign::Right,
        _ => TextAlign::Left,
    }
}

fn scroll_fade(theme: &Theme) -> AnyElement {
    div()
        .absolute()
        .top_0()
        .bottom_0()
        .right_0()
        .w(px(24.0))
        .bg(linear_gradient(
            90.0,
            linear_color_stop(theme.surface.opacity(0.0), 0.0),
            linear_color_stop(theme.surface, 1.0),
        ))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulldown_cmark::{Options, Parser};

    /// Drives the real parser through [`TableCollector`] exactly the way the
    /// Markdown builder does, and returns the finished [`TableData`].
    fn collect(source: &str) -> Option<TableData> {
        let mut options = Options::empty();
        options.insert(Options::ENABLE_TABLES);
        let mut collector: Option<TableCollector> = None;
        for event in Parser::new_ext(source, options) {
            if let Event::Start(Tag::Table(alignments)) = &event {
                collector = Some(TableCollector::new(alignments.clone()));
                continue;
            }
            if let Some(data) = collector
                .as_mut()
                .and_then(|active| active.handle_event(event))
            {
                return Some(data);
            }
        }
        None
    }

    fn table(headers: &[&str], rows: &[&[&str]]) -> TableData {
        TableData {
            alignments: vec![Alignment::None; headers.len()],
            headers: headers.iter().map(|cell| cell.to_string()).collect(),
            rows: rows
                .iter()
                .map(|row| row.iter().map(|cell| cell.to_string()).collect())
                .collect(),
        }
        .normalized()
    }

    #[test]
    fn collects_headers_rows_and_alignments_from_table_events() {
        let data = collect("| 左 | 中 | 右 |\n| --- | :---: | ---: |\n| 1 | 2 | 3 |")
            .expect("table events should finish with a TableData");
        assert_eq!(data.headers, vec!["左", "中", "右"]);
        assert_eq!(data.rows, vec![vec!["1", "2", "3"]]);
        assert_eq!(
            data.alignments,
            vec![Alignment::None, Alignment::Center, Alignment::Right]
        );
        assert_eq!(data.column_count(), 3);
    }

    #[test]
    fn flattens_inline_events_in_cells_to_plain_text() {
        let data = collect("| **bold** `code` <br>x |\n| --- |\n| a |")
            .expect("table events should finish with a TableData");
        // Text/Code are kept verbatim, `<br>` becomes a newline so multi-line
        // cells survive, other inline events (emphasis wrappers) flatten away.
        assert_eq!(data.headers, vec!["bold code \nx"]);
        assert_eq!(data.rows, vec![vec!["a"]]);
    }

    #[test]
    fn normalizes_ragged_rows_to_the_widest_row() {
        let data = TableData {
            alignments: vec![],
            headers: vec!["a".into(), "b".into()],
            rows: vec![vec!["1".into(), "2".into(), "3".into()]],
        }
        .normalized();
        assert_eq!(data.headers, vec!["a", "b", ""]);
        assert_eq!(data.rows, vec![vec!["1", "2", "3"]]);
    }

    #[test]
    fn non_table_input_yields_no_table_data() {
        assert!(collect("just a **paragraph**, no table").is_none());
        assert!(collect("- list item\n- another item").is_none());
    }

    #[test]
    fn markdown_output_round_trips_through_the_parser() {
        let data = table(&["name", "pipe|cell"], &[&["x", "line1\nline2"]]);
        let markdown = table_to_markdown(&data);
        let reparsed = collect(&markdown).expect("markdown should re-parse as a table");
        assert_eq!(reparsed.headers, vec!["name", "pipe|cell"]);
        assert_eq!(reparsed.rows, vec![vec!["x", "line1\nline2"]]);
        assert_eq!(table_to_markdown(&reparsed), markdown);
    }

    #[test]
    fn csv_quotes_commas_quotes_and_newlines() {
        let data = table(
            &["plain", "needs \"quotes\"", "a,b", "line1\nline2"],
            &[&["1", "2", "3", "4"]],
        );
        let csv = table_to_csv(&data);
        assert_eq!(
            csv,
            "plain,\"needs \"\"quotes\"\"\",\"a,b\",\"line1\nline2\"\n1,2,3,4\n"
        );
    }

    #[test]
    fn csv_guards_formula_injection_prefixes() {
        let data = table(
            &["=cmd", "-1", "+2", "@x", "safe"],
            &[&["a", "b", "c", "d", "e"]],
        );
        let csv = table_to_csv(&data);
        assert_eq!(csv, "'=cmd,'-1,'+2,'@x,safe\na,b,c,d,e\n");
    }

    #[test]
    fn measure_counts_cjk_as_double_width_and_floors_small_columns() {
        let units = measure_columns(&table(&["ab", "中文"], &[]));
        assert_eq!(units, vec![6.0, 6.0]);
        let units = measure_columns(&table(&["abcdefghijklmno", "中文列名"], &[]));
        assert_eq!(units, vec![15.0, 8.0]);
    }

    #[test]
    fn measure_caps_oversized_columns() {
        let long = "x".repeat(200);
        let units = measure_columns(&table(&[&long], &[]));
        assert_eq!(units, vec![40.0]);
    }

    #[test]
    fn measure_falls_back_to_equal_split_when_columns_exceed_the_cap() {
        let headers: Vec<String> = (0..MAX_COLUMNS + 2)
            .map(|index| format!("c{index}"))
            .collect();
        let data = TableData {
            alignments: vec![],
            headers,
            rows: Vec::new(),
        };
        let units = measure_columns(&data.normalized());
        assert_eq!(units, vec![1.0; MAX_COLUMNS + 2]);
    }
}
