//! The glue between the SSH power tools ([`crate::ssh_power`]) and the app:
//! their actions and dialogs, typing snippets, fanning out broadcast input,
//! sending startup commands, and the script steps that drive them.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use super::App;
use crate::profiles::Panes;
use crate::ssh_power::broadcast::{self, Typed, type_in};
use crate::ssh_power::snippets::{self, Readiness, Snippet, Step, screen_signature};
use crate::ssh_power::stats::Panel;
use crate::tabs::Side;
use crate::terminal::{Status, Terminal};
use crate::ui::dialogs::Dialog;
use crate::ui::ssh_power::{Manager, PowerAction, PowerDialog, Published, VariablesForm, picker_id, publish};

/// Seconds between a change in the snippets dialog and its saving, so that
/// typing does not write the file on every key.
const SAVE_DELAY: f64 = 1.0;

pub(super) struct PowerState {
    /// The library, in alphabetical order.
    snippets: Arc<Vec<Snippet>>,
    /// The snippets dialog sends the library here each time it changes.
    changes: (Sender<Vec<Snippet>>, Receiver<Vec<Snippet>>),
    /// The library changed: save it then.
    save_at: Option<f64>,
}

impl PowerState {
    pub(super) fn load() -> Self {
        Self {
            snippets: Arc::new(snippets::load()),
            changes: channel(),
            save_at: None,
        }
    }
}

/// Who a command is typed for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Typing {
    /// The user ran it: it goes to the other broadcasting terminals too, and
    /// its tab comes into view with the keyboard.
    User,
    /// A startup command of the connection: this terminal only, quietly.
    Startup,
}

impl App {
    pub(super) fn apply_ssh_power(&mut self, action: PowerAction, now: f64) {
        match action {
            PowerAction::ManageSnippets(id) => self.open_snippets(id.as_deref(), false),
            PowerAction::NewSnippet => self.open_snippets(None, true),
            PowerAction::RunSnippet { snippet, tab } => self.run_snippet(&snippet, tab, now),
            PowerAction::ToggleBroadcast(id) => {
                if let Some(index) = self.tab_index(id) {
                    let tab = &mut self.tabs[index];
                    tab.power.broadcast = !tab.power.broadcast;
                    broadcast::sync(tab);
                }
            }
            PowerAction::BroadcastAll => {
                for tab in &mut self.tabs {
                    if tab.terminal.is_some() {
                        tab.power.broadcast = true;
                        broadcast::sync(tab);
                    }
                }
                let count = broadcast::receivers(&self.tabs);
                self.toast(now, format!("La saisie est diffusée à {count} terminaux"), false);
            }
            PowerAction::StopBroadcast => {
                for tab in &mut self.tabs {
                    tab.power.broadcast = false;
                    broadcast::sync(tab);
                }
                self.toast(now, "Diffusion de la saisie arrêtée", false);
            }
            PowerAction::ToggleStats(id) => {
                let Some(index) = self.tab_index(id) else {
                    return;
                };
                let tab = &mut self.tabs[index];
                let Some(ssh) = tab.ssh.clone() else {
                    return;
                };
                // Dropping the panel stops its task and its connection.
                if tab.power.stats.take().is_none() {
                    tab.power.stats = Some(Panel::start(&self.worker, ssh));
                    // It is drawn above the terminal.
                    if tab.panes == Panes::Files {
                        tab.panes = Panes::Both;
                    }
                }
            }
        }
    }

    /// Opens the snippets dialog on the snippet of this id, or on a new one.
    fn open_snippets(&mut self, open: Option<&str>, new: bool) {
        let target = self.snippet_target().map(|index| self.tabs[index].title.clone());
        let changes = self.ssh_power.changes.0.clone();
        let mut manager = Manager::new(self.ssh_power.snippets.to_vec(), open, target, changes);
        if new {
            manager.start_new();
        }
        self.open_dialog(Dialog::SshPower(PowerDialog::Manager(Box::new(manager))));
    }

    /// The tab whose terminal runs the snippets picked without naming one:
    /// the tab in use when it has a terminal, or else the one the other view
    /// shows.
    fn snippet_target(&self) -> Option<usize> {
        let usable = |index: usize| {
            self.tabs
                .get(index)
                .is_some_and(|tab| tab.ssh.is_some() || tab.terminal.is_some())
        };
        if usable(self.active) {
            return Some(self.active);
        }
        let other = if self.views.focus == Side::Left { Side::Right } else { Side::Left };
        self.shown_index(other).filter(|index| usable(*index))
    }

    /// Types a snippet, after asking for the values of its variables.
    fn run_snippet(&mut self, id: &str, tab: Option<u64>, now: f64) {
        let Some(snippet) = self.ssh_power.snippets.iter().find(|snippet| snippet.id == id).cloned() else {
            self.toast(now, "Ce snippet n'existe plus.", true);
            return;
        };
        let index = match tab {
            Some(id) => self.tab_index(id),
            None => self.snippet_target(),
        };
        let Some(index) = index else {
            self.toast(now, format!("Ouvrez d'abord un terminal SSH pour lancer « {} ».", snippet.title()), true);
            return;
        };
        if snippet.variables().is_empty() {
            self.type_command(index, &snippet.text(&[]), snippet.press_enter, Typing::User);
            return;
        }
        let tab = &self.tabs[index];
        let others = if tab.power.broadcast {
            broadcast::receivers(&self.tabs).saturating_sub(1)
        } else {
            0
        };
        let form = VariablesForm::new(snippet, tab.id, tab.title.clone(), others, None);
        self.open_dialog(Dialog::SshPower(PowerDialog::Variables(form)));
    }

    /// Types a command in the terminal of a tab, opening the terminal first
    /// when needed: typed before the shell is up, it simply waits for it.
    fn type_command(&mut self, index: usize, text: &str, enter: bool, typing: Typing) {
        let Some(tab) = self.tabs.get_mut(index) else {
            return;
        };
        if tab.terminal.is_none() {
            let Some(ssh) = &tab.ssh else {
                return;
            };
            tab.terminal = Some(Terminal::open(&self.worker, ssh.clone()));
        }
        broadcast::sync(tab);
        let Some(terminal) = tab.terminal.as_mut() else {
            return;
        };
        // A paste, so that a command of several lines does not run line by
        // line where the shell can wait for Enter.
        let mut typed = vec![Typed::Paste(text.trim_end_matches(['\r', '\n']).to_owned())];
        if enter {
            typed.push(Typed::Bytes(b"\r".to_vec()));
        }
        for item in typed {
            match typing {
                Typing::User => type_in(terminal, item),
                Typing::Startup => {
                    item.apply(terminal);
                }
            }
        }
        if typing == Typing::User {
            terminal.wants_focus = true;
            if !tab.panes.terminal() {
                tab.panes = Panes::Both;
            }
            let id = tab.id;
            self.views.show(&self.tabs, id);
            self.settle();
        }
    }

    /// A dialog of the power tools was confirmed.
    pub(super) fn ssh_power_dialog_done(&mut self, dialog: PowerDialog, now: f64) {
        match dialog {
            PowerDialog::Manager(manager) => {
                self.take_snippet_changes(now);
                if let Some(id) = manager.run {
                    self.run_snippet(&id, None, now);
                }
            }
            PowerDialog::Variables(form) => {
                let Some(index) = self.tab_index(form.tab) else {
                    return;
                };
                let typing = if form.startup.is_some() { Typing::Startup } else { Typing::User };
                self.type_command(index, &form.text(), form.snippet.press_enter, typing);
                if let Some(rest) = form.startup {
                    self.run_startup(index, rest, now);
                }
            }
        }
    }

    /// Once per frame, after everything else: broadcast input goes out, the
    /// shells that just became ready get their startup commands, and the
    /// screens learn what changed.
    pub(super) fn ssh_power_frame(&mut self, ctx: &egui::Context, now: f64) {
        self.take_snippet_changes(now);
        self.save_snippets_when_due(ctx, now);

        broadcast::fan_out(&mut self.tabs);
        for tab in &mut self.tabs {
            broadcast::sync(tab);
        }
        self.send_startup_commands(ctx, now);

        let target = self.snippet_target().map(|index| self.tabs[index].title.clone());
        publish(
            ctx,
            Published {
                snippets: self.ssh_power.snippets.clone(),
                target,
                receivers: broadcast::receivers(&self.tabs),
            },
        );
    }

    fn take_snippet_changes(&mut self, now: f64) {
        let mut latest = None;
        while let Ok(snippets) = self.ssh_power.changes.1.try_recv() {
            latest = Some(snippets);
        }
        if let Some(snippets) = latest {
            self.ssh_power.snippets = Arc::new(snippets);
            self.ssh_power.save_at = Some(now + SAVE_DELAY);
        }
    }

    /// Saves the library a moment after it changed, or as soon as its
    /// dialog closes.
    fn save_snippets_when_due(&mut self, ctx: &egui::Context, now: f64) {
        let Some(at) = self.ssh_power.save_at else {
            return;
        };
        let editing = matches!(self.dialog, Some(Dialog::SshPower(PowerDialog::Manager(_))));
        if editing && now < at {
            ctx.request_repaint_after(Duration::from_secs_f64(at - now));
            return;
        }
        self.ssh_power.save_at = None;
        if let Err(error) = snippets::save(&self.ssh_power.snippets) {
            self.toast(now, format!("Enregistrement des snippets impossible : {error:#}"), true);
        }
    }

    /// Follows every shell, and types the startup commands of its connection
    /// in the ones that just became ready.
    fn send_startup_commands(&mut self, ctx: &egui::Context, now: f64) {
        let mut due = Vec::new();
        for (index, tab) in self.tabs.iter_mut().enumerate() {
            let screen = tab
                .terminal
                .as_ref()
                .filter(|terminal| terminal.status() == Status::Open)
                .map(|terminal| terminal.with_screen(screen_signature));
            match tab.power.startup.check(screen.is_some(), screen.flatten(), now) {
                Readiness::Due => due.push(index),
                Readiness::Waiting => ctx.request_repaint_after(Duration::from_millis(150)),
                Readiness::Idle => {}
            }
        }
        for index in due {
            let profile_id = &self.tabs[index].profile_id;
            let steps = self
                .profiles
                .iter()
                .find(|profile| profile.id == *profile_id)
                .map(|profile| profile.startup.steps())
                .unwrap_or_default();
            self.run_startup(index, steps, now);
        }
    }

    /// Types startup steps in order. A snippet whose variables have no value
    /// asks for them, and the steps after it wait for the answer.
    fn run_startup(&mut self, index: usize, steps: Vec<Step>, now: f64) {
        let mut steps = steps.into_iter();
        while let Some(step) = steps.next() {
            let snippet = match step {
                Step::Command(command) => {
                    self.type_command(index, &command, true, Typing::Startup);
                    continue;
                }
                Step::Snippet(id) => match self.ssh_power.snippets.iter().find(|snippet| snippet.id == id) {
                    Some(snippet) => snippet.clone(),
                    // Deleted since: nothing to type.
                    None => continue,
                },
            };
            if !snippet.needs_input() {
                self.type_command(index, &snippet.text(&[]), snippet.press_enter, Typing::Startup);
                continue;
            }
            if self.dialog.is_some() {
                let name = snippet.title();
                let text = format!("Snippet de démarrage « {name} » ignoré : ses variables sont à saisir.");
                self.toast(now, text, true);
                continue;
            }
            let tab = &self.tabs[index];
            let form = VariablesForm::new(snippet, tab.id, tab.title.clone(), 0, Some(steps.collect()));
            self.open_dialog(Dialog::SshPower(PowerDialog::Variables(form)));
            return;
        }
    }

    /// Script steps of the power tools; see the list in `run_dev_step`.
    pub(super) fn dev_step_ssh_power(&mut self, verb: &str, argument: &str, ctx: &egui::Context, now: f64) -> bool {
        let snippet_named = |app: &Self, name: &str| {
            app.ssh_power
                .snippets
                .iter()
                .find(|snippet| snippet.title() == name)
                .map(|snippet| snippet.id.clone())
        };
        let tab_at = |app: &Self, word: &str| match word.parse::<usize>() {
            Ok(index) => app.tabs.get(index).map(|tab| tab.id),
            Err(_) => app.tabs.get(app.active).map(|tab| tab.id),
        };
        match verb {
            "snippets" => self.open_snippets(None, false),
            "newsnippet" => {
                let (name, command) = argument.split_once('>').unwrap_or((argument, ""));
                let mut snippet = Snippet::new();
                snippet.name = name.trim().to_owned();
                // `\n` in a script stands for a new line.
                snippet.command = command.trim().replace("\\n", "\n");
                let mut list = self.ssh_power.snippets.to_vec();
                list.push(snippet);
                snippets::sort(&mut list);
                self.ssh_power.snippets = Arc::new(list);
                self.ssh_power.save_at = Some(now);
            }
            "snippet" => match snippet_named(self, argument) {
                Some(id) => self.run_snippet(&id, None, now),
                None => self.toast(now, format!("Pas de snippet « {argument} »"), true),
            },
            "snippetvar" => {
                let (name, value) = argument.split_once(' ').unwrap_or((argument, ""));
                if let Some(Dialog::SshPower(PowerDialog::Variables(form))) = &mut self.dialog {
                    form.set(name, value);
                }
            }
            "snippetnew" | "snippetpick" | "snippetfield" | "snippetenter" => {
                let Some(Dialog::SshPower(PowerDialog::Manager(manager))) = &mut self.dialog else {
                    return true;
                };
                match verb {
                    "snippetnew" => manager.start_new(),
                    "snippetpick" => manager.select(argument),
                    "snippetenter" => manager.draft.press_enter = argument != "off",
                    _ => {
                        let (field, value) = argument.split_once(' ').unwrap_or((argument, ""));
                        let value = value.replace("\\n", "\n");
                        match field {
                            "name" => manager.draft.name = value,
                            "command" => manager.draft.command = value,
                            "description" => manager.draft.description = value,
                            _ => {}
                        }
                    }
                }
            }
            "snippetmenu" => {
                if let Some(id) = tab_at(self, argument) {
                    egui::Popup::open_id(ctx, picker_id(id));
                }
            }
            "broadcast" => match argument {
                "all" => self.apply_ssh_power(PowerAction::BroadcastAll, now),
                "off" => self.apply_ssh_power(PowerAction::StopBroadcast, now),
                _ => {
                    if let Some(id) = tab_at(self, argument) {
                        self.apply_ssh_power(PowerAction::ToggleBroadcast(id), now);
                    }
                }
            },
            "stats" => {
                if let Some(id) = tab_at(self, argument) {
                    self.apply_ssh_power(PowerAction::ToggleStats(id), now);
                }
            }
            "startup" | "startupcmd" => {
                let Some(profile_id) = self.tabs.get(self.active).map(|tab| tab.profile_id.clone()) else {
                    return true;
                };
                let id = snippet_named(self, argument);
                if let Some(profile) = self.profiles.iter_mut().find(|profile| profile.id == profile_id) {
                    match (verb, id) {
                        ("startup", Some(id)) => profile.startup.snippets.push(id),
                        ("startupcmd", _) => profile.startup.command = argument.to_owned(),
                        _ => {}
                    }
                }
            }
            "startuppick" => {
                let id = snippet_named(self, argument);
                if let (Some(Dialog::Profile(form)), Some(id)) = (&mut self.dialog, id) {
                    form.draft.startup.snippets.push(id);
                }
            }
            // `hover 300 200` leaves the pointer there, for tooltips.
            "hover" => {
                let numbers: Vec<f32> = argument.split(' ').filter_map(|word| word.parse().ok()).collect();
                if let [x, y] = numbers[..] {
                    self.dev_input.push(egui::Event::PointerMoved(egui::pos2(x, y)));
                }
            }
            "focusterm" => {
                if let Some(terminal) = self.tabs.get_mut(self.active).and_then(|tab| tab.terminal.as_mut()) {
                    terminal.wants_focus = true;
                }
            }
            _ => return false,
        }
        true
    }
}
