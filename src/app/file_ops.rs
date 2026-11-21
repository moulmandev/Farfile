//! The file operations in the app: their actions, the events of their jobs,
//! their dialogs, shortcuts and script steps. The rules and the jobs are in
//! `crate::file_ops`, the drawing in `crate::ui::file_ops`.

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use egui::{Event, Key, Modifiers};

use super::App;
use crate::dev;
use crate::file_ops::{
    self as model, Change, Clipboard, ClipboardView, Dragged, Existing, Mode, OpsEvent, Picked, Plan, Renaming, Scope,
    Sizing, check_name, describe, moved_path, refuse, toggle_bookmark,
};
use crate::session::Session;
use crate::storage::ops::Tally;
use crate::storage::{Entry, EntryKind, RemotePath, Storage, storage_key};
use crate::ui::Action;
use crate::ui::dialogs::Dialog;
use crate::ui::file_ops::{ClashForm, ColumnKind, MoveForm, OpsAction, OpsDialog, PermissionsForm, PropertiesView};

/// Listings an operation changed are listed again after this delay.
const REFRESH_DELAY: f64 = 0.05;

/// The selected entries of a browser, as picked items.
fn picked(session: &Session) -> Vec<Picked> {
    session.selected().map(|entry| Picked::of(&session.path, entry)).collect()
}

/// What the current folder holds, when its listing is complete.
fn known_names(session: &Session) -> Option<HashMap<String, bool>> {
    let listing = session.listing().filter(|listing| !listing.loading && listing.error.is_none())?;
    Some(listing.entries.iter().map(|entry| (entry.name.clone(), entry.is_dir())).collect())
}

/// What the status bar says while a plan runs.
fn plan_label(plan: &Plan) -> String {
    let what = describe(&plan.items);
    match (plan.rename_to.is_some(), plan.mode) {
        (true, _) => format!("Renommage de {what}"),
        (false, Mode::Move) => format!("Déplacement de {what}"),
        (false, Mode::Copy) => format!("Copie de {what}"),
    }
}

impl App {
    pub(super) fn apply_file_ops(&mut self, action: OpsAction, ctx: &egui::Context, now: f64) {
        match action {
            OpsAction::StartRename(name) => self.start_rename(&name),
            OpsAction::Rename { from, to } => self.finish_rename(&from, &to, ctx, now),
            OpsAction::CancelRename => {
                if let Some(session) = self.session_mut() {
                    session.file_ops.renaming = None;
                }
            }
            OpsAction::AskMoveTo { copy } => self.ask_move_to(copy),
            OpsAction::Drop { dragged, dest } => {
                // Ctrl held on release copies, as in Explorer.
                let copy = ctx.input(|input| input.modifiers.ctrl);
                self.send(&dragged.storage, dragged.items.clone(), dest, copy, None, ctx, now);
            }
            OpsAction::Copy | OpsAction::Cut => self.take_selection(matches!(action, OpsAction::Cut), ctx, now),
            OpsAction::Paste(into) => self.paste(into, ctx, now),
            OpsAction::Duplicate => {
                let Some(session) = self.session() else {
                    return;
                };
                let (storage, items, dest) = (session.storage.clone(), picked(session), session.path.clone());
                let known = known_names(session);
                self.send(&storage, items, dest, true, known, ctx, now);
            }
            OpsAction::AskPermissions => self.ask_permissions(ctx),
            OpsAction::Properties => self.show_properties(ctx),
            OpsAction::ComputeSize => {
                let Some(session) = self.session() else {
                    return;
                };
                let storage = session.storage.clone();
                let folders: Vec<RemotePath> = picked(session).into_iter().filter(|item| item.is_dir).map(|item| item.path).collect();
                self.count_sizes(&storage, folders, ctx, now);
            }
            OpsAction::CancelJob(id) => {
                if let Some(job) = self.file_ops.jobs.iter().find(|job| job.id == id) {
                    job.cancel();
                }
            }
            OpsAction::CancelSize(key, path) => self.stop_size(key, &path),
            OpsAction::ToggleBookmark => self.toggle_bookmark(now),
            OpsAction::GoBookmark(index) => {
                let path = self.active_bookmarks().and_then(|(_, bookmarks)| bookmarks.get(index)).map(|b| b.path.clone());
                if let Some(path) = path {
                    self.apply(Action::Navigate(RemotePath::parse(&path)), ctx, now);
                }
            }
            OpsAction::AskRenameBookmark(index) => {
                let found = self.active_bookmarks().and_then(|(profile, bookmarks)| {
                    bookmarks.get(index).map(|bookmark| (profile, bookmark.name.clone()))
                });
                if let Some((profile, name)) = found {
                    self.open_dialog(Dialog::FileOps(OpsDialog::RenameBookmark { profile, index, name }));
                }
            }
            OpsAction::RemoveBookmark(index) => {
                if let Some((profile, _)) = self.active_bookmarks() {
                    self.edit_bookmarks(&profile, now, |bookmarks| {
                        if index < bookmarks.len() {
                            bookmarks.remove(index);
                        }
                    });
                }
            }
            OpsAction::ToggleColumn(kind) => {
                let columns = &mut self.settings.file_columns;
                match kind {
                    ColumnKind::Permissions => columns.permissions = !columns.permissions,
                    ColumnKind::Owner => columns.owner = !columns.owner,
                }
                self.save_settings();
            }
        }
    }

    /// Starts a move or a copy of `items` into `dest`, on `storage`.
    #[allow(clippy::too_many_arguments)]
    fn send(
        &mut self,
        storage: &Arc<dyn Storage>,
        items: Vec<Picked>,
        dest: RemotePath,
        copy: bool,
        known: Option<HashMap<String, bool>>,
        ctx: &egui::Context,
        now: f64,
    ) {
        if items.is_empty() {
            return;
        }
        if let Some(why) = refuse(&items, &dest, storage.has_buckets()) {
            self.toast(now, why, true);
            return;
        }
        let ops = storage.operations();
        if !(if copy { ops.copy } else { ops.rename }) {
            self.toast(now, "Ce type de connexion ne le permet pas.", true);
            return;
        }
        let plan = Plan {
            storage: storage.clone(),
            mode: if copy { Mode::Copy } else { Mode::Move },
            items,
            dest,
            rename_to: None,
            existing: Existing::Ask,
            known,
        };
        self.run_plan(plan, ctx, now);
    }

    fn run_plan(&mut self, plan: Plan, ctx: &egui::Context, now: f64) {
        let job = self.file_ops.start_job(plan_label(&plan), plan.items.len(), now);
        let reply = self.file_ops.reply(ctx);
        self.worker.spawn(model::run_plan(plan, job, reply));
    }

    fn start_rename(&mut self, name: &str) {
        self.file_ops.rename_serial += 1;
        let serial = self.file_ops.rename_serial;
        let Some(session) = self.session_mut() else {
            return;
        };
        if !session.storage.operations().rename || !session.writable() {
            return;
        }
        let Some(row) = session.row_of(name) else {
            return;
        };
        session.select_only(name);
        session.scroll_to = Some(row);
        session.file_ops.renaming = Some(Renaming {
            folder: session.path.clone(),
            name: name.to_owned(),
            serial,
        });
    }

    fn finish_rename(&mut self, from: &str, to: &str, ctx: &egui::Context, now: f64) {
        let Some(session) = self.session_mut() else {
            return;
        };
        session.file_ops.renaming = None;
        let to = match check_name(to) {
            Ok(to) => to.to_owned(),
            Err(why) => return self.toast(now, why, true),
        };
        if to == from {
            return;
        }
        let Some(item) = session.rows().find(|entry| entry.name == from).map(|entry| Picked::of(&session.path, entry)) else {
            return;
        };
        let plan = Plan {
            storage: session.storage.clone(),
            mode: Mode::Move,
            items: vec![item],
            dest: session.path.clone(),
            rename_to: Some(to),
            existing: Existing::Ask,
            known: known_names(session),
        };
        self.run_plan(plan, ctx, now);
    }

    fn ask_move_to(&mut self, copy: bool) {
        let Some(session) = self.session() else {
            return;
        };
        let items = picked(session);
        if items.is_empty() || !session.writable() {
            return;
        }
        let mut places = Vec::new();
        if let Some(parent) = session.path.parent() {
            places.push(("Dossier parent".to_owned(), parent.to_string()));
        }
        for bookmark in &session.file_ops.bookmarks {
            places.push((bookmark.name.clone(), bookmark.path.clone()));
        }
        let form = MoveForm {
            storage: session.storage.clone(),
            items,
            copy,
            path: session.path.to_string(),
            from: session.path.clone(),
            has_buckets: session.storage.has_buckets(),
            places,
        };
        self.open_dialog(Dialog::FileOps(OpsDialog::MoveTo(form)));
    }

    /// Ctrl+C and Ctrl+X: the items wait in the clipboard for Ctrl+V.
    fn take_selection(&mut self, cut: bool, ctx: &egui::Context, now: f64) {
        let Some(session) = self.session() else {
            return;
        };
        let items = picked(session);
        if items.is_empty() || !session.writable() {
            return;
        }
        let ops = session.storage.operations();
        if !(if cut { ops.rename } else { ops.copy }) {
            self.toast(now, "Ce type de connexion ne permet pas de copier sur le serveur.", true);
            return;
        }
        let text = items.iter().map(|item| item.path.to_string()).collect::<Vec<_>>().join("\n");
        // Ctrl+V only reaches the app while the system clipboard holds some
        // text: the paths go there, which is handy elsewhere too. Scripts
        // leave the user's clipboard alone.
        if !dev::active() {
            ctx.copy_text(text);
        }
        self.file_ops.clipboard = Some(Clipboard {
            storage: session.storage.clone(),
            connection: session.file_ops.connection.clone(),
            origin: session.title.clone(),
            items,
            cut,
        });
    }

    fn paste(&mut self, into: Option<String>, ctx: &egui::Context, now: f64) {
        let (Some(clipboard), Some(session)) = (&self.file_ops.clipboard, self.session()) else {
            return;
        };
        if !session.writable() {
            return self.toast(now, "Ouvrez d'abord un bucket pour y coller.", true);
        }
        let same_storage = storage_key(&clipboard.storage) == session.storage_key();
        if !same_storage && clipboard.connection != session.file_ops.connection {
            let text = format!(
                "Ces éléments viennent de « {} » : coller d'une connexion à une autre n'est pas encore possible ici.",
                clipboard.origin
            );
            return self.toast(now, text, true);
        }
        let dest = match &into {
            Some(name) => session.path.child(name),
            None => session.path.clone(),
        };
        let known = if into.is_none() { known_names(session) } else { None };
        // Another tab of the same connection reaches the same files.
        let (storage, items, cut) = (session.storage.clone(), clipboard.items.clone(), clipboard.cut);
        // What was cut goes once.
        if cut {
            self.file_ops.clipboard = None;
        }
        self.send(&storage, items, dest, !cut, known, ctx, now);
    }

    fn ask_permissions(&mut self, ctx: &egui::Context) {
        let Some(session) = self.session() else {
            return;
        };
        let items = picked(session);
        if items.is_empty() || !session.storage.operations().permissions {
            return;
        }
        let entries: Vec<&Entry> = session.selected().collect();
        let modes: Vec<Option<u32>> = entries.iter().map(|entry| entry.permissions).collect();
        let shared = |field: fn(&Entry) -> &Option<String>| {
            let first = field(entries.first()?).clone();
            entries.iter().all(|entry| *field(entry) == first).then_some(first).flatten()
        };
        let (owner, group) = (shared(|entry| &entry.owner), shared(|entry| &entry.group));
        let storage = session.storage.clone();
        let mut form = PermissionsForm::new(storage.clone(), items, &modes, owner, group);
        if form.can_owner {
            form.accounts = self.accounts_of(&storage, ctx);
        }
        self.open_dialog(Dialog::FileOps(OpsDialog::Permissions(Box::new(form))));
    }

    /// The user and group names of a connection, asked for the first time
    /// they are wanted: `None` until they come.
    fn accounts_of(&mut self, storage: &Arc<dyn Storage>, ctx: &egui::Context) -> Option<Arc<crate::storage::ops::Accounts>> {
        let key = storage_key(storage);
        if let Some(known) = self.file_ops.accounts.get(&key) {
            return known.clone();
        }
        self.file_ops.accounts.insert(key, None);
        let reply = self.file_ops.reply(ctx);
        self.worker.spawn(model::run_accounts(storage.clone(), reply));
        None
    }

    /// Alt+Enter: the selection, or the folder shown when nothing is selected.
    fn show_properties(&mut self, ctx: &egui::Context) {
        let request = self.file_ops.next_id();
        let Some(session) = self.session() else {
            return;
        };
        let (items, entries): (Vec<Picked>, Vec<Entry>) = if session.selection.is_empty() {
            if session.path.is_root() || session.known_path().is_none() {
                return;
            }
            let entry = Entry {
                name: session.path.name().to_owned(),
                kind: if session.storage.has_buckets() && session.path.segments().len() == 1 {
                    EntryKind::Bucket
                } else {
                    EntryKind::Folder
                },
                ..Entry::default()
            };
            let parent = session.path.parent().unwrap_or_default();
            (vec![Picked::of(&parent, &entry)], vec![entry])
        } else {
            (picked(session), session.selected().cloned().collect())
        };
        let single = items.len() == 1;
        let storage = session.storage.clone();
        let url = (single && !items[0].is_dir).then(|| storage.plain_url(&items[0].path)).flatten();
        let view = PropertiesView {
            storage: storage.clone(),
            storage_key: storage_key(&storage),
            connection: session.title.clone(),
            url,
            operations: storage.operations(),
            request,
            loaded: None,
            sizes: Vec::new(),
            wants_size: false,
            wants_permissions: false,
            stop_size: false,
            items: items.clone(),
            entries,
        };
        if single {
            let reply = self.file_ops.reply(ctx);
            self.worker.spawn(model::run_properties(storage, items[0].clone(), request, reply));
        }
        self.open_dialog(Dialog::FileOps(OpsDialog::Properties(Box::new(view))));
    }

    /// Counts the size of these folders, again for those counted already.
    fn count_sizes(&mut self, storage: &Arc<dyn Storage>, folders: Vec<RemotePath>, ctx: &egui::Context, now: f64) {
        let key = storage_key(storage);
        for path in folders {
            if self.file_ops.size_of(key, &path).is_some_and(|sizing| sizing.is_running()) {
                continue;
            }
            let sizing = Arc::new(Sizing {
                tally: Tally::default(),
                finished: AtomicBool::new(false),
                error: Mutex::new(None),
                started: now,
            });
            self.file_ops.sizes.insert((key, path.clone()), sizing.clone());
            let reply = self.file_ops.reply(ctx);
            self.worker.spawn(model::run_size(storage.clone(), path, sizing, reply));
        }
    }

    fn stop_size(&mut self, key: usize, path: &RemotePath) {
        if let Some(sizing) = self.file_ops.sizes.remove(&(key, path.clone())) {
            sizing.tally.cancel();
            self.file_ops.sizes_changed = true;
        }
    }

    /// The connection of the tab in use, and its bookmarks.
    fn active_bookmarks(&self) -> Option<(String, &[model::Bookmark])> {
        let tab = self.tabs.get(self.active)?;
        let profile = self.profiles.iter().find(|profile| profile.id == tab.profile_id)?;
        Some((profile.id.clone(), &profile.bookmarks))
    }

    /// Changes the bookmarks of a connection and saves them with it.
    fn edit_bookmarks(&mut self, profile_id: &str, now: f64, change: impl FnOnce(&mut Vec<model::Bookmark>)) {
        let Some(profile) = self.profiles.iter_mut().find(|profile| profile.id == profile_id) else {
            return;
        };
        change(&mut profile.bookmarks);
        let saved = self.persist_profiles();
        self.report_save_error(saved, now);
    }

    fn toggle_bookmark(&mut self, now: f64) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let (Some(path), profile_id) = (tab.session.known_path(), tab.profile_id.clone()) else {
            return;
        };
        let title = tab.title.clone();
        let mut added = false;
        self.edit_bookmarks(&profile_id, now, |bookmarks| added = toggle_bookmark(bookmarks, &path, &title));
        let name = if path.is_root() { title } else { path.name().to_owned() };
        let text = if added {
            format!("« {name} » ajouté aux signets")
        } else {
            format!("« {name} » retiré des signets")
        };
        self.toast(now, text, false);
    }

    /// Once a frame, before drawing: the jobs' news, and what the browsers
    /// show of the file operations.
    pub(super) fn file_ops_frame(&mut self, ctx: &egui::Context, now: f64) {
        self.drain_file_ops(now);
        self.sync_sessions(ctx);
        self.show_sizes();
        self.select_pending();
        self.serve_dialog(ctx, now);
        // Clashes found while another dialog was open wait for it to close.
        if self.dialog.is_none()
            && self.quick_open.is_none()
            && let Some((plan, clashes)) = self.file_ops.pending_clashes.pop_front()
        {
            self.open_dialog(Dialog::FileOps(OpsDialog::Clashes(ClashForm {
                plan,
                clashes,
                choice: Existing::Ask,
            })));
        }
        let running = !self.file_ops.jobs.is_empty() || self.file_ops.sizes.values().any(|sizing| sizing.is_running());
        if running {
            ctx.request_repaint_after(Duration::from_millis(150));
        }
    }

    fn drain_file_ops(&mut self, now: f64) {
        while let Ok(event) = self.file_ops.events.try_recv() {
            match event {
                OpsEvent::Finished {
                    job,
                    storage,
                    changed,
                    moved,
                    select,
                    notice,
                } => {
                    self.file_ops.end_job(job);
                    for (from, to) in &moved {
                        self.follow_move(storage, from, to, now);
                    }
                    for path in &changed {
                        self.ops_changed(storage, path, now);
                    }
                    if let Some((folder, names)) = select {
                        for index in 0..self.tabs.len() {
                            if self.on_connection(index, storage) && self.tabs[index].session.path == folder {
                                self.tabs[index].session.file_ops.select_after = Some((folder.clone(), names.clone()));
                            }
                        }
                    }
                    if let Some((text, error)) = notice {
                        self.toast(now, text, error);
                    }
                }
                OpsEvent::Clashes { job, plan, clashes } => {
                    self.file_ops.end_job(job);
                    self.file_ops.pending_clashes.push_back((plan, clashes));
                }
                OpsEvent::Properties { request, result } => {
                    if let Some(Dialog::FileOps(OpsDialog::Properties(view))) = &mut self.dialog
                        && view.request == request
                    {
                        view.loaded = Some(result);
                    }
                }
                OpsEvent::Accounts { storage, accounts } => {
                    self.file_ops.accounts.insert(storage, Some(accounts.clone()));
                    if let Some(Dialog::FileOps(OpsDialog::Permissions(form))) = &mut self.dialog
                        && storage_key(&form.storage) == storage
                    {
                        form.accounts = Some(accounts);
                    }
                    // The listings can now put names on the ids.
                    for tab in &mut self.tabs {
                        let session = &mut tab.session;
                        if session.storage_key() == storage && session.started && session.refresh_at.is_none() {
                            session.refresh_at = Some(now + REFRESH_DELAY);
                        }
                    }
                }
                OpsEvent::Sized { error } => {
                    self.file_ops.sizes_changed = true;
                    if let Some(error) = error.filter(|error| error != "annulé") {
                        self.toast(now, error, true);
                    }
                }
            }
        }
    }

    /// Whether a tab shows the files of this connection: through this very
    /// storage, or as another tab of the same saved connection.
    fn on_connection(&self, index: usize, storage: usize) -> bool {
        let tab = &self.tabs[index];
        tab.session.storage_key() == storage
            || self
                .tabs
                .iter()
                .any(|other| other.session.storage_key() == storage && other.profile_id == tab.profile_id)
    }

    /// The content of `path` changed: its listings are stale, and so are
    /// the sizes counted around it.
    fn ops_changed(&mut self, storage: usize, path: &RemotePath, now: f64) {
        for index in 0..self.tabs.len() {
            if !self.on_connection(index, storage) {
                continue;
            }
            let session = &mut self.tabs[index].session;
            self.file_ops.forget_sizes(session.storage_key(), path);
            session.invalidate_below(path);
            if session.path == *path && session.refresh_at.is_none() {
                session.refresh_at = Some(now + REFRESH_DELAY);
            }
        }
    }

    /// A folder moved: browsers inside it follow, and so do the bookmarks
    /// and the clipboard.
    fn follow_move(&mut self, storage: usize, from: &RemotePath, to: &RemotePath, now: f64) {
        let mut profiles = Vec::new();
        for index in 0..self.tabs.len() {
            if !self.on_connection(index, storage) {
                continue;
            }
            let tab = &mut self.tabs[index];
            profiles.push(tab.profile_id.clone());
            let session = &mut tab.session;
            if let Some(new) = moved_path(&session.path, from, to) {
                session.go_to(new);
                self.reload(index);
            }
        }
        for profile_id in profiles {
            let Some(profile) = self.profiles.iter_mut().find(|profile| profile.id == profile_id) else {
                continue;
            };
            let mut changed = false;
            for bookmark in &mut profile.bookmarks {
                if let Some(new) = moved_path(&RemotePath::parse(&bookmark.path), from, to) {
                    bookmark.path = new.to_string();
                    changed = true;
                }
            }
            if changed {
                let saved = self.persist_profiles();
                self.report_save_error(saved, now);
            }
        }
        if let Some(clipboard) = &mut self.file_ops.clipboard
            && storage_key(&clipboard.storage) == storage
        {
            for item in &mut clipboard.items {
                if let Some(new) = moved_path(&item.path, from, to) {
                    item.path = new;
                }
            }
        }
    }

    /// Copies into each browser what it draws of the file operations.
    fn sync_sessions(&mut self, ctx: &egui::Context) {
        let clipboard = self.file_ops.clipboard.as_ref().map(|clipboard| {
            (storage_key(&clipboard.storage), clipboard.connection.as_str(), clipboard.items.len(), clipboard.cut)
        });
        let mut wanted = Vec::new();
        for tab in &mut self.tabs {
            let bookmarks = self
                .profiles
                .iter()
                .find(|profile| profile.id == tab.profile_id)
                .map(|profile| profile.bookmarks.as_slice())
                .unwrap_or_default();
            let session = &mut tab.session;
            let state = &mut session.file_ops;
            if state.connection != tab.profile_id {
                state.connection.clone_from(&tab.profile_id);
            }
            if state.bookmarks != bookmarks {
                state.bookmarks = bookmarks.to_vec();
            }
            state.columns = self.settings.file_columns;
            let key = storage_key(&session.storage);
            state.clipboard = clipboard.map(|(from, connection, count, cut)| ClipboardView {
                count,
                cut,
                here: from == key || connection == tab.profile_id,
            });
            if state.renaming.as_ref().is_some_and(|renaming| renaming.folder != session.path) {
                state.renaming = None;
            }
            state.sizing.clear();
            for ((sized_key, path), sizing) in &self.file_ops.sizes {
                if *sized_key == key && sizing.is_running() && path.parent().as_ref() == Some(&session.path) {
                    state.sizing.insert(path.name().to_owned());
                }
            }
            if state.columns.owner
                && session.storage.operations().owner
                && session.started
                && !self.file_ops.accounts.contains_key(&key)
            {
                wanted.push(session.storage.clone());
            }
        }
        for storage in wanted {
            self.accounts_of(&storage, ctx);
        }
    }

    /// Folder sizes go in the size column of the listings, live while they
    /// are counted.
    fn show_sizes(&mut self) {
        let reset = std::mem::take(&mut self.file_ops.sizes_changed);
        if self.file_ops.sizes.is_empty() && !reset {
            return;
        }
        let sizes = &self.file_ops.sizes;
        for tab in &mut self.tabs {
            let session = &mut tab.session;
            let (key, folder) = (session.storage_key(), session.path.clone());
            let mut changed = false;
            if let Some(listing) = session.listing_mut() {
                for (path, sizing) in sizes.iter().filter(|((k, path), _)| *k == key && path.parent().as_ref() == Some(&folder)) {
                    let size = (!sizing.failed()).then(|| sizing.tally.snapshot().bytes);
                    let name = path.1.name();
                    if let Some(entry) = listing.entries.iter_mut().find(|entry| entry.is_dir() && entry.name == name)
                        && entry.size != size
                    {
                        entry.size = size;
                        changed = true;
                    }
                }
                // Sizes forgotten since: back to none.
                if reset {
                    for entry in listing.entries.iter_mut().filter(|entry| entry.is_dir() && entry.size.is_some()) {
                        if !sizes.contains_key(&(key, folder.child(&entry.name))) {
                            entry.size = None;
                            changed = true;
                        }
                    }
                }
            }
            if changed {
                session.mark_view_stale();
            }
        }
    }

    /// Selects what an operation brought into a folder, once its listing
    /// came back.
    fn select_pending(&mut self) {
        for tab in &mut self.tabs {
            let session = &mut tab.session;
            let Some((folder, names)) = &session.file_ops.select_after else {
                continue;
            };
            if *folder != session.path {
                session.file_ops.select_after = None;
                continue;
            }
            let ready = session.refresh_at.is_none() && session.listing().is_some_and(|listing| !listing.loading);
            if !ready {
                continue;
            }
            let names = names.clone();
            session.file_ops.select_after = None;
            session.refresh_view();
            session.selection.clear();
            let mut first = None;
            for name in &names {
                if let Some(row) = session.row_of(name) {
                    session.selection.insert(name.clone());
                    first = Some(first.map_or(row, |first: usize| first.min(row)));
                }
            }
            session.anchor = names.first().cloned();
            session.scroll_to = first;
        }
    }

    /// What the open dialog asked for since the last frame, and the sizes it
    /// shows.
    fn serve_dialog(&mut self, ctx: &egui::Context, now: f64) {
        let Some(Dialog::FileOps(OpsDialog::Properties(view))) = &mut self.dialog else {
            return;
        };
        let folders: Vec<RemotePath> = view.items.iter().filter(|item| item.is_dir).map(|item| item.path.clone()).collect();
        view.sizes = folders.iter().map(|path| self.file_ops.size_of(view.storage_key, path).cloned()).collect();
        let (size, stop, permissions) = (
            std::mem::take(&mut view.wants_size),
            std::mem::take(&mut view.stop_size),
            std::mem::take(&mut view.wants_permissions),
        );
        let (storage, key, items, entries) = (view.storage.clone(), view.storage_key, view.items.clone(), view.entries.clone());
        if size {
            self.count_sizes(&storage, folders.clone(), ctx, now);
        }
        if stop {
            for path in &folders {
                self.stop_size(key, path);
            }
        }
        if permissions {
            let modes: Vec<Option<u32>> = entries.iter().map(|entry| entry.permissions).collect();
            let first = entries.first();
            let mut form = PermissionsForm::new(
                storage.clone(),
                items,
                &modes,
                first.and_then(|entry| entry.owner.clone()),
                first.and_then(|entry| entry.group.clone()),
            );
            if form.can_owner {
                form.accounts = self.accounts_of(&storage, ctx);
            }
            self.open_dialog(Dialog::FileOps(OpsDialog::Permissions(Box::new(form))));
        }
    }

    /// A dialog of the file operations was confirmed.
    pub(super) fn answer_file_ops(&mut self, dialog: OpsDialog, ctx: &egui::Context, now: f64) {
        match dialog {
            OpsDialog::MoveTo(form) => {
                let Ok(dest) = form.destination() else {
                    return;
                };
                let known = self
                    .session()
                    .filter(|session| session.storage_key() == storage_key(&form.storage) && session.path == dest)
                    .and_then(known_names);
                self.send(&form.storage, form.items, dest, form.copy, known, ctx, now);
            }
            OpsDialog::Clashes(form) => {
                let mut plan = form.plan;
                plan.existing = form.choice;
                if plan.existing != Existing::Ask {
                    self.run_plan(plan, ctx, now);
                }
            }
            OpsDialog::Permissions(form) => {
                let Ok(owner) = form.owner_change() else {
                    return;
                };
                let mode = (form.touched || form.recursive).then_some(form.mode);
                if mode.is_none() && owner.is_none() {
                    return;
                }
                let label = format!("Permissions de {}", describe(&form.items));
                let job = self.file_ops.start_job(label, form.items.len(), now);
                let change = Change {
                    storage: form.storage.clone(),
                    items: form.items,
                    mode,
                    owner,
                    recursive: form.recursive.then_some(form.scope),
                };
                let reply = self.file_ops.reply(ctx);
                self.worker.spawn(model::run_change(change, job, reply));
            }
            OpsDialog::Properties(_) => {}
            OpsDialog::RenameBookmark { profile, index, name } => {
                let name = name.trim().to_owned();
                self.edit_bookmarks(&profile, now, |bookmarks| {
                    if let Some(bookmark) = bookmarks.get_mut(index) {
                        bookmark.name = name;
                    }
                });
            }
        }
    }

    /// F2, Ctrl+C, Ctrl+X, Ctrl+V, Ctrl+D and Alt+Enter in a file browser.
    /// Ctrl+C and the like arrive as clipboard events from a keyboard, as
    /// key presses from a script.
    pub(super) fn file_ops_shortcuts(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        if !tab.panes.files() || tab.terminal_focused() || ctx.egui_wants_keyboard_input() {
            return;
        }
        let taken = |input: &mut egui::InputState, wanted: fn(&Event) -> bool| {
            let before = input.events.len();
            input.events.retain(|event| !wanted(event));
            input.events.len() != before
        };
        let [rename, bookmark, properties, copy, cut, paste] = ctx.input_mut(|input| {
            [
                input.consume_key(Modifiers::NONE, Key::F2),
                input.consume_key(Modifiers::CTRL, Key::D),
                input.consume_key(Modifiers::ALT, Key::Enter),
                input.consume_key(Modifiers::CTRL, Key::C) | taken(input, |event| matches!(event, Event::Copy)),
                input.consume_key(Modifiers::CTRL, Key::X) | taken(input, |event| matches!(event, Event::Cut)),
                input.consume_key(Modifiers::CTRL, Key::V) | taken(input, |event| matches!(event, Event::Paste(_))),
            ]
        });
        let session = &tab.session;
        let mut push = |action| actions.push(Action::FileOps(action));
        if rename
            && session.selection.len() == 1
            && let Some(name) = session.selection.iter().next()
        {
            push(OpsAction::StartRename(name.clone()));
        }
        if bookmark {
            push(OpsAction::ToggleBookmark);
        }
        if properties {
            push(OpsAction::Properties);
        }
        if copy && !session.selection.is_empty() {
            push(OpsAction::Copy);
        }
        if cut && !session.selection.is_empty() {
            push(OpsAction::Cut);
        }
        if paste && self.file_ops.clipboard.is_some() {
            push(OpsAction::Paste(None));
        }
    }

    /// Script steps of the file operations:
    /// `rename old > new` renames at once, `startrename name` edits the name
    /// in place; `copy`, `cut`, `paste` (`paste folder` into that folder),
    /// `duplicatefile`; `moveinto /dest` and `copyto /dest` send the selection,
    /// `askmove` and `askcopy` open the dialog; `dropon folder` drops the
    /// selection on that folder row, `dropcrumb 1` on that part of the path;
    /// `perms` opens the permissions, then `octal 750`, `recurse files`
    /// (`folders`, `all`, `off`), `owner name`, `group name`; `props`,
    /// `size`, `propsize` (its button in the properties); `bookmark`,
    /// `gobookmark 0`, `renamebookmark 0`, `removebookmark 0`; `columns
    /// perms` or `columns owner`; `fill text` types in the field of the
    /// dialog; `choose replace` (or `skip`) answers the clashes before
    /// `confirm`; `cancelops` stops every operation.
    pub(super) fn dev_step_file_ops(&mut self, verb: &str, argument: &str, ctx: &egui::Context, now: f64) -> bool {
        let action = match verb {
            "rename" => {
                let (from, to) = argument.split_once('>').unwrap_or((argument, ""));
                OpsAction::Rename {
                    from: from.trim().to_owned(),
                    to: to.trim().to_owned(),
                }
            }
            "startrename" => OpsAction::StartRename(argument.to_owned()),
            "copy" => OpsAction::Copy,
            "cut" => OpsAction::Cut,
            "paste" => OpsAction::Paste((!argument.is_empty()).then(|| argument.to_owned())),
            "duplicatefile" => OpsAction::Duplicate,
            "moveinto" | "copyto" => {
                if let Some(session) = self.session() {
                    let dest = model::resolve_path(&session.path, argument);
                    let (storage, items) = (session.storage.clone(), picked(session));
                    self.send(&storage, items, dest, verb == "copyto", None, ctx, now);
                }
                return true;
            }
            "askmove" | "askcopy" => OpsAction::AskMoveTo { copy: verb == "askcopy" },
            "dropon" | "dropcrumb" => {
                let Some(session) = self.session() else {
                    return true;
                };
                let dest = if verb == "dropon" {
                    session.path.child(argument)
                } else {
                    session.path.ancestor(argument.parse().unwrap_or(0))
                };
                let dragged = Dragged {
                    storage: session.storage.clone(),
                    connection: session.file_ops.connection.clone(),
                    folder: session.path.clone(),
                    items: picked(session),
                    listing: crate::ui::local::listing_key(session),
                };
                OpsAction::Drop {
                    dragged: Arc::new(dragged),
                    dest,
                }
            }
            "perms" => OpsAction::AskPermissions,
            "octal" | "recurse" | "owner" | "group" => {
                if let Some(Dialog::FileOps(OpsDialog::Permissions(form))) = &mut self.dialog {
                    match verb {
                        "octal" => {
                            form.octal = argument.to_owned();
                            form.typed();
                        }
                        "recurse" => {
                            form.recursive = argument != "off";
                            form.scope = match argument {
                                "files" => Scope::Files,
                                "folders" => Scope::Folders,
                                _ => Scope::All,
                            };
                        }
                        "owner" => form.owner = argument.to_owned(),
                        _ => form.group = argument.to_owned(),
                    }
                }
                return true;
            }
            "props" => OpsAction::Properties,
            "size" => OpsAction::ComputeSize,
            "propsize" => {
                if let Some(Dialog::FileOps(OpsDialog::Properties(view))) = &mut self.dialog {
                    view.wants_size = true;
                }
                return true;
            }
            "bookmark" => OpsAction::ToggleBookmark,
            "gobookmark" | "renamebookmark" | "removebookmark" => {
                let index = argument.parse().unwrap_or(0);
                match verb {
                    "gobookmark" => OpsAction::GoBookmark(index),
                    "renamebookmark" => OpsAction::AskRenameBookmark(index),
                    _ => OpsAction::RemoveBookmark(index),
                }
            }
            "columns" => OpsAction::ToggleColumn(if argument == "owner" { ColumnKind::Owner } else { ColumnKind::Permissions }),
            "fill" => {
                if let Some(Dialog::FileOps(dialog)) = &mut self.dialog {
                    crate::ui::file_ops::fill(dialog, argument);
                }
                return true;
            }
            "choose" => {
                if let Some(Dialog::FileOps(OpsDialog::Clashes(form))) = &mut self.dialog {
                    form.choice = if argument == "skip" { Existing::Skip } else { Existing::Replace };
                }
                return true;
            }
            "cancelops" => {
                for job in &self.file_ops.jobs {
                    job.cancel();
                }
                for sizing in self.file_ops.sizes.values() {
                    sizing.tally.cancel();
                }
                return true;
            }
            _ => return false,
        };
        self.apply_file_ops(action, ctx, now);
        true
    }
}
