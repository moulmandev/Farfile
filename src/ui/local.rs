//! What this computer brings to the screens: the "Cet ordinateur" entry of
//! the sidebar, the start page and the tab row, the menu of local shells,
//! and copying files from one view to the other by dragging them, by the
//! file menu or with F5.

use std::sync::Arc;

use egui::{
    Align, Align2, Button, Color32, CursorIcon, DragAndDrop, FontId, Id, Key, Label, Layout, Order, Popup, Rect,
    Response, RichText, Sense, Stroke, StrokeKind, Ui, Vec2, pos2, vec2,
};

use super::Action;
use super::dialogs::Outcome;
use super::widgets::{danger_button, dropped, icon, icon_button, muted, primary_button};
use crate::copy::{Destination, Picked, Source};
use crate::local::{self, ID};
use crate::profiles::{Panes, Protocol};
use crate::pty::{self, Shell, ShellKind};
use crate::session::Session;
use crate::storage::local::{DriveKind, Spot, TITLE};
use crate::storage::{Entry, RemotePath};
use crate::tabs::Tab;
use crate::theme::{Palette, RADIUS, semibold};
use crate::worker::Item;

/// Rows of the sidebar, as tall as those of the connections.
const ROW_HEIGHT: f32 = 28.0;
const ICON_X: f32 = 28.0;
const LABEL_X: f32 = 44.0;
/// Width the "+" button takes in the row of tabs.
pub const NEW_TAB_WIDTH: f32 = 34.0;

pub enum LocalAction {
    /// Show "Cet ordinateur" in the other view, next to this tab: the one
    /// in use when none is given.
    OpenBeside(Option<u64>),
    /// Copy what was dragged into a folder of the listing it was dropped on.
    Drop { files: Arc<DraggedFiles>, into: Destination },
    /// Copy the selection of the view in use into the folder the other view
    /// shows; `ask` first (F5).
    CopyToOther { ask: bool },
}

pub enum LocalDialog {
    /// Delete files of this computer: to the Recycle Bin when the drive has
    /// one, for good otherwise.
    Trash {
        items: Vec<Item>,
        parent: RemotePath,
        summary: String,
        recycle: bool,
    },
    /// Copy the selection to the other view? (F5)
    Copy { source: Source, dest: Destination, summary: String },
}

/// Entries of a listing dropped on the other view.
pub struct DraggedFiles {
    pub source: Source,
}

impl DraggedFiles {
    /// The rows being dragged, as the copy sees them. A drag of rows has
    /// one payload, set by `file_ops`: moves within a listing, copies here.
    fn of(dragged: &crate::file_ops::Dragged) -> Arc<Self> {
        let items = dragged
            .items
            .iter()
            .map(|item| Picked {
                name: item.path.name().to_owned(),
                is_dir: item.is_dir,
                size: item.size,
            })
            .collect();
        Arc::new(Self {
            source: Source {
                storage: dragged.storage.clone(),
                folder: dragged.folder.clone(),
                items,
            },
        })
    }
}

/// Tells one file browser from another, for as long as it lives.
pub(crate) fn listing_key(session: &Session) -> usize {
    Arc::as_ptr(&session.generation) as usize
}

fn shell_icon(shell: &Shell) -> &'static str {
    match shell.kind {
        ShellKind::Wsl(_) => icon::LINUX_LOGO,
        ShellKind::Cmd => icon::TERMINAL,
        _ => icon::TERMINAL_WINDOW,
    }
}

fn item(ui: &mut Ui, text: impl Into<egui::WidgetText>) -> bool {
    let clicked = ui.button(text).clicked();
    if clicked {
        ui.close();
    }
    clicked
}

/// One entry per shell of this computer, each opening a tab of its own.
pub fn shells_menu(ui: &mut Ui, p: &Palette, actions: &mut Vec<Action>) {
    ui.set_min_width(210.0);
    for shell in pty::shells() {
        if item(ui, format!("{}  {}", shell_icon(&shell), shell.label)) {
            actions.push(Action::OpenTab(local::shell_id(&shell), Some(Panes::Terminal)));
        }
    }
    if !pty::wsl_known() {
        ui.label(RichText::new(tr!("Recherche des distributions WSL…")).size(11.5).color(p.faint));
    }
}

/// What can be done with "Cet ordinateur": its menu in the sidebar.
fn computer_menu(ui: &mut Ui, p: &Palette, actions: &mut Vec<Action>) {
    ui.set_min_width(230.0);
    if item(ui, format!("{}  {}", icon::DESKTOP, tr!("Ouvrir"))) {
        actions.push(Action::Connect(ID.to_owned()));
    }
    if item(ui, format!("{}  {}", icon::PLUS, tr!("Ouvrir dans un nouvel onglet"))) {
        actions.push(Action::OpenTab(ID.to_owned(), None));
    }
    if item(ui, format!("{}  {}", icon::SQUARE_SPLIT_VERTICAL, tr!("Fichiers et terminal"))) {
        actions.push(Action::OpenTab(ID.to_owned(), Some(Panes::Both)));
    }
    ui.separator();
    ui.menu_button(format!("{}  {}", icon::TERMINAL_WINDOW, tr!("Nouveau terminal local")), |ui| shells_menu(ui, p, actions));
}

/// "Cet ordinateur", at the top of the sidebar: its files on a click, its
/// shells from the button that shows under the pointer.
pub fn sidebar_entry(ui: &mut Ui, p: &Palette, active: Option<&str>, open: &[&str], actions: &mut Vec<Action>) {
    let (_, rect) = ui.allocate_space(vec2(ui.available_width(), ROW_HEIGHT));
    let id = Id::new("local-entry");
    let response = ui.interact(rect, id, Sense::click());
    let ctx = ui.ctx().clone();
    let shells_id = id.with("shells");
    let menu_open = Popup::is_id_open(&ctx, shells_id.with("popup"));
    let pointed = ui.rect_contains_pointer(rect);
    let is_active = active.is_some_and(local::is_local);
    let buttons = (pointed && !DragAndDrop::has_any_payload(&ctx)) || menu_open;

    if ui.is_rect_visible(rect) {
        if is_active {
            ui.painter().rect_filled(rect, RADIUS, p.surface);
        } else if pointed || menu_open {
            ui.painter().rect_filled(rect, RADIUS, p.surface.gamma_multiply(0.6));
        }
        let glyph_at = pos2(rect.left() + ICON_X, rect.center().y);
        ui.painter().text(
            glyph_at,
            Align2::CENTER_CENTER,
            icon::DESKTOP,
            FontId::proportional(16.0),
            if is_active { p.accent } else { p.muted },
        );
        if open.iter().any(|id| local::is_local(id)) {
            let dot = glyph_at + vec2(9.0, -7.0);
            ui.painter().circle_filled(dot, 3.5, if is_active { p.surface } else { p.sidebar });
            ui.painter().circle_filled(dot, 2.5, p.success);
        }
        let text = Rect::from_min_max(
            pos2(rect.left() + LABEL_X, rect.top()),
            pos2(rect.right() - if buttons { 34.0 } else { 8.0 }, rect.bottom()),
        );
        let mut label = ui.new_child(egui::UiBuilder::new().max_rect(text).layout(Layout::left_to_right(Align::Center)));
        label.add(Label::new(RichText::new(crate::i18n::lookup(TITLE)).color(p.text)).truncate().selectable(false));
    }

    if buttons {
        let button = Rect::from_center_size(pos2(rect.right() - 15.0, rect.center().y), Vec2::splat(22.0));
        let shells = ui.interact(button, shells_id, Sense::click());
        if shells.hovered() || menu_open {
            ui.painter().rect_filled(button, 6, p.surface_hover);
        }
        ui.painter().text(
            button.center(),
            Align2::CENTER_CENTER,
            icon::TERMINAL_WINDOW,
            FontId::proportional(15.0),
            if shells.hovered() { p.text } else { p.muted },
        );
        let shells = shells.on_hover_cursor(CursorIcon::PointingHand).on_hover_text(tr!("Nouveau terminal local"));
        Popup::menu(&shells).show(|ui| shells_menu(ui, p, actions));
    }

    if response.clicked() {
        actions.push(Action::Connect(ID.to_owned()));
    }
    if response.middle_clicked() {
        actions.push(Action::OpenTab(ID.to_owned(), None));
    }
    response.context_menu(|ui| computer_menu(ui, p, actions));
    if !menu_open {
        response.on_hover_text(tr!("Les disques et dossiers de ce PC, et ses terminaux"));
    }
    ui.add_space(6.0);
}

/// The same, in the folded sidebar.
pub fn rail_button(ui: &mut Ui, p: &Palette, actions: &mut Vec<Action>) {
    let button = icon_button(ui, p, icon::DESKTOP, crate::i18n::lookup(TITLE), true);
    if button.clicked() {
        actions.push(Action::Connect(ID.to_owned()));
    }
    button.context_menu(|ui| computer_menu(ui, p, actions));
}

/// The start page's way to a local shell, next to its other buttons.
pub fn welcome_buttons(ui: &mut Ui, p: &Palette, actions: &mut Vec<Action>) {
    let button = ui.button(format!("{}  {}  {}", icon::TERMINAL_WINDOW, tr!("Terminal local"), icon::CARET_DOWN));
    Popup::menu(&button).show(|ui| shells_menu(ui, p, actions));
}

/// The "+" at the end of a row of tabs: this computer, its shells, or a
/// connection.
pub fn new_tab_button(ui: &mut Ui, p: &Palette, actions: &mut Vec<Action>) {
    let button = icon_button(ui, p, icon::PLUS, tr!("Nouvel onglet"), true);
    Popup::menu(&button).show(|ui| {
        ui.set_min_width(240.0);
        if item(ui, format!("{}  {}", icon::DESKTOP, crate::i18n::lookup(TITLE))) {
            actions.push(Action::OpenTab(ID.to_owned(), None));
        }
        ui.menu_button(format!("{}  {}", icon::TERMINAL_WINDOW, tr!("Nouveau terminal local")), |ui| shells_menu(ui, p, actions));
        ui.separator();
        let palette = Button::new(format!("{}  {}", icon::MAGNIFYING_GLASS, tr!("Ouvrir une connexion…")));
        let palette = palette.shortcut_text(tr!("Ctrl+K"));
        if ui.add(palette).clicked() {
            actions.push(Action::QuickOpen);
            ui.close();
        }
        if item(ui, format!("{}  {}", icon::PLUGS_CONNECTED, tr!("Nouvelle connexion…"))) {
            actions.push(Action::NewProfile);
        }
    });
}

/// "Ouvrir Cet ordinateur à côté", for the menu of a tab of a server.
pub fn tab_menu_item(ui: &mut Ui, tab: &Tab, actions: &mut Vec<Action>) {
    if tab.protocol != Protocol::Local && item(ui, format!("{}  {}", icon::COLUMNS, tr!("Ouvrir Cet ordinateur à côté"))) {
        actions.push(Action::Local(LocalAction::OpenBeside(Some(tab.id))));
    }
}

/// The same in one click, from the files of a server, unless this computer
/// is there already.
pub fn beside_button(ui: &mut Ui, p: &Palette, session: &Session, actions: &mut Vec<Action>) {
    let beside = session.other_view.as_ref().is_some_and(|other| other.local);
    if session.storage.local().is_some() || beside {
        return;
    }
    let tooltip = tr!("Ouvrir Cet ordinateur à côté, pour y copier des fichiers en les faisant glisser");
    if icon_button(ui, p, icon::COLUMNS, tooltip, true).clicked() {
        actions.push(Action::Local(LocalAction::OpenBeside(None)));
    }
}

/// The icons of the usual folders and drives, at the root of this computer.
pub fn root_icon(session: &Session, entry: &Entry, p: &Palette) -> Option<(&'static str, Color32)> {
    if !session.path.is_root() {
        return None;
    }
    let spot = session.storage.local()?.spot(&entry.name)?;
    Some(match spot {
        Spot::Desktop => (icon::DESKTOP, p.accent),
        Spot::Documents => (icon::FILE_TEXT, p.folder),
        Spot::Downloads => (icon::DOWNLOAD_SIMPLE, p.success),
        Spot::Home => (icon::HOUSE, p.folder),
        Spot::Drive(DriveKind::Removable) => (icon::USB, p.accent),
        Spot::Drive(DriveKind::Network) => (icon::NETWORK, p.accent),
        Spot::Drive(DriveKind::Optical) => (icon::DISC, p.accent),
        Spot::Drive(_) => (icon::HARD_DRIVE, p.accent),
    })
}

/// Where the listing of a session puts what is dropped on it.
fn destination(session: &Session, folder: RemotePath) -> Destination {
    Destination {
        storage: session.storage.clone(),
        folder,
        title: session.title.clone(),
    }
}

/// Rows of a listing can be dragged to the other view, and its folders take
/// what is dropped on them.
pub fn drag_and_drop(response: &Response, session: &Session, entry: &Entry, p: &Palette, actions: &mut Vec<Action>) {
    if !session.writable() {
        return;
    }
    let ctx = &response.ctx;
    if !entry.is_dir() {
        return;
    }
    let Some(files) = response.dnd_hover_payload::<crate::file_ops::Dragged>() else {
        return;
    };
    if files.listing == listing_key(session) {
        return;
    }
    // Only the part of the row that shows, not what the listing clips.
    let row = response.interact_rect;
    let painter = ctx.layer_painter(response.layer_id).with_clip_rect(row);
    painter.rect_filled(row, 0, p.accent.gamma_multiply(0.16));
    painter.rect_stroke(row, 0, Stroke::new(1.5, p.accent), StrokeKind::Inside);
    // The listing says where it goes (see `drop_on_listing`).
    ctx.data_mut(|data| data.insert_temp(Id::new(("local-drop-row", listing_key(session))), entry.name.clone()));
    if let Some(dragged) = dropped::<crate::file_ops::Dragged>(response) {
        let files = DraggedFiles::of(&dragged);
        let into = destination(session, session.path.child(&entry.name));
        actions.push(Action::Local(LocalAction::Drop { files, into }));
    }
}

/// What is dragged from the other view over this listing lands in the
/// folder it shows, or in the folder row it is over.
pub fn drop_on_listing(ui: &mut Ui, p: &Palette, area: Rect, session: &Session, actions: &mut Vec<Action>) {
    let key = listing_key(session);
    let row = ui.ctx().data_mut(|data| data.remove_temp::<String>(Id::new(("local-drop-row", key))));
    let Some(files) = DragAndDrop::payload::<crate::file_ops::Dragged>(ui.ctx()) else {
        return;
    };
    if files.listing == key {
        return;
    }
    let response = ui.interact(area, Id::new(("local-drop", key)), Sense::hover());
    if !response.contains_pointer() {
        return;
    }
    let text = match (&row, session.writable()) {
        (Some(name), _) => format!("{}  {}", icon::COPY, tr!("Copier dans « {name} »", name)),
        (None, true) => format!("{}  {}", icon::COPY, tr!("Copier ici")),
        (None, false) => format!("{}  {}", icon::PROHIBIT, tr!("Ouvrez d'abord un dossier pour y copier")),
    };
    let layer = egui::LayerId::new(Order::Foreground, Id::new(("local-drop-zone", key)));
    let painter = ui.ctx().layer_painter(layer);
    let tint = if row.is_some() || session.writable() { p.accent } else { p.faint };
    painter.rect_stroke(area, 10, Stroke::new(1.5, tint), StrokeKind::Inside);
    let galley = painter.layout_no_wrap(text, semibold(13.5), p.on_accent);
    let pill = Rect::from_center_size(
        pos2(area.center().x, area.bottom() - 28.0),
        galley.size() + vec2(28.0, 14.0),
    );
    painter.rect_filled(pill, 16, tint);
    painter.galley(pill.center() - galley.size() / 2.0, galley, p.on_accent);

    if row.is_none()
        && session.writable()
        && let Some(dragged) = dropped::<crate::file_ops::Dragged>(&response)
    {
        let files = DraggedFiles::of(&dragged);
        let into = destination(session, session.path.clone());
        actions.push(Action::Local(LocalAction::Drop { files, into }));
    }
}

/// "Copier vers l'autre vue", in the menu of the files, while another view
/// shows a folder.
pub fn copy_menu_item(ui: &mut Ui, session: &Session, suffix: &str, actions: &mut Vec<Action>) {
    let Some(other) = session.other_view.as_ref().filter(|other| other.writable) else {
        return;
    };
    let button = Button::new(format!("{}  {}{suffix}", icon::COPY, tr!("Copier vers l'autre vue"))).shortcut_text("F5");
    if ui.add(button).on_hover_text(tr!("Dans {place}", place = other.label)).clicked() {
        actions.push(Action::Local(LocalAction::CopyToOther { ask: false }));
        ui.close();
    }
}

/// F5 copies the selection to the other view when there is one to copy to,
/// as in the file managers with two panes; it refreshes otherwise.
pub fn f5(session: &Session) -> Action {
    let other = session.other_view.as_ref().is_some_and(|other| other.writable);
    if other && !session.selection.is_empty() && session.writable() {
        Action::Local(LocalAction::CopyToOther { ask: true })
    } else {
        Action::Refresh
    }
}

/// Draws a dialog of this module.
pub fn dialog(ui: &mut Ui, p: &Palette, dialog: &mut LocalDialog, outcome: &mut Outcome) {
    match dialog {
        LocalDialog::Trash { summary, recycle: true, .. } => ask(
            ui,
            p,
            tr!("Placer dans la corbeille ?"),
            &format!("{summary}\n{}", tr!("Vous pourrez le récupérer depuis la Corbeille de Windows.")),
            &format!("{}  {}", icon::TRASH, tr!("Mettre à la corbeille")),
            false,
            outcome,
        ),
        LocalDialog::Trash { summary, .. } => ask(
            ui,
            p,
            tr!("Supprimer définitivement ?"),
            &format!("{summary}\n{}", tr!("Ce lecteur n'a pas de corbeille : cette action est irréversible.")),
            tr!("Supprimer"),
            true,
            outcome,
        ),
        LocalDialog::Copy { summary, .. } => ask(
            ui,
            p,
            tr!("Copier vers l'autre vue ?"),
            summary,
            &format!("{}  {}", icon::COPY, tr!("Copier")),
            false,
            outcome,
        ),
    }
}

/// A question with one way forward, like the other dialogs; Enter answers
/// yes unless that destroys something.
fn ask(ui: &mut Ui, p: &Palette, heading: &str, body: &str, action: &str, destructive: bool, outcome: &mut Outcome) {
    ui.set_width(400.0);
    ui.label(RichText::new(heading).font(semibold(17.0)).color(p.text));
    ui.add_space(8.0);
    ui.add(Label::new(muted(p, body)).wrap());
    ui.add_space(14.0);
    if !destructive && ui.input(|input| input.key_pressed(Key::Enter)) {
        *outcome = Outcome::Confirmed;
    }
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        let confirmed = if destructive {
            danger_button(ui, p, action).clicked()
        } else {
            primary_button(ui, p, action, true).clicked()
        };
        if confirmed {
            *outcome = Outcome::Confirmed;
        }
        if ui.button(tr!("Annuler")).clicked() {
            *outcome = Outcome::Cancelled;
        }
    });
}

/// What deleting the selection of this computer does, in a sentence.
pub fn trash_summary(items: &[Item], recycle: bool) -> String {
    let folders = items.iter().filter(|item| item.is_dir).count();
    let count = items.len();
    match (items, folders, recycle) {
        ([only], 0, true) => tr!("« {name} » sera placé dans la corbeille.", name = only.path.name()),
        ([only], _, true) => {
            tr!("Le dossier « {name} » et son contenu seront placés dans la corbeille.", name = only.path.name())
        }
        (_, 0, true) => tr!("{count} éléments seront placés dans la corbeille.", count),
        (_, _, true) => trn!(
            folders,
            "{count} éléments, dont {n} dossier, seront placés dans la corbeille.",
            "{count} éléments, dont {n} dossiers, seront placés dans la corbeille.",
            count
        ),
        ([only], 0, false) => tr!("« {name} » sera supprimé.", name = only.path.name()),
        ([only], _, false) => tr!("Le dossier « {name} » et son contenu seront supprimés.", name = only.path.name()),
        (_, 0, false) => tr!("{count} éléments seront supprimés.", count),
        (_, _, false) => trn!(
            folders,
            "{count} éléments, dont {n} dossier, seront supprimés.",
            "{count} éléments, dont {n} dossiers, seront supprimés.",
            count
        ),
    }
}

/// Two listings side by side, driven with synthetic pointer events the way
/// a mouse would: scripts cannot drag in the off-screen window.
#[cfg(test)]
mod tests {
    use egui::{Context, Event, Modifiers, PointerButton, Pos2, RawInput};

    use super::*;
    use crate::storage::local::tests::{Scratch, storage_in};
    use crate::storage::{EntryKind, Storage};

    struct Harness {
        ctx: Context,
        left: Session,
        right: Session,
        time: f64,
        _scratch: Scratch,
    }

    fn entry(name: &str, kind: EntryKind) -> Entry {
        Entry {
            permissions: None,
            owner: None,
            group: None,
            link: None,
            name: name.to_owned(),
            kind,
            size: (kind == EntryKind::File).then_some(12),
            modified: None,
        }
    }

    fn session(storage: Arc<dyn Storage>, title: &str, entries: Vec<Entry>) -> Session {
        let mut session = Session::new(title.to_owned(), storage, Some(RemotePath::parse("Essai")));
        session.begin_listing();
        session.listed(entries, true);
        session
    }

    impl Harness {
        fn new() -> Self {
            let scratch = Scratch::new("ui-drag");
            let storage: Arc<dyn Storage> = Arc::new(storage_in(&scratch.0));
            let ctx = Context::default();
            crate::theme::install_fonts(&ctx);
            crate::theme::apply(&ctx, &Palette::dark());
            let left = session(
                storage.clone(),
                "Gauche",
                vec![entry("a.txt", EntryKind::File), entry("b.txt", EntryKind::File)],
            );
            let right = session(storage, "Droite", vec![entry("archives", EntryKind::Folder), entry("c.txt", EntryKind::File)]);
            let mut harness = Self {
                ctx,
                left,
                right,
                time: 0.0,
                _scratch: scratch,
            };
            harness.frame(Vec::new());
            harness.frame(Vec::new());
            harness
        }

        /// The two browsers, each on half of a window.
        fn frame(&mut self, events: Vec<Event>) -> Vec<Action> {
            let input = RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1200.0, 600.0))),
                time: Some(self.time),
                events,
                ..RawInput::default()
            };
            self.time += 1.0 / 60.0;
            let mut actions = Vec::new();
            let palette = Palette::dark();
            let output = self.ctx.run_ui(input, |ui| {
                actions.clear();
                let halves = [
                    Rect::from_min_size(Pos2::ZERO, vec2(600.0, 600.0)),
                    Rect::from_min_size(pos2(600.0, 0.0), vec2(600.0, 600.0)),
                ];
                for (half, session) in halves.into_iter().zip([&mut self.left, &mut self.right]) {
                    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(half).id_salt(half.min.x as i32));
                    super::super::browser::show(&mut child, &palette, session, false, "", None, &mut actions);
                }
            });
            output.drop_without_applying_deltas();
            actions
        }

        /// The middle of the row of this name, on one side: the rows are the
        /// draggable widgets of the last frame, from the top down.
        fn row(&self, name: &str, right: bool) -> Pos2 {
            let session = if right { &self.right } else { &self.left };
            let index = session.row_of(name).expect("row listed");
            let left = if right { 600.0 } else { 0.0 };
            let mut centers: Vec<f32> = self.ctx.viewport(|viewport| {
                let widgets = &viewport.prev_pass.widgets;
                widgets
                    .layer_ids()
                    .flat_map(|layer| widgets.get_layer(layer))
                    // Rows are as tall as the browser makes them; the header
                    // and the filter field, which drag too, are not.
                    .filter(|widget| widget.sense == Sense::click_and_drag() && (widget.rect.height() - 32.0).abs() < 0.25)
                    .filter(|widget| widget.rect.left() >= left && widget.rect.right() <= left + 601.0)
                    .map(|widget| widget.rect.center().y)
                    .collect()
            });
            centers.sort_by(f32::total_cmp);
            centers.dedup_by(|a, b| (*a - *b).abs() < 1.0);
            pos2(left + 100.0, centers[index])
        }

        fn drag(&mut self, from: Pos2, to: Pos2) -> Vec<Action> {
            let button = |at, pressed| Event::PointerButton {
                pos: at,
                button: PointerButton::Primary,
                pressed,
                modifiers: Modifiers::NONE,
            };
            let mut actions = self.frame(vec![Event::PointerMoved(from)]);
            actions.extend(self.frame(Vec::new()));
            actions.extend(self.frame(vec![button(from, true)]));
            for step in 1..=10 {
                let at = from + (to - from) * (step as f32 / 10.0);
                actions.extend(self.frame(vec![Event::PointerMoved(at)]));
            }
            actions.extend(self.frame(Vec::new()));
            actions.extend(self.frame(vec![button(to, false)]));
            actions.extend(self.frame(Vec::new()));
            actions
        }
    }

    fn drops(actions: &[Action]) -> Vec<(Vec<String>, String)> {
        actions
            .iter()
            .filter_map(|action| match action {
                Action::Local(LocalAction::Drop { files, into }) => Some((
                    files.source.items.iter().map(|item| item.name.clone()).collect(),
                    into.folder.to_string(),
                )),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn dragging_rows_to_the_other_view_copies_them() {
        let mut ui = Harness::new();
        // A row alone, dropped on a file of the other side: the folder it shows.
        let (a, c) = (ui.row("a.txt", false), ui.row("c.txt", true));
        assert_eq!(drops(&ui.drag(a, c)), [(vec!["a.txt".to_owned()], "/Essai".to_owned())]);

        // The selection goes along when the row is part of it, and a folder
        // row takes it in.
        ui.left.select_only("a.txt");
        ui.left.toggle("b.txt");
        let archives = ui.row("archives", true);
        let mut dropped = drops(&ui.drag(ui.row("b.txt", false), archives));
        dropped[0].0.sort();
        assert_eq!(dropped, [(vec!["a.txt".to_owned(), "b.txt".to_owned()], "/Essai/archives".to_owned())]);

        // Below the last row too.
        let empty = pos2(700.0, 500.0);
        assert_eq!(drops(&ui.drag(a, empty)).len(), 1);
    }

    #[test]
    fn dropping_back_on_the_same_listing_does_nothing() {
        let mut ui = Harness::new();
        let (a, b) = (ui.row("a.txt", false), ui.row("b.txt", false));
        let actions = ui.drag(a, b);
        assert!(drops(&actions).is_empty());
        assert!(!actions.iter().any(|action| matches!(action, Action::Open(_))), "nor opens anything");
    }

    #[test]
    fn f5_copies_only_where_there_is_somewhere_to_copy() {
        let scratch = Scratch::new("ui-f5");
        let storage: Arc<dyn Storage> = Arc::new(storage_in(&scratch.0));
        let mut session = session(storage, "Gauche", vec![entry("a.txt", EntryKind::File)]);
        assert!(matches!(f5(&session), Action::Refresh));
        session.other_view = Some(crate::copy::OtherView {
            label: "Serveur · /srv".to_owned(),
            local: false,
            writable: true,
        });
        assert!(matches!(f5(&session), Action::Refresh), "nothing selected");
        session.select_only("a.txt");
        assert!(matches!(f5(&session), Action::Local(LocalAction::CopyToOther { ask: true })));
    }

    #[test]
    fn deleting_on_this_computer_says_where_things_go() {
        let item = |name: &str, is_dir| Item {
            path: RemotePath::parse("Essai").child(name),
            is_dir,
            size: 0,
        };
        assert_eq!(trash_summary(&[item("a.txt", false)], true), "« a.txt » sera placé dans la corbeille.");
        assert_eq!(
            trash_summary(&[item("photos", true)], true),
            "Le dossier « photos » et son contenu seront placés dans la corbeille."
        );
        assert_eq!(
            trash_summary(&[item("a", false), item("b", true)], false),
            "2 éléments, dont 1 dossier, seront supprimés."
        );
    }
}
