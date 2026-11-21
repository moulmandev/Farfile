//! The tunnels in the app: started and stopped as the user asks, when the
//! tabs of their connection open, and kept in line with the saved rules.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use super::App;
use crate::profiles::{self, Profile};
use crate::storage::ssh::SshSettings;
use crate::tunnels::{DEFAULT_BIND, ForwardRule, Tunnels};
use crate::ui::Action;
use crate::ui::dialogs::Dialog;
use crate::ui::tunnels::{self as view, MenuState, Overview, RuleDialog, TunnelAction};

#[derive(Default)]
pub struct State {
    pub manager: Tunnels,
    /// The tabs whose connection opened already: their rules that start on
    /// their own were started then.
    opened_tabs: HashSet<u64>,
}

/// What the tunnels of a connection depend on: when it changes, they
/// connect again with the new settings.
fn fingerprint(profile: &Profile) -> String {
    format!(
        "{}\n{}\n{}\n{}",
        profile.host(),
        profile.port(),
        profile.user.trim(),
        profile.key_path.trim()
    )
}

impl App {
    fn profile(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|profile| profile.id == id)
    }

    /// The SSH settings a tab of this connection would use.
    fn tunnel_settings(&mut self, profile_id: &str, now: f64) -> Option<Arc<SshSettings>> {
        self.build_tab(profile_id, now).and_then(|tab| tab.ssh)
    }

    /// Starts these rules of the connection, opening the SSH connection of
    /// its tunnels if needed. Rules already running restart.
    fn start_tunnels(&mut self, profile_id: &str, rules: Vec<ForwardRule>, now: f64) {
        if rules.is_empty() {
            return;
        }
        if self.tunnels.manager.host(profile_id).is_none() {
            let Some(profile) = self.profile(profile_id).filter(|profile| profile.has_terminal()) else {
                return;
            };
            let print = fingerprint(profile);
            let Some(settings) = self.tunnel_settings(profile_id, now) else {
                return;
            };
            self.tunnels.manager.open(&self.worker, profile_id, settings, print);
        }
        for rule in rules {
            self.tunnels.manager.start(profile_id, rule);
        }
    }

    pub(super) fn apply_tunnels(&mut self, action: TunnelAction, ctx: &egui::Context, now: f64) {
        match action {
            TunnelAction::StartAll(id) => {
                let Some(profile) = self.profile(&id) else {
                    return;
                };
                if profile.forwards.is_empty() {
                    return self.apply_tunnels(TunnelAction::AskAdd(id), ctx, now);
                }
                let idle: Vec<ForwardRule> = profile
                    .forwards
                    .iter()
                    .filter(|rule| !self.tunnels.manager.is_running(&id, &rule.id))
                    .cloned()
                    .collect();
                self.start_tunnels(&id, idle, now);
                // Their state is the answer to the request.
                view::unfold(ctx);
            }
            TunnelAction::Start(id, rule_id) => {
                let rule = self
                    .profile(&id)
                    .and_then(|profile| profile.forwards.iter().find(|rule| rule.id == rule_id))
                    .cloned();
                if let Some(rule) = rule {
                    self.start_tunnels(&id, vec![rule], now);
                }
            }
            TunnelAction::Stop(id, rule_id) => self.tunnels.manager.stop(&id, &rule_id),
            TunnelAction::StopAll(id) => self.tunnels.manager.stop_all(&id),
            TunnelAction::Reconnect(id) => {
                // Read again: the password may have changed since.
                let settings = self.tunnel_settings(&id, now);
                self.tunnels.manager.reconnect(&id, settings);
            }
            TunnelAction::AskAdd(id) => {
                if let Some(profile) = self.profile(&id) {
                    let dialog = RuleDialog::new(profile, None);
                    self.open_tunnel_dialog(dialog, ctx);
                }
            }
            TunnelAction::AskEdit(id, rule_id) => {
                let Some(profile) = self.profile(&id) else {
                    return;
                };
                if let Some(rule) = profile.forwards.iter().find(|rule| rule.id == rule_id).cloned() {
                    let dialog = RuleDialog::new(profile, Some(rule));
                    self.open_tunnel_dialog(dialog, ctx);
                }
            }
            TunnelAction::ShowList => view::unfold(ctx),
        }
    }

    /// The list or menu it was asked from would stay drawn above it.
    fn open_tunnel_dialog(&mut self, dialog: RuleDialog, ctx: &egui::Context) {
        egui::Popup::close_all(ctx);
        self.open_dialog(Dialog::Tunnel(Box::new(dialog)));
    }

    /// Saves the rule of the dialog with its connection, and starts it
    /// when asked to. A running rule that changed restarts on its own.
    pub(super) fn tunnel_dialog_done(&mut self, form: RuleDialog, start: bool, ctx: &egui::Context, now: f64) {
        let rule = form.editor.rule();
        let Some(profile) = self.profiles.iter_mut().find(|profile| profile.id == form.profile_id) else {
            return;
        };
        match profile.forwards.iter_mut().find(|saved| saved.id == rule.id) {
            Some(saved) => *saved = rule.clone(),
            None => profile.forwards.push(rule.clone()),
        }
        let saved = self.persist_profiles();
        self.report_save_error(saved, now);
        if start {
            self.start_tunnels(&form.profile_id, vec![rule], now);
            view::unfold(ctx);
        }
    }

    /// Once a frame: starts what tabs that just connected start, follows
    /// the saved rules, and tells the menus what runs.
    pub(super) fn tunnels_frame(&mut self, ctx: &egui::Context, now: f64) {
        self.auto_start_tunnels(now);
        self.follow_saved_rules(now);

        let mut menus = MenuState::default();
        for profile in self.profiles.iter().filter(|profile| profile.has_terminal()) {
            let rules = profile
                .forwards
                .iter()
                .map(|rule| (rule.clone(), self.tunnels.manager.is_running(&profile.id, &rule.id)))
                .collect();
            menus.rules.insert(profile.id.clone(), rules);
        }
        view::publish(ctx, menus);

        if self.tunnels.manager.counting_down() {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
    }

    /// The rules marked to start with the connection start once a tab of
    /// it has connected (the files listed or the shell opened).
    fn auto_start_tunnels(&mut self, now: f64) {
        let tabs = &self.tabs;
        let opened_tabs = &mut self.tunnels.opened_tabs;
        opened_tabs.retain(|id| tabs.iter().any(|tab| tab.id == *id));
        let connected: Vec<String> = tabs
            .iter()
            .filter(|tab| tab.ssh.is_some() && (tab.session.started || tab.terminal.is_some()))
            .filter(|tab| opened_tabs.insert(tab.id))
            .map(|tab| tab.profile_id.clone())
            .collect();
        for id in connected {
            let Some(profile) = self.profile(&id) else {
                continue;
            };
            let rules: Vec<ForwardRule> = profile
                .forwards
                .iter()
                .filter(|rule| rule.auto_start && !self.tunnels.manager.is_running(&id, &rule.id))
                .cloned()
                .collect();
            self.start_tunnels(&id, rules, now);
        }
    }

    /// Running tunnels follow their saved rule: a rule removed stops, one
    /// changed restarts, and a connection whose settings changed connects
    /// again. Deleting the connection stops its tunnels.
    fn follow_saved_rules(&mut self, now: f64) {
        let hosts: Vec<(String, String, Vec<ForwardRule>)> = self
            .tunnels
            .manager
            .hosts()
            .iter()
            .map(|host| (host.profile_id.clone(), host.fingerprint.clone(), host.rules()))
            .collect();
        for (id, print, running) in hosts {
            let Some(profile) = self.profile(&id).filter(|profile| profile.has_terminal()) else {
                self.tunnels.manager.stop_all(&id);
                continue;
            };
            let saved: Vec<ForwardRule> = profile.forwards.clone();
            if fingerprint(profile) != print {
                let kept = saved
                    .into_iter()
                    .filter(|rule| running.iter().any(|run| run.id == rule.id))
                    .collect();
                self.tunnels.manager.stop_all(&id);
                self.start_tunnels(&id, kept, now);
                continue;
            }
            for run in running {
                match saved.iter().find(|rule| rule.id == run.id) {
                    None => self.tunnels.manager.stop(&id, &run.id),
                    Some(rule) if !rule.same_tunnel(&run) => {
                        self.tunnels.manager.start(&id, rule.clone());
                    }
                    Some(rule) if rule.name != run.name || rule.auto_start != run.auto_start => {
                        self.tunnels.manager.rename(&id, rule);
                    }
                    Some(_) => {}
                }
            }
        }
    }

    /// The button of the status bar and its list: the connections with
    /// tunnels running first, then the others that have rules.
    pub(super) fn tunnels_status(&self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let manager = &self.tunnels.manager;
        let mut shown: Vec<&Profile> = manager
            .hosts()
            .iter()
            .filter_map(|host| self.profile(&host.profile_id))
            .collect();
        let mut idle: Vec<&Profile> = self
            .profiles
            .iter()
            .filter(|profile| profile.has_terminal() && !profile.forwards.is_empty())
            .filter(|profile| manager.host(&profile.id).is_none())
            .collect();
        idle.sort_by_cached_key(|profile| profiles::sort_key(profile.title()));
        shown.extend(idle);
        let overview = Overview {
            tunnels: manager,
            profiles: shown,
        };
        view::status(ui, &self.palette, &overview, actions);
    }

    /// Script steps of the tunnels. Connections are numbered as for
    /// `connect`:
    ///
    /// - `tunneladd 1 L 15020 web:80 > Site` adds a rule (`R`, `D`; the
    ///   address to listen on may be `0.0.0.0:2242`; `auto` after the
    ///   destination starts it with the connection), in memory only;
    /// - `tunnelstart 1` starts them all, `tunnelstart 1 Site` one of them,
    ///   `tunnelstop 1 [Site]`, `tunnelonly 1` as the sidebar's menu does,
    ///   `tunnelreconnect 1`;
    /// - `tunnels` unfolds the list of the status bar;
    /// - `tunnelnew 1`, `tunneledit 1 Site` open the dialog of a rule;
    ///   `tunnelform` (or `tunnelform 0`, a rule's index) opens the editor
    ///   in the connection form; `tunnelset dest web:80` fills a field of
    ///   the open editor (`bind`, `dest`, `name`, `kind D`, `auto`), and
    ///   `tunnelformok` closes the form's editor, keeping the rule.
    pub(super) fn dev_step_tunnels(&mut self, verb: &str, argument: &str, ctx: &egui::Context, now: f64) -> bool {
        let (target, rest) = argument.split_once(' ').unwrap_or((argument, ""));
        let id = format!("{}{}", crate::dev::PROFILE_PREFIX, target.trim());
        let rest = rest.trim();
        let named = |app: &App| -> Vec<String> {
            app.profile(&id)
                .map(|profile| {
                    profile
                        .forwards
                        .iter()
                        .filter(|rule| rest.is_empty() || rule.title() == rest)
                        .map(|rule| rule.id.clone())
                        .collect()
                })
                .unwrap_or_default()
        };
        match verb {
            "tunneladd" => {
                if let Some(rule) = rule_from_script(rest)
                    && let Some(profile) = self.profiles.iter_mut().find(|profile| profile.id == id)
                {
                    profile.forwards.push(rule);
                }
            }
            "tunnelstart" => {
                for rule in named(self) {
                    self.apply_tunnels(TunnelAction::Start(id.clone(), rule), ctx, now);
                }
            }
            "tunnelstop" => {
                for rule in named(self) {
                    self.apply_tunnels(TunnelAction::Stop(id.clone(), rule), ctx, now);
                }
            }
            "tunnelonly" => self.apply_tunnels(TunnelAction::StartAll(id), ctx, now),
            "tunnelreconnect" => self.apply_tunnels(TunnelAction::Reconnect(id), ctx, now),
            "tunnels" => view::unfold(ctx),
            "tunnelnew" => self.apply_tunnels(TunnelAction::AskAdd(id), ctx, now),
            "tunneledit" => {
                if let Some(rule) = named(self).into_iter().next() {
                    self.apply_tunnels(TunnelAction::AskEdit(id, rule), ctx, now);
                }
            }
            "tunnelform" | "tunnelformok" => {
                if let Some(Dialog::Profile(form)) = &mut self.dialog {
                    if verb == "tunnelform" {
                        form.tunnels.edit(&form.draft.forwards, argument.parse().ok());
                    } else {
                        form.tunnels.commit(&mut form.draft.forwards);
                    }
                }
            }
            "tunnelset" => {
                let (field, value) = argument.split_once(' ').unwrap_or((argument, ""));
                let editor = match &mut self.dialog {
                    Some(Dialog::Tunnel(form)) => Some(&mut form.editor),
                    Some(Dialog::Profile(form)) => form.tunnels.editor_mut(),
                    _ => None,
                };
                if let Some(editor) = editor {
                    editor.set(field, value.trim());
                }
            }
            _ => return false,
        }
        true
    }
}

/// `L 15020 web:80 auto > Site`, as in [`App::dev_step_tunnels`].
fn rule_from_script(text: &str) -> Option<ForwardRule> {
    let (spec, name) = text.split_once('>').unwrap_or((text, ""));
    let words: Vec<&str> = spec.split_whitespace().collect();
    let [kind, bind, rest @ ..] = words.as_slice() else {
        return None;
    };
    let split = |text: &str| -> Option<(String, u16)> {
        match text.rsplit_once(':') {
            Some((host, port)) => Some((host.to_owned(), port.parse().ok()?)),
            None => Some((DEFAULT_BIND.to_owned(), text.parse().ok()?)),
        }
    };
    let (bind_host, bind_port) = split(bind)?;
    let mut rule = ForwardRule {
        name: name.trim().to_owned(),
        kind: view::kind_named(kind)?,
        bind_host,
        bind_port,
        auto_start: rest.contains(&"auto"),
        ..ForwardRule::default()
    };
    if let Some((host, port)) = rest.iter().find(|word| word.contains(':')).and_then(|word| split(word)) {
        rule.dest_host = host;
        rule.dest_port = port;
    }
    Some(rule)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tunnels::ForwardKind;

    #[test]
    fn scripts_describe_rules_as_ssh_does() {
        let rule = rule_from_script("L 15020 web:80 auto > Site interne").unwrap();
        assert_eq!(rule.kind, ForwardKind::Local);
        assert_eq!((rule.bind_host.as_str(), rule.bind_port), ("127.0.0.1", 15020));
        assert_eq!(rule.destination(), "web:80");
        assert!(rule.auto_start);
        assert_eq!(rule.name, "Site interne");

        let rule = rule_from_script("r 0.0.0.0:2242 127.0.0.1:15022").unwrap();
        assert_eq!(rule.summary(), "serveur 0.0.0.0:2242 → 127.0.0.1:15022");
        assert!(!rule.auto_start);
        assert_eq!(rule_from_script("D 1080").unwrap().summary(), "SOCKS 127.0.0.1:1080");
        assert!(rule_from_script("X 1080").is_none());
        assert!(rule_from_script("L").is_none());
    }
}
