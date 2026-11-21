//! The file operations on screen: their menu entries, renaming in place,
//! dragging entries onto folders, the bookmarks, the optional columns, the
//! long operations in the status bar, and the dialogs (move to, clashes,
//! permissions, properties, bookmark name). See `crate::file_ops`.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use egui::text::{CCursor, CCursorRange};
use egui::{
    Align, Button, Grid, Id, Key, Label, Layout, Margin, Popup, ProgressBar, Response, RichText, Sense, Stroke,
    TextEdit, Ui, vec2,
};
use egui_extras::{Column, TableBuilder, TableRow};

use super::Action;
use super::dialogs::Outcome;
use super::widgets::{checkbox, danger_button, drag_label, dropped, icon, icon_button, muted, primary_button, small};
use crate::file_ops::{
    Dragged, Existing, OpsState, Owner, Picked, Plan, Scope, Sizing, bookmark_of, describe, refuse, resolve_path, stem,
};
use crate::format;
use crate::session::Session;
use crate::storage::ops::{Accounts, FolderSize, Operations, Properties, mode_text, octal_text, parse_octal};
use crate::storage::{Entry, EntryKind, RemotePath, Storage, storage_key};
use crate::theme::{self, Palette, semibold};

const FIELD_WIDTH: f32 = 380.0;
/// Room taken by the bookmark star next to the path.
pub const STAR_WIDTH: f32 = 32.0;
const PERMISSIONS_WIDTH: f32 = 100.0;
const OWNER_WIDTH: f32 = 140.0;
/// Below this width the optional columns give way, well before the date.
const EXTRA_BELOW: f32 = 600.0;
/// Operations quicker than this never show in the status bar.
const SHOW_AFTER: f64 = 0.4;

/// What the user asks of the file operations, applied by the app.
pub enum OpsAction {
    /// Edit the name of this entry of the current folder in place (F2).
    StartRename(String),
    /// The name typed in place.
    Rename { from: String, to: String },
    CancelRename,
    /// Ask where to send the selection.
    AskMoveTo { copy: bool },
    /// Entries dropped on a folder of their connection.
    Drop { dragged: Arc<Dragged>, dest: RemotePath },
    Copy,
    Cut,
    /// Paste into the named folder of the listing, or into the current one.
    Paste(Option<String>),
    Duplicate,
    AskPermissions,
    Properties,
    ComputeSize,
    CancelJob(u64),
    /// Stop counting the size of this folder of this connection.
    CancelSize(usize, RemotePath),
    /// Bookmark the current folder, or remove its bookmark.
    ToggleBookmark,
    GoBookmark(usize),
    AskRenameBookmark(usize),
    RemoveBookmark(usize),
    ToggleColumn(ColumnKind),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnKind {
    Permissions,
    Owner,
}

fn push(actions: &mut Vec<Action>, action: OpsAction) {
    actions.push(Action::FileOps(action));
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

/// The file menu entries of these operations, for the clicked `entry`.
/// `many`: the menu acts on a multiple selection the row is part of.
pub fn context_menu(ui: &mut Ui, session: &Session, entry: &Entry, many: bool, actions: &mut Vec<Action>) {
    let ops = session.storage.operations();
    ui.separator();
    if ops.rename && item(ui, true, format!("{}  Couper", icon::SCISSORS), "Ctrl+X") {
        push(actions, OpsAction::Cut);
    }
    if ops.copy && item(ui, true, format!("{}  Copier", icon::COPY), "Ctrl+C") {
        push(actions, OpsAction::Copy);
    }
    if let Some(clipboard) = session.file_ops.clipboard.filter(|clipboard| clipboard.here) {
        let into_folder = entry.is_dir() && !many;
        let text = if into_folder {
            format!("{}  Coller dans « {} »", icon::CLIPBOARD, entry.name)
        } else {
            format!("{}  Coller ({})", icon::CLIPBOARD, clipboard.count)
        };
        if item(ui, true, text, "Ctrl+V") {
            push(actions, OpsAction::Paste(into_folder.then(|| entry.name.clone())));
        }
    }
    if ops.copy && item(ui, true, format!("{}  Dupliquer", icon::STACK), "") {
        push(actions, OpsAction::Duplicate);
    }
    if ops.rename && item(ui, true, format!("{}  Déplacer vers…", icon::ARROW_SQUARE_RIGHT), "") {
        push(actions, OpsAction::AskMoveTo { copy: false });
    }
    if ops.copy && item(ui, true, format!("{}  Copier vers…", icon::FOLDERS), "") {
        push(actions, OpsAction::AskMoveTo { copy: true });
    }
    ui.separator();
    if ops.rename && item(ui, !many, format!("{}  Renommer", icon::CURSOR_TEXT), "F2") {
        push(actions, OpsAction::StartRename(entry.name.clone()));
    }
    if ops.permissions && item(ui, true, format!("{}  Permissions…", icon::LOCK_SIMPLE), "") {
        push(actions, OpsAction::AskPermissions);
    }
    let folders = if many { session.selected().any(Entry::is_dir) } else { entry.is_dir() };
    if folders && item(ui, true, format!("{}  Calculer la taille", icon::CALCULATOR), "") {
        push(actions, OpsAction::ComputeSize);
    }
    if item(ui, true, format!("{}  Propriétés", icon::INFO), "Alt+Entrée") {
        push(actions, OpsAction::Properties);
    }
}

/// The name of a row being renamed, as a text field: Enter or a click
/// elsewhere keeps the name typed, Escape gives up. Returns false for the
/// other rows, which show their name as usual.
pub fn rename_field(ui: &mut Ui, p: &Palette, session: &Session, entry: &Entry, actions: &mut Vec<Action>) -> bool {
    let Some(renaming) = &session.file_ops.renaming else {
        return false;
    };
    if renaming.name != entry.name || renaming.folder != session.path {
        return false;
    }
    let id = Id::new(("rename-field", renaming.serial));
    let started = id.with("started");
    let mut text = ui.data(|data| data.get_temp::<String>(id)).unwrap_or_else(|| entry.name.clone());
    let response = ui.add(
        TextEdit::singleline(&mut text)
            .id(id)
            // Selected rows tint their text; the name being typed stays plain.
            .text_color(p.text)
            .desired_width((ui.available_width() - 12.0).max(80.0))
            .margin(Margin::symmetric(6, 3)),
    );
    if !ui.data(|data| data.get_temp::<bool>(started).unwrap_or(false)) {
        ui.data_mut(|data| data.insert_temp(started, true));
        response.request_focus();
        // The name comes selected without its extension, as in Explorer.
        let mut state = TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
        let end = stem(&entry.name, entry.is_dir()).chars().count();
        state
            .cursor
            .set_char_range(Some(CCursorRange::two(CCursor::new(0), CCursor::new(end))));
        state.store(ui.ctx(), id);
    }
    if response.lost_focus() {
        let cancelled = ui.input(|input| input.key_pressed(Key::Escape));
        ui.data_mut(|data| {
            data.remove::<String>(id);
            data.remove::<bool>(started);
        });
        push(
            actions,
            if cancelled {
                OpsAction::CancelRename
            } else {
                OpsAction::Rename {
                    from: entry.name.clone(),
                    to: text,
                }
            },
        );
    } else {
        ui.data_mut(|data| data.insert_temp(id, text));
    }
    true
}

/// Rows are dragged onto a folder row, or onto a part of the path. Dragging
/// a selected row takes the whole selection along.
pub fn row_drag_and_drop(response: &Response, p: &Palette, session: &Session, entry: &Entry, actions: &mut Vec<Action>) {
    let ctx = &response.ctx;
    let renaming = session.file_ops.renaming.as_ref().is_some_and(|renaming| renaming.name == entry.name);
    if response.drag_started() && session.writable() && !renaming {
        let items: Vec<Picked> = if session.selection.contains(&entry.name) {
            session.selected().map(|entry| Picked::of(&session.path, entry)).collect()
        } else {
            vec![Picked::of(&session.path, entry)]
        };
        egui::DragAndDrop::set_payload(
            ctx,
            Dragged {
                storage: session.storage.clone(),
                connection: session.file_ops.connection.clone(),
                folder: session.path.clone(),
                items,
                listing: crate::ui::local::listing_key(session),
            },
        );
    }
    if response.dragged()
        && let Some(dragged) = egui::DragAndDrop::payload::<Dragged>(ctx)
    {
        drag_label(ctx, p, &describe(&dragged.items));
    }
    if entry.is_dir() {
        accept_drop(response, p, session, &session.path.child(&entry.name), actions);
    }
}

/// A part of the path above takes entries dropped on it too.
pub fn crumb_drop(response: &Response, p: &Palette, session: &Session, depth: usize, actions: &mut Vec<Action>) {
    accept_drop(response, p, session, &session.path.ancestor(depth), actions);
}

/// Whether what is dragged can go into `dest` of this browser.
fn droppable(dragged: &Dragged, session: &Session, dest: &RemotePath) -> bool {
    let connection = &session.file_ops.connection;
    let same_connection = storage_key(&dragged.storage) == session.storage_key()
        || (!connection.is_empty() && dragged.connection == *connection);
    same_connection
        && dragged.listing == crate::ui::local::listing_key(session)
        && session.storage.operations().rename
        && dragged.folder != *dest
        && !dragged.items.iter().any(|item| item.path == *dest)
        && refuse(&dragged.items, dest, session.storage.has_buckets()).is_none()
}

fn accept_drop(response: &Response, p: &Palette, session: &Session, dest: &RemotePath, actions: &mut Vec<Action>) {
    let Some(dragged) = egui::DragAndDrop::payload::<Dragged>(&response.ctx) else {
        return;
    };
    if !droppable(&dragged, session, dest) {
        return;
    }
    if response.contains_pointer() {
        let painter = response.ctx.layer_painter(response.layer_id).with_clip_rect(response.interact_rect);
        let rect = response.rect.shrink(1.0);
        painter.rect_filled(rect, 6, p.accent.gamma_multiply(0.14));
        painter.rect_stroke(rect, 6, Stroke::new(1.5, p.accent), egui::StrokeKind::Inside);
    }
    if let Some(dragged) = dropped::<Dragged>(response) {
        push(actions, OpsAction::Drop { dragged, dest: dest.clone() });
    }
}

/// The star next to the path: bookmarks the folder shown, or removes it.
pub fn bookmark_star(ui: &mut Ui, p: &Palette, session: &Session, actions: &mut Vec<Action>) {
    let marked = bookmark_of(&session.file_ops.bookmarks, &session.path).is_some();
    let enabled = session.known_path().is_some();
    let (rect, response) = ui.allocate_exact_size(vec2(30.0, 30.0), if enabled { Sense::click() } else { Sense::hover() });
    if response.hovered() && enabled {
        ui.painter().rect_filled(rect, theme::RADIUS, p.surface_hover);
    }
    let (font, color) = if marked {
        (theme::filled_icon(17.0), p.folder)
    } else {
        (egui::FontId::proportional(17.0), if response.hovered() { p.text } else { p.muted })
    };
    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, icon::STAR, font, color);
    let tooltip = if marked { "Retirer des signets (Ctrl+D)" } else { "Ajouter aux signets (Ctrl+D)" };
    if response.on_hover_text(tooltip).clicked() {
        push(actions, OpsAction::ToggleBookmark);
    }
}

/// The right side of the toolbar: what Ctrl+V would paste, and the bookmarks.
pub fn toolbar(ui: &mut Ui, p: &Palette, session: &Session, compact: bool, actions: &mut Vec<Action>) {
    let caption = |glyph: &str, text: &str| {
        if compact {
            glyph.to_owned()
        } else {
            format!("{glyph}  {text}")
        }
    };
    let button = ui
        .button(caption(icon::BOOKMARKS_SIMPLE, "Signets"))
        .on_hover_text("Les dossiers gardés pour cette connexion");
    Popup::menu(&button).show(|ui| bookmarks_menu(ui, p, session, actions));

    if let Some(clipboard) = session.file_ops.clipboard.filter(|clipboard| clipboard.here)
        && session.writable()
    {
        let what = if clipboard.cut { "Déplacer ici" } else { "Copier ici" };
        let hover = format!("{what} {} (Ctrl+V)", format::count(clipboard.count, "élément", "éléments"));
        if ui
            .button(caption(icon::CLIPBOARD, &format!("Coller ({})", clipboard.count)))
            .on_hover_text(hover)
            .clicked()
        {
            push(actions, OpsAction::Paste(None));
        }
    }
}

fn bookmarks_menu(ui: &mut Ui, p: &Palette, session: &Session, actions: &mut Vec<Action>) {
    const WIDTH: f32 = 300.0;
    ui.set_min_width(WIDTH);
    let bookmarks = &session.file_ops.bookmarks;
    if bookmarks.is_empty() {
        ui.add_space(4.0);
        ui.label(muted(p, "Aucun signet pour cette connexion."));
        ui.label(small(p, "L'étoile à côté du chemin, ou Ctrl+D, garde le dossier affiché."));
        ui.add_space(4.0);
    }
    let current = session.path.to_string();
    for (index, bookmark) in bookmarks.iter().enumerate() {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let width = ui.available_width() - 2.0 * 32.0;
            let (rect, row) = ui.allocate_exact_size(vec2(width, 38.0), Sense::click());
            let here = bookmark.path == current;
            if row.hovered() || here {
                let fill = if row.hovered() { p.surface_hover } else { p.surface };
                ui.painter().rect_filled(rect, 6, fill);
            }
            ui.painter().text(
                rect.left_center() + vec2(14.0, 0.0),
                egui::Align2::CENTER_CENTER,
                icon::FOLDER_SIMPLE_STAR,
                egui::FontId::proportional(16.0),
                p.folder,
            );
            let text = egui::Rect::from_min_max(rect.min + vec2(30.0, 3.0), rect.max - vec2(6.0, 3.0));
            let mut lines = ui.new_child(egui::UiBuilder::new().max_rect(text).layout(Layout::top_down(Align::Min)));
            lines.spacing_mut().item_spacing.y = 0.0;
            lines.add(Label::new(RichText::new(&bookmark.name).color(p.text)).truncate().selectable(false));
            lines.add(Label::new(RichText::new(&bookmark.path).size(11.5).color(p.muted)).truncate().selectable(false));
            if row.on_hover_text(&bookmark.path).clicked() {
                push(actions, OpsAction::GoBookmark(index));
                ui.close();
            }
            if icon_button(ui, p, icon::PENCIL_SIMPLE, "Renommer le signet", true).clicked() {
                push(actions, OpsAction::AskRenameBookmark(index));
                ui.close();
            }
            if icon_button(ui, p, icon::X, "Retirer le signet", true).clicked() {
                push(actions, OpsAction::RemoveBookmark(index));
                ui.close();
            }
        });
    }
    ui.separator();
    let marked = bookmark_of(bookmarks, &session.path).is_some();
    let text = if marked {
        format!("{}  Retirer ce dossier des signets", icon::STAR)
    } else {
        format!("{}  Ajouter ce dossier aux signets", icon::STAR)
    };
    if item(ui, session.known_path().is_some(), text, "Ctrl+D") {
        push(actions, OpsAction::ToggleBookmark);
    }
}

/// The size column: what was counted for a folder, with an ellipsis while
/// the count goes on.
pub fn size_text(session: &Session, entry: &Entry) -> String {
    let text = entry.size.map(format::size).unwrap_or_else(|| "—".to_owned());
    if entry.is_dir() && session.file_ops.sizing.contains(&entry.name) {
        format!("{text}…")
    } else {
        text
    }
}

/// The optional columns the table shows: those asked for, when the
/// connection tells them and the view is wide enough.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Extra {
    pub permissions: bool,
    pub owner: bool,
}

impl Extra {
    pub fn new(session: &Session, width: f32) -> Self {
        let asked = session.file_ops.columns;
        if !session.storage.operations().details || width < EXTRA_BELOW {
            return Self::default();
        }
        let permissions = asked.permissions;
        let owner = asked.owner && width >= EXTRA_BELOW + if permissions { PERMISSIONS_WIDTH } else { 0.0 };
        Self { permissions, owner }
    }

    pub fn columns(self, mut builder: TableBuilder<'_>) -> TableBuilder<'_> {
        if self.permissions {
            builder = builder.column(Column::exact(PERMISSIONS_WIDTH));
        }
        if self.owner {
            builder = builder.column(Column::exact(OWNER_WIDTH));
        }
        builder
    }

    pub fn headers(self, header: &mut TableRow<'_, '_>, p: &Palette) {
        let title = |ui: &mut Ui, text: &str| {
            ui.add_space(12.0);
            ui.add(Label::new(RichText::new(text).size(12.5).color(p.muted)).selectable(false));
        };
        if self.permissions {
            header.col(|ui| title(ui, "Permissions"));
        }
        if self.owner {
            header.col(|ui| title(ui, "Propriétaire"));
        }
    }

    pub fn cells(self, row: &mut TableRow<'_, '_>, p: &Palette, entry: &Entry) {
        if self.permissions {
            row.col(|ui| {
                ui.add_space(12.0);
                match entry.permissions {
                    Some(mode) => {
                        ui.add(Label::new(muted(p, mode_text(mode))).selectable(false))
                            .on_hover_text(octal_text(mode));
                    }
                    None => {
                        ui.add(Label::new(muted(p, "—")).selectable(false));
                    }
                }
            });
        }
        if self.owner {
            row.col(|ui| {
                ui.add_space(12.0);
                let text = match (&entry.owner, &entry.group) {
                    (Some(owner), Some(group)) => format!("{owner} · {group}"),
                    (Some(owner), None) => owner.clone(),
                    _ => "—".to_owned(),
                };
                ui.add(Label::new(muted(p, text)).truncate().selectable(false));
            });
        }
    }
}

/// The menu of the column titles, opened by a right click anywhere on
/// them: the titles that sort take the clicks before the row does.
pub fn header_menu(header: &Response, p: &Palette, session: &Session, actions: &mut Vec<Action>) {
    let rect = header.rect;
    let asked = header.ctx.input(|input| {
        input.pointer.secondary_clicked() && input.pointer.interact_pos().is_some_and(|pos| rect.contains(pos))
    });
    Popup::menu(header)
        .open_memory(asked.then_some(egui::SetOpenCommand::Bool(true)))
        .at_pointer_fixed()
        .show(|ui| columns_menu(ui, p, session, actions));
}

fn columns_menu(ui: &mut Ui, p: &Palette, session: &Session, actions: &mut Vec<Action>) {
    ui.set_min_width(220.0);
    let details = session.storage.operations().details;
    let asked = session.file_ops.columns;
    let mark = |on: bool| if on { icon::CHECK } else { "    " };
    if item(ui, details, format!("{}  Permissions", mark(asked.permissions)), "") {
        push(actions, OpsAction::ToggleColumn(ColumnKind::Permissions));
    }
    if item(ui, details, format!("{}  Propriétaire", mark(asked.owner)), "") {
        push(actions, OpsAction::ToggleColumn(ColumnKind::Owner));
    }
    let note = if details {
        "Masquées quand la vue est étroite."
    } else {
        "Ce type de connexion n'a ni permissions ni propriétaire."
    };
    ui.label(small(p, note));
}

/// The long operations under way, in the status bar; the menu lists them,
/// each with a way to stop it.
pub fn status(ui: &mut Ui, p: &Palette, state: &OpsState, actions: &mut Vec<Action>) {
    let now = ui.input(|input| input.time);
    let jobs: Vec<_> = state.jobs.iter().filter(|job| now - job.started > SHOW_AFTER).collect();
    let sizes: Vec<_> = state
        .sizes
        .iter()
        .filter(|(_, sizing)| sizing.is_running() && now - sizing.started > SHOW_AFTER)
        .collect();
    if jobs.is_empty() && sizes.is_empty() {
        return;
    }
    let label = match (jobs.as_slice(), sizes.as_slice()) {
        ([job], []) => format!("{} · {}", job.label, job_figures(job)),
        ([], [((_, path), sizing)]) => format!("Taille de « {} » · {}", path.name(), size_figures(sizing.tally.snapshot())),
        _ => format::count(jobs.len() + sizes.len(), "opération en cours", "opérations en cours"),
    };
    let button = ui.add(Button::new(RichText::new(label).size(12.0).color(p.accent)).frame(false));
    ui.add(egui::Spinner::new().size(12.0).color(p.accent));
    Popup::menu(&button).show(|ui| {
        ui.set_min_width(360.0);
        for job in &jobs {
            let (done, total) = job.progress.snapshot();
            let fraction = if total > 0 {
                done as f32 / total as f32
            } else {
                let count = job.count.load(Ordering::Relaxed).max(1);
                job.done.load(Ordering::Relaxed) as f32 / count as f32
            };
            if operation_row(ui, p, &job.label, &job_figures(job), Some(fraction.min(1.0))) {
                push(actions, OpsAction::CancelJob(job.id));
            }
        }
        for ((key, path), sizing) in &sizes {
            let label = format!("Taille de « {} »", path.name());
            if operation_row(ui, p, &label, &size_figures(sizing.tally.snapshot()), None) {
                push(actions, OpsAction::CancelSize(*key, path.clone()));
            }
        }
    });
}

/// One operation in the menu of the status bar. Returns true to stop it.
fn operation_row(ui: &mut Ui, p: &Palette, label: &str, figures: &str, fraction: Option<f32>) -> bool {
    let mut stop = false;
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.set_width(310.0);
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.add(Label::new(RichText::new(label).color(p.text)).truncate());
            ui.label(small(p, figures));
            let bar = ProgressBar::new(fraction.unwrap_or(0.0)).desired_height(4.0).fill(p.accent);
            ui.add(if fraction.is_none() { bar.animate(true) } else { bar });
        });
        stop = icon_button(ui, p, icon::X, "Arrêter", true).clicked();
    });
    ui.add_space(4.0);
    stop
}

fn job_figures(job: &crate::file_ops::Job) -> String {
    let (done, total) = job.progress.snapshot();
    let (items, count) = (job.done.load(Ordering::Relaxed), job.count.load(Ordering::Relaxed));
    let mut text = if count > 1 { format!("{items} sur {count}") } else { String::new() };
    if total > 0 {
        if !text.is_empty() {
            text.push_str(" · ");
        }
        text.push_str(&format!("{} sur {}", format::size(done), format::size(total)));
    }
    if text.is_empty() { "en cours".to_owned() } else { text }
}

fn size_figures(size: FolderSize) -> String {
    format!(
        "{} · {}",
        format::count(size.files as usize, "fichier", "fichiers"),
        format::size(size.bytes)
    )
}

/// A folder size for display: « 1 234 fichiers, 56 dossiers · 3,4 Go ».
pub fn folder_summary(size: FolderSize) -> String {
    format!(
        "{}, {} · {}",
        format::count(size.files as usize, "fichier", "fichiers"),
        format::count(size.folders as usize, "dossier", "dossiers"),
        format::size(size.bytes)
    )
}

/// `12345678` → `12 345 678`.
fn digits(value: u64) -> String {
    let text = value.to_string();
    let mut grouped = String::new();
    for (index, c) in text.chars().enumerate() {
        if index > 0 && (text.len() - index).is_multiple_of(3) {
            grouped.push(' ');
        }
        grouped.push(c);
    }
    grouped
}

// ----- Dialogs -----

pub enum OpsDialog {
    MoveTo(MoveForm),
    Clashes(ClashForm),
    Permissions(Box<PermissionsForm>),
    Properties(Box<PropertiesView>),
    /// `profile` is the connection the bookmark belongs to.
    RenameBookmark { profile: String, index: usize, name: String },
}

/// Where to move or copy the selection to.
pub struct MoveForm {
    pub storage: Arc<dyn Storage>,
    pub items: Vec<Picked>,
    pub copy: bool,
    pub path: String,
    /// The folder the items are in.
    pub from: RemotePath,
    pub has_buckets: bool,
    /// Places a click away: the folder above and the bookmarks.
    pub places: Vec<(String, String)>,
}

impl MoveForm {
    /// The destination typed, if the items can go there. An empty error
    /// says nothing worth showing.
    pub fn destination(&self) -> Result<RemotePath, String> {
        if self.path.trim().is_empty() {
            return Err(String::new());
        }
        let dest = resolve_path(&self.from, &self.path);
        if let Some(why) = refuse(&self.items, &dest, self.has_buckets) {
            return Err(why);
        }
        if !self.copy && dest == self.from {
            return Err("Les éléments sont déjà dans ce dossier.".to_owned());
        }
        Ok(dest)
    }
}

/// What is already at the destination of a move or a copy.
pub struct ClashForm {
    pub plan: Plan,
    pub clashes: Vec<(String, bool)>,
    pub choice: Existing,
}

pub struct PermissionsForm {
    pub storage: Arc<dyn Storage>,
    pub items: Vec<Picked>,
    pub mode: u32,
    pub octal: String,
    /// The mode was changed: only then is it applied, so that changing the
    /// owner of several entries keeps their own modes.
    pub touched: bool,
    pub recursive: bool,
    pub scope: Scope,
    pub can_owner: bool,
    pub owner: String,
    pub group: String,
    pub initial_owner: String,
    pub initial_group: String,
    pub accounts: Option<Arc<Accounts>>,
    pub error: Option<String>,
    /// The items do not all have the mode shown.
    pub mixed: bool,
}

impl PermissionsForm {
    pub fn new(
        storage: Arc<dyn Storage>,
        items: Vec<Picked>,
        modes: &[Option<u32>],
        owner: Option<String>,
        group: Option<String>,
    ) -> Self {
        let can_owner = storage.operations().owner;
        let known: Vec<u32> = modes.iter().flatten().copied().collect();
        let mode = known.first().copied().unwrap_or(if items.iter().all(|item| item.is_dir) { 0o755 } else { 0o644 });
        let mixed = known.iter().any(|other| *other != mode) || known.len() < modes.len();
        let (owner, group) = (owner.unwrap_or_default(), group.unwrap_or_default());
        Self {
            storage,
            items,
            octal: octal_text(mode),
            mode,
            touched: false,
            recursive: false,
            scope: Scope::All,
            can_owner,
            initial_owner: owner.clone(),
            initial_group: group.clone(),
            owner,
            group,
            accounts: None,
            error: None,
            mixed,
        }
    }

    pub fn set_bit(&mut self, bit: u32, on: bool) {
        if on {
            self.mode |= bit;
        } else {
            self.mode &= !bit;
        }
        self.octal = octal_text(self.mode);
        self.touched = true;
    }

    /// Follows what is typed in the octal field.
    pub fn typed(&mut self) {
        if let Some(mode) = parse_octal(&self.octal) {
            self.mode = mode;
            self.touched = true;
        }
    }

    pub fn has_folders(&self) -> bool {
        self.items.iter().any(|item| item.is_dir)
    }

    /// The ids to give, when owner or group changed. Names are looked up in
    /// the server's accounts; numbers are taken as they are.
    pub fn owner_change(&self) -> Result<Option<Owner>, String> {
        if !self.can_owner {
            return Ok(None);
        }
        let empty = Accounts::default();
        let accounts = self.accounts.as_deref().unwrap_or(&empty);
        let pick = |typed: &str, initial: &str, find: &dyn Fn(&str) -> Option<u32>, what: &str| {
            let typed = typed.trim();
            if typed.is_empty() || typed == initial.trim() {
                return Ok(None);
            }
            find(typed).map(Some).ok_or_else(|| format!("{what} « {typed} » inconnu du serveur : indiquez son numéro."))
        };
        let uid = pick(&self.owner, &self.initial_owner, &|text| accounts.user_id(text), "Utilisateur")?;
        let gid = pick(&self.group, &self.initial_group, &|text| accounts.group_id(text), "Groupe")?;
        Ok((uid.is_some() || gid.is_some()).then_some((uid, gid)))
    }

    /// Whether applying would change anything.
    pub fn changes(&self) -> bool {
        self.touched || self.recursive || matches!(self.owner_change(), Ok(Some(_)))
    }
}

pub struct PropertiesView {
    pub storage: Arc<dyn Storage>,
    pub items: Vec<Picked>,
    /// The listing's entries of the items, in the same order.
    pub entries: Vec<Entry>,
    pub storage_key: usize,
    pub connection: String,
    pub url: Option<String>,
    pub operations: Operations,
    pub request: u64,
    /// What the server said, for one item; `None` while waiting.
    pub loaded: Option<Result<Properties, String>>,
    /// The size counts of the folders among the items, kept up to date.
    pub sizes: Vec<Option<Arc<Sizing>>>,
    /// Asked from the dialog, done by the app on the next frame.
    pub wants_size: bool,
    pub wants_permissions: bool,
    pub stop_size: bool,
}

/// Draws one of these dialogs.
pub fn dialog(ui: &mut Ui, p: &Palette, dialog: &mut OpsDialog, first_frame: bool, outcome: &mut Outcome) {
    match dialog {
        OpsDialog::MoveTo(form) => move_form(ui, p, form, first_frame, outcome),
        OpsDialog::Clashes(form) => clash_form(ui, p, form, outcome),
        OpsDialog::Permissions(form) => permissions_form(ui, p, form, first_frame, outcome),
        OpsDialog::Properties(view) => properties(ui, p, view, outcome),
        OpsDialog::RenameBookmark { name, .. } => rename_bookmark(ui, p, name, first_frame, outcome),
    }
}

/// What a script types in the main field of the open dialog.
pub fn fill(dialog: &mut OpsDialog, text: &str) {
    match dialog {
        OpsDialog::MoveTo(form) => form.path = text.to_owned(),
        OpsDialog::RenameBookmark { name, .. } => *name = text.to_owned(),
        OpsDialog::Permissions(form) => {
            form.octal = text.to_owned();
            form.typed();
        }
        OpsDialog::Clashes(_) | OpsDialog::Properties(_) => {}
    }
}

fn title(ui: &mut Ui, p: &Palette, text: &str) {
    ui.label(RichText::new(text).font(semibold(17.0)).color(p.text));
    ui.add_space(8.0);
}

fn buttons(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    ui.add_space(12.0);
    ui.with_layout(Layout::right_to_left(Align::Center), add);
}

fn move_form(ui: &mut Ui, p: &Palette, form: &mut MoveForm, first_frame: bool, outcome: &mut Outcome) {
    ui.set_width(FIELD_WIDTH + 40.0);
    let verb = if form.copy { "Copier" } else { "Déplacer" };
    title(ui, p, &format!("{verb} {}", describe(&form.items)));
    ui.label(small(p, format!("{}  Depuis {}", icon::FOLDER, form.from)));
    ui.add_space(8.0);

    ui.label(small(p, "Dossier de destination"));
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        let width = FIELD_WIDTH + 40.0 - if form.places.is_empty() { 0.0 } else { 32.0 };
        let field = ui.add(
            TextEdit::singleline(&mut form.path)
                .hint_text("/chemin/du/dossier, ou relatif à celui-ci")
                .desired_width(width)
                .margin(Margin::symmetric(10, 7)),
        );
        if first_frame {
            field.request_focus();
        }
        if !form.places.is_empty() {
            let pick = icon_button(ui, p, icon::CARET_DOWN, "Dossier parent et signets", true);
            Popup::menu(&pick).show(|ui| {
                ui.set_min_width(240.0);
                for (label, path) in &form.places {
                    if ui
                        .button(format!("{}  {label}", icon::FOLDER))
                        .on_hover_text(path)
                        .clicked()
                    {
                        form.path.clone_from(path);
                        ui.close();
                    }
                }
            });
        }
    });
    ui.add_space(4.0);
    let destination = form.destination();
    // Opened on the folder of the items: no reproach before anything was typed.
    let untouched = resolve_path(&form.from, &form.path) == form.from;
    match &destination {
        Err(_) if untouched && !form.copy => {
            ui.label(small(p, "Tapez le dossier de destination, ou choisissez-le dans la liste."));
        }
        Err(why) if !why.is_empty() => {
            ui.label(RichText::new(why).size(12.0).color(p.danger));
        }
        Ok(dest) => {
            ui.label(small(p, format!("{}  Vers {dest} — créé s'il n'existe pas", icon::ARROW_BEND_DOWN_RIGHT)));
        }
        Err(_) => {}
    }
    let valid = destination.is_ok();
    if valid && ui.input(|input| input.key_pressed(Key::Enter)) {
        *outcome = Outcome::Confirmed;
    }
    buttons(ui, |ui| {
        if primary_button(ui, p, verb, valid).clicked() {
            *outcome = Outcome::Confirmed;
        }
        if ui.button("Annuler").clicked() {
            *outcome = Outcome::Cancelled;
        }
    });
}

fn clash_form(ui: &mut Ui, p: &Palette, form: &mut ClashForm, outcome: &mut Outcome) {
    ui.set_width(FIELD_WIDTH + 40.0);
    let dest = if form.plan.dest.is_root() {
        "la racine".to_owned()
    } else {
        format!("« {} »", form.plan.dest.name())
    };
    let (heading, body) = match form.clashes.as_slice() {
        [(name, _)] if form.plan.rename_to.is_some() => ("Remplacer ?".to_owned(), format!("« {name} » existe déjà dans ce dossier.")),
        [(name, _)] => ("Remplacer ?".to_owned(), format!("« {name} » existe déjà dans {dest}.")),
        many => {
            let mut names: Vec<String> = many.iter().take(4).map(|(name, _)| format!("« {name} »")).collect();
            if many.len() > 4 {
                names.push(format!("et {} autres", many.len() - 4));
            }
            (
                format!("{} existent déjà", format::count(many.len(), "élément", "éléments")),
                format!("Dans {dest} : {}.", names.join(", ")),
            )
        }
    };
    title(ui, p, &heading);
    ui.add(Label::new(muted(p, body)).wrap());
    if form.clashes.iter().any(|(_, is_dir)| *is_dir) {
        ui.add_space(6.0);
        ui.add(
            Label::new(
                RichText::new("Un dossier remplacé est d'abord supprimé avec tout son contenu.")
                    .size(12.0)
                    .color(p.danger),
            )
            .wrap(),
        );
    }
    buttons(ui, |ui| {
        if danger_button(ui, p, "Remplacer").clicked() {
            form.choice = Existing::Replace;
            *outcome = Outcome::Confirmed;
        }
        let all = form.clashes.len() >= form.plan.items.len();
        let skip = if all { "Ne rien remplacer" } else { "Ignorer ceux-là" };
        if ui.button(skip).clicked() {
            form.choice = Existing::Skip;
            *outcome = Outcome::Confirmed;
        }
        if ui.button("Annuler").clicked() {
            *outcome = Outcome::Cancelled;
        }
    });
}

fn permissions_form(ui: &mut Ui, p: &Palette, form: &mut PermissionsForm, first_frame: bool, outcome: &mut Outcome) {
    ui.set_width(FIELD_WIDTH + 40.0);
    title(ui, p, &format!("Permissions de {}", describe(&form.items)));
    if form.mixed && form.items.len() > 1 {
        ui.label(small(p, "Leurs permissions diffèrent : celles du premier servent de point de départ."));
        ui.add_space(4.0);
    }

    const WHO: [(&str, u32); 3] = [("Propriétaire", 6), ("Groupe", 3), ("Autres", 0)];
    const WHAT: [(&str, u32); 3] = [("Lecture", 0o4), ("Écriture", 0o2), ("Exécution", 0o1)];
    Grid::new("permissions-grid").spacing(vec2(26.0, 8.0)).show(ui, |ui| {
        ui.label("");
        for (what, _) in WHAT {
            ui.label(small(p, what));
        }
        ui.end_row();
        for (who, shift) in WHO {
            ui.label(RichText::new(who).color(p.text));
            for (_, bit) in WHAT {
                let bit = bit << shift;
                let mut on = form.mode & bit != 0;
                if checkbox(ui, p, &mut on, "").changed() {
                    form.set_bit(bit, on);
                }
            }
            ui.end_row();
        }
    });
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Valeur octale").color(p.text));
        let field = ui.add(
            TextEdit::singleline(&mut form.octal)
                .desired_width(56.0)
                .char_limit(4)
                .margin(Margin::symmetric(10, 7)),
        );
        if first_frame {
            field.request_focus();
        }
        if field.changed() {
            form.typed();
        }
        let valid = parse_octal(&form.octal).is_some();
        let shown = if valid { mode_text(form.mode) } else { "trois ou quatre chiffres de 0 à 7".to_owned() };
        ui.label(RichText::new(shown).color(if valid { p.muted } else { p.danger }));
    });

    if form.has_folders() {
        ui.add_space(10.0);
        checkbox(ui, p, &mut form.recursive, "Appliquer aussi à leur contenu");
        if form.recursive {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.add_space(24.0);
                ui.spacing_mut().item_spacing.x = 4.0;
                for (scope, label) in [
                    (Scope::All, "Fichiers et dossiers"),
                    (Scope::Files, "Fichiers seulement"),
                    (Scope::Folders, "Dossiers seulement"),
                ] {
                    let selected = form.scope == scope;
                    let text = RichText::new(label).size(12.5).color(if selected { p.on_accent } else { p.text });
                    let mut button = Button::new(text);
                    if selected {
                        button = button.fill(p.accent);
                    }
                    if ui.add(button).clicked() {
                        form.scope = scope;
                    }
                }
            });
        }
    }

    if form.can_owner {
        ui.add_space(10.0);
        ui.separator();
        ui.add_space(4.0);
        ui.horizontal_top(|ui| {
            let half = (FIELD_WIDTH + 40.0 - ui.spacing().item_spacing.x) / 2.0;
            owner_field(ui, p, "Propriétaire", &mut form.owner, form.accounts.as_deref().map(|a| &a.users), half);
            owner_field(ui, p, "Groupe", &mut form.group, form.accounts.as_deref().map(|a| &a.groups), half);
        });
        ui.label(small(p, "Un nom ou un numéro. Changer de propriétaire demande en général d'être administrateur."));
    }

    let owner = form.owner_change();
    if let Err(why) = &owner {
        ui.add_space(4.0);
        ui.label(RichText::new(why).size(12.0).color(p.danger));
    }
    if let Some(error) = &form.error {
        ui.add_space(4.0);
        ui.label(RichText::new(error).size(12.0).color(p.danger));
    }
    let valid = parse_octal(&form.octal).is_some() && owner.is_ok() && form.changes();
    buttons(ui, |ui| {
        if primary_button(ui, p, "Appliquer", valid).clicked() {
            *outcome = Outcome::Confirmed;
        }
        if ui.button("Annuler").clicked() {
            *outcome = Outcome::Cancelled;
        }
    });
}

/// A name or id field, with the names the server knows a click away.
fn owner_field(
    ui: &mut Ui,
    p: &Palette,
    label: &str,
    value: &mut String,
    known: Option<&std::collections::BTreeMap<u32, String>>,
    width: f32,
) {
    ui.vertical(|ui| {
        ui.set_width(width);
        ui.label(small(p, label));
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let picking = known.is_some_and(|known| !known.is_empty());
            ui.add(
                TextEdit::singleline(value)
                    .desired_width(width - if picking { 34.0 } else { 0.0 })
                    .margin(Margin::symmetric(10, 7)),
            );
            if let Some(known) = known.filter(|_| picking) {
                let pick = icon_button(ui, p, icon::CARET_DOWN, "Choisir parmi ceux du serveur", true);
                Popup::menu(&pick).show(|ui| {
                    ui.set_min_width(180.0);
                    egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                        for (id, name) in known {
                            if ui.button(format!("{name}  ({id})")).clicked() {
                                value.clone_from(name);
                                ui.close();
                            }
                        }
                    });
                });
            }
        });
    });
}

fn properties(ui: &mut Ui, p: &Palette, view: &mut PropertiesView, outcome: &mut Outcome) {
    const WIDTH: f32 = 460.0;
    ui.set_width(WIDTH);
    let single = view.items.len() == 1;
    let (glyph, tint) = match view.entries.first() {
        Some(entry) if single => super::widgets::entry_icon(entry, p),
        _ => (icon::FILES, p.muted),
    };
    ui.horizontal(|ui| {
        ui.label(RichText::new(glyph).font(theme::filled_icon(26.0)).color(tint));
        let name = if single { view.items[0].path.name().to_owned() } else { describe(&view.items) };
        ui.add(Label::new(RichText::new(name).font(semibold(17.0)).color(p.text)).truncate());
    });
    ui.add_space(10.0);

    Grid::new("properties-grid")
        .num_columns(2)
        .spacing(vec2(18.0, 7.0))
        .min_col_width(120.0)
        .show(ui, |ui| {
            if single {
                single_rows(ui, p, view);
            } else {
                many_rows(ui, p, view);
            }
        });

    if single {
        match &view.loaded {
            None => {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(13.0).color(p.muted));
                    ui.label(small(p, "Lecture des détails sur le serveur…"));
                });
            }
            Some(Err(error)) => {
                ui.add_space(6.0);
                ui.add(Label::new(RichText::new(format!("Détails indisponibles : {error}")).size(12.0).color(p.danger)).wrap());
            }
            Some(Ok(_)) => {}
        }
    }

    buttons(ui, |ui| {
        if primary_button(ui, p, "Fermer", true).clicked() {
            *outcome = Outcome::Cancelled;
        }
        if view.operations.permissions && ui.button(format!("{}  Permissions…", icon::LOCK_SIMPLE)).clicked() {
            view.wants_permissions = true;
        }
    });
    if ui.input(|input| input.key_pressed(Key::Enter)) {
        *outcome = Outcome::Cancelled;
    }
}

fn row(ui: &mut Ui, p: &Palette, label: &str, value: impl Into<String>) {
    ui.label(RichText::new(label).color(p.muted));
    ui.add(Label::new(RichText::new(value.into()).color(p.text)).wrap());
    ui.end_row();
}

fn single_rows(ui: &mut Ui, p: &Palette, view: &mut PropertiesView) {
    let item = view.items[0].clone();
    let entry = view.entries.first().cloned().unwrap_or_default();
    let loaded = view.loaded.as_ref().and_then(|loaded| loaded.as_ref().ok()).cloned().unwrap_or_default();
    let link = loaded.link.clone().or_else(|| entry.link.clone());

    let kind = match (entry.kind, &link) {
        (EntryKind::Bucket, _) => "Bucket",
        (EntryKind::Folder, Some(_)) => "Lien vers un dossier",
        (EntryKind::Folder, None) => "Dossier",
        (EntryKind::File, Some(_)) => "Lien vers un fichier",
        (EntryKind::File, None) => "Fichier",
    };
    row(ui, p, "Type", kind);
    ui.label(RichText::new("Emplacement").color(p.muted));
    ui.horizontal(|ui| {
        ui.add(Label::new(RichText::new(item.path.to_string()).color(p.text)).wrap());
        if icon_button(ui, p, icon::COPY_SIMPLE, "Copier le chemin", true).clicked() {
            ui.ctx().copy_text(item.path.to_string());
        }
    });
    ui.end_row();
    if let Some(target) = &link {
        row(ui, p, "Cible du lien", target);
    }
    row(ui, p, "Connexion", &view.connection);

    if !item.is_dir {
        if let Some(size) = loaded.size.or(entry.size) {
            row(ui, p, "Taille", format!("{} ({} octets)", format::size(size), digits(size)));
        }
    } else if entry.kind != EntryKind::Bucket {
        folder_size_row(ui, p, view, 0);
    }
    if let Some(modified) = loaded.modified.or(entry.modified) {
        let label = if entry.kind == EntryKind::Bucket { "Créé" } else { "Modifié" };
        row(ui, p, label, format::date(modified));
    }
    if let Some(mode) = loaded.permissions.or(entry.permissions) {
        row(ui, p, "Permissions", format!("{}   ({})", mode_text(mode), octal_text(mode)));
    }
    let named = |name: Option<String>, id: Option<u32>| match (name, id) {
        (Some(name), Some(id)) if name != id.to_string() => Some(format!("{name} ({id})")),
        (Some(name), _) => Some(name),
        (None, Some(id)) => Some(id.to_string()),
        (None, None) => None,
    };
    if let Some(owner) = named(loaded.owner.clone().or(entry.owner.clone()), loaded.uid) {
        row(ui, p, "Propriétaire", owner);
    }
    if let Some(group) = named(loaded.group.clone().or(entry.group.clone()), loaded.gid) {
        row(ui, p, "Groupe", group);
    }
    for (label, value) in &loaded.details {
        row(ui, p, label, value);
    }
    if !loaded.metadata.is_empty() {
        ui.label(RichText::new("Métadonnées").color(p.muted));
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            for (key, value) in &loaded.metadata {
                ui.add(Label::new(RichText::new(format!("{key} : {value}")).color(p.text)).wrap());
            }
        });
        ui.end_row();
    }
    if let Some(url) = &view.url {
        ui.label(RichText::new("Adresse").color(p.muted));
        ui.horizontal(|ui| {
            ui.add(Label::new(RichText::new(url).size(12.0).color(p.text)).wrap());
            if icon_button(ui, p, icon::COPY_SIMPLE, "Copier l'adresse", true).clicked() {
                ui.ctx().copy_text(url.clone());
            }
        });
        ui.end_row();
    }
}

fn many_rows(ui: &mut Ui, p: &Palette, view: &mut PropertiesView) {
    let folders = view.items.iter().filter(|item| item.is_dir).count();
    let files = view.items.len() - folders;
    let what = match (files, folders) {
        (_, 0) => format::count(files, "fichier", "fichiers"),
        (0, _) => format::count(folders, "dossier", "dossiers"),
        _ => format!(
            "{} et {}",
            format::count(files, "fichier", "fichiers"),
            format::count(folders, "dossier", "dossiers")
        ),
    };
    row(ui, p, "Sélection", what);
    if let Some(parent) = view.items[0].path.parent() {
        row(ui, p, "Emplacement", parent.to_string());
    }
    row(ui, p, "Connexion", &view.connection);
    if files > 0 {
        let bytes: u64 = view.items.iter().filter(|item| !item.is_dir).map(|item| item.size).sum();
        row(ui, p, "Taille des fichiers", format!("{} ({} octets)", format::size(bytes), digits(bytes)));
    }
    if folders > 0 {
        folder_size_row(ui, p, view, files as u64);
    }
}

/// What the folders hold, with the count to start or stop. `extra_files`
/// are files of the selection beyond the folders, added to the totals.
fn folder_size_row(ui: &mut Ui, p: &Palette, view: &mut PropertiesView, extra_files: u64) {
    ui.label(RichText::new("Contenu").color(p.muted));
    let counted: Vec<&Arc<Sizing>> = view.sizes.iter().flatten().collect();
    let folders = view.items.iter().filter(|item| item.is_dir).count();
    ui.horizontal(|ui| {
        if counted.len() < folders || counted.iter().any(|sizing| sizing.failed()) {
            if ui.button(format!("{}  Calculer la taille", icon::CALCULATOR)).clicked() {
                view.wants_size = true;
            }
            if let Some(error) = counted.iter().find_map(|sizing| sizing.error.lock().ok().and_then(|e| e.clone())) {
                ui.label(RichText::new(error).size(12.0).color(p.danger));
            }
            return;
        }
        let mut total = FolderSize::default();
        for sizing in &counted {
            let size = sizing.tally.snapshot();
            total.files += size.files;
            total.folders += size.folders;
            total.bytes += size.bytes;
        }
        total.files += extra_files;
        if view.items.len() > 1 {
            total.folders += folders as u64;
        }
        ui.label(RichText::new(folder_summary(total)).color(p.text));
        if counted.iter().any(|sizing| sizing.is_running()) {
            ui.add(egui::Spinner::new().size(13.0).color(p.muted));
            if ui.small_button("Arrêter").clicked() {
                view.stop_size = true;
            }
        }
    });
    ui.end_row();
}

fn rename_bookmark(ui: &mut Ui, p: &Palette, name: &mut String, first_frame: bool, outcome: &mut Outcome) {
    ui.set_width(FIELD_WIDTH);
    title(ui, p, "Renommer le signet");
    ui.label(small(p, "Nom du signet"));
    let field = ui.add(
        TextEdit::singleline(name)
            .desired_width(FIELD_WIDTH)
            .margin(Margin::symmetric(10, 7)),
    );
    if first_frame {
        field.request_focus();
    }
    let valid = !name.trim().is_empty();
    if valid && ui.input(|input| input.key_pressed(Key::Enter)) {
        *outcome = Outcome::Confirmed;
    }
    buttons(ui, |ui| {
        if primary_button(ui, p, "Renommer", valid).clicked() {
            *outcome = Outcome::Confirmed;
        }
        if ui.button("Annuler").clicked() {
            *outcome = Outcome::Cancelled;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picked(path: &str, is_dir: bool) -> Picked {
        Picked {
            path: RemotePath::parse(path),
            is_dir,
            size: 0,
        }
    }

    /// A connection that is never opened: SFTP, for its operations.
    fn storage() -> Arc<dyn Storage> {
        Arc::new(crate::storage::sftp::SftpStorage::new(Arc::new(crate::storage::ssh::SshSettings {
            extra: Default::default(),
            host: "sftp.example.org".to_owned(),
            port: 22,
            user: "dev".to_owned(),
            secret: String::new(),
            key_path: None,
            known_hosts: None,
        })))
    }

    #[test]
    fn the_octal_field_and_the_grid_agree() {
        let mut form = PermissionsForm::new(storage(), vec![picked("/a", false)], &[Some(0o640)], None, None);
        assert_eq!((form.mode, form.octal.as_str(), form.touched), (0o640, "640", false));
        form.set_bit(0o004, true);
        assert_eq!((form.mode, form.octal.as_str()), (0o644, "644"));
        form.octal = "2750".to_owned();
        form.typed();
        assert_eq!(form.mode, 0o2750);
        form.set_bit(0o100, false);
        assert_eq!(form.octal, "2650", "the special bits stay");
        form.octal = "9".to_owned();
        form.typed();
        assert_eq!(form.mode, 0o2650, "a wrong value changes nothing");
        assert!(form.touched);

        // Several items with different modes, or a folder without one.
        let mixed = PermissionsForm::new(storage(), vec![picked("/a", false), picked("/b", false)], &[Some(0o600), Some(0o644)], None, None);
        assert!(mixed.mixed);
        assert_eq!(mixed.mode, 0o600);
        let unknown = PermissionsForm::new(storage(), vec![picked("/d", true)], &[None], None, None);
        assert_eq!(unknown.mode, 0o755);
    }

    #[test]
    fn owners_are_typed_by_name_or_number() {
        let mut form = PermissionsForm::new(storage(), vec![picked("/a", false)], &[Some(0o644)], Some("dev".into()), Some("dev".into()));
        form.accounts = Some(Arc::new(Accounts::parse("root:x:0:0::/:/bin/sh\ndev:x:1000:1000::/:/bin/sh\n", "dev:x:1000:\nwww:x:33:\n")));
        assert_eq!(form.owner_change(), Ok(None), "nothing changed");
        assert!(!form.changes());
        form.group = "www".into();
        assert_eq!(form.owner_change(), Ok(Some((None, Some(33)))));
        form.owner = "0".into();
        assert_eq!(form.owner_change(), Ok(Some((Some(0), Some(33)))));
        form.owner = "nobody".into();
        assert!(form.owner_change().is_err());
        assert!(form.changes() || form.owner_change().is_err());
    }

    #[test]
    fn destinations_are_checked_as_typed() {
        let mut form = MoveForm {
            storage: storage(),
            items: vec![picked("/srv/www/site", true)],
            copy: false,
            path: "/srv/www".to_owned(),
            from: RemotePath::parse("/srv/www"),
            has_buckets: false,
            places: Vec::new(),
        };
        assert!(form.destination().is_err(), "already there");
        form.copy = true;
        assert_eq!(form.destination(), Ok(RemotePath::parse("/srv/www")), "a copy next to it is fine");
        form.path = "site/old".to_owned();
        assert!(form.destination().is_err(), "inside itself");
        form.path = "../backup".to_owned();
        assert_eq!(form.destination(), Ok(RemotePath::parse("/srv/backup")));
        form.path = "  ".to_owned();
        assert_eq!(form.destination(), Err(String::new()));
    }

    /// The real file browser, driven with synthetic pointer and key events.
    struct Browser {
        ctx: egui::Context,
        session: Session,
        time: f64,
    }

    impl Browser {
        fn new() -> Self {
            let ctx = egui::Context::default();
            crate::theme::install_fonts(&ctx);
            crate::theme::apply(&ctx, &Palette::dark());
            let storage: Arc<dyn Storage> = Arc::new(crate::storage::ftp::FtpStorage::new(crate::storage::ftp::FtpSettings {
                host: "ftp.example.org".to_owned(),
                port: 21,
                user: String::new(),
                password: String::new(),
                tls: false,
            }));
            let mut session = Session::new("Test".to_owned(), storage, Some(RemotePath::parse("/srv")));
            session.begin_listing();
            let entry = |name: &str, kind| Entry {
                name: name.to_owned(),
                kind,
                size: Some(1),
                ..Entry::default()
            };
            session.listed(
                vec![
                    entry("b.txt", EntryKind::File),
                    entry("docs", EntryKind::Folder),
                    entry("a.txt", EntryKind::File),
                    entry("photos", EntryKind::Folder),
                ],
                true,
            );
            let mut browser = Self { ctx, session, time: 0.0 };
            browser.frame(Vec::new());
            browser.frame(Vec::new());
            browser
        }

        fn frame(&mut self, events: Vec<egui::Event>) -> Vec<Action> {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(1000.0, 600.0))),
                time: Some(self.time),
                events,
                ..egui::RawInput::default()
            };
            self.time += 1.0 / 60.0;
            let mut actions = Vec::new();
            let output = self.ctx.run_ui(input, |ui| {
                super::super::browser::show(ui, &Palette::dark(), &mut self.session, false, "", None, &mut actions);
            });
            output.drop_without_applying_deltas();
            actions
        }

        /// The name cells, top to bottom: the only wide areas a row high.
        fn rows(&self) -> Vec<egui::Rect> {
            let mut rows: Vec<egui::Rect> = self
                .ctx
                .interactive_rects_last_pass()
                .into_iter()
                .filter(|rect| (rect.height() - 32.0).abs() < 0.5 && rect.width() > 300.0)
                .collect();
            rows.sort_by(|a, b| a.top().total_cmp(&b.top()));
            rows.dedup_by(|a, b| (a.top() - b.top()).abs() < 1.0);
            rows
        }

        fn drag(&mut self, from: egui::Pos2, to: egui::Pos2) -> Vec<Action> {
            let button = |pos, pressed| egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            let mut actions = self.frame(vec![egui::Event::PointerMoved(from)]);
            actions.extend(self.frame(vec![button(from, true)]));
            for step in 1..=8 {
                let at = from + (to - from) * (step as f32 / 8.0);
                actions.extend(self.frame(vec![egui::Event::PointerMoved(at)]));
            }
            actions.extend(self.frame(Vec::new()));
            actions.extend(self.frame(vec![button(to, false)]));
            actions.extend(self.frame(Vec::new()));
            actions
        }
    }

    /// Where dropped entries go, and which ones.
    fn drops(actions: &[Action]) -> Vec<(String, Vec<String>)> {
        actions
            .iter()
            .filter_map(|action| match action {
                Action::FileOps(OpsAction::Drop { dragged, dest }) => {
                    Some((dest.to_string(), dragged.items.iter().map(|item| item.path.to_string()).collect()))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn rows_dragged_onto_a_folder_move_there() {
        let mut browser = Browser::new();
        let rows = browser.rows();
        assert_eq!(rows.len(), 4, "docs, photos, a.txt, b.txt: {rows:?}");
        let (docs, photos, a, b) = (rows[0].center(), rows[1].center(), rows[2].center(), rows[3].center());

        assert_eq!(drops(&browser.drag(a, docs)), [("/srv/docs".to_owned(), vec!["/srv/a.txt".to_owned()])]);

        // A selected row takes the selection along.
        browser.session.select_only("a.txt");
        browser.session.toggle("b.txt");
        let dropped = drops(&browser.drag(b, photos));
        assert_eq!(dropped.len(), 1);
        let mut items = dropped[0].1.clone();
        items.sort();
        assert_eq!((dropped[0].0.as_str(), items), ("/srv/photos", vec!["/srv/a.txt".to_owned(), "/srv/b.txt".to_owned()]));

        // Onto a file, or a folder onto itself: nothing.
        browser.session.selection.clear();
        assert!(drops(&browser.drag(a, b)).is_empty());
        assert!(drops(&browser.drag(docs, docs + vec2(40.0, 2.0))).is_empty());
    }

    #[test]
    fn a_name_typed_in_place_renames_on_enter() {
        let mut browser = Browser::new();
        browser.session.file_ops.renaming = Some(crate::file_ops::Renaming {
            folder: RemotePath::parse("/srv"),
            name: "a.txt".to_owned(),
            serial: 1,
        });
        browser.frame(Vec::new());
        browser.frame(Vec::new());
        // The name comes selected without its extension: typing replaces it.
        browser.frame(vec![egui::Event::Text("notes".to_owned())]);
        let enter = egui::Event::Key {
            key: Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        let actions = browser.frame(vec![enter]);
        let renamed: Vec<(&str, &str)> = actions
            .iter()
            .filter_map(|action| match action {
                Action::FileOps(OpsAction::Rename { from, to }) => Some((from.as_str(), to.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(renamed, [("a.txt", "notes.txt")]);
    }

    #[test]
    fn escape_gives_up_renaming() {
        let mut browser = Browser::new();
        browser.session.file_ops.renaming = Some(crate::file_ops::Renaming {
            folder: RemotePath::parse("/srv"),
            name: "docs".to_owned(),
            serial: 2,
        });
        browser.frame(Vec::new());
        browser.frame(vec![egui::Event::Text("x".to_owned())]);
        let escape = egui::Event::Key {
            key: Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        let actions = browser.frame(vec![escape]);
        assert!(actions.iter().any(|action| matches!(action, Action::FileOps(OpsAction::CancelRename))));
        assert!(!actions.iter().any(|action| matches!(action, Action::FileOps(OpsAction::Rename { .. }))));
    }

    #[test]
    fn digits_are_grouped_by_three() {
        assert_eq!(digits(0), "0");
        assert_eq!(digits(999), "999");
        assert_eq!(digits(1000), "1 000");
        assert_eq!(digits(12_345_678), "12 345 678");
    }
}
