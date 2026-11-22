//! The app's side of SSH authentication: what each SSH connection is given
//! (identity, jump hosts, agent, passphrase prompts), the keychain and its
//! dialogs, key generation, and installing a key on a server.
//!
//! A keychain screen opened from another dialog (the connection form, the
//! list of identities, the installation of a key) goes back to it when it
//! closes: the dialogs it was opened over wait in [`State::stack`]. The
//! passphrase question shows over whatever is open, as the connection
//! waiting for it cannot go on without.
//!
//! Scripts (see [`crate::dev`]) never reach the user's SSH agent:
//! `S3V_DEV_SSH_AGENT` names the pipe of an agent to use instead.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

use super::{App, is_dev};
use crate::dev;
use crate::profiles::{self, Profile, Protocol};
use crate::ssh_auth::keychain::{self, Identity, Secret};
use crate::ssh_auth::keys::{self, Generated, Installed, KeyType};
use crate::ssh_auth::{AuthMethod, Owner, PassphraseAnswer, PassphraseRequest, PromptEvent, Prompter, SshExtra, jump};
use crate::storage::ssh::SshSettings;
use crate::ui::Action;
use crate::ui::dialogs::Dialog;
use crate::ui::ssh_auth::{
    AuthDialog, FormExtras, FormRequest, GenerateForm, IdentityForm, IdentityRequest, InstallForm, InstallRequest,
    InstallTarget, KeychainRequest, KeychainView, PassphraseForm, SshAuthAction,
};

enum Event {
    Prompt(PromptEvent),
    Generated(Result<Generated, String>),
    Installed {
        target: InstallTarget,
        /// The connection and the identity, for the message.
        title: String,
        identity: String,
        identity_id: String,
        /// Then make the connection use that identity.
        use_it: bool,
        result: Result<Installed, String>,
    },
}

pub struct State {
    pub identities: Vec<Identity>,
    /// Secrets typed while a script drives the app: never stored.
    dev_secrets: HashMap<(String, Secret), String>,
    prompter: Prompter,
    sender: Sender<Event>,
    events: Receiver<Event>,
    ctx: egui::Context,
    /// Passphrase questions waiting for the one on screen to be answered.
    pending: VecDeque<PassphraseRequest>,
    /// The dialogs a keychain screen was opened over, the latest last.
    stack: Vec<Dialog>,
}

impl State {
    pub fn new(ctx: &egui::Context) -> Self {
        let (sender, events) = channel();
        let (prompts, wake) = (sender.clone(), ctx.clone());
        let prompter = Prompter::new(move |event| {
            let _ = prompts.send(Event::Prompt(event));
            wake.request_repaint();
        });
        Self {
            identities: keychain::load(),
            dev_secrets: HashMap::new(),
            prompter,
            sender,
            events,
            ctx: ctx.clone(),
            pending: VecDeque::new(),
            stack: Vec::new(),
        }
    }

    /// Reports the end of a background job and wakes the UI.
    fn reporter(&self) -> impl Fn(Event) + Send + 'static {
        let (sender, ctx) = (self.sender.clone(), self.ctx.clone());
        move |event| {
            let _ = sender.send(event);
            ctx.request_repaint();
        }
    }

    fn identity(&self, id: &str) -> Option<&Identity> {
        self.identities.iter().find(|identity| identity.id == id)
    }

    fn named(&self, name: &str) -> Option<&Identity> {
        self.identities.iter().find(|identity| identity.title() == name.trim())
    }
}

/// The agent scripts may use, never the user's.
fn dev_agent() -> Option<String> {
    std::env::var("S3V_DEV_SSH_AGENT").ok().filter(|pipe| !pipe.is_empty())
}

impl App {
    /// What the tab of `profile` connects with, its identity and jump hosts
    /// applied: the profile with the user and key that count, the secret,
    /// where host keys are kept, and the rest for `SshSettings`. Other
    /// protocols come back as they are.
    pub(super) fn ssh_route(
        &self,
        mut profile: Profile,
        secret: String,
        known_hosts: Option<PathBuf>,
    ) -> (Profile, String, Option<PathBuf>, SshExtra) {
        if profile.protocol != Protocol::Sftp {
            return (profile, secret, known_hosts, SshExtra::default());
        }
        // Scripts never write in the user's list of known hosts.
        let known_hosts = if dev::active() { None } else { known_hosts };
        let settings = self.ssh_settings(&profile, &secret, known_hosts.clone());
        profile.user.clone_from(&settings.user);
        profile.key_path = settings.key_path.as_ref().map(|path| path.display().to_string()).unwrap_or_default();
        (profile, settings.secret, known_hosts, settings.extra)
    }

    /// The full settings of an SSH connection, its jump hosts included.
    fn ssh_settings(&self, profile: &Profile, secret: &str, known_hosts: Option<PathBuf>) -> SshSettings {
        let scripted = dev::active();
        let mut extra = SshExtra {
            method: profile.auth,
            agent: if scripted { dev_agent() } else { None },
            system: !scripted,
            label: profile.title().to_owned(),
            prompter: Some(self.ssh_auth.prompter.clone()),
            owner: Owner::Profile(profile.id.clone()),
            ..SshExtra::default()
        };

        let identity = match profile.identity.as_str() {
            "" => None,
            id => {
                let found = self.ssh_auth.identity(id);
                match found {
                    Some(identity) => extra.owner = Owner::Identity(identity.id.clone()),
                    None => {
                        extra.problem = Some(tr!(
                            "l'identité de « {name} » n'existe plus dans le trousseau : modifiez la connexion",
                            name = profile.title()
                        ));
                    }
                }
                found
            }
        };
        let credentials = keychain::credentials(profile, secret, identity, |which| {
            identity.and_then(|identity| self.identity_secret(&identity.id, which))
        });
        // What the method leaves out is not even offered.
        let (key_path, secret, password) = match profile.auth {
            AuthMethod::Password => (None, credentials.password, None),
            AuthMethod::Agent => (None, None, None),
            AuthMethod::Auto | AuthMethod::Key => match credentials.key_path {
                Some(key) => (Some(key), credentials.passphrase, credentials.password),
                None => (None, credentials.password, None),
            },
        };
        extra.password = password;

        match jump::chain(&self.profiles, profile) {
            Ok(hops) => {
                if let Some(first) = hops.first() {
                    let secret = self.profile_secret(first);
                    extra.jump = Some(Arc::new(self.ssh_settings(first, &secret, known_hosts.clone())));
                }
            }
            Err(problem) => extra.problem = Some(problem),
        }

        SshSettings {
            host: profile.host().to_owned(),
            port: profile.port(),
            user: credentials.user,
            secret: secret.unwrap_or_default(),
            key_path,
            known_hosts,
            extra,
        }
    }

    /// The stored secret of a saved connection; empty when none.
    fn profile_secret(&self, profile: &Profile) -> String {
        if is_dev(&profile.id) || dev::active() {
            return self.dev_secrets.get(&profile.id).cloned().unwrap_or_default();
        }
        profiles::secret(&profile.id).unwrap_or_default()
    }

    fn identity_secret(&self, id: &str, which: Secret) -> Option<String> {
        if dev::active() {
            return self.ssh_auth.dev_secrets.get(&(id.to_owned(), which)).cloned();
        }
        keychain::secret(id, which)
    }

    fn set_identity_secret(&mut self, id: &str, which: Secret, secret: Option<&str>) -> anyhow::Result<()> {
        if dev::active() {
            let key = (id.to_owned(), which);
            match secret {
                Some(secret) => self.ssh_auth.dev_secrets.insert(key, secret.to_owned()),
                None => self.ssh_auth.dev_secrets.remove(&key),
            };
            return Ok(());
        }
        match secret {
            Some(secret) => keychain::set_secret(id, which, secret),
            None => {
                keychain::delete_secret(id, which);
                Ok(())
            }
        }
    }

    /// What the connection form offers for this connection.
    pub(super) fn ssh_form_extras(&self, draft: &Profile) -> FormExtras {
        let jumps = jump::candidates(&self.profiles, &draft.id)
            .into_iter()
            .map(|profile| {
                let mut address = profile.summary();
                if profile.port != 0 {
                    address.push_str(&format!(":{}", profile.port));
                }
                (profile.id.clone(), profile.title().to_owned(), address)
            })
            .collect();
        FormExtras {
            identities: self.sorted_identities(),
            jumps,
            request: None,
        }
    }

    fn sorted_identities(&self) -> Vec<Identity> {
        let mut list = self.ssh_auth.identities.clone();
        list.sort_by_cached_key(|identity| profiles::sort_key(identity.title()));
        list
    }

    fn keychain_view(&self) -> KeychainView {
        let users = self
            .ssh_auth
            .identities
            .iter()
            .map(|identity| (identity.id.clone(), keychain::users(&self.profiles, &identity.id)))
            .collect();
        KeychainView {
            identities: self.sorted_identities(),
            users,
            request: None,
        }
    }

    /// Opens one of the keychain screens over the dialog on screen, which
    /// comes back once it closes.
    fn open_over(&mut self, dialog: AuthDialog) {
        if let Some(current) = self.dialog.take() {
            self.ssh_auth.stack.push(current);
        }
        self.open_dialog(Dialog::SshAuth(dialog));
    }

    fn identity_form(&self, id: Option<&str>) -> Option<IdentityForm> {
        match id {
            None => Some(IdentityForm::new(Identity::new(), true)),
            Some(id) => self.ssh_auth.identity(id).map(|identity| IdentityForm::new(identity.clone(), false)),
        }
    }

    pub(super) fn apply_ssh_auth(&mut self, action: SshAuthAction, now: f64) {
        match action {
            SshAuthAction::OpenKeychain => {
                let view = self.keychain_view();
                self.open_over(AuthDialog::Keychain(view));
            }
            SshAuthAction::AskInstallOnTab(id) => {
                let Some(tab) = self.tab_index(id).map(|index| &self.tabs[index]) else {
                    return;
                };
                let Some(ssh) = &tab.ssh else {
                    return;
                };
                let (title, account, user) = (tab.title.clone(), ssh.address(), ssh.user.clone());
                let target = InstallTarget::Tab(id);
                self.ask_install(target, title, account, user);
            }
            SshAuthAction::AskInstallOnProfile(id) => {
                let Some(profile) = self.profiles.iter().find(|profile| profile.id == id).cloned() else {
                    return;
                };
                let secret = self.profile_secret(&profile);
                let settings = self.ssh_settings(&profile, &secret, None);
                if let Some(problem) = &settings.extra.problem {
                    self.toast(now, tr!("Installation impossible : {error}", error = problem), true);
                    return;
                }
                let (account, user) = (settings.address(), settings.user.clone());
                self.ask_install(InstallTarget::Profile(id), profile.title().to_owned(), account, user);
            }
        }
    }

    fn ask_install(&mut self, target: InstallTarget, title: String, account: String, user: String) {
        let identities: Vec<Identity> = self
            .sorted_identities()
            .into_iter()
            .filter(|identity| identity.key().is_some())
            .collect();
        let selected = (identities.len() == 1).then(|| identities[0].id.clone());
        let form = InstallForm {
            target,
            title,
            account,
            user,
            identities,
            selected,
            use_it: false,
            busy: false,
            error: None,
            request: None,
        };
        self.open_over(AuthDialog::Install(Box::new(form)));
    }

    /// The settings of the connection a key goes to.
    fn install_settings(&self, target: &InstallTarget) -> Option<Arc<SshSettings>> {
        match target {
            InstallTarget::Tab(id) => self.tab_index(*id).and_then(|index| self.tabs[index].ssh.clone()),
            InstallTarget::Profile(id) => {
                let profile = self.profiles.iter().find(|profile| profile.id == *id)?;
                let known_hosts = if dev::active() || is_dev(id) { None } else { profiles::known_hosts_path() };
                Some(Arc::new(self.ssh_settings(profile, &self.profile_secret(profile), known_hosts)))
            }
        }
    }

    /// A keychain dialog was confirmed.
    pub(super) fn answer_ssh_auth(&mut self, dialog: AuthDialog, now: f64) {
        match dialog {
            AuthDialog::Keychain(_) => {}
            AuthDialog::Identity(form) => self.save_identity(*form, now),
            AuthDialog::Generate(mut form) => {
                let path = form.path();
                let (kind, passphrase, comment) = (form.kind, form.passphrase.clone(), form.comment.trim().to_owned());
                let report = self.ssh_auth.reporter();
                std::thread::spawn(move || {
                    let generated = keys::generate(kind, &passphrase, &comment, &path).map_err(|error| format!("{error:#}"));
                    report(Event::Generated(generated));
                });
                form.busy = true;
                form.error = None;
                self.dialog = Some(Dialog::SshAuth(AuthDialog::Generate(form)));
            }
            AuthDialog::DeleteIdentity { id, title, .. } => {
                self.ssh_auth.identities.retain(|identity| identity.id != id);
                for which in [Secret::Password, Secret::Passphrase] {
                    let _ = self.set_identity_secret(&id, which, None);
                }
                let mut changed = false;
                for profile in self.profiles.iter_mut().filter(|profile| profile.identity == id) {
                    profile.identity.clear();
                    changed = true;
                }
                let saved = keychain::save(&self.ssh_auth.identities);
                self.report_save_error(saved, now);
                if changed {
                    let saved = self.persist_profiles();
                    self.report_save_error(saved, now);
                }
                self.toast(now, tr!("Identité « {title} » supprimée", title), false);
                self.refresh_parent(None);
            }
            AuthDialog::Passphrase(form) => {
                let PassphraseForm { request, passphrase, save } = *form;
                let _ = request.reply.send(Some(PassphraseAnswer { passphrase, save }));
            }
            AuthDialog::Install(mut form) => {
                let Some(identity) = form.chosen().cloned() else {
                    return;
                };
                let Some(key) = identity.key() else {
                    return;
                };
                let passphrase = self.identity_secret(&identity.id, Secret::Passphrase);
                let line = match keys::public_line(&key, passphrase.as_deref()) {
                    Ok(line) => line,
                    Err(error) => {
                        form.error = Some(tr!("Clé publique introuvable : {error}", error = format!("{error:#}")));
                        self.dialog = Some(Dialog::SshAuth(AuthDialog::Install(form)));
                        return;
                    }
                };
                let Some(settings) = self.install_settings(&form.target) else {
                    self.toast(now, tr!("Cette connexion n'existe plus."), true);
                    return;
                };
                let report = self.ssh_auth.reporter();
                let (target, title, use_it) = (form.target.clone(), form.title.clone(), form.use_it && form.can_use());
                let (name, identity_id) = (identity.title().to_owned(), identity.id.clone());
                self.worker.spawn(async move {
                    let result = keys::install(&settings, &line).await.map_err(|error| format!("{error:#}"));
                    report(Event::Installed {
                        target,
                        title,
                        identity: name,
                        identity_id,
                        use_it,
                        result,
                    });
                });
                form.busy = true;
                form.error = None;
                self.dialog = Some(Dialog::SshAuth(AuthDialog::Install(form)));
            }
        }
    }

    fn save_identity(&mut self, form: IdentityForm, now: f64) {
        let IdentityForm {
            mut identity,
            password,
            passphrase,
            forget_password,
            forget_passphrase,
            ..
        } = form;
        identity.name = identity.name.trim().to_owned();
        identity.user = identity.user.trim().to_owned();
        identity.key_path = identity.key_path.trim().trim_matches('"').to_owned();

        let mut stored = Ok(());
        let id = identity.id.clone();
        if !password.is_empty() {
            stored = stored.and(self.set_identity_secret(&id, Secret::Password, Some(&password)));
            identity.has_password = true;
        } else if forget_password {
            stored = stored.and(self.set_identity_secret(&id, Secret::Password, None));
            identity.has_password = false;
        }
        if identity.key().is_none() || forget_passphrase {
            if identity.has_passphrase {
                stored = stored.and(self.set_identity_secret(&id, Secret::Passphrase, None));
            }
            identity.has_passphrase = false;
        } else if !passphrase.is_empty() {
            stored = stored.and(self.set_identity_secret(&id, Secret::Passphrase, Some(&passphrase)));
            identity.has_passphrase = true;
        }
        if let Some(key) = identity.key() {
            // Typed again: what the session remembered may be stale.
            self.ssh_auth.prompter.forget(&key);
        }
        if let Err(error) = stored {
            let mut form = IdentityForm::new(identity, false);
            form.error = Some(tr!("Secret non enregistré : {error}", error = format!("{error:#}")));
            self.dialog = Some(Dialog::SshAuth(AuthDialog::Identity(Box::new(form))));
            return;
        }

        let title = identity.title().to_owned();
        match self.ssh_auth.identities.iter_mut().find(|existing| existing.id == id) {
            Some(existing) => *existing = identity,
            None => self.ssh_auth.identities.push(identity),
        }
        let saved = keychain::save(&self.ssh_auth.identities);
        self.report_save_error(saved, now);
        self.toast(now, tr!("Identité « {title} » enregistrée", title), false);
        self.refresh_parent(Some(&id));
    }

    /// Brings the dialog waiting below up to date with the keychain, and
    /// picks `chosen` in it when it was just made for it.
    fn refresh_parent(&mut self, chosen: Option<&str>) {
        let identities = self.sorted_identities();
        let view = self.keychain_view();
        match self.ssh_auth.stack.last_mut() {
            Some(Dialog::Profile(form)) => {
                form.ssh.identities = identities;
                if let Some(id) = chosen {
                    form.draft.identity = id.to_owned();
                }
            }
            Some(Dialog::SshAuth(AuthDialog::Keychain(parent))) => *parent = view,
            Some(Dialog::SshAuth(AuthDialog::Install(form))) => {
                form.identities = identities.into_iter().filter(|identity| identity.key().is_some()).collect();
                if let Some(id) = chosen
                    && form.identities.iter().any(|identity| identity.id == id)
                {
                    form.selected = Some(id.to_owned());
                }
            }
            _ => {}
        }
    }

    /// Once per frame, after the dialogs: results of background jobs, what
    /// the dialog on screen asked for, and the dialog to come back to.
    pub(super) fn ssh_auth_frame(&mut self, ctx: &egui::Context, now: f64) {
        while let Ok(event) = self.ssh_auth.events.try_recv() {
            self.ssh_auth_event(event, now);
        }
        self.ssh_auth_requests(ctx, now);
        if self.dialog.is_none()
            && let Some(parent) = self.ssh_auth.stack.pop()
        {
            self.open_dialog(parent);
        }

        let asking = matches!(self.dialog, Some(Dialog::SshAuth(AuthDialog::Passphrase(_))));
        if !asking {
            // A connection that gave up meanwhile needs no answer.
            while let Some(request) = self.ssh_auth.pending.pop_front() {
                if request.reply.is_closed() {
                    continue;
                }
                let form = PassphraseForm {
                    request,
                    passphrase: String::new(),
                    save: false,
                };
                self.open_over(AuthDialog::Passphrase(Box::new(form)));
                break;
            }
        }
    }

    fn ssh_auth_event(&mut self, event: Event, now: f64) {
        match event {
            Event::Prompt(PromptEvent::Ask(request)) => self.ssh_auth.pending.push_back(request),
            Event::Prompt(PromptEvent::Save { owner, passphrase }) => self.save_passphrase(owner, &passphrase, now),
            Event::Generated(result) => {
                let Some(Dialog::SshAuth(AuthDialog::Generate(form))) = &mut self.dialog else {
                    return;
                };
                match result {
                    Ok(generated) => {
                        let passphrase = std::mem::take(&mut form.passphrase);
                        let kind = keys::kind(&generated.public);
                        self.dialog = None;
                        if let Some(Dialog::SshAuth(AuthDialog::Identity(identity))) = self.ssh_auth.stack.last_mut() {
                            identity.identity.key_path = generated.private.display().to_string();
                            identity.passphrase = passphrase;
                            identity.forget_passphrase = false;
                        }
                        let name = generated.private.file_name().unwrap_or_default().to_string_lossy();
                        self.toast(now, tr!("Clé {kind} créée : {name} et {name}.pub", kind, name), false);
                    }
                    Err(error) => {
                        form.busy = false;
                        form.error = Some(tr!("Génération impossible : {error}", error));
                    }
                }
            }
            Event::Installed {
                target,
                title,
                identity,
                identity_id,
                use_it,
                result,
            } => {
                let showing = matches!(&self.dialog, Some(Dialog::SshAuth(AuthDialog::Install(form))) if form.target == target);
                let text = match result {
                    Err(error) => {
                        if showing && let Some(Dialog::SshAuth(AuthDialog::Install(form))) = &mut self.dialog {
                            form.busy = false;
                            form.error = Some(tr!("Installation impossible : {error}", error));
                        } else {
                            let text = tr!("Installation de la clé impossible sur « {title} » : {error}", title, error);
                            self.toast(now, text, true);
                        }
                        return;
                    }
                    Ok(Installed::Added) => tr!("Clé « {identity} » installée sur « {title} »", identity, title),
                    Ok(Installed::AlreadyThere) => {
                        tr!("La clé « {identity} » était déjà installée sur « {title} »", identity, title)
                    }
                };
                if showing {
                    self.dialog = None;
                }
                let mut text = text;
                if use_it {
                    let profile_id = match &target {
                        InstallTarget::Tab(tab) => self.tab_index(*tab).map(|index| self.tabs[index].profile_id.clone()),
                        InstallTarget::Profile(id) => Some(id.clone()),
                    };
                    if let Some(profile) = profile_id.and_then(|pid| self.profiles.iter_mut().find(|p| p.id == pid)) {
                        profile.identity = identity_id;
                        profile.auth = AuthMethod::Auto;
                        let said = tr!("{text} ; la connexion l'utilise désormais", text);
                        text = said;
                        let saved = self.persist_profiles();
                        self.report_save_error(saved, now);
                    }
                }
                self.toast(now, text, false);
            }
        }
    }

    /// Keeps for good a passphrase the user asked to save, where its key is
    /// configured: in the connection, or in its identity.
    fn save_passphrase(&mut self, owner: Owner, passphrase: &str, now: f64) {
        let result = match &owner {
            Owner::Nobody => return,
            Owner::Profile(id) if is_dev(id) || dev::active() => {
                self.dev_secrets.insert(id.clone(), passphrase.to_owned());
                Ok(())
            }
            Owner::Profile(id) => profiles::set_secret(id, passphrase),
            Owner::Identity(id) => {
                let id = id.clone();
                let stored = self.set_identity_secret(&id, Secret::Passphrase, Some(passphrase));
                if let Some(identity) = self.ssh_auth.identities.iter_mut().find(|identity| identity.id == id) {
                    identity.has_passphrase = true;
                }
                stored.and(keychain::save(&self.ssh_auth.identities))
            }
        };
        match result {
            Ok(()) => self.toast(now, tr!("Phrase secrète enregistrée"), false),
            Err(error) => {
                let error = format!("{error:#}");
                self.toast(now, tr!("Phrase secrète non enregistrée : {error}", error), true);
            }
        }
    }

    /// Acts on what the dialog on screen asked for while it was drawn.
    fn ssh_auth_requests(&mut self, ctx: &egui::Context, now: f64) {
        match &mut self.dialog {
            Some(Dialog::Profile(form)) => {
                let Some(request) = form.ssh.request.take() else {
                    return;
                };
                let identity = match request {
                    FormRequest::Keychain => {
                        let view = self.keychain_view();
                        return self.open_over(AuthDialog::Keychain(view));
                    }
                    FormRequest::NewIdentity => self.identity_form(None),
                    FormRequest::EditIdentity(id) => self.identity_form(Some(&id)),
                };
                if let Some(form) = identity {
                    self.open_over(AuthDialog::Identity(Box::new(form)));
                }
            }
            Some(Dialog::SshAuth(AuthDialog::Keychain(view))) => {
                let Some(request) = view.request.take() else {
                    return;
                };
                match request {
                    KeychainRequest::New | KeychainRequest::Edit(_) => {
                        let id = match &request {
                            KeychainRequest::Edit(id) => Some(id.as_str()),
                            _ => None,
                        };
                        if let Some(form) = self.identity_form(id) {
                            self.open_over(AuthDialog::Identity(Box::new(form)));
                        }
                    }
                    KeychainRequest::Delete(id) => {
                        if let Some(identity) = self.ssh_auth.identity(&id) {
                            let title = identity.title().to_owned();
                            let users = keychain::users(&self.profiles, &id);
                            self.open_over(AuthDialog::DeleteIdentity { id, title, users });
                        }
                    }
                }
            }
            Some(Dialog::SshAuth(AuthDialog::Identity(form))) => match form.request.take() {
                Some(IdentityRequest::Generate) => {
                    let generate = GenerateForm::new(form.identity.title(), &form.identity.user);
                    self.open_over(AuthDialog::Generate(Box::new(generate)));
                }
                Some(IdentityRequest::CopyPublic(line)) => {
                    ctx.copy_text(line);
                    self.toast(now, tr!("Clé publique copiée"), false);
                }
                None => {}
            },
            Some(Dialog::SshAuth(AuthDialog::Install(form))) => match form.request.take() {
                Some(InstallRequest::NewIdentity) => {
                    if let Some(form) = self.identity_form(None) {
                        self.open_over(AuthDialog::Identity(Box::new(form)));
                    }
                }
                Some(InstallRequest::Keychain) => {
                    let view = self.keychain_view();
                    self.open_over(AuthDialog::Keychain(view));
                }
                None => {}
            },
            _ => {}
        }
    }

    /// Links the connections just imported to the jump hosts their
    /// ssh_config names, and says which could not be found.
    pub(super) fn link_jumps(&mut self, imported: &[(String, Option<String>)], now: f64) {
        let missing = jump::link_imported(&mut self.profiles, imported);
        if !missing.is_empty() {
            let names: Vec<String> = missing.iter().map(|name| tr!("« {name} »", name)).collect();
            let text = trn!(
                missing.len(),
                "Serveur de rebond introuvable : {names}. Importez-le, puis choisissez-le dans la connexion.",
                "Serveur de rebond introuvable : {names}. Importez-le, puis choisissez-le dans la connexion.",
                names = names.join(", ")
            );
            self.toast(now, text, true);
        }
    }

    /// Script steps of this module, all starting with `ssh`:
    /// `sshkeychain` opens the keychain; `sshidentity new`,
    /// `sshidentity edit Name`, `sshidentity delete Name` open an identity
    /// (over the connection form or the keychain when one is open);
    /// `sshset field value` fills the open form: `name`, `user`, `password`,
    /// `passphrase`, `confirm`, `key`, `path`, `comment`, `kind rsa`,
    /// `method agent` (or `auto`, `password`, `key`), `identity Name` and
    /// `jump Name` (`none` for neither), `save` and `use` tick their box,
    /// `pick Name` chooses the identity of a key to install;
    /// `sshgenerate` and `sshcopy` press those buttons of an identity;
    /// `sshinstall tab 1` or `sshinstall Name` ask to install a key.
    pub(super) fn dev_step_ssh_auth(&mut self, verb: &str, argument: &str, ctx: &egui::Context, now: f64) -> bool {
        match verb {
            "sshkeychain" => self.apply(Action::SshAuth(SshAuthAction::OpenKeychain), ctx, now),
            "sshidentity" => {
                let (what, name) = argument.split_once(' ').unwrap_or((argument, ""));
                let id = self.ssh_auth.named(name).map(|identity| identity.id.clone());
                match (&mut self.dialog, what) {
                    (Some(Dialog::Profile(form)), "new") => form.ssh.request = Some(FormRequest::NewIdentity),
                    (Some(Dialog::Profile(form)), "edit") => form.ssh.request = id.map(FormRequest::EditIdentity),
                    (Some(Dialog::SshAuth(AuthDialog::Keychain(view))), _) => {
                        view.request = match what {
                            "new" => Some(KeychainRequest::New),
                            "edit" => id.map(KeychainRequest::Edit),
                            _ => id.map(KeychainRequest::Delete),
                        };
                    }
                    (Some(Dialog::SshAuth(AuthDialog::Install(form))), "new") => {
                        form.request = Some(InstallRequest::NewIdentity);
                    }
                    _ if what == "new" => {
                        if let Some(form) = self.identity_form(None) {
                            self.open_over(AuthDialog::Identity(Box::new(form)));
                        }
                    }
                    _ => {
                        let view = self.keychain_view();
                        self.open_over(AuthDialog::Keychain(view));
                        if let Some(Dialog::SshAuth(AuthDialog::Keychain(view))) = &mut self.dialog {
                            view.request = match what {
                                "edit" => id.map(KeychainRequest::Edit),
                                _ => id.map(KeychainRequest::Delete),
                            };
                        }
                    }
                }
            }
            "sshset" => {
                let (field, value) = argument.split_once(' ').unwrap_or((argument, ""));
                self.dev_set_field(field, value.trim());
            }
            "sshgenerate" | "sshcopy" => {
                if let Some(Dialog::SshAuth(AuthDialog::Identity(form))) = &mut self.dialog {
                    form.request = if verb == "sshgenerate" {
                        Some(IdentityRequest::Generate)
                    } else {
                        form.public.as_ref().and_then(|(_, public)| public.clone().ok()).map(IdentityRequest::CopyPublic)
                    };
                }
            }
            "sshinstall" => {
                let action = match argument.split_once(' ') {
                    Some(("tab", index)) => match index.trim().parse::<usize>().ok().and_then(|index| self.tabs.get(index)) {
                        Some(tab) => SshAuthAction::AskInstallOnTab(tab.id),
                        None => return true,
                    },
                    _ => match self.profile_named(argument) {
                        Some(profile) => SshAuthAction::AskInstallOnProfile(profile.id.clone()),
                        None => return true,
                    },
                };
                self.apply(Action::SshAuth(action), ctx, now);
            }
            _ => return false,
        }
        true
    }

    fn dev_set_field(&mut self, field: &str, value: &str) {
        let identity_id = self.ssh_auth.named(value).map(|identity| identity.id.clone());
        let jump_id = self.profile_named(value).map(|profile| profile.id.clone());
        let method = match value {
            "agent" => AuthMethod::Agent,
            "key" => AuthMethod::Key,
            "password" => AuthMethod::Password,
            _ => AuthMethod::Auto,
        };
        match &mut self.dialog {
            Some(Dialog::Profile(form)) => match field {
                "user" => form.draft.user = value.to_owned(),
                "password" | "passphrase" => form.secret = value.to_owned(),
                "key" => form.draft.key_path = value.to_owned(),
                "method" => form.draft.auth = method,
                "identity" => form.draft.identity = identity_id.unwrap_or_default(),
                "jump" => form.draft.jump = jump_id.unwrap_or_default(),
                "name" => form.draft.name = value.to_owned(),
                _ => {}
            },
            Some(Dialog::SshAuth(AuthDialog::Identity(form))) => match field {
                "name" => form.identity.name = value.to_owned(),
                "user" => form.identity.user = value.to_owned(),
                "password" => form.password = value.to_owned(),
                "passphrase" => form.passphrase = value.to_owned(),
                "key" => form.identity.key_path = value.to_owned(),
                _ => {}
            },
            Some(Dialog::SshAuth(AuthDialog::Generate(form))) => match field {
                "passphrase" => form.passphrase = value.to_owned(),
                "confirm" => form.confirm = value.to_owned(),
                "path" => {
                    form.name = value.to_owned();
                    form.name_auto = false;
                }
                "comment" => form.comment = value.to_owned(),
                "kind" => {
                    form.kind = if value == "rsa" { KeyType::Rsa4096 } else { KeyType::Ed25519 };
                    if form.name_auto {
                        form.suggest_name();
                    }
                }
                _ => {}
            },
            Some(Dialog::SshAuth(AuthDialog::Passphrase(form))) => match field {
                "passphrase" => form.passphrase = value.to_owned(),
                "save" => form.save = true,
                _ => {}
            },
            Some(Dialog::SshAuth(AuthDialog::Install(form))) => match field {
                "pick" => form.selected = identity_id,
                "use" => form.use_it = true,
                _ => {}
            },
            _ => {}
        }
    }
}
