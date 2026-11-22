//! The app side of the viewers (src/viewers.rs): their actions, their keys
//! and the steps scripts drive them with.

use std::path::PathBuf;

use egui::{Key, KeyboardShortcut, Modifiers};

use super::App;
use crate::dev;
use crate::launch::{self, Launch};
use crate::storage::RemotePath;
use crate::tabs::Tab;
use crate::ui::Action;
use crate::ui::viewers::ViewerAction;
use crate::viewers::preview::{Body, Origin};
use crate::viewers::search::Kinds;
use crate::viewers::{Pane, Reveal, parallel_listings};
use crate::worker::Item;

impl App {
    pub(super) fn apply_viewers(&mut self, action: ViewerAction, ctx: &egui::Context, now: f64) {
        match action {
            ViewerAction::Preview(name) => self.preview_named(&name),
            ViewerAction::PreviewHit(index) => self.preview_hit(index),
            ViewerAction::Follow(name) => {
                let Some(tab) = self.tabs.get_mut(self.active) else {
                    return;
                };
                let session = &tab.session;
                let size = session.rows().find(|entry| entry.name == name).and_then(|entry| entry.size);
                let path = session.path.child(&name);
                tab.viewers.follow(session.storage.clone(), path, size.unwrap_or(0));
            }
            ViewerAction::FollowPath { path, size } => {
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    tab.viewers.follow(tab.session.storage.clone(), path, size);
                }
            }
            ViewerAction::Search => {
                if let Some(tab) = self.tabs.get_mut(self.active)
                    && tab.panes.files()
                {
                    let session = &tab.session;
                    tab.viewers.open_search(session.path.clone(), &session.filter);
                }
            }
            ViewerAction::Reveal { path, then } => self.reveal(path, then, ctx, now),
            ViewerAction::Open { path, how } => {
                let here = self.session().is_some_and(|session| {
                    path.parent().as_ref() == Some(&session.path) && session.row_of(path.name()).is_some()
                });
                if here {
                    self.open_remote(path.name(), how, false, now);
                } else {
                    self.reveal(path, Some(how), ctx, now);
                }
            }
            ViewerAction::Download(items) => self.download_items(items),
            ViewerAction::Copy { text, notice } => {
                ctx.copy_text(text);
                self.toast(now, notice, false);
            }
        }
    }

    fn preview_named(&mut self, name: &str) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let session = &tab.session;
        let Some(entry) = session.rows().find(|entry| entry.name == name && !entry.is_dir()).cloned() else {
            return;
        };
        let path = session.path.child(name);
        tab.viewers.show_preview(session.storage.clone(), Origin::Listing, path, entry);
    }

    fn preview_hit(&mut self, index: usize) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let Tab { session, viewers, .. } = tab;
        let Some(search) = &mut viewers.search else {
            return;
        };
        search.selected = Some(index);
        let Some((hit, path)) = search.selected_hit() else {
            return;
        };
        if hit.entry.is_dir() {
            return;
        }
        let entry = hit.entry.clone();
        viewers.results_focused = true;
        viewers.show_preview(session.storage.clone(), Origin::Search, path, entry);
    }

    /// Goes to the folder of `path`, where the panel selects it once listed.
    fn reveal(&mut self, path: RemotePath, then: Option<Launch>, ctx: &egui::Context, now: f64) {
        let Some(folder) = path.parent() else {
            return;
        };
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        tab.viewers.reveal = Some(Reveal {
            folder: folder.clone(),
            name: path.name().to_owned(),
            then,
        });
        if tab.session.path != folder {
            self.apply(Action::Navigate(folder), ctx, now);
        }
    }

    fn download_items(&mut self, items: Vec<Item>) {
        let Some(session) = self.session() else {
            return;
        };
        let storage = session.storage.clone();
        let start = self.last_download_dir.clone().or_else(|| {
            directories::UserDirs::new().and_then(|dirs| dirs.download_dir().map(PathBuf::from))
        });
        let mut dialog = rfd::FileDialog::new().set_title(tr!("Télécharger dans…"));
        if let Some(start) = start {
            dialog = dialog.set_directory(start);
        }
        if let Some(folder) = dev::download_dir().or_else(|| dialog.pick_folder()) {
            self.worker.download(storage, items, folder.clone());
            self.last_download_dir = Some(folder);
        }
    }

    /// The keys of the viewers: Ctrl+Shift+F, and while a preview shows,
    /// the space bar, Escape and the arrows. Called before the shortcuts of
    /// the app, which would otherwise move the selection or clear the filter.
    pub(super) fn viewer_shortcuts(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        if !tab.panes.files() || tab.terminal_focused() {
            return;
        }
        let shifted = Modifiers::CTRL | Modifiers::SHIFT;
        if ctx.input_mut(|input| input.consume_shortcut(&KeyboardShortcut::new(shifted, Key::F))) {
            actions.push(Action::Viewers(ViewerAction::Search));
            return;
        }
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        if tab.viewers.preview.is_some() {
            let (close, step) = ctx.input_mut(|input| {
                let mut key = |key| input.consume_key(Modifiers::NONE, key);
                let close = key(Key::Space) || key(Key::Escape);
                let next = key(Key::ArrowDown) || key(Key::ArrowRight);
                let previous = key(Key::ArrowUp) || key(Key::ArrowLeft);
                (close, if next { 1 } else if previous { -1 } else { 0 })
            });
            if close {
                tab.viewers.preview = None;
            } else if step != 0 {
                self.step_preview(step);
            }
            return;
        }
        if ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Space)) {
            self.preview_selection();
        }
    }

    /// The space bar: previews the selected file, or the selected search
    /// result when the results were clicked last.
    fn preview_selection(&mut self) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let Tab { session, viewers, .. } = tab;
        if viewers.results_focused
            && let Some(index) = viewers.search.as_ref().and_then(|search| search.selected)
        {
            self.preview_hit(index);
            return;
        }
        if session.selection.len() != 1 {
            return;
        }
        let Some(entry) = session.selected().next().filter(|entry| !entry.is_dir()).cloned() else {
            return;
        };
        let path = session.path.child(&entry.name);
        viewers.show_preview(session.storage.clone(), Origin::Listing, path, entry);
    }

    /// Previews the file before or after, in the list the preview was
    /// opened from; the selection follows.
    fn step_preview(&mut self, step: isize) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let Tab { session, viewers, .. } = tab;
        let Some(origin) = viewers.preview.as_ref().map(|preview| preview.origin) else {
            return;
        };
        match origin {
            Origin::Listing => {
                let current = viewers.preview.as_ref().map(|preview| preview.entry.name.clone()).unwrap_or_default();
                let files: Vec<_> = session.rows().filter(|entry| !entry.is_dir()).collect();
                let Some(at) = files.iter().position(|entry| entry.name == current) else {
                    return;
                };
                let next = at.checked_add_signed(step).and_then(|next| files.get(next));
                let Some(entry) = next.map(|entry| (*entry).clone()) else {
                    return;
                };
                session.select_only(&entry.name);
                session.scroll_to = session.row_of(&entry.name);
                let path = session.path.child(&entry.name);
                viewers.show_preview(session.storage.clone(), Origin::Listing, path, entry);
            }
            Origin::Search => {
                if let Some(index) = viewers.search.as_mut().and_then(|search| search.step_file(step)) {
                    self.preview_hit(index);
                }
            }
        }
    }

    /// The preview over the files of the tab at `shown`, in `area`.
    pub(super) fn viewers_overlay(
        &mut self,
        ui: &mut egui::Ui,
        shown: Option<usize>,
        area: egui::Rect,
        actions: &mut Vec<Action>,
    ) {
        // "Modifier dans …" names the editor files open in.
        let editor = if self.settings.text_editor.builtin {
            tr!("l'éditeur intégré").to_owned()
        } else {
            launch::editor_label(&self.settings.editor)
        };
        if let Some(tab) = shown.and_then(|index| self.tabs.get_mut(index))
            && tab.panes.files()
        {
            crate::ui::viewers::preview::overlay(ui, &self.palette, tab, area, &editor, actions);
        }
    }

    /// Script steps of the viewers, on the tab in use:
    /// `preview notes.txt` (as the space bar would), `previewnext`,
    /// `previewprev`, `previewclose`, `previewhex`, `previewtext`,
    /// `previewpretty`, `previewwrap`, `previewmore`, `previewfit`;
    /// `find *.log` searches the folder shown, after `findkind files`
    /// (`folders`, `all`), `findmin 10k`, `findmax 2M`, `findafter 01/11/2026`;
    /// `findstop`, `findselect 2` (the third result), `findshow 2` (as a
    /// double click), `findpreview 2`, `findhere`;
    /// `follow app.log`, `followfilter ERROR`, `followregex`, `followmark
    /// disk, timeout`, `followpause`, `followwrap`, `followclear`,
    /// `followtab 1` (0 is the search), `panelfold`.
    pub(super) fn dev_step_viewers(&mut self, verb: &str, argument: &str, ctx: &egui::Context, now: f64) -> bool {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return verb.starts_with("preview") || verb.starts_with("find") || verb.starts_with("follow");
        };
        let Tab { session, viewers, protocol, .. } = tab;
        match verb {
            "preview" => {
                session.select_only(argument);
                self.preview_named(argument);
            }
            "previewnext" | "previewprev" => self.step_preview(if verb == "previewnext" { 1 } else { -1 }),
            "previewclose" => viewers.preview = None,
            "previewhex" | "previewtext" | "previewpretty" | "previewwrap" | "previewmore" | "previewfit" => {
                let Some(preview) = &mut viewers.preview else {
                    return true;
                };
                match verb {
                    "previewhex" => preview.hex = true,
                    "previewtext" if matches!(preview.body, Body::Hex) => preview.force_text(),
                    "previewtext" => preview.hex = false,
                    "previewpretty" => preview.pretty = !preview.pretty,
                    "previewwrap" => preview.wrap = !preview.wrap,
                    "previewmore" => preview.load_more(),
                    _ => preview.actual_size = !preview.actual_size,
                }
            }
            "find" => {
                viewers.open_search(session.path.clone(), "");
                if let Some(search) = &mut viewers.search {
                    search.pattern = argument.to_owned();
                    search.focus = false;
                    search.start(&viewers.spawner, session.storage.clone(), parallel_listings(*protocol), now);
                }
            }
            "findkind" | "findmin" | "findmax" | "findafter" | "findstop" | "findhere" => {
                viewers.open_search(session.path.clone(), "");
                let Some(search) = &mut viewers.search else {
                    return true;
                };
                search.focus = false;
                match verb {
                    "findkind" => {
                        search.kinds = match argument {
                            "files" => Kinds::Files,
                            "folders" => Kinds::Folders,
                            _ => Kinds::All,
                        }
                    }
                    "findmin" => search.min_size = argument.to_owned(),
                    "findmax" => search.max_size = argument.to_owned(),
                    "findafter" => search.after = argument.to_owned(),
                    "findhere" => search.root = session.path.clone(),
                    _ => search.stop(now),
                }
            }
            "findselect" | "findshow" | "findpreview" => {
                let Some(search) = &mut viewers.search else {
                    return true;
                };
                let Some(&index) = argument.parse::<usize>().ok().and_then(|at| search.view().0.get(at)) else {
                    return true;
                };
                search.selected = Some(index);
                viewers.results_focused = true;
                let path = search.selected_hit().map(|(_, path)| path);
                match (verb, path) {
                    ("findshow", Some(path)) => self.reveal(path, None, ctx, now),
                    ("findpreview", _) => self.preview_hit(index),
                    _ => {}
                }
            }
            "follow" => {
                session.select_only(argument);
                self.apply_viewers(ViewerAction::Follow(argument.to_owned()), ctx, now);
            }
            "followfilter" | "followregex" | "followmark" | "followpause" | "followwrap" | "followclear" => {
                let Pane::Follow(id) = viewers.pane() else {
                    return true;
                };
                let Some(follow) = viewers.follows.iter_mut().find(|follow| follow.id == id) else {
                    return true;
                };
                match verb {
                    "followfilter" => follow.filter = argument.to_owned(),
                    "followregex" => follow.regex = !follow.regex,
                    "followmark" => follow.highlight = argument.to_owned(),
                    "followpause" => follow.set_paused(!follow.paused),
                    "followwrap" => follow.wrap = !follow.wrap,
                    _ => follow.log.clear(),
                }
            }
            "followtab" => {
                let at: usize = argument.parse().unwrap_or(0);
                viewers.pane = match at.checked_sub(1).and_then(|index| viewers.follows.get(index)) {
                    Some(follow) => Pane::Follow(follow.id),
                    None => Pane::Search,
                };
            }
            "panelfold" => viewers.folded = !viewers.folded,
            _ => return false,
        }
        true
    }
}
