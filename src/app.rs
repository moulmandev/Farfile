//! Application state and the glue between the screens and the worker.

mod ssh_auth;
mod hosts;
mod terminal_ext;
mod ssh_power;
mod tunnels;
mod s3;
mod viewers;
mod local;
mod editor;
mod update;
mod monitor;

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use anyhow::Result;
use egui::{
    Align, Align2, CentralPanel, Frame, Id, Key, KeyboardShortcut, Layout, Margin, Modifiers, Order,
    Panel, RichText, Stroke,
};

use crate::dev;
use crate::edits::{self, Edits, State as EditState};
use crate::format;
use crate::import::{self, Locations};
use crate::launch::{self, Launch};
use crate::os_drag;
use crate::profiles::{self, Panes, Profile, Protocol, Settings};
use crate::session::Session;
use crate::storage::RemotePath;
use crate::tabs::{Closing, Side, Tab, TabPlace, Views, is_split, place as place_tab, shell_quote, to_close};
use crate::terminal::Terminal;
use crate::theme::{self, Palette};
use crate::ui::dialogs::{self, Dialog, FolderForm, ImportForm, Outcome, ProfileForm};
use crate::ui::quick_open::{self, QuickOpen};
use crate::ui::transfers::Meter;
use crate::ui::widgets::{icon, primary_button, protocol_icon, small};
use crate::ui::{Action, browser, sidebar, tabs, terminal, transfers};
use crate::worker::{Event, Item, Transfer, TransferState, Worker, safe_component};

mod file_ops;
mod transfer;

/// Uploads finishing in a burst trigger a single refresh after this delay.
const REFRESH_DELAY: f64 = 0.4;
/// Opening a file downloads all of it: above this size, ask first.
const LARGE_FILE: u64 = 64 * 1024 * 1024;
/// How many closed tabs can be brought back.
const CLOSED_TABS: usize = 20;
/// Neither of two views side by side gets narrower than this.
const MIN_VIEW_WIDTH: f32 = 300.0;

/// What it takes to bring a closed tab back.
struct ClosedTab {
    profile_id: String,
    title: String,
    /// The folder it showed, when it had got that far.
    path: Option<RemotePath>,
    panes: Panes,
    /// A file of the built-in editor: it is read again (see `app::editor`).
    file: Option<editor::Source>,
}

impl ClosedTab {
    fn of(tab: &Tab) -> Self {
        Self {
            profile_id: tab.profile_id.clone(),
            title: tab.title.clone(),
            path: tab.session.known_path(),
            panes: tab.panes,
            file: editor::Source::of_tab(tab),
        }
    }
}

fn is_dev(profile_id: &str) -> bool {
    profile_id.starts_with(dev::PROFILE_PREFIX)
}

struct Toast {
    text: String,
    error: bool,
    expires: f64,
}

pub struct App {
    palette: Palette,
    settings: Settings,
    profiles: Vec<Profile>,
    /// Secrets of the throwaway connections of [`crate::dev`].
    dev_secrets: HashMap<String, String>,

    /// The search field of the sidebar, and the like.
    sidebar: sidebar::State,
    /// The palette that opens a connection by typing, while it shows.
    quick_open: Option<QuickOpen>,

    tabs: Vec<Tab>,
    /// What each of the two views shows, and which one is in use.
    views: Views,
    /// Index of the tab shown by the view in use: the one the keyboard, the
    /// menus and the status bar are about. Kept by [`App::settle`].
    active: usize,
    next_tab_id: u64,
    /// The tabs closed last, the most recent at the end.
    closed: Vec<ClosedTab>,

    worker: Worker,
    events: Receiver<Event>,
    /// Bumped on every listing request, so each one has its own number.
    generation: u64,

    transfers: Vec<Arc<Transfer>>,
    meters: HashMap<u64, Meter>,
    show_transfers: bool,
    /// Questions about existing files, and folder synchronisations.
    transfer: transfer::State,

    /// The remote files opened in a local program.
    edits: Edits,
    /// Edited files saved since, about which the user is still to be asked.
    edit_prompts: VecDeque<u64>,
    /// Scripts only: unfold the list of edited files on the next frame.
    open_edits: bool,
    /// The user chose to leave although some changes were never sent.
    quitting: bool,

    dialog: Option<Dialog>,
    dialog_just_opened: bool,
    toasts: Vec<Toast>,
    last_download_dir: Option<PathBuf>,

    /// Remaining steps of the `S3V_DEV_SCRIPT`, and when to play the next.
    dev_script: VecDeque<String>,
    dev_next_step: f64,
    /// Set by the script to answer the open dialog on the next frame.
    dev_answer: Option<bool>,
    /// Pointer events from the script, delivered with the next frame's input.
    dev_input: Vec<egui::Event>,
    /// A file the script holds over the window, or drops on it.
    dev_files: Option<DevFiles>,
    /// The left button was released during a drag to another program,
    /// which kept the release to itself: egui is to be told.
    release_pointer: bool,

    /// The keychain, passphrase questions, keys being made or installed.
    ssh_auth: ssh_auth::State,
    /// Host colours and tags, export and import, tabs kept for next time.
    hosts: hosts::State,
    /// Snippets, broadcast input and server overview: see `app::ssh_power`.
    ssh_power: ssh_power::PowerState,
    /// SSH port forwarding: the tunnels running, and what they follow.
    tunnels: tunnels::State,
    /// Clipboard, long operations and folder sizes of the file operations.
    file_ops: crate::file_ops::OpsState,
    /// The built-in editor: files being looked at before opening, leaving
    /// once saved, measures for scripts.
    editor: editor::EditorState,

    /// Looks for, downloads and installs newer releases.
    updater: crate::update::Updater,
    /// The watched servers of the dashboard, their alerts, and the icon of
    /// the notification area: see `app::monitor`.
    monitor: monitor::MonitorState,
}

/// A file from another program, as a script plays it.
struct DevFiles {
    pointer: egui::Pos2,
    path: PathBuf,
    /// Dropped rather than only held over the window.
    drop: bool,
    /// The drop went in with the input of a frame.
    delivered: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let settings = profiles::load_settings();
        crate::i18n::set(dev::language().unwrap_or_else(|| crate::i18n::choose(settings.language.as_deref())));
        let palette = Palette::for_mode(settings.dark);
        theme::install_fonts(&cc.egui_ctx);
        theme::apply(&cc.egui_ctx, &palette);

        let (worker, events) = Worker::new(cc.egui_ctx.clone());
        let monitor = monitor::MonitorState::new(&cc.egui_ctx, &settings.monitor);
        let mut app = Self {
            palette,
            settings,
            // A script may bring its own list, shown in place of the saved one.
            profiles: dev::profiles().unwrap_or_else(profiles::load),
            dev_secrets: HashMap::new(),
            sidebar: sidebar::State::default(),
            quick_open: None,
            tabs: Vec::new(),
            views: Views::default(),
            active: 0,
            next_tab_id: 1,
            closed: Vec::new(),
            worker,
            events,
            generation: 0,
            transfers: Vec::new(),
            meters: HashMap::new(),
            show_transfers: false,
            transfer: transfer::State::default(),
            edits: Edits::new(),
            edit_prompts: VecDeque::new(),
            open_edits: false,
            quitting: false,
            dialog: None,
            dialog_just_opened: false,
            toasts: Vec::new(),
            last_download_dir: None,
            dev_script: dev::script(),
            dev_next_step: 1.5,
            dev_answer: None,
            dev_input: Vec::new(),
            dev_files: None,
            release_pointer: false,
            ssh_auth: ssh_auth::State::new(&cc.egui_ctx),
            hosts: hosts::State::default(),
            ssh_power: ssh_power::PowerState::load(),
            tunnels: tunnels::State::default(),
            file_ops: Default::default(),
            editor: Default::default(),
            updater: crate::update::Updater::new(),
            monitor,
        };
        app.normalize_folders();
        app.monitor_settings_for_scripts();
        app.open_dev_profiles();
        app.restore_tabs();
        app
    }

    /// Folders are compared by their path: bring what was saved, or typed
    /// by hand in the files, to the one way of writing it.
    fn normalize_folders(&mut self) {
        for profile in &mut self.profiles {
            profile.folder = profiles::folder_path(&profile.folder);
        }
        let settings = &mut self.settings;
        for folder in settings.folders.iter_mut().chain(&mut settings.collapsed) {
            *folder = profiles::folder_path(folder);
        }
    }

    /// Adds the connections described by `S3V_DEV_CONNECTIONS`, if any,
    /// without saving them, and opens the first. See [`crate::dev`].
    fn open_dev_profiles(&mut self) {
        let connections = dev::connections();
        let first = connections.first().map(|(profile, _)| profile.id.clone());
        for (index, (profile, secret)) in connections.into_iter().enumerate() {
            self.dev_secrets.insert(profile.id.clone(), secret);
            self.profiles.insert(index, profile);
        }
        if let Some(id) = first {
            self.open_tab(&id, None, 0.0);
        }
    }

    /// Plays the next step of the dev script once its time has come.
    fn play_dev_script(&mut self, ctx: &egui::Context, now: f64) {
        if self.dev_script.is_empty() {
            return;
        }
        if now < self.dev_next_step {
            ctx.request_repaint_after(Duration::from_secs_f64(self.dev_next_step - now));
            return;
        }
        if let Some(step) = self.dev_script.pop_front() {
            self.run_dev_step(&step, ctx, now);
        }
        self.dev_next_step = now + dev::STEP_DELAY;
        ctx.request_repaint();
    }

    /// One scripted step: either an [`Action`] or a direct change of what
    /// the user would otherwise type or click.
    fn run_dev_step(&mut self, step: &str, ctx: &egui::Context, now: f64) {
        let (verb, argument) = step.split_once(' ').unwrap_or((step, ""));
        let argument = argument.trim();
        let dev_profile = |number: &str| format!("{}{}", dev::PROFILE_PREFIX, number.trim());
        let panes = |name: &str| match name.trim() {
            "terminal" => Some(Panes::Terminal),
            "both" => Some(Panes::Both),
            "files" => Some(Panes::Files),
            _ => None,
        };
        let action = match verb {
            "open" => Action::Open(argument.to_owned()),
            "goto" => Action::Navigate(RemotePath::parse(argument)),
            "back" => Action::Back,
            "up" => Action::Up,
            "refresh" => Action::Refresh,
            "download" => Action::DownloadSelection,
            "upload" => Action::UploadFolder,
            "delete" => Action::AskDeleteSelection,
            "newfolder" => Action::AskNewFolder,
            "share" => Action::ShareLink(argument.to_owned()),
            "copyurl" => Action::CopyUrl(argument.to_owned()),
            "theme" => Action::ToggleTheme,
            "checkupdates" => Action::Update(crate::ui::update::UpdateAction::Check),
            // `lang en`, or `lang` alone for the system's.
            "lang" => Action::SetLanguage(crate::i18n::Lang::from_code(argument)),
            "transfers" => Action::ToggleTransfers,
            "clear" => Action::ClearFinishedTransfers,
            // Tabs: `connect 1`, `newtab 1 terminal`, `tab 0`, `closetab`.
            "connect" => Action::Connect(dev_profile(argument)),
            "newtab" => {
                let (number, wanted) = argument.split_once(' ').unwrap_or((argument, ""));
                Action::OpenTab(dev_profile(number), panes(wanted))
            }
            "tab" => Action::SelectTab(argument.parse().unwrap_or(0)),
            "closetab" => Action::CloseTab(self.active),
            // Tabs are named by their index, from 0: `closetabs 2 others`
            // (or `left`, `right`, `all`), `duplicate 1`, `renametab 1`,
            // `reconnecttab 1`, `reopen`, `join`, and
            // `movetab 3 right` (the view), `left`, `first`, `last`,
            // `before 0`, `after 0`.
            "closetabs" | "duplicate" | "renametab" | "reconnecttab" | "movetab" | "copyaddress" => {
                let mut words = argument.split(' ').filter(|word| !word.is_empty());
                let id_at = |word: Option<&str>| {
                    word.and_then(|word| word.parse::<usize>().ok())
                        .and_then(|index| self.tabs.get(index))
                        .map(|tab| tab.id)
                };
                let Some(id) = id_at(words.next()) else {
                    return;
                };
                let how = words.next().unwrap_or("");
                match verb {
                    "closetabs" => Action::CloseTabs(
                        id,
                        match how {
                            "others" => Closing::Others,
                            "left" => Closing::Left,
                            "right" => Closing::Right,
                            "all" => Closing::All,
                            _ => Closing::Only,
                        },
                    ),
                    "duplicate" => Action::DuplicateTab(id),
                    "renametab" => Action::AskRenameTab(id),
                    "reconnecttab" => Action::ReconnectTab(id),
                    "copyaddress" => Action::CopyTabAddress(id),
                    _ => {
                        let place = match how {
                            "right" => TabPlace::View(Side::Right),
                            "left" => TabPlace::View(Side::Left),
                            "first" => TabPlace::First,
                            "last" => TabPlace::Last,
                            "before" | "after" => match id_at(words.next()) {
                                Some(target) if how == "before" => TabPlace::Before(target),
                                Some(target) => TabPlace::After(target),
                                None => return,
                            },
                            _ => return,
                        };
                        Action::MoveTab(id, place)
                    }
                }
            }
            "reopen" => Action::ReopenTab,
            "join" => Action::JoinViews,
            "panes" => Action::SetPanes(panes(argument).unwrap_or_default()),
            "here" => Action::TerminalHere,
            "import" => Action::AskImport,
            // `openprofile My server` opens the connection of that name.
            "openprofile" => match self.profiles.iter().find(|p| p.title() == argument) {
                Some(profile) => Action::Connect(profile.id.clone()),
                None => return,
            },
            "reconnect" => Action::ReconnectTerminal,
            // Files opened in a program: `openfile notes.txt` (or
            // `openfile notes.txt > editor`), `localedit notes.txt > text`
            // saves the local copy as an editor would, `edits` unfolds
            // their list, `pushedit`, `reopenedit` and `forgetedit` take a
            // name, `always` ticks the box of the question, `askquit`.
            "openfile" => {
                let (name, how) = argument.split_once('>').unwrap_or((argument, ""));
                let how = match how.trim() {
                    "editor" => Launch::Editor,
                    "choose" => Launch::Choose,
                    _ => Launch::Default,
                };
                Action::OpenFile(name.trim().to_owned(), how)
            }
            "localedit" => {
                let (name, text) = argument.split_once('>').unwrap_or((argument, "modifié"));
                if let Some(edit) = self.edits.list().iter().find(|edit| edit.name == name.trim()) {
                    use std::io::Write;
                    if let Ok(mut file) = std::fs::OpenOptions::new().append(true).open(&edit.local) {
                        let _ = writeln!(file, "{}", text.trim());
                    }
                }
                return;
            }
            "edits" => return self.open_edits = true,
            "pushedit" | "reopenedit" | "forgetedit" => {
                match self.edits.list().iter().find(|edit| edit.name == argument) {
                    Some(edit) if verb == "pushedit" => Action::PushEdit(edit.id),
                    Some(edit) if verb == "reopenedit" => Action::ReopenEdit(edit.id),
                    Some(edit) => Action::ForgetEdit(edit.id),
                    None => return,
                }
            }
            "always" => {
                if let Some(Dialog::PushEdit { always, .. }) = &mut self.dialog {
                    *always = true;
                }
                return;
            }
            "askquit" => {
                let unsent = self.edits.unsent();
                return self.open_dialog(Dialog::Quit { unsent });
            }
            // The list of connections: `sidebar` folds it, `search web`
            // types in its field, `favorite Name`, `moveto Name > Folder`,
            // `newgroup Folder`, `renamegroup Folder`, `deletegroup Folder`,
            // `collapse Folder`, `collapsefav`, `menu Name`, `editprofile Name`.
            "sidebar" => Action::ToggleSidebar,
            "sidebarwidth" => {
                self.settings.sidebar_width = argument.parse().unwrap_or(sidebar::DEFAULT_WIDTH);
                ctx.data_mut(|data| data.remove::<egui::PanelState>(Id::new("sidebar")));
                return;
            }
            "search" => return self.sidebar.filter = argument.to_owned(),
            "favorite" => match self.profile_named(argument) {
                Some(profile) => Action::SetFavorite(profile.id.clone(), !profile.favorite),
                None => return,
            },
            "moveto" => {
                let (name, folder) = argument.split_once('>').unwrap_or((argument, ""));
                match self.profile_named(name.trim()) {
                    Some(profile) => Action::MoveToFolder(profile.id.clone(), folder.trim().to_owned()),
                    None => return,
                }
            }
            "newgroup" => {
                self.remember_folder(argument);
                return;
            }
            // `movegroup Web > Production` puts a folder inside another,
            // `movegroup Production / Web >` brings it back to the first
            // level; `newgroup Production / Web / Old` makes every level.
            "movegroup" => {
                let (folder, parent) = argument.split_once('>').unwrap_or((argument, ""));
                Action::MoveHostFolder(profiles::folder_path(folder), profiles::folder_path(parent))
            }
            "renamegroup" => Action::AskRenameHostFolder(argument.to_owned()),
            "deletegroup" => Action::AskDeleteHostFolder(argument.to_owned()),
            "collapse" => Action::ToggleHostFolder(argument.to_owned()),
            "collapsefav" => Action::ToggleFavorites,
            "menu" => return self.sidebar.open_menu = self.profile_named(argument).map(|p| p.id.clone()),
            "editprofile" => match self.profile_named(argument) {
                Some(profile) => Action::EditProfile(profile.id.clone()),
                None => return,
            },
            // The palette: `quickopen fac` opens it on that text, `down`
            // and `pickup` move the highlight, `confirm` opens the result.
            "quickopen" => return self.quick_open = Some(QuickOpen::with_query(argument)),
            "down" | "pickup" => {
                let count = self
                    .quick_open
                    .as_ref()
                    .map_or(0, |palette| quick_open::ranked(&self.profiles, &palette.query).len());
                if let Some(palette) = &mut self.quick_open {
                    palette.step(if verb == "down" { 1 } else { -1 }, count);
                }
                return;
            }
            "confirm" if self.quick_open.is_some() => {
                let palette = self.quick_open.take().unwrap_or_default();
                match quick_open::ranked(&self.profiles, &palette.query).get(palette.selected) {
                    Some(profile) => Action::Connect(profile.id.clone()),
                    None => return,
                }
            }
            "cancel" if self.quick_open.is_some() => return self.quick_open = None,
            "confirm" => return self.dev_answer = Some(true),
            "cancel" => return self.dev_answer = Some(false),
            // `newprofile sftp` opens the form on that protocol.
            "newprofile" => {
                let mut draft = Profile::new();
                draft.protocol = match argument {
                    "sftp" => Protocol::Sftp,
                    "ftp" => Protocol::Ftp,
                    _ => Protocol::S3,
                };
                return self.edit_profile(draft, true);
            }
            // `key ctrl+k`, `key down`, `key enter` press a key and `text abc`
            // types, through the same events a keyboard produces: this is
            // how the shortcuts themselves are exercised.
            "key" => {
                let mut modifiers = Modifiers::NONE;
                let mut key = None;
                for part in argument.split('+').map(str::trim) {
                    match part.to_ascii_lowercase().as_str() {
                        "ctrl" => modifiers |= Modifiers::CTRL,
                        "shift" => modifiers |= Modifiers::SHIFT,
                        "alt" => modifiers |= Modifiers::ALT,
                        "down" => key = Some(Key::ArrowDown),
                        "up" => key = Some(Key::ArrowUp),
                        "enter" => key = Some(Key::Enter),
                        "escape" => key = Some(Key::Escape),
                        name => key = Key::from_name(&name.to_ascii_uppercase()).or(Key::from_name(part)),
                    }
                }
                if let Some(key) = key {
                    ctx.input_mut(|input| {
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
                return;
            }
            "text" => {
                ctx.input_mut(|input| {
                    input.modifiers = Modifiers::NONE;
                    input.events.push(egui::Event::Text(argument.to_owned()));
                });
                return;
            }
            // `refile` ticks the option of the import that files the saved
            // connections in the folders of the tool they came from.
            "refile" => {
                if let Some(Dialog::Import(form)) = &mut self.dialog {
                    form.apply_folders = true;
                }
                return;
            }
            // `protocol ftp` switches the open form, as its buttons do.
            "protocol" => {
                if let Some(Dialog::Profile(form)) = &mut self.dialog {
                    form.draft.protocol = match argument {
                        "sftp" => Protocol::Sftp,
                        "ftp" => Protocol::Ftp,
                        _ => Protocol::S3,
                    };
                }
                return;
            }
            "type" => {
                match &mut self.dialog {
                    Some(Dialog::NewFolder { name } | Dialog::RenameTab { name, .. }) => {
                        *name = argument.to_owned();
                    }
                    Some(Dialog::Folder(form)) => form.name = argument.to_owned(),
                    _ => {}
                }
                return;
            }
            // Terminal: `send ls -la` types a line, `raw q` types as is,
            // `ctrl c` sends the control character, `scroll 5` goes back.
            "send" | "raw" | "ctrl" | "scroll" | "mark" => {
                let terminal = self.tabs.get_mut(self.active).and_then(|tab| tab.terminal.as_mut());
                if let Some(terminal) = terminal {
                    match verb {
                        "send" => terminal.send(format!("{argument}\r")),
                        "raw" => terminal.send(argument),
                        "ctrl" => {
                            let letter = argument.bytes().next().unwrap_or(b'c').to_ascii_uppercase();
                            terminal.send([letter & 0x1f]);
                        }
                        "scroll" => terminal.scroll(argument.parse().unwrap_or(0)),
                        // `mark 2 0 4 20` selects from row 2, column 0 to
                        // row 4, column 20, as a drag would.
                        _ => {
                            let cells: Vec<u16> =
                                argument.split(' ').filter_map(|n| n.parse().ok()).collect();
                            if let [r1, c1, r2, c2] = cells[..] {
                                terminal.selection = Some(((r1, c1), (r2, c2)));
                            }
                        }
                    }
                }
                return;
            }
            // `dropinto C:older` hands the selection to Explorer as if it
            // had been dragged there.
            #[cfg(windows)]
            "dropinto" => {
                let items = self.selected_items();
                if let Some(session) = self.session() {
                    let storage = session.storage.clone();
                    let folder = PathBuf::from(argument);
                    if let Err(error) = os_drag::drop_into(&folder, self.worker.fetcher(), storage, items) {
                        let error = format!("{error:#}");
                        self.toast(now, tr!("Dépôt impossible : {error}", error), true);
                    }
                }
                return;
            }
            // A file from another program, at a point of the window:
            // `hoverfile 400 300 C:\path` holds it there, `dropfile 400 300
            // C:\path` drops it, `hoverfile off` takes it away.
            "hoverfile" | "dropfile" => {
                if argument == "off" {
                    self.dev_files = None;
                    return;
                }
                let mut words = argument.splitn(3, ' ');
                let (Some(x), Some(y), Some(path)) = (
                    words.next().and_then(|n| n.parse().ok()),
                    words.next().and_then(|n| n.parse().ok()),
                    words.next(),
                ) else {
                    return;
                };
                self.dev_files = Some(DevFiles {
                    pointer: egui::pos2(x, y),
                    path: PathBuf::from(path.trim()),
                    drop: verb == "dropfile",
                    delivered: false,
                });
                ctx.request_repaint();
                return;
            }
            // Pointer, in points from the window's top left: `click 300 200`
            // (`click 300 200 right`), `wheel 300 200 down`.
            "click" | "wheel" => {
                let words: Vec<&str> = argument.split(' ').filter(|w| !w.is_empty()).collect();
                let (Some(x), Some(y)) = (
                    words.first().and_then(|n| n.parse().ok()),
                    words.get(1).and_then(|n| n.parse().ok()),
                ) else {
                    return;
                };
                let pos = egui::pos2(x, y);
                let modifiers = egui::Modifiers::NONE;
                self.dev_input.push(egui::Event::PointerMoved(pos));
                if verb == "wheel" {
                    let notches = if words.get(2) == Some(&"down") { -1.0 } else { 1.0 };
                    self.dev_input.push(egui::Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Line,
                        delta: egui::vec2(0.0, notches),
                        phase: egui::TouchPhase::Move,
                        modifiers,
                    });
                } else {
                    let button = match words.get(2) {
                        Some(&"right") => egui::PointerButton::Secondary,
                        Some(&"middle") => egui::PointerButton::Middle,
                        _ => egui::PointerButton::Primary,
                    };
                    for pressed in [true, false] {
                        self.dev_input.push(egui::Event::PointerButton { pos, button, pressed, modifiers });
                    }
                }
                ctx.request_repaint();
                return;
            }
            "select" | "also" | "all" | "filter" => {
                if let Some(session) = self.session_mut() {
                    match verb {
                        "select" => session.select_only(argument),
                        "also" => session.toggle(argument),
                        "all" => session.select_all(),
                        _ => {
                            session.filter = argument.to_owned();
                            session.mark_view_stale();
                        }
                    }
                }
                return;
            }
            // The keychain, keys and SSH options: see `dev_step_ssh_auth`.
            _ if self.dev_step_ssh_auth(verb, argument, ctx, now) => return,
            // Colours, tags, export and import, tabs kept (src/app/hosts.rs):
            // `hostcolor Name > red`, `foldercolor Folder > none`,
            // `hosttags Name > prod, web`, `tagsdialog Name`, in the open
            // form `formcolor red`, `formtags a, b`, `tagtext pro`;
            // `exportall`, `exportfolder Folder`, `exporthost Name`,
            // `importvault` (both through S3V_DEV_VAULT), then `vaultpass x`,
            // `vaultconfirm x`, `vaultsecrets off`, `vaultkeys on`,
            // `vaulttoggle Name`, `merge skip|replace|both`; `restoretabs off`,
            // `savetabs` (to S3V_DEV_SESSION).
            _ if self.dev_step_hosts(verb, argument, ctx, now) => return,
            // Terminals: `termfind 29`, `termscheme braise`, `termlog`… (see
            // `dev_step_terminal` in app/terminal_ext.rs).
            _ if self.dev_step_terminal(verb, argument, ctx, now) => return,
            // Snippets, broadcast, overview (src/app/ssh_power.rs):
            // `snippets` opens their dialog, where `snippetnew`, `snippetpick
            // Name`, `snippetfield name|command|description Value` (`\n` for a
            // new line) and `snippetenter off` edit; `newsnippet Name > cmd`
            // adds one, `snippet Name` runs one in the terminal in use,
            // `snippetvar port 8080` answers its question (then `confirm`),
            // `snippetmenu` opens the Snippets list of the terminal in use;
            // `broadcast` toggles the tab in use (`broadcast 2` another,
            // `broadcast all`, `broadcast off`); `stats` toggles the server
            // overview (`stats 2`); `startup Name` and `startupcmd ls` set the
            // startup of the connection in use, `startuppick Name` adds a
            // snippet in the open connection form; `focusterm` gives the
            // keyboard to the terminal in use; `hover 300 200` moves the
            // pointer without clicking, for tooltips.
            _ if self.dev_step_ssh_power(verb, argument, ctx, now) => return,
            // Tunnels: `tunneladd 1 L 15020 web:80 > Site`, `tunnelstart 1`,
            // `tunnels`, … (see `App::dev_step_tunnels`).
            _ if self.dev_step_tunnels(verb, argument, ctx, now) => return,
            // S3: `s3versions`, `s3settings`, `s3deleted`, `s3press`… (src/app/s3.rs).
            _ if self.dev_step_s3(verb, argument, ctx, now) => return,
            // Preview, search and followed files: see `App::dev_step_viewers`.
            _ if self.dev_step_viewers(verb, argument, ctx, now) => return,
            // Rename, move, copy, permissions, properties, sizes, bookmarks:
            // see `dev_step_file_ops` for its steps.
            _ if self.dev_step_file_ops(verb, argument, ctx, now) => return,
            // This computer: `local`, `localtab`, `localterm cmd` (or
            // `powershell`, `wsl:Ubuntu`), `localgoto C:\some\folder`,
            // `beside`, `copyother`, `askcopy` (F5), and
            // `drag 300 200 900 240`, a drag from one point to another
            // (`… hold` keeps the button down until `drop`).
            _ if self.dev_step_local(verb, argument, ctx, now) => return,
            // Transfers: see `dev_step_transfers` in src/app/transfer.rs.
            _ if self.dev_step_transfers(verb, argument, ctx, now) => return,
            // The built-in editor: `edit notes.txt`, `etype`, `efind`,
            // `esave`… (see `dev_step_editor` in src/app/editor.rs).
            _ if self.dev_step_editor(verb, argument, ctx, now) => return,
            // The dashboard: `monitor`, `watch NAME`, `monitorpick`,
            // `monitoralert NAME disk / 92`… (see `dev_step_monitor`).
            _ if self.dev_step_monitor(verb, argument, now) => return,
            // "wait" and anything unknown just let time pass.
            _ => return,
        };
        self.apply(action, ctx, now);
    }

    fn profile_named(&self, name: &str) -> Option<&Profile> {
        self.profiles.iter().find(|profile| profile.title() == name)
    }

    /// Opens the form of a connection, new or existing.
    fn edit_profile(&mut self, draft: Profile, is_new: bool) {
        let mut form = ProfileForm::new(draft, is_new);
        form.folders = profiles::folders(&self.profiles, &self.settings);
        form.ssh = self.ssh_form_extras(&form.draft);
        form.hosts.known = crate::hosts::known_tags(&self.profiles);
        self.open_dialog(Dialog::Profile(Box::new(form)));
    }

    /// Preferences are the user's: a script never overwrites them.
    fn save_settings(&self) {
        if !dev::active() {
            profiles::save_settings(&self.settings);
        }
    }

    /// Keeps a folder in the list even once it is empty.
    fn remember_folder(&mut self, name: &str) {
        let path = profiles::folder_path(name);
        if !path.is_empty() && !self.settings.folders.contains(&path) {
            self.settings.folders.push(path);
            self.save_settings();
        }
    }

    /// Opens the folder of this path and those it sits in, so that what was
    /// just put there shows.
    fn reveal_folder(&mut self, path: &str) {
        self.settings
            .collapsed
            .retain(|closed| !profiles::folder_contains(closed, path));
    }

    fn report_save_error(&mut self, result: Result<()>, now: f64) {
        if let Err(error) = result {
            self.toast(now, save_failed(&error), true);
        }
    }

    fn toast(&mut self, now: f64, text: impl Into<String>, error: bool) {
        self.toasts.push(Toast {
            text: text.into(),
            error,
            expires: now + if error { 9.0 } else { 3.5 },
        });
    }

    /// The file browser of the tab being shown.
    fn session(&self) -> Option<&Session> {
        self.tabs.get(self.active).map(|tab| &tab.session)
    }

    fn session_mut(&mut self) -> Option<&mut Session> {
        self.tabs.get_mut(self.active).map(|tab| &mut tab.session)
    }

    /// Brings the views back in order after any change to the tabs.
    fn settle(&mut self) {
        self.active = self.views.settle(&mut self.tabs);
    }

    /// The index of the tab this view shows.
    fn shown_index(&self, side: Side) -> Option<usize> {
        let id = self.views.shown(side)?;
        self.tabs.iter().position(|tab| tab.id == id)
    }

    fn tab_index(&self, id: u64) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.id == id)
    }

    /// Opens a new tab on a saved connection, in the view in use. Nothing
    /// connects yet: see [`App::start_panes`].
    fn open_tab(&mut self, profile_id: &str, panes: Option<Panes>, now: f64) {
        let Some(mut tab) = self.build_tab(profile_id, now) else {
            return;
        };
        if let Some(panes) = panes
            && tab.has_shell()
        {
            tab.panes = panes;
        }
        self.add_tab(tab, None);
    }

    /// Adds a tab and shows it: right after the tab `after`, in its view,
    /// or else at the end of the view in use.
    fn add_tab(&mut self, mut tab: Tab, after: Option<u64>) {
        let id = tab.id;
        match after.and_then(|after| self.tab_index(after)) {
            Some(at) => {
                tab.side = self.tabs[at].side;
                self.tabs.insert(at + 1, tab);
            }
            None => {
                tab.side = if self.tabs.is_empty() { Side::Left } else { self.views.focus };
                self.tabs.push(tab);
            }
        }
        self.views.show(&self.tabs, id);
        self.settle();
    }

    /// A tab like one that exists or existed: same connection, name,
    /// folder and panes. It is not added yet.
    fn tab_like(&mut self, like: &ClosedTab, now: f64) -> Option<Tab> {
        if let Some(file) = &like.file {
            return Some(self.editor_tab(file));
        }
        let mut tab = self.build_tab(&like.profile_id, now)?;
        if tab.has_shell() {
            tab.panes = like.panes;
        }
        tab.title.clone_from(&like.title);
        tab.session.title.clone_from(&like.title);
        if let Some(path) = &like.path {
            tab.session.start_at(path.clone());
        }
        Some(tab)
    }

    /// Prepares a tab on a saved connection, without adding it.
    fn build_tab(&mut self, profile_id: &str, now: f64) -> Option<Tab> {
        if crate::monitor::is_dashboard(profile_id) {
            return Some(self.dashboard_tab());
        }
        let profile = self.profiles.iter().find(|p| p.id == profile_id).cloned();
        // This computer is built in, not saved.
        let profile = profile.or_else(|| crate::local::entry(profile_id))?;
        let dev = is_dev(profile_id);
        let secret = if profile.protocol == Protocol::Local {
            String::new()
        } else if dev {
            self.dev_secrets.get(profile_id).cloned().unwrap_or_default()
        } else {
            match profiles::secret(profile_id) {
                Ok(secret) => secret,
                // A password can be empty: key files, anonymous FTP.
                Err(_) if profile.protocol != Protocol::S3 => String::new(),
                Err(_) => {
                    self.toast(
                        now,
                        tr!(
                            "Clé secrète introuvable pour « {name} ». Modifiez la connexion pour la saisir à nouveau.",
                            name = profile.title()
                        ),
                        true,
                    );
                    return None;
                }
            }
        };
        // The throwaway connections of tests trust any host and record none.
        let known_hosts = if dev {
            None
        } else {
            Some(profiles::known_hosts_path().unwrap_or_else(|| PathBuf::from("known_hosts")))
        };

        // Identity, jump hosts, agent and passphrase prompts.
        let (profile, secret, known_hosts, extra) = self.ssh_route(profile, secret, known_hosts);
        let tab = Tab::open(self.next_tab_id, &profile, &secret, known_hosts, extra, &self.worker);
        self.next_tab_id += 1;
        Some(tab)
    }

    fn close_tab(&mut self, index: usize) {
        if let Some(tab) = self.tabs.get(index) {
            let id = tab.id;
            self.close_tabs(&[id], true);
        }
    }

    /// Closes the tabs of these ids. With `remember`, they can be brought
    /// back with [`Action::ReopenTab`]. Files with changes are asked about
    /// first (see `app::editor`).
    fn close_tabs(&mut self, ids: &[u64], remember: bool) {
        if ids.is_empty() || self.ask_before_closing(ids, remember) {
            return;
        }
        self.drop_tabs(ids, remember);
    }

    /// Closes the tabs of these ids without asking anything.
    fn drop_tabs(&mut self, ids: &[u64], remember: bool) {
        if ids.is_empty() {
            return;
        }
        self.views.closing(&self.tabs, ids);
        if remember {
            let closed = self.tabs.iter().filter(|tab| ids.contains(&tab.id)).map(ClosedTab::of);
            self.closed.extend(closed);
            let extra = self.closed.len().saturating_sub(CLOSED_TABS);
            self.closed.drain(..extra);
        }
        // Dropping a tab ends its shell and closes its connections.
        self.tabs.retain(|tab| !ids.contains(&tab.id));
        self.settle();
    }

    fn open_terminal(&mut self, index: usize) {
        if let Some(tab) = self.tabs.get_mut(index)
            && let Some(ssh) = &tab.ssh
        {
            tab.terminal = Some(Terminal::open(&self.worker, ssh.clone()));
        }
        self.open_local_terminal(index);
    }

    /// Connects what the tabs on screen show and have not opened yet. Tabs
    /// in the background, and panes that are hidden, stay disconnected.
    fn start_panes(&mut self) {
        for side in Side::BOTH {
            let Some(index) = self.shown_index(side) else {
                continue;
            };
            let tab = &self.tabs[index];
            let files = tab.panes.files() && !tab.session.started;
            let shell = tab.panes.terminal() && tab.terminal.is_none();
            if files {
                self.reload(index);
            }
            if shell {
                self.open_terminal(index);
            }
        }
    }

    /// (Re)lists the current folder of a tab.
    fn reload(&mut self, index: usize) {
        let Some(tab) = self.tabs.get_mut(index) else {
            return;
        };
        // A file in the editor, or the dashboard, has no listing to refresh.
        if tab.editor.is_some() || tab.panes == Panes::Monitor {
            tab.session.refresh_at = None;
            return;
        }
        let session = &mut tab.session;
        session.started = true;
        session.begin_listing();
        session.refresh_at = None;

        self.generation += 1;
        session.generation.store(self.generation, Ordering::Relaxed);
        let (storage, current) = (session.storage.clone(), session.generation.clone());
        if session.needs_home {
            self.worker.home(storage, self.generation, current);
        } else {
            self.worker
                .list(storage, session.path.clone(), self.generation, current);
        }
    }

    /// The tab waiting for the listing of this generation, if it still is.
    fn tab_awaiting(&self, generation: u64) -> Option<usize> {
        self.tabs
            .iter()
            .position(|tab| tab.session.generation.load(Ordering::Relaxed) == generation)
    }

    fn drain_events(&mut self, ctx: &egui::Context, now: f64) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                Event::Listed {
                    generation,
                    entries,
                    done,
                } => {
                    if let Some(index) = self.tab_awaiting(generation) {
                        self.tabs[index].session.listed(entries, done);
                    }
                }
                Event::ListFailed { generation, error } => {
                    if let Some(index) = self.tab_awaiting(generation)
                        && let Some(error) = self.tabs[index].session.list_failed(error)
                    {
                        self.toast(now, tr!("Actualisation impossible : {error}", error), true);
                    }
                }
                Event::Home { generation, path } => {
                    let Some(index) = self.tab_awaiting(generation) else {
                        continue;
                    };
                    match path {
                        Ok(path) => {
                            self.tabs[index].session.set_home(path);
                            self.reload(index);
                        }
                        // Shown in place of the listing, with a retry button.
                        Err(error) => {
                            self.tabs[index].session.list_failed(error);
                        }
                    }
                }
                Event::TransferAdded(transfer) => {
                    // Opening a file is not a transfer worth a panel.
                    self.show_transfers |= !transfer.quiet;
                    self.transfers.push(transfer);
                }
                Event::Changed { storage, path } => {
                    for tab in &mut self.tabs {
                        let session = &mut tab.session;
                        if session.storage_key() != storage {
                            continue;
                        }
                        session.invalidate_below(&path);
                        if session.path == path && session.refresh_at.is_none() {
                            session.refresh_at = Some(now + REFRESH_DELAY);
                        }
                    }
                }
                Event::Notice { text, error } => self.toast(now, text, error),
                Event::Clipboard { text, notice } => {
                    ctx.copy_text(text);
                    self.toast(now, notice, false);
                }
                Event::EditFetched { id, error } => self.edit_fetched(id, error, now),
                Event::EditSaved { id } => self.edit_saved(id, ctx),
                Event::EditUploaded { id, error } => self.edit_uploaded(id, error, ctx, now),
                Event::S3(event) => self.s3_event(event, now),
                Event::Transfers(event) => self.transfer_event(event, ctx, now),
            }
        }
    }

    /// Opens a file of the current folder: in the built-in editor when it
    /// is the editor chosen and the file is text, otherwise fetched and
    /// opened in a local program. `confirmed` skips the question asked for
    /// big files.
    fn open_remote(&mut self, name: &str, how: Launch, confirmed: bool, now: f64) {
        if self.open_in_editor(name, how) {
            return;
        }
        self.open_externally(name, how, confirmed, now);
    }

    /// Opens a file of the current folder in a local program, whatever the
    /// editor chosen.
    fn open_externally(&mut self, name: &str, how: Launch, confirmed: bool, now: f64) {
        if self.open_local_file(name, how, now) {
            return;
        }
        let Some(session) = self.session() else {
            return;
        };
        let Some(size) = session
            .rows()
            .find(|entry| entry.name == name && !entry.is_dir())
            .map(|entry| entry.size.unwrap_or(0))
        else {
            return;
        };
        let (storage, remote, origin) = (
            session.storage.clone(),
            session.path.child(name),
            session.title.clone(),
        );

        if how == Launch::Default && launch::kind(name) == launch::Kind::Program {
            self.toast(
                now,
                tr!(
                    "« {name} » est un programme : par sécurité, il n'est pas lancé depuis un serveur. \
                     Téléchargez-le, ou choisissez « Modifier » pour le lire comme du texte.",
                    name
                ),
                true,
            );
            return;
        }
        if size > LARGE_FILE && !confirmed {
            self.open_dialog(Dialog::OpenLarge {
                name: name.to_owned(),
                size,
                how,
            });
            return;
        }
        self.edit_remote(storage, remote, origin, size, how, now);
    }

    /// Fetches `remote` into a private copy and opens it in a local
    /// program, which is then watched for saves.
    fn edit_remote(
        &mut self,
        storage: Arc<dyn crate::storage::Storage>,
        remote: RemotePath,
        origin: String,
        size: u64,
        how: Launch,
        now: f64,
    ) {
        let name = remote.name().to_owned();
        let name = name.as_str();
        // Already open: never fetch over changes that were not sent.
        if let Some(id) = self.edits.find(&edits::key(&storage, &remote)) {
            let Some(edit) = self.edits.get_mut(id) else {
                return;
            };
            match edit.state {
                EditState::Fetching => {}
                EditState::Synced => {
                    edit.state = EditState::Fetching;
                    edit.launch = how;
                    self.worker.fetch_edit(
                        id,
                        edit.storage.clone(),
                        edit.remote.clone(),
                        edit.local.clone(),
                        size,
                        edit.watch.clone(),
                    );
                }
                _ => {
                    let local = edit.local.clone();
                    self.toast(
                        now,
                        tr!("« {name} » a des modifications non envoyées : c'est la copie locale qui s'ouvre.", name),
                        false,
                    );
                    self.launch(&local, how, now);
                }
            }
            return;
        }

        let edit = self.edits.add(storage, remote, origin, &safe_component(name), how);
        self.worker.fetch_edit(
            edit.id,
            edit.storage.clone(),
            edit.remote.clone(),
            edit.local.clone(),
            size,
            edit.watch.clone(),
        );
        self.worker.watch_edit(edit.id, edit.local.clone(), edit.watch.clone());
    }

    fn launch(&mut self, local: &Path, how: Launch, now: f64) {
        if let Err(error) = launch::start(local, how, &self.settings.editor) {
            let error = format!("{error:#}");
            self.toast(now, tr!("Ouverture impossible : {error}", error), true);
        }
    }

    fn edit_fetched(&mut self, id: u64, error: Option<String>, now: f64) {
        let Some(edit) = self.edits.get_mut(id) else {
            return;
        };
        match error {
            None => {
                edit.state = EditState::Synced;
                let (local, how) = (edit.local.clone(), edit.launch);
                self.launch(&local, how, now);
            }
            Some(error) => {
                let name = edit.name.clone();
                // A copy fetched earlier is still good to work on.
                if edit.local.exists() {
                    edit.state = EditState::Synced;
                } else {
                    self.edits.forget(id);
                }
                self.toast(now, tr!("Ouverture de « {name} » impossible : {error}", name, error), true);
            }
        }
    }

    /// The program editing a file saved it: send it, or ask.
    fn edit_saved(&mut self, id: u64, ctx: &egui::Context) {
        let Some(edit) = self.edits.get_mut(id) else {
            return;
        };
        match edit.state {
            EditState::Fetching => {}
            EditState::Uploading => edit.saved_again = true,
            _ => {
                edit.state = EditState::Modified;
                if edit.auto_upload {
                    self.push_edit(id);
                    return;
                }
                if !self.edit_prompts.contains(&id) {
                    self.edit_prompts.push_back(id);
                }
                // The user is in the editor: flash, without taking over.
                if !dev::active() && !ctx.input(|input| input.focused) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
                        egui::UserAttentionType::Informational,
                    ));
                }
            }
        }
    }

    /// Sends the local changes of an edited file back to the server.
    fn push_edit(&mut self, id: u64) {
        let Some(edit) = self.edits.get_mut(id) else {
            return;
        };
        if !matches!(edit.state, EditState::Modified | EditState::Failed(_)) {
            return;
        }
        edit.state = EditState::Uploading;
        edit.saved_again = false;
        self.worker
            .upload_edit(id, edit.storage.clone(), edit.local.clone(), edit.remote.clone());
    }

    fn edit_uploaded(&mut self, id: u64, error: Option<String>, ctx: &egui::Context, now: f64) {
        let Some(edit) = self.edits.get_mut(id) else {
            return;
        };
        let (name, origin) = (edit.name.clone(), edit.origin.clone());
        let saved_again = std::mem::take(&mut edit.saved_again);
        match error {
            None => {
                edit.state = EditState::Synced;
                self.toast(now, tr!("« {name} » envoyé sur {origin}", name, origin), false);
                if saved_again {
                    self.edit_saved(id, ctx);
                }
            }
            Some(error) => {
                edit.state = EditState::Failed(error.clone());
                self.toast(now, tr!("Envoi de « {name} » impossible : {error}", name, error), true);
            }
        }
    }

    /// Asks about the next edited file saved since, once nothing else is
    /// being asked.
    fn ask_about_saved_edits(&mut self) {
        if self.dialog.is_some() || self.quick_open.is_some() {
            return;
        }
        while let Some(id) = self.edit_prompts.pop_front() {
            if let Some(edit) = self.edits.get(id)
                && edit.state == EditState::Modified
                && !edit.auto_upload
            {
                let dialog = Dialog::PushEdit {
                    id,
                    name: edit.name.clone(),
                    target: edit.target(),
                    always: false,
                };
                self.open_dialog(dialog);
                return;
            }
        }
    }

    /// Closing the window with changes that never reached the server asks
    /// first: files of the editor not saved, then files edited in another
    /// program and not sent.
    fn guard_close(&mut self, ctx: &egui::Context) {
        if self.quitting || !ctx.input(|input| input.viewport().close_requested()) {
            return;
        }
        // Watching servers in the background, if the preferences say so.
        if self.close_to_background(ctx) {
            return;
        }
        if self.ask_before_quitting(ctx) {
            return;
        }
        let unsent = self.edits.unsent();
        if unsent > 0 {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.open_dialog(Dialog::Quit { unsent });
        }
    }

    /// Time-driven housekeeping: delayed refresh, speed meters, toasts.
    fn tick(&mut self, ctx: &egui::Context, now: f64) {
        for index in 0..self.tabs.len() {
            let Some(at) = self.tabs[index].session.refresh_at else {
                continue;
            };
            if now >= at {
                self.reload(index);
            } else {
                ctx.request_repaint_after(Duration::from_secs_f64(at - now));
            }
        }

        let mut active = false;
        for transfer in &self.transfers {
            match transfer.state() {
                TransferState::Running => {
                    active = true;
                    // A folder is measured on all of its files, and its stream.
                    let done = transfer.bytes_done();
                    self.meters.entry(transfer.id).or_default().sample(now, done);
                }
                TransferState::Queued => active = true,
                _ => {
                    self.meters.remove(&transfer.id);
                }
            }
        }
        if active {
            ctx.request_repaint_after(Duration::from_millis(120));
        }

        self.toasts.retain(|toast| toast.expires > now);
        if let Some(next) = self.toasts.iter().map(|toast| toast.expires).reduce(f64::min) {
            ctx.request_repaint_after(Duration::from_secs_f64((next - now).max(0.0)));
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        let in_terminal = self.tabs.get(self.active).is_some_and(Tab::terminal_focused);

        // Tabs first, and taken out of the input so the shell never sees
        // them. Ctrl+W belongs to the shell while it has the keyboard.
        let shortcut = |modifiers, key| KeyboardShortcut::new(modifiers, key);
        let (previous, next, close, sidebar, palette, reopen) = ctx.input_mut(|input| {
            let shifted = Modifiers::CTRL | Modifiers::SHIFT;
            // With Shift, a shortcut works everywhere; without, the shell
            // keeps the keys it gives a meaning to (Ctrl+B is tmux's).
            let mut anywhere = |key| {
                input.consume_shortcut(&shortcut(shifted, key))
                    || (!in_terminal && input.consume_shortcut(&shortcut(Modifiers::CTRL, key)))
            };
            let (close, sidebar, palette) = (anywhere(Key::W), anywhere(Key::B), anywhere(Key::K));
            (
                input.consume_shortcut(&shortcut(shifted, Key::Tab)),
                input.consume_shortcut(&shortcut(Modifiers::CTRL, Key::Tab)),
                close,
                sidebar,
                palette,
                input.consume_shortcut(&shortcut(shifted, Key::T)),
            )
        });
        if reopen {
            actions.push(Action::ReopenTab);
        }
        if sidebar {
            actions.push(Action::ToggleSidebar);
        }
        if palette {
            actions.push(Action::QuickOpen);
        }
        // Ctrl+Tab goes round the tabs of the view in use.
        let side = self.views.focus;
        let mine: Vec<usize> = (0..self.tabs.len())
            .filter(|index| self.tabs[*index].side == side)
            .collect();
        if let Some(at) = mine.iter().position(|index| *index == self.active) {
            let count = mine.len();
            if next {
                actions.push(Action::SelectTab(mine[(at + 1) % count]));
            } else if previous {
                actions.push(Action::SelectTab(mine[(at + count - 1) % count]));
            }
            if close {
                actions.push(Action::CloseTab(self.active));
            }
        }

        // The rest drives the file browser.
        self.file_ops_shortcuts(ctx, actions);
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        if in_terminal || !tab.panes.files() {
            return;
        }
        let session = &mut tab.session;
        let typing = ctx.egui_wants_keyboard_input();
        let (pressed, modifiers) = ctx.input(|input| {
            let pressed = |key| input.key_pressed(key);
            (
                [
                    pressed(Key::F5),
                    pressed(Key::F),
                    pressed(Key::Backspace),
                    pressed(Key::ArrowLeft),
                    pressed(Key::ArrowRight),
                    pressed(Key::Delete),
                    pressed(Key::A),
                    pressed(Key::Enter),
                    pressed(Key::ArrowUp),
                    pressed(Key::ArrowDown),
                    pressed(Key::Escape),
                ],
                input.modifiers,
            )
        });
        let [f5, f, backspace, left, right, delete, a, enter, up, down, escape] = pressed;

        if f5 {
            actions.push(crate::ui::local::f5(session));
        }
        if f && modifiers.command {
            session.focus_filter = true;
        }
        if escape && !session.filter.is_empty() {
            session.filter.clear();
            session.mark_view_stale();
        }
        if typing {
            return;
        }

        if backspace || (up && modifiers.alt) {
            actions.push(Action::Up);
        } else if left && modifiers.alt {
            actions.push(Action::Back);
        } else if right && modifiers.alt {
            actions.push(Action::Forward);
        } else if up {
            session.move_selection(-1);
        } else if down {
            session.move_selection(1);
        }

        if a && modifiers.command {
            session.select_all();
        }
        if delete && !session.selection.is_empty() && session.writable() {
            actions.push(Action::AskDeleteSelection);
        }
        if enter
            && session.selection.len() == 1
            && let Some(name) = session.selection.iter().next()
        {
            actions.push(Action::Open(name.clone()));
        }
    }

    /// Uploads the files dropped from another program where they landed:
    /// the folder or the listing under the pointer, else the folder of the
    /// view in use.
    fn dropped_files(&mut self, ctx: &egui::Context, now: f64, incoming: browser::Incoming) {
        if self.dev_files.as_ref().is_some_and(|files| files.delivered) {
            self.dev_files = None;
        }
        let dropped: Vec<PathBuf> = ctx.input(|input| {
            input
                .raw
                .dropped_files
                .iter()
                .map(|file| file.path().to_path_buf())
                .filter(|path| !path.as_os_str().is_empty())
                .collect()
        });
        if dropped.is_empty() {
            return;
        }
        match incoming.target.or(incoming.fallback) {
            Some(browser::DropTarget {
                folder: Some(folder),
                storage,
                ..
            }) => self.worker.upload(storage, dropped, folder),
            Some(target) => self.toast(now, format!("{}.", closed_to_files(&target)), true),
            None => self.toast(now, format!("{}.", tr!("Affichez d'abord les fichiers d'une connexion")), true),
        }
    }

    fn open_dialog(&mut self, dialog: Dialog) {
        self.dialog = Some(dialog);
        self.dialog_just_opened = true;
    }

    /// The selected entries of the current folder, as worker items.
    fn selected_items(&self) -> Vec<Item> {
        let Some(session) = self.session() else {
            return Vec::new();
        };
        session
            .selected()
            .map(|entry| Item {
                path: session.path.child(&entry.name),
                is_dir: entry.is_dir(),
                size: entry.size.unwrap_or(0),
            })
            .collect()
    }

    fn apply(&mut self, action: Action, ctx: &egui::Context, now: f64) {
        match action {
            // The files of a connection open in the editor are not its tab.
            Action::Connect(id) => match self.tabs.iter().find(|tab| tab.profile_id == id && tab.editor.is_none()) {
                Some(tab) => {
                    let id = tab.id;
                    self.views.show(&self.tabs, id);
                    self.settle();
                }
                None => self.open_tab(&id, None, now),
            },
            Action::OpenTab(id, panes) => self.open_tab(&id, panes, now),
            Action::SelectTab(index) => {
                if let Some(tab) = self.tabs.get_mut(index) {
                    if let Some(terminal) = &mut tab.terminal {
                        terminal.wants_focus = true;
                    }
                    let id = tab.id;
                    self.views.show(&self.tabs, id);
                    self.settle();
                }
            }
            Action::CloseTab(index) => self.close_tab(index),
            Action::CloseTabs(id, which) => {
                let ids = to_close(&self.tabs, id, which);
                self.close_tabs(&ids, true);
            }
            Action::DuplicateTab(id) => {
                // A file opens once.
                let index = self.tab_index(id).filter(|index| self.tabs[*index].editor.is_none());
                let Some(like) = index.map(|index| ClosedTab::of(&self.tabs[index])) else {
                    return;
                };
                if let Some(tab) = self.tab_like(&like, now) {
                    self.add_tab(tab, Some(id));
                }
            }
            Action::AskRenameTab(id) => {
                if let Some(index) = self.tab_index(id) {
                    let name = self.tabs[index].title.clone();
                    self.open_dialog(Dialog::RenameTab { id, name });
                }
            }
            Action::ReconnectTab(id) => {
                let Some(index) = self.tab_index(id).filter(|index| self.tabs[*index].editor.is_none()) else {
                    return;
                };
                let like = ClosedTab::of(&self.tabs[index]);
                // The new tab takes the place and the identity of the old
                // one, whose shell and connections end as it is dropped.
                if let Some(mut tab) = self.tab_like(&like, now) {
                    tab.id = id;
                    tab.side = self.tabs[index].side;
                    self.tabs[index] = tab;
                }
            }
            Action::ReopenTab => {
                // A connection deleted since cannot come back: try the next.
                while let Some(closed) = self.closed.pop() {
                    if self.show_open_file(&closed) {
                        break;
                    }
                    if let Some(tab) = self.tab_like(&closed, now) {
                        self.add_tab(tab, None);
                        break;
                    }
                }
            }
            Action::MoveTab(id, place) => {
                let Some(index) = self.tab_index(id) else {
                    return;
                };
                let target = match place {
                    TabPlace::View(side) => Some(side),
                    TabPlace::Before(other) | TabPlace::After(other) => {
                        self.tab_index(other).map(|at| self.tabs[at].side)
                    }
                    TabPlace::First | TabPlace::Last => None,
                };
                // The view it leaves turns to the neighbour of the tab.
                if target.is_some_and(|side| side != self.tabs[index].side) {
                    self.views.closing(&self.tabs, &[id]);
                }
                if place_tab(&mut self.tabs, id, place) {
                    self.views.show(&self.tabs, id);
                }
                self.settle();
            }
            Action::JoinViews => {
                let keep = self.views.shown(self.views.focus);
                for tab in &mut self.tabs {
                    tab.side = Side::Left;
                }
                if let Some(id) = keep {
                    self.views.show(&self.tabs, id);
                }
                self.settle();
            }
            Action::CopyTabAddress(id) => {
                let Some(session) = self.tab_index(id).map(|index| &self.tabs[index].session) else {
                    return;
                };
                match session.storage.plain_url(&session.path) {
                    Some(url) => {
                        ctx.copy_text(url);
                        self.toast(now, tr!("Adresse copiée"), false);
                    }
                    None => self.toast(now, tr!("Ce stockage n'a pas d'adresse directe."), true),
                }
            }
            Action::SetPanes(panes) => {
                let Some(tab) = self.tabs.get_mut(self.active) else {
                    return;
                };
                if !tab.has_shell() {
                    return;
                }
                tab.panes = panes;
                if let Some(terminal) = &mut tab.terminal {
                    terminal.wants_focus = panes.terminal();
                }
                // The connection reopens the way it was last used.
                let id = tab.profile_id.clone();
                if !is_dev(&id)
                    && let Some(profile) = self.profiles.iter_mut().find(|p| p.id == id)
                {
                    profile.panes = panes;
                    let _ = self.persist_profiles();
                }
            }
            Action::TerminalHere => {
                if self.local_terminal_here() {
                    return;
                }
                let Some(tab) = self.tabs.get_mut(self.active) else {
                    return;
                };
                let Some(ssh) = &tab.ssh else {
                    return;
                };
                if tab.panes == Panes::Files {
                    tab.panes = Panes::Both;
                }
                let terminal = tab
                    .terminal
                    .get_or_insert_with(|| Terminal::open(&self.worker, ssh.clone()));
                // Typed before the shell is up, the line simply waits for it.
                terminal.send(format!("cd {}\r", shell_quote(&tab.session.path.to_string())));
                terminal.wants_focus = true;
            }
            Action::ReconnectTerminal => self.open_terminal(self.active),
            Action::NewProfile => self.edit_profile(Profile::new(), true),
            Action::NewProfileIn(folder) => {
                let mut draft = Profile::new();
                draft.folder = folder;
                self.edit_profile(draft, true);
            }
            Action::AskImport => {
                // While a script drives the app, only its fixtures are read.
                let locations = match dev::import_dir() {
                    Some(dir) => Some(Locations::in_dir(&dir)),
                    None if dev::active() => None,
                    None => Some(Locations::system()),
                };
                let found = locations.map(|locations| import::scan(&locations)).unwrap_or_default();
                self.open_dialog(Dialog::Import(ImportForm::new(found, &self.profiles)));
            }
            Action::EditProfile(id) => {
                if let Some(profile) = self.profiles.iter().find(|p| p.id == id).cloned() {
                    self.edit_profile(profile, false);
                }
            }
            Action::AskDeleteProfile(id) => {
                if let Some(profile) = self.profiles.iter().find(|p| p.id == id) {
                    let title = profile.title().to_owned();
                    self.open_dialog(Dialog::DeleteProfile { id, title });
                }
            }
            Action::ToggleTheme => {
                self.settings.dark = !self.settings.dark;
                self.palette = Palette::for_mode(self.settings.dark);
                theme::apply(ctx, &self.palette);
                self.save_settings();
            }

            Action::SetLanguage(lang) => {
                self.settings.language = lang.map(|lang| lang.code().to_owned());
                crate::i18n::set(lang.unwrap_or_else(crate::i18n::system));
                self.save_settings();
                ctx.request_repaint();
            }

            Action::ToggleSidebar => {
                self.settings.sidebar_visible = !self.settings.sidebar_visible;
                self.save_settings();
            }
            Action::QuickOpen => {
                if self.dialog.is_none() {
                    self.quick_open = Some(QuickOpen::default());
                }
            }
            Action::SetFavorite(id, favorite) => {
                if let Some(profile) = self.profiles.iter_mut().find(|p| p.id == id)
                    && profile.favorite != favorite
                {
                    profile.favorite = favorite;
                    let saved = self.persist_profiles();
                    self.report_save_error(saved, now);
                }
            }
            Action::MoveToFolder(id, folder) => {
                let folder = profiles::folder_path(&folder);
                if let Some(profile) = self.profiles.iter_mut().find(|p| p.id == id)
                    && profile.folder != folder
                {
                    profile.folder.clone_from(&folder);
                    self.remember_folder(&folder);
                    // A folder that receives something shows it.
                    self.reveal_folder(&folder);
                    let saved = self.persist_profiles();
                    self.report_save_error(saved, now);
                }
            }
            Action::AskNewHostFolder { parent, profile } => self.open_dialog(Dialog::Folder(FolderForm {
                name: String::new(),
                renaming: None,
                parent,
                profile,
                existing: profiles::folders(&self.profiles, &self.settings),
            })),
            Action::AskRenameHostFolder(path) => self.open_dialog(Dialog::Folder(FolderForm {
                name: profiles::folder_name(&path).to_owned(),
                parent: profiles::folder_parent(&path),
                renaming: Some(path),
                profile: None,
                existing: profiles::folders(&self.profiles, &self.settings),
            })),
            Action::AskDeleteHostFolder(name) => {
                let count = self
                    .profiles
                    .iter()
                    .filter(|p| profiles::folder_contains(&name, &p.folder))
                    .count();
                let subfolders = profiles::folders(&self.profiles, &self.settings)
                    .iter()
                    .filter(|folder| **folder != name && profiles::folder_contains(&name, folder))
                    .count();
                if count + subfolders == 0 {
                    // Nothing to lose: no need to ask.
                    self.delete_folder(&name, now);
                } else {
                    self.open_dialog(Dialog::DeleteFolder {
                        name,
                        count,
                        subfolders,
                    });
                }
            }
            Action::MoveHostFolder(path, parent) => {
                // Not inside itself, and not where it already is.
                let moved = profiles::folder_join(&parent, profiles::folder_name(&path));
                if path.is_empty() || profiles::folder_contains(&path, &parent) || moved == path {
                    return;
                }
                self.rename_folder(&path, &moved);
                self.reveal_folder(&parent);
                self.save_settings();
                let saved = self.persist_profiles();
                self.report_save_error(saved, now);
            }
            Action::ToggleHostFolder(name) => {
                let collapsed = &mut self.settings.collapsed;
                match collapsed.iter().position(|folder| *folder == name) {
                    Some(index) => {
                        collapsed.remove(index);
                    }
                    None => collapsed.push(name),
                }
                self.save_settings();
            }
            Action::ToggleFavorites => {
                self.settings.favorites_collapsed = !self.settings.favorites_collapsed;
                self.save_settings();
            }

            Action::Navigate(path) => {
                if let Some(session) = self.session_mut() {
                    session.go_to(path);
                    self.reload(self.active);
                }
            }
            Action::Back => {
                if self.session_mut().is_some_and(Session::go_back) {
                    self.reload(self.active);
                }
            }
            Action::Forward => {
                if self.session_mut().is_some_and(Session::go_forward) {
                    self.reload(self.active);
                }
            }
            Action::Up => {
                if let Some(parent) = self.session().and_then(|s| s.path.parent()) {
                    self.apply(Action::Navigate(parent), ctx, now);
                }
            }
            Action::Refresh => self.reload(self.active),
            Action::Open(name) => {
                let Some(session) = self.session_mut() else {
                    return;
                };
                let is_dir = session
                    .rows()
                    .find(|entry| entry.name == name)
                    .map(|entry| entry.is_dir());
                match is_dir {
                    Some(true) => {
                        let path = session.path.child(&name);
                        self.apply(Action::Navigate(path), ctx, now);
                    }
                    Some(false) => self.open_remote(&name, Launch::Default, false, now),
                    None => {}
                }
            }

            Action::UploadFiles | Action::UploadFolder => {
                let Some(session) = self.session() else {
                    return;
                };
                let dialog = rfd::FileDialog::new();
                let picked = if let Some(source) = dev::upload_source() {
                    Some(vec![source])
                } else if matches!(action, Action::UploadFiles) {
                    dialog.set_title(tr!("Importer des fichiers")).pick_files()
                } else {
                    dialog.set_title(tr!("Importer un dossier")).pick_folders()
                };
                if let Some(sources) = picked.filter(|sources| !sources.is_empty()) {
                    self.worker
                        .upload(session.storage.clone(), sources, session.path.clone());
                }
            }
            Action::AskNewFolder => self.open_dialog(Dialog::NewFolder {
                name: String::new(),
            }),
            Action::DragOut => {
                // The rows being dragged, whichever view they come from. The
                // drag is the other program's from now on: dropped back on
                // this window, nothing must move here.
                let Some(dragged) = egui::DragAndDrop::take_payload::<crate::file_ops::Dragged>(ctx) else {
                    return;
                };
                let items: Vec<Item> = dragged
                    .items
                    .iter()
                    .map(|item| Item {
                        path: item.path.clone(),
                        is_dir: item.is_dir,
                        size: item.size,
                    })
                    .collect();
                if items.is_empty() {
                    return;
                }
                let storage = dragged.storage.clone();
                self.release_pointer = true;
                if dev::active() {
                    // A script holds no button: the drop would land wherever
                    // the real pointer happens to be.
                    self.toast(now, tr!("Glisser hors de la fenêtre : ignoré pendant un script."), false);
                    return;
                }
                if let Err(error) = os_drag::drag_out(self.worker.fetcher(), storage, items) {
                    let error = format!("{error:#}");
                    self.toast(now, tr!("Impossible de glisser ces fichiers : {error}", error), true);
                }
            }
            Action::DownloadSelection => {
                let items = self.selected_items();
                let Some(session) = self.session() else {
                    return;
                };
                if items.is_empty() || !session.writable() {
                    return;
                }
                let start = self.last_download_dir.clone().or_else(|| {
                    directories::UserDirs::new()
                        .and_then(|dirs| dirs.download_dir().map(PathBuf::from))
                });
                let mut dialog = rfd::FileDialog::new().set_title(tr!("Télécharger dans…"));
                if let Some(start) = start {
                    dialog = dialog.set_directory(start);
                }
                if let Some(folder) = dev::download_dir().or_else(|| dialog.pick_folder()) {
                    self.worker
                        .download(session.storage.clone(), items, folder.clone());
                    self.last_download_dir = Some(folder);
                }
            }
            Action::AskDeleteSelection => {
                let items = self.selected_items();
                let Some(session) = self.session() else {
                    return;
                };
                if items.is_empty() || !session.writable() {
                    return;
                }
                // On this computer, deleting goes to the Recycle Bin.
                if session.storage.local().is_some() {
                    return self.ask_trash(items);
                }
                let folders = items.iter().filter(|item| item.is_dir).count();
                let summary = match (items.as_slice(), folders) {
                    ([only], 0) => tr!("« {name} » sera supprimé.", name = only.path.name()),
                    ([only], _) => tr!(
                        "Le dossier « {name} » et tout son contenu seront supprimés.",
                        name = only.path.name()
                    ),
                    (many, 0) => tr!("{count} éléments seront supprimés.", count = many.len()),
                    (many, _) => trn!(
                        folders,
                        "{count} éléments seront supprimés, dont {n} dossier avec tout leur contenu.",
                        "{count} éléments seront supprimés, dont {n} dossiers avec tout leur contenu.",
                        count = many.len(),
                    ),
                };
                let parent = session.path.clone();
                self.open_dialog(Dialog::DeleteItems {
                    items,
                    parent,
                    summary,
                });
            }
            Action::CopyUrl(name) => {
                let Some(session) = self.session() else {
                    return;
                };
                match session.storage.plain_url(&session.path.child(&name)) {
                    Some(url) => {
                        ctx.copy_text(url);
                        self.toast(now, tr!("Adresse copiée"), false);
                    }
                    None => self.toast(now, tr!("Ce stockage n'a pas d'adresse directe."), true),
                }
            }
            Action::ShareLink(name) => self.share_link(&name),

            // The program used as text editor is chosen in the dialog of the
            // editor (`EditorAction::ChooseEditor`).
            Action::OpenFile(name, how) => self.open_remote(&name, how, false, now),
            Action::PushEdit(id) => self.push_edit(id),
            Action::ReopenEdit(id) => {
                if let Some(edit) = self.edits.get(id) {
                    let (local, how) = (edit.local.clone(), edit.launch);
                    self.launch(&local, how, now);
                }
            }
            Action::ForgetEdit(id) => {
                self.edits.forget(id);
                self.edit_prompts.retain(|pending| *pending != id);
            }

            Action::ToggleTransfers => self.show_transfers = !self.show_transfers,
            Action::SshAuth(action) => self.apply_ssh_auth(action, now),
            Action::Hosts(action) => self.apply_hosts(action, now),
            Action::Terminal(action) => self.terminal_action(action, ctx, now),
            Action::SshPower(action) => self.apply_ssh_power(action, now),
            Action::Tunnels(action) => self.apply_tunnels(action, ctx, now),
            Action::S3(action) => self.apply_s3(action, ctx, now),
            Action::Viewers(action) => self.apply_viewers(action, ctx, now),
            Action::FileOps(action) => self.apply_file_ops(action, ctx, now),
            Action::Local(action) => self.apply_local(action, now),
            Action::ClearFinishedTransfers => self.clear_finished_transfers(),
            Action::Transfers(action) => self.apply_transfer(action, ctx, now),
            Action::Editor(action) => self.apply_editor(action, ctx, now),
            Action::Update(action) => self.apply_update(action, ctx),
            Action::Monitor(action) => self.apply_monitor(action, now),
        }
    }

    /// Saves the ticked connections of the import dialog.
    fn import(&mut self, form: ImportForm, now: f64) {
        if form.vault {
            return self.ask_vault_import(now);
        }
        let scripted = dev::active();
        let stamp = Profile::new().id;
        let (mut imported, mut refiled, mut unsaved_secrets) = (0, 0, 0);
        // The imported connections and the jump hosts they go through.
        let mut jumps = Vec::new();
        for (index, item) in form.items.into_iter().enumerate() {
            // Saved before folders existed: back in the one it came from.
            if form.apply_folders
                && let Some(id) = &item.refile
                && let Some(saved) = self.profiles.iter_mut().find(|p| p.id == *id)
                && saved.folder.trim().is_empty()
            {
                saved.folder = profiles::folder_path(&item.candidate.group);
                self.remember_folder(&item.candidate.group);
                refiled += 1;
            }
            if !item.selected {
                continue;
            }
            let mut profile = item.candidate.profile;
            profile.folder = profiles::folder_path(&item.candidate.group);
            self.remember_folder(&item.candidate.group);
            // Nothing a script imports may reach the user's own list.
            profile.id = if scripted {
                format!("{}import-{stamp}-{index}", dev::PROFILE_PREFIX)
            } else {
                format!("{stamp}-{index}")
            };
            jumps.push((profile.id.clone(), item.candidate.jump));
            if let Some(secret) = item.candidate.secret {
                if scripted {
                    self.dev_secrets.insert(profile.id.clone(), secret);
                } else if profiles::set_secret(&profile.id, &secret).is_err() {
                    unsaved_secrets += 1;
                }
            }
            self.profiles.push(profile);
            imported += 1;
        }
        self.link_jumps(&jumps, now);
        if imported + refiled == 0 {
            return;
        }

        let mut text = match (imported, refiled) {
            (0, _) => trn!(refiled, "{n} connexion rangée dans son dossier", "{n} connexions rangées dans leur dossier"),
            (_, 0) => trn!(imported, "{n} connexion importée", "{n} connexions importées"),
            _ => tr!("{imported} importée(s), {refiled} rangée(s) dans leur dossier", imported, refiled),
        };
        let mut failed = false;
        if let Err(error) = self.persist_profiles() {
            text = save_failed(&error);
            failed = true;
        } else if unsaved_secrets > 0 {
            text = trn!(
                unsaved_secrets,
                "{text} ; {n} mot(s) de passe n'ont pas pu être enregistrés et seront à saisir",
                "{text} ; {n} mot(s) de passe n'ont pas pu être enregistrés et seront à saisir",
                text,
            );
            failed = true;
        }
        self.toast(now, text, failed);
    }

    fn persist_profiles(&self) -> Result<()> {
        // What a script changes stays in memory: the saved list is the user's.
        if dev::active() {
            return Ok(());
        }
        let saved: Vec<Profile> = self
            .profiles
            .iter()
            .filter(|profile| !is_dev(&profile.id))
            .cloned()
            .collect();
        profiles::save(&saved)
    }

    /// Stores the edited profile and its secret. Returns the profile id.
    fn save_profile(&mut self, form: &ProfileForm) -> Result<String> {
        let mut profile = form.profile();
        profile.name = profile.name.trim().to_owned();
        profile.endpoint = profile.endpoint.trim().to_owned();
        profile.region = profile.region.trim().to_owned();
        profile.user = profile.user.trim().to_owned();
        profile.key_path = profile.key_path.trim().to_owned();
        profile.initial_path = profile.initial_path.trim().to_owned();
        profile.folder = profiles::folder_path(&profile.folder);

        // Pasted S3 keys often carry stray spaces; a password may mean them.
        let secret = match profile.protocol {
            Protocol::S3 => form.secret.trim(),
            _ => form.secret.as_str(),
        };
        if !secret.is_empty() {
            if is_dev(&profile.id) || dev::active() {
                self.dev_secrets.insert(profile.id.clone(), secret.to_owned());
            } else {
                profiles::set_secret(&profile.id, secret)?;
            }
        }

        let id = profile.id.clone();
        self.remember_folder(&profile.folder);
        match self.profiles.iter_mut().find(|existing| existing.id == id) {
            Some(existing) => *existing = profile,
            None => self.profiles.push(profile),
        }
        self.persist_profiles()?;
        Ok(id)
    }

    /// Removes a folder of the list and the folders it holds. Their
    /// connections stay, in the folder this one was in.
    fn delete_folder(&mut self, name: &str, now: f64) {
        let parent = profiles::folder_parent(name);
        for profile in &mut self.profiles {
            if profiles::folder_contains(name, &profile.folder) {
                profile.folder.clone_from(&parent);
            }
        }
        let settings = &mut self.settings;
        settings.folders.retain(|folder| !profiles::folder_contains(name, folder));
        settings.collapsed.retain(|folder| !profiles::folder_contains(name, folder));
        self.save_settings();
        let saved = self.persist_profiles();
        self.report_save_error(saved, now);
    }

    /// Gives the folder `old` the path `new`, taking along everything it
    /// holds. A folder already at `new` receives the lot. Not saved here.
    fn rename_folder(&mut self, old: &str, new: &str) {
        for profile in &mut self.profiles {
            profile.folder = profiles::folder_moved(&profile.folder, old, new);
        }
        let settings = &mut self.settings;
        for folder in settings.folders.iter_mut().chain(&mut settings.collapsed) {
            *folder = profiles::folder_moved(folder, old, new);
        }
        for list in [&mut settings.folders, &mut settings.collapsed] {
            let mut seen = std::collections::HashSet::new();
            list.retain(|folder| !folder.is_empty() && seen.insert(folder.clone()));
        }
        if !new.is_empty() && !settings.folders.iter().any(|folder| folder == new) {
            settings.folders.push(new.to_owned());
        }
    }

    /// Creates or renames a folder of the list, as its dialog asked.
    fn save_folder(&mut self, form: &FolderForm, now: f64) {
        let Some(name) = form.path() else {
            return;
        };
        if let Some(old) = &form.renaming {
            if *old == name {
                return;
            }
            self.rename_folder(old, &name);
        }
        if let Some(id) = &form.profile
            && let Some(profile) = self.profiles.iter_mut().find(|p| p.id == *id)
        {
            profile.folder.clone_from(&name);
        }
        if !self.settings.folders.contains(&name) {
            self.settings.folders.push(name.clone());
        }
        self.reveal_folder(&name);
        self.save_settings();
        let saved = self.persist_profiles();
        self.report_save_error(saved, now);
    }

    fn dialogs(&mut self, ctx: &egui::Context, now: f64) {
        self.ask_about_saved_edits();
        let Some(mut dialog) = self.dialog.take() else {
            return;
        };
        let first_frame = std::mem::take(&mut self.dialog_just_opened);
        let mut outcome = dialogs::show(ctx, &self.palette, &mut dialog, first_frame);
        if let Some(confirmed) = self.dev_answer.take() {
            outcome = if confirmed {
                Outcome::Confirmed
            } else {
                Outcome::Cancelled
            };
        }

        let connect = matches!(outcome, Outcome::ConfirmedAndConnect);
        match outcome {
            Outcome::Pending => self.dialog = Some(dialog),
            Outcome::Cancelled => {}
            Outcome::Confirmed | Outcome::ConfirmedAndConnect => match dialog {
                Dialog::Profile(mut form) => match self.save_profile(&form) {
                    Ok(id) => {
                        self.monitor_profile_saved(&id);
                        if connect {
                            self.open_tab(&id, None, now);
                        }
                    }
                    Err(error) => {
                        form.error = Some(save_failed(&error));
                        self.dialog = Some(Dialog::Profile(form));
                    }
                },
                Dialog::Import(form) => self.import(form, now),
                Dialog::Folder(form) => self.save_folder(&form, now),
                Dialog::DeleteFolder { name, .. } => self.delete_folder(&name, now),
                Dialog::DeleteProfile { id, .. } => {
                    self.profiles.retain(|profile| profile.id != id);
                    if !dev::active() {
                        profiles::delete_secret(&id);
                    }
                    if let Err(error) = self.persist_profiles() {
                        self.toast(now, save_failed(&error), true);
                    }
                    // Files open in the editor keep their own connection:
                    // their changes are not lost with it.
                    let gone: Vec<u64> = self
                        .tabs
                        .iter()
                        .filter(|tab| tab.profile_id == id && tab.editor.is_none())
                        .map(|tab| tab.id)
                        .collect();
                    self.close_tabs(&gone, false);
                    self.closed.retain(|closed| closed.profile_id != id);
                }
                Dialog::RenameTab { id, name } => {
                    if let Some(index) = self.tab_index(id) {
                        // No name: back to the one of the connection.
                        let mut name = name.trim().to_owned();
                        if name.is_empty() {
                            let profile_id = &self.tabs[index].profile_id;
                            name = self
                                .profiles
                                .iter()
                                .find(|profile| profile.id == *profile_id)
                                .map_or_else(|| self.tabs[index].title.clone(), |profile| profile.title().to_owned());
                        }
                        let tab = &mut self.tabs[index];
                        tab.title.clone_from(&name);
                        tab.session.title = name;
                    }
                }
                Dialog::DeleteItems { items, parent, .. } => {
                    if let Some(tab) = self.tabs.get_mut(self.active) {
                        tab.session.selection.clear();
                        self.worker.delete(tab.session.storage.clone(), items, parent);
                    }
                }
                Dialog::NewFolder { name } => {
                    if let Some(session) = self.session() {
                        self.worker
                            .create_folder(session.storage.clone(), session.path.child(name.trim()));
                    }
                }
                Dialog::PushEdit { id, always, .. } => {
                    if let Some(edit) = self.edits.get_mut(id) {
                        edit.auto_upload = always;
                    }
                    self.push_edit(id);
                }
                Dialog::OpenLarge { name, how, .. } => self.open_remote(&name, how, true, now),
                Dialog::Quit { .. } => {
                    self.quitting = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                Dialog::SshAuth(dialog) => self.answer_ssh_auth(dialog, now),
                Dialog::Hosts(dialog) => self.hosts_dialog_confirmed(dialog, ctx, now),
                Dialog::Terminal(dialog) => self.terminal_dialog_done(dialog, ctx),
                Dialog::SshPower(dialog) => self.ssh_power_dialog_done(dialog, now),
                Dialog::Tunnel(form) => self.tunnel_dialog_done(*form, connect, ctx, now),
                // They do their work while open, and only ever close.
                Dialog::S3(_) => {}
                Dialog::FileOps(dialog) => self.answer_file_ops(dialog, ctx, now),
                Dialog::Local(dialog) => self.answer_local(dialog, now),
                Dialog::Transfers(dialog) => self.answer_transfer_dialog(dialog, now),
                Dialog::Editor(dialog) => self.answer_editor(dialog, ctx, now),
                Dialog::Update(dialog) => self.answer_update(dialog, ctx, now),
                Dialog::Monitor(dialog) => self.monitor_dialog_done(dialog),
            },
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let p = &self.palette;
        let open_edits = std::mem::take(&mut self.open_edits);
        ui.horizontal_centered(|ui| {
            match self.tabs.get(self.active) {
                Some(tab) if tab.editor.is_some() => crate::ui::editor::status(ui, p, tab, actions),
                Some(tab) if tab.panes.files() => {
                    ui.label(small(p, browser::summary(&tab.session)));
                }
                Some(tab) => {
                    if let Some(terminal) = &tab.terminal {
                        let (cols, rows) = terminal.size();
                        let mut text = format!("{} · {} · {cols}×{rows}", terminal.kind_label(), terminal.address);
                        if let Some(title) = terminal.title().filter(|title| !title.is_empty()) {
                            text.push_str(&format!(" · {title}"));
                        }
                        if terminal.mouse_mode() != vt100::MouseProtocolMode::None {
                            text.push_str(&format!(" · {}", tr!("souris au programme, Maj pour sélectionner")));
                        }
                        ui.add(egui::Label::new(small(p, text)).truncate());
                    }
                }
                None => {}
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                crate::ui::update::status_item(ui, p, &self.updater, actions);
                let active = self
                    .transfers
                    .iter()
                    .filter(|transfer| !transfer.state().is_finished())
                    .count();
                let label = if active > 0 {
                    let speed: f64 = self.meters.values().map(|meter| meter.bytes_per_second).sum();
                    format!(
                        "{}  {} · {}",
                        icon::ARROWS_DOWN_UP,
                        trn!(active, "{n} transfert", "{n} transferts"),
                        format::speed(speed)
                    )
                } else {
                    format!("{}  {}", icon::ARROWS_DOWN_UP, tr!("Transferts"))
                };
                let color = if active > 0 { p.accent } else { p.muted };
                let button = egui::Button::new(RichText::new(label).size(12.0).color(color))
                    .frame(false);
                if ui.add(button).clicked() {
                    actions.push(Action::ToggleTransfers);
                }
                ui.add_space(8.0);
                crate::ui::edits::button(ui, p, self.edits.list(), open_edits, actions);
                self.tunnels_status(ui, actions);
                crate::ui::file_ops::status(ui, p, &self.file_ops, actions);
            });
        });
    }

    /// The start page, shown while no tab is open: the favourites one click
    /// away, and the ways to reach everything else.
    fn welcome(&self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        const CARD: egui::Vec2 = egui::vec2(206.0, 54.0);
        const GAP: f32 = 10.0;
        let p = &self.palette;
        let mut favorites: Vec<&Profile> = self.profiles.iter().filter(|profile| profile.favorite).collect();
        favorites.sort_by_cached_key(|profile| profiles::sort_key(profile.title()));
        let pinned = favorites.len();
        // This computer is always one click away.
        let computer = crate::local::entry(crate::local::ID);
        favorites.splice(0..0, computer.iter());

        let columns = (((ui.available_width() + GAP) / (CARD.x + GAP)).floor() as usize).clamp(1, 4);
        let rows = favorites.len().div_ceil(columns);
        let cards_height = rows as f32 * (CARD.y + GAP);
        let (title, hint) = if self.profiles.is_empty() {
            (tr!("Bienvenue"), tr!("Ajoutez une connexion S3, SFTP ou FTP pour commencer."))
        } else if pinned == 0 {
            (
                tr!("Aucune connexion ouverte"),
                tr!(
                    "Choisissez-en une dans la liste, ou tapez son nom avec Ctrl+K.\n\
                     L'étoile d'une connexion l'épingle ici."
                ),
            )
        } else {
            (
                tr!("Aucune connexion ouverte"),
                tr!("Vos favoris, ou Ctrl+K pour ouvrir n'importe quelle connexion."),
            )
        };

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.vertical_centered(|ui| {
                let needed = 190.0 + cards_height;
                ui.add_space(((ui.available_height() - needed) * 0.4).max(20.0));
                ui.label(RichText::new(icon::PLUGS_CONNECTED).size(40.0).color(p.accent));
                ui.add_space(4.0);
                ui.label(RichText::new(title).font(theme::semibold(16.0)).color(p.text));
                ui.label(RichText::new(hint).color(p.muted));
                ui.add_space(16.0);

                for row in favorites.chunks(columns) {
                    // Each row is centred, the last one included.
                    let width = row.len() as f32 * (CARD.x + GAP) - GAP;
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, CARD.y), egui::Sense::hover());
                    for (index, profile) in row.iter().enumerate() {
                        let card = egui::Rect::from_min_size(
                            rect.min + egui::vec2(index as f32 * (CARD.x + GAP), 0.0),
                            CARD,
                        );
                        if favorite_card(ui, p, card, profile) {
                            actions.push(Action::Connect(profile.id.clone()));
                        }
                    }
                    ui.add_space(GAP - ui.spacing().item_spacing.y);
                }
                if !favorites.is_empty() {
                    ui.add_space(10.0);
                }

                ui.horizontal(|ui| {
                    // Centre the row of buttons: measured on the first frame.
                    let id = Id::new("welcome-buttons");
                    let width: f32 = ui.data(|data| data.get_temp(id)).unwrap_or(0.0);
                    ui.add_space(((ui.available_width() - width) / 2.0).max(0.0));
                    let start = ui.cursor().left();
                    let new = format!("{}  {}", icon::PLUS, tr!("Nouvelle connexion"));
                    if primary_button(ui, p, &new, true).clicked() {
                        actions.push(Action::NewProfile);
                    }
                    if !self.profiles.is_empty()
                        && ui
                            .button(format!("{}  {}", icon::MAGNIFYING_GLASS, tr!("Rechercher…")))
                            .on_hover_text(tr!("Ctrl+K"))
                            .clicked()
                    {
                        actions.push(Action::QuickOpen);
                    }
                    if ui
                        .button(format!("{}  {}", icon::DOWNLOAD_SIMPLE, tr!("Importer…")))
                        .on_hover_text(tr!("Depuis FileZilla, Windows Terminal ou ~/.ssh/config"))
                        .clicked()
                    {
                        actions.push(Action::AskImport);
                    }
                    crate::ui::local::welcome_buttons(ui, p, actions);
                    let measured = ui.min_rect().right() - start;
                    ui.data_mut(|data| data.insert_temp(id, measured));
                });
                ui.add_space(20.0);
            });
        });
    }

    /// One view: its row of tabs, and the tab it shows. Returns true when
    /// the pointer was just pressed in it.
    fn view(
        &mut self,
        ui: &mut egui::Ui,
        side: Side,
        split: bool,
        interactive: bool,
        mut incoming: Option<&mut browser::Incoming>,
        actions: &mut Vec<Action>,
    ) -> bool {
        let p = self.palette;
        let area = ui.available_rect_before_wrap();
        let shown = self.shown_index(side);
        let bar = tabs::Bar {
            tabs: &self.tabs,
            side,
            shown,
            split,
            focused: self.views.focus == side,
            can_reopen: !self.closed.is_empty(),
        };
        tabs::show(ui, &p, &bar, actions);
        ui.add_space(6.0);

        let content = ui.available_rect_before_wrap();
        let editor = self.settings.editor.as_str();
        if let Some(incoming) = incoming.as_deref_mut() {
            incoming.in_use = self.views.focus == side;
        }
        if let Some(tab) = shown.and_then(|index| self.tabs.get_mut(index)) {
            if tab.panes == Panes::Monitor {
                let dashboard = crate::ui::monitor::Dashboard {
                    monitor: &self.monitor.monitor,
                    settings: &self.settings.monitor,
                    profiles: &self.profiles,
                };
                let state = &mut self.monitor.dash;
                ui.push_id(tab.id, |ui| crate::ui::monitor::show(ui, &p, &dashboard, state, actions));
            } else {
                // Each tab keeps its own scroll positions and the like.
                ui.push_id(tab.id, |ui| tab_content(ui, &p, tab, interactive, editor, incoming, actions));
            }
        }
        self.viewers_overlay(ui, shown, content, actions);

        // A tab dragged over the content goes to this view, or, while
        // there is only one, to a new view on the right half.
        if let Some(dragged) = egui::DragAndDrop::payload::<tabs::DraggedTab>(ui.ctx()) {
            let here = self.tabs.iter().filter(|tab| tab.side == side).count();
            let from_here = self.tabs.iter().any(|tab| tab.id == dragged.0 && tab.side == side);
            let zone = if split {
                (!from_here).then_some((content, side, tr!("Déplacer dans cette vue")))
            } else {
                (here > 1).then(|| {
                    let half = content.with_min_x(content.center().x);
                    (half, Side::Right, tr!("Ouvrir dans une vue à droite"))
                })
            };
            if let Some((zone, target, label)) = zone {
                let response = ui.interact(zone, ui.id().with(("tab-drop", side)), egui::Sense::hover());
                if response.dnd_hover_payload::<tabs::DraggedTab>().is_some() {
                    let layer = egui::LayerId::new(Order::Foreground, Id::new(("tab-drop", side)));
                    let painter = ui.ctx().layer_painter(layer);
                    painter.rect_filled(zone, 10, p.accent.gamma_multiply(0.14));
                    painter.rect_stroke(zone, 10, Stroke::new(1.5, p.accent), egui::StrokeKind::Inside);
                    painter.text(zone.center(), Align2::CENTER_CENTER, label, theme::semibold(15.0), p.text);
                }
                if let Some(dragged) = crate::ui::widgets::dropped::<tabs::DraggedTab>(&response) {
                    actions.push(Action::MoveTab(dragged.0, TabPlace::View(target)));
                }
            }
        }

        ui.rect_contains_pointer(area) && ui.input(|input| input.pointer.any_pressed())
    }

    fn toasts(&self, ctx: &egui::Context) {
        if self.toasts.is_empty() {
            return;
        }
        let p = &self.palette;
        egui::Area::new(Id::new("toasts"))
            .order(Order::Foreground)
            .anchor(Align2::RIGHT_BOTTOM, [-16.0, -44.0])
            .show(ctx, |ui| {
                // Newest at the bottom, each as wide as its own text.
                ui.with_layout(Layout::top_down(Align::Max), |ui| {
                    for toast in &self.toasts {
                        let (glyph, tint) = if toast.error {
                            (icon::WARNING_CIRCLE, p.danger)
                        } else {
                            (icon::CHECK_CIRCLE, p.success)
                        };
                        Frame::new()
                            .fill(p.surface)
                            .stroke(Stroke::new(1.0, if toast.error { p.danger } else { p.border }))
                            .corner_radius(10)
                            .inner_margin(Margin::symmetric(12, 10))
                            .show(ui, |ui| {
                                ui.set_max_width(420.0);
                                // Not `horizontal`: it would follow the
                                // right alignment of the column.
                                ui.with_layout(Layout::left_to_right(Align::Min), |ui| {
                                    ui.label(RichText::new(glyph).size(17.0).color(tint));
                                    ui.add(
                                        egui::Label::new(RichText::new(&toast.text).color(p.text))
                                            .wrap(),
                                    );
                                });
                            });
                    }
                });
            });
    }

    /// Dims the window around where dragged files would land, and says
    /// next to the pointer what dropping them does.
    fn drop_overlay(&self, ctx: &egui::Context, incoming: &browser::Incoming) {
        if ctx.input(|input| input.raw.hovered_files.is_empty()) {
            return;
        }
        // While another program drags, the window hears nothing of the
        // pointer: where it is gets looked at again and again.
        ctx.request_repaint_after(Duration::from_millis(30));

        let p = &self.palette;
        let window = ctx.content_rect();
        let (zone, text, taken) = match incoming.target.as_ref().or(incoming.fallback.as_ref()) {
            Some(target) => match &target.folder {
                Some(folder) => (target.rect, tr!("Importer dans {folder}", folder), true),
                None => (target.rect, closed_to_files(target).to_owned(), false),
            },
            None => (window, tr!("Affichez d'abord les fichiers d'une connexion").to_owned(), false),
        };
        let color = if taken { p.accent } else { p.danger };
        let painter = ctx.layer_painter(egui::LayerId::new(Order::Foreground, Id::new("drop")));
        dim_around(&painter, window, zone, p.bg.gamma_multiply(0.75));
        painter.rect_filled(zone, 8, color.gamma_multiply(0.12));
        painter.rect_stroke(zone, 8, Stroke::new(2.0, color), egui::StrokeKind::Inside);

        let galley = painter.layout_no_wrap(text, theme::semibold(14.0), p.text);
        let size = galley.size() + egui::vec2(46.0, 18.0);
        let wanted = if window.contains(incoming.pointer) {
            incoming.pointer + egui::vec2(18.0, 22.0)
        } else {
            zone.center() - size / 2.0
        };
        let bounds = window.shrink(8.0);
        let corner = egui::pos2(
            wanted.x.min(bounds.right() - size.x).max(bounds.left()),
            wanted.y.min(bounds.bottom() - size.y).max(bounds.top()),
        );
        let label = egui::Rect::from_min_size(corner, size);
        painter.rect(label, 8, p.surface, Stroke::new(1.0, color), egui::StrokeKind::Inside);
        painter.text(
            label.left_center() + egui::vec2(19.0, 0.0),
            Align2::CENTER_CENTER,
            if taken { icon::UPLOAD_SIMPLE } else { icon::PROHIBIT },
            egui::FontId::proportional(17.0),
            color,
        );
        painter.galley(label.min + egui::vec2(36.0, 9.0), galley, p.text);
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let now = ctx.input(|input| input.time);
        let mut actions = Vec::new();

        self.drain_events(&ctx, now);
        self.editor_frame(&ctx, now);
        self.file_ops_frame(&ctx, now);
        self.guard_close(&ctx);
        self.hosts_frame(&ctx, now);
        self.tick(&ctx, now);
        self.play_dev_script(&ctx, now);
        self.terminal_frame(&ctx, now);
        self.start_panes();
        self.tunnels_frame(&ctx, now);
        self.local_frame(&ctx);
        self.update_frame(&ctx, now);
        self.monitor_frame(&ctx, frame, now);
        if self.dialog.is_none() && self.quick_open.is_none() {
            self.editor_shortcuts(&ctx, &mut actions);
            self.viewer_shortcuts(&ctx, &mut actions);
            self.shortcuts(&ctx, &mut actions);
        }
        // Files from another program held over the window, or dropped on
        // it: each listing says whether they would land there.
        let mut incoming = (self.dialog.is_none()
            && self.quick_open.is_none()
            && ctx.input(|input| !input.raw.hovered_files.is_empty() || !input.raw.dropped_files.is_empty()))
        .then(|| {
            let pointer = match &self.dev_files {
                Some(files) => Some(files.pointer),
                None => os_drag::pointer(&ctx),
            };
            browser::Incoming::at(pointer.unwrap_or(egui::pos2(-1e6, -1e6)))
        });

        let p = self.palette;
        if self.settings.sidebar_visible {
            let shown = Panel::left("sidebar")
                .resizable(true)
                .default_size(self.settings.sidebar_width)
                .size_range(sidebar::MIN_WIDTH..=sidebar::MAX_WIDTH)
                .show_separator_line(false)
                .frame(Frame::new().fill(p.sidebar).inner_margin(Margin::same(10)))
                .show(ui, |ui| {
                    let open: Vec<&str> = self.tabs.iter().map(|tab| tab.profile_id.as_str()).collect();
                    let view = sidebar::View {
                        profiles: &self.profiles,
                        settings: &self.settings,
                        active: self.tabs.get(self.active).map(|tab| tab.profile_id.as_str()),
                        open: &open,
                        updater: &self.updater,
                    };
                    sidebar::show(ui, &p, &view, &mut self.sidebar, &mut actions);
                });
            // The width chosen by dragging the edge is kept for next time.
            let width = shown.response.rect.width();
            if (width - self.settings.sidebar_width).abs() > 0.5 && !ctx.input(|input| input.pointer.any_down()) {
                self.settings.sidebar_width = width;
                self.save_settings();
            }
        } else {
            Panel::left("rail")
                .exact_size(sidebar::RAIL_WIDTH)
                .resizable(false)
                .show_separator_line(false)
                .frame(Frame::new().fill(p.sidebar).inner_margin(Margin::same(8)))
                .show(ui, |ui| sidebar::rail(ui, &p, &self.updater, &self.settings, &mut actions));
        }

        Panel::bottom("status")
            .exact_size(30.0)
            .resizable(false)
            .frame(Frame::new().fill(p.bg).inner_margin(Margin::symmetric(14, 0)))
            .show(ui, |ui| self.status_bar(ui, &mut actions));

        if self.show_transfers {
            Panel::bottom("transfers")
                .resizable(true)
                .default_size(210.0)
                .size_range(120.0..=480.0)
                .frame(Frame::new().fill(p.bg).inner_margin(Margin::symmetric(14, 10)))
                .show(ui, |ui| {
                    let panel = transfers::Panel {
                        transfers: &self.transfers,
                        meters: &self.meters,
                        settings: &self.settings.transfers,
                        rules: self.worker.transfer_rules(),
                    };
                    transfers::show(ui, &p, &panel, &mut actions);
                });
        }

        let content = CentralPanel::default().frame(Frame::new().fill(p.bg).inner_margin(Margin {
            left: 14,
            right: 14,
            top: 8,
            bottom: 0,
        }));
        if self.tabs.is_empty() {
            content.show(ui, |ui| self.welcome(ui, &mut actions));
        } else {
            let interactive = self.dialog.is_none() && self.quick_open.is_none();
            let split = is_split(&self.tabs);
            // The view the pointer was pressed in becomes the one in use,
            // before what was clicked there is applied.
            let mut pressed = None;
            content.show(ui, |ui| {
                if !split {
                    self.view(ui, Side::Left, split, interactive, incoming.as_mut(), &mut actions);
                    return;
                }
                let width = ui.available_width();
                Panel::right("right-view")
                    .resizable(true)
                    .default_size((width / 2.0).round())
                    .size_range(MIN_VIEW_WIDTH..=(width - MIN_VIEW_WIDTH).max(MIN_VIEW_WIDTH))
                    .frame(Frame::new().fill(p.bg).inner_margin(Margin {
                        left: 12,
                        ..Margin::ZERO
                    }))
                    .show(ui, |ui| {
                        if self.view(ui, Side::Right, split, interactive, incoming.as_mut(), &mut actions) {
                            pressed = Some(Side::Right);
                        }
                    });
                let left = Frame::new().inner_margin(Margin {
                    right: 12,
                    ..Margin::ZERO
                });
                left.show(ui, |ui| {
                    if self.view(ui, Side::Left, split, interactive, incoming.as_mut(), &mut actions) {
                        pressed = Some(Side::Left);
                    }
                });
            });
            if let Some(side) = pressed
                && side != self.views.focus
                && self.views.shown(side).is_some()
            {
                self.views.focus = side;
                self.settle();
            }
        }

        self.dialogs(&ctx, now);
        self.ssh_auth_frame(&ctx, now);
        self.s3_requests();
        if let Some(mut palette) = self.quick_open.take() {
            let open: Vec<&str> = self.tabs.iter().map(|tab| tab.profile_id.as_str()).collect();
            if quick_open::show(&ctx, &p, &mut palette, &self.profiles, &open, &mut actions) {
                self.quick_open = Some(palette);
            }
        }
        self.toasts(&ctx);
        if let Some(incoming) = incoming {
            self.drop_overlay(&ctx, &incoming);
            self.dropped_files(&ctx, now, incoming);
        }
        self.transfer_frame(&ctx, now);

        for action in actions {
            self.apply(action, &ctx, now);
        }
        self.ssh_power_frame(&ctx, now);
        self.editor_frame_end();
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        raw_input.events.append(&mut self.dev_input);
        if std::mem::take(&mut self.release_pointer) {
            let pos = ctx.input(|input| input.pointer.latest_pos()).unwrap_or_default();
            raw_input.events.push(egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: ctx.input(|input| input.modifiers),
            });
            raw_input.events.push(egui::Event::PointerGone);
        }
        if let Some(files) = &mut self.dev_files {
            if !files.drop {
                raw_input.hovered_files.push(egui::HoveredFile {
                    path: Some(files.path.clone()),
                    ..Default::default()
                });
            } else if !files.delivered {
                files.delivered = true;
                raw_input.dropped_files.push(Arc::new(dev::DroppedPath(files.path.clone())));
            }
        }
        // Off screen the window never gets the focus, without which egui
        // gives no widget the keyboard: scripts type as in a focused window.
        raw_input.focused |= dev::offscreen();
    }

    /// The tabs open now are the ones reopened next time, by the new
    /// version too when an update is put in place on the way out.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_tabs_now();
        self.update_on_exit();
    }
}

/// Why files from another program cannot land on a listing: that of the
/// buckets, or of the drives of this computer.
fn closed_to_files(target: &browser::DropTarget) -> &'static str {
    if target.storage.local().is_some() {
        tr!("Ouvrez d'abord un dossier pour y importer des fichiers")
    } else {
        tr!("Ouvrez d'abord un bucket pour y importer des fichiers")
    }
}

/// The toast, or the line of a form, saying that saving failed.
fn save_failed(error: &anyhow::Error) -> String {
    let error = format!("{error:#}");
    tr!("Enregistrement impossible : {error}", error)
}

/// Paints `shade` over `window`, except over `hole`.
fn dim_around(painter: &egui::Painter, window: egui::Rect, hole: egui::Rect, shade: egui::Color32) {
    let hole = hole.intersect(window);
    let parts = [
        egui::Rect::from_min_max(window.min, egui::pos2(window.max.x, hole.min.y)),
        egui::Rect::from_min_max(egui::pos2(window.min.x, hole.max.y), window.max),
        egui::Rect::from_min_max(egui::pos2(window.min.x, hole.min.y), egui::pos2(hole.min.x, hole.max.y)),
        egui::Rect::from_min_max(egui::pos2(hole.max.x, hole.min.y), egui::pos2(window.max.x, hole.max.y)),
    ];
    for part in parts {
        if part.is_positive() {
            painter.rect_filled(part, 0, shade);
        }
    }
}

/// One favourite of the start page. Returns true when clicked.
fn favorite_card(ui: &mut egui::Ui, p: &Palette, rect: egui::Rect, profile: &Profile) -> bool {
    let response = ui
        .interact(rect, Id::new(("favorite-card", &profile.id)), egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let fill = if response.hovered() { p.surface_hover } else { p.surface };
    ui.painter().rect_filled(rect, 10, fill);
    ui.painter().text(
        rect.left_center() + egui::vec2(24.0, 0.0),
        Align2::CENTER_CENTER,
        protocol_icon(profile.protocol),
        egui::FontId::proportional(20.0),
        crate::ui::hosts::icon_color(p, profile, true),
    );
    let text = egui::Rect::from_min_max(rect.min + egui::vec2(46.0, 8.0), rect.max - egui::vec2(10.0, 8.0));
    let mut lines = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(text)
            .layout(Layout::top_down(Align::Min)),
    );
    lines.spacing_mut().item_spacing.y = 0.0;
    lines.add(egui::Label::new(RichText::new(profile.title()).color(p.text)).truncate());
    lines.add(egui::Label::new(RichText::new(profile.summary()).size(11.5).color(p.muted)).truncate());
    response.clicked()
}

/// The panes of a tab: its files, its terminal, or one above the other.
fn tab_content(
    ui: &mut egui::Ui,
    p: &Palette,
    tab: &mut Tab,
    interactive: bool,
    editor: &str,
    incoming: Option<&mut browser::Incoming>,
    actions: &mut Vec<Action>,
) {
    let has_terminal = tab.has_shell();
    let shell = |ui: &mut egui::Ui, tab: &mut Tab, actions: &mut Vec<Action>| {
        crate::ui::ssh_power::terminal_header(ui, p, tab, actions);
        if let Some(shell) = &mut tab.terminal {
            let area = ui.available_rect_before_wrap();
            terminal::show(ui, p, shell, interactive, actions);
            crate::ui::hosts::terminal_border(ui, p, &tab.profile_id, area);
        }
    };
    let gap = Frame::new().inner_margin(Margin {
        bottom: 8,
        ..Margin::ZERO
    });

    match tab.panes {
        Panes::Editor => gap.show(ui, |ui| crate::ui::editor::show(ui, p, tab, interactive, actions)).inner,
        // Drawn by `App::view`, which has what it shows.
        Panes::Monitor => {}
        Panes::Files => {
            crate::ui::viewers::panel(ui, p, tab, actions);
            browser::show(ui, p, &mut tab.session, has_terminal, editor, incoming, actions);
        }
        Panes::Terminal => {
            gap.show(ui, |ui| shell(ui, tab, actions));
        }
        Panes::Both => {
            let height = ui.available_height();
            Panel::bottom(Id::new(("terminal", tab.id)))
                .resizable(true)
                .default_size((height * 0.45).max(120.0))
                .size_range(90.0..=(height - 230.0).max(90.0))
                .frame(Frame::new().fill(p.bg).inner_margin(Margin {
                    top: 8,
                    bottom: 8,
                    ..Margin::ZERO
                }))
                .show(ui, |ui| shell(ui, tab, actions));
            crate::ui::viewers::panel(ui, p, tab, actions);
            browser::show(ui, p, &mut tab.session, has_terminal, editor, incoming, actions);
        }
    }
}
