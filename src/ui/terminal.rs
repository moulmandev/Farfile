//! Draws a [`Terminal`] and forwards the keyboard and the mouse to it.

use egui::{
    Align2, Color32, CursorIcon, Event, EventFilter, FontId, Key, MouseWheelUnit, PointerButton,
    Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Ui, UiBuilder, ViewportCommand, pos2, vec2,
};
use vt100::MouseProtocolMode;

use super::Action;
use super::terminal_ext::{self as ext, Marks};
use super::widgets::{icon, muted};
use crate::ssh_power::broadcast::{Typed, type_in};
use crate::terminal::{Cell, MouseButton, MouseEvent, Status, Terminal, encode_key};
use crate::terminal_ext::fonts::{self, TermFonts};
use crate::terminal_ext::look::{self, CursorShape, Scheme, TerminalPrefs};
use crate::theme::{Palette, RADIUS};

const PADDING: f32 = 8.0;
const MIN_COLS: f32 = 20.0;
const MIN_ROWS: f32 = 4.0;
/// Rows scrolled per arrow key when a full-screen program has the wheel.
const WHEEL_ARROWS: i32 = 3;
/// Seconds the cursor stays lit, then dark, when it blinks.
const BLINK: f64 = 0.53;

pub(crate) struct Colors {
    pub background: Color32,
    text: Color32,
    /// Default text in bold.
    strong: Color32,
    cursor: Color32,
    selection: Color32,
    /// The 16 ANSI colours: 8 normal, then 8 bright.
    ansi: [Color32; 16],
    /// Search results, the one being shown, and the text on it.
    found: Color32,
    found_current: Color32,
    found_text: Color32,
    link: Color32,
}

const fn rgb(value: u32) -> Color32 {
    Color32::from_rgb((value >> 16) as u8, (value >> 8) as u8, value as u8)
}

fn mix(from: Color32, to: Color32, amount: f32) -> Color32 {
    let channel = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * amount).round() as u8;
    Color32::from_rgb(channel(from.r(), to.r()), channel(from.g(), to.g()), channel(from.b(), to.b()))
}

impl Colors {
    pub(crate) fn new(scheme: &Scheme) -> Self {
        let background = rgb(scheme.background);
        // Yellow for every result and orange for the one shown, as in
        // browsers, whatever the scheme.
        let found = if scheme.dark {
            mix(background, rgb(0xffcc33), 0.40)
        } else {
            mix(background, rgb(0xffd400), 0.50)
        };
        let ansi = scheme.ansi.map(rgb);
        Self {
            background,
            text: rgb(scheme.foreground),
            strong: rgb(scheme.bold),
            cursor: rgb(scheme.cursor),
            selection: rgb(scheme.selection),
            ansi,
            found,
            found_current: rgb(0xff9f1c),
            found_text: rgb(0x111111),
            link: ansi[12],
        }
    }

    fn indexed(&self, index: u8) -> Color32 {
        match index {
            0..=15 => self.ansi[usize::from(index)],
            // The 6×6×6 colour cube.
            16..=231 => {
                let level = |step: u8| if step == 0 { 0 } else { 55 + step * 40 };
                let index = index - 16;
                Color32::from_rgb(level(index / 36), level(index / 6 % 6), level(index % 6))
            }
            // The grey ramp.
            232..=255 => Color32::from_gray(8 + (index - 232) * 10),
        }
    }

    fn foreground(&self, cell: &vt100::Cell) -> Color32 {
        let color = match cell.fgcolor() {
            vt100::Color::Default if cell.bold() => self.strong,
            vt100::Color::Default => self.text,
            // Bold on a basic colour is traditionally its bright variant.
            vt100::Color::Idx(index @ 0..=7) if cell.bold() => self.indexed(index + 8),
            vt100::Color::Idx(index) => self.indexed(index),
            vt100::Color::Rgb(r, g, b) => Color32::from_rgb(r, g, b),
        };
        if cell.dim() { color.gamma_multiply(0.6) } else { color }
    }

    fn background(&self, cell: &vt100::Cell) -> Option<Color32> {
        match cell.bgcolor() {
            vt100::Color::Default => None,
            vt100::Color::Idx(index) => Some(self.indexed(index)),
            vt100::Color::Rgb(r, g, b) => Some(Color32::from_rgb(r, g, b)),
        }
    }
}

/// How the screen is drawn: colours, fonts, and the cursor this frame.
pub(crate) struct Look<'a> {
    pub colors: &'a Colors,
    pub fonts: &'a TermFonts,
    pub cursor: CursorShape,
    /// Lit this frame: false while blinking off.
    pub cursor_lit: bool,
    pub focused: bool,
}

/// A stretch of cells drawn in one go: same colours, plain ASCII.
struct Run {
    col: u16,
    width: u16,
    text: String,
    foreground: Color32,
    background: Option<Color32>,
    underline: bool,
    bold: bool,
}

pub(crate) struct Grid {
    pub origin: Pos2,
    pub cell: egui::Vec2,
    pub cols: u16,
    pub rows: u16,
}

impl Grid {
    fn rect(&self, row: u16, col: u16, width: u16) -> Rect {
        let min = self.origin + vec2(f32::from(col) * self.cell.x, f32::from(row) * self.cell.y);
        Rect::from_min_size(min, vec2(f32::from(width) * self.cell.x, self.cell.y))
    }

    pub fn cell_at(&self, pos: Pos2) -> Cell {
        let col = ((pos.x - self.origin.x) / self.cell.x).floor();
        let row = ((pos.y - self.origin.y) / self.cell.y).floor();
        (
            row.clamp(0.0, f32::from(self.rows - 1)) as u16,
            col.clamp(0.0, f32::from(self.cols - 1)) as u16,
        )
    }
}

/// The size of a cell in this font.
pub(crate) fn cell_size(ctx: &egui::Context, font: &FontId) -> egui::Vec2 {
    ctx.fonts_mut(|fonts| vec2(fonts.glyph_width(font, 'M'), fonts.row_height(font)))
}

/// Draws the terminal in all the space left. `interactive` is false while a
/// dialog is open: the shell then ignores the keyboard.
pub fn show(ui: &mut Ui, p: &Palette, terminal: &mut Terminal, interactive: bool, actions: &mut Vec<Action>) {
    let prefs = look::current(ui.ctx());
    let colors = Colors::new(look::scheme(&prefs.scheme, p.dark));
    let fonts = fonts::terminal_fonts(ui.ctx(), &prefs.font, prefs.font_size);
    let cell = cell_size(ui.ctx(), &fonts.regular);

    let (outer, response) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
    ui.painter().rect_filled(outer, RADIUS, colors.background);
    let area = outer.shrink(PADDING);
    let grid = Grid {
        origin: area.min,
        cell,
        cols: (area.width() / cell.x).floor().max(MIN_COLS) as u16,
        rows: (area.height() / cell.y).floor().max(MIN_ROWS) as u16,
    };
    terminal.resize(grid.cols, grid.rows);

    let wants_focus = std::mem::take(&mut terminal.wants_focus);
    if interactive && (wants_focus || response.clicked() || response.drag_started()) {
        response.request_focus();
    }
    let focused = interactive && response.has_focus();
    terminal.focused = focused;
    if focused {
        // Keep the keys egui would use to move the focus.
        ui.memory_mut(|memory| {
            memory.set_focus_lock_filter(
                response.id,
                EventFilter {
                    tab: true,
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    escape: true,
                },
            );
        });
    }

    let status = terminal.status();
    let mut program_has_mouse = false;
    let mut link = None;
    if status == Status::Open {
        let hover = ext::before_input(ui, &response, &grid, terminal, &prefs, focused, interactive, actions);
        link = hover.link;
        if !hover.took_pointer {
            program_has_mouse = pointer(ui, &response, &grid, terminal, &prefs, interactive);
        }
        if focused {
            keyboard(ui, &grid, terminal);
        }
    }
    let marks = ext::before_paint(ui, &grid, terminal, link.as_ref());

    let now = ui.input(|input| input.time);
    let painter = ui.painter().with_clip_rect(outer);
    let selection = terminal.selection_range();
    let active_at = terminal.extras.active_at;
    let scrolled = terminal.with_screen(|screen| {
        let (cursor, cursor_lit, next) = cursor_state(screen, &prefs, focused, now - active_at);
        if let Some(next) = next {
            ui.ctx().request_repaint_after(std::time::Duration::from_secs_f64(next));
        }
        let look = Look {
            colors: &colors,
            fonts: &fonts,
            cursor,
            cursor_lit,
            focused,
        };
        paint_screen(&painter, &look, &grid, screen, selection, &marks);
        screen.scrollback()
    });
    ext::overlays(ui, p, outer, &response, &grid, terminal, scrolled, actions);

    if status != Status::Open {
        overlay(ui, p, outer, terminal, &status, actions);
    }
    super::ssh_power::terminal_outline(ui, p, outer, terminal);
    response.on_hover_cursor(if link.is_some() {
        CursorIcon::PointingHand
    } else if program_has_mouse {
        CursorIcon::Default
    } else {
        CursorIcon::Text
    });
}

/// The shape of the cursor, whether it is lit now, and in how many seconds
/// that changes when it blinks. A program may ask for a shape of its own
/// (vim in insert mode, for one); otherwise the preferences decide. It
/// stays lit while the user types or output comes (`idle` seconds ago).
pub(crate) fn cursor_state(
    screen: &vt100::Screen,
    prefs: &TerminalPrefs,
    focused: bool,
    idle: f64,
) -> (CursorShape, bool, Option<f64>) {
    use vt100::CursorStyle as Style;
    let (shape, blink) = match screen.cursor_style() {
        Style::Default => (prefs.cursor, prefs.cursor_blink),
        Style::BlinkBlock => (CursorShape::Block, true),
        Style::SteadyBlock => (CursorShape::Block, false),
        Style::BlinkUnderline => (CursorShape::Underline, true),
        Style::SteadyUnderline => (CursorShape::Underline, false),
        Style::BlinkVerticalBar => (CursorShape::Bar, true),
        Style::SteadyVerticalBar => (CursorShape::Bar, false),
    };
    if !blink || !focused || screen.hide_cursor() {
        return (shape, true, None);
    }
    let idle = idle.max(0.0);
    let phase = idle % (2.0 * BLINK);
    let lit = phase < BLINK;
    let next = if lit { BLINK - phase } else { 2.0 * BLINK - phase };
    (shape, lit, Some(next + 0.01))
}

/// Selection, copy and paste with the mouse, and the wheel, unless the
/// running program takes the mouse. Returns whether it does.
fn pointer(
    ui: &Ui,
    response: &egui::Response,
    grid: &Grid,
    terminal: &mut Terminal,
    prefs: &TerminalPrefs,
    interactive: bool,
) -> bool {
    if interactive && report_mouse(ui, response, grid, terminal) {
        return true;
    }

    if response.drag_started() {
        let origin = ui.input(|input| input.pointer.press_origin());
        if let Some(pos) = origin {
            let cell = grid.cell_at(pos);
            terminal.selection = Some((cell, cell));
        }
    } else if response.dragged() {
        if let (Some(pos), Some((start, _))) = (response.interact_pointer_pos(), terminal.selection) {
            terminal.selection = Some((start, grid.cell_at(pos)));
        }
    } else if response.triple_clicked() || response.double_clicked() {
        // A word, then the whole row, as in other terminals.
        if let Some(pos) = response.interact_pointer_pos() {
            terminal.selection = ext::word_or_row(terminal, grid.cell_at(pos), response.triple_clicked(), grid.cols);
            if prefs.copy_on_select
                && let Some(text) = terminal.selected_text()
            {
                ui.ctx().copy_text(text);
            }
        }
    } else if response.clicked() {
        terminal.selection = None;
    }
    if response.drag_stopped()
        && prefs.copy_on_select
        && let Some(text) = terminal.selected_text()
    {
        ui.ctx().copy_text(text);
    }

    // Right click opens the menu; or, if the user prefers, copies the
    // selection or pastes when there is none, Shift+right click being the
    // way to the menu.
    if response.secondary_clicked() {
        let shift = ui.input(|input| input.modifiers.shift);
        if prefs.right_click_paste && !shift {
            match terminal.selected_text() {
                Some(text) => {
                    ui.ctx().copy_text(text);
                    terminal.selection = None;
                }
                None => {
                    response.request_focus();
                    ui.ctx().send_viewport_cmd(ViewportCommand::RequestPaste);
                }
            }
        } else {
            terminal.extras.open_menu = true;
        }
    }

    if response.contains_pointer() {
        let delta = ui.input(|input| input.smooth_scroll_delta.y);
        if delta != 0.0 {
            terminal.wheel += delta / grid.cell.y;
            let rows = terminal.wheel.trunc() as i32;
            terminal.wheel -= rows as f32;
            if rows != 0 {
                wheel(terminal, rows);
            }
        }
    }
    false
}

/// Passes the mouse on to programs that asked for it (htop, mc, vim…), as
/// xterm does: clicks, the wheel, and moves when they want them. Shift keeps
/// the mouse here to select text. Returns whether the program has it.
fn report_mouse(ui: &Ui, response: &egui::Response, grid: &Grid, terminal: &mut Terminal) -> bool {
    if terminal.mouse_mode() == MouseProtocolMode::None {
        terminal.mouse_held = None;
        return false;
    }
    let (events, held, pointer) = ui.input(|input| {
        (input.events.clone(), input.modifiers, input.pointer.latest_pos())
    });
    let line = ui.ctx().options(|options| options.input_options.line_scroll_speed);
    // A drag the program already heard about stays with it.
    if held.shift && terminal.mouse_held.is_none() {
        return false;
    }
    let over = |pos: Pos2| response.contains_pointer() && response.rect.contains(pos);

    for event in events {
        match event {
            Event::PointerMoved(pos) if terminal.mouse_held.is_some() || over(pos) => {
                terminal.mouse(MouseEvent::Motion(terminal.mouse_held), grid.cell_at(pos), held);
            }
            Event::PointerButton {
                pos,
                button,
                pressed,
                modifiers,
            } => {
                let button = match button {
                    PointerButton::Primary => MouseButton::Left,
                    PointerButton::Middle => MouseButton::Middle,
                    PointerButton::Secondary => MouseButton::Right,
                    _ => continue,
                };
                if pressed {
                    // Presses start over the terminal; others may follow
                    // anywhere while one is held.
                    if terminal.mouse_held.is_none() {
                        if !over(pos) {
                            continue;
                        }
                        terminal.mouse_held = Some(button);
                    }
                    terminal.mouse(MouseEvent::Press(button), grid.cell_at(pos), modifiers);
                } else if terminal.mouse_held.is_some() {
                    if terminal.mouse_held == Some(button) {
                        terminal.mouse_held = None;
                    }
                    terminal.mouse(MouseEvent::Release(button), grid.cell_at(pos), modifiers);
                }
            }
            Event::MouseWheel {
                unit,
                delta,
                modifiers,
                ..
            } => {
                let Some(pos) = pointer.filter(|&pos| over(pos)) else {
                    continue;
                };
                // One report per notch of the wheel, as xterm sends.
                terminal.wheel += match unit {
                    MouseWheelUnit::Line => delta.y,
                    MouseWheelUnit::Point => delta.y / line,
                    MouseWheelUnit::Page => delta.y * response.rect.height() / line,
                };
                let notches = terminal.wheel.trunc() as i32;
                terminal.wheel -= notches as f32;
                let button = if notches > 0 { MouseButton::WheelUp } else { MouseButton::WheelDown };
                for _ in 0..notches.unsigned_abs() {
                    terminal.mouse(MouseEvent::Press(button), grid.cell_at(pos), modifiers);
                }
            }
            _ => {}
        }
    }
    // A release lost outside the window must not leave a button stuck.
    if !ui.input(|input| input.pointer.any_down()) {
        terminal.mouse_held = None;
    }
    true
}

/// `rows` is positive when scrolling towards older lines.
fn wheel(terminal: &mut Terminal, rows: i32) {
    let (alternate, application) =
        terminal.with_screen(|screen| (screen.alternate_screen(), screen.application_cursor()));
    if !alternate {
        terminal.scroll(rows);
        return;
    }
    // Full-screen programs (less, vim…) have no history: give them arrows.
    let key = if rows > 0 { Key::ArrowUp } else { Key::ArrowDown };
    if let Some(bytes) = encode_key(key, egui::Modifiers::NONE, application) {
        let count = (rows.abs() * WHEEL_ARROWS) as usize;
        terminal.send(bytes.repeat(count));
    }
}

fn keyboard(ui: &Ui, grid: &Grid, terminal: &mut Terminal) {
    let (events, held) = ui.input(|input| (input.events.clone(), input.modifiers));
    for event in events {
        match event {
            Event::Text(text) => {
                // Alt+key is "meta": the key, preceded by escape.
                if held.alt && !held.ctrl {
                    type_in(terminal, Typed::Bytes(format!("\x1b{text}").into_bytes()));
                } else {
                    type_in(terminal, Typed::Text(text));
                }
            }
            Event::Paste(text) => type_in(terminal, Typed::Paste(text)),
            // Ctrl+C copies when something is selected, interrupts otherwise.
            // Ctrl+Shift+C only ever copies.
            Event::Copy => match terminal.selected_text() {
                Some(text) => {
                    ui.ctx().copy_text(text);
                    terminal.selection = None;
                }
                None if !held.shift => type_in(terminal, Typed::Bytes(vec![0x03])),
                None => {}
            },
            Event::Cut => type_in(terminal, Typed::Bytes(vec![0x18])),
            Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } => {
                let page = i32::from(grid.rows.saturating_sub(1).max(1));
                match key {
                    Key::PageUp if modifiers.shift => terminal.scroll(page),
                    Key::PageDown if modifiers.shift => terminal.scroll(-page),
                    // Encoded by `type_in` for each terminal it reaches.
                    _ => type_in(terminal, Typed::Key(key, modifiers)),
                }
            }
            _ => {}
        }
    }
}

pub(crate) fn paint_screen(
    painter: &egui::Painter,
    look: &Look<'_>,
    grid: &Grid,
    screen: &vt100::Screen,
    selection: Option<(Cell, Cell)>,
    marks: &Marks,
) {
    let colors = look.colors;
    let selected = |row: u16, col: u16| {
        marks.all_selected || selection.is_some_and(|(start, end)| (row, col) >= start && (row, col) <= end)
    };

    let flush = |row: u16, run: &mut Option<Run>| {
        let Some(run) = run.take() else {
            return;
        };
        let rect = grid.rect(row, run.col, run.width);
        if let Some(background) = run.background {
            painter.rect_filled(rect, 0, background);
        }
        let font = match &look.fonts.bold {
            Some(bold) if run.bold => bold,
            _ => &look.fonts.regular,
        };
        if !run.text.trim_end().is_empty() {
            painter.text(rect.min, Align2::LEFT_TOP, &run.text, font.clone(), run.foreground);
        }
        if run.underline {
            let y = rect.bottom() - 1.5;
            painter.hline(rect.x_range(), y, Stroke::new(1.0, run.foreground));
        }
    };

    for row in 0..grid.rows {
        let found = marks.found.get(usize::from(row)).map_or(&[][..], Vec::as_slice);
        let mut run: Option<Run> = None;
        for col in 0..grid.cols {
            let Some(cell) = screen.cell(row, col) else {
                break;
            };
            // The second half of a wide character belongs to the first.
            if cell.is_wide_continuation() {
                continue;
            }

            let mut foreground = colors.foreground(cell);
            let mut background = colors.background(cell);
            let mut underline = cell.underline();
            if cell.inverse() {
                (foreground, background) = (background.unwrap_or(colors.background), Some(foreground));
            }
            if marks.link.iter().any(|&(at, from, to)| at == usize::from(row) && (from..to).contains(&col)) {
                foreground = colors.link;
                underline = true;
            }
            match found.iter().find(|(from, to, _)| (*from..*to).contains(&col)) {
                Some((_, _, true)) => {
                    background = Some(colors.found_current);
                    foreground = colors.found_text;
                }
                Some(_) => background = Some(colors.found),
                None => {}
            }
            if selected(row, col) {
                background = Some(colors.selection);
            }
            let bold = cell.bold() && look.fonts.bold.is_some();
            let contents = cell.contents();
            let width = if cell.is_wide() { 2 } else { 1 };
            let plain = contents.len() <= 1 && contents.is_ascii();

            // Anything but plain ASCII is drawn alone, on its own cell, so
            // a glyph of an unexpected width cannot shift what follows.
            let joins = plain
                && run.as_ref().is_some_and(|run| {
                    run.foreground == foreground
                        && run.background == background
                        && run.underline == underline
                        && run.bold == bold
                        && run.col + run.width == col
                });
            if joins {
                if let Some(run) = &mut run {
                    run.text.push_str(if contents.is_empty() { " " } else { contents });
                    run.width += 1;
                }
                continue;
            }

            flush(row, &mut run);
            run = Some(Run {
                col,
                width,
                text: if contents.is_empty() { " ".to_owned() } else { contents.to_owned() },
                foreground,
                background,
                underline,
                bold,
            });
            if !plain {
                flush(row, &mut run);
            }
        }
        flush(row, &mut run);
    }

    // The cursor is hidden while looking at the history.
    if screen.hide_cursor() || screen.scrollback() > 0 || !look.cursor_lit {
        return;
    }
    let (row, col) = screen.cursor_position();
    if row >= grid.rows || col >= grid.cols {
        return;
    }
    let rect = grid.rect(row, col, 1);
    let thin = (grid.cell.x * 0.14).clamp(1.5, 3.0);
    match look.cursor {
        CursorShape::Block if look.focused => {
            painter.rect_filled(rect, 1, colors.cursor);
            if let Some(cell) = screen.cell(row, col)
                && !cell.contents().trim().is_empty()
            {
                painter.text(
                    rect.min,
                    Align2::LEFT_TOP,
                    cell.contents(),
                    look.fonts.regular.clone(),
                    colors.background,
                );
            }
        }
        CursorShape::Block => {
            painter.rect_stroke(rect, 1, Stroke::new(1.0, colors.cursor), StrokeKind::Inside);
        }
        CursorShape::Bar => {
            let bar = Rect::from_min_size(rect.min, vec2(thin, rect.height()));
            painter.rect_filled(bar, 0, colors.cursor.gamma_multiply(if look.focused { 1.0 } else { 0.5 }));
        }
        CursorShape::Underline => {
            let line = Rect::from_min_max(pos2(rect.left(), rect.bottom() - thin), rect.max);
            painter.rect_filled(line, 0, colors.cursor.gamma_multiply(if look.focused { 1.0 } else { 0.5 }));
        }
    }
}

/// What is shown over the screen while connecting and once the shell ended.
fn overlay(
    ui: &mut Ui,
    p: &Palette,
    rect: Rect,
    terminal: &Terminal,
    status: &Status,
    actions: &mut Vec<Action>,
) {
    let height = if *status == Status::Connecting { 80.0 } else { 132.0 };
    let band = Rect::from_center_size(
        pos2(rect.center().x, rect.center().y),
        vec2((rect.width() - 40.0).min(520.0), height),
    );
    ui.painter().rect_filled(band, 10, p.bg.gamma_multiply(0.94));
    ui.painter()
        .rect_stroke(band, 10, Stroke::new(1.0, p.border), StrokeKind::Inside);

    let mut content = ui.new_child(UiBuilder::new().max_rect(band.shrink(12.0)));
    content.vertical_centered(|ui| match status {
        Status::Open => {}
        Status::Connecting => {
            ui.add_space(6.0);
            ui.add(egui::Spinner::new().size(22.0).color(p.accent));
            ui.add_space(4.0);
            ui.label(muted(p, terminal.starting_text()));
        }
        Status::Failed(_) | Status::Closed(_) => {
            let (failed, title, detail) = terminal.ending(status);
            let (glyph, tint) = match (failed, terminal.is_local()) {
                (true, _) => (icon::WARNING_CIRCLE, p.danger),
                (false, true) => (icon::TERMINAL_WINDOW, p.muted),
                (false, false) => (icon::PLUGS, p.muted),
            };
            ui.label(RichText::new(format!("{glyph}  {title}")).color(tint));
            ui.add(egui::Label::new(muted(p, detail)).truncate());
            ui.add_space(6.0);
            if ui
                .button(format!("{}  {}", icon::ARROWS_CLOCKWISE, terminal.restart_label()))
                .clicked()
            {
                actions.push(Action::ReconnectTerminal);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cursor_blinks_only_when_asked_and_focused() {
        let mut parser = vt100::Parser::new(std::num::NonZeroU16::MIN, std::num::NonZeroU16::MIN, 0);
        let mut prefs = TerminalPrefs::default();
        assert_eq!(cursor_state(parser.screen(), &prefs, true, 5.0), (CursorShape::Block, true, None));
        prefs.cursor_blink = true;
        prefs.cursor = CursorShape::Bar;
        let (shape, lit, next) = cursor_state(parser.screen(), &prefs, true, 0.1);
        assert_eq!((shape, lit), (CursorShape::Bar, true));
        assert!(next.is_some_and(|next| (next - (BLINK - 0.1 + 0.01)).abs() < 1e-9));
        assert!(!cursor_state(parser.screen(), &prefs, true, BLINK + 0.1).1);
        assert!(cursor_state(parser.screen(), &prefs, false, BLINK + 0.1).1, "unfocused: steady");
        // A program asking for a steady underline wins.
        parser.process(b"\x1b[4 q");
        assert_eq!(cursor_state(parser.screen(), &prefs, true, BLINK + 0.1), (CursorShape::Underline, true, None));
    }

    #[test]
    fn every_scheme_reads_well() {
        // The text stands out from the background in every scheme.
        for scheme in &look::SCHEMES {
            let colors = Colors::new(scheme);
            let luminance = |c: Color32| 0.2126 * f32::from(c.r()) + 0.7152 * f32::from(c.g()) + 0.0722 * f32::from(c.b());
            let contrast = (luminance(colors.text) - luminance(colors.background)).abs();
            assert!(contrast > 90.0, "{}: {contrast}", scheme.name);
            assert_ne!(colors.found, colors.background, "{}", scheme.name);
        }
    }
}
