//! The row of tabs above a view, their menu, and the switch between files
//! and terminal for the connections that have both.

use egui::scroll_area::ScrollBarVisibility;
use egui::{
    Align, Align2, Button, DragAndDrop, FontId, Id, Label, Layout, Rect, RichText, ScrollArea, Sense,
    Stroke, Ui, UiBuilder, Vec2, vec2,
};

use super::Action;
use super::widgets::{drag_label, dropped, icon, protocol_icon};
use crate::profiles::{Panes, Protocol};
use crate::tabs::{Closing, Side, Tab, TabPlace};
use crate::theme::{Palette, RADIUS};

pub const HEIGHT: f32 = 32.0;
const MAX_WIDTH: f32 = 210.0;
const MIN_WIDTH: f32 = 96.0;
const SWITCH_WIDTH: f32 = 96.0;
const GAP: f32 = 4.0;

/// A tab being dragged to another place, by its id.
pub struct DraggedTab(pub u64);

/// The row of tabs of one view.
pub struct Bar<'a> {
    /// Every tab; the row lists those of `side`.
    pub tabs: &'a [Tab],
    pub side: Side,
    /// Index of the tab this view shows.
    pub shown: Option<usize>,
    /// Two views are on screen.
    pub split: bool,
    /// This is the view the keyboard and the menus act on.
    pub focused: bool,
    /// A closed tab can be brought back.
    pub can_reopen: bool,
}

pub fn tab_id(id: u64) -> Id {
    Id::new(("tab", id))
}

pub fn show(ui: &mut Ui, p: &Palette, bar: &Bar<'_>, actions: &mut Vec<Action>) {
    let shown = bar.shown.and_then(|index| bar.tabs.get(index));
    let mine: Vec<(usize, &Tab)> = bar
        .tabs
        .iter()
        .enumerate()
        .filter(|(_, tab)| tab.side == bar.side)
        .collect();

    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = GAP;
        ui.set_height(HEIGHT);

        let has_terminal = shown.is_some_and(Tab::has_shell);
        let reserved = if has_terminal { SWITCH_WIDTH + 12.0 } else { 0.0 };
        let room = (ui.available_width() - reserved - super::local::NEW_TAB_WIDTH).max(MIN_WIDTH);
        let width = (room / mine.len().max(1) as f32 - GAP).clamp(MIN_WIDTH, MAX_WIDTH);

        // When the row overflows, the tab shown is brought into view: as
        // it becomes the one shown, and as the row changes size.
        let memory = Id::new(("tabs-shown", bar.side));
        let state = (shown.map(|tab| tab.id), room.round() as i32, mine.len());
        let reveal = ui.data_mut(|data| {
            let before = data.get_temp::<(Option<u64>, i32, usize)>(memory);
            data.insert_temp(memory, state);
            before != Some(state)
        });

        // More tabs than fit: the row scrolls, with the wheel too.
        ui.style_mut().always_scroll_the_only_direction = true;
        ScrollArea::horizontal()
            .id_salt(("tabs", bar.side))
            .max_width(room)
            .scroll_bar_visibility(ScrollBarVisibility::AlwaysHidden)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = GAP;
                    for (position, (index, tab)) in mine.iter().enumerate() {
                        let place = Placed {
                            index: *index,
                            position,
                            count: mine.len(),
                            active: bar.shown == Some(*index),
                            reveal,
                        };
                        tab_button(ui, p, bar, tab, &place, width, actions);
                    }
                });
            });
        super::local::new_tab_button(ui, p, actions);

        // What is left of the row takes a dropped tab, and has a menu.
        let rest = (ui.available_width() - reserved).max(0.0);
        if rest > 8.0 {
            let (rect, response) = ui.allocate_exact_size(vec2(rest, HEIGHT), Sense::click());
            if response.dnd_hover_payload::<DraggedTab>().is_some() {
                let mark = Rect::from_min_size(rect.min + vec2(0.0, 5.0), vec2(2.0, HEIGHT - 10.0));
                ui.painter().rect_filled(mark, 1, p.accent);
            }
            if let Some(dragged) = dropped::<DraggedTab>(&response) {
                actions.push(Action::MoveTab(dragged.0, TabPlace::View(bar.side)));
            }
            response.context_menu(|ui| row_menu(ui, bar, mine.first().map(|(_, tab)| tab.id), actions));
        }

        if let Some(tab) = shown.filter(|tab| tab.has_shell()) {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                panes_switch(ui, p, tab.panes, actions);
            });
        }
    });
}

/// Where a tab sits in the row of its view.
struct Placed {
    /// Its index among all the tabs.
    index: usize,
    /// Its rank in the row, and how many the row has.
    position: usize,
    count: usize,
    active: bool,
    /// Bring it into view if it is the active one.
    reveal: bool,
}

fn tab_button(
    ui: &mut Ui,
    p: &Palette,
    bar: &Bar<'_>,
    tab: &Tab,
    place: &Placed,
    width: f32,
    actions: &mut Vec<Action>,
) {
    let (_, rect) = ui.allocate_space(vec2(width, HEIGHT));
    let mut response = ui.interact(rect, tab_id(tab.id), Sense::click_and_drag());
    // The tab of a file says where the file lives.
    if let Some(address) = super::editor::tab_tooltip(tab) {
        response = response.on_hover_text(address);
    }
    let active = place.active;
    if active && place.reveal {
        response.scroll_to_me(None);
    }

    if ui.is_rect_visible(rect) {
        let pointed = ui.rect_contains_pointer(rect);
        // With two views, only the one in use keeps its tab lit.
        let lit = active && (bar.focused || !bar.split);
        if active {
            let fill = if lit { p.surface } else { p.surface.gamma_multiply(0.7) };
            ui.painter().rect_filled(rect, RADIUS, fill);
        } else if pointed {
            ui.painter().rect_filled(rect, RADIUS, p.surface.gamma_multiply(0.6));
        }
        super::hosts::tint_tab(ui, p, &tab.profile_id, rect, active, lit, pointed);
        if lit && bar.split {
            let line = Rect::from_min_max(rect.left_bottom() + vec2(10.0, -2.0), rect.right_bottom() - vec2(10.0, 0.0));
            ui.painter().rect_filled(line, 1, p.accent);
        }

        let glyph = if tab.panes == Panes::Terminal {
            icon::TERMINAL_WINDOW
        } else if tab.panes == Panes::Monitor {
            icon::GAUGE
        } else {
            protocol_icon(tab.protocol)
        };
        let (glyph, tint) = match super::editor::tab_glyph(p, tab) {
            Some((glyph, tint)) => (glyph, if lit { tint } else { tint.gamma_multiply(0.75) }),
            None => (glyph, super::hosts::tab_glyph_color(ui.ctx(), p, &tab.profile_id, lit)),
        };
        ui.painter().text(rect.left_center() + vec2(18.0, 0.0), Align2::CENTER_CENTER, glyph, FontId::proportional(16.0), tint);
        super::terminal_ext::tab_badge(ui, p, tab, rect);
        super::ssh_power::tab_badge(ui, p, rect, tab);

        // The close button sits above the tab, so clicking it does not select it.
        // A file with changes shows a dot there instead, but under the pointer.
        let closable = (pointed || active) && !DragAndDrop::has_any_payload(ui.ctx());
        let dirty = super::editor::is_dirty(tab);
        let button = Rect::from_center_size(rect.right_center() - vec2(15.0, 0.0), Vec2::splat(20.0));
        if closable {
            let close = ui.interact(button, response.id.with("close"), Sense::click());
            if close.hovered() {
                ui.painter().rect_filled(button, 6, p.surface_hover);
            }
            if dirty && !close.hovered() {
                super::editor::dirty_dot(ui, p, tab, button.center());
            } else {
                ui.painter().text(
                    button.center(),
                    Align2::CENTER_CENTER,
                    icon::X,
                    FontId::proportional(12.0),
                    if close.hovered() { p.text } else { p.muted },
                );
            }
            if close.on_hover_text(tr!("Fermer l'onglet (Ctrl+W)")).clicked() {
                actions.push(Action::CloseTab(place.index));
            }
        } else if dirty {
            super::editor::dirty_dot(ui, p, tab, button.center());
        }

        let text_rect = Rect::from_min_max(
            rect.min + vec2(32.0, 0.0),
            rect.max - vec2(if closable || dirty { 28.0 } else { 8.0 }, 0.0),
        );
        let mut text = ui.new_child(
            UiBuilder::new()
                .max_rect(text_rect)
                .layout(Layout::left_to_right(Align::Center)),
        );
        text.add(
            Label::new(RichText::new(&tab.title).color(if active { p.text } else { p.muted }))
                .truncate()
                .selectable(false),
        );
    }

    // Dragging a tab onto another puts it on the side it is dropped on.
    response.dnd_set_drag_payload(DraggedTab(tab.id));
    if response.dragged() && DragAndDrop::has_payload_of_type::<DraggedTab>(ui.ctx()) {
        drag_label(ui.ctx(), p, &tab.title);
    }
    let after = ui
        .ctx()
        .pointer_interact_pos()
        .is_some_and(|pointer| pointer.x > rect.center().x);
    if response
        .dnd_hover_payload::<DraggedTab>()
        .is_some_and(|dragged| dragged.0 != tab.id)
    {
        let x = if after { rect.right() + GAP / 2.0 } else { rect.left() - GAP / 2.0 };
        ui.painter()
            .vline(x, rect.y_range().shrink(4.0), Stroke::new(2.0, p.accent));
    }
    if let Some(dragged) = dropped::<DraggedTab>(&response)
        && dragged.0 != tab.id
    {
        let place = if after { TabPlace::After(tab.id) } else { TabPlace::Before(tab.id) };
        actions.push(Action::MoveTab(dragged.0, place));
    }

    if response.clicked() {
        actions.push(Action::SelectTab(place.index));
    }
    if response.middle_clicked() {
        actions.push(Action::CloseTab(place.index));
    }
    response.context_menu(|ui| tab_menu(ui, p, bar, tab, place, actions));
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

fn tab_menu(ui: &mut Ui, p: &Palette, bar: &Bar<'_>, tab: &Tab, place: &Placed, actions: &mut Vec<Action>) {
    ui.set_min_width(250.0);
    let id = tab.id;
    let (before, after) = (place.position, place.count - place.position - 1);

    if item(ui, true, format!("{}  {}", icon::X, tr!("Fermer")), tr!("Ctrl+W")) {
        actions.push(Action::CloseTabs(id, Closing::Only));
    }
    if item(ui, place.count > 1, tr!("Fermer les autres"), "") {
        actions.push(Action::CloseTabs(id, Closing::Others));
    }
    if item(ui, before > 0, tr!("Fermer les onglets à gauche"), "") {
        actions.push(Action::CloseTabs(id, Closing::Left));
    }
    if item(ui, after > 0, tr!("Fermer les onglets à droite"), "") {
        actions.push(Action::CloseTabs(id, Closing::Right));
    }
    let all = if bar.split { tr!("Fermer tous ceux de cette vue") } else { tr!("Fermer tout") };
    if item(ui, true, all, "") {
        actions.push(Action::CloseTabs(id, Closing::All));
    }
    let reopen = format!("{}  {}", icon::ARROW_U_UP_LEFT, tr!("Rouvrir l'onglet fermé"));
    if item(ui, bar.can_reopen, reopen, tr!("Ctrl+Maj+T")) {
        actions.push(Action::ReopenTab);
    }
    ui.separator();

    // Sending the only tab to a new view would leave this one empty.
    if !bar.split {
        let right = format!("{}  {}", icon::SQUARE_SPLIT_HORIZONTAL, tr!("Ouvrir dans une vue à droite"));
        if item(ui, place.count > 1, right, "") {
            actions.push(Action::MoveTab(id, TabPlace::View(Side::Right)));
        }
    } else {
        let (text, side) = match tab.side {
            Side::Left => (format!("{}  {}", icon::ARROW_LINE_RIGHT, tr!("Déplacer vers la vue de droite")), Side::Right),
            Side::Right => (format!("{}  {}", icon::ARROW_LINE_LEFT, tr!("Déplacer vers la vue de gauche")), Side::Left),
        };
        if item(ui, true, text, "") {
            actions.push(Action::MoveTab(id, TabPlace::View(side)));
        }
        if item(ui, true, tr!("Tout regrouper dans une seule vue"), "") {
            actions.push(Action::JoinViews);
        }
    }
    // A file is not a connection: no shells, colours or settings of its own.
    let file = tab.editor.is_some();
    if !file {
        super::local::tab_menu_item(ui, tab, actions);
    }
    if item(ui, before > 0, tr!("Placer en premier"), "") {
        actions.push(Action::MoveTab(id, TabPlace::First));
    }
    if item(ui, after > 0, tr!("Placer en dernier"), "") {
        actions.push(Action::MoveTab(id, TabPlace::Last));
    }
    // The dashboard is no connection either: only its place and closing.
    if tab.panes == Panes::Monitor {
        return;
    }
    ui.separator();

    if file {
        super::editor::tab_menu(ui, tab, actions);
        ui.separator();
        if item(ui, true, format!("{}  {}", icon::COPY_SIMPLE, tr!("Copier l'adresse")), "") {
            actions.push(Action::CopyTabAddress(id));
        }
        return;
    }
    if item(ui, true, format!("{}  {}", icon::COPY, tr!("Dupliquer")), "") {
        actions.push(Action::DuplicateTab(id));
    }
    if item(ui, true, format!("{}  {}", icon::PENCIL_SIMPLE, tr!("Renommer…")), "") {
        actions.push(Action::AskRenameTab(id));
    }
    if item(ui, true, format!("{}  {}", icon::ARROWS_CLOCKWISE, tr!("Reconnecter")), "") {
        actions.push(Action::ReconnectTab(id));
    }
    if tab.has_shell() {
        ui.menu_button(format!("{}  {}", icon::LAYOUT, tr!("Afficher")), |ui| {
            let choices = [
                (Panes::Files, tr!("Les fichiers")),
                (Panes::Both, tr!("Les fichiers et le terminal")),
                (Panes::Terminal, tr!("Le terminal")),
            ];
            for (panes, label) in choices {
                let mark = if tab.panes == panes { icon::CHECK } else { "    " };
                if item(ui, true, format!("{mark}  {label}"), "") && tab.panes != panes {
                    actions.push(Action::SelectTab(place.index));
                    actions.push(Action::SetPanes(panes));
                }
            }
        });
        super::ssh_auth::tab_menu_items(ui, tab, actions);
        super::terminal_ext::tab_menu(ui, tab, actions);
        super::ssh_power::tab_menu(ui, p, bar.tabs, tab, actions);
    }
    super::tunnels::tab_menu(ui, tab, actions);
    ui.separator();

    if item(ui, true, format!("{}  {}", icon::COPY_SIMPLE, tr!("Copier l'adresse")), "") {
        actions.push(Action::CopyTabAddress(id));
    }
    super::hosts::tab_menu(ui, p, &tab.profile_id, actions);
    // This computer is not a saved connection.
    if tab.protocol != Protocol::Local
        && item(ui, true, RichText::new(format!("{}  {}", icon::GEAR, tr!("Modifier la connexion…"))).color(p.text), "")
    {
        actions.push(Action::EditProfile(tab.profile_id.clone()));
    }
}

/// The menu of the empty part of the row.
fn row_menu(ui: &mut Ui, bar: &Bar<'_>, first: Option<u64>, actions: &mut Vec<Action>) {
    ui.set_min_width(230.0);
    let reopen = format!("{}  {}", icon::ARROW_U_UP_LEFT, tr!("Rouvrir l'onglet fermé"));
    if item(ui, bar.can_reopen, reopen, tr!("Ctrl+Maj+T")) {
        actions.push(Action::ReopenTab);
    }
    if bar.split && item(ui, true, tr!("Tout regrouper dans une seule vue"), "") {
        actions.push(Action::JoinViews);
    }
    let all = if bar.split { tr!("Fermer tous ceux de cette vue") } else { tr!("Fermer tout") };
    if let Some(first) = first
        && item(ui, true, all, "")
    {
        actions.push(Action::CloseTabs(first, Closing::All));
    }
    super::hosts::restore_item(ui, actions);
}

/// Three buttons: files, both, terminal.
fn panes_switch(ui: &mut Ui, p: &Palette, current: Panes, actions: &mut Vec<Action>) {
    let (rect, _) = ui.allocate_exact_size(vec2(SWITCH_WIDTH, 28.0), Sense::hover());
    ui.painter().rect_filled(rect, RADIUS, p.surface);

    let choices = [
        (Panes::Files, icon::FOLDER, tr!("Fichiers")),
        (Panes::Both, icon::SQUARE_SPLIT_VERTICAL, tr!("Fichiers et terminal")),
        (Panes::Terminal, icon::TERMINAL_WINDOW, tr!("Terminal")),
    ];
    let cell = rect.width() / choices.len() as f32;
    for (index, (panes, glyph, tooltip)) in choices.into_iter().enumerate() {
        let button = Rect::from_min_size(
            rect.min + vec2(cell * index as f32, 0.0),
            vec2(cell, rect.height()),
        )
        .shrink(2.0);
        let response = ui.interact(button, ui.id().with(("panes", index)), Sense::click());
        let selected = panes == current;
        if selected {
            ui.painter().rect_filled(button, 6, p.accent);
        } else if response.hovered() {
            ui.painter().rect_filled(button, 6, p.surface_hover);
        }
        ui.painter().text(
            button.center(),
            Align2::CENTER_CENTER,
            glyph,
            FontId::proportional(15.0),
            if selected { p.on_accent } else { p.muted },
        );
        if response.on_hover_text(tooltip).clicked() && !selected {
            actions.push(Action::SetPanes(panes));
        }
    }
}

/// The row is driven here with synthetic pointer events, as a mouse would:
/// scripts cannot drag in the off-screen window.
#[cfg(test)]
mod tests {
    use egui::{Context, Event, Modifiers, PointerButton, Popup, Pos2, RawInput};

    use super::*;
    use crate::tabs::tests::sample;

    struct Harness {
        ctx: Context,
        tabs: Vec<Tab>,
        time: f64,
    }

    impl Harness {
        fn new(sides: &str) -> Self {
            let ctx = Context::default();
            crate::theme::install_fonts(&ctx);
            crate::theme::apply(&ctx, &Palette::dark());
            let mut harness = Self {
                ctx,
                tabs: sample(sides),
                time: 0.0,
            };
            harness.frame(Vec::new());
            harness.frame(Vec::new());
            harness
        }

        /// Both rows, one above the other, in a window too narrow for many tabs.
        fn frame(&mut self, events: Vec<Event>) -> Vec<Action> {
            let input = RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(620.0, 300.0))),
                time: Some(self.time),
                events,
                ..RawInput::default()
            };
            self.time += 1.0 / 60.0;
            let mut actions = Vec::new();
            let palette = Palette::dark();
            let output = self.ctx.run_ui(input, |ui| {
                actions.clear();
                for side in Side::BOTH {
                    let bar = Bar {
                        tabs: &self.tabs,
                        side,
                        shown: self.tabs.iter().position(|tab| tab.side == side),
                        split: true,
                        focused: side == Side::Left,
                        can_reopen: false,
                    };
                    ui.push_id(side, |ui| show(ui, &palette, &bar, &mut actions));
                }
            });
            output.drop_without_applying_deltas();
            actions
        }

        fn rect(&self, tab: u64) -> Rect {
            self.ctx.read_response(tab_id(tab)).expect("tab on screen").rect
        }

        fn button(at: Pos2, button: PointerButton, pressed: bool) -> Event {
            Event::PointerButton {
                pos: at,
                button,
                pressed,
                modifiers: Modifiers::NONE,
            }
        }

        fn click_with(&mut self, at: Pos2, button: PointerButton) -> Vec<Action> {
            let mut actions = self.frame(vec![Event::PointerMoved(at)]);
            actions.extend(self.frame(Vec::new()));
            actions.extend(self.frame(vec![Self::button(at, button, true)]));
            actions.extend(self.frame(vec![Self::button(at, button, false)]));
            actions.extend(self.frame(Vec::new()));
            actions
        }

        fn drag(&mut self, from: Pos2, to: Pos2) -> Vec<Action> {
            let mut actions = self.frame(vec![Event::PointerMoved(from)]);
            actions.extend(self.frame(Vec::new()));
            actions.extend(self.frame(vec![Self::button(from, PointerButton::Primary, true)]));
            for step in 1..=8 {
                let at = from + (to - from) * (step as f32 / 8.0);
                actions.extend(self.frame(vec![Event::PointerMoved(at)]));
            }
            actions.extend(self.frame(Vec::new()));
            actions.extend(self.frame(vec![Self::button(to, PointerButton::Primary, false)]));
            actions.extend(self.frame(Vec::new()));
            actions
        }
    }

    fn told(actions: &[Action]) -> Vec<String> {
        actions
            .iter()
            .map(|action| match action {
                Action::SelectTab(index) => format!("select {index}"),
                Action::CloseTab(index) => format!("close {index}"),
                Action::MoveTab(id, place) => format!("move {id} {place:?}"),
                _ => "other".to_owned(),
            })
            .collect()
    }

    #[test]
    fn clicks_select_and_close() {
        let mut ui = Harness::new("LLRL");
        // Ids start at 1: the tab 4 is the fourth of the list, index 3.
        let fourth = ui.rect(4).left_center() + vec2(40.0, 0.0);
        assert_eq!(told(&ui.click_with(fourth, PointerButton::Primary)), ["select 3"]);
        assert_eq!(told(&ui.click_with(fourth, PointerButton::Middle)), ["close 3"]);
        let cross = ui.rect(4).right_center() - vec2(15.0, 0.0);
        assert_eq!(told(&ui.click_with(cross, PointerButton::Primary)), ["close 3"]);
    }

    #[test]
    fn a_right_click_opens_the_menu_without_selecting() {
        let mut ui = Harness::new("LLL");
        let second = ui.rect(2).center();
        assert!(ui.click_with(second, PointerButton::Secondary).is_empty());
        let response = ui.ctx.read_response(tab_id(2)).unwrap();
        assert!(Popup::is_id_open(&ui.ctx, Popup::default_response_id(&response)));
    }

    #[test]
    fn dragging_a_tab_drops_it_on_the_side_pointed_at() {
        let mut ui = Harness::new("LLLR");
        let (first, third) = (ui.rect(1), ui.rect(3));
        let right_half = third.center() + vec2(third.width() * 0.3, 0.0);
        let left_half = third.center() - vec2(third.width() * 0.3, 0.0);
        assert_eq!(told(&ui.drag(first.center(), right_half)), ["move 1 After(3)"]);
        assert_eq!(told(&ui.drag(first.center(), left_half)), ["move 1 Before(3)"]);

        // Onto a tab of the other row, or onto the empty part of that row.
        let other = ui.rect(4);
        assert_eq!(
            told(&ui.drag(first.center(), other.center() - vec2(other.width() * 0.3, 0.0))),
            ["move 1 Before(4)"]
        );
        let empty = Pos2::new(other.right() + 150.0, other.center().y);
        assert_eq!(told(&ui.drag(first.center(), empty)), ["move 1 View(Right)"]);

        // Dropped on itself: neither a move nor a click.
        assert!(ui.drag(first.center(), first.center() + vec2(30.0, 0.0)).is_empty());
    }

    /// A file with changes shows a dot where its close button is; the
    /// button still closes it, and the menu has no shell, colour or
    /// connection settings.
    #[test]
    fn the_tab_of_a_file_with_changes() {
        let mut ui = Harness::new("LL");
        let mut editor = crate::editor::Editor::with_text("abc", "Plain Text");
        editor.insert("x");
        ui.tabs[1].panes = Panes::Editor;
        ui.tabs[1].editor = Some(Box::new(editor));
        ui.frame(Vec::new());
        assert!(super::super::editor::is_dirty(&ui.tabs[1]));
        let cross = ui.rect(2).right_center() - vec2(15.0, 0.0);
        assert_eq!(told(&ui.click_with(cross, PointerButton::Primary)), ["close 1"]);
        let second = ui.rect(2).center();
        assert!(ui.click_with(second, PointerButton::Secondary).is_empty());
    }

    #[test]
    fn a_crowded_row_keeps_readable_tabs() {
        // Twelve tabs cannot fit 620 points: they keep a readable width
        // and the last ones start out of sight.
        let ui = Harness::new("LLLLLLLLLLLL");
        assert!(ui.rect(1).width() >= MIN_WIDTH);
        assert!(ui.rect(12).left() > 620.0);
        // `frame` shows the first tab of each view; what matters here is
        // that every tab keeps its place in the row.
        for id in 1..12 {
            assert!(ui.rect(id).right() < ui.rect(id + 1).right());
        }
    }
}
