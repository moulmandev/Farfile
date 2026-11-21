//! Colours and tags on screen: the marks in the list and the palette, the
//! tinted tabs, the choices of the connection form and of the menus, and
//! the small dialogs of this feature. The export and import dialogs are in
//! [`vault`].

pub mod vault;

use std::collections::HashMap;
use std::sync::Arc;

use egui::{
    Align, Align2, Color32, CornerRadius, CursorIcon, Direction, FontId, Frame, Id, Key, Label, Layout,
    Margin, Modifiers, Rect, Response, RichText, Sense, Stroke, StrokeKind, TextEdit, Ui, Vec2, pos2, vec2,
};

use super::Action;
use super::dialogs::Outcome;
use super::widgets::{icon, icon_button, muted, primary_button, small};
use crate::hosts::{self, HostColor, mix};
use crate::profiles::{self, Profile};
use crate::theme::{Palette, RADIUS, semibold};

/// What the user asked of this feature.
pub enum HostsAction {
    /// Give this connection a colour, or none.
    SetColor(String, Option<HostColor>),
    /// The same for every connection of this folder, at any depth.
    SetFolderColor(String, Option<HostColor>),
    /// Edit the tags of this connection.
    AskTags(String),
    /// Prepare an encrypted export of these connections.
    AskExport(ExportScope),
    /// Reopen, or not, the tabs at startup.
    SetRestoreTabs(bool),
}

/// What an export starts with ticked; everything can be ticked after.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExportScope {
    All,
    /// A folder of the list, by path, with what it holds.
    Folder(String),
    /// One connection, by id.
    Host(String),
}

/// The dialogs of this feature.
pub enum HostsDialog {
    Tags(TagsForm),
    Export(Box<vault::ExportForm>),
    Unlock(Box<vault::UnlockForm>),
    Preview(Box<vault::PreviewForm>),
}

pub fn show_dialog(ui: &mut Ui, p: &Palette, dialog: &mut HostsDialog, first_frame: bool, outcome: &mut Outcome) {
    match dialog {
        HostsDialog::Tags(form) => tags_dialog(ui, p, form, first_frame, outcome),
        HostsDialog::Export(form) => vault::export_dialog(ui, p, form, first_frame, outcome),
        HostsDialog::Unlock(form) => vault::unlock_dialog(ui, p, form, first_frame, outcome),
        HostsDialog::Preview(form) => vault::preview_dialog(ui, p, form, outcome),
    }
}

// ---- What the app publishes for the widgets ---------------------------------

/// Published by the app at the start of each frame, for the widgets that
/// only know a connection by its id (tabs, terminal) or have no access to
/// the settings (the menu of the row of tabs).
#[derive(Clone, Default)]
struct Published {
    colors: Arc<HashMap<String, HostColor>>,
    restore_tabs: bool,
}

fn published_id() -> Id {
    Id::new("hosts-published")
}

pub fn publish(ctx: &egui::Context, profiles: &[Profile], restore_tabs: bool) {
    let colors: HashMap<String, HostColor> = profiles
        .iter()
        .filter_map(|profile| Some((profile.id.clone(), profile.color?)))
        .collect();
    let published = Published {
        colors: Arc::new(colors),
        restore_tabs,
    };
    ctx.data_mut(|data| data.insert_temp(published_id(), published));
}

fn published(ctx: &egui::Context) -> Published {
    ctx.data(|data| data.get_temp(published_id())).unwrap_or_default()
}

/// The colour of the connection of this id, as last published.
pub fn color_of(ctx: &egui::Context, profile_id: &str) -> Option<HostColor> {
    published(ctx).colors.get(profile_id).copied()
}

// ---- Marks ---------------------------------------------------------------------

/// The protocol icon of a row: in the connection's colour when it has one.
pub fn icon_color(p: &Palette, profile: &Profile, active: bool) -> Color32 {
    match profile.color {
        Some(color) => color.rgb(p.dark),
        None if active => p.accent,
        None => p.muted,
    }
}

/// The colour mark of a row of a list: a short bar centred there, in front
/// of its icon.
pub fn row_mark(ui: &Ui, p: &Palette, profile: &Profile, center: egui::Pos2) {
    if let Some(color) = profile.color {
        let bar = Rect::from_center_size(center, vec2(3.0, 14.0));
        ui.painter().rect_filled(bar, 2, color.rgb(p.dark));
    }
}

/// Adds the tags to the tooltip of a connection.
pub fn tooltip_tags(text: &mut String, profile: &Profile) {
    if !profile.tags.is_empty() {
        let tags: Vec<String> = profile.tags.iter().map(|tag| format!("#{tag}")).collect();
        text.push('\n');
        text.push_str(&tags.join("  "));
    }
}

/// A tab of a coloured connection: tinted all over, more when it is the one
/// shown, with a band of its colour along the top. Painted over the usual
/// background of the tab.
pub fn tint_tab(ui: &Ui, p: &Palette, profile_id: &str, rect: Rect, active: bool, lit: bool, pointed: bool) {
    let Some(color) = color_of(ui.ctx(), profile_id) else {
        return;
    };
    let rgb = color.rgb(p.dark);
    let fill = match (active, lit) {
        (true, true) => mix(p.surface, rgb, 0.24),
        (true, false) => mix(p.surface, rgb, 0.15),
        (false, _) if pointed => mix(p.bg, rgb, 0.17),
        (false, _) => mix(p.bg, rgb, 0.10),
    };
    let painter = ui.painter();
    painter.rect_filled(rect, RADIUS, fill);
    let band = Rect::from_min_max(rect.left_top() + vec2(7.0, 0.0), rect.right_top() + vec2(-7.0, 3.0));
    let band_color = if active { rgb } else { rgb.gamma_multiply(0.75) };
    painter.rect_filled(band, CornerRadius { nw: 0, ne: 0, sw: 2, se: 2 }, band_color);
}

/// The icon of a tab: in its connection's colour when it has one.
pub fn tab_glyph_color(ctx: &egui::Context, p: &Palette, profile_id: &str, lit: bool) -> Color32 {
    match color_of(ctx, profile_id) {
        Some(color) if lit => color.rgb(p.dark),
        Some(color) => mix(p.muted, color.rgb(p.dark), 0.7),
        None if lit => p.accent,
        None => p.muted,
    }
}

/// A frame of the connection's colour around its terminal, the last thing
/// seen before typing a command on the wrong server.
pub fn terminal_border(ui: &Ui, p: &Palette, profile_id: &str, rect: Rect) {
    if let Some(color) = color_of(ui.ctx(), profile_id) {
        ui.painter()
            .rect_stroke(rect, RADIUS, Stroke::new(1.5, color.rgb(p.dark)), StrokeKind::Inside);
    }
}

/// Up to `max` tags as small chips, then how many more there are. Follows
/// the direction of the layout it is drawn in.
pub fn tag_chips(ui: &mut Ui, p: &Palette, tags: &[String], max: usize) {
    if tags.is_empty() {
        return;
    }
    let mut chips: Vec<String> = tags.iter().take(max).cloned().collect();
    if tags.len() > max {
        chips.push(format!("+{}", tags.len() - max));
    }
    if ui.layout().main_dir() == Direction::RightToLeft {
        chips.reverse();
    }
    let spacing = std::mem::replace(&mut ui.spacing_mut().item_spacing.x, 4.0);
    for chip in chips {
        let galley = ui.painter().layout_no_wrap(chip, FontId::proportional(11.0), p.muted);
        let (rect, _) = ui.allocate_exact_size(galley.size() + vec2(10.0, 4.0), Sense::hover());
        ui.painter().rect_filled(rect, 5, p.surface_hover);
        ui.painter().galley(rect.min + vec2(5.0, 2.0), galley, p.muted);
    }
    ui.spacing_mut().item_spacing.x = spacing;
}

// ---- Choosing a colour -----------------------------------------------------------

/// The swatches of the connection form: none, then each colour.
pub fn color_swatches(ui: &mut Ui, p: &Palette, color: &mut Option<HostColor>) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for choice in std::iter::once(None).chain(HostColor::ALL.map(Some)) {
            let (rect, response) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::click());
            let center = rect.center();
            let painter = ui.painter();
            match choice {
                Some(choice) => {
                    painter.circle_filled(center, 8.0, choice.rgb(p.dark));
                }
                None => {
                    let stroke = Stroke::new(1.5, p.muted);
                    painter.circle_stroke(center, 7.5, stroke);
                    let offset = vec2(5.3, -5.3);
                    painter.line_segment([center - offset, center + offset], stroke);
                }
            }
            if *color == choice {
                painter.circle_stroke(center, 11.0, Stroke::new(1.5, p.text));
            } else if response.hovered() {
                painter.circle_stroke(center, 11.0, Stroke::new(1.0, p.faint));
            }
            let label = choice.map_or("Aucune couleur", HostColor::label);
            if response.on_hover_cursor(CursorIcon::PointingHand).on_hover_text(label).clicked() {
                *color = choice;
            }
        }
    });
}

/// The entries of a "Couleur" submenu. Returns the colour picked, if any.
fn color_menu(ui: &mut Ui, p: &Palette, current: Option<HostColor>) -> Option<Option<HostColor>> {
    const WIDTH: f32 = 170.0;
    let mut picked = None;
    for choice in std::iter::once(None).chain(HostColor::ALL.map(Some)) {
        let (rect, response) = ui.allocate_exact_size(vec2(WIDTH, 26.0), Sense::click());
        let painter = ui.painter();
        if response.hovered() {
            painter.rect_filled(rect, RADIUS, p.surface_hover);
        }
        let dot = pos2(rect.left() + 14.0, rect.center().y);
        match choice {
            Some(choice) => {
                painter.circle_filled(dot, 6.0, choice.rgb(p.dark));
            }
            None => {
                painter.circle_stroke(dot, 5.5, Stroke::new(1.2, p.muted));
            }
        }
        let label = choice.map_or("Aucune", HostColor::label);
        painter.text(
            pos2(rect.left() + 30.0, rect.center().y),
            Align2::LEFT_CENTER,
            label,
            FontId::proportional(14.0),
            p.text,
        );
        if choice == current {
            painter.text(
                pos2(rect.right() - 12.0, rect.center().y),
                Align2::RIGHT_CENTER,
                icon::CHECK,
                FontId::proportional(13.0),
                p.muted,
            );
        }
        if response.on_hover_cursor(CursorIcon::PointingHand).clicked() {
            picked = Some(choice);
            ui.close();
        }
    }
    picked
}

fn menu_item(ui: &mut Ui, text: impl Into<egui::WidgetText>) -> bool {
    let clicked = ui.button(text).clicked();
    if clicked {
        ui.close();
    }
    clicked
}

/// The entries of the menu of a connection: its colour, its tags, its
/// export.
pub fn host_menu(ui: &mut Ui, p: &Palette, profile: &Profile, actions: &mut Vec<Action>) {
    let id = || profile.id.clone();
    ui.menu_button(format!("{}  Couleur", icon::PALETTE), |ui| {
        if let Some(color) = color_menu(ui, p, profile.color) {
            actions.push(Action::Hosts(HostsAction::SetColor(id(), color)));
        }
    });
    if menu_item(ui, format!("{}  Étiquettes…", icon::TAG)) {
        actions.push(Action::Hosts(HostsAction::AskTags(id())));
    }
    if menu_item(ui, format!("{}  Exporter…", icon::EXPORT)) {
        actions.push(Action::Hosts(HostsAction::AskExport(ExportScope::Host(id()))));
    }
}

/// The entry of the menu of a tab: the colour of its connection.
pub fn tab_menu(ui: &mut Ui, p: &Palette, profile_id: &str, actions: &mut Vec<Action>) {
    let current = color_of(ui.ctx(), profile_id);
    ui.menu_button(format!("{}  Couleur de la connexion", icon::PALETTE), |ui| {
        if let Some(color) = color_menu(ui, p, current) {
            actions.push(Action::Hosts(HostsAction::SetColor(profile_id.to_owned(), color)));
        }
    });
}

/// The entries of the menu of a folder: a colour for all it holds, and its
/// export.
pub fn folder_menu(ui: &mut Ui, p: &Palette, path: &str, actions: &mut Vec<Action>) {
    ui.menu_button(format!("{}  Couleur des connexions", icon::PALETTE), |ui| {
        if let Some(color) = color_menu(ui, p, None) {
            actions.push(Action::Hosts(HostsAction::SetFolderColor(path.to_owned(), color)));
        }
    });
    if menu_item(ui, format!("{}  Exporter le dossier…", icon::EXPORT)) {
        actions.push(Action::Hosts(HostsAction::AskExport(ExportScope::Folder(path.to_owned()))));
    }
}

/// The export button of the header of the list.
pub fn export_button(ui: &mut Ui, p: &Palette, actions: &mut Vec<Action>) {
    let tooltip = "Exporter les connexions… (fichier chiffré par mot de passe)";
    if icon_button(ui, p, icon::EXPORT, tooltip, true).clicked() {
        actions.push(Action::Hosts(HostsAction::AskExport(ExportScope::All)));
    }
}

/// The option of the menu of the row of tabs.
pub fn restore_item(ui: &mut Ui, actions: &mut Vec<Action>) {
    let enabled = published(ui.ctx()).restore_tabs;
    ui.separator();
    let mark = if enabled { icon::CHECK } else { "    " };
    let button = egui::Button::new(format!("{mark}  Rouvrir les onglets au démarrage"));
    if ui
        .add(button)
        .on_hover_text("Les onglets ouverts à la fermeture reviennent au prochain lancement ;\nseuls ceux affichés se connectent tout de suite.")
        .clicked()
    {
        ui.close();
        actions.push(Action::Hosts(HostsAction::SetRestoreTabs(!enabled)));
    }
}

/// The encrypted export as one more source of the import dialog. Sets
/// `vault` and confirms the dialog when chosen.
pub fn vault_source(ui: &mut Ui, p: &Palette, vault: &mut bool, outcome: &mut Outcome) {
    Frame::new()
        .fill(p.surface)
        .corner_radius(RADIUS)
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon::LOCK_KEY).size(20.0).color(p.accent));
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.label(RichText::new("Sauvegarde chiffrée de Farfile").color(p.text));
                    ui.label(small(p, format!("Un fichier .{} exporté ici ou sur un autre ordinateur", hosts::vault::EXTENSION)));
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button(format!("{}  Ouvrir…", icon::FOLDER_OPEN)).clicked() {
                        *vault = true;
                        *outcome = Outcome::Confirmed;
                    }
                });
            });
        });
    ui.add_space(6.0);
}

// ---- Editing tags --------------------------------------------------------------

/// What a tag field remembers between frames.
#[derive(Default)]
pub struct TagField {
    /// Every tag in use, offered to complete what is typed.
    pub known: Vec<String>,
    /// The tag being typed.
    pub input: String,
    /// Where the suggestions were drawn: pressing one must not first add
    /// what was typed as it is.
    suggestions: Option<Rect>,
    /// The suggestion chosen with the arrows, which Enter takes.
    highlight: Option<usize>,
}

impl TagField {
    pub fn new(known: Vec<String>) -> Self {
        Self {
            known,
            ..Self::default()
        }
    }

    /// Adds what is being typed, if anything.
    pub fn commit(&mut self, tags: &mut Vec<String>) {
        self.finish(tags);
        self.input.clear();
    }

    /// Adds what is being typed to the tags about to be saved: a form may
    /// be confirmed before the field noticed it lost the keyboard.
    pub fn finish(&self, tags: &mut Vec<String>) {
        for tag in hosts::parse_tags(&self.input) {
            hosts::add_tag(tags, &tag);
        }
    }

    /// Scripts only: types into the field.
    pub fn set_input(&mut self, text: &str) {
        text.clone_into(&mut self.input);
    }
}

/// One tag: its name and, when `removable`, a cross that removes it.
/// Returns true when the cross was clicked.
fn chip(ui: &mut Ui, p: &Palette, text: &str, removable: bool) -> bool {
    let galley = ui.painter().layout_no_wrap(text.to_owned(), FontId::proportional(12.5), p.text);
    let cross = if removable { 16.0 } else { 0.0 };
    let size = vec2(galley.size().x + 14.0 + cross, 22.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter().rect_filled(rect, 6, p.surface_hover);
    ui.painter()
        .galley(pos2(rect.left() + 7.0, rect.center().y - galley.size().y / 2.0), galley, p.text);
    if !removable {
        return false;
    }
    let button = Rect::from_center_size(pos2(rect.right() - 11.0, rect.center().y), Vec2::splat(16.0));
    let response = ui
        .interact(button, Id::new(("tag-editor-remove", text)), Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text("Retirer");
    if response.hovered() {
        ui.painter().rect_filled(button, 4, p.border);
    }
    ui.painter().text(
        button.center(),
        Align2::CENTER_CENTER,
        icon::X,
        FontId::proportional(10.0),
        if response.hovered() { p.text } else { p.muted },
    );
    response.clicked()
}

/// A field holding tags: the tags as chips, then what is being typed.
/// Enter or a comma adds a tag, Backspace in an empty field removes the
/// last one. The known tags that fit are offered, to pick with the mouse
/// or the arrows and Enter: `inline`, always, as chips below the field (a
/// dialog of its own); otherwise only while typing, floating below it so
/// that the form around keeps its height. Returns the text field, to give
/// it the keyboard.
pub fn tag_editor(
    ui: &mut Ui,
    p: &Palette,
    tags: &mut Vec<String>,
    field: &mut TagField,
    salt: &str,
    inline: bool,
) -> Response {
    let id = Id::new(("tag-editor", salt));
    let had_focus = ui.memory(|memory| memory.has_focus(id));
    let was_empty = field.input.is_empty();

    // The arrows move in the suggestions shown last frame, before the text
    // field sees them.
    let shown = field.suggestions.is_some();
    let (enter, backspace, up, down) = ui.input_mut(|input| {
        let mut arrow = |key| had_focus && shown && input.consume_key(Modifiers::NONE, key);
        let (up, down) = (arrow(Key::ArrowUp), arrow(Key::ArrowDown));
        (input.key_pressed(Key::Enter), input.key_pressed(Key::Backspace), up, down)
    });

    let mut removed = None;
    let width = ui.available_width();
    let frame = Frame::new()
        .fill(p.surface)
        .corner_radius(RADIUS)
        .inner_margin(Margin::symmetric(6, 4))
        .show(ui, |ui| {
            ui.set_width(width - 12.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
                for (index, tag) in tags.iter().enumerate() {
                    if chip(ui, p, tag, true) {
                        removed = Some(index);
                    }
                }
                let hint = if tags.is_empty() { "Ajouter des étiquettes : prod, web…" } else { "Ajouter…" };
                // The rest of the line, or a line of its own when too short.
                let left = ui.available_size_before_wrap().x;
                let room = if left >= 110.0 { left } else { ui.available_width() };
                ui.add(
                    TextEdit::singleline(&mut field.input)
                        .id(id)
                        .hint_text(hint)
                        .frame(Frame::NONE)
                        .margin(Margin::symmetric(4, 3))
                        .return_key(None)
                        .desired_width(room),
                )
            })
            .inner
        });
    let (text, field_rect) = (frame.inner, frame.response.rect);
    if let Some(index) = removed {
        tags.remove(index);
    }
    if text.changed() {
        field.highlight = None;
    }

    // A comma or a space ends a tag as it is typed.
    if field.input.contains([',', ' ', ';']) {
        let ends_open = !field.input.ends_with([',', ' ', ';']);
        let mut parts = hosts::parse_tags(&field.input);
        let rest = if ends_open { parts.pop().unwrap_or_default() } else { String::new() };
        for tag in parts {
            hosts::add_tag(tags, &tag);
        }
        field.input = rest;
    }

    // Pressing a suggestion takes the keyboard from the text field: the list
    // stays while the pointer is on it, for the click to land.
    let on_suggestion = ui.input(|input| {
        input.pointer.latest_pos().zip(field.suggestions).is_some_and(|(at, rect)| rect.contains(at))
    });
    let typing = had_focus || text.has_focus() || (shown && on_suggestion);
    let offered: Vec<String> = hosts::suggest(&field.known, &field.input, tags, if inline { 12 } else { 6 })
        .into_iter()
        .map(str::to_owned)
        .collect();
    let open = !offered.is_empty() && (inline || (typing && !field.input.trim().is_empty()));
    if !open {
        field.highlight = None;
    } else if down || up {
        let count = offered.len();
        field.highlight = Some(match (field.highlight, down) {
            (None, true) => 0,
            (None, false) => count - 1,
            (Some(at), true) => (at + 1) % count,
            (Some(at), false) => (at + count - 1) % count,
        });
    }

    let mut picked = None;
    if had_focus && enter {
        match field.highlight.and_then(|at| offered.get(at)) {
            Some(tag) => picked = Some(tag.clone()),
            None if !field.input.trim().is_empty() => {
                field.commit(tags);
                text.request_focus();
            }
            None => {}
        }
    }
    if had_focus && backspace && was_empty {
        tags.pop();
    }
    // Leaving the field keeps what was typed, unless it was for a
    // suggestion: that one is added instead.
    if text.lost_focus() && !on_suggestion {
        field.commit(tags);
    }

    field.suggestions = None;
    if open && inline {
        ui.add_space(4.0);
        let chips = ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
            for (index, tag) in offered.iter().enumerate() {
                if suggestion_chip(ui, p, tag, field.highlight == Some(index)).clicked() {
                    picked = Some(tag.clone());
                }
            }
        });
        field.suggestions = Some(chips.response.rect);
    } else if open && picked.is_none() {
        let list = egui::Area::new(id.with("suggestions"))
            .order(egui::Order::Tooltip)
            .fade_in(false)
            .fixed_pos(field_rect.left_bottom() + vec2(0.0, 4.0))
            .show(ui.ctx(), |ui| {
                Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_width(field_rect.width().min(260.0));
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for (index, tag) in offered.iter().enumerate() {
                        if suggestion(ui, p, tag, field.highlight == Some(index)).clicked() {
                            picked = Some(tag.clone());
                        }
                    }
                });
            });
        field.suggestions = Some(list.response.rect);
    }
    if let Some(tag) = picked {
        hosts::add_tag(tags, &tag);
        field.input.clear();
        field.highlight = None;
        text.request_focus();
    }
    text
}

pub fn suggestion_id(tag: &str) -> Id {
    Id::new(("tag-suggestion", tag))
}

/// A known tag offered below the field, as a chip to add.
fn suggestion_chip(ui: &mut Ui, p: &Palette, tag: &str, highlighted: bool) -> Response {
    let galley = ui.painter().layout_no_wrap(format!("+ {tag}"), FontId::proportional(12.5), p.muted);
    let (rect, _) = ui.allocate_exact_size(vec2(galley.size().x + 14.0, 22.0), Sense::hover());
    let response = ui.interact(rect, suggestion_id(tag), Sense::click());
    let lit = highlighted || response.hovered();
    ui.painter().rect_stroke(
        rect,
        6,
        Stroke::new(1.0, if lit { p.accent } else { p.border }),
        StrokeKind::Inside,
    );
    let color = if lit { p.text } else { p.muted };
    let at = pos2(rect.left() + 7.0, rect.center().y - galley.size().y / 2.0);
    ui.painter().galley_with_override_text_color(at, galley, color);
    response.on_hover_cursor(CursorIcon::PointingHand)
}

/// A known tag offered to complete what is typed.
fn suggestion(ui: &mut Ui, p: &Palette, tag: &str, highlighted: bool) -> Response {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::hover());
    let response = ui.interact(rect, suggestion_id(tag), Sense::click());
    if highlighted || response.hovered() {
        ui.painter().rect_filled(rect, 6, p.surface_hover);
    }
    ui.painter().text(
        pos2(rect.left() + 14.0, rect.center().y),
        Align2::CENTER_CENTER,
        icon::TAG,
        FontId::proportional(13.0),
        p.muted,
    );
    ui.painter().text(
        pos2(rect.left() + 28.0, rect.center().y),
        Align2::LEFT_CENTER,
        tag,
        FontId::proportional(13.5),
        p.text,
    );
    response.on_hover_cursor(CursorIcon::PointingHand)
}

/// The part of the connection form that is this feature's: one call, in
/// the form, right below the name and the folder.
pub fn form_section(ui: &mut Ui, p: &Palette, profile: &mut Profile, field: &mut TagField) {
    ui.horizontal(|ui| {
        ui.label(small(p, "Couleur"));
        ui.add_space(4.0);
        color_swatches(ui, p, &mut profile.color);
    });
    ui.add_space(2.0);
    tag_editor(ui, p, &mut profile.tags, field, "profile-form", false);
    ui.add_space(6.0);
}

/// The tags of one connection, edited from the menu of the list.
pub struct TagsForm {
    pub profile_id: String,
    pub title: String,
    pub tags: Vec<String>,
    pub field: TagField,
}

fn tags_dialog(ui: &mut Ui, p: &Palette, form: &mut TagsForm, first_frame: bool, outcome: &mut Outcome) {
    ui.set_width(380.0);
    ui.label(RichText::new("Étiquettes").font(semibold(17.0)).color(p.text));
    ui.add(Label::new(muted(p, format!("« {} »", form.title))).truncate());
    ui.add_space(10.0);
    let empty = form.field.input.trim().is_empty();
    let enter = ui.input(|input| input.key_pressed(Key::Enter));
    let field = tag_editor(ui, p, &mut form.tags, &mut form.field, "tags-dialog", true);
    if first_frame {
        field.request_focus();
    }
    ui.add_space(6.0);
    ui.label(small(
        p,
        "Entrée ou virgule pour ajouter. Dans la liste et avec Ctrl+K,\n#étiquette retrouve les connexions qui la portent.",
    ));
    // Enter in an empty field saves, as in the other small dialogs.
    if enter && empty {
        *outcome = Outcome::Confirmed;
    }
    ui.add_space(12.0);
    ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
        if primary_button(ui, p, "Enregistrer", true).clicked() {
            form.field.commit(&mut form.tags);
            *outcome = Outcome::Confirmed;
        }
        if ui.button("Annuler").clicked() {
            *outcome = Outcome::Cancelled;
        }
    });
}

/// The folder paths of the list, set back by their depth, for the export
/// and import lists.
pub(crate) fn folder_label(path: &str) -> (usize, &str) {
    (profiles::folder_depth(path), profiles::folder_name(path))
}

/// The tag field is driven here with synthetic keyboard and pointer
/// events, as a user would.
#[cfg(test)]
mod tests {
    use egui::{Context, Event, Modifiers, PointerButton, Pos2, RawInput};

    use super::*;

    struct Harness {
        ctx: Context,
        tags: Vec<String>,
        field: TagField,
        time: f64,
    }

    impl Harness {
        fn new(tags: &[&str], known: &[&str]) -> Self {
            let ctx = Context::default();
            crate::theme::install_fonts(&ctx);
            let mut harness = Self {
                ctx,
                tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
                field: TagField::new(known.iter().map(|tag| (*tag).to_owned()).collect()),
                time: 0.0,
            };
            harness.frame(Vec::new());
            harness
                .ctx
                .memory_mut(|memory| memory.request_focus(Id::new(("tag-editor", "test"))));
            harness.frame(Vec::new());
            harness.frame(Vec::new());
            harness
        }

        fn frame(&mut self, events: Vec<Event>) {
            let input = RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(420.0, 300.0))),
                time: Some(self.time),
                events,
                ..RawInput::default()
            };
            self.time += 1.0 / 60.0;
            let palette = Palette::dark();
            let (tags, field) = (&mut self.tags, &mut self.field);
            let output = self.ctx.run_ui(input, |ui| {
                tag_editor(ui, &palette, tags, field, "test", false);
            });
            output.drop_without_applying_deltas();
        }

        fn key(&mut self, key: Key) {
            self.frame(vec![Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            }]);
            self.frame(Vec::new());
        }

        fn type_text(&mut self, text: &str) {
            self.frame(vec![Event::Text(text.to_owned())]);
            self.frame(Vec::new());
        }

        fn click(&mut self, at: Pos2) {
            let button = |pressed| Event::PointerButton {
                pos: at,
                button: PointerButton::Primary,
                pressed,
                modifiers: Modifiers::NONE,
            };
            self.frame(vec![Event::PointerMoved(at)]);
            self.frame(vec![button(true)]);
            self.frame(vec![button(false)]);
            self.frame(Vec::new());
        }
    }

    #[test]
    fn enter_and_commas_make_tags() {
        let mut ui = Harness::new(&[], &[]);
        ui.type_text("prod");
        assert!(ui.tags.is_empty(), "not before it is finished");
        ui.key(Key::Enter);
        assert_eq!(ui.tags, ["prod"]);
        assert!(ui.field.input.is_empty());

        // The field keeps the keyboard: a comma or a space ends the next ones.
        ui.type_text("web, db");
        assert_eq!(ui.tags, ["prod", "web"]);
        assert_eq!(ui.field.input, "db");
        ui.type_text(" ");
        assert_eq!(ui.tags, ["prod", "web", "db"]);
        // Twice the same: once.
        ui.type_text("PROD,");
        assert_eq!(ui.tags, ["prod", "web", "db"]);
    }

    #[test]
    fn backspace_in_an_empty_field_removes_the_last_tag() {
        let mut ui = Harness::new(&["prod", "web"], &[]);
        ui.key(Key::Backspace);
        assert_eq!(ui.tags, ["prod"]);
        // While something is typed, Backspace only edits it.
        ui.type_text("ab");
        ui.key(Key::Backspace);
        assert_eq!((ui.tags.len(), ui.field.input.as_str()), (1, "a"));
    }

    #[test]
    fn a_suggestion_replaces_what_was_typed() {
        let mut ui = Harness::new(&[], &["production", "preprod", "web"]);
        ui.type_text("prod");
        // The list floats in an area of its own, placed once it has a size.
        ui.frame(Vec::new());
        // Both contain it; the one starting with it comes first.
        let first = ui.ctx.read_response(suggestion_id("production")).expect("offered");
        let second = ui.ctx.read_response(suggestion_id("preprod")).expect("offered");
        assert!(first.rect.top() < second.rect.top());
        assert!(ui.ctx.read_response(suggestion_id("web")).is_none());

        ui.click(first.rect.center());
        assert_eq!(ui.tags, ["production"], "not \"prod\" as typed");
        assert!(ui.field.input.is_empty());
    }

    #[test]
    fn the_arrows_pick_a_suggestion() {
        let mut ui = Harness::new(&[], &["production", "preprod", "web"]);
        ui.type_text("pr");
        ui.key(Key::ArrowDown);
        ui.key(Key::ArrowDown);
        ui.key(Key::Enter);
        assert_eq!(ui.tags, ["preprod"]);
        // Without a highlight, Enter keeps what was typed.
        ui.type_text("pr");
        ui.key(Key::Enter);
        assert_eq!(ui.tags, ["preprod", "pr"]);
    }

    #[test]
    fn nothing_is_offered_before_typing_in_the_form() {
        let mut ui = Harness::new(&[], &["production"]);
        ui.frame(Vec::new());
        assert!(ui.ctx.read_response(suggestion_id("production")).is_none());
    }

    #[test]
    fn leaving_the_field_keeps_what_was_typed() {
        let mut ui = Harness::new(&[], &[]);
        ui.type_text("staging");
        ui.click(Pos2::new(400.0, 280.0));
        assert_eq!(ui.tags, ["staging"]);
    }

    #[test]
    fn the_cross_of_a_tag_removes_it() {
        let mut ui = Harness::new(&["prod", "web"], &[]);
        let cross = ui
            .ctx
            .read_response(Id::new(("tag-editor-remove", "web")))
            .map(|response| response.rect.center());
        // The cross is found through its id within the field.
        let cross = cross.unwrap_or_else(|| panic!("no cross"));
        ui.click(cross);
        assert_eq!(ui.tags, ["prod"]);
    }
}
