//! The built-in editor on screen: the tab of a file (its header, search
//! bar and text), what the editor adds to the tabs, to the file menus and
//! to the status bar, and its dialogs.

mod find;
pub(crate) mod view;

use egui::{Align, Button, Color32, FontId, Label, Layout, Popup, PopupCloseBehavior, RichText, ScrollArea, Ui, vec2};

use super::Action;
use super::dialogs::Outcome;
use super::widgets::{checkbox, entry_icon, icon, icon_button, muted, placeholder, primary_button, small};
use crate::editor::encoding::{ENCODINGS, Indent, LineEnding};
use crate::editor::prefs::{self, EditorPrefs, MAX_FONT_SIZE, MIN_FONT_SIZE};
use crate::editor::syntax::{self, THEMES};
use crate::editor::{Editor, Saving, State};
use crate::format;
use crate::launch::{self, Launch};
use crate::storage::{Entry, EntryKind};
use crate::tabs::Tab;
use crate::theme::{Palette, semibold};
use crate::viewers::decode::Encoding;

/// How long "Enregistré" stays in the header after a save.
const SAVED_SHOWN: f64 = 2.5;

pub enum EditorAction {
    /// The named file of the folder shown, in the built-in editor.
    OpenBuiltin(String),
    /// The same, in the other view, next to the files.
    OpenBeside(String),
    /// The named file, in the program chosen as text editor.
    OpenExternal(String),
    /// What the editor of the tab of this id does.
    Save(u64),
    Reload(u64),
    /// Edit the file in the program chosen as text editor instead.
    OpenElsewhere(u64),
    /// With the program the system associates with the file.
    OpenWithSystem(u64),
    /// A file that does not look like text, shown as text all the same.
    ForceText(u64),
    SetLanguage(u64, String),
    /// Written in this encoding from now on.
    SetEncoding(u64, Encoding),
    /// Read again from the server in this encoding.
    ReopenAs(u64, Encoding),
    SetEnding(u64, LineEnding),
    SetIndent(u64, Indent),
    Find(u64, bool),
    GoToLine(u64),
    /// Text size, by steps of a point; 0 for the default.
    Zoom(i32),
    /// New preferences: word wrap, spaces shown, theme…
    Prefs(EditorPrefs),
    /// The dialog choosing which editor opens text files.
    ChooseEditor,
    /// Something to say in a toast.
    Notice(String),
}

/// What closing a file with changes does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseChoice {
    Save,
    Discard,
}

/// What to do about a file changed on the server since it was opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangedChoice {
    Overwrite,
    Reload,
}

pub enum EditorDialog {
    /// Closing tabs, or leaving, with changes not saved.
    Unsaved {
        /// Every tab being closed; `names` are those of the files with changes.
        ids: Vec<u64>,
        names: Vec<String>,
        quitting: bool,
        /// The tabs can be reopened once closed.
        remember: bool,
        /// Files opened in another program and not sent, when leaving.
        unsent: usize,
        choice: Option<CloseChoice>,
    },
    /// Saving over a file that changed on the server since.
    Changed {
        id: u64,
        name: String,
        /// What the save was for: closing the tab, leaving the app.
        then: crate::editor::Then,
        choice: Option<ChangedChoice>,
    },
    /// Reading the file again loses the changes; read in this encoding.
    Reload { id: u64, name: String, encoding: Option<Encoding> },
    /// The encoding cannot hold a character: save in UTF-8 instead?
    Unencodable { id: u64, encoding: Encoding, c: char, line: usize },
    /// The file cannot open in the editor, and why.
    Refused { name: String, why: String, external: String, choice: Option<RefusedChoice> },
    /// Which editor opens text files, and how the built-in one looks.
    Choice(Box<ChoiceForm>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefusedChoice {
    Preview,
    External,
}

pub struct ChoiceForm {
    pub builtin: bool,
    /// The other program; empty for the system's own.
    pub program: String,
    pub prefs: EditorPrefs,
    /// "Parcourir…" was clicked: the app shows the picker.
    pub browse: bool,
}

fn push(actions: &mut Vec<Action>, action: EditorAction) {
    actions.push(Action::Editor(action));
}

/// The label of the program chosen as text editor, for menus.
pub fn external_label(program: &str) -> String {
    launch::editor_label(program)
}

/// "Ouvrir dans le Bloc-notes", or in the program chosen as text editor.
fn open_in(program: &str) -> String {
    tr!("Ouvrir dans {program}", program = external_label(program))
}

/// The icon of a file, from its name.
fn file_icon(name: &str, p: &Palette) -> (&'static str, Color32) {
    let entry = Entry { name: name.to_owned(), kind: EntryKind::File, ..Entry::default() };
    match entry_icon(&entry, p) {
        // What the listing does not know, the grammars may.
        (icon::FILE, _) if syntax::detect(name, "", "").name != syntax::plain_text().name => (icon::FILE_CODE, p.success),
        (icon::FILE, _) if syntax::text_by_name(name) => (icon::FILE_TEXT, p.muted),
        found => found,
    }
}

fn frame_time_id() -> egui::Id {
    egui::Id::new("editor-frame-time")
}

/// Milliseconds the files shown took to draw in this frame, for the
/// measures scripts take.
pub fn take_frame_time(ctx: &egui::Context) -> f64 {
    ctx.data_mut(|data| data.remove_temp::<f64>(frame_time_id())).unwrap_or(0.0)
}

/// The tab of a file: header, search bar and text.
pub fn show(ui: &mut Ui, p: &Palette, tab: &mut Tab, interactive: bool, actions: &mut Vec<Action>) {
    let started = std::time::Instant::now();
    show_file(ui, p, tab, interactive, actions);
    let spent = started.elapsed().as_secs_f64() * 1000.0;
    ui.ctx().data_mut(|data| *data.get_temp_mut_or_default::<f64>(frame_time_id()) += spent);
}

fn show_file(ui: &mut Ui, p: &Palette, tab: &mut Tab, interactive: bool, actions: &mut Vec<Action>) {
    let (prefs, external) = prefs::current(ui.ctx());
    let id = tab.id;
    let name = tab.session.path.name().to_owned();
    let place = place_of(tab);
    let Some(editor) = tab.editor.as_deref_mut() else {
        return;
    };
    let theme = syntax::theme(&prefs.theme, p);
    let now = ui.input(|input| input.time);
    header(ui, p, editor, id, &name, &place, &prefs, &external, now, actions);
    ui.add_space(4.0);

    match editor.state.clone() {
        State::Loading => {
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() * 0.35).max(24.0));
                ui.add(egui::Spinner::new().size(26.0).color(p.accent));
                ui.add_space(6.0);
                ui.label(muted(p, tr!("Ouverture de « {name} »…", name)));
            });
        }
        State::NotText => {
            let hint = tr!("{size} qui ne se lisent pas comme des lettres.", size = format::size(editor.size));
            placeholder(ui, p, icon::FILE_DASHED, p.faint, tr!("Ce fichier n'est pas du texte"), &hint);
            ui.vertical_centered(|ui| {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    center_row(ui, 520.0);
                    if ui.button(format!("{}  {}", icon::ARROW_SQUARE_OUT, tr!("Ouvrir avec le programme associé"))).clicked() {
                        push(actions, EditorAction::OpenWithSystem(id));
                    }
                    if ui.button(format!("{}  {}", icon::PENCIL_SIMPLE, open_in(&external))).clicked() {
                        push(actions, EditorAction::OpenElsewhere(id));
                    }
                    let kept = tr!("Chaque octet est gardé tel quel à l'enregistrement");
                    if ui.button(tr!("Afficher comme du texte")).on_hover_text(kept).clicked() {
                        push(actions, EditorAction::ForceText(id));
                    }
                });
            });
        }
        State::Failed(error) => {
            placeholder(ui, p, icon::WARNING_CIRCLE, p.danger, &tr!("Ouverture de « {name} » impossible", name), "");
            ui.vertical_centered(|ui| {
                ui.add(Label::new(muted(p, error)).wrap());
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    center_row(ui, 300.0);
                    if ui.button(format!("{}  {}", icon::ARROWS_CLOCKWISE, tr!("Réessayer"))).clicked() {
                        push(actions, EditorAction::Reload(id));
                    }
                    if ui.button(format!("{}  {}", icon::PENCIL_SIMPLE, open_in(&external))).clicked() {
                        push(actions, EditorAction::OpenElsewhere(id));
                    }
                });
            });
        }
        State::Ready => {
            if let Some(notice) = find::bar(ui, p, editor) {
                push(actions, EditorAction::Notice(notice));
            }
            if editor.find.open {
                ui.add_space(4.0);
            }
            if editor.mixed {
                banner(ui, p, editor, id, actions);
            }
            let area = ui.available_rect_before_wrap();
            let frame = egui::Frame::new()
                .corner_radius(8)
                .fill(theme.background)
                .stroke(egui::Stroke::new(1.0, p.border))
                .inner_margin(1);
            frame.show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                let rounded = ui.available_rect_before_wrap();
                ui.scope_builder(egui::UiBuilder::new().max_rect(rounded), |ui| {
                    ui.set_clip_rect(rounded.intersect(ui.clip_rect()));
                    view::show(ui, theme, editor, &prefs, interactive, actions);
                });
            });
            find::goto(ui, p, editor, area);
        }
    }
}

/// Gives a row of buttons of about `width` a centred start.
fn center_row(ui: &mut Ui, width: f32) {
    ui.add_space(((ui.available_width() - width) / 2.0).max(0.0));
}

/// Where the file lives, for the header and the tooltip of its tab.
fn place_of(tab: &Tab) -> String {
    let session = &tab.session;
    if let Some(local) = session.storage.local()
        && let Ok(real) = local.resolve(&session.path)
    {
        return real.parent().map(|folder| folder.display().to_string()).unwrap_or_default();
    }
    let folder = session.path.parent().unwrap_or_default();
    format!("{} · {folder}", session.title)
}

/// The full address of the file, shown over its tab.
pub fn tab_tooltip(tab: &Tab) -> Option<String> {
    tab.editor.as_ref()?;
    let session = &tab.session;
    Some(session.storage.plain_url(&session.path).unwrap_or_else(|| format!("{} · {}", session.title, session.path)))
}

/// The icon of the tab of a file, and its tint.
pub fn tab_glyph(p: &Palette, tab: &Tab) -> Option<(&'static str, Color32)> {
    let editor = tab.editor.as_ref()?;
    Some(editor_icon(editor, tab.session.path.name(), p))
}

/// The icon of a file being edited: its language, found from its first
/// line too, says it is code.
fn editor_icon(editor: &Editor, name: &str, p: &Palette) -> (&'static str, Color32) {
    match file_icon(name, p) {
        (icon::FILE | icon::FILE_TEXT, _) if editor.is_ready() && editor.language().name != syntax::plain_text().name => {
            (icon::FILE_CODE, p.success)
        }
        found => found,
    }
}

/// The file of this tab has changes that are not saved.
pub fn is_dirty(tab: &Tab) -> bool {
    tab.editor.as_ref().is_some_and(|editor| editor.is_dirty())
}

/// The dot that says a tab's file has changes, where its close button goes.
pub fn dirty_dot(ui: &Ui, p: &Palette, tab: &Tab, center: egui::Pos2) {
    let saving = tab.editor.as_ref().is_some_and(|editor| editor.saving == Saving::Running);
    let color = if saving { p.accent } else { p.text.gamma_multiply(0.8) };
    ui.painter().circle_filled(center, 4.0, color);
}

#[allow(clippy::too_many_arguments)]
fn header(
    ui: &mut Ui,
    p: &Palette,
    editor: &mut Editor,
    id: u64,
    name: &str,
    place: &str,
    prefs: &EditorPrefs,
    external: &str,
    now: f64,
    actions: &mut Vec<Action>,
) {
    ui.horizontal(|ui| {
        ui.set_height(32.0);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let menu = icon_button(ui, p, icon::DOTS_THREE, tr!("Plus d'actions"), true);
            Popup::menu(&menu).show(|ui| more_menu(ui, p, editor, id, prefs, external, actions));
            if editor.is_ready() {
                let wrap = if prefs.wrap { tr!("Ne plus revenir à la ligne") } else { tr!("Retour automatique à la ligne") };
                if toggle_icon(ui, p, icon::ARROW_U_DOWN_LEFT, wrap, prefs.wrap).clicked() {
                    push(actions, EditorAction::Prefs(EditorPrefs { wrap: !prefs.wrap, ..prefs.clone() }));
                }
                let find = tr!("Rechercher (Ctrl+F), remplacer (Ctrl+H)");
                if toggle_icon(ui, p, icon::MAGNIFYING_GLASS, find, editor.find.open).clicked() {
                    if editor.find.open {
                        editor.find.open = false;
                    } else {
                        push(actions, EditorAction::Find(id, false));
                    }
                }
                ui.add_space(6.0);
                let dirty = editor.is_dirty();
                let label = format!("{}  {}", icon::FLOPPY_DISK, tr!("Enregistrer"));
                let save = if dirty {
                    primary_button(ui, p, &label, editor.saving != Saving::Running)
                } else {
                    ui.add_enabled(false, Button::new(label))
                };
                if save.on_hover_text(tr!("Ctrl+S")).on_disabled_hover_text(tr!("Aucune modification")).clicked() {
                    push(actions, EditorAction::Save(id));
                }
                ui.add_space(6.0);
                save_state(ui, p, editor, now);
            }
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let (glyph, tint) = editor_icon(editor, name, p);
                ui.label(RichText::new(glyph).font(crate::theme::filled_icon(19.0)).color(tint));
                ui.add(Label::new(RichText::new(name).font(semibold(15.0)).color(p.text)).truncate());
                ui.add(Label::new(small(p, place)).truncate());
            });
        });
    });
}

fn toggle_icon(ui: &mut Ui, p: &Palette, glyph: &str, tooltip: &str, on: bool) -> egui::Response {
    let response = icon_button(ui, p, glyph, tooltip, true);
    if on {
        ui.painter().rect_filled(response.rect, crate::theme::RADIUS, p.accent.gamma_multiply(0.18));
        ui.painter().text(response.rect.center(), egui::Align2::CENTER_CENTER, glyph, FontId::proportional(17.0), p.accent);
    }
    response
}

/// "Modifié", "Enregistrement…", "Enregistré" or why it failed.
fn save_state(ui: &mut Ui, p: &Palette, editor: &Editor, now: f64) {
    match &editor.saving {
        Saving::Running => {
            ui.label(small(p, tr!("Enregistrement…")));
            ui.add(egui::Spinner::new().size(13.0).color(p.accent));
        }
        Saving::Failed(error) => {
            let text = RichText::new(format!("{}  {}", icon::WARNING_CIRCLE, tr!("Échec de l'enregistrement")));
            ui.add(Label::new(text.size(12.5).color(p.danger))).on_hover_text(error);
        }
        Saving::Done(at) if now - at < SAVED_SHOWN && !editor.is_dirty() => {
            ui.label(RichText::new(format!("{}  {}", icon::CHECK, tr!("Enregistré"))).size(12.5).color(p.success));
            ui.ctx().request_repaint_after(std::time::Duration::from_secs_f64(SAVED_SHOWN - (now - at) + 0.05));
        }
        _ if editor.is_dirty() => {
            ui.label(RichText::new(format!("● {}", tr!("Modifié"))).size(12.5).color(p.folder));
        }
        _ => {}
    }
}

fn more_menu(ui: &mut Ui, p: &Palette, editor: &mut Editor, id: u64, prefs: &EditorPrefs, external: &str, actions: &mut Vec<Action>) {
    ui.set_min_width(260.0);
    let item = |ui: &mut Ui, text: String, shortcut: &str| {
        let mut button = Button::new(text);
        if !shortcut.is_empty() {
            button = button.shortcut_text(shortcut);
        }
        let clicked = ui.add(button).clicked();
        if clicked {
            ui.close();
        }
        clicked
    };
    if editor.is_ready() {
        if item(ui, format!("{}  {}", icon::MAGNIFYING_GLASS, trc!("éditeur", "Rechercher")), tr!("Ctrl+F")) {
            push(actions, EditorAction::Find(id, false));
        }
        if item(ui, format!("{}  {}", icon::SWAP, tr!("Remplacer")), tr!("Ctrl+H")) {
            push(actions, EditorAction::Find(id, true));
        }
        if item(ui, format!("{}  {}", icon::ARROW_BEND_DOWN_RIGHT, tr!("Aller à la ligne…")), tr!("Ctrl+G")) {
            push(actions, EditorAction::GoToLine(id));
        }
        ui.separator();
    }
    if item(ui, format!("{}  {}", icon::ARROWS_CLOCKWISE, tr!("Recharger depuis le serveur")), "") {
        push(actions, EditorAction::Reload(id));
    }
    if item(ui, format!("{}  {}", icon::ARROW_SQUARE_OUT, open_in(external)), "") {
        push(actions, EditorAction::OpenElsewhere(id));
    }
    ui.separator();
    let mut whitespace = prefs.whitespace;
    if checkbox(ui, p, &mut whitespace, tr!("Afficher les espaces et tabulations")).clicked() {
        push(actions, EditorAction::Prefs(EditorPrefs { whitespace, ..prefs.clone() }));
    }
    let mut wrap = prefs.wrap;
    if checkbox(ui, p, &mut wrap, tr!("Retour automatique à la ligne")).clicked() {
        push(actions, EditorAction::Prefs(EditorPrefs { wrap, ..prefs.clone() }));
    }
    ui.menu_button(format!("{}  {}", icon::PALETTE, tr!("Thème")), |ui| {
        ui.set_min_width(240.0);
        for (theme, label) in THEMES {
            let mark = if prefs.theme == theme { icon::CHECK } else { "    " };
            if ui.button(format!("{mark}  {}", crate::i18n::lookup(label))).clicked() {
                push(actions, EditorAction::Prefs(EditorPrefs { theme: theme.to_owned(), ..prefs.clone() }));
                ui.close();
            }
        }
    });
    ui.horizontal(|ui| {
        ui.label(format!("{}  {}", icon::TEXT_AA, tr!("Taille du texte")));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.small_button("+").on_hover_text(tr!("Ctrl+= ou Ctrl+molette")).clicked() {
                push(actions, EditorAction::Zoom(1));
            }
            ui.label(format!("{:.0}", prefs.font_size));
            if ui.small_button("−").on_hover_text(tr!("Ctrl+-")).clicked() {
                push(actions, EditorAction::Zoom(-1));
            }
        });
    });
    ui.separator();
    if item(ui, format!("{}  {}", icon::GEAR, tr!("Préférences de l'éditeur…")), "") {
        push(actions, EditorAction::ChooseEditor);
    }
}

/// Says that some lines end otherwise, and offers to make them all alike.
fn banner(ui: &mut Ui, p: &Palette, editor: &Editor, id: u64, actions: &mut Vec<Action>) {
    egui::Frame::new()
        .fill(p.folder.gamma_multiply(0.12))
        .corner_radius(8)
        .inner_margin(egui::Margin::symmetric(10, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon::INFO).color(p.folder));
                let ending = editor.ending.label();
                ui.label(small(p, tr!("Fins de ligne mélangées : la plupart en {ending}.", ending)));
                if ui.small_button(tr!("Tout convertir en {ending}", ending)).clicked() {
                    push(actions, EditorAction::SetEnding(id, editor.ending));
                }
            });
        });
    ui.add_space(4.0);
}

/// The menu of the text.
pub(super) fn context_menu(ui: &mut Ui, editor: &mut Editor, actions: &mut Vec<Action>) {
    ui.set_min_width(240.0);
    let selected = !editor.sel.is_empty();
    let item = |ui: &mut Ui, enabled: bool, text: String, shortcut: &str| {
        let clicked = ui.add_enabled(enabled, Button::new(text).shortcut_text(shortcut)).clicked();
        if clicked {
            ui.close();
        }
        clicked
    };
    // "Annuler" alone is the Cancel of the dialogs; here it is Undo.
    let undo = format!("{}  {}", icon::ARROW_COUNTER_CLOCKWISE, trc!("édition", "Annuler"));
    if item(ui, editor.history.can_undo(), undo, tr!("Ctrl+Z")) {
        editor.undo();
    }
    if item(ui, editor.history.can_redo(), format!("{}  {}", icon::ARROW_CLOCKWISE, tr!("Rétablir")), tr!("Ctrl+Y")) {
        editor.redo();
    }
    ui.separator();
    if item(ui, true, format!("{}  {}", icon::SCISSORS, tr!("Couper")), tr!("Ctrl+X")) {
        let text = editor.cut();
        ui.ctx().copy_text(text);
    }
    if item(ui, true, format!("{}  {}", icon::COPY, tr!("Copier")), tr!("Ctrl+C")) {
        ui.ctx().copy_text(editor.copied());
    }
    if item(ui, true, format!("{}  {}", icon::CLIPBOARD_TEXT, tr!("Coller")), tr!("Ctrl+V")) {
        // The backend answers with a paste event, which the text takes in
        // like Ctrl+V.
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::RequestPaste);
        editor.view.focus = true;
    }
    if item(ui, selected, format!("{}  {}", icon::BACKSPACE, tr!("Supprimer")), tr!("Suppr")) {
        editor.delete();
    }
    if item(ui, true, format!("{}  {}", icon::SELECTION_ALL, tr!("Tout sélectionner")), tr!("Ctrl+A")) {
        editor.select_all();
    }
    ui.separator();
    if item(ui, true, format!("{}  {}", icon::CHAT_CENTERED_SLASH, tr!("Commenter / décommenter")), tr!("Ctrl+/")) {
        editor.toggle_comment();
    }
    if item(ui, true, format!("{}  {}", icon::COPY_SIMPLE, tr!("Dupliquer")), tr!("Ctrl+D")) {
        editor.duplicate();
    }
    if item(ui, true, format!("{}  {}", icon::MINUS_CIRCLE, tr!("Supprimer la ligne")), tr!("Ctrl+Maj+K")) {
        editor.delete_lines();
    }
    let find = format!("{}  {}", icon::MAGNIFYING_GLASS, tr!("Rechercher la sélection"));
    if selected && item(ui, true, find, tr!("Ctrl+F")) {
        editor.open_find(false);
    }
    let _ = actions;
}

/// Ouvrir, Modifier and Ouvrir avec, in the menu of a file of a listing.
/// `editor` is the program chosen as text editor, empty for the system's.
pub fn file_menu(ui: &mut Ui, entry: &Entry, editor: &str, actions: &mut Vec<Action>) {
    let (prefs, _) = prefs::current(ui.ctx());
    let name = entry.name.clone();
    let open = |ui: &mut Ui, label: String, how: Launch, actions: &mut Vec<Action>| {
        if ui.button(label).clicked() {
            actions.push(Action::OpenFile(name.clone(), how));
            ui.close();
        }
    };
    let action = |ui: &mut Ui, label: String, action: EditorAction, actions: &mut Vec<Action>| {
        if ui.button(label).clicked() {
            push(actions, action);
            ui.close();
        }
    };
    // What the system would run is only ever opened as text.
    if launch::kind(&entry.name) == launch::Kind::Document {
        open(ui, format!("{}  {}", icon::ARROW_SQUARE_OUT, tr!("Ouvrir")), Launch::Default, actions);
    }
    let edit = if prefs.builtin {
        format!("{}  {}", icon::PENCIL_SIMPLE, tr!("Modifier"))
    } else {
        format!("{}  {}", icon::PENCIL_SIMPLE, tr!("Modifier dans {program}", program = external_label(editor)))
    };
    open(ui, edit, Launch::Editor, actions);
    if prefs.builtin {
        let beside = format!("{}  {}", icon::COLUMNS, tr!("Modifier à côté"));
        action(ui, beside, EditorAction::OpenBeside(entry.name.clone()), actions);
    }
    ui.menu_button(format!("{}  {}", icon::APP_WINDOW, tr!("Ouvrir avec")), |ui| {
        ui.set_min_width(230.0);
        let builtin = if prefs.builtin { tr!("Éditeur intégré (par défaut)") } else { tr!("Éditeur intégré") };
        action(ui, builtin.to_owned(), EditorAction::OpenBuiltin(entry.name.clone()), actions);
        let program = capitalized(&external_label(editor));
        let external = if prefs.builtin { program } else { tr!("{program} (par défaut)", program) };
        action(ui, external, EditorAction::OpenExternal(entry.name.clone()), actions);
        open(ui, tr!("Choisir un programme…").to_owned(), Launch::Choose, actions);
        ui.separator();
        action(ui, tr!("Changer l'éditeur par défaut…").to_owned(), EditorAction::ChooseEditor, actions);
    });
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map(|first| first.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

/// What the menu of a tab of a file offers besides closing and moving it.
pub fn tab_menu(ui: &mut Ui, tab: &Tab, actions: &mut Vec<Action>) {
    let Some(editor) = tab.editor.as_ref() else {
        return;
    };
    let id = tab.id;
    let (_, external) = prefs::current(ui.ctx());
    let item = |ui: &mut Ui, enabled: bool, text: String, shortcut: &str| {
        let clicked = ui.add_enabled(enabled, Button::new(text).shortcut_text(shortcut)).clicked();
        if clicked {
            ui.close();
        }
        clicked
    };
    if item(ui, editor.is_dirty(), format!("{}  {}", icon::FLOPPY_DISK, tr!("Enregistrer")), tr!("Ctrl+S")) {
        push(actions, EditorAction::Save(id));
    }
    if item(ui, true, format!("{}  {}", icon::ARROWS_CLOCKWISE, tr!("Recharger depuis le serveur")), "") {
        push(actions, EditorAction::Reload(id));
    }
    if item(ui, true, format!("{}  {}", icon::ARROW_SQUARE_OUT, open_in(&external)), "") {
        push(actions, EditorAction::OpenElsewhere(id));
    }
}

/// The editor's part of the status bar: where the cursor is, then the
/// language, the indentation, the encoding and the line ends, each a menu.
pub fn status(ui: &mut Ui, p: &Palette, tab: &Tab, actions: &mut Vec<Action>) {
    let Some(editor) = tab.editor.as_ref().filter(|editor| editor.is_ready()) else {
        if let Some(editor) = &tab.editor {
            let text = match &editor.state {
                State::Loading => tr!("Ouverture…").to_owned(),
                State::NotText => tr!("Fichier binaire · {size}", size = format::size(editor.size)),
                State::Failed(_) => tr!("Ouverture impossible").to_owned(),
                State::Ready => String::new(),
            };
            ui.label(small(p, text));
        }
        return;
    };
    let id = tab.id;
    let head = editor.sel.head;
    let column = crate::editor::layout::cells(&editor.doc.line(head.line)[..head.col], editor.tab_width()) + 1;
    let mut text = tr!("Ln {line}, Col {column}", line = crate::editor::thousands(head.line + 1), column);
    if !editor.sel.is_empty() {
        let selected = editor.selected_text();
        let chars = selected.chars().count();
        let lines = selected.matches('\n').count();
        let count = crate::editor::thousands(chars);
        // The French keeps its plural for one, as it always did ("1
        // sélectionnés", "1 lignes" below); the other languages count.
        text.push_str("  ");
        text.push_str(&if lines > 0 {
            let lines = lines + 1;
            trn!(chars, "({count} sélectionnés, {lines} lignes)", "({count} sélectionnés, {lines} lignes)", count, lines)
        } else {
            trn!(chars, "({count} sélectionnés)", "({count} sélectionnés)", count)
        });
    }
    ui.label(small(p, text));
    ui.add_space(10.0);
    let lines = editor.doc.line_count();
    let count = crate::editor::thousands(lines);
    ui.label(small(p, trn!(lines, "{count} lignes", "{count} lignes", count)));
    ui.add_space(10.0);

    // A script opens one of the menus by its name (see `dev_menu_id`).
    let wanted: Option<String> = ui.ctx().data_mut(|data| data.remove_temp(dev_menu_id()));
    let menu_button = |ui: &mut Ui, label: String, tooltip: &str, name: &str| {
        let response = ui.add(Button::new(RichText::new(label).size(12.0).color(p.muted)).frame(false)).on_hover_text(tooltip);
        if wanted.as_deref() == Some(name) {
            Popup::open_id(ui.ctx(), Popup::default_response_id(&response));
        }
        response
    };
    // The language, with a search field: there are two hundred of them.
    let mut label = editor.language().name.clone();
    if editor.highlight.is_off() && label != syntax::plain_text().name {
        label = tr!("{language} (sans couleurs : fichier trop gros)", language = label);
    }
    let language = menu_button(ui, label, tr!("Langage, pour les couleurs"), "language");
    Popup::menu(&language)
        .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| language_menu(ui, p, editor, id, actions));

    let indent = menu_button(ui, editor.indent.label(), tr!("Indentation"), "indent");
    Popup::menu(&indent).show(|ui| {
        ui.set_min_width(220.0);
        ui.label(small(p, tr!("Indenter avec")));
        for (tabs, label) in [(false, tr!("Des espaces")), (true, tr!("Des tabulations"))] {
            let mark = if editor.indent.tabs == tabs { icon::CHECK } else { "    " };
            if ui.button(format!("{mark}  {label}")).clicked() {
                push(actions, EditorAction::SetIndent(id, Indent { tabs, width: editor.indent.width }));
                ui.close();
            }
        }
        ui.separator();
        ui.label(small(p, tr!("Largeur")));
        for width in [2, 3, 4, 8] {
            let mark = if editor.indent.width == width { icon::CHECK } else { "    " };
            if ui.button(format!("{mark}  {width}")).clicked() {
                push(actions, EditorAction::SetIndent(id, Indent { tabs: editor.indent.tabs, width }));
                ui.close();
            }
        }
    });

    let encoding = menu_button(ui, editor.encoding.label().to_owned(), tr!("Encodage"), "encoding");
    Popup::menu(&encoding).show(|ui| {
        ui.set_min_width(230.0);
        ui.label(small(p, tr!("Enregistrer en")));
        for choice in ENCODINGS {
            let mark = if editor.encoding == choice { icon::CHECK } else { "    " };
            if ui.button(format!("{mark}  {}", choice.label())).clicked() {
                push(actions, EditorAction::SetEncoding(id, choice));
                ui.close();
            }
        }
        ui.separator();
        ui.menu_button(tr!("Rouvrir en"), |ui| {
            ui.set_min_width(170.0);
            let again = tr!("Relit le fichier depuis le serveur avec cet encodage");
            for choice in ENCODINGS {
                if ui.button(choice.label()).on_hover_text(again).clicked() {
                    push(actions, EditorAction::ReopenAs(id, choice));
                    ui.close();
                }
            }
        });
    });

    let ending = editor.ending.label();
    let ending = if editor.mixed { tr!("{ending} (mixte)", ending) } else { ending.to_owned() };
    let endings = menu_button(ui, ending, tr!("Fins de ligne"), "ending");
    Popup::menu(&endings).show(|ui| {
        ui.set_min_width(200.0);
        for choice in LineEnding::ALL {
            let mark = if editor.ending == choice && !editor.mixed { icon::CHECK } else { "    " };
            if ui.button(format!("{mark}  {}", choice.description())).clicked() {
                push(actions, EditorAction::SetEnding(id, choice));
                ui.close();
            }
        }
    });
}

/// Where a script names the menu of the status bar to open.
pub fn dev_menu_id() -> egui::Id {
    egui::Id::new("editor-dev-menu")
}

fn language_menu(ui: &mut Ui, p: &Palette, editor: &Editor, id: u64, actions: &mut Vec<Action>) {
    ui.set_width(240.0);
    let filter_id = ui.id().with("language-filter");
    let mut filter: String = ui.data(|data| data.get_temp(filter_id)).unwrap_or_default();
    let field = ui.add(egui::TextEdit::singleline(&mut filter).hint_text(tr!("Rechercher un langage")).desired_width(f32::INFINITY));
    if !field.has_focus() && filter.is_empty() {
        field.request_focus();
    }
    ui.data_mut(|data| data.insert_temp(filter_id, filter.clone()));
    let wanted = crate::profiles::fold(&filter);
    let current = &editor.language().name;
    ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
        for syntax in syntax::languages() {
            if !wanted.is_empty() && !crate::profiles::fold(&syntax.name).contains(&wanted) {
                continue;
            }
            let selected = syntax.name == *current;
            let text = RichText::new(&syntax.name).color(if selected { p.accent } else { p.text });
            if ui.add(Button::selectable(selected, text)).clicked() {
                push(actions, EditorAction::SetLanguage(id, syntax.name.clone()));
                ui.data_mut(|data| data.remove::<String>(filter_id));
                ui.close();
            }
        }
    });
}

/// Draws a dialog of the editor.
pub fn dialog(ui: &mut Ui, p: &Palette, dialog: &mut EditorDialog, first_frame: bool, outcome: &mut Outcome) {
    match dialog {
        EditorDialog::Unsaved { names, quitting, unsent, choice, .. } => unsaved(ui, p, names, *quitting, *unsent, choice, outcome),
        EditorDialog::Changed { name, choice, .. } => {
            ui.set_width(420.0);
            title(ui, p, &tr!("« {name} » a changé sur le serveur", name));
            let text = tr!("Quelqu'un l'a modifié depuis que vous l'avez ouvert (sa taille ou sa date ne sont plus les mêmes).\n\
                        Écraser remplace sa version par la vôtre ; recharger remplace la vôtre par la sienne.");
            ui.add(Label::new(muted(p, text)).wrap());
            ui.add_space(14.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if super::widgets::danger_button(ui, p, tr!("Écraser")).clicked() {
                    *choice = Some(ChangedChoice::Overwrite);
                    *outcome = Outcome::Confirmed;
                }
                if ui.button(tr!("Recharger")).on_hover_text(tr!("Vos modifications seront perdues")).clicked() {
                    *choice = Some(ChangedChoice::Reload);
                    *outcome = Outcome::Confirmed;
                }
                if ui.button(tr!("Annuler")).clicked() {
                    *outcome = Outcome::Cancelled;
                }
            });
        }
        EditorDialog::Reload { name, .. } => ask(
            ui,
            p,
            tr!("Recharger depuis le serveur ?"),
            &tr!("Les modifications de « {name} » qui ne sont pas enregistrées seront perdues.", name),
            tr!("Recharger"),
            true,
            outcome,
        ),
        EditorDialog::Unencodable { encoding, c, line, .. } => ask(
            ui,
            p,
            tr!("Enregistrer en UTF-8 ?"),
            &tr!(
                "Le caractère « {c} » (ligne {line}) n'existe pas en {encoding}. En UTF-8, tous les caractères peuvent être enregistrés.",
                c,
                line = *line + 1,
                encoding = encoding.label()
            ),
            tr!("Enregistrer en UTF-8"),
            false,
            outcome,
        ),
        EditorDialog::Refused { name, why, external, choice } => {
            ui.set_width(400.0);
            title(ui, p, &tr!("« {name} » ne s'ouvre pas dans l'éditeur", name));
            ui.add(Label::new(muted(p, why.as_str())).wrap());
            ui.add_space(14.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if primary_button(ui, p, &open_in(external), true).clicked() {
                    *choice = Some(RefusedChoice::External);
                    *outcome = Outcome::Confirmed;
                }
                if ui.button(format!("{}  {}", icon::EYE, tr!("Aperçu"))).clicked() {
                    *choice = Some(RefusedChoice::Preview);
                    *outcome = Outcome::Confirmed;
                }
                if ui.button(tr!("Annuler")).clicked() {
                    *outcome = Outcome::Cancelled;
                }
            });
        }
        EditorDialog::Choice(form) => choice_form(ui, p, form, first_frame, outcome),
    }
}

fn title(ui: &mut Ui, p: &Palette, text: &str) {
    ui.add(Label::new(RichText::new(text).font(semibold(17.0)).color(p.text)).wrap());
    ui.add_space(8.0);
}

fn ask(ui: &mut Ui, p: &Palette, heading: &str, body: &str, action: &str, destructive: bool, outcome: &mut Outcome) {
    ui.set_width(400.0);
    title(ui, p, heading);
    ui.add(Label::new(muted(p, body)).wrap());
    ui.add_space(14.0);
    if !destructive && ui.input(|input| input.key_pressed(egui::Key::Enter)) {
        *outcome = Outcome::Confirmed;
    }
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        let confirmed = if destructive {
            super::widgets::danger_button(ui, p, action).clicked()
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

fn unsaved(
    ui: &mut Ui,
    p: &Palette,
    names: &[String],
    quitting: bool,
    unsent: usize,
    choice: &mut Option<CloseChoice>,
    outcome: &mut Outcome,
) {
    ui.set_width(420.0);
    let heading = match (names, quitting) {
        ([only], false) => tr!("Enregistrer « {only} » ?", only),
        ([_], true) => tr!("Enregistrer avant de quitter ?").to_owned(),
        (many, _) => tr!("Enregistrer les modifications de {count} fichiers ?", count = many.len()),
    };
    title(ui, p, &heading);
    let mut body = if names.len() == 1 {
        let name = &names[0];
        tr!("« {name} » a des modifications qui ne sont pas enregistrées. Sans enregistrement, elles seront perdues.", name)
    } else {
        tr!("Ces fichiers ont des modifications qui ne sont pas enregistrées. Sans enregistrement, elles seront perdues.").to_owned()
    };
    if unsent > 0 {
        body.push('\n');
        body.push_str(&trn!(
            unsent,
            "{n} fichier ouvert(s) dans un autre programme n'a (n'ont) pas été renvoyé(s) : sa copie locale est conservée une semaine.",
            "{n} fichiers ouvert(s) dans un autre programme n'a (n'ont) pas été renvoyé(s) : sa copie locale est conservée une semaine."
        ));
    }
    ui.add(Label::new(muted(p, body)).wrap());
    if names.len() > 1 {
        ui.add_space(6.0);
        for name in names.iter().take(8) {
            ui.label(RichText::new(format!("•  {name}")).color(p.text));
        }
        if names.len() > 8 {
            ui.label(small(p, trn!(names.len() - 8, "et {n} autres", "et {n} autres")));
        }
    }
    ui.add_space(14.0);
    if ui.input(|input| input.key_pressed(egui::Key::Enter)) {
        *choice = Some(CloseChoice::Save);
        *outcome = Outcome::Confirmed;
    }
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        let save = match (names.len(), quitting) {
            (1, false) => tr!("Enregistrer"),
            (1, true) => tr!("Enregistrer et quitter"),
            (_, false) => tr!("Tout enregistrer"),
            (_, true) => tr!("Tout enregistrer et quitter"),
        };
        if primary_button(ui, p, &format!("{}  {save}", icon::FLOPPY_DISK), true).clicked() {
            *choice = Some(CloseChoice::Save);
            *outcome = Outcome::Confirmed;
        }
        let discard = if quitting { tr!("Quitter sans enregistrer") } else { tr!("Ne pas enregistrer") };
        if ui.button(RichText::new(discard).color(p.danger)).clicked() {
            *choice = Some(CloseChoice::Discard);
            *outcome = Outcome::Confirmed;
        }
        if ui.button(tr!("Annuler")).clicked() {
            *outcome = Outcome::Cancelled;
        }
    });
}

fn choice_form(ui: &mut Ui, p: &Palette, form: &mut ChoiceForm, first_frame: bool, outcome: &mut Outcome) {
    let _ = first_frame;
    ui.set_width(460.0);
    title(ui, p, tr!("Éditeur de texte"));
    ui.label(small(p, tr!("Les fichiers texte s'ouvrent dans")));
    ui.add_space(2.0);
    let direct = tr!("Coloration, recherche, enregistrement direct sur le serveur");
    radio(ui, p, &mut form.builtin, true, tr!("L'éditeur intégré"), direct);
    let copied = tr!("Le fichier est copié sur cet ordinateur, puis renvoyé à chaque enregistrement");
    radio(ui, p, &mut form.builtin, false, tr!("Un autre programme"), copied);
    ui.add_enabled_ui(!form.builtin, |ui| {
        ui.horizontal(|ui| {
            ui.add_space(26.0);
            ui.add(
                egui::TextEdit::singleline(&mut form.program)
                    .hint_text(if cfg!(windows) { tr!("Vide pour le Bloc-notes") } else { tr!("Vide pour l'éditeur du système") })
                    .desired_width(300.0)
                    .margin(egui::Margin::symmetric(8, 5)),
            );
            if ui.button(tr!("Parcourir…")).clicked() {
                form.browse = true;
            }
        });
    });
    ui.add_space(12.0);
    ui.separator();
    ui.add_space(6.0);
    ui.label(RichText::new(tr!("Éditeur intégré")).font(semibold(14.0)).color(p.text));
    ui.add_space(4.0);
    egui::Grid::new("editor-prefs").num_columns(2).spacing(vec2(12.0, 8.0)).show(ui, |ui| {
        ui.label(small(p, tr!("Thème")));
        egui::ComboBox::from_id_salt("editor-theme")
            .width(240.0)
            .selected_text(syntax::theme_label(&form.prefs.theme))
            .show_ui(ui, |ui| {
                for (id, label) in THEMES {
                    ui.selectable_value(&mut form.prefs.theme, id.to_owned(), crate::i18n::lookup(label));
                }
            });
        ui.end_row();
        ui.label(small(p, tr!("Police")));
        let fonts = crate::terminal_ext::fonts::catalog();
        if fonts.is_none() {
            crate::terminal_ext::fonts::scan_in_background(ui.ctx());
        }
        let shown = if form.prefs.font.is_empty() { tr!("Par défaut").to_owned() } else { form.prefs.font.clone() };
        egui::ComboBox::from_id_salt("editor-font").width(240.0).selected_text(shown).show_ui(ui, |ui| {
            ui.selectable_value(&mut form.prefs.font, String::new(), tr!("Par défaut"));
            for family in fonts.unwrap_or_default() {
                ui.selectable_value(&mut form.prefs.font, family.name.clone(), &family.name);
            }
        });
        ui.end_row();
        ui.label(small(p, tr!("Taille")));
        ui.add(egui::Slider::new(&mut form.prefs.font_size, MIN_FONT_SIZE..=MAX_FONT_SIZE).step_by(0.5).suffix(" pt"));
        ui.end_row();
        ui.label(small(p, tr!("Tabulation")));
        ui.horizontal(|ui| {
            for width in [2, 4, 8] {
                ui.selectable_value(&mut form.prefs.tab_width, width, tr!("{width} colonnes", width));
            }
        });
        ui.end_row();
    });
    ui.add_space(4.0);
    checkbox(ui, p, &mut form.prefs.spaces, tr!("Indenter avec des espaces quand le fichier ne dit rien"));
    checkbox(ui, p, &mut form.prefs.wrap, tr!("Retour automatique à la ligne"));
    checkbox(ui, p, &mut form.prefs.whitespace, tr!("Afficher les espaces et les tabulations"));
    ui.add_space(14.0);
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if primary_button(ui, p, tr!("Enregistrer"), true).clicked() {
            *outcome = Outcome::Confirmed;
        }
        if ui.button(tr!("Annuler")).clicked() {
            *outcome = Outcome::Cancelled;
        }
    });
}

fn radio(ui: &mut Ui, p: &Palette, value: &mut bool, mine: bool, label: &str, hint: &str) {
    ui.horizontal(|ui| {
        let selected = *value == mine;
        let response = ui.add(egui::RadioButton::new(selected, RichText::new(label).color(p.text)));
        ui.label(small(p, hint));
        if response.clicked() {
            *value = mine;
        }
    });
}
