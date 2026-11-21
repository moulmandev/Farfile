//! The text of a file being edited, drawn row by row and only the rows in
//! view: a file of fifty thousand lines costs no more than one of fifty.
//! Every character sits on a grid of cells (see `editor::layout`), so the
//! place of the pointer or of the cursor is arithmetic, never a measure of
//! the whole text.

use std::collections::HashMap;
use std::time::Duration;

use egui::text::{ByteIndex, LayoutJob, LayoutSection, TextWrapping};
use egui::{
    Align, Color32, CursorIcon, Event, EventFilter, FontId, Key, Modifiers, MouseWheelUnit, Pos2, Rect, ScrollArea,
    Sense, Stroke, TextFormat, Ui, Vec2, pos2, vec2,
};

use super::EditorAction;
use crate::editor::highlight::Span;
use crate::editor::layout::{cells, char_cells, wrap_points};
use crate::editor::prefs::EditorPrefs;
use crate::editor::syntax::EditorTheme;
use crate::editor::text::{Pos, Selection};
use crate::editor::{Editor, Reveal};
use crate::terminal_ext::fonts::terminal_fonts;
use crate::ui::Action;

/// Room left of the text, inside the text area.
const LEFT: f32 = 6.0;
/// Room above the first line.
const TOP: f32 = 4.0;
/// Lines kept in view above and below the cursor when it moves.
const MARGIN_ROWS: f32 = 2.0;
/// Lines longer than this are drawn from where the view shows them only.
const WINDOWED: usize = 1_000;
/// Time given to colouring on each frame.
const COLOUR_BUDGET: Duration = Duration::from_millis(5);
/// Time given to the reading of the states on each frame.
const READ_BUDGET: Duration = Duration::from_millis(3);
const BLINK: f64 = 0.53;

/// The measures of the text, for this frame.
pub struct Metrics {
    pub font: FontId,
    pub bold: Option<FontId>,
    /// The height of a row and the width of a cell.
    pub row: f32,
    pub cell: f32,
    /// Where the glyphs sit in their row.
    pub baseline: f32,
    pub tab: usize,
    pub wrap: bool,
    pub whitespace: bool,
}

impl Metrics {
    pub fn new(ctx: &egui::Context, prefs: &EditorPrefs, editor: &Editor) -> Self {
        let fonts = terminal_fonts(ctx, &prefs.font, prefs.font_size);
        let (height, cell) = ctx.fonts_mut(|fonts_view| {
            (fonts_view.row_height(&fonts.regular), fonts_view.glyph_width(&fonts.regular, '0'))
        });
        let row = (height * 1.38).round();
        Self {
            font: fonts.regular,
            bold: fonts.bold,
            row,
            cell,
            baseline: ((row - height) / 2.0).round(),
            tab: editor.tab_width(),
            wrap: prefs.wrap,
            whitespace: prefs.whitespace,
        }
    }
}

/// One row on screen: a piece of a line.
#[derive(Clone, Copy, Debug)]
struct Row {
    line: usize,
    /// Bytes of the line it shows.
    start: usize,
    end: usize,
    /// The cell of the line it starts at: rows of a wrapped line after the
    /// first start further in.
    start_cell: usize,
    /// It is the first row of its line.
    first: bool,
}

/// The rows of `line`, wrapped or not.
fn rows_of(editor: &Editor, metrics: &Metrics, line: usize) -> Vec<Row> {
    let text = editor.doc.line(line);
    if !metrics.wrap {
        return vec![Row { line, start: 0, end: text.len(), start_cell: 0, first: true }];
    }
    let points = wrap_points(text, editor.view.wrap.columns(), metrics.tab);
    let mut rows = Vec::with_capacity(points.len() + 1);
    let mut start = 0;
    for end in points.iter().copied().chain(std::iter::once(text.len())) {
        rows.push(Row { line, start, end, start_cell: cells(&text[..start], metrics.tab), first: start == 0 });
        start = end;
    }
    rows
}

/// The row of the screen a place is on, from the top of the text.
fn row_index(editor: &Editor, metrics: &Metrics, pos: Pos) -> (usize, Row) {
    let rows = rows_of(editor, metrics, pos.line);
    if !metrics.wrap {
        return (pos.line, rows[0]);
    }
    // The end of a row is the start of the next one: the cursor goes to the
    // next row.
    let index = rows.iter().rposition(|row| row.start <= pos.col).unwrap_or(0);
    (editor.view.wrap.row_of(pos.line) + index, rows[index])
}

fn total_rows(editor: &Editor, metrics: &Metrics) -> usize {
    if metrics.wrap { editor.view.wrap.total() } else { editor.doc.line_count() }
}

/// The row at `index` from the top.
fn row_at(editor: &Editor, metrics: &Metrics, index: usize) -> Row {
    if !metrics.wrap {
        let line = index.min(editor.doc.line_count() - 1);
        return rows_of(editor, metrics, line)[0];
    }
    let (line, sub) = editor.view.wrap.line_at(index);
    let rows = rows_of(editor, metrics, line);
    rows[sub.min(rows.len() - 1)]
}

/// The cell of `pos` in its row.
fn cell_in_row(editor: &Editor, metrics: &Metrics, pos: Pos, row: &Row) -> usize {
    let text = editor.doc.line(pos.line);
    cells(&text[..pos.col], metrics.tab).saturating_sub(row.start_cell)
}

/// The place at cell `cell` of a row.
fn pos_in_row(editor: &Editor, metrics: &Metrics, row: &Row, cell: f32) -> Pos {
    let text = editor.doc.line(row.line);
    // Tabs keep the stops of the whole line.
    let mut at = row.start_cell;
    for (index, c) in text[row.start..row.end].char_indices() {
        let width = char_cells(c, at, metrics.tab);
        if cell + (row.start_cell as f32) < at as f32 + width as f32 / 2.0 {
            return Pos::new(row.line, row.start + index);
        }
        at += width;
    }
    // Past the end of a wrapped row, the cursor stays on it.
    let end = if row.end < text.len() && row.end > row.start {
        text[..row.end].char_indices().next_back().map_or(row.end, |(index, _)| index)
    } else {
        row.end
    };
    Pos::new(row.line, end)
}

/// Draws the text and takes the keyboard and the mouse. Returns whether
/// the text has the keyboard.
pub fn show(
    ui: &mut Ui,
    theme: &EditorTheme,
    editor: &mut Editor,
    prefs: &EditorPrefs,
    interactive: bool,
    actions: &mut Vec<Action>,
) -> bool {
    let ctx = ui.ctx().clone();
    let metrics = Metrics::new(&ctx, prefs, editor);
    let area = ui.available_rect_before_wrap();
    ui.allocate_rect(area, Sense::hover());
    let id = ui.id().with("editor-text");
    let digits = editor.doc.line_count().to_string().len().max(3);
    let gutter_width = (digits as f32 * metrics.cell + 30.0).round();
    let gutter = Rect::from_min_max(area.min, pos2(area.left() + gutter_width, area.bottom()));
    let text_rect = Rect::from_min_max(pos2(gutter.right(), area.top()), area.max);
    let left = egui::CornerRadius { nw: 8, sw: 8, ne: 0, se: 0 };
    let right = egui::CornerRadius { nw: 0, sw: 0, ne: 8, se: 8 };
    ui.painter().rect_filled(gutter, left, theme.gutter);
    ui.painter().rect_filled(text_rect, right, theme.background);

    // Word wrap measures every line at the width of the view: before the
    // keys, which move by rows, and again after what they changed.
    let scroll_bar = ui.spacing().scroll.bar_width + ui.spacing().scroll.bar_inner_margin;
    let columns = ((text_rect.width() - LEFT - scroll_bar - 8.0) / metrics.cell).floor().max(8.0) as usize;
    if metrics.wrap {
        editor.view.wrap.update(&editor.doc, columns, metrics.tab);
    }
    let focused = ui.memory(|memory| memory.has_focus(id));
    if focused && interactive {
        ui.memory_mut(|memory| {
            memory.set_focus_lock_filter(
                id,
                EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true },
            );
        });
        keyboard(ui, editor, &metrics);
    }
    editor.catch_up(READ_BUDGET);
    if metrics.wrap {
        editor.view.wrap.update(&editor.doc, columns, metrics.tab);
    }
    let rows = total_rows(editor, &metrics);
    let viewport = editor.view.viewport;
    let height = TOP + rows as f32 * metrics.row + (viewport.y * 0.5).max(metrics.row * 2.0);
    let width = if metrics.wrap {
        text_rect.width() - scroll_bar
    } else {
        LEFT + (editor.widest() + 4) as f32 * metrics.cell
    };

    let mut scroll = if metrics.wrap { ScrollArea::vertical() } else { ScrollArea::both() }
        .id_salt(id.with("scroll"))
        .auto_shrink([false, false])
        .content_margin(egui::Margin::ZERO);
    if let Some(offset) = wanted_offset(editor, &metrics, viewport) {
        scroll = scroll.scroll_offset(offset);
    }
    let mut text_ui = ui.new_child(egui::UiBuilder::new().max_rect(text_rect).id_salt(id.with("area")));
    let output = scroll.show_viewport(&mut text_ui, |ui, view| {
        ui.set_min_size(vec2(width, height));
        let origin = ui.max_rect().min;
        let response = ui.interact(ui.clip_rect(), id, Sense::click_and_drag());
        if interactive {
            mouse(ui, &response, editor, &metrics, origin);
            if std::mem::take(&mut editor.view.focus) {
                response.request_focus();
            }
        }
        response.context_menu(|ui| super::context_menu(ui, editor, actions));
        let first = ((view.min.y - TOP) / metrics.row).floor().max(0.0) as usize;
        let last = (((view.max.y - TOP) / metrics.row).ceil().max(0.0) as usize + 1).min(rows);
        paint(ui, theme, editor, &metrics, origin, first..last, view, focused && interactive);
        response
    });
    editor.view.offset = output.state.offset;
    editor.view.viewport = output.inner_rect.size();
    let response = output.inner;
    if response.hovered() {
        ctx.set_cursor_icon(CursorIcon::Text);
    }
    gutter_numbers(ui, theme, editor, &metrics, gutter, output.state.offset.y, rows);

    if focused && interactive {
        let (row, row_info) = row_index(editor, &metrics, editor.sel.head);
        let cell = cell_in_row(editor, &metrics, editor.sel.head, &row_info) as f32;
        let x = text_rect.left() + LEFT + cell * metrics.cell - output.state.offset.x;
        let y = text_rect.top() + TOP + row as f32 * metrics.row - output.state.offset.y;
        let cursor = Rect::from_min_size(pos2(x, y), vec2(2.0, metrics.row));
        ctx.output_mut(|output| {
            output.ime = Some(egui::output::IMEOutput {
                purpose: egui::IMEPurpose::Normal,
                rect: text_rect,
                cursor_rect: cursor,
                should_interrupt_composition: false,
            });
        });
    }
    zoom_wheel(ui, &response, editor, actions);
    focused
}

/// The scroll offset for this frame, when it is to change: the cursor
/// brought into view, or a scroll asked by a key.
fn wanted_offset(editor: &mut Editor, metrics: &Metrics, viewport: Vec2) -> Option<Vec2> {
    if viewport.y <= 0.0 {
        return None;
    }
    let mut offset = editor.view.offset;
    offset.y = (offset.y + std::mem::take(&mut editor.view.scroll_by)).max(0.0);
    if let Some(reveal) = editor.view.reveal.take() {
        let (row, row_info) = row_index(editor, metrics, editor.sel.head);
        let y = TOP + row as f32 * metrics.row;
        let x = LEFT + cell_in_row(editor, metrics, editor.sel.head, &row_info) as f32 * metrics.cell;
        let margin = (MARGIN_ROWS * metrics.row).min(viewport.y / 3.0);
        let outside = y < offset.y || y + metrics.row > offset.y + viewport.y;
        if reveal == Reveal::Center && outside {
            offset.y = (y - viewport.y / 2.0 + metrics.row / 2.0).max(0.0);
        } else if y < offset.y + margin {
            offset.y = (y - margin).max(0.0);
        } else if y + metrics.row > offset.y + viewport.y - margin {
            offset.y = y + metrics.row - viewport.y + margin;
        }
        if !metrics.wrap {
            let room = 6.0 * metrics.cell;
            if x < offset.x + room {
                offset.x = (x - room * 2.0).max(0.0);
            } else if x > offset.x + viewport.x - room {
                offset.x = x - viewport.x + room * 2.0;
            }
        }
    }
    (offset != editor.view.offset).then_some(offset)
}

/// The place under a point of the screen.
fn place(editor: &Editor, metrics: &Metrics, origin: Pos2, at: Pos2) -> Pos {
    let y = (at.y - origin.y - TOP) / metrics.row;
    let total = total_rows(editor, metrics);
    if y < 0.0 {
        return Pos::default();
    }
    if y >= total as f32 {
        return editor.doc.end();
    }
    let row = row_at(editor, metrics, (y.floor() as usize).min(total.saturating_sub(1)));
    let cell = ((at.x - origin.x - LEFT) / metrics.cell).max(0.0);
    pos_in_row(editor, metrics, &row, cell)
}

/// Clicks, double and triple clicks, drags, and a right click that puts
/// the cursor where the menu opens.
fn mouse(ui: &Ui, response: &egui::Response, editor: &mut Editor, metrics: &Metrics, origin: Pos2) {
    let pointer = response.interact_pointer_pos().or_else(|| response.hover_pos());
    let (pressed, shift, now) = ui.input(|input| (input.pointer.primary_pressed(), input.modifiers.shift, input.time));
    if pressed
        && response.is_pointer_button_down_on()
        && let Some(at) = pointer
    {
        let pos = place(editor, metrics, origin, at);
        let count = match editor.view.click {
            Some((time, last, count)) if now - time < 0.45 && last.line == pos.line && last.col.abs_diff(pos.col) <= 2 => {
                count % 3 + 1
            }
            _ => 1,
        };
        editor.view.click = Some((now, pos, count));
        editor.view.drag_unit = count;
        editor.view.column = None;
        editor.view.moved_at = now;
        match count {
            2 => {
                let (start, end) = editor.doc.word_at(pos);
                editor.sel = Selection { anchor: start, head: end };
            }
            3 => editor.sel = Selection { anchor: Pos::new(pos.line, 0), head: line_end(editor, pos.line) },
            _ => editor.move_to(pos, shift),
        }
        editor.history.seal();
        response.request_focus();
    } else if response.dragged_by(egui::PointerButton::Primary)
        && let Some(at) = pointer
    {
        let pos = place(editor, metrics, origin, at);
        editor.sel = match editor.view.drag_unit {
            // By words: the word first clicked stays selected whichever way.
            2 => {
                let (first_start, first_end) = editor.doc.word_at(editor.view.click.map_or(pos, |(_, at, _)| at));
                let (start, end) = editor.doc.word_at(pos);
                if pos < first_start {
                    Selection { anchor: first_end, head: start }
                } else {
                    Selection { anchor: first_start, head: end }
                }
            }
            3 => {
                let first = editor.view.click.map_or(pos.line, |(_, at, _)| at.line);
                if pos.line < first {
                    Selection { anchor: line_end(editor, first), head: Pos::new(pos.line, 0) }
                } else {
                    Selection { anchor: Pos::new(first, 0), head: line_end(editor, pos.line) }
                }
            }
            _ => Selection { anchor: editor.sel.anchor, head: pos },
        };
        editor.view.moved_at = now;
        // Dragged past the edge: the view follows.
        if !response.rect.contains(at) {
            editor.view.reveal = Some(Reveal::Cursor);
            ui.ctx().request_repaint();
        }
    }
    if response.secondary_clicked()
        && let Some(at) = pointer
    {
        let pos = place(editor, metrics, origin, at);
        let (a, b) = editor.sel.range();
        if editor.sel.is_empty() || pos < a || pos > b {
            editor.move_to(pos, false);
        }
        response.request_focus();
    }
}

/// The start of the next line, or the end of the last: what a line
/// selected whole goes to.
fn line_end(editor: &Editor, line: usize) -> Pos {
    if line + 1 < editor.doc.line_count() {
        Pos::new(line + 1, 0)
    } else {
        Pos::new(line, editor.doc.line(line).len())
    }
}

/// Ctrl+wheel changes the size of the text.
fn zoom_wheel(ui: &Ui, response: &egui::Response, editor: &mut Editor, actions: &mut Vec<Action>) {
    if !response.contains_pointer() {
        return;
    }
    let line = ui.ctx().options(|options| options.input_options.line_scroll_speed);
    let mut zoom = 0.0;
    ui.ctx().input_mut(|input| {
        input.events.retain(|event| match event {
            Event::MouseWheel { unit, delta, modifiers, .. } if modifiers.ctrl || modifiers.command => {
                zoom += match unit {
                    MouseWheelUnit::Line => delta.y,
                    MouseWheelUnit::Point => delta.y / line,
                    MouseWheelUnit::Page => delta.y * 3.0,
                };
                false
            }
            _ => true,
        });
    });
    if zoom != 0.0 {
        editor.view.zoom_wheel += zoom;
        let steps = editor.view.zoom_wheel.trunc() as i32;
        editor.view.zoom_wheel -= steps as f32;
        if steps != 0 {
            actions.push(Action::Editor(EditorAction::Zoom(steps)));
        }
    }
}

/// What the keys do to the text. Shortcuts of the whole editor (save,
/// search) were taken out of the input before.
fn keyboard(ui: &Ui, editor: &mut Editor, metrics: &Metrics) {
    let events = ui.input(|input| input.events.clone());
    let now = ui.input(|input| input.time);
    let mut handled = false;
    for event in events {
        handled |= match event {
            Event::Text(text) if !text.is_empty() => {
                editor.insert(&text);
                true
            }
            Event::Ime(egui::ImeEvent::Commit(text)) if !text.is_empty() => {
                editor.insert(&text);
                true
            }
            Event::Paste(text) => {
                editor.insert(&text);
                true
            }
            Event::Copy => {
                ui.ctx().copy_text(editor.copied());
                false
            }
            Event::Cut => {
                let text = editor.cut();
                ui.ctx().copy_text(text);
                true
            }
            Event::Key { key, pressed: true, modifiers, .. } => key_press(ui, editor, metrics, key, modifiers),
            _ => false,
        };
    }
    if handled {
        editor.view.moved_at = now;
    }
}

/// One key. Returns whether it changed the text or moved the cursor.
fn key_press(ui: &Ui, editor: &mut Editor, metrics: &Metrics, key: Key, modifiers: Modifiers) -> bool {
    // AltGr comes as Ctrl+Alt: it types, it is no shortcut.
    let ctrl = (modifiers.ctrl || modifiers.command) && !modifiers.alt;
    let shift = modifiers.shift;
    let page = ((editor.view.viewport.y / metrics.row).floor() as usize).saturating_sub(1).max(1);
    match key {
        Key::ArrowLeft | Key::ArrowRight => {
            let right = key == Key::ArrowRight;
            let (a, b) = editor.sel.range();
            let target = if !shift && !ctrl && !editor.sel.is_empty() {
                if right { b } else { a }
            } else if ctrl {
                if right { editor.doc.word_right(editor.sel.head) } else { editor.doc.word_left(editor.sel.head) }
            } else if right {
                editor.doc.next_char(editor.sel.head)
            } else {
                editor.doc.prev_char(editor.sel.head)
            };
            editor.move_to(target, shift);
            editor.view.column = None;
        }
        Key::ArrowUp | Key::ArrowDown if modifiers.alt || (ctrl && shift) => editor.move_lines(key == Key::ArrowUp),
        Key::ArrowUp | Key::ArrowDown if ctrl => {
            // The view moves by a line, the cursor stays.
            editor.view.scroll_by += if key == Key::ArrowUp { -metrics.row } else { metrics.row };
            ui.ctx().request_repaint();
            return false;
        }
        Key::ArrowUp | Key::ArrowDown => vertical(editor, metrics, if key == Key::ArrowUp { -1 } else { 1 }, shift),
        Key::PageUp | Key::PageDown => {
            let rows = if key == Key::PageUp { -(page as isize) } else { page as isize };
            editor.view.scroll_by += rows as f32 * metrics.row;
            vertical(editor, metrics, rows, shift);
        }
        Key::Home => {
            let head = editor.sel.head;
            let target = if ctrl {
                Pos::default()
            } else {
                // To the text first, then to the very start of the line.
                let indent = editor.doc.indent_of(head.line).len();
                Pos::new(head.line, if head.col == indent { 0 } else { indent })
            };
            editor.move_to(target, shift);
            editor.view.column = None;
        }
        Key::End => {
            let head = editor.sel.head;
            let target = if ctrl { editor.doc.end() } else { Pos::new(head.line, editor.doc.line(head.line).len()) };
            editor.move_to(target, shift);
            editor.view.column = None;
        }
        Key::Backspace if ctrl => editor.delete_word(false),
        Key::Delete if ctrl => editor.delete_word(true),
        Key::Backspace => editor.backspace(),
        Key::Delete if shift => {
            let text = editor.cut();
            ui.ctx().copy_text(text);
        }
        Key::Delete => editor.delete(),
        Key::Enter if ctrl => {
            // A new line below, wherever the cursor is in this one.
            let line = editor.sel.head.line;
            editor.move_to(Pos::new(line, editor.doc.line(line).len()), false);
            editor.newline();
        }
        Key::Enter => editor.newline(),
        Key::Tab if shift => editor.indent_lines(true),
        Key::Tab if !ctrl => editor.tab(),
        Key::Escape => {
            if !editor.sel.is_empty() {
                editor.sel = Selection::cursor(editor.sel.head);
            }
            return false;
        }
        Key::A if ctrl => editor.select_all(),
        Key::Z if ctrl && shift => editor.redo(),
        Key::Z if ctrl => editor.undo(),
        Key::Y if ctrl => editor.redo(),
        Key::D if ctrl => editor.duplicate(),
        Key::K if ctrl && shift => editor.delete_lines(),
        Key::L if ctrl => {
            let line = editor.sel.head.line;
            let start = if editor.sel.is_empty() { Pos::new(line, 0) } else { editor.sel.range().0 };
            editor.sel = Selection { anchor: start, head: line_end(editor, line) };
            editor.view.reveal = Some(Reveal::Cursor);
        }
        // Ctrl+/ (Ctrl+: on a French keyboard), and Ctrl+Q as in Notepad++.
        Key::Slash | Key::Colon | Key::Q if ctrl => editor.toggle_comment(),
        // From scripts, which send keys rather than clipboard events.
        Key::C if ctrl => {
            ui.ctx().copy_text(editor.copied());
            return false;
        }
        Key::X if ctrl => {
            let text = editor.cut();
            ui.ctx().copy_text(text);
        }
        _ => return false,
    }
    true
}

/// Up and down, by rows of the screen, keeping to a column.
fn vertical(editor: &mut Editor, metrics: &Metrics, rows: isize, extend: bool) {
    let head = editor.sel.head;
    let (index, row) = row_index(editor, metrics, head);
    let column = match editor.view.column {
        Some(column) => column,
        None => cell_in_row(editor, metrics, head, &row),
    };
    let total = total_rows(editor, metrics) as isize;
    let target = index as isize + rows;
    let pos = if target < 0 {
        Pos::default()
    } else if target >= total {
        editor.doc.end()
    } else {
        let row = row_at(editor, metrics, target as usize);
        pos_in_row(editor, metrics, &row, column as f32)
    };
    editor.move_to(pos, extend);
    editor.view.column = Some(column);
}

/// The colours of the rows in view, the cursor and everything marked.
#[allow(clippy::too_many_arguments)]
fn paint(
    ui: &Ui,
    theme: &EditorTheme,
    editor: &mut Editor,
    metrics: &Metrics,
    origin: Pos2,
    rows: std::ops::Range<usize>,
    view: Rect,
    focused: bool,
) {
    let row_list: Vec<Row> = rows.clone().map(|index| row_at(editor, metrics, index)).collect();
    let lines = match (row_list.first(), row_list.last()) {
        (Some(first), Some(last)) => first.line..last.line + 1,
        _ => 0..0,
    };
    if !editor.highlight.prepare(&editor.doc, lines.clone(), theme, COLOUR_BUDGET) || editor.highlight.busy() {
        ui.ctx().request_repaint();
    }
    let painter = ui.painter();
    let (sel_start, sel_end) = editor.sel.range();
    let head = editor.sel.head;
    let cursor_row = row_index(editor, metrics, head).0;
    let brackets = if editor.sel.is_empty() { editor.bracket_pair() } else { None };
    let matches = editor.find.matches();
    let current = editor.find.current;
    let width = ui.max_rect().width().max(view.max.x);
    let left_cell = if metrics.wrap { 0 } else { ((view.min.x - LEFT) / metrics.cell).floor().max(0.0) as usize };
    let shown_cells = (view.width() / metrics.cell).ceil() as usize + 2;
    let mut glyphs: HashMap<char, f32> = HashMap::new();
    let first_match = matches.partition_point(|found| found.line < lines.start);
    let now = ui.input(|input| input.time);

    for (offset, row) in row_list.iter().enumerate() {
        let index = rows.start + offset;
        let top = origin.y + TOP + index as f32 * metrics.row;
        let x0 = origin.x + LEFT;
        let text = editor.doc.line(row.line);
        let row_rect = Rect::from_min_size(pos2(origin.x, top), vec2(width, metrics.row));
        // The cells within the row of bytes of the line.
        let span = |from: usize, to: usize| {
            let (from, to) = (from.clamp(row.start, row.end), to.clamp(row.start, row.end));
            let a = cells(&text[..from], metrics.tab) - row.start_cell;
            let b = cells(&text[..to], metrics.tab) - row.start_cell;
            (a, b)
        };
        let cell_rect = |a: usize, b: usize| {
            Rect::from_min_max(pos2(x0 + a as f32 * metrics.cell, top), pos2(x0 + b as f32 * metrics.cell, top + metrics.row))
        };

        if focused && sel_start == sel_end && row.line == head.line {
            painter.rect_filled(row_rect, 0, theme.current_line);
        }
        // The matches of the search, the one shown stronger.
        for (number, found) in matches.iter().enumerate().skip(first_match) {
            if found.line > row.line {
                break;
            }
            if found.line < row.line || found.end <= row.start || found.start >= row.end {
                continue;
            }
            let (a, b) = span(found.start, found.end);
            let color = if Some(number) == current { theme.found_current } else { theme.found };
            painter.rect_filled(cell_rect(a, b.max(a + 1)), 2, color);
        }
        // The selection, a cell past the end of a row it goes on from.
        if sel_start != sel_end && sel_start.line <= row.line && row.line <= sel_end.line {
            let from = if sel_start.line == row.line { sel_start.col } else { row.start };
            let to = if sel_end.line == row.line { sel_end.col } else { text.len() };
            if to >= row.start && from <= row.end {
                let (a, mut b) = span(from, to);
                if sel_end.line > row.line && to >= row.end && row.end == text.len() {
                    b += 1;
                }
                if b > a {
                    let color = if focused { theme.selection } else { theme.selection.gamma_multiply(0.75) };
                    painter.rect_filled(cell_rect(a, b), 0, color);
                }
            }
        }
        if let Some((one, other)) = brackets {
            for pos in [one, other] {
                if pos.line == row.line && pos.col >= row.start && pos.col < row.end {
                    let size = text[pos.col..].chars().next().map_or(1, char::len_utf8);
                    let (a, b) = span(pos.col, pos.col + size);
                    painter.rect_filled(cell_rect(a, b), 2, theme.bracket.gamma_multiply(0.45));
                    painter.rect_stroke(cell_rect(a, b), 2, Stroke::new(1.0, theme.bracket), egui::StrokeKind::Inside);
                }
            }
        }

        // The text; of a long line, only what the view shows.
        let (from, skip_cells) = if !metrics.wrap && row.end - row.start > WINDOWED && left_cell > 8 {
            window_start(text, left_cell - 8, metrics.tab)
        } else {
            (row.start, 0)
        };
        let to = if !metrics.wrap && row.end - from > WINDOWED {
            window_end(text, from, skip_cells, shown_cells + 16, metrics.tab)
        } else {
            row.end
        };
        let job = row_job(editor, theme, metrics, row.line, from, to, &mut glyphs, ui.ctx());
        if !job.text.is_empty() {
            let galley = painter.layout_job(job);
            let x = x0 + (skip_cells.saturating_sub(row.start_cell)) as f32 * metrics.cell;
            painter.galley(pos2(x, top + metrics.baseline), galley, theme.foreground);
        }

        // The cursor, steady while typing, then blinking.
        if focused && index == cursor_row {
            let since = now - editor.view.moved_at;
            if since < BLINK || ((since / BLINK) as u64).is_multiple_of(2) {
                let (a, _) = span(head.col, head.col);
                let x = x0 + a as f32 * metrics.cell;
                let caret = Rect::from_min_size(pos2(x - 0.5, top + 1.0), vec2(2.0, metrics.row - 2.0));
                painter.rect_filled(caret, 0, theme.caret);
            }
            let wait = BLINK - since.rem_euclid(BLINK);
            ui.ctx().request_repaint_after(Duration::from_secs_f64(wait.max(0.02)));
        }
    }
}

/// The byte where a long line starts to show, from about cell `cell`, and
/// the cell it is at.
fn window_start(text: &str, cell: usize, tab: usize) -> (usize, usize) {
    let mut at = 0;
    for (index, c) in text.char_indices() {
        if at >= cell {
            return (index, at);
        }
        at += char_cells(c, at, tab);
    }
    (text.len(), at)
}

fn window_end(text: &str, from: usize, from_cell: usize, wanted: usize, tab: usize) -> usize {
    let mut at = from_cell;
    for (index, c) in text[from..].char_indices() {
        if at >= from_cell + wanted {
            return from + index;
        }
        at += char_cells(c, at, tab);
    }
    text.len()
}

/// A row as egui lays it out: coloured runs, tabs and spaces made visible
/// when asked, and the characters of another width held to the grid.
#[allow(clippy::too_many_arguments)]
fn row_job(
    editor: &Editor,
    theme: &EditorTheme,
    metrics: &Metrics,
    line: usize,
    from: usize,
    to: usize,
    glyphs: &mut HashMap<char, f32>,
    ctx: &egui::Context,
) -> LayoutJob {
    let text = editor.doc.line(line);
    let spans: &[Span] = editor.highlight.spans(line).unwrap_or(&[]);
    let mut job = LayoutJob {
        wrap: TextWrapping::no_max_width(),
        break_on_newline: false,
        halign: Align::LEFT,
        ..Default::default()
    };
    let plain = TextFormat { font_id: metrics.font.clone(), color: theme.foreground, ..Default::default() };
    let format_of = |byte: usize| -> TextFormat {
        let found = spans.partition_point(|span| (span.end as usize) <= byte);
        match spans.get(found) {
            Some(span) if (span.start as usize) <= byte => {
                let font_id = match (&metrics.bold, span.bold) {
                    (Some(bold), true) => bold.clone(),
                    _ => metrics.font.clone(),
                };
                let underline = if span.underline { Stroke::new(1.0, span.color) } else { Stroke::NONE };
                TextFormat { font_id, color: span.color, italics: span.italic, underline, ..Default::default() }
            }
            _ => plain.clone(),
        }
    };

    let mut at = cells(&text[..from], metrics.tab);
    let mut section_start = 0usize;
    let mut current: Option<TextFormat> = None;
    let mut lead = 0.0f32;
    let mut pending_lead = 0.0f32;
    let close = |job: &mut LayoutJob, current: &mut Option<TextFormat>, lead: &mut f32, start: &mut usize| {
        if let Some(format) = current.take()
            && job.text.len() > *start
        {
            let byte_range = ByteIndex(*start)..ByteIndex(job.text.len());
            job.sections.push(LayoutSection { leading_space: *lead, byte_range, format });
            *lead = 0.0;
        }
        *start = job.text.len();
    };
    let mut buffer = [0u8; 4];
    let mut spaces = String::new();
    for (index, c) in text[from..to].char_indices() {
        let width = char_cells(c, at, metrics.tab);
        let mut format = format_of(from + index);
        let shown: &str = match c {
            '\t' => {
                spaces.clear();
                if metrics.whitespace {
                    spaces.push('→');
                    spaces.extend(std::iter::repeat_n(' ', width.saturating_sub(1)));
                    format.color = theme.whitespace;
                } else {
                    spaces.extend(std::iter::repeat_n(' ', width));
                }
                &spaces
            }
            ' ' if metrics.whitespace => {
                format.color = theme.whitespace;
                "·"
            }
            c if c.is_control() => {
                format.color = if theme.dark { Color32::from_rgb(0xe0, 0x6c, 0x75) } else { Color32::from_rgb(0xc9, 0x2a, 0x2a) };
                "·"
            }
            c => c.encode_utf8(&mut buffer),
        };
        // A glyph wider or narrower than its cells: what follows starts
        // back on the grid.
        let expected = width as f32 * metrics.cell;
        let actual = if shown.is_ascii() {
            expected
        } else {
            let first = shown.chars().next().unwrap_or(' ');
            let one = *glyphs.entry(first).or_insert_with(|| ctx.fonts_mut(|fonts| fonts.glyph_width(&format.font_id, first)));
            one + (shown.chars().count() - 1) as f32 * metrics.cell
        };
        let odd = (actual - expected).abs() > 0.25;
        let same = pending_lead == 0.0 && current.as_ref().is_some_and(|open| same_format(open, &format));
        if !same {
            close(&mut job, &mut current, &mut lead, &mut section_start);
            lead = std::mem::take(&mut pending_lead);
            current = Some(format);
        }
        job.text.push_str(shown);
        if odd {
            close(&mut job, &mut current, &mut lead, &mut section_start);
            pending_lead = expected - actual;
        }
        at += width;
    }
    close(&mut job, &mut current, &mut lead, &mut section_start);
    job
}

fn same_format(a: &TextFormat, b: &TextFormat) -> bool {
    a.color == b.color && a.italics == b.italics && a.underline == b.underline && a.font_id == b.font_id
}

/// The line numbers beside the rows in view, the cursor's brighter.
fn gutter_numbers(ui: &Ui, theme: &EditorTheme, editor: &Editor, metrics: &Metrics, gutter: Rect, offset: f32, rows: usize) {
    let painter = ui.painter().with_clip_rect(gutter);
    let first = ((offset - TOP) / metrics.row).floor().max(0.0) as usize;
    let last = ((((offset + gutter.height()) - TOP) / metrics.row).ceil().max(0.0) as usize + 1).min(rows);
    let head = editor.sel.head.line;
    let font = FontId::new((metrics.font.size * 0.92).round(), metrics.font.family.clone());
    for index in first..last {
        let row = row_at(editor, metrics, index);
        if !row.first {
            continue;
        }
        let y = gutter.top() + TOP + index as f32 * metrics.row - offset;
        let color = if row.line == head { theme.gutter_current } else { theme.gutter_text };
        painter.text(
            pos2(gutter.right() - 14.0, y + metrics.row / 2.0),
            egui::Align2::RIGHT_CENTER,
            (row.line + 1).to_string(),
            font.clone(),
            color,
        );
    }
}

/// The text area driven frame by frame, as the app draws it: typing,
/// scrolling and jumping through a big file.
#[cfg(test)]
pub mod tests {
    use std::time::Instant;

    use egui::{Context, Event, Modifiers, Pos2, RawInput};

    use super::*;
    use crate::theme::Palette;

    /// A file of code of about `bytes` bytes.
    pub fn big_code(bytes: usize) -> String {
        let mut text = String::with_capacity(bytes + 512);
        let mut i = 0;
        while text.len() < bytes {
            text.push_str(&format!(
                "/// Documentation for item {i}, with a few words to read.\n\
                 pub fn function_{i}(value: &str, count: usize) -> Result<Vec<String>, Error> {{\n    \
                 let mut out = Vec::with_capacity(count); // comment {i}\n    \
                 for n in 0..count {{ out.push(format!(\"{{}}-{{}}\", value, n * {i})); }}\n    \
                 if out.is_empty() {{ return Err(Error::new(\"empty\")); }}\n    Ok(out)\n}}\n\n"
            ));
            i += 1;
        }
        text
    }

    pub struct Harness {
        pub ctx: Context,
        pub editor: Editor,
        pub prefs: EditorPrefs,
        time: f64,
        /// How long each frame took to build, and to turn into triangles.
        pub frames: Vec<(f64, f64)>,
    }

    impl Harness {
        pub fn new(text: &str, language: &str) -> Self {
            let ctx = Context::default();
            crate::theme::install_fonts(&ctx);
            crate::theme::apply(&ctx, &Palette::dark());
            let mut harness = Self {
                ctx,
                editor: Editor::with_text(text, language),
                prefs: EditorPrefs::default(),
                time: 0.0,
                frames: Vec::new(),
            };
            // Fonts and focus settle over the first frames.
            for _ in 0..3 {
                harness.frame(Vec::new());
            }
            harness
        }

        pub fn frame(&mut self, events: Vec<Event>) -> Vec<Action> {
            let input = RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1200.0, 800.0))),
                time: Some(self.time),
                focused: true,
                events,
                ..RawInput::default()
            };
            self.time += 1.0 / 60.0;
            let mut actions = Vec::new();
            let palette = Palette::dark();
            let theme = crate::editor::syntax::theme(&self.prefs.theme, &palette);
            crate::editor::prefs::publish(&self.ctx, &self.prefs, "");
            let started = Instant::now();
            let mut output = self.ctx.run_ui(input, |ui| {
                show(ui, theme, &mut self.editor, &self.prefs, true, &mut actions);
            });
            let built = started.elapsed().as_secs_f64() * 1000.0;
            let started = Instant::now();
            let (drawn, ppp) = (std::mem::take(&mut output.shapes), output.pixels_per_point);
            output.drop_without_applying_deltas();
            let shapes = self.ctx.tessellate(drawn, ppp);
            let tessellated = started.elapsed().as_secs_f64() * 1000.0;
            std::hint::black_box(shapes);
            self.frames.push((built, tessellated));
            actions
        }

        pub fn key(&mut self, key: Key, modifiers: Modifiers) {
            self.frame(vec![Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }]);
        }

        pub fn typed(&mut self, text: &str) {
            self.frame(vec![Event::Text(text.to_owned())]);
        }
    }

    #[test]
    fn typing_scrolling_and_jumping_through_a_big_file() {
        let mut ui = Harness::new(&big_code(300_000), "Rust");
        ui.editor.view.focus = true;
        ui.frame(Vec::new());
        ui.editor.move_to(Pos::new(3_000, 4), false);
        ui.editor.view.reveal = Some(Reveal::Center);
        for c in "let x = 1;".chars() {
            ui.typed(&c.to_string());
        }
        ui.key(Key::Enter, Modifiers::NONE);
        assert_eq!(ui.editor.doc.line(3_000), "/// let x = 1;");
        for _ in 0..20 {
            ui.key(Key::PageDown, Modifiers::NONE);
        }
        ui.key(Key::End, Modifiers::COMMAND);
        ui.typed("z");
        ui.key(Key::Home, Modifiers::COMMAND);
        ui.key(Key::ArrowDown, Modifiers::SHIFT);
        ui.key(Key::Tab, Modifiers::NONE);
        assert!(ui.editor.doc.line(0).starts_with("    ///"));
        // Word wrap on: the keys still go down by pages.
        ui.prefs.wrap = true;
        let before = ui.editor.sel.head.line;
        for _ in 0..5 {
            ui.key(Key::PageDown, Modifiers::NONE);
        }
        let after = ui.editor.sel.head.line;
        assert!(after > before + 50 && after < before + 400, "five pages: {before} → {after}");
        assert!(ui.editor.view.offset.y > 0.0);
        assert!(ui.editor.is_dirty());
    }
}

/// Frame times on a file of 2 MB, in a release build:
/// `cargo test --release editor_frame_times -- --ignored --nocapture`.
#[cfg(test)]
mod perf {
    use egui::Modifiers;

    use super::tests::{Harness, big_code};
    use super::*;

    fn summary(name: &str, frames: &[(f64, f64)]) {
        let mut total: Vec<f64> = frames.iter().map(|(built, drawn)| built + drawn).collect();
        let mut built: Vec<f64> = frames.iter().map(|(built, _)| *built).collect();
        total.sort_by(f64::total_cmp);
        built.sort_by(f64::total_cmp);
        let at = |list: &[f64], share: f64| list[((list.len() - 1) as f64 * share).round() as usize];
        let mean = total.iter().sum::<f64>() / total.len() as f64;
        println!(
            "{name}: {} frames, built+tessellated mean {mean:.2} ms, median {:.2}, p95 {:.2}, max {:.2} (built only: median {:.2}, p95 {:.2})",
            total.len(),
            at(&total, 0.5),
            at(&total, 0.95),
            at(&total, 1.0),
            at(&built, 0.5),
            at(&built, 0.95),
        );
    }

    #[test]
    #[ignore]
    fn editor_frame_times() {
        let text = big_code(2_000_000);
        let started = std::time::Instant::now();
        let mut ui = Harness::new(&text, "Rust");
        println!("{} lines, {} bytes, opened and first frames in {:?}", ui.editor.doc.line_count(), text.len(), started.elapsed());
        ui.editor.view.focus = true;
        ui.frames.clear();
        for _ in 0..30 {
            ui.frame(Vec::new());
        }
        summary("idle at the top", &ui.frames);

        // Typing in the middle of the file, before the thread got there.
        ui.editor.move_to(Pos::new(25_000, 4), false);
        ui.editor.view.reveal = Some(Reveal::Center);
        ui.frame(Vec::new());
        ui.frames.clear();
        for index in 0..300 {
            if index % 40 == 39 {
                ui.key(Key::Enter, Modifiers::NONE);
            } else {
                ui.typed(&((b'a' + (index % 26) as u8) as char).to_string());
            }
        }
        summary("typing at line 25 000", &ui.frames);

        ui.frames.clear();
        for _ in 0..200 {
            ui.key(Key::PageDown, Modifiers::NONE);
        }
        summary("page down x200", &ui.frames);

        ui.frames.clear();
        for _ in 0..200 {
            ui.key(Key::ArrowUp, Modifiers::NONE);
        }
        summary("arrow up x200", &ui.frames);

        ui.frames.clear();
        ui.key(Key::End, Modifiers::COMMAND);
        ui.key(Key::Home, Modifiers::COMMAND);
        summary("jump to the end and back", &ui.frames);

        ui.prefs.wrap = true;
        ui.frames.clear();
        for _ in 0..100 {
            ui.key(Key::PageDown, Modifiers::NONE);
        }
        summary("page down x100, word wrap", &ui.frames);
        println!("  cursor now at line {}, view at {:.0} points", ui.editor.sel.head.line + 1, ui.editor.view.offset.y);

        // The whole file read in the end.
        let started = std::time::Instant::now();
        while ui.editor.highlight.busy() {
            ui.frame(Vec::new());
        }
        println!("reading finished {:?} after the last step", started.elapsed());
    }
}
