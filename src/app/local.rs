//! The App side of this computer: its tabs and shells, its files opened in
//! place, copying between the two views, and the steps scripts drive all
//! this with.

use std::collections::VecDeque;
use std::path::Path;

use egui::Id;

use super::App;
use crate::copy::{self, Destination, OtherView, Picked, Source};
use crate::dev;
use crate::launch::{self, Launch};
use crate::local::{self, ID};
use crate::profiles::Panes;
use crate::pty::{self, Shell};
use crate::tabs::{Side, TabPlace, is_split, place as place_tab};
use crate::terminal::{Backend, Status, Terminal};
use crate::ui::Action;
use crate::ui::dialogs::Dialog;
use crate::ui::local::{LocalAction, LocalDialog, trash_summary};
use crate::worker::Item;

/// Where the steps of a scripted drag wait for their frame.
fn dev_drag_id() -> Id {
    Id::new("local-dev-drag")
}

fn other(side: Side) -> Side {
    match side {
        Side::Left => Side::Right,
        Side::Right => Side::Left,
    }
}

impl App {
    /// Once per frame, before anything is drawn: the label of what is being
    /// dragged, and what each browser on screen has next to it.
    pub(super) fn local_frame(&mut self, ctx: &egui::Context) {
        let pending: Option<VecDeque<Vec<egui::Event>>> = ctx.data_mut(|data| data.get_temp(dev_drag_id()));
        if let Some(mut pending) = pending {
            if let Some(events) = pending.pop_front() {
                self.dev_input.extend(events);
                ctx.request_repaint();
            }
            ctx.data_mut(|data| data.insert_temp(dev_drag_id(), pending));
        }

        for tab in &mut self.tabs {
            tab.session.other_view = None;
        }
        if !is_split(&self.tabs) {
            return;
        }
        let shown = Side::BOTH.map(|side| self.shown_index(side));
        for (here, there) in [(shown[0], shown[1]), (shown[1], shown[0])] {
            let (Some(here), Some(there)) = (here, there) else {
                continue;
            };
            let target = &self.tabs[there];
            if !target.panes.files() {
                continue;
            }
            let session = &target.session;
            let view = OtherView {
                label: copy::place_label(&session.storage, &target.title, &session.path),
                local: session.storage.local().is_some(),
                writable: session.writable(),
            };
            self.tabs[here].session.other_view = Some(view);
        }
    }

    pub(super) fn apply_local(&mut self, action: LocalAction, now: f64) {
        match action {
            LocalAction::OpenBeside(id) => self.open_beside(id, now),
            LocalAction::Drop { files, into } => self.copy(files.source.clone(), into, now),
            LocalAction::CopyToOther { ask } => {
                let Some((source, dest)) = self.copy_to_other(now) else {
                    return;
                };
                if !ask {
                    return self.copy(source, dest, now);
                }
                if let Some(reason) = copy::refusal(&source, &dest) {
                    return self.toast(now, reason, true);
                }
                let summary = copy::summary(&source, &dest);
                self.open_dialog(Dialog::Local(LocalDialog::Copy { source, dest, summary }));
            }
        }
    }

    pub(super) fn answer_local(&mut self, dialog: LocalDialog, now: f64) {
        match dialog {
            LocalDialog::Trash { items, parent, .. } => {
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    tab.session.selection.clear();
                    self.worker.delete(tab.session.storage.clone(), items, parent);
                }
            }
            LocalDialog::Copy { source, dest, .. } => self.copy(source, dest, now),
        }
    }

    /// Starts a copy, unless it makes no sense.
    fn copy(&mut self, source: Source, dest: Destination, now: f64) {
        match copy::refusal(&source, &dest) {
            Some(reason) => self.toast(now, reason, true),
            None => copy::start(&self.worker, source, dest),
        }
    }

    /// The selection of the view in use, and the folder the other view shows.
    fn copy_to_other(&mut self, now: f64) -> Option<(Source, Destination)> {
        let there = self.shown_index(other(self.views.focus)).filter(|index| self.tabs[*index].panes.files());
        let Some(there) = there else {
            self.toast(now, tr!("Affichez une deuxième vue à côté pour y copier des fichiers."), true);
            return None;
        };
        let session = &self.tabs.get(self.active)?.session;
        let items: Vec<Picked> = session
            .selected()
            .map(|entry| Picked {
                name: entry.name.clone(),
                is_dir: entry.is_dir(),
                size: entry.size.unwrap_or(0),
            })
            .collect();
        let source = Source {
            storage: session.storage.clone(),
            folder: session.path.clone(),
            items,
        };
        let target = &self.tabs[there];
        let dest = Destination {
            storage: target.session.storage.clone(),
            folder: target.session.path.clone(),
            title: target.title.clone(),
        };
        Some((source, dest))
    }

    /// The layout of FileZilla: this computer on one side, the server on
    /// the other. A tab of this computer's files is reused when there is
    /// one; the server keeps the keyboard.
    fn open_beside(&mut self, id: Option<u64>, now: f64) {
        let index = match id {
            Some(id) => self.tab_index(id),
            None => (!self.tabs.is_empty()).then_some(self.active),
        };
        let Some(index) = index else {
            return;
        };
        let (server, side) = (self.tabs[index].id, self.tabs[index].side);
        let split = is_split(&self.tabs);
        // Without a second view, this computer goes left, as in FileZilla.
        let target = if split { other(side) } else { Side::Left };
        let files_tab = |tab: &crate::tabs::Tab| tab.profile_id == ID && tab.id != server;

        let computer = match self.tabs.iter().find(|tab| files_tab(tab) && tab.side == target) {
            Some(tab) => tab.id,
            None => match self.tabs.iter().find(|tab| files_tab(tab)).map(|tab| tab.id) {
                Some(found) => {
                    self.views.closing(&self.tabs, &[found]);
                    place_tab(&mut self.tabs, found, TabPlace::View(target));
                    found
                }
                None => {
                    let Some(mut tab) = self.build_tab(ID, now) else {
                        return;
                    };
                    tab.side = target;
                    let id = tab.id;
                    self.tabs.push(tab);
                    id
                }
            },
        };
        if !split {
            self.views.closing(&self.tabs, &[server]);
            place_tab(&mut self.tabs, server, TabPlace::View(Side::Right));
        }
        self.views.show(&self.tabs, computer);
        self.views.show(&self.tabs, server);
        self.settle();
    }

    /// Opens the terminal of a tab of this computer, in the folder its files
    /// show when they show one.
    pub(super) fn open_local_terminal(&mut self, index: usize) {
        let Some(tab) = self.tabs.get_mut(index) else {
            return;
        };
        let Some(shell) = tab.local_shell.clone() else {
            return;
        };
        let session = &tab.session;
        let cwd = session
            .storage
            .local()
            .and_then(|local| local.resolve(&session.path).ok())
            .filter(|dir| dir.is_dir());
        tab.terminal = Some(Terminal::start(&self.worker, Backend::Local(pty::Launch { shell, cwd })));
    }

    /// "Ouvrir le terminal dans ce dossier" on this computer. False for the
    /// tabs of servers.
    pub(super) fn local_terminal_here(&mut self) -> bool {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return false;
        };
        let Some(shell) = tab.local_shell.clone() else {
            return false;
        };
        if tab.panes == Panes::Files {
            tab.panes = Panes::Both;
        }
        let session = &tab.session;
        let dir = session.storage.local().and_then(|local| local.resolve(&session.path).ok());
        let running = tab
            .terminal
            .as_ref()
            .is_some_and(|terminal| matches!(terminal.status(), Status::Open | Status::Connecting));
        if running && let Some(terminal) = &mut tab.terminal {
            if let Some(dir) = dir
                && let Some(line) = shell.cd_command(&dir)
            {
                terminal.send(format!("{line}\r"));
            }
            terminal.wants_focus = true;
        } else {
            // Not started, or ended: it starts again right there.
            tab.terminal = None;
        }
        true
    }

    /// Opens a file of this computer where it is, with what the system uses
    /// for it, as Explorer would. False for the files of servers.
    pub(super) fn open_local_file(&mut self, name: &str, how: Launch, now: f64) -> bool {
        let Some(session) = self.session() else {
            return false;
        };
        let Some(local) = session.storage.local() else {
            return false;
        };
        let file = local.resolve(&session.path.child(name));
        let result = file.and_then(|file| open_in_place(&file, how, &self.settings.editor));
        if let Err(error) = result {
            self.toast(now, tr!("Ouverture impossible : {error}", error = format!("{error:#}")), true);
        }
        true
    }

    /// Deleting files of this computer: asked in words that say where they
    /// go.
    pub(super) fn ask_trash(&mut self, items: Vec<Item>) {
        let Some(session) = self.session() else {
            return;
        };
        let parent = session.path.clone();
        let recycle = session.storage.local().is_some_and(|local| local.recycles(&parent));
        let summary = trash_summary(&items, recycle);
        self.open_dialog(Dialog::Local(LocalDialog::Trash {
            items,
            parent,
            summary,
            recycle,
        }));
    }

    /// The steps of the dev script about this computer (see `run_dev_step`).
    pub(super) fn dev_step_local(&mut self, verb: &str, argument: &str, ctx: &egui::Context, now: f64) -> bool {
        let action = match verb {
            "local" => Action::Connect(ID.to_owned()),
            "localtab" => Action::OpenTab(ID.to_owned(), None),
            "localterm" => {
                let spec = if argument.is_empty() { "powershell" } else { argument };
                let Some(shell) = Shell::from_spec(spec) else {
                    return true;
                };
                Action::OpenTab(local::shell_id(&shell), Some(Panes::Terminal))
            }
            "localgoto" => {
                let found = self
                    .session()
                    .and_then(|session| session.storage.local()?.locate(Path::new(argument)));
                match found {
                    Some(path) => Action::Navigate(path),
                    None => return true,
                }
            }
            "beside" => Action::Local(LocalAction::OpenBeside(None)),
            "copyother" => Action::Local(LocalAction::CopyToOther { ask: false }),
            "askcopy" => Action::Local(LocalAction::CopyToOther { ask: true }),
            // `drag 300 200 900 240 hold` stops before releasing, for a
            // capture of what is under the pointer; `drop` then releases.
            "drag" | "drop" => {
                let numbers: Vec<f32> = argument.split(' ').filter_map(|word| word.parse().ok()).collect();
                let steps = match numbers[..] {
                    [x1, y1, x2, y2] if verb == "drag" => {
                        let to = egui::pos2(x2, y2);
                        ctx.data_mut(|data| data.insert_temp(dev_drag_id().with("at"), to));
                        drag_steps(egui::pos2(x1, y1), to, !argument.ends_with("hold"))
                    }
                    _ => {
                        let at: Option<egui::Pos2> = ctx.data_mut(|data| data.get_temp(dev_drag_id().with("at")));
                        let Some(at) = at else {
                            return true;
                        };
                        VecDeque::from([vec![release(at)], Vec::new()])
                    }
                };
                ctx.data_mut(|data| data.insert_temp(dev_drag_id(), steps));
                ctx.request_repaint();
                return true;
            }
            _ => return false,
        };
        self.apply(action, ctx, now);
        true
    }
}

fn button(pos: egui::Pos2, pressed: bool) -> egui::Event {
    egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    }
}

fn release(pos: egui::Pos2) -> egui::Event {
    button(pos, false)
}

/// The pointer events of a drag, one batch per frame: egui only starts a
/// drag once the pointer has moved with the button held.
fn drag_steps(from: egui::Pos2, to: egui::Pos2, released: bool) -> VecDeque<Vec<egui::Event>> {
    let mut steps = VecDeque::new();
    steps.push_back(vec![egui::Event::PointerMoved(from)]);
    steps.push_back(vec![button(from, true)]);
    for step in 1..=12 {
        steps.push_back(vec![egui::Event::PointerMoved(from + (to - from) * (step as f32 / 12.0))]);
    }
    steps.push_back(Vec::new());
    if released {
        steps.push_back(vec![release(to)]);
        steps.push_back(Vec::new());
    }
    steps
}

/// Opens a file of the disk: with its program, in the editor, or with the
/// program the user picks. A script never shows anything on the desktop.
fn open_in_place(file: &Path, how: Launch, editor: &str) -> anyhow::Result<()> {
    if how != Launch::Default || dev::active() {
        return launch::start(file, how, editor);
    }
    // What Explorer does on a double click, programs included: the file is
    // the user's own, not one fetched from a server.
    open::that_detached(file)?;
    Ok(())
}
