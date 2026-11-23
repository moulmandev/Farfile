//! The glue between the monitoring ([`crate::monitor`]) and the app: which
//! connections are watched, the dashboard's tab, alerts turned into
//! messages, the window closed to the notification area, and the script
//! steps that drive them.

use std::path::PathBuf;
use std::sync::Arc;

use super::App;
use crate::dev;
use crate::monitor::alerts::{Change, Level, Metric};
use crate::monitor::{self, Monitor, notify};
use crate::profiles::{self, Panes, Profile, Protocol};
use crate::session::Session;
use crate::storage::ssh::SshSettings;
use crate::tabs::{Side, Tab};
use crate::ui::dialogs::Dialog;
use crate::ui::monitor::{CollectorForm, MonitorAction, MonitorDialog, PickForm, Published, SettingsForm, publish};

pub(super) struct MonitorState {
    pub(super) monitor: Monitor,
    /// What the dashboard remembers between frames: the server shown in
    /// full, the panel of the widgets, the servers hidden from a chart.
    pub(super) dash: crate::ui::monitor::DashState,
    /// The window's handle went to the notifications.
    window_known: bool,
    /// The icon of the notification area, while the window is closed to it.
    #[cfg(windows)]
    tray: Option<crate::monitor::tray::Tray>,
    #[cfg(windows)]
    tray_commands: (
        std::sync::mpsc::Sender<crate::monitor::tray::TrayCommand>,
        std::sync::mpsc::Receiver<crate::monitor::tray::TrayCommand>,
    ),
    /// The user asked to leave from the notification area: closing the
    /// window quits for real.
    quit: bool,
}

impl MonitorState {
    pub(super) fn new(ctx: &egui::Context, settings: &monitor::MonitorSettings) -> Self {
        Self {
            monitor: Monitor::new(ctx, settings),
            dash: Default::default(),
            window_known: false,
            #[cfg(windows)]
            tray: None,
            #[cfg(windows)]
            tray_commands: std::sync::mpsc::channel(),
            quit: false,
        }
    }
}

/// What the connection of a watch is made of: a change starts it again.
fn fingerprint(profile: &Profile) -> String {
    format!(
        "{}|{}|{}|{}|{:?}|{}|{}",
        profile.endpoint, profile.port, profile.user, profile.key_path, profile.auth, profile.jump, profile.identity
    )
}

impl App {
    /// A run driven by a script never watches the user's own servers: it
    /// starts with nothing watched and never closes to the notification
    /// area.
    pub(super) fn monitor_settings_for_scripts(&mut self) {
        if dev::active() {
            let settings = &mut self.settings.monitor;
            settings.watched.clear();
            settings.overrides.clear();
            settings.background = false;
        }
    }

    /// The settings of the SSH connection of a watched server, as its tab
    /// would have them.
    fn watch_settings(&self, profile: &Profile) -> Arc<SshSettings> {
        let secret = if super::is_dev(&profile.id) || dev::active() {
            self.dev_secrets.get(&profile.id).cloned().unwrap_or_default()
        } else {
            // A password can be empty: keys, agents.
            profiles::secret(&profile.id).unwrap_or_default()
        };
        let known_hosts = if super::is_dev(&profile.id) {
            None
        } else {
            Some(profiles::known_hosts_path().unwrap_or_else(|| PathBuf::from("known_hosts")))
        };
        let (profile, secret, known_hosts, extra) = self.ssh_route(profile.clone(), secret, known_hosts);
        let key_path = profile.key_path.trim();
        Arc::new(SshSettings {
            host: profile.host().to_owned(),
            port: profile.port(),
            user: profile.user.trim().to_owned(),
            secret,
            key_path: (!key_path.is_empty()).then(|| PathBuf::from(key_path)),
            known_hosts,
            extra,
        })
    }

    /// Once per frame: the watches follow the preferences, alerts become
    /// messages, the icon of the notification area answers.
    pub(super) fn monitor_frame(&mut self, ctx: &egui::Context, frame: &eframe::Frame, now: f64) {
        if !self.monitor.window_known {
            self.monitor.window_known = true;
            if let Some(handle) = window_handle(frame) {
                notify::set_window(handle);
            }
        }

        // Start, refresh or stop the watches.
        let mut watched: Vec<String> = Vec::new();
        for id in &self.settings.monitor.watched {
            let Some(profile) = self.profiles.iter().find(|profile| profile.id == *id) else {
                continue;
            };
            if profile.protocol != Protocol::Sftp {
                continue;
            }
            watched.push(id.clone());
            let rules = self.settings.monitor.rules_for(id);
            let print = fingerprint(profile);
            if !self.monitor.monitor.refresh(id, profile.title(), &print, rules) {
                let settings = self.watch_settings(profile);
                let rules = rules.clone();
                let (name, id) = (profile.title().to_owned(), id.clone());
                self.monitor.monitor.start(&self.worker, &id, &name, print, &rules, settings);
            }
        }
        self.monitor.monitor.keep_only(&watched);

        // Alerts, in a corner of the window.
        let in_app = self.settings.monitor.delivery.in_app;
        // Alerts that came while the window was hidden are in the journal;
        // only the fresh ones pop up.
        let fresh = chrono::Local::now() - chrono::Duration::minutes(2);
        for alert in self.monitor.monitor.take_alerts() {
            if in_app && alert.told && alert.at > fresh {
                let text = format!("{} — {}", alert.name, alert.text());
                // Red for what cannot wait; warnings and good news plainly.
                self.toast(now, text, alert.level() == Some(Level::Critical));
            }
        }
        // How the jobs of the collectors ended.
        for (text, failed) in self.monitor.monitor.take_notes() {
            self.toast(now, text, failed);
        }
        self.refresh_collector_dialog();
        if notify::take_open_request() {
            self.open_dashboard();
        }
        self.tray_frame(ctx);

        let (critical, warning) = self.monitor.monitor.counts();
        publish(ctx, Published { watched, critical, warning });
    }

    pub(super) fn apply_monitor(&mut self, action: MonitorAction, now: f64) {
        match action {
            MonitorAction::Open => self.open_dashboard(),
            MonitorAction::Pick => {
                let form = PickForm::new(&self.profiles, &self.settings.monitor.watched);
                self.open_dialog(Dialog::Monitor(MonitorDialog::Pick(form)));
            }
            MonitorAction::Settings(scope) => {
                let form = SettingsForm::new(&self.settings.monitor, &self.profiles, scope);
                self.open_dialog(Dialog::Monitor(MonitorDialog::Settings(Box::new(form))));
            }
            MonitorAction::Watch(id, watch) => {
                self.settings.monitor.set_watched(&id, watch);
                self.save_settings();
                if watch {
                    let name = self.profiles.iter().find(|profile| profile.id == id).map(|profile| profile.title());
                    let text = tr!(
                        "« {name} » est surveillée : retrouvez-la dans Surveillance, en haut de la liste.",
                        name = name.unwrap_or_default()
                    );
                    self.toast(now, text, false);
                }
            }
            MonitorAction::Retry(id) => self.monitor.monitor.restart(&id),
            MonitorAction::Collector(id) => {
                let Some(profile) = self.profiles.iter().find(|profile| profile.id == id) else {
                    return;
                };
                let form = CollectorForm::new(&id, profile.title(), profile.user.trim());
                self.open_dialog(Dialog::Monitor(MonitorDialog::Collector(Box::new(form))));
                self.refresh_collector_dialog();
            }
            MonitorAction::ClearJournal => self.monitor.monitor.journal.clear(),
            MonitorAction::Prefs(prefs) => {
                self.settings.monitor.dashboard = *prefs;
                self.save_settings();
            }
        }
    }

    pub(super) fn monitor_dialog_done(&mut self, dialog: MonitorDialog) {
        match dialog {
            MonitorDialog::Pick(form) => {
                if form.rows.is_empty() {
                    return;
                }
                let chosen = form.chosen(&self.settings.monitor.watched);
                let monitor = &mut self.settings.monitor;
                monitor.overrides.retain(|id, _| chosen.contains(id));
                monitor.watched = chosen;
                self.save_settings();
                self.open_dashboard();
            }
            MonitorDialog::Settings(form) => {
                self.settings.monitor = form.draft;
                self.monitor.monitor.apply(&self.settings.monitor);
                self.save_settings();
            }
            MonitorDialog::Collector(form) => {
                if let Some(job) = form.choice {
                    self.monitor.monitor.collector(&form.id, job);
                }
            }
        }
    }

    /// The dialog of a collector follows what its watch learns.
    fn refresh_collector_dialog(&mut self) {
        let Some(Dialog::Monitor(MonitorDialog::Collector(form))) = &mut self.dialog else {
            return;
        };
        let Some(host) = self.monitor.monitor.host(&form.id) else {
            form.ready = false;
            return;
        };
        host.read(|state| {
            form.collector.clone_from(&state.collector);
            let disks = state.view.sample.as_ref().map(|sample| sample.disks.as_slice());
            form.ready = disks.is_some();
            let script = crate::monitor::collector::script(&crate::monitor::collector::mounts(disks.unwrap_or_default()));
            if form.script != script {
                form.script = script;
            }
        });
    }

    /// A connection was saved: its watch starts again, with what changed
    /// (a new password does not change what [`fingerprint`] sees).
    pub(super) fn monitor_profile_saved(&mut self, id: &str) {
        self.monitor.monitor.restart(id);
    }

    /// The tab of the dashboard; nothing to connect.
    pub(super) fn dashboard_tab(&mut self) -> Tab {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let title = tr!("Surveillance").to_owned();
        Tab {
            id,
            side: Side::Left,
            profile_id: monitor::ID.to_owned(),
            title: title.clone(),
            protocol: Protocol::Local,
            session: Session::new(title, crate::storage::local::shared(), None),
            ssh: None,
            terminal: None,
            panes: Panes::Monitor,
            power: Default::default(),
            viewers: crate::viewers::Viewers::new(&self.worker),
            local_shell: None,
            editor: None,
        }
    }

    /// Shows the dashboard, opening its tab when there is none.
    pub(super) fn open_dashboard(&mut self) {
        match self.tabs.iter().find(|tab| tab.panes == Panes::Monitor) {
            Some(tab) => {
                let id = tab.id;
                self.views.focus = tab.side;
                self.views.show(&self.tabs, id);
                self.settle();
            }
            None => {
                let tab = self.dashboard_tab();
                self.add_tab(tab, None);
            }
        }
    }

    /// Closing the window while servers are watched and the preferences ask
    /// for it: the window goes, the monitoring stays. True when it did.
    pub(super) fn close_to_background(&mut self, ctx: &egui::Context) -> bool {
        // Leaving on purpose, or to put a new version in place, still leaves.
        let leaving = self.monitor.quit || self.quitting || self.updater.restart_on_exit;
        if !self.settings.monitor.background || leaving || self.monitor.monitor.is_empty() {
            return false;
        }
        #[cfg(windows)]
        {
            if self.monitor.tray.is_none() {
                let labels = crate::monitor::tray::Labels {
                    tip: tr!("Farfile : surveillance en cours").to_owned(),
                    open: tr!("Ouvrir Farfile").to_owned(),
                    quit: tr!("Quitter Farfile").to_owned(),
                };
                let commands = self.monitor.tray_commands.0.clone();
                self.monitor.tray = crate::monitor::tray::Tray::show(labels, commands, ctx.clone());
            }
            // Without an icon there would be no way back: quit instead.
            if self.monitor.tray.is_none() || !notify::hide_window() {
                self.monitor.tray = None;
                return false;
            }
        }
        #[cfg(not(windows))]
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        true
    }

    /// The icon of the notification area: back to the window, or quit.
    fn tray_frame(&mut self, ctx: &egui::Context) {
        #[cfg(windows)]
        {
            use crate::monitor::tray::TrayCommand;
            let commands: Vec<TrayCommand> = self.monitor.tray_commands.1.try_iter().collect();
            for command in commands {
                self.monitor.tray = None;
                if command == TrayCommand::Quit {
                    self.monitor.quit = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
            // Back on screen another way (a notification clicked, Farfile
            // started again): the icon has done its job.
            if self.monitor.tray.is_some() && notify::window_visible() {
                self.monitor.tray = None;
            }
        }
        #[cfg(not(windows))]
        let _ = ctx;
    }

    /// Script steps of the monitoring; see the list in `run_dev_step`.
    pub(super) fn dev_step_monitor(&mut self, verb: &str, argument: &str, now: f64) -> bool {
        let named = |app: &Self, name: &str| app.profile_named(name).map(|profile| profile.id.clone());
        match verb {
            "monitor" => self.open_dashboard(),
            "watch" | "unwatch" => match named(self, argument) {
                Some(id) => self.apply_monitor(MonitorAction::Watch(id, verb == "watch"), now),
                None => self.toast(now, format!("no connection named {argument}"), true),
            },
            "monitorpick" => self.apply_monitor(MonitorAction::Pick, now),
            "monitortick" => {
                if let Some(Dialog::Monitor(MonitorDialog::Pick(form))) = &mut self.dialog {
                    form.tick(argument, true);
                }
            }
            "monitorsettings" => {
                let scope = (!argument.is_empty()).then(|| named(self, argument)).flatten();
                self.apply_monitor(MonitorAction::Settings(scope), now);
            }
            // `monitorrule disk 15` sets the warning threshold of every
            // server, so that real readings raise a real alert.
            "monitorrule" => {
                let (metric, value) = argument.split_once(' ').unwrap_or((argument, ""));
                let value: f32 = value.trim().parse().unwrap_or(85.0);
                let rules = &mut self.settings.monitor.rules;
                let threshold = match metric {
                    "disk" => &mut rules.disk,
                    "memory" => &mut rules.memory,
                    "cpu" => &mut rules.cpu,
                    "load" => &mut rules.load,
                    _ => return true,
                };
                threshold.warning = value;
                threshold.minutes = 0;
            }
            // `monitorview list`, `heat`, `charts`, `wall` or `cards`.
            "monitorview" => {
                use crate::monitor::dashboard::View;
                let view = match argument {
                    "list" => View::List,
                    "heat" => View::Heat,
                    "charts" => View::Charts,
                    "wall" => View::Wall,
                    _ => View::Cards,
                };
                self.settings.monitor.dashboard.view = view;
                self.monitor.dash.detail = None;
            }
            // `monitorrange 30m`, `6h`, `24h` or `7d`; `monitormeasure cpu`,
            // `memory`, `disk`, `load` or `network`.
            "monitorrange" | "monitormeasure" => {
                let prefs = &mut self.settings.monitor.dashboard;
                let quoted = format!("\"{argument}\"");
                if verb == "monitorrange" {
                    if let Ok(range) = serde_json::from_str(&quoted) {
                        prefs.range = range;
                    }
                } else if let Ok(measure) = serde_json::from_str(&quoted) {
                    prefs.measure = measure;
                }
            }
            "monitordetail" => self.monitor.dash.detail = named(self, argument),
            // `monitorcollector NAME` opens the dialog of its collector;
            // `monitorcollect NAME install` or `remove` does the job.
            "monitorcollector" => {
                if let Some(id) = named(self, argument) {
                    self.apply_monitor(MonitorAction::Collector(id), now);
                }
            }
            "monitorcollect" => {
                use crate::monitor::collector::Job;
                let (name, job) = argument.rsplit_once(' ').unwrap_or((argument, ""));
                let job = if job == "remove" { Job::Remove } else { Job::Install };
                if let Some(id) = named(self, name) {
                    self.monitor.monitor.collector(&id, job);
                }
            }
            "monitorcustomize" => self.monitor.dash.customizing = !self.monitor.dash.customizing,
            // `monitorhistory NAME`: a made-up week behind it.
            "monitorhistory" => {
                if let Some(id) = named(self, argument) {
                    let seed = id.bytes().map(u64::from).sum();
                    self.monitor.monitor.simulate_history(&id, seed);
                }
            }
            // `monitorstyle cpu ring`: a widget of the cards, in a style.
            "monitorstyle" => {
                let words: Vec<&str> = argument.split_whitespace().collect();
                if let [widget, style] = words[..] {
                    let parse = |word: &str| format!("\"{word}\"");
                    if let (Ok(widget), Ok(style)) = (serde_json::from_str(&parse(widget)), serde_json::from_str(&parse(style))) {
                        let prefs = &mut self.settings.monitor.dashboard;
                        if let Some(slot) = prefs.slots.iter_mut().find(|slot| slot.widget == widget) {
                            slot.style = style;
                            slot.on = true;
                        }
                        prefs.normalize();
                    }
                }
            }
            "monitoralerts" => {
                if let Some(Dialog::Monitor(MonitorDialog::Settings(form))) = &mut self.dialog {
                    form.show_alerts();
                }
            }
            // `monitoralert NAME disk / 92` raises an alert as the server
            // would; `monitoralert NAME disk / ok` ends it.
            "monitoralert" => {
                let words: Vec<&str> = argument.split_whitespace().collect();
                let Some((name, rest)) = words.split_first() else {
                    return true;
                };
                let Some(id) = named(self, name) else {
                    return true;
                };
                let (metric, rest) = match rest {
                    ["disk", mount, rest @ ..] => (Metric::Disk((*mount).to_owned()), rest),
                    ["memory", rest @ ..] => (Metric::Memory, rest),
                    ["cpu", rest @ ..] => (Metric::Cpu, rest),
                    ["load", rest @ ..] => (Metric::Load, rest),
                    ["offline", rest @ ..] => (Metric::Offline, rest),
                    _ => return true,
                };
                let (to, value) = match rest.first().copied() {
                    Some("ok") | None => (None, 50.0),
                    Some(value) => {
                        let value: f32 = value.parse().unwrap_or(0.0);
                        let threshold = self.settings.monitor.rules_for(&id).threshold(&metric).copied();
                        let critical = threshold.is_none_or(|threshold| value >= threshold.limit(Level::Critical));
                        (Some(if critical { Level::Critical } else { Level::Warning }), value)
                    }
                };
                let from = if to.is_none() { Some(Level::Warning) } else { None };
                self.monitor.monitor.simulate(&id, Change { metric, from, to, value });
            }
            _ => return false,
        }
        true
    }
}

/// The handle of the window, as the system knows it.
fn window_handle(frame: &eframe::Frame) -> Option<isize> {
    #[cfg(windows)]
    {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        match frame.window_handle().ok()?.as_raw() {
            RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
            _ => None,
        }
    }
    #[cfg(not(windows))]
    {
        let _ = frame;
        None
    }
}
