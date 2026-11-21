//! The terminals' side of the app: their preferences, session logs, links,
//! and the dialogs and script steps about them.

use std::path::{Path, PathBuf};

use super::App;
use crate::dev;
use crate::terminal::Terminal;
use crate::terminal_ext::log::Recorder;
use crate::terminal_ext::look::{self, CursorShape};
use crate::terminal_ext::search::Search;
use crate::terminal_ext::{self, fonts};
use crate::ui::Action;
use crate::ui::dialogs::Dialog;
use crate::ui::terminal_ext::{self as ext, TerminalAction};
use crate::ui::terminal_look::{LookForm, PasteForm, TerminalDialog};

impl App {
    /// Every frame, before the tabs are drawn (and their shells opened).
    pub(super) fn terminal_frame(&mut self, ctx: &egui::Context, now: f64) {
        look::publish(ctx, &self.settings.terminal_prefs);
        self.record_new_sessions(now);
        let failures: Vec<String> = self
            .tabs
            .iter()
            .filter_map(|tab| tab.terminal.as_ref())
            .filter_map(|terminal| terminal.extras.tap.take_failure())
            .collect();
        for failure in failures {
            self.toast(now, failure, true);
        }
    }

    fn terminal_mut(&mut self, id: u64) -> Option<&mut Terminal> {
        self.tabs
            .iter_mut()
            .filter_map(|tab| tab.terminal.as_mut())
            .find(|terminal| terminal.extras.id == id)
    }

    fn active_terminal(&mut self) -> Option<&mut Terminal> {
        self.tabs.get_mut(self.active).and_then(|tab| tab.terminal.as_mut())
    }

    pub(super) fn terminal_action(&mut self, action: TerminalAction, ctx: &egui::Context, now: f64) {
        match action {
            TerminalAction::Zoom(steps) => {
                let prefs = &mut self.settings.terminal_prefs;
                prefs.font_size = prefs.zoomed(steps);
                ext::show_zoom(ctx, prefs.font_size);
                look::publish(ctx, prefs);
                self.save_settings();
            }
            TerminalAction::OpenLook => {
                if self.dialog.is_none() {
                    let form = LookForm::new(&self.settings.terminal_prefs);
                    self.open_dialog(Dialog::Terminal(TerminalDialog::Look(Box::new(form))));
                }
            }
            TerminalAction::AskPaste { terminal, text } => {
                let confirm = self.settings.terminal_prefs.confirm_paste && ext::is_multiline(&text);
                if confirm && self.dialog.is_none() {
                    self.open_dialog(Dialog::Terminal(TerminalDialog::Paste(PasteForm {
                        terminal,
                        text,
                        dont_ask: false,
                    })));
                } else if !confirm && let Some(terminal) = self.terminal_mut(terminal) {
                    terminal.paste(&text);
                }
            }
            TerminalAction::StartLog { terminal } => self.start_log(terminal, true, now),
            TerminalAction::StopLog { terminal } => self.stop_log(terminal, now),
            TerminalAction::OpenLink(url) => self.open_link(&url, now),
            TerminalAction::Notice(text) => self.toast(now, text, false),
            TerminalAction::Search { tab } => {
                let Some(index) = self.tab_index(tab) else {
                    return;
                };
                self.apply(Action::SelectTab(index), ctx, now);
                if let Some(terminal) = self.tabs[index].terminal.as_mut() {
                    ext::open_search(terminal);
                }
            }
        }
    }

    /// A dialog of the terminals was confirmed.
    pub(super) fn terminal_dialog_done(&mut self, dialog: TerminalDialog, ctx: &egui::Context) {
        match dialog {
            TerminalDialog::Look(form) => {
                self.settings.terminal_prefs = form.draft;
                look::publish(ctx, &self.settings.terminal_prefs);
                self.save_settings();
            }
            TerminalDialog::Paste(form) => {
                if form.dont_ask {
                    self.settings.terminal_prefs.confirm_paste = false;
                    self.save_settings();
                }
                if let Some(terminal) = self.terminal_mut(form.terminal) {
                    terminal.paste(&form.text);
                    terminal.wants_focus = true;
                }
            }
        }
    }

    /// Only web addresses are ever found in terminals; a script opens
    /// nothing on the desktop and says what it would have opened.
    fn open_link(&mut self, url: &str, now: f64) {
        let lower = url.to_ascii_lowercase();
        if !(lower.starts_with("https://") || lower.starts_with("http://")) {
            return;
        }
        if dev::active() {
            self.toast(now, format!("Lien à ouvrir : {url}"), false);
            return;
        }
        if let Err(error) = open::that_detached(url) {
            self.toast(now, format!("Ouverture du lien impossible : {error}"), true);
        }
    }

    /// Where session logs go: the folder of the preferences, or a folder
    /// in the documents. While a script drives the app, a dev folder.
    fn log_folder(&self) -> PathBuf {
        if dev::active() {
            return dev::download_dir()
                .unwrap_or_else(|| std::env::temp_dir().join("s3viewer-dev"))
                .join("journaux");
        }
        let chosen = self.settings.terminal_prefs.log_folder.trim();
        if !chosen.is_empty() {
            return PathBuf::from(chosen);
        }
        directories::UserDirs::new()
            .and_then(|dirs| dirs.document_dir().map(Path::to_path_buf))
            .unwrap_or_else(std::env::temp_dir)
            .join(crate::APP_NAME)
            .join("Journaux")
    }

    /// Starts recording a terminal. `ask` lets the user choose the file;
    /// otherwise (sessions recorded automatically, scripts) it goes to the
    /// log folder under a name made of the address and the time.
    fn start_log(&mut self, id: u64, ask: bool, now: f64) {
        let raw = self.settings.terminal_prefs.log_raw;
        let folder = self.log_folder();
        let Some(address) = self.terminal_mut(id).map(|terminal| terminal.address.clone()) else {
            return;
        };
        let name = terminal_ext::log_file_name(&address);
        let path = if ask && !dev::active() {
            // The folder is only made once something is recorded in it.
            let start = if folder.is_dir() {
                folder
            } else {
                directories::UserDirs::new()
                    .and_then(|dirs| dirs.document_dir().map(Path::to_path_buf))
                    .unwrap_or(folder)
            };
            let picked = rfd::FileDialog::new()
                .set_title("Enregistrer la session")
                .set_directory(&start)
                .set_file_name(&name)
                .add_filter("Journal de session", &["log", "txt"])
                .save_file();
            match picked {
                Some(path) => path,
                None => return,
            }
        } else {
            // Two tabs of one connection may start in the same second.
            terminal_ext::unused_path(&folder.join(&name))
        };
        let started = Recorder::create(&path, raw, &terminal_ext::log_header(&address)).and_then(|recorder| {
            match self.terminal_mut(id) {
                Some(terminal) => terminal.extras.tap.start(recorder),
                None => Ok(()),
            }
        });
        let file = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
        match started {
            Ok(()) if ask => self.toast(now, format!("Enregistrement de la session dans « {file} »"), false),
            Ok(()) => {}
            Err(error) => self.toast(now, format!("Enregistrement de la session impossible : {error:#}"), true),
        }
    }

    fn stop_log(&mut self, id: u64, now: f64) {
        let Some(stopped) = self.terminal_mut(id).and_then(|terminal| terminal.extras.tap.stop()) else {
            return;
        };
        match stopped {
            Ok(path) => self.toast(now, format!("Session enregistrée dans {}", path.display()), false),
            Err(error) => self.toast(now, format!("Le journal n'a pas pu être terminé : {error:#}"), true),
        }
    }

    /// Terminals of connections that record every session start recording
    /// as they open.
    fn record_new_sessions(&mut self, now: f64) {
        let mut wanted = Vec::new();
        for tab in &mut self.tabs {
            let Some(terminal) = &mut tab.terminal else {
                continue;
            };
            if std::mem::replace(&mut terminal.extras.auto_log_checked, true) {
                continue;
            }
            let id = &tab.profile_id;
            if self.profiles.iter().any(|profile| profile.id == *id && profile.log_sessions) {
                wanted.push(terminal.extras.id);
            }
        }
        for id in wanted {
            self.start_log(id, false, now);
        }
    }

    /// Script steps about terminals, on the terminal of the tab shown:
    ///
    /// - search: `termfind 29` opens the bar on that text, `termfindnext`
    ///   (as Enter: up), `termfindprev` (as Shift+Enter: down),
    ///   `termfindcase`, `termfindclose`;
    /// - appearance: `termscheme braise`, `termfont Cascadia Mono` (nothing
    ///   for the default, `Hack` for the built-in one), `termfontsize 16`,
    ///   `termzoom in` (`out`, `reset`), `termcursor bar blink`, `termrightpaste on`;
    ///   `termlook` opens the dialog, `termlookscheme fjord`,
    ///   `termlookfont Consolas`, `termlookcursor underline` change it there,
    ///   `termlookpage 2` turns to its second page (`confirm` saves it);
    /// - links: `termctrl 300 200` moves the pointer there as if Ctrl were
    ///   held (then `click 300 200` opens the link), `termctrl off`;
    ///   `termwheel 600 400 in` (or `out`) turns the wheel with Ctrl held;
    /// - logs: `termlog` (`termlog raw`) records into the dev folder,
    ///   `termlogstop`, `termautolog` makes the connection of the tab
    ///   record each session (from its next terminal);
    /// - `termpaste one\ntwo` pastes as the clipboard would; `termselectall`,
    ///   `termcopy`, `termclear` (the history), `termclearscreen`;
    /// - `termkey ctrl+shift+f` presses keys as if the window had the
    ///   keyboard (`termkey ctrl+=`, `termkey ctrl+0`…), `termfonts` lists
    ///   the fixed-width fonts found in a message.
    pub(super) fn dev_step_terminal(&mut self, verb: &str, argument: &str, ctx: &egui::Context, now: f64) -> bool {
        let prefs = &mut self.settings.terminal_prefs;
        match verb {
            "termscheme" => prefs.scheme = argument.to_owned(),
            "termfont" => prefs.font = argument.to_owned(),
            "termfontsize" => prefs.font_size = argument.parse().unwrap_or(look::DEFAULT_FONT_SIZE),
            "termzoom" => {
                let steps = match argument {
                    "in" => 1,
                    "out" => -1,
                    _ => 0,
                };
                self.terminal_action(TerminalAction::Zoom(steps), ctx, now);
            }
            "termcursor" => {
                let mut words = argument.split(' ');
                prefs.cursor = cursor_shape(words.next().unwrap_or(""));
                prefs.cursor_blink = words.next() == Some("blink");
            }
            "termrightpaste" => prefs.right_click_paste = argument == "on",
            "termlook" => self.terminal_action(TerminalAction::OpenLook, ctx, now),
            "termlookscheme" | "termlookfont" | "termlookcursor" | "termlookpage" => {
                if let Some(Dialog::Terminal(TerminalDialog::Look(form))) = &mut self.dialog {
                    match verb {
                        "termlookscheme" => form.draft.scheme = argument.to_owned(),
                        "termlookfont" => form.draft.font = argument.to_owned(),
                        "termlookpage" => form.show_behaviour(argument == "2"),
                        _ => form.draft.cursor = cursor_shape(argument),
                    }
                }
            }
            // `termwheel 600 400 in` turns the wheel there with Ctrl held.
            "termwheel" => {
                let words: Vec<&str> = argument.split(' ').filter(|word| !word.is_empty()).collect();
                if let [x, y, way] = words[..]
                    && let (Ok(x), Ok(y)) = (x.parse(), y.parse())
                {
                    let pos = egui::pos2(x, y);
                    self.dev_input.push(egui::Event::PointerMoved(pos));
                    self.dev_input.push(egui::Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Line,
                        delta: egui::vec2(0.0, if way == "out" { -1.0 } else { 1.0 }),
                        phase: egui::TouchPhase::Move,
                        modifiers: egui::Modifiers::CTRL,
                    });
                }
            }
            "termctrl" => {
                let held = argument != "off";
                ctx.data_mut(|data| data.insert_temp(ext::dev_ctrl_id(), held));
                let numbers: Vec<f32> = argument.split(' ').filter_map(|n| n.parse().ok()).collect();
                if let [x, y] = numbers[..] {
                    self.dev_input.push(egui::Event::PointerMoved(egui::pos2(x, y)));
                }
                ctx.request_repaint();
            }
            "termautolog" => {
                let id = self.tabs.get(self.active).map(|tab| tab.profile_id.clone());
                if let Some(profile) = self.profiles.iter_mut().find(|profile| Some(&profile.id) == id.as_ref()) {
                    profile.log_sessions = true;
                }
            }
            "termlog" | "termlogstop" => {
                let raw = argument == "raw";
                let Some(id) = self.active_terminal().map(|terminal| terminal.extras.id) else {
                    return true;
                };
                if verb == "termlog" {
                    self.settings.terminal_prefs.log_raw = raw;
                    self.start_log(id, false, now);
                } else {
                    self.stop_log(id, now);
                }
            }
            "termpaste" => {
                let text = argument.replace("\\n", "\n");
                let prefs = self.settings.terminal_prefs.clone();
                let mut actions = Vec::new();
                if let Some(terminal) = self.active_terminal() {
                    ext::paste(terminal, &prefs, text, &mut actions);
                }
                for action in actions {
                    self.apply(action, ctx, now);
                }
            }
            "termfind" | "termfindnext" | "termfindprev" | "termfindcase" | "termfindclose" | "termselectall"
            | "termcopy" | "termclear" | "termclearscreen" => {
                let Some(terminal) = self.active_terminal() else {
                    return true;
                };
                match verb {
                    "termfind" => {
                        let mut search = Search::new();
                        search.query = argument.to_owned();
                        terminal.extras.search = Some(search);
                    }
                    "termselectall" => terminal.extras.all_selected = true,
                    "termcopy" => {
                        let text = if terminal.extras.all_selected {
                            Some(ext::all_text(terminal))
                        } else {
                            terminal.selected_text()
                        };
                        if let Some(text) = text {
                            ctx.copy_text(text);
                        }
                    }
                    "termclear" => ext::clear_history(terminal),
                    "termclearscreen" => terminal.send([0x0c]),
                    _ => {
                        if verb == "termfindclose" {
                            terminal.extras.search = None;
                        } else if let Some(search) = &mut terminal.extras.search {
                            match verb {
                                "termfindnext" => search.older(),
                                "termfindprev" => search.newer(),
                                _ => search.case_sensitive = !search.case_sensitive,
                            }
                        }
                    }
                }
            }
            // The off-screen window never has the keyboard, so `key` cannot
            // reach the terminal: this presses the keys as if it had.
            "termkey" => {
                let mut modifiers = egui::Modifiers::NONE;
                let mut key = None;
                for part in argument.split('+').map(str::trim).filter(|part| !part.is_empty()) {
                    match part.to_ascii_lowercase().as_str() {
                        "ctrl" => modifiers |= egui::Modifiers::CTRL,
                        "shift" => modifiers |= egui::Modifiers::SHIFT,
                        "alt" => modifiers |= egui::Modifiers::ALT,
                        "enter" => key = Some(egui::Key::Enter),
                        "escape" => key = Some(egui::Key::Escape),
                        name => key = egui::Key::from_name(&name.to_ascii_uppercase()).or(egui::Key::from_name(part)),
                    }
                }
                if let Some(key) = key {
                    ctx.input_mut(|input| {
                        input.focused = true;
                        input.modifiers = modifiers;
                        input.events.push(egui::Event::Key {
                            key,
                            physical_key: None,
                            pressed: true,
                            repeat: false,
                            modifiers,
                        });
                    });
                }
            }
            "termfonts" => {
                // Lists the fixed-width fonts found, for the record of a run.
                let names: Vec<&str> = fonts::catalog_now().iter().map(|family| family.name.as_str()).collect();
                self.toast(now, format!("Polices : {}", names.join(", ")), false);
            }
            _ => return false,
        }
        ctx.request_repaint();
        true
    }
}

fn cursor_shape(name: &str) -> CursorShape {
    match name {
        "bar" => CursorShape::Bar,
        "underline" => CursorShape::Underline,
        _ => CursorShape::Block,
    }
}
