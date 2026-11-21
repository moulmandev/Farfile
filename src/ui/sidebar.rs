//! Left column: the saved connections. Favourites come first, then the
//! folders, as a tree, then what is filed nowhere. The whole column can fold
//! into a thin rail (see [`rail`]) to leave the room to the files.

use egui::{
    Align, Align2, Color32, CursorIcon, DragAndDrop, FontId, Id, Key, Label, Layout, Popup,
    Rect, Response, RichText, ScrollArea, Sense, Stroke, StrokeKind, Ui, UiBuilder, Vec2, pos2, vec2,
};

use super::Action;
use super::widgets::{drag_label, dropped, icon, icon_button, protocol_icon};
use crate::profiles::{self, Panes, Profile, Settings};
use crate::theme::{Palette, RADIUS, filled_icon, semibold};

pub const DEFAULT_WIDTH: f32 = 250.0;
pub const MIN_WIDTH: f32 = 200.0;
pub const MAX_WIDTH: f32 = 480.0;
pub const RAIL_WIDTH: f32 = 46.0;

const ROW_HEIGHT: f32 = 28.0;
/// Where the icon and the label of a row start; the arrow of a group sits
/// before its icon.
const ICON_X: f32 = 28.0;
const LABEL_X: f32 = 44.0;
/// How far each level of the tree sits to the right of the one above.
const INDENT: f32 = 14.0;
/// From this many connections on, a search field helps finding one.
const FILTER_FROM: usize = 8;
/// Rows at least this wide also say where the connection goes.
const WIDE: f32 = 320.0;

/// What the list shows, borrowed from the app.
pub struct View<'a> {
    pub profiles: &'a [Profile],
    pub settings: &'a Settings,
    /// The connection of the tab being shown.
    pub active: Option<&'a str>,
    /// The connections that have a tab.
    pub open: &'a [&'a str],
    /// For the version menu at the foot of the column.
    pub updater: &'a crate::update::Updater,
}

/// What the list remembers from one frame to the next.
#[derive(Default)]
pub struct State {
    pub filter: String,
    /// Scripts only: open the menu of this connection, as its "…" button does.
    pub open_menu: Option<String>,
}

/// A connection being dragged to another place of the list, by its id.
struct Dragged(String);

/// A folder being dragged into another one, by its path.
struct DraggedFolder(String);

/// What a pass over the tree needs to know.
struct Tree<'a> {
    /// Every folder, by path (see [`profiles::folders`]).
    folders: &'a [String],
    /// The connections, in the order they are listed.
    sorted: &'a [&'a Profile],
    /// The folded text being searched; empty when not searching.
    needle: &'a str,
}

/// The part of the list a row belongs to: the same connection shows both in
/// the favourites and in its folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Section {
    Favorites,
    Folder,
    Root,
}

pub fn host_id(section: Section, profile_id: &str) -> Id {
    Id::new(("host", section, profile_id))
}

pub fn folder_id(name: &str) -> Id {
    Id::new(("host-folder", name))
}

pub fn favorites_id() -> Id {
    Id::new("host-favorites")
}

/// The star of a row, and its "…" button.
pub fn star_id(row: Id) -> Id {
    row.with("star")
}

pub fn more_id(row: Id) -> Id {
    row.with("more")
}

/// `needle` is already folded (see [`profiles::fold`]); `#word` looks for a
/// tag (see [`crate::hosts::matches`]).
fn matches(profile: &Profile, needle: &str) -> bool {
    needle.is_empty() || crate::hosts::matches(profile, needle)
}

pub fn show(ui: &mut Ui, p: &Palette, view: &View<'_>, state: &mut State, actions: &mut Vec<Action>) {
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        ui.add_space(4.0);
        ui.label(RichText::new(icon::PLUGS_CONNECTED).size(20.0).color(p.accent));
        ui.label(RichText::new(crate::APP_NAME).font(semibold(16.0)).color(p.text));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if icon_button(ui, p, icon::SIDEBAR_SIMPLE, "Masquer la liste (Ctrl+B)", true).clicked() {
                actions.push(Action::ToggleSidebar);
            }
        });
    });
    ui.add_space(8.0);
    super::local::sidebar_entry(ui, p, view.active, view.open, actions);

    ui.horizontal(|ui| {
        ui.add_space(4.0);
        ui.label(RichText::new("CONNEXIONS").size(11.0).color(p.faint));
        if !view.profiles.is_empty() {
            ui.label(RichText::new(view.profiles.len().to_string()).size(11.0).color(p.faint));
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            if icon_button(ui, p, icon::PLUS, "Nouvelle connexion", true).clicked() {
                actions.push(Action::NewProfile);
            }
            let import = "Importer depuis FileZilla, Windows Terminal ou ~/.ssh/config";
            if icon_button(ui, p, icon::DOWNLOAD_SIMPLE, import, true).clicked() {
                actions.push(Action::AskImport);
            }
            super::hosts::export_button(ui, p, actions);
            if icon_button(ui, p, icon::FOLDER_PLUS, "Nouveau dossier", true).clicked() {
                actions.push(Action::AskNewHostFolder {
                    parent: String::new(),
                    profile: None,
                });
            }
            super::ssh_auth::sidebar_button(ui, p, actions);
        });
    });

    // Enter in the search field opens the first connection listed.
    let mut open_first = false;
    if view.profiles.len() >= FILTER_FROM {
        let search = ui.add(
            egui::TextEdit::singleline(&mut state.filter)
                .hint_text(format!("{}  Rechercher", icon::MAGNIFYING_GLASS))
                .margin(egui::Margin::symmetric(8, 5))
                .desired_width(ui.available_width()),
        );
        if search.lost_focus() {
            let (enter, escape) =
                ui.input(|input| (input.key_pressed(Key::Enter), input.key_pressed(Key::Escape)));
            if escape {
                state.filter.clear();
            }
            open_first = enter && !state.filter.trim().is_empty();
        }
        ui.add_space(2.0);
    } else {
        state.filter.clear();
    }

    let needle = profiles::fold(state.filter.trim());
    let filtering = !needle.is_empty();
    let mut sorted: Vec<&Profile> = view.profiles.iter().collect();
    sorted.sort_by_cached_key(|profile| profiles::sort_key(profile.title()));
    let folders = profiles::folders(view.profiles, view.settings);

    // Leave room at the bottom for the theme switch.
    let list_height = (ui.available_height() - 36.0).max(0.0);
    let mut first = None;
    ScrollArea::vertical()
        .max_height(list_height)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            if view.profiles.is_empty() && folders.is_empty() {
                ui.add_space(8.0);
                ui.label(
                    RichText::new("Aucune connexion enregistrée.\nAjoutez-en une avec le bouton +.")
                        .size(12.5)
                        .color(p.muted),
                );
                return;
            }

            let mut listing = Listing {
                p,
                view,
                folders: &folders,
                open_menu: &mut state.open_menu,
                actions: &mut *actions,
                first: None,
            };
            let mut above = false;

            // While searching, each connection shows once, in its own place.
            let favorites: Vec<&Profile> = sorted.iter().copied().filter(|profile| profile.favorite).collect();
            if !filtering && !favorites.is_empty() {
                let open = !view.settings.favorites_collapsed;
                listing.favorites_header(ui, favorites.len(), open);
                if open {
                    for profile in &favorites {
                        listing.host(ui, profile, Section::Favorites, 1);
                    }
                }
                above = true;
            }

            let tree = Tree {
                folders: &folders,
                sorted: &sorted,
                needle: &needle,
            };
            for path in folders.iter().filter(|path| profiles::folder_depth(path) == 0) {
                if !tree.shows(path) {
                    continue;
                }
                if std::mem::replace(&mut above, false) {
                    ui.add_space(6.0);
                }
                listing.folder(ui, &tree, path, 0);
            }

            let loose: Vec<&Profile> = sorted
                .iter()
                .copied()
                .filter(|profile| profile.folder.trim().is_empty() && matches(profile, &needle))
                .collect();
            if !loose.is_empty() && (above || !folders.is_empty()) {
                ui.add_space(6.0);
            }
            for profile in &loose {
                listing.host(ui, profile, Section::Root, 0);
            }

            if listing.first.is_none() && filtering {
                ui.add_space(8.0);
                ui.label(RichText::new("Aucune connexion ne correspond.").size(12.5).color(p.muted));
            }
            first = listing.first.take();

            // Dropping a connection or a folder below the list takes it out
            // of the folder it is in.
            let rest = ui.available_rect_before_wrap();
            if rest.height() >= ROW_HEIGHT {
                let below = ui.interact(rest, Id::new("host-root-drop"), Sense::hover());
                if below.dnd_hover_payload::<Dragged>().is_some()
                    || below.dnd_hover_payload::<DraggedFolder>().is_some()
                {
                    let hint = Rect::from_min_size(rest.min + vec2(0.0, 6.0), vec2(rest.width(), ROW_HEIGHT));
                    ui.painter()
                        .rect_stroke(hint, RADIUS, Stroke::new(1.0, p.accent), StrokeKind::Inside);
                    ui.painter().text(
                        hint.center(),
                        Align2::CENTER_CENTER,
                        "Sortir du dossier",
                        FontId::proportional(12.0),
                        p.muted,
                    );
                }
                if let Some(dragged) = dropped::<Dragged>(&below) {
                    actions.push(Action::MoveToFolder(dragged.0.clone(), String::new()));
                }
                if let Some(dragged) = dropped::<DraggedFolder>(&below) {
                    actions.push(Action::MoveHostFolder(dragged.0.clone(), String::new()));
                }
            }
        });
    if open_first && let Some(id) = first {
        actions.push(Action::Connect(id));
    }

    ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            theme_switch(ui, p, actions);
            super::update::version_button(ui, p, view.updater, view.settings.check_updates, actions);
        });
    });
}

/// What is left of the column once folded: a way back, the search, and the
/// button to add a connection.
pub fn rail(ui: &mut Ui, p: &Palette, updater: &crate::update::Updater, check_updates: bool, actions: &mut Vec<Action>) {
    ui.add_space(2.0);
    if icon_button(ui, p, icon::SIDEBAR_SIMPLE, "Afficher la liste des connexions (Ctrl+B)", true).clicked() {
        actions.push(Action::ToggleSidebar);
    }
    if icon_button(ui, p, icon::MAGNIFYING_GLASS, "Ouvrir une connexion… (Ctrl+K)", true).clicked() {
        actions.push(Action::QuickOpen);
    }
    super::local::rail_button(ui, p, actions);
    if icon_button(ui, p, icon::PLUS, "Nouvelle connexion", true).clicked() {
        actions.push(Action::NewProfile);
    }
    ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
        theme_switch(ui, p, actions);
        super::update::version_button(ui, p, updater, check_updates, actions);
    });
}

fn theme_switch(ui: &mut Ui, p: &Palette, actions: &mut Vec<Action>) {
    let (glyph, tooltip) = if p.dark {
        (icon::SUN, "Passer au thème clair")
    } else {
        (icon::MOON, "Passer au thème sombre")
    };
    if icon_button(ui, p, glyph, tooltip, true).clicked() {
        actions.push(Action::ToggleTheme);
    }
}

/// A line of text cut with an ellipsis when it does not fit `rect`.
fn text_in(ui: &mut Ui, rect: Rect, text: RichText) {
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::left_to_right(Align::Center)),
    );
    child.add(Label::new(text).truncate().selectable(false));
}

/// A small icon button drawn over a row. It is added after the row, so it
/// gets the clicks that land on it.
fn row_button(ui: &Ui, p: &Palette, rect: Rect, id: Id, glyph: &str, font: FontId, color: Color32) -> Response {
    let response = ui.interact(rect, id, Sense::click());
    if response.hovered() {
        ui.painter().rect_filled(rect, 6, p.surface_hover);
    }
    let color = if response.hovered() && color == p.muted { p.text } else { color };
    ui.painter()
        .text(rect.center(), Align2::CENTER_CENTER, glyph, font, color);
    response.on_hover_cursor(CursorIcon::PointingHand)
}

fn menu_item(ui: &mut Ui, text: impl Into<egui::WidgetText>) -> bool {
    let clicked = ui.button(text).clicked();
    if clicked {
        ui.close();
    }
    clicked
}

struct Listing<'a> {
    p: &'a Palette,
    view: &'a View<'a>,
    folders: &'a [String],
    open_menu: &'a mut Option<String>,
    actions: &'a mut Vec<Action>,
    /// The first connection listed.
    first: Option<String>,
}

impl Listing<'_> {
    fn favorites_header(&mut self, ui: &mut Ui, count: usize, open: bool) {
        let p = self.p;
        let (_, rect) = ui.allocate_space(vec2(ui.available_width(), ROW_HEIGHT));
        let response = ui.interact(rect, favorites_id(), Sense::click());
        let dropping = response.dnd_hover_payload::<Dragged>().is_some();
        if ui.is_rect_visible(rect) {
            group_background(ui, p, rect, dropping, ui.rect_contains_pointer(rect));
            group_glyphs(ui, p, rect, open, icon::STAR, filled_icon(15.0), p.folder);
            count_label(ui, p, rect, count);
            let text = Rect::from_min_max(rect.min + vec2(LABEL_X, 0.0), rect.max - vec2(34.0, 0.0));
            text_in(ui, text, RichText::new("Favoris").color(p.text));
        }
        if let Some(dragged) = dropped::<Dragged>(&response) {
            self.actions.push(Action::SetFavorite(dragged.0.clone(), true));
        }
        if response.clicked() {
            self.actions.push(Action::ToggleFavorites);
        }
    }

    /// A folder and, while it is open, what it holds: its folders first,
    /// then its connections.
    fn folder(&mut self, ui: &mut Ui, tree: &Tree<'_>, path: &str, depth: usize) {
        let searching = !tree.needle.is_empty();
        let open = searching || !self.view.settings.collapsed.iter().any(|closed| closed == path);
        self.folder_header(ui, path, depth, tree.count(path), open);
        if !open {
            return;
        }
        for child in tree.children(path, depth) {
            if tree.shows(child) {
                self.folder(ui, tree, child, depth + 1);
            }
        }
        for profile in tree.sorted {
            if profile.folder == path && matches(profile, tree.needle) {
                self.host(ui, profile, Section::Folder, depth + 1);
            }
        }
    }

    fn folder_header(&mut self, ui: &mut Ui, path: &str, depth: usize, count: usize, open: bool) {
        let p = self.p;
        let (_, rect) = ui.allocate_space(vec2(ui.available_width(), ROW_HEIGHT));
        let id = folder_id(path);
        let response = ui.interact(rect, id, Sense::click_and_drag());
        let ctx = ui.ctx().clone();
        let more = more_id(id);
        let menu_open = Popup::is_id_open(&ctx, more.with("popup"));
        // A folder cannot go inside itself.
        let dropping = response.dnd_hover_payload::<Dragged>().is_some()
            || response
                .dnd_hover_payload::<DraggedFolder>()
                .is_some_and(|dragged| !profiles::folder_contains(&dragged.0, path));
        let pointed = ui.rect_contains_pointer(rect);
        let buttons = (pointed && !DragAndDrop::has_any_payload(&ctx)) || menu_open;

        if ui.is_rect_visible(rect) {
            group_background(ui, p, rect, dropping, pointed || menu_open);
            let inner = rect.with_min_x(rect.left() + depth as f32 * INDENT);
            let glyph = if open { icon::FOLDER_OPEN } else { icon::FOLDER };
            group_glyphs(ui, p, inner, open, glyph, FontId::proportional(16.0), p.folder);
            if !buttons {
                count_label(ui, p, rect, count);
            }
            let text = Rect::from_min_max(inner.min + vec2(LABEL_X, 0.0), rect.max - vec2(34.0, 0.0));
            text_in(ui, text, RichText::new(profiles::folder_name(path)).color(p.text));
        }
        if buttons {
            let button = Rect::from_center_size(pos2(rect.right() - 15.0, rect.center().y), Vec2::splat(22.0));
            let dots = row_button(ui, p, button, more, icon::DOTS_THREE, FontId::proportional(16.0), p.muted);
            Popup::menu(&dots).show(|ui| folder_menu(ui, p, path, self.folders, self.actions));
        }

        // Dragging a folder onto another puts it inside.
        response.dnd_set_drag_payload(DraggedFolder(path.to_owned()));
        if response.dragged() && DragAndDrop::has_payload_of_type::<DraggedFolder>(&ctx) {
            drag_label(&ctx, p, profiles::folder_name(path));
        }
        if let Some(dragged) = dropped::<Dragged>(&response) {
            self.actions
                .push(Action::MoveToFolder(dragged.0.clone(), path.to_owned()));
        }
        if let Some(dragged) = dropped::<DraggedFolder>(&response)
            && !profiles::folder_contains(&dragged.0, path)
        {
            self.actions
                .push(Action::MoveHostFolder(dragged.0.clone(), path.to_owned()));
        }
        if response.clicked() {
            self.actions.push(Action::ToggleHostFolder(path.to_owned()));
        }
        response.context_menu(|ui| folder_menu(ui, p, path, self.folders, self.actions));
    }

    /// `depth` is how many folders the row sits in.
    fn host(&mut self, ui: &mut Ui, profile: &Profile, section: Section, depth: usize) {
        let p = self.p;
        if self.first.is_none() {
            self.first = Some(profile.id.clone());
        }
        let indent = depth as f32 * INDENT;
        let (_, rect) = ui.allocate_space(vec2(ui.available_width(), ROW_HEIGHT));
        let id = host_id(section, &profile.id);
        let response = ui.interact(rect, id, Sense::click_and_drag());
        let ctx = ui.ctx().clone();

        let more = more_id(id);
        let menu_id = more.with("popup");
        if section != Section::Favorites && self.open_menu.as_deref() == Some(profile.id.as_str()) {
            *self.open_menu = None;
            Popup::open_id(&ctx, menu_id);
        }
        let menu_open = Popup::is_id_open(&ctx, menu_id);
        let dragging = DragAndDrop::has_payload_of_type::<Dragged>(&ctx);
        let pointed = ui.rect_contains_pointer(rect);
        let buttons = (pointed && !dragging) || menu_open;
        let active = self.view.active == Some(profile.id.as_str());
        // In the favourites every row has the star: no need to draw it.
        let starred = profile.favorite && section != Section::Favorites;

        if ui.is_rect_visible(rect) {
            if active {
                ui.painter().rect_filled(rect, RADIUS, p.surface);
            } else if pointed || menu_open {
                ui.painter().rect_filled(rect, RADIUS, p.surface.gamma_multiply(0.6));
            }

            let glyph_at = pos2(rect.left() + indent + ICON_X, rect.center().y);
            super::hosts::row_mark(ui, p, profile, pos2(rect.left() + indent + 12.0, rect.center().y));
            ui.painter().text(
                glyph_at,
                Align2::CENTER_CENTER,
                protocol_icon(profile.protocol),
                FontId::proportional(16.0),
                super::hosts::icon_color(p, profile, active),
            );
            // A dot marks the connections that have a tab.
            if self.view.open.contains(&profile.id.as_str()) {
                let dot = glyph_at + vec2(9.0, -7.0);
                ui.painter().circle_filled(dot, 3.5, if active { p.surface } else { p.sidebar });
                ui.painter().circle_filled(dot, 2.5, p.success);
            }

            let right = if buttons {
                52.0
            } else if starred {
                28.0
            } else {
                8.0
            };
            let text = Rect::from_min_max(
                pos2(rect.left() + indent + LABEL_X, rect.top()),
                pos2(rect.right() - right, rect.bottom()),
            );
            let name = RichText::new(profile.title()).color(p.text);
            // The addresses line up, whatever the depth of the row.
            let split = rect.left() + rect.width() * 0.56;
            if rect.width() >= WIDE && text.right() - split > 60.0 {
                text_in(ui, text.with_max_x(split - 8.0), name);
                let address = RichText::new(profile.summary()).size(12.0).color(p.muted);
                text_in(ui, text.with_min_x(split), address);
            } else {
                text_in(ui, text, name);
            }

            if starred && !buttons {
                ui.painter().text(
                    pos2(rect.right() - 15.0, rect.center().y),
                    Align2::CENTER_CENTER,
                    icon::STAR,
                    filled_icon(12.0),
                    p.folder,
                );
            }
        }

        if buttons {
            let size = Vec2::splat(22.0);
            let star = Rect::from_center_size(pos2(rect.right() - 39.0, rect.center().y), size);
            let (font, color, tooltip) = if profile.favorite {
                (filled_icon(14.0), p.folder, "Retirer des favoris")
            } else {
                (FontId::proportional(14.0), p.muted, "Ajouter aux favoris")
            };
            if row_button(ui, p, star, star_id(id), icon::STAR, font, color)
                .on_hover_text(tooltip)
                .clicked()
            {
                self.actions
                    .push(Action::SetFavorite(profile.id.clone(), !profile.favorite));
            }

            let button = Rect::from_center_size(pos2(rect.right() - 15.0, rect.center().y), size);
            let dots = row_button(ui, p, button, more, icon::DOTS_THREE, FontId::proportional(16.0), p.muted);
            Popup::menu(&dots).show(|ui| host_menu(ui, p, profile, self.folders, self.actions));
        }

        // Dragging a row files the connection where it is dropped: on a
        // folder, on the favourites, or next to another connection.
        response.dnd_set_drag_payload(Dragged(profile.id.clone()));
        if response.dragged() && dragging {
            drag_label(&ctx, p, profile.title());
        }
        if let Some(dragged) = dropped::<Dragged>(&response)
            && dragged.0 != profile.id
        {
            self.actions.push(match section {
                Section::Favorites => Action::SetFavorite(dragged.0.clone(), true),
                Section::Folder => Action::MoveToFolder(dragged.0.clone(), profile.folder.clone()),
                Section::Root => Action::MoveToFolder(dragged.0.clone(), String::new()),
            });
        }

        if response.clicked() {
            self.actions.push(Action::Connect(profile.id.clone()));
        }
        if response.middle_clicked() {
            self.actions.push(Action::OpenTab(profile.id.clone(), None));
        }
        response.context_menu(|ui| host_menu(ui, p, profile, self.folders, self.actions));

        if !dragging && !menu_open {
            let mut address = format!("{} · {}", profile.protocol.label(), profile.summary());
            if profile.port != 0 {
                address.push_str(&format!(":{}", profile.port));
            }
            super::hosts::tooltip_tags(&mut address, profile);
            response.on_hover_text(address);
        }
    }
}

impl Tree<'_> {
    /// The folders sitting right inside `path`, itself `depth` levels deep.
    fn children<'a>(&'a self, path: &'a str, depth: usize) -> impl Iterator<Item = &'a String> {
        self.folders.iter().filter(move |folder| {
            folder.as_str() != path
                && profiles::folder_depth(folder) == depth + 1
                && profiles::folder_contains(path, folder)
        })
    }

    /// How many connections the folder holds, at any depth.
    fn count(&self, path: &str) -> usize {
        self.sorted
            .iter()
            .filter(|profile| profiles::folder_contains(path, &profile.folder))
            .count()
    }

    /// While searching, only the folders holding a result are listed.
    fn shows(&self, path: &str) -> bool {
        self.needle.is_empty()
            || self
                .sorted
                .iter()
                .any(|profile| profiles::folder_contains(path, &profile.folder) && matches(profile, self.needle))
    }
}

/// A folder as an entry of a menu, set back by its depth.
fn folder_entry(path: &str, glyph: &str) -> String {
    format!(
        "{}{glyph}  {}",
        "      ".repeat(profiles::folder_depth(path)),
        profiles::folder_name(path)
    )
}

fn group_background(ui: &Ui, p: &Palette, rect: Rect, dropping: bool, pointed: bool) {
    if dropping {
        ui.painter().rect_filled(rect, RADIUS, p.selection);
        ui.painter()
            .rect_stroke(rect, RADIUS, Stroke::new(1.0, p.accent), StrokeKind::Inside);
    } else if pointed {
        ui.painter().rect_filled(rect, RADIUS, p.surface.gamma_multiply(0.6));
    }
}

/// The arrow that says whether the group is open, and its icon.
fn group_glyphs(ui: &Ui, p: &Palette, rect: Rect, open: bool, glyph: &str, font: FontId, tint: Color32) {
    ui.painter().text(
        pos2(rect.left() + 10.0, rect.center().y),
        Align2::CENTER_CENTER,
        if open { icon::CARET_DOWN } else { icon::CARET_RIGHT },
        FontId::proportional(11.0),
        p.faint,
    );
    ui.painter().text(
        pos2(rect.left() + ICON_X, rect.center().y),
        Align2::CENTER_CENTER,
        glyph,
        font,
        tint,
    );
}

fn count_label(ui: &Ui, p: &Palette, rect: Rect, count: usize) {
    ui.painter().text(
        pos2(rect.right() - 10.0, rect.center().y),
        Align2::RIGHT_CENTER,
        count.to_string(),
        FontId::proportional(11.5),
        p.faint,
    );
}

fn folder_menu(ui: &mut Ui, p: &Palette, path: &str, folders: &[String], actions: &mut Vec<Action>) {
    if menu_item(ui, format!("{}  Nouvelle connexion ici", icon::PLUS)) {
        actions.push(Action::NewProfileIn(path.to_owned()));
    }
    if menu_item(ui, format!("{}  Nouveau sous-dossier…", icon::FOLDER_PLUS)) {
        actions.push(Action::AskNewHostFolder {
            parent: path.to_owned(),
            profile: None,
        });
    }
    if menu_item(ui, format!("{}  Renommer…", icon::PENCIL_SIMPLE)) {
        actions.push(Action::AskRenameHostFolder(path.to_owned()));
    }
    let parent = profiles::folder_parent(path);
    // Anywhere but where it is, itself, or what it holds.
    let targets: Vec<&String> = folders
        .iter()
        .filter(|folder| **folder != parent && !profiles::folder_contains(path, folder))
        .collect();
    if !targets.is_empty() || !parent.is_empty() {
        ui.menu_button(format!("{}  Déplacer vers", icon::FOLDER), |ui| {
            if !parent.is_empty() && menu_item(ui, format!("{}  Le premier niveau", icon::ARROW_ELBOW_LEFT_UP)) {
                actions.push(Action::MoveHostFolder(path.to_owned(), String::new()));
            }
            for folder in targets {
                if menu_item(ui, folder_entry(folder, icon::FOLDER)) {
                    actions.push(Action::MoveHostFolder(path.to_owned(), folder.clone()));
                }
            }
        });
    }
    super::hosts::folder_menu(ui, p, path, actions);
    ui.separator();
    let delete = RichText::new(format!("{}  Supprimer le dossier…", icon::TRASH)).color(p.danger);
    if menu_item(ui, delete) {
        actions.push(Action::AskDeleteHostFolder(path.to_owned()));
    }
}

fn host_menu(ui: &mut Ui, p: &Palette, profile: &Profile, folders: &[String], actions: &mut Vec<Action>) {
    let id = || profile.id.clone();
    if menu_item(ui, format!("{}  Ouvrir", icon::PLUGS_CONNECTED)) {
        actions.push(Action::Connect(id()));
    }
    if menu_item(ui, format!("{}  Ouvrir dans un nouvel onglet", icon::PLUS)) {
        actions.push(Action::OpenTab(id(), None));
    }
    if profile.has_terminal() {
        if menu_item(ui, format!("{}  Nouveau terminal", icon::TERMINAL_WINDOW)) {
            actions.push(Action::OpenTab(id(), Some(Panes::Terminal)));
        }
        if menu_item(ui, format!("{}  Fichiers et terminal", icon::SQUARE_SPLIT_VERTICAL)) {
            actions.push(Action::OpenTab(id(), Some(Panes::Both)));
        }
        super::ssh_auth::host_menu_items(ui, profile, actions);
    }
    super::tunnels::host_menu(ui, profile, actions);
    ui.separator();

    let favorite = if profile.favorite { "Retirer des favoris" } else { "Ajouter aux favoris" };
    if menu_item(ui, format!("{}  {favorite}", icon::STAR)) {
        actions.push(Action::SetFavorite(id(), !profile.favorite));
    }
    let current = profile.folder.as_str();
    ui.menu_button(format!("{}  Déplacer vers", icon::FOLDER), |ui| {
        for folder in folders {
            let mark = if folder == current { icon::CHECK } else { icon::FOLDER };
            if menu_item(ui, folder_entry(folder, mark)) && folder != current {
                actions.push(Action::MoveToFolder(id(), folder.clone()));
            }
        }
        if !folders.is_empty() {
            ui.separator();
        }
        if menu_item(ui, format!("{}  Nouveau dossier…", icon::FOLDER_PLUS)) {
            actions.push(Action::AskNewHostFolder {
                parent: String::new(),
                profile: Some(id()),
            });
        }
        if !current.is_empty() && menu_item(ui, format!("{}  Sortir du dossier", icon::FOLDER_MINUS)) {
            actions.push(Action::MoveToFolder(id(), String::new()));
        }
    });
    super::hosts::host_menu(ui, p, profile, actions);
    if menu_item(ui, format!("{}  Modifier…", icon::PENCIL_SIMPLE)) {
        actions.push(Action::EditProfile(id()));
    }
    ui.separator();

    let delete = RichText::new(format!("{}  Supprimer…", icon::TRASH)).color(p.danger);
    if menu_item(ui, delete) {
        actions.push(Action::AskDeleteProfile(id()));
    }
}

/// The list is driven here with synthetic pointer events, the way a mouse
/// would: scripts cannot click in the off-screen window, so this is where
/// clicks, the star and drag and drop are checked.
#[cfg(test)]
mod tests {
    use egui::{Context, Event, Modifiers, PointerButton, Pos2, RawInput};

    use super::*;
    use crate::profiles::Protocol;

    struct Harness {
        ctx: Context,
        profiles: Vec<Profile>,
        settings: Settings,
        state: State,
        time: f64,
    }

    fn host(id: &str, folder: &str, favorite: bool) -> Profile {
        Profile {
            id: id.to_owned(),
            name: format!("Host {id}"),
            protocol: Protocol::Sftp,
            endpoint: format!("{id}.example.org"),
            user: "root".to_owned(),
            folder: folder.to_owned(),
            favorite,
            ..Profile::default()
        }
    }

    impl Harness {
        fn new(profiles: Vec<Profile>) -> Self {
            let ctx = Context::default();
            crate::theme::install_fonts(&ctx);
            crate::theme::apply(&ctx, &Palette::dark());
            let mut harness = Self {
                ctx,
                profiles,
                settings: Settings::default(),
                state: State::default(),
                time: 0.0,
            };
            // Two frames: widgets are hit against the rectangles of the
            // frame before.
            harness.frame(Vec::new());
            harness.frame(Vec::new());
            harness
        }

        fn frame(&mut self, events: Vec<Event>) -> Vec<Action> {
            let input = RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(260.0, 700.0))),
                time: Some(self.time),
                events,
                ..RawInput::default()
            };
            self.time += 1.0 / 60.0;
            let mut actions = Vec::new();
            let palette = Palette::dark();
            let output = self.ctx.run_ui(input, |ui| {
                // A frame may be laid out twice: keep the last pass only.
                actions.clear();
                let updater = crate::update::Updater::with(crate::update::Kind::Off, crate::update::Source::github());
                let view = View {
                    profiles: &self.profiles,
                    settings: &self.settings,
                    active: None,
                    open: &[],
                    updater: &updater,
                };
                show(ui, &palette, &view, &mut self.state, &mut actions);
            });
            output.drop_without_applying_deltas();
            actions
        }

        fn center(&self, id: Id) -> Pos2 {
            self.ctx
                .read_response(id)
                .unwrap_or_else(|| panic!("no widget {id:?} on screen"))
                .rect
                .center()
        }

        fn button(at: Pos2, button: PointerButton, pressed: bool) -> Event {
            Event::PointerButton {
                pos: at,
                button,
                pressed,
                modifiers: Modifiers::NONE,
            }
        }

        fn move_to(&mut self, at: Pos2) -> Vec<Action> {
            let mut actions = self.frame(vec![Event::PointerMoved(at)]);
            actions.extend(self.frame(Vec::new()));
            actions
        }

        fn click_with(&mut self, at: Pos2, button: PointerButton) -> Vec<Action> {
            let mut actions = self.move_to(at);
            actions.extend(self.frame(vec![Self::button(at, button, true)]));
            actions.extend(self.frame(vec![Self::button(at, button, false)]));
            actions.extend(self.frame(Vec::new()));
            actions
        }

        fn click(&mut self, at: Pos2) -> Vec<Action> {
            self.click_with(at, PointerButton::Primary)
        }

        fn drag(&mut self, from: Pos2, to: Pos2) -> Vec<Action> {
            let mut actions = self.move_to(from);
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

    /// What the actions say, in a form that is easy to compare.
    fn told(actions: &[Action]) -> Vec<String> {
        actions
            .iter()
            .map(|action| match action {
                Action::Connect(id) => format!("connect {id}"),
                Action::OpenTab(id, _) => format!("tab {id}"),
                Action::SetFavorite(id, favorite) => format!("favorite {id} {favorite}"),
                Action::MoveToFolder(id, folder) => format!("move {id} to [{folder}]"),
                Action::MoveHostFolder(folder, parent) => format!("folder [{folder}] into [{parent}]"),
                Action::ToggleHostFolder(name) => format!("toggle {name}"),
                Action::ToggleFavorites => "toggle favorites".to_owned(),
                _ => "other".to_owned(),
            })
            .collect()
    }

    fn fixture() -> Harness {
        Harness::new(vec![
            host("a", "", false),
            host("b", "Work", false),
            host("c", "", true),
            host("d", "Work", false),
        ])
    }

    #[test]
    fn clicking_a_row_opens_the_connection() {
        let mut ui = fixture();
        let row = ui.center(host_id(Section::Root, "a"));
        assert_eq!(told(&ui.click(row)), ["connect a"]);
        let row = ui.center(host_id(Section::Folder, "b"));
        assert_eq!(told(&ui.click_with(row, PointerButton::Middle)), ["tab b"]);
    }

    #[test]
    fn the_star_sets_the_favorite_without_opening() {
        let mut ui = fixture();
        let row = host_id(Section::Root, "a");
        // The star only shows under the pointer.
        assert!(ui.ctx.read_response(star_id(row)).is_none());
        let at = ui.center(row);
        ui.move_to(at);
        let star = ui.center(star_id(row));
        assert_eq!(told(&ui.click(star)), ["favorite a true"]);

        // In the favourites, the same button removes it.
        let row = host_id(Section::Favorites, "c");
        let at = ui.center(row);
        ui.move_to(at);
        let star = ui.center(star_id(row));
        assert_eq!(told(&ui.click(star)), ["favorite c false"]);
    }

    #[test]
    fn dragging_files_a_connection_where_it_is_dropped() {
        let mut ui = fixture();
        let a = ui.center(host_id(Section::Root, "a"));
        let folder = ui.center(folder_id("Work"));
        assert_eq!(told(&ui.drag(a, folder)), ["move a to [Work]"]);

        // On a row of a folder: next to it. On the favourites: one more.
        let d = ui.center(host_id(Section::Folder, "d"));
        assert_eq!(told(&ui.drag(a, d)), ["move a to [Work]"]);
        let favorites = ui.center(favorites_id());
        assert_eq!(told(&ui.drag(a, favorites)), ["favorite a true"]);

        // Out of the folder: on a row filed nowhere, or below the list.
        let b = ui.center(host_id(Section::Folder, "b"));
        assert_eq!(told(&ui.drag(b, a)), ["move b to []"]);
        assert_eq!(told(&ui.drag(b, Pos2::new(130.0, 560.0))), ["move b to []"]);

        // Dropped where it came from, nothing happens: neither a move nor a click.
        assert!(ui.drag(b, b + vec2(40.0, 0.0)).is_empty());
    }

    #[test]
    fn folders_open_and_close() {
        let mut ui = fixture();
        let folder = ui.center(folder_id("Work"));
        assert_eq!(told(&ui.click(folder)), ["toggle Work"]);
        let favorites = ui.center(favorites_id());
        assert_eq!(told(&ui.click(favorites)), ["toggle favorites"]);

        ui.settings.collapsed = vec!["Work".to_owned()];
        ui.frame(Vec::new());
        ui.frame(Vec::new());
        assert!(ui.ctx.read_response(host_id(Section::Folder, "b")).is_none());
        assert!(ui.ctx.read_response(host_id(Section::Root, "a")).is_some());
    }

    #[test]
    fn searching_looks_inside_closed_folders() {
        // The search field only shows once the list is long enough.
        let mut hosts = vec![host("a", "", false), host("b", "Work", false), host("c", "", true)];
        hosts.push(host("d", "Work", false));
        hosts.extend((0..FILTER_FROM).map(|index| host(&format!("x{index}"), "", false)));
        let mut ui = Harness::new(hosts);
        ui.settings.collapsed = vec!["Work".to_owned()];
        ui.state.filter = "HOST B".to_owned();
        ui.frame(Vec::new());
        ui.frame(Vec::new());
        assert!(ui.ctx.read_response(host_id(Section::Folder, "b")).is_some());
        assert!(ui.ctx.read_response(host_id(Section::Folder, "d")).is_none());
        assert!(ui.ctx.read_response(host_id(Section::Root, "a")).is_none());
        // Each connection shows once: no favourites while searching.
        assert!(ui.ctx.read_response(favorites_id()).is_none());

        // The name of a folder finds everything in it.
        ui.state.filter = "work".to_owned();
        ui.frame(Vec::new());
        ui.frame(Vec::new());
        assert!(ui.ctx.read_response(host_id(Section::Folder, "d")).is_some());
    }

    fn nested() -> Harness {
        Harness::new(vec![
            host("a", "", false),
            host("b", "Work", false),
            host("c", "Work / Web", false),
            host("d", "Work / Web / Old", false),
            host("e", "Home", false),
        ])
    }

    #[test]
    fn folders_hold_folders() {
        let mut ui = nested();
        // Every level is there, also "Work / Web / Old" that nothing but a
        // connection names, and each row sits further right than its folder.
        let x = |ui: &Harness, id: Id| ui.ctx.read_response(id).unwrap().rect;
        for path in ["Home", "Work", "Work / Web", "Work / Web / Old"] {
            assert!(ui.ctx.read_response(folder_id(path)).is_some(), "{path}");
        }
        let (work, web) = (x(&ui, folder_id("Work")), x(&ui, folder_id("Work / Web")));
        let (b, c) = (x(&ui, host_id(Section::Folder, "b")), x(&ui, host_id(Section::Folder, "c")));
        // Inside a folder: its folders first, then its connections.
        assert!(work.top() < web.top() && web.top() < c.top() && c.top() < b.top());

        // Closing a folder hides everything below it, whatever the depth.
        ui.settings.collapsed = vec!["Work".to_owned()];
        ui.frame(Vec::new());
        ui.frame(Vec::new());
        assert!(ui.ctx.read_response(folder_id("Work / Web")).is_none());
        assert!(ui.ctx.read_response(host_id(Section::Folder, "d")).is_none());
        assert!(ui.ctx.read_response(host_id(Section::Folder, "e")).is_some());

        // A closed folder inside an open one only hides its own content.
        ui.settings.collapsed = vec!["Work / Web".to_owned()];
        ui.frame(Vec::new());
        ui.frame(Vec::new());
        assert!(ui.ctx.read_response(host_id(Section::Folder, "b")).is_some());
        assert!(ui.ctx.read_response(host_id(Section::Folder, "c")).is_none());
        assert!(ui.ctx.read_response(folder_id("Work / Web / Old")).is_none());
    }

    #[test]
    fn dragging_reaches_folders_at_any_depth() {
        let mut ui = nested();
        let a = ui.center(host_id(Section::Root, "a"));
        let deep = ui.center(folder_id("Work / Web / Old"));
        assert_eq!(told(&ui.drag(a, deep)), ["move a to [Work / Web / Old]"]);
        let c = ui.center(host_id(Section::Folder, "c"));
        assert_eq!(told(&ui.drag(a, c)), ["move a to [Work / Web]"]);

        // A folder goes inside another, or back to the first level.
        let (home, web) = (ui.center(folder_id("Home")), ui.center(folder_id("Work / Web")));
        assert_eq!(told(&ui.drag(home, web)), ["folder [Home] into [Work / Web]"]);
        assert_eq!(told(&ui.drag(web, home)), ["folder [Work / Web] into [Home]"]);
        assert_eq!(told(&ui.drag(web, Pos2::new(130.0, 620.0))), ["folder [Work / Web] into []"]);

        // But never inside itself or what it holds.
        let work = ui.center(folder_id("Work"));
        assert!(ui.drag(work, deep).is_empty());
        assert!(ui.drag(work, work + vec2(30.0, 0.0)).is_empty());
    }

    #[test]
    fn searching_keeps_the_folders_leading_to_a_result() {
        let mut hosts = vec![host("c", "Work / Web", false), host("e", "Home", false)];
        hosts.extend((0..FILTER_FROM).map(|index| host(&format!("x{index}"), "Work", false)));
        let mut ui = Harness::new(hosts);
        ui.settings.collapsed = vec!["Work".to_owned(), "Work / Web".to_owned()];
        ui.state.filter = "host c".to_owned();
        ui.frame(Vec::new());
        ui.frame(Vec::new());
        assert!(ui.ctx.read_response(folder_id("Work")).is_some());
        assert!(ui.ctx.read_response(folder_id("Work / Web")).is_some());
        assert!(ui.ctx.read_response(host_id(Section::Folder, "c")).is_some());
        assert!(ui.ctx.read_response(folder_id("Home")).is_none());
        assert!(ui.ctx.read_response(host_id(Section::Folder, "x0")).is_none());
    }

    #[test]
    fn a_right_click_opens_the_menu() {
        let mut ui = fixture();
        let row = host_id(Section::Root, "a");
        let at = ui.center(row);
        let actions = ui.click_with(at, PointerButton::Secondary);
        assert!(actions.is_empty(), "a right click must not open the connection");
        assert!(Popup::is_id_open(&ui.ctx, Popup::default_response_id(&ui.ctx.read_response(row).unwrap())));
    }
}
