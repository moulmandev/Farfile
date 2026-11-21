//! What a terminal shows and handles besides the screen itself: the search
//! bar, links under Ctrl, the menu, the recording badge, the zoom keys, and
//! the confirmation of multi-line pastes. Called from `ui::terminal`.

use egui::containers::SetOpenCommand;
use egui::{
    Align, Align2, Button, Event, FontId, Id, Key, Layout, MouseWheelUnit, Popup, Rect,
    RichText, Sense, Stroke, StrokeKind, TextEdit, Ui, UiBuilder, ViewportCommand, pos2, vec2,
};

use super::Action;
use super::terminal::Grid;
use super::widgets::{checkbox, icon, icon_button};
use crate::profiles::Profile;
use crate::tabs::Tab;
use crate::terminal::{Cell, Terminal};
use crate::terminal_ext::links::{self, Link};
use crate::terminal_ext::look::TerminalPrefs;
use crate::terminal_ext::search::Search;
use crate::terminal_ext::text;
use crate::theme::Palette;

/// What the terminals ask the app for.
pub enum TerminalAction {
    /// Change the font size by this many points; 0 brings back the default.
    Zoom(i32),
    /// Open the dialog of the appearance of terminals.
    OpenLook,
    /// Paste into the terminal of this id, asking first if it holds several
    /// lines and the user wants to be asked.
    AskPaste { terminal: u64, text: String },
    /// Ask where to record the session of this terminal, and start.
    StartLog { terminal: u64 },
    StopLog { terminal: u64 },
    /// Open a web address in the browser.
    OpenLink(String),
    /// Show this message, as a confirmation.
    Notice(String),
    /// From the tab menu: show this tab and the search bar of its terminal.
    Search { tab: u64 },
}

/// Text that would run several commands if pasted as is.
pub fn is_multiline(text: &str) -> bool {
    text.trim_end_matches(['\r', '\n']).contains(['\r', '\n'])
}

/// Pastes, unless the text holds several lines and the user wants to
/// confirm those: the app then asks.
pub fn paste(terminal: &mut Terminal, prefs: &TerminalPrefs, text: String, actions: &mut Vec<Action>) {
    if prefs.confirm_paste && is_multiline(&text) {
        actions.push(Action::Terminal(TerminalAction::AskPaste {
            terminal: terminal.extras.id,
            text,
        }));
    } else {
        terminal.paste(&text);
    }
}

/// The link under the pointer while Ctrl is held, and whether it took the
/// pointer away from selection and from the program.
pub struct Hover {
    pub link: Option<Link>,
    pub took_pointer: bool,
}

/// Scripts cannot hold Ctrl in the off-screen window: `termctrl` steps
/// make links react as if it were held.
pub fn dev_ctrl_id() -> Id {
    Id::new("terminal-dev-ctrl")
}

/// Before the terminal sees the input: its own shortcuts, pastes to
/// confirm, Ctrl+wheel, and links under Ctrl.
#[allow(clippy::too_many_arguments)]
pub fn before_input(
    ui: &Ui,
    response: &egui::Response,
    grid: &Grid,
    terminal: &mut Terminal,
    prefs: &TerminalPrefs,
    focused: bool,
    interactive: bool,
    actions: &mut Vec<Action>,
) -> Hover {
    let now = ui.input(|input| input.time);
    // Output keeps the cursor lit, as typing does.
    let received = terminal.extras.tap.received();
    if received != terminal.extras.seen {
        terminal.extras.seen = received;
        terminal.extras.active_at = now;
    }
    if !interactive {
        return Hover {
            link: None,
            took_pointer: false,
        };
    }
    if focused {
        shortcuts(ui, terminal, prefs, actions);
    }
    if response.clicked() || response.drag_started() {
        terminal.extras.all_selected = false;
    }

    // Ctrl+wheel changes the size of the text, rather than reaching the
    // program or the history.
    if response.contains_pointer() {
        let line = ui.ctx().options(|options| options.input_options.line_scroll_speed);
        let mut zoom = 0.0;
        ui.ctx().input_mut(|input| {
            input.events.retain(|event| match event {
                Event::MouseWheel {
                    unit, delta, modifiers, ..
                } if modifiers.ctrl || modifiers.command => {
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
            let extras = &mut terminal.extras;
            extras.zoom_wheel += zoom;
            let steps = extras.zoom_wheel.trunc() as i32;
            extras.zoom_wheel -= steps as f32;
            if steps != 0 {
                actions.push(Action::Terminal(TerminalAction::Zoom(steps)));
            }
        }
    }

    let ctrl = ui.input(|input| input.modifiers.ctrl || input.modifiers.command)
        || ui.ctx().data(|data| data.get_temp::<bool>(dev_ctrl_id())).unwrap_or(false);
    let pointer = response.hover_pos().or_else(|| response.interact_pointer_pos());
    let link = match pointer {
        Some(pos) if ctrl => link_at(terminal, grid.cell_at(pos)),
        _ => None,
    };
    let Some(link) = link else {
        return Hover {
            link: None,
            took_pointer: false,
        };
    };
    if response.clicked() {
        actions.push(Action::Terminal(TerminalAction::OpenLink(link.url.clone())));
    }
    response.clone().on_hover_text_at_pointer(format!("{}\nCtrl+clic pour ouvrir", link.url));
    Hover {
        link: Some(link),
        took_pointer: true,
    }
}

fn link_at(terminal: &Terminal, cell: Cell) -> Option<Link> {
    let rows = terminal.with_screen(text::visible);
    links::at(&rows, cell)
}

/// Takes a press of Ctrl+`wanted` out of the input. Unlike egui's
/// shortcuts, Alt must not be held: AltGr comes as Ctrl+Alt, and AltGr+à
/// or AltGr+= type `@` and `}` on a French keyboard. `shift` is required,
/// refused, or (None) either.
fn take_ctrl(input: &mut egui::InputState, wanted: Key, shift: Option<bool>) -> bool {
    let before = input.events.len();
    input.events.retain(|event| {
        !matches!(
            event,
            Event::Key { key, pressed: true, modifiers, .. }
                if *key == wanted
                    && modifiers.ctrl
                    && !modifiers.alt
                    && shift.is_none_or(|shift| modifiers.shift == shift)
        )
    });
    input.events.len() != before
}

/// The keys the terminal keeps for itself, taken out of the input so the
/// shell never sees them.
fn shortcuts(ui: &Ui, terminal: &mut Terminal, prefs: &TerminalPrefs, actions: &mut Vec<Action>) {
    let (find, select_all, bigger, smaller, reset) = ui.ctx().input_mut(|input| {
        (
            take_ctrl(input, Key::F, Some(true)),
            take_ctrl(input, Key::A, Some(true)),
            // Ctrl++ needs Shift on some keyboards; Ctrl+_ is readline's.
            take_ctrl(input, Key::Equals, None) | take_ctrl(input, Key::Plus, None),
            take_ctrl(input, Key::Minus, Some(false)),
            take_ctrl(input, Key::Num0, None),
        )
    });
    if find {
        open_search(terminal);
    }
    if select_all {
        terminal.selection = None;
        terminal.extras.all_selected = true;
    }
    for (pressed, steps) in [(bigger, 1), (smaller, -1), (reset, 0)] {
        if pressed {
            actions.push(Action::Terminal(TerminalAction::Zoom(steps)));
        }
    }

    // Pastes of several lines may need a confirmation; copying everything
    // that is selected takes the history along.
    let all_selected = terminal.extras.all_selected;
    let mut pasted = Vec::new();
    let mut copy_all = false;
    let mut typed = false;
    ui.ctx().input_mut(|input| {
        input.events.retain(|event| match event {
            Event::Paste(text) if prefs.confirm_paste && is_multiline(text) => {
                pasted.push(text.clone());
                false
            }
            Event::Copy if all_selected => {
                copy_all = true;
                false
            }
            Event::Text(_) | Event::Key { pressed: true, .. } | Event::Paste(_) => {
                typed = true;
                true
            }
            _ => true,
        });
    });
    for text in pasted {
        paste(terminal, prefs, text, actions);
    }
    if copy_all {
        ui.ctx().copy_text(all_text(terminal));
        terminal.extras.all_selected = false;
    } else if typed {
        terminal.extras.all_selected = false;
        terminal.extras.active_at = ui.input(|input| input.time);
    }
}

/// Opens the search bar, or gives it the keyboard if it is open. Text
/// selected on one row becomes what is searched.
pub fn open_search(terminal: &mut Terminal) {
    let selected = terminal
        .selected_text()
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty() && !text.contains('\n'));
    let search = terminal.extras.search.get_or_insert_with(Search::new);
    search.focus = true;
    if let Some(text) = selected {
        search.query = text;
        // The result shown stands out better than the selection would.
        terminal.selection = None;
    }
}

/// Everything the terminal holds, history first, as text.
pub fn all_text(terminal: &Terminal) -> String {
    let snapshot = terminal.with_screen(text::snapshot);
    let lines: Vec<String> = text::lines(&snapshot.rows, 0)
        .into_iter()
        .map(|line| line.text.trim_end().to_owned())
        .collect();
    lines.join("\n").trim_end().to_owned()
}

/// The word under `cell`, or the whole row: what a double or triple click
/// selects.
pub fn word_or_row(terminal: &Terminal, (row, col): Cell, whole_row: bool, cols: u16) -> Option<(Cell, Cell)> {
    if whole_row {
        return Some(((row, 0), (row, cols.saturating_sub(1))));
    }
    let rows = terminal.with_screen(text::visible);
    let (first, last) = word_at(&rows.get(usize::from(row))?.text, col)?;
    Some(((row, first), (row, last)))
}

/// The first and last columns of the word at `col` of a row. Paths, URLs
/// and options hold together; brackets, quotes and spaces end a word.
fn word_at(text: &str, col: u16) -> Option<(u16, u16)> {
    let separator = |c: char| c.is_whitespace() || "()[]{}<>'\"`,;|│".contains(c);
    let chars: Vec<(char, u16, u16)> = text::columns(text).map(|(_, c, at, width)| (c, at, width)).collect();
    let index = chars.iter().position(|&(_, at, width)| at <= col && col < at + width.max(1))?;
    if separator(chars[index].0) {
        return None;
    }
    let start = chars[..index].iter().rposition(|(c, ..)| separator(*c)).map_or(0, |at| at + 1);
    let end = chars[index..].iter().position(|(c, ..)| separator(*c)).map_or(chars.len(), |at| index + at);
    let (first, last) = (chars[start], chars[end - 1]);
    Some((first.1, last.1 + last.2.max(1) - 1))
}

/// Search results and the link under the pointer, on the rows in view.
#[derive(Default)]
pub struct Marks {
    pub all_selected: bool,
    /// For each row of the screen: (first column, column after the last,
    /// is the current result).
    pub found: Vec<Vec<(u16, u16, bool)>>,
    pub link: Vec<(usize, u16, u16)>,
}

/// Brings the search up to date with the terminal, scrolls to the result
/// being shown when asked, and says what to mark on screen.
pub fn before_paint(ui: &Ui, grid: &Grid, terminal: &mut Terminal, link: Option<&Link>) -> Marks {
    let mut marks = Marks {
        all_selected: terminal.extras.all_selected,
        found: Vec::new(),
        link: link.map(|link| link.spans.clone()).unwrap_or_default(),
    };
    if terminal.extras.search.is_none() {
        return marks;
    }
    let now = ui.input(|input| input.time);
    let received = terminal.extras.tap.received();
    let size = terminal.size();
    let rows = usize::from(grid.rows);

    let wants_fresh = terminal.extras.search.as_ref().is_some_and(|search| search.reveal);
    let stale = terminal.extras.search.as_ref().is_some_and(|search| {
        search.stale(received, size, if wants_fresh { None } else { Some(now) })
    });
    if stale {
        let snapshot = terminal.with_screen(text::snapshot);
        if let Some(search) = &mut terminal.extras.search {
            search.set_snapshot(snapshot, received, size, now);
        }
    } else if terminal
        .extras
        .search
        .as_ref()
        .is_some_and(|search| search.stale(received, size, None))
    {
        // More is coming: look again shortly.
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(160));
    }

    let (offset, history) = terminal.with_screen(|screen| (screen.scrollback(), screen.scrollback_len()));
    let Some(search) = &mut terminal.extras.search else {
        return marks;
    };
    let top = history.saturating_sub(offset);
    search.update(top + rows - 1);
    if std::mem::take(&mut search.reveal)
        && let Some(target) = search.reveal_offset(offset, rows)
    {
        terminal.selection = None;
        terminal.with_screen_mut(|screen| screen.set_scrollback(target));
    }
    let (offset, history) = terminal.with_screen(|screen| (screen.scrollback(), screen.scrollback_len()));
    if let Some(search) = &terminal.extras.search {
        marks.found = search.marks(history.saturating_sub(offset), grid.rows);
    }
    marks
}

/// Recently changed font size, shown for a moment over the terminals.
fn zoom_id() -> Id {
    Id::new("terminal-zoomed")
}

pub fn show_zoom(ctx: &egui::Context, size: f32) {
    let now = ctx.input(|input| input.time);
    ctx.data_mut(|data| data.insert_temp(zoom_id(), (now, size)));
}

/// What is drawn over the screen: the search bar, the badges (recording,
/// position in the history, text size) and the menu.
#[allow(clippy::too_many_arguments)]
pub fn overlays(
    ui: &mut Ui,
    p: &Palette,
    outer: Rect,
    response: &egui::Response,
    grid: &Grid,
    terminal: &mut Terminal,
    scrolled: usize,
    actions: &mut Vec<Action>,
) {
    let mut top = outer.top() + 8.0;
    if terminal.extras.search.is_some() {
        top = search_bar(ui, p, outer, terminal) + 6.0;
    }

    let right = outer.right() - 10.0;
    if let Some(path) = terminal.extras.tap.recording() {
        let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
        let badge = badge(ui, p, pos2(right, top), &format!("{}  Enregistrement", icon::RECORD), p.danger);
        let clicked = ui
            .interact(badge, Id::new(("terminal-recording", terminal.extras.id)), Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text(format!("Enregistré dans « {name} »\nCliquer pour arrêter"))
            .clicked();
        if clicked {
            actions.push(Action::Terminal(TerminalAction::StopLog {
                terminal: terminal.extras.id,
            }));
        }
        top = badge.bottom() + 6.0;
    }
    if scrolled > 0 {
        let text = format!("{}  {scrolled} lignes plus haut", icon::ARROW_UP);
        top = badge(ui, p, pos2(right, top), &text, p.muted).bottom() + 6.0;
    }
    let now = ui.input(|input| input.time);
    if let Some((at, size)) = ui.ctx().data(|data| data.get_temp::<(f64, f32)>(zoom_id()))
        && now - at < 1.2
    {
        badge(ui, p, pos2(right, top), &format!("{}  Texte {} pt", icon::TEXT_AA, format_size(size)), p.text);
        ui.ctx().request_repaint_after(std::time::Duration::from_secs_f64(1.2 - (now - at)));
    }

    context_menu(ui, response, grid, terminal, actions);
}

/// A size in points, with a decimal comma when it has a fraction.
pub fn format_size(size: f32) -> String {
    if size.fract() == 0.0 {
        format!("{size:.0}")
    } else {
        format!("{size:.1}").replace('.', ",")
    }
}

/// A small label in a rounded box, its top right corner at `anchor`.
fn badge(ui: &Ui, p: &Palette, anchor: egui::Pos2, text: &str, color: egui::Color32) -> Rect {
    let galley = ui.painter().layout_no_wrap(text.to_owned(), FontId::proportional(11.5), color);
    let size = galley.size() + vec2(16.0, 8.0);
    let rect = Rect::from_min_size(pos2(anchor.x - size.x, anchor.y), size);
    ui.painter().rect(rect, 6, p.bg.gamma_multiply(0.92), Stroke::new(1.0, p.border), StrokeKind::Inside);
    ui.painter().galley(rect.min + vec2(8.0, 4.0), galley, color);
    rect
}

/// The search bar, at the top right of the terminal. Returns its bottom.
fn search_bar(ui: &mut Ui, p: &Palette, outer: Rect, terminal: &mut Terminal) -> f32 {
    const HEIGHT: f32 = 40.0;
    let width = (outer.width() - 20.0).min(420.0);
    let rect = Rect::from_min_size(pos2(outer.right() - 10.0 - width, outer.top() + 8.0), vec2(width, HEIGHT));
    let id = terminal.extras.id;
    let Some(search) = &mut terminal.extras.search else {
        return outer.top();
    };

    ui.painter()
        .rect(rect, 9, p.surface, Stroke::new(1.0, p.border), StrokeKind::Inside);
    // The bar takes the clicks that land on it, not the terminal below.
    ui.interact(rect, Id::new(("terminal-search-bar", id)), Sense::click());
    let mut bar = ui.new_child(
        UiBuilder::new()
            .max_rect(rect.shrink2(vec2(6.0, 5.0)))
            .layout(Layout::right_to_left(Align::Center)),
    );
    bar.spacing_mut().item_spacing.x = 2.0;

    let mut close = false;
    if icon_button(&mut bar, p, icon::X, "Fermer (Échap)", true).clicked() {
        close = true;
    }
    let any = !search.found.is_empty();
    if icon_button(&mut bar, p, icon::CARET_DOWN, "Plus bas (Maj+Entrée)", any).clicked() {
        search.newer();
    }
    if icon_button(&mut bar, p, icon::CARET_UP, "Plus haut (Entrée)", any).clicked() {
        search.older();
    }
    if toggle(&mut bar, p, "Aa", search.case_sensitive, "Respecter la casse").clicked() {
        search.case_sensitive = !search.case_sensitive;
    }
    let summary = search.summary();
    if !summary.is_empty() {
        let color = if search.found.is_empty() { p.danger } else { p.muted };
        bar.add_sized(vec2(84.0, 28.0), egui::Label::new(RichText::new(summary).size(12.0).color(color)));
    }

    let field_id = Id::new(("terminal-search", id));
    let field = bar.add(
        TextEdit::singleline(&mut search.query)
            .id(field_id)
            .hint_text("Rechercher dans le terminal")
            .desired_width(bar.available_width())
            .return_key(None)
            .margin(egui::Margin { left: 26, right: 6, top: 5, bottom: 5 }),
    );
    ui.painter().text(
        field.rect.left_center() + vec2(13.0, 0.0),
        Align2::CENTER_CENTER,
        icon::MAGNIFYING_GLASS,
        FontId::proportional(14.0),
        p.muted,
    );
    if std::mem::take(&mut search.focus) {
        field.request_focus();
        // Typing replaces what was searched before.
        if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), field_id) {
            let all = egui::text::CCursorRange::two(
                egui::text::CCursor::new(0),
                egui::text::CCursor::new(search.query.chars().count()),
            );
            state.cursor.set_char_range(Some(all));
            state.store(ui.ctx(), field_id);
        }
    }
    if field.changed() {
        ui.ctx().request_repaint();
    }
    if field.has_focus() {
        let (enter, shift) = ui.input(|input| (input.key_pressed(Key::Enter), input.modifiers.shift));
        if enter && shift {
            search.newer();
        } else if enter {
            search.older();
        }
        // Ctrl+Shift+F again: select what is typed.
        if ui.ctx().input_mut(|input| take_ctrl(input, Key::F, Some(true))) {
            search.focus = true;
        }
    }
    // egui takes the focus away as Escape comes in, before the field sees
    // it: the field just lost it, or (keys injected by scripts) still has it.
    if (field.lost_focus() || field.has_focus()) && ui.input(|input| input.key_pressed(Key::Escape)) {
        close = true;
    }
    if close {
        terminal.extras.search = None;
        terminal.wants_focus = true;
    }
    rect.bottom()
}

/// A small button that stays lit while its option is on.
fn toggle(ui: &mut Ui, p: &Palette, text: &str, on: bool, tooltip: &str) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(30.0, 30.0), Sense::click());
    let fill = if on {
        p.accent
    } else if response.hovered() {
        p.surface_hover
    } else {
        egui::Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, 6, fill);
    let color = if on { p.on_accent } else { p.muted };
    ui.painter()
        .text(rect.center(), Align2::CENTER_CENTER, text, FontId::proportional(13.0), color);
    response.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tooltip)
}

/// One entry of a menu. Returns true when chosen.
fn item(ui: &mut Ui, enabled: bool, text: impl Into<egui::WidgetText>, shortcut: &str) -> bool {
    let mut button = Button::new(text);
    if !shortcut.is_empty() {
        button = button.shortcut_text(shortcut);
    }
    let clicked = ui.add_enabled(enabled, button).clicked();
    if clicked {
        ui.close();
    }
    clicked
}

/// The menu of the terminal, at the pointer, after a right click (or
/// Shift+right click, see `ui::terminal::pointer`).
fn context_menu(
    ui: &mut Ui,
    response: &egui::Response,
    grid: &Grid,
    terminal: &mut Terminal,
    actions: &mut Vec<Action>,
) {
    let open = std::mem::take(&mut terminal.extras.open_menu);
    if open {
        let pointer = ui.ctx().pointer_interact_pos();
        terminal.extras.menu_link = pointer
            .filter(|pos| response.rect.contains(*pos))
            .and_then(|pos| link_at(terminal, grid.cell_at(pos)))
            .map(|link| link.url);
    }
    let popup = Popup::menu(response)
        .open_memory(open.then_some(SetOpenCommand::Bool(true)))
        .at_pointer_fixed();
    popup.show(|ui| {
        ui.set_min_width(250.0);
        menu(ui, terminal, actions);
    });
}

fn menu(ui: &mut Ui, terminal: &mut Terminal, actions: &mut Vec<Action>) {
    let id = terminal.extras.id;
    if let Some(url) = terminal.extras.menu_link.clone() {
        if item(ui, true, format!("{}  Ouvrir le lien", icon::ARROW_SQUARE_OUT), "Ctrl+clic") {
            actions.push(Action::Terminal(TerminalAction::OpenLink(url.clone())));
        }
        if item(ui, true, format!("{}  Copier le lien", icon::LINK), "") {
            ui.ctx().copy_text(url);
            actions.push(Action::Terminal(TerminalAction::Notice("Lien copié".to_owned())));
        }
        ui.separator();
    }

    let has_selection = terminal.extras.all_selected || terminal.selection_range().is_some();
    if item(ui, has_selection, format!("{}  Copier", icon::COPY), "Ctrl+Maj+C") {
        let text = if terminal.extras.all_selected {
            Some(all_text(terminal))
        } else {
            terminal.selected_text()
        };
        if let Some(text) = text {
            ui.ctx().copy_text(text);
        }
        terminal.selection = None;
        terminal.extras.all_selected = false;
    }
    if item(ui, true, format!("{}  Coller", icon::CLIPBOARD_TEXT), "Ctrl+V") {
        terminal.wants_focus = true;
        ui.ctx().send_viewport_cmd(ViewportCommand::RequestPaste);
    }
    if item(ui, true, format!("{}  Tout sélectionner", icon::SELECTION_ALL), "Ctrl+Maj+A") {
        terminal.selection = None;
        terminal.extras.all_selected = true;
    }
    ui.separator();
    if item(ui, true, format!("{}  Rechercher…", icon::MAGNIFYING_GLASS), "Ctrl+Maj+F") {
        open_search(terminal);
    }
    ui.separator();
    if item(ui, true, format!("{}  Effacer l'écran", icon::ERASER), "Ctrl+L") {
        // The shell redraws its prompt at the top, as with `clear`.
        terminal.send([0x0c]);
        terminal.wants_focus = true;
    }
    if item(ui, true, format!("{}  Effacer l'historique", icon::BROOM), "") {
        clear_history(terminal);
    }
    ui.separator();
    if terminal.extras.tap.recording().is_some() {
        if item(ui, true, RichText::new(format!("{}  Arrêter l'enregistrement", icon::STOP_CIRCLE)), "") {
            actions.push(Action::Terminal(TerminalAction::StopLog { terminal: id }));
        }
    } else if item(ui, true, format!("{}  Enregistrer la session…", icon::RECORD), "") {
        actions.push(Action::Terminal(TerminalAction::StartLog { terminal: id }));
    }
    if item(ui, true, format!("{}  Apparence…", icon::PALETTE), "") {
        actions.push(Action::Terminal(TerminalAction::OpenLook));
    }
}

/// Forgets the lines above the screen; the screen itself stays.
pub fn clear_history(terminal: &mut Terminal) {
    terminal.with_screen_mut(vt100::Screen::clear_scrollback);
    terminal.extras.all_selected = false;
    terminal.selection = None;
    if let Some(search) = &mut terminal.extras.search {
        search.invalidate();
    }
    terminal.wants_focus = true;
}

/// A red dot on the icon of a tab whose session is being recorded.
pub fn tab_badge(ui: &Ui, p: &Palette, tab: &Tab, rect: Rect) {
    let recording = tab
        .terminal
        .as_ref()
        .is_some_and(|terminal| terminal.extras.tap.recording().is_some());
    if recording {
        let center = rect.left_center() + vec2(25.0, -6.0);
        ui.painter().circle_filled(center, 4.5, p.bg);
        ui.painter().circle_filled(center, 3.2, p.danger);
    }
}

/// The entries of the menu of a tab that are about its terminal.
pub fn tab_menu(ui: &mut Ui, tab: &Tab, actions: &mut Vec<Action>) {
    let Some(terminal) = tab.terminal.as_ref().filter(|_| tab.panes.terminal()) else {
        return;
    };
    let id = terminal.extras.id;
    if item(ui, true, format!("{}  Rechercher dans le terminal…", icon::MAGNIFYING_GLASS), "") {
        actions.push(Action::Terminal(TerminalAction::Search { tab: tab.id }));
    }
    if terminal.extras.tap.recording().is_some() {
        if item(ui, true, format!("{}  Arrêter l'enregistrement", icon::STOP_CIRCLE), "") {
            actions.push(Action::Terminal(TerminalAction::StopLog { terminal: id }));
        }
    } else if item(ui, true, format!("{}  Enregistrer la session…", icon::RECORD), "") {
        actions.push(Action::Terminal(TerminalAction::StartLog { terminal: id }));
    }
}

/// In the form of an SSH connection: record every session.
pub fn profile_fields(ui: &mut Ui, p: &Palette, draft: &mut Profile) {
    checkbox(ui, p, &mut draft.log_sessions, "Enregistrer chaque session du terminal").on_hover_text(
        "Le journal de chaque session est écrit dans le dossier choisi\n\
         dans l'apparence du terminal (menu du terminal, « Apparence… »).",
    );
    ui.add_space(6.0);
}

#[cfg(test)]
mod tests {
    use egui::Modifiers;

    use super::*;

    #[test]
    fn only_several_lines_need_a_confirmation() {
        assert!(!is_multiline("ls -la"));
        assert!(!is_multiline("ls -la\n"));
        assert!(!is_multiline("ls -la\r\n"));
        assert!(is_multiline("cd /tmp\nrm -rf *"));
        assert!(is_multiline("a\r\nb\r\n"));
    }

    /// AltGr+à types `@` on a French keyboard: it must reach the shell,
    /// not reset the size of the text as Ctrl+0 does.
    #[test]
    fn altgr_characters_are_not_taken_for_shortcuts() {
        let press = |key, modifiers| Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        };
        let ctx = egui::Context::default();
        let altgr = Modifiers::CTRL | Modifiers::ALT;
        let input = egui::RawInput {
            events: vec![
                press(Key::Num0, altgr),
                press(Key::Equals, altgr),
                press(Key::Minus, Modifiers::CTRL | Modifiers::SHIFT),
                press(Key::Num0, Modifiers::CTRL),
                press(Key::F, Modifiers::CTRL),
            ],
            ..Default::default()
        };
        let mut taken = Vec::new();
        let mut left = 0;
        let output = ctx.run_ui(input, |ui| {
            ui.ctx().input_mut(|input| {
                taken = vec![
                    take_ctrl(input, Key::Num0, None),
                    take_ctrl(input, Key::Equals, None),
                    take_ctrl(input, Key::Minus, Some(false)),
                    take_ctrl(input, Key::F, Some(true)),
                ];
                left = input.events.len();
            });
        });
        output.drop_without_applying_deltas();
        // Only the plain Ctrl+0 is a shortcut; the rest stays for the shell.
        assert_eq!(taken, [true, false, false, false]);
        assert_eq!(left, 4);
    }

    #[test]
    fn a_double_click_selects_a_word_a_path_or_an_address() {
        let text = "cat \"/var/log/nginx/error.log\" (voir https://example.org/a?b=1) 日本語";
        let word = |col| word_at(text, col).map(|(first, last)| &text[usize::from(first)..=usize::from(last)]);
        assert_eq!(word(1), Some("cat"));
        assert_eq!(word(10), Some("/var/log/nginx/error.log"));
        assert_eq!(word(40), Some("https://example.org/a?b=1"));
        assert_eq!(word(3), None, "a space");
        // Wide characters: columns, not bytes.
        let wide = "ok 日本語 fin";
        assert_eq!(word_at(wide, 4), Some((3, 8)));
        assert_eq!(word_at(wide, 10), Some((10, 12)));
    }

    #[test]
    fn sizes_are_written_the_french_way() {
        assert_eq!(format_size(14.0), "14");
        assert_eq!(format_size(13.5), "13,5");
    }

    /// The search bar of a terminal, driven as a keyboard would, in a
    /// headless egui. The shell never connects (nothing listens on port 1):
    /// the bar works on whatever the terminal shows.
    #[test]
    fn the_search_bar_takes_the_keyboard_and_escape_closes_it() {
        use std::sync::Arc;

        use egui::{Context, Pos2, RawInput};

        use crate::storage::ssh::SshSettings;
        use crate::worker::Worker;

        let ctx = Context::default();
        crate::theme::install_fonts(&ctx);
        crate::theme::apply(&ctx, &Palette::dark());
        let (worker, _events) = Worker::new(ctx.clone());
        let settings = Arc::new(SshSettings {
            extra: Default::default(),
            host: "127.0.0.1".to_owned(),
            port: 1,
            user: "nobody".to_owned(),
            secret: String::new(),
            key_path: None,
            known_hosts: None,
        });
        let mut terminal = Terminal::open(&worker, settings);
        open_search(&mut terminal);

        let mut time = 0.0;
        let mut frame = |terminal: &mut Terminal, events: Vec<Event>| {
            let input = RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(900.0, 500.0))),
                time: Some(time),
                focused: true,
                events,
                ..RawInput::default()
            };
            time += 1.0 / 60.0;
            let mut actions = Vec::new();
            let output = ctx.run_ui(input, |ui| {
                super::super::terminal::show(ui, &Palette::dark(), terminal, true, &mut actions);
            });
            output.drop_without_applying_deltas();
        };
        let key = |key| Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        };

        frame(&mut terminal, Vec::new());
        frame(&mut terminal, Vec::new());
        frame(&mut terminal, vec![Event::Text("err".to_owned())]);
        assert_eq!(terminal.extras.search.as_ref().map(|search| search.query.as_str()), Some("err"));
        frame(&mut terminal, vec![key(Key::Enter)]);
        assert!(terminal.extras.search.is_some(), "Enter goes to the next result");
        frame(&mut terminal, vec![key(Key::Escape)]);
        assert!(terminal.extras.search.is_none(), "Escape closes the bar");
        assert!(terminal.wants_focus || terminal.focused, "the terminal gets the keyboard back");
    }
}
