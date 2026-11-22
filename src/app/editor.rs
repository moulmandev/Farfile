//! The app side of the built-in editor: opening files in tabs of their
//! own, reading and saving them where they came from, the questions asked
//! before closing or leaving with changes, the editor's keys and the steps
//! scripts drive it with.

use std::sync::Arc;
use std::time::Instant;

use egui::{Event, Key, Modifiers};

use super::{App, ClosedTab};
use crate::dev;
use crate::editor::encoding::Indent;
use crate::editor::file::{self, MAX_SIZE, Pending, Saved};
use crate::editor::text::{Pos, Selection};
use crate::editor::{Editor, Reveal, Saving, State, Then, syntax};
use crate::launch::{self, Launch};
use crate::profiles::{Panes, Protocol};
use crate::session::Session;
use crate::storage::{RemotePath, Storage};
use crate::tabs::{Side, Tab, is_split};
use crate::ui::Action;
use crate::ui::dialogs::Dialog;
use crate::ui::editor::{ChangedChoice, ChoiceForm, CloseChoice, EditorAction, EditorDialog, RefusedChoice};
use crate::viewers::decode::{self, Encoding};

/// Bytes read to tell whether a file with an unknown name is text.
const SNIFF: u64 = 8 * 1024;

/// A file to open in the editor, and where it comes from.
#[derive(Clone)]
pub struct Source {
    pub storage: Arc<dyn Storage>,
    pub path: RemotePath,
    /// The name of its connection, for display.
    pub origin: String,
    pub profile_id: String,
    pub protocol: Protocol,
}

impl Source {
    /// The file the tab of an editor shows.
    pub fn of_tab(tab: &Tab) -> Option<Self> {
        tab.editor.as_ref()?;
        Some(Self {
            storage: tab.session.storage.clone(),
            path: tab.session.path.clone(),
            origin: tab.session.title.clone(),
            profile_id: tab.profile_id.clone(),
            protocol: tab.protocol,
        })
    }

    fn key(&self) -> String {
        crate::edits::key(&self.storage, &self.path)
    }
}

/// A file whose first bytes are being read, to tell whether it is text.
struct Sniff {
    source: Source,
    size: u64,
    beside: bool,
    pending: Pending<bool>,
}

/// Frame times measured for a script: the whole frame of the app, and the
/// part the file shown took.
struct Bench {
    what: BenchKind,
    left: usize,
    frames: Vec<(f64, f64)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BenchKind {
    Typing,
    Scrolling,
    /// Nothing done: what a frame costs anyway.
    Idle,
}

#[derive(Default)]
pub struct EditorState {
    sniffs: Vec<Sniff>,
    /// Leaving once the files being saved for it are.
    quit_after: bool,
    /// The tab shown last, to give the text the keyboard when it changes.
    last_shown: Option<u64>,
    frame_started: Option<Instant>,
    bench: Option<Bench>,
}

impl App {
    /// Once per frame, before anything is drawn: what the widgets read of
    /// the preferences, and the reads and saves that ended.
    pub(super) fn editor_frame(&mut self, ctx: &egui::Context, now: f64) {
        self.editor.frame_started = Some(Instant::now());
        crate::editor::prefs::publish(ctx, &self.settings.text_editor, &self.settings.editor);
        self.take_reads_and_saves(now);
        self.take_sniffs(now);
        self.name_file_tabs();
        // A file shown gets the keyboard, as a terminal does.
        let shown = self.tabs.get(self.active).map(|tab| tab.id);
        if shown != self.editor.last_shown {
            self.editor.last_shown = shown;
            if let Some(editor) = self.tabs.get_mut(self.active).and_then(|tab| tab.editor.as_deref_mut()) {
                editor.view.focus = true;
            }
        }
        self.serve_choice_dialog();
        self.leave_once_saved(ctx);
        self.bench_step(ctx);
    }

    /// The end of the frame: its time, while a script measures.
    pub(super) fn editor_frame_end(&mut self) {
        let Some(started) = self.editor.frame_started.take() else {
            return;
        };
        if let Some(bench) = &mut self.editor.bench {
            let editor = crate::ui::editor::take_frame_time(&self.worker.ctx());
            bench.frames.push((started.elapsed().as_secs_f64() * 1000.0, editor));
        }
    }

    fn take_reads_and_saves(&mut self, now: f64) {
        let prefs = self.settings.text_editor.clone();
        let mut notices: Vec<(String, bool)> = Vec::new();
        let mut changed = Vec::new();
        let mut close = Vec::new();
        let mut again = Vec::new();
        for tab in &mut self.tabs {
            let name = tab.session.path.name().to_owned();
            let id = tab.id;
            let Some(editor) = tab.editor.as_deref_mut() else {
                continue;
            };
            if let Some(result) = editor.load.as_ref().and_then(Pending::take) {
                editor.load = None;
                match result {
                    Ok(opened) => editor.opened(opened, &prefs),
                    Err(error) if editor.is_ready() => {
                        notices.push((tr!("Rechargement de « {name} » impossible : {error}", name, error), true));
                    }
                    Err(error) => editor.state = State::Failed(error),
                }
            }
            if let Some(result) = editor.save.as_ref().and_then(Pending::take) {
                editor.save = None;
                match result {
                    Ok(Saved::Done(stamp)) => {
                        editor.save_done(stamp, now);
                        match editor.then {
                            Then::Nothing => {}
                            _ if !editor.is_dirty() => close.push(id),
                            // Typed while it saved, and asked to save before
                            // closing: what was typed goes too.
                            _ => again.push(id),
                        }
                    }
                    Ok(Saved::Changed) => {
                        editor.saving = Saving::Idle;
                        // Nothing closes nor leaves until the question is
                        // answered: the answer says what goes on.
                        let then = std::mem::take(&mut editor.then);
                        if then == Then::Quit {
                            self.editor.quit_after = false;
                        }
                        changed.push((id, name.clone(), then));
                    }
                    Err(error) => {
                        editor.saving = Saving::Failed(error.clone());
                        editor.then = Then::Nothing;
                        self.editor.quit_after = false;
                        notices.push((tr!("Enregistrement de « {name} » impossible : {error}", name, error), true));
                    }
                }
            }
        }
        let mut changed = changed.into_iter();
        if let Some((id, name, then)) = changed.next() {
            self.open_dialog(Dialog::Editor(EditorDialog::Changed { id, name, then, choice: None }));
        }
        // One question at a time: the others are said, to be saved again.
        for (_, name, _) in changed {
            notices.push((tr!("« {name} » a changé sur le serveur : enregistrez-le de nouveau pour choisir.", name), true));
        }
        for (text, error) in notices {
            self.toast(now, text, error);
        }
        self.drop_tabs(&close, true);
        for id in again {
            self.save(id, false);
        }
    }

    /// The tabs of files are named after them, with their folder when two
    /// share a name.
    fn name_file_tabs(&mut self) {
        let names: Vec<String> = self
            .tabs
            .iter()
            .filter(|tab| tab.editor.is_some())
            .map(|tab| tab.session.path.name().to_owned())
            .collect();
        for tab in self.tabs.iter_mut().filter(|tab| tab.editor.is_some()) {
            let name = tab.session.path.name();
            let twice = names.iter().filter(|other| *other == name).count() > 1;
            let folder = tab.session.path.parent().map(|parent| parent.name().to_owned()).unwrap_or_default();
            let title = if twice && !folder.is_empty() { format!("{name} — {folder}") } else { name.to_owned() };
            if tab.title != title {
                tab.title = title;
            }
        }
    }

    /// The editor of the tab of this id.
    fn editor_of(&mut self, id: u64) -> Option<&mut Editor> {
        self.tabs.iter_mut().find(|tab| tab.id == id)?.editor.as_deref_mut()
    }

    /// The source of the named file of the folder shown.
    fn source_of(&self, name: &str) -> Option<(Source, u64)> {
        let tab = self.tabs.get(self.active)?;
        let session = &tab.session;
        let entry = session.rows().find(|entry| entry.name == name && !entry.is_dir())?;
        let source = Source {
            storage: session.storage.clone(),
            path: session.path.child(name),
            origin: session.title.clone(),
            profile_id: tab.profile_id.clone(),
            protocol: tab.protocol,
        };
        Some((source, entry.size.unwrap_or(0)))
    }

    /// Opens the file in the editor when that is what `how` means with the
    /// editor chosen. Returns false to let it open in a local program.
    pub(super) fn open_in_editor(&mut self, name: &str, how: Launch) -> bool {
        if !self.settings.text_editor.builtin || how == Launch::Choose {
            return false;
        }
        let Some((source, size)) = self.source_of(name) else {
            return false;
        };
        if how == Launch::Editor || syntax::text_by_name(name) {
            self.open_or_refuse(source, size, false);
            return true;
        }
        if known_binary(name) {
            return false;
        }
        // A name that says nothing: its first bytes tell.
        self.sniff(source, size, false);
        true
    }

    /// Opens a file in the editor, unless it is too big for it.
    fn open_or_refuse(&mut self, source: Source, size: u64, beside: bool) {
        if size > MAX_SIZE {
            let why = tr!(
                "Il pèse {size}, au-delà des {max} que l'éditeur intégré ouvre. L'aperçu en montre le début ; un autre programme peut l'ouvrir en entier.",
                size = crate::format::size(size),
                max = crate::format::size(MAX_SIZE)
            );
            let dialog = EditorDialog::Refused {
                name: source.path.name().to_owned(),
                why,
                external: self.settings.editor.clone(),
                choice: None,
            };
            self.open_dialog(Dialog::Editor(dialog));
            return;
        }
        self.open_file(source, beside);
    }

    /// Reads the start of a file to tell whether it is text, then opens it
    /// in the editor or in its program.
    fn sniff(&mut self, source: Source, size: u64, beside: bool) {
        let (storage, path) = (source.storage.clone(), source.path.clone());
        let pending = Pending::start(&self.worker, async move {
            let bytes = match storage.local().map(|local| local.resolve(&path)) {
                Some(Ok(real)) => read_start(&real).await,
                Some(Err(_)) => None,
                None => storage.read_range(&path, 0, SNIFF).await.ok().map(|chunk| chunk.bytes),
            };
            bytes.is_some_and(|bytes| decode::sniff(&bytes).is_some())
        });
        self.editor.sniffs.push(Sniff { source, size, beside, pending });
    }

    fn take_sniffs(&mut self, now: f64) {
        let mut done = Vec::new();
        self.editor.sniffs.retain(|sniff| match sniff.pending.take() {
            Some(text) => {
                done.push((sniff.source.clone(), sniff.size, sniff.beside, text));
                false
            }
            None => true,
        });
        for (source, size, beside, text) in done {
            if text {
                self.open_or_refuse(source, size, beside);
            } else {
                self.open_with_program(source, size, Launch::Default, now);
            }
        }
    }

    /// Opens a file in a local program, as before the editor existed.
    fn open_with_program(&mut self, source: Source, size: u64, how: Launch, now: f64) {
        if let Some(local) = source.storage.local() {
            let result = local.resolve(&source.path).and_then(|real| match how {
                // What Explorer does on a double click.
                Launch::Default if !dev::active() => open::that_detached(&real).map_err(Into::into),
                _ => launch::start(&real, how, &self.settings.editor),
            });
            if let Err(error) = result {
                self.toast(now, tr!("Ouverture impossible : {error}", error = format!("{error:#}")), true);
            }
            return;
        }
        if how == Launch::Default && launch::kind(source.path.name()) == launch::Kind::Program {
            return self.toast(now, tr!("Ce type de fichier ne s'ouvre pas depuis un serveur."), true);
        }
        self.edit_remote(source.storage, source.path, source.origin, size, how, now);
    }

    /// Shows the tab of the file if it is open already, or opens one: in
    /// the other view when there are two, so that the files stay in sight.
    fn open_file(&mut self, source: Source, beside: bool) {
        let key = source.key();
        if let Some(index) = self.tabs.iter().position(|tab| Source::of_tab(tab).is_some_and(|open| open.key() == key)) {
            let id = self.tabs[index].id;
            self.views.show(&self.tabs, id);
            self.settle();
            return;
        }
        let mut tab = self.editor_tab(&source);
        let from = self.tabs.get(self.active).map(|tab| (tab.id, tab.side));
        match from {
            Some((_, side)) if beside || is_split(&self.tabs) => {
                tab.side = match side {
                    Side::Left => Side::Right,
                    Side::Right => Side::Left,
                };
                let id = tab.id;
                self.tabs.push(tab);
                self.views.show(&self.tabs, id);
                self.settle();
            }
            Some((id, _)) => self.add_tab(tab, Some(id)),
            None => self.add_tab(tab, None),
        }
    }

    /// A tab for the file of `source`, reading it already.
    pub(super) fn editor_tab(&mut self, source: &Source) -> Tab {
        let session = Session::new(source.origin.clone(), source.storage.clone(), Some(source.path.clone()));
        let mut editor = Editor::loading(Some(self.worker.ctx()));
        editor.load = Some(self.read(source.storage.clone(), source.path.clone(), None));
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        Tab {
            id,
            side: Side::Left,
            profile_id: source.profile_id.clone(),
            title: source.path.name().to_owned(),
            protocol: source.protocol,
            session,
            ssh: None,
            terminal: None,
            panes: Panes::Editor,
            power: Default::default(),
            viewers: crate::viewers::Viewers::new(&self.worker),
            local_shell: None,
            editor: Some(Box::new(editor)),
        }
    }

    fn read(
        &self,
        storage: Arc<dyn Storage>,
        path: RemotePath,
        forced: Option<Encoding>,
    ) -> Pending<Result<file::Opened, String>> {
        Pending::start(&self.worker, async move {
            file::open(storage, path, MAX_SIZE, forced).await.map_err(|error| format!("{error:#}"))
        })
    }

    /// "Rouvrir l'onglet fermé" on a file that is open again since: it
    /// shows. True when `closed` was a file.
    pub(super) fn show_open_file(&mut self, closed: &ClosedTab) -> bool {
        let Some(file) = &closed.file else {
            return false;
        };
        let key = file.key();
        let Some(id) = self
            .tabs
            .iter()
            .find(|tab| Source::of_tab(tab).is_some_and(|open| open.key() == key))
            .map(|tab| tab.id)
        else {
            return false;
        };
        self.views.show(&self.tabs, id);
        self.settle();
        true
    }

    /// Reads the file of a tab again, keeping the place of the cursor.
    fn read_again(&mut self, id: u64, forced: Option<Encoding>) {
        let Some(index) = self.tab_index(id) else {
            return;
        };
        let (storage, path) = (self.tabs[index].session.storage.clone(), self.tabs[index].session.path.clone());
        let pending = self.read(storage, path, forced);
        if let Some(editor) = self.tabs[index].editor.as_deref_mut() {
            if editor.is_ready() {
                editor.prepare_reload();
            } else {
                editor.state = State::Loading;
            }
            editor.load = Some(pending);
        }
    }

    /// Writes the file of a tab where it came from. Unless `force`, a file
    /// changed on the server since is left alone and asked about.
    fn save(&mut self, id: u64, force: bool) {
        let Some(index) = self.tab_index(id) else {
            return;
        };
        let tab = &mut self.tabs[index];
        let (storage, path) = (tab.session.storage.clone(), tab.session.path.clone());
        let Some(editor) = tab.editor.as_deref_mut() else {
            return;
        };
        if !editor.is_ready() || editor.saving == Saving::Running {
            return;
        }
        let bytes = match editor.encoded() {
            Ok(bytes) => bytes,
            Err(problem) => {
                // Not written: nothing closes nor leaves on its account.
                editor.then = Then::Nothing;
                self.editor.quit_after = false;
                let encoding = editor.encoding;
                self.open_dialog(Dialog::Editor(EditorDialog::Unencodable { id, encoding, c: problem.c, line: problem.line }));
                return;
            }
        };
        editor.save_started();
        let expected = editor.stamp;
        let emitter = Some(self.worker.emitter());
        editor.save = Some(Pending::start(&self.worker, async move {
            file::save(storage, path, bytes, expected, force, emitter).await.map_err(|error| format!("{error:#}"))
        }));
    }

    /// Before closing tabs: files with changes are asked about. True when
    /// the question is open, and the tabs wait for its answer.
    pub(super) fn ask_before_closing(&mut self, ids: &[u64], remember: bool) -> bool {
        let names: Vec<String> = self
            .tabs
            .iter()
            .filter(|tab| ids.contains(&tab.id) && crate::ui::editor::is_dirty(tab))
            .map(|tab| tab.session.path.name().to_owned())
            .collect();
        if names.is_empty() {
            return false;
        }
        let dialog = EditorDialog::Unsaved { ids: ids.to_vec(), names, quitting: false, remember, unsent: 0, choice: None };
        self.open_dialog(Dialog::Editor(dialog));
        true
    }

    /// Before leaving: files with changes are asked about. True when the
    /// window stays open for the question.
    pub(super) fn ask_before_quitting(&mut self, ctx: &egui::Context) -> bool {
        let dirty: Vec<(u64, String)> = self
            .tabs
            .iter()
            .filter(|tab| crate::ui::editor::is_dirty(tab))
            .map(|tab| (tab.id, tab.session.path.name().to_owned()))
            .collect();
        if dirty.is_empty() {
            return false;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        let (ids, names) = dirty.into_iter().unzip();
        let unsent = self.edits.unsent();
        let dialog = EditorDialog::Unsaved { ids, names, quitting: true, remember: false, unsent, choice: None };
        self.open_dialog(Dialog::Editor(dialog));
        true
    }

    /// Leaves once the saves made for it are over, and only if no file has
    /// changes left: a file that could not be saved keeps the app open,
    /// and leaving is then given up rather than done later by surprise.
    fn leave_once_saved(&mut self, ctx: &egui::Context) {
        if !self.editor.quit_after {
            return;
        }
        let saving = self
            .tabs
            .iter()
            .any(|tab| tab.editor.as_ref().is_some_and(|editor| editor.saving == Saving::Running));
        if saving {
            return;
        }
        self.editor.quit_after = false;
        if !self.tabs.iter().any(crate::ui::editor::is_dirty) {
            self.quitting = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// A dialog of the editor was answered.
    pub(super) fn answer_editor(&mut self, dialog: EditorDialog, ctx: &egui::Context, now: f64) {
        match dialog {
            EditorDialog::Unsaved { ids, quitting, remember, choice, .. } => match choice {
                Some(CloseChoice::Discard) if quitting => {
                    self.quitting = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                Some(CloseChoice::Discard) => self.drop_tabs(&ids, remember),
                Some(CloseChoice::Save) => {
                    let then = if quitting { Then::Quit } else { Then::Close };
                    // Before the saves: one that cannot start says so.
                    self.editor.quit_after = quitting;
                    let mut clean = Vec::new();
                    for id in &ids {
                        let dirty = self.tabs.iter().any(|tab| tab.id == *id && crate::ui::editor::is_dirty(tab));
                        if !dirty {
                            clean.push(*id);
                            continue;
                        }
                        if let Some(editor) = self.editor_of(*id) {
                            editor.then = then;
                        }
                        self.save(*id, false);
                    }
                    if !quitting {
                        self.drop_tabs(&clean, remember);
                    }
                }
                None => {}
            },
            EditorDialog::Changed { id, then, choice, .. } => match choice {
                // What the save was for goes on once it is written.
                Some(ChangedChoice::Overwrite) => {
                    if let Some(editor) = self.editor_of(id) {
                        editor.then = then;
                    }
                    self.editor.quit_after |= then == Then::Quit;
                    self.save(id, true);
                }
                Some(ChangedChoice::Reload) => self.read_again(id, None),
                None => {}
            },
            EditorDialog::Reload { id, encoding, .. } => self.read_again(id, encoding),
            EditorDialog::Unencodable { id, .. } => {
                if let Some(editor) = self.editor_of(id) {
                    editor.set_encoding(Encoding::Utf8);
                }
                self.save(id, false);
            }
            EditorDialog::Refused { name, choice, .. } => match choice {
                Some(RefusedChoice::Preview) => {
                    self.apply(Action::Viewers(crate::ui::viewers::ViewerAction::Preview(name)), ctx, now);
                }
                Some(RefusedChoice::External) => self.open_externally(&name, Launch::Editor, false, now),
                None => {}
            },
            EditorDialog::Choice(form) => {
                let program = form.program.trim().to_owned();
                self.settings.editor = program;
                self.settings.text_editor = crate::editor::prefs::EditorPrefs { builtin: form.builtin, ..form.prefs };
                self.save_settings();
                let label = if form.builtin {
                    tr!("l'éditeur intégré").to_owned()
                } else {
                    launch::editor_label(&self.settings.editor)
                };
                self.toast(now, tr!("Les fichiers texte s'ouvrent désormais dans {label}", label), false);
            }
        }
    }

    /// "Parcourir…" of the dialog choosing the editor: the system's picker.
    fn serve_choice_dialog(&mut self) {
        let Some(Dialog::Editor(EditorDialog::Choice(form))) = &mut self.dialog else {
            return;
        };
        if !std::mem::take(&mut form.browse) || dev::active() {
            return;
        }
        let mut picker = rfd::FileDialog::new().set_title(tr!("Programme pour modifier les fichiers"));
        if cfg!(windows) {
            picker = picker.add_filter(tr!("Programmes"), &["exe", "cmd", "bat"]);
        }
        if let Some(program) = picker.pick_file() {
            form.program = program.display().to_string();
        }
    }

    pub(super) fn apply_editor(&mut self, action: EditorAction, ctx: &egui::Context, now: f64) {
        match action {
            EditorAction::OpenBuiltin(name) => {
                if let Some((source, size)) = self.source_of(&name) {
                    self.open_or_refuse(source, size, false);
                }
            }
            // Always in the other view, made if needed.
            EditorAction::OpenBeside(name) => {
                if let Some((source, size)) = self.source_of(&name) {
                    self.open_or_refuse(source, size, true);
                }
            }
            EditorAction::OpenExternal(name) => self.open_externally(&name, Launch::Editor, false, now),
            EditorAction::Save(id) => self.save(id, false),
            EditorAction::Reload(id) => self.ask_reload(id, None),
            EditorAction::ReopenAs(id, encoding) => self.ask_reload(id, Some(encoding)),
            EditorAction::ForceText(id) => self.read_again(id, Some(Encoding::Latin1)),
            EditorAction::OpenElsewhere(id) => self.edit_elsewhere(id, Launch::Editor, now),
            EditorAction::OpenWithSystem(id) => self.edit_elsewhere(id, Launch::Default, now),
            EditorAction::SetLanguage(id, name) => {
                if let (Some(syntax), Some(editor)) = (syntax::by_name(&name), self.editor_of(id)) {
                    editor.set_language(syntax);
                }
            }
            EditorAction::SetEncoding(id, encoding) => {
                if let Some(editor) = self.editor_of(id) {
                    editor.set_encoding(encoding);
                }
            }
            EditorAction::SetEnding(id, ending) => {
                if let Some(editor) = self.editor_of(id) {
                    editor.set_ending(ending);
                }
            }
            EditorAction::SetIndent(id, indent) => {
                if let Some(editor) = self.editor_of(id) {
                    editor.set_indent(indent);
                }
            }
            EditorAction::Find(id, replacing) => {
                if let Some(editor) = self.editor_of(id) {
                    editor.open_find(replacing);
                }
            }
            EditorAction::GoToLine(id) => {
                if let Some(editor) = self.editor_of(id) {
                    editor.view.goto = Some(String::new());
                }
            }
            EditorAction::Zoom(steps) => {
                let prefs = &mut self.settings.text_editor;
                prefs.font_size = prefs.zoomed(steps);
                self.save_settings();
            }
            EditorAction::Prefs(prefs) => {
                self.settings.text_editor = prefs;
                self.save_settings();
                ctx.request_repaint();
            }
            EditorAction::ChooseEditor => {
                let form = ChoiceForm {
                    builtin: self.settings.text_editor.builtin,
                    program: self.settings.editor.clone(),
                    prefs: self.settings.text_editor.clone(),
                    browse: false,
                };
                self.open_dialog(Dialog::Editor(EditorDialog::Choice(Box::new(form))));
            }
            EditorAction::Notice(text) => self.toast(now, text, false),
        }
    }

    fn ask_reload(&mut self, id: u64, encoding: Option<Encoding>) {
        let Some(tab) = self.tabs.iter().find(|tab| tab.id == id) else {
            return;
        };
        if crate::ui::editor::is_dirty(tab) {
            let name = tab.session.path.name().to_owned();
            self.open_dialog(Dialog::Editor(EditorDialog::Reload { id, name, encoding }));
        } else {
            self.read_again(id, encoding);
        }
    }

    /// The file of an editor tab, in a local program.
    fn edit_elsewhere(&mut self, id: u64, how: Launch, now: f64) {
        let Some(tab) = self.tabs.iter().find(|tab| tab.id == id) else {
            return;
        };
        if crate::ui::editor::is_dirty(tab) {
            let name = tab.session.path.name().to_owned();
            return self.toast(now, tr!("Enregistrez d'abord les modifications de « {name} » (Ctrl+S).", name), true);
        }
        let Some(source) = Source::of_tab(tab) else {
            return;
        };
        let size = tab.editor.as_ref().map_or(0, |editor| editor.size);
        self.open_with_program(source, size, how, now);
    }

    /// The keys of the whole editor, taken before anything else sees them:
    /// Ctrl+S, the search, "go to line", the size of the text.
    pub(super) fn editor_shortcuts(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let Some(editor) = tab.editor.as_deref() else {
            return;
        };
        let id = tab.id;
        let ready = editor.is_ready();
        // Escape closes a menu first.
        let find_open = editor.find.open && editor.view.goto.is_none() && !egui::Popup::is_any_open(ctx);
        let keys = ctx.input_mut(|input| {
            [
                take_ctrl(input, Key::S, Some(false)),
                take_ctrl(input, Key::F, Some(false)),
                take_ctrl(input, Key::H, Some(false)),
                take_ctrl(input, Key::G, Some(false)),
                input.consume_key(Modifiers::NONE, Key::F3),
                input.consume_key(Modifiers::SHIFT, Key::F3),
                take_ctrl(input, Key::Equals, None) | take_ctrl(input, Key::Plus, None),
                take_ctrl(input, Key::Minus, Some(false)),
                take_ctrl(input, Key::Num0, None),
                find_open && input.consume_key(Modifiers::NONE, Key::Escape),
            ]
        });
        let [save, find, replace, goto, next, previous, bigger, smaller, reset, close_find] = keys;
        let mut push = |action| actions.push(Action::Editor(action));
        if save {
            push(EditorAction::Save(id));
        }
        for (pressed, steps) in [(bigger, 1), (smaller, -1), (reset, 0)] {
            if pressed {
                push(EditorAction::Zoom(steps));
            }
        }
        if !ready {
            return;
        }
        if find || replace {
            push(EditorAction::Find(id, replace));
        }
        if goto {
            push(EditorAction::GoToLine(id));
        }
        let Some(editor) = self.tabs.get_mut(self.active).and_then(|tab| tab.editor.as_deref_mut()) else {
            return;
        };
        if (next || previous) && !editor.find.open {
            editor.open_find(false);
        }
        if next || previous {
            editor.find_next(next);
        }
        if close_find {
            editor.find.open = false;
            editor.view.focus = true;
        }
    }

    /// Steps the script plays a frame at a time, measuring each frame.
    fn bench_step(&mut self, ctx: &egui::Context) {
        let Some(bench) = &mut self.editor.bench else {
            return;
        };
        if bench.left == 0 {
            let report = report(bench);
            self.editor.bench = None;
            if let Some(folder) = dev::download_dir() {
                use std::io::Write;
                let _ = std::fs::create_dir_all(&folder);
                let file = std::fs::OpenOptions::new().create(true).append(true).open(folder.join("editor-bench.txt"));
                if let Ok(mut file) = file {
                    let _ = writeln!(file, "{report}");
                }
            }
            let now = ctx.input(|input| input.time);
            self.toast(now, report, false);
            return;
        }
        bench.left -= 1;
        let what = bench.what;
        ctx.request_repaint();
        let Some(editor) = self.tabs.get_mut(self.active).and_then(|tab| tab.editor.as_deref_mut()) else {
            return;
        };
        match what {
            BenchKind::Typing => {
                // As typed: words and spaces, and a new line now and then.
                if bench_line_full(editor) {
                    editor.newline();
                } else {
                    let letters = "editor ";
                    let index = editor.history.state() as usize % letters.len();
                    editor.insert(&letters[index..index + 1]);
                }
            }
            BenchKind::Scrolling => {
                editor.view.scroll_by += editor.view.viewport.y.max(200.0) * 0.5;
            }
            BenchKind::Idle => {}
        }
    }

    /// The steps of the dev script about the editor (see `run_dev_step`):
    /// `edit notes.txt` opens a file of the folder shown in the editor
    /// (`editbeside notes.txt` in the other view), `etype text` types into
    /// the file shown (`\n` for Enter), `egoto 120` or `egoto 120:4`,
    /// `eselect 3 0 5 2` (line, column, line, column, from 1), `efind word`,
    /// `ereplace other`, `ereplaceall`, `enext`, `eprev`, `efindopt case`
    /// (`word`, `regex`), `esave`, `ereload`, `elang Python`, `etheme
    /// Dracula`, `ewrap`, `espaces`, `ezoom 2`, `eencoding latin1`
    /// (`utf8`, `utf8bom`, `utf16le`, `utf16be`), `eending crlf`, `emenu
    /// language` (`indent`, `encoding`, `ending`) opens that menu of the
    /// status bar, `echoice` the dialog of the editor chosen, `ebuiltin
    /// off`, `equit` asks as closing the window would, `eanswer save`
    /// (`discard`, `overwrite`, `reload`, `external`, `preview`) answers the
    /// open question, `eundo`, `eredo`, `ecomment`, `ebench type 300` or
    /// `ebench scroll 200` measures that many frames into
    /// `editor-bench.txt` of S3V_DEV_DOWNLOAD_DIR.
    pub(super) fn dev_step_editor(&mut self, verb: &str, argument: &str, ctx: &egui::Context, now: f64) -> bool {
        let id = self.tabs.get(self.active).map_or(0, |tab| tab.id);
        let action = match verb {
            "edit" => EditorAction::OpenBuiltin(argument.to_owned()),
            "editbeside" => EditorAction::OpenBeside(argument.to_owned()),
            "esave" => EditorAction::Save(id),
            "ereload" => EditorAction::Reload(id),
            "eexternal" => EditorAction::OpenElsewhere(id),
            "elang" => EditorAction::SetLanguage(id, argument.to_owned()),
            "ezoom" => EditorAction::Zoom(argument.parse().unwrap_or(1)),
            "echoice" => EditorAction::ChooseEditor,
            "eending" => {
                let ending = match argument {
                    "crlf" => crate::editor::encoding::LineEnding::Crlf,
                    "cr" => crate::editor::encoding::LineEnding::Cr,
                    _ => crate::editor::encoding::LineEnding::Lf,
                };
                EditorAction::SetEnding(id, ending)
            }
            "eencoding" => {
                let encoding = match argument {
                    "latin1" => Encoding::Latin1,
                    "utf8bom" => Encoding::Utf8Bom,
                    "utf16le" => Encoding::Utf16Le,
                    "utf16be" => Encoding::Utf16Be,
                    _ => Encoding::Utf8,
                };
                EditorAction::SetEncoding(id, encoding)
            }
            "eindent" => {
                let width = argument.trim_start_matches("tabs").trim().parse().unwrap_or(4);
                EditorAction::SetIndent(id, Indent { tabs: argument.starts_with("tabs"), width })
            }
            "etheme" | "ewrap" | "espaces" | "ebuiltin" => {
                let mut prefs = self.settings.text_editor.clone();
                match verb {
                    "etheme" => prefs.theme = argument.to_owned(),
                    "ewrap" => prefs.wrap = !prefs.wrap,
                    "espaces" => prefs.whitespace = !prefs.whitespace,
                    _ => prefs.builtin = argument != "off",
                }
                EditorAction::Prefs(prefs)
            }
            "equit" => {
                self.ask_before_quitting(ctx);
                return true;
            }
            "eanswer" => {
                self.dev_answer_editor(argument);
                return true;
            }
            "emenu" => {
                ctx.data_mut(|data| data.insert_temp(crate::ui::editor::dev_menu_id(), argument.to_owned()));
                return true;
            }
            "ebench" => {
                let mut words = argument.split_whitespace();
                let what = match words.next() {
                    Some("scroll") => BenchKind::Scrolling,
                    Some("idle") => BenchKind::Idle,
                    _ => BenchKind::Typing,
                };
                let left = words.next().and_then(|count| count.parse().ok()).unwrap_or(200);
                self.editor.bench = Some(Bench { what, left, frames: Vec::with_capacity(left) });
                ctx.request_repaint();
                return true;
            }
            _ => return self.dev_step_text(verb, argument),
        };
        self.apply_editor(action, ctx, now);
        true
    }

    /// The steps that act on the text of the file shown.
    fn dev_step_text(&mut self, verb: &str, argument: &str) -> bool {
        let known = matches!(
            verb,
            "etype" | "egoto" | "eselect" | "efind" | "ereplace" | "ereplaceall" | "enext" | "eprev" | "efindopt" | "eundo"
                | "eredo" | "ecomment"
        );
        if !known {
            return false;
        }
        let Some(editor) = self.tabs.get_mut(self.active).and_then(|tab| tab.editor.as_deref_mut()) else {
            return true;
        };
        match verb {
            "etype" => {
                for (index, piece) in argument.split("\\n").enumerate() {
                    if index > 0 {
                        editor.newline();
                    }
                    if !piece.is_empty() {
                        editor.insert(piece);
                    }
                }
            }
            "egoto" => {
                let mut parts = argument.split(':').map(|part| part.trim().parse::<usize>().unwrap_or(1).max(1) - 1);
                let line = parts.next().unwrap_or(0);
                let col = parts.next().unwrap_or(0);
                editor.move_to(Pos::new(line, col), false);
                editor.view.reveal = Some(Reveal::Center);
            }
            "eselect" => {
                let numbers: Vec<usize> = argument.split_whitespace().filter_map(|n| n.parse().ok()).collect();
                if let [l1, c1, l2, c2] = numbers[..] {
                    let at = |line: usize, col: usize| editor.doc.clamp(Pos::new(line.max(1) - 1, col.max(1) - 1));
                    editor.sel = Selection { anchor: at(l1, c1), head: at(l2, c2) };
                    editor.view.reveal = Some(Reveal::Cursor);
                }
            }
            "efind" => {
                editor.open_find(false);
                editor.find.focus = None;
                editor.find.query.text = argument.to_owned();
                editor.refresh_matches();
                editor.find_next(true);
            }
            "ereplace" => {
                editor.open_find(true);
                editor.find.focus = None;
                editor.find.with = argument.to_owned();
            }
            "ereplaceall" => {
                editor.replace_all();
            }
            "enext" | "eprev" => {
                editor.find_next(verb == "enext");
            }
            "efindopt" => {
                let query = &mut editor.find.query;
                match argument {
                    "case" => query.case = !query.case,
                    "word" => query.word = !query.word,
                    _ => query.regex = !query.regex,
                }
                editor.refresh_matches();
            }
            "eundo" => editor.undo(),
            "eredo" => editor.redo(),
            _ => editor.toggle_comment(),
        }
        true
    }

    fn dev_answer_editor(&mut self, answer: &str) {
        let Some(Dialog::Editor(dialog)) = &mut self.dialog else {
            return;
        };
        match dialog {
            EditorDialog::Unsaved { choice, .. } => {
                *choice = Some(if answer == "discard" { CloseChoice::Discard } else { CloseChoice::Save });
            }
            EditorDialog::Changed { choice, .. } => {
                *choice = Some(if answer == "reload" { ChangedChoice::Reload } else { ChangedChoice::Overwrite });
            }
            EditorDialog::Refused { choice, .. } => {
                *choice = Some(if answer == "preview" { RefusedChoice::Preview } else { RefusedChoice::External });
            }
            _ => {}
        }
        self.dev_answer = Some(true);
    }
}

/// The line typed on by a measure is as long as lines of code get.
fn bench_line_full(editor: &Editor) -> bool {
    editor.doc.line(editor.sel.head.line).len() >= 48
}

/// The first bytes of a file of the disk.
async fn read_start(real: &std::path::Path) -> Option<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let file = tokio::fs::File::open(real).await.ok()?;
    let mut bytes = Vec::with_capacity(SNIFF as usize);
    file.take(SNIFF).read_to_end(&mut bytes).await.ok()?;
    Some(bytes)
}

/// Kinds of files never worth reading as text: they keep opening in their
/// program.
fn known_binary(name: &str) -> bool {
    let extension = name.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase()).unwrap_or_default();
    launch::kind(name) == launch::Kind::Program
        || matches!(
            extension.as_str(),
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "svg" | "avif" | "tif" | "tiff" | "psd"
                | "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "odt" | "ods" | "odp" | "rtf"
                | "zip" | "7z" | "rar" | "gz" | "tgz" | "tar" | "xz" | "zst" | "bz2" | "iso" | "img" | "dmg"
                | "mp3" | "wav" | "flac" | "ogg" | "m4a" | "mp4" | "mkv" | "webm" | "mov" | "avi"
                | "ttf" | "otf" | "woff" | "woff2" | "db" | "sqlite" | "bin" | "so" | "o" | "a" | "class" | "pyc"
        )
}

/// Takes a press of Ctrl+`wanted` out of the input, Alt not held (AltGr
/// types). `shift` is required, refused, or (None) either.
fn take_ctrl(input: &mut egui::InputState, wanted: Key, shift: Option<bool>) -> bool {
    let before = input.events.len();
    input.events.retain(|event| {
        !matches!(
            event,
            Event::Key { key, pressed: true, modifiers, .. }
                if *key == wanted
                    && (modifiers.ctrl || modifiers.command)
                    && !modifiers.alt
                    && shift.is_none_or(|shift| modifiers.shift == shift)
        )
    });
    input.events.len() != before
}

/// What the frames measured say, in a line.
fn report(bench: &Bench) -> String {
    let mut frames = bench.frames.clone();
    // The first frames build caches: they are not what typing feels like.
    if frames.len() > 10 {
        frames.drain(..5);
    }
    if frames.is_empty() {
        return "Mesure : aucune image".to_owned();
    }
    let sorted = |pick: fn(&(f64, f64)) -> f64| {
        let mut list: Vec<f64> = frames.iter().map(pick).collect();
        list.sort_by(f64::total_cmp);
        list
    };
    let (whole, editor) = (sorted(|frame| frame.0), sorted(|frame| frame.1));
    let at = |list: &[f64], share: f64| list[((list.len() - 1) as f64 * share).round() as usize];
    let mean = whole.iter().sum::<f64>() / whole.len() as f64;
    let what = match bench.what {
        BenchKind::Typing => "frappe",
        BenchKind::Scrolling => "défilement",
        BenchKind::Idle => "repos",
    };
    format!(
        "Mesure ({what}, {} images) : image entière moyenne {mean:.2} ms, médiane {:.2} ms, 95 % {:.2} ms, max {:.2} ms ; \
         dont l'éditeur médiane {:.2} ms, 95 % {:.2} ms",
        whole.len(),
        at(&whole, 0.5),
        at(&whole, 0.95),
        at(&whole, 1.0),
        at(&editor, 0.5),
        at(&editor, 0.95),
    )
}
