//! The app's side of host colours and tags, of the encrypted export and
//! import, and of the tabs reopened at startup.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use zeroize::Zeroizing;

use super::{App, ClosedTab, is_dev};
use crate::dev;
use crate::hosts::restore::{self, SavedTab, SavedTabs};
use crate::hosts::vault::{self, Bundle, KdfParams, KeyFile, MAX_KEY_FILE};
use crate::hosts::{self, HostColor};
use crate::profiles::{self, Profile, Settings};
use crate::storage::RemotePath;
use crate::tabs::{Side, Views};
use crate::ui::dialogs::Dialog;
use crate::ui::hosts::vault::{ExportDone, ExportForm, PreviewForm, UnlockForm};
use crate::ui::hosts::{ExportScope, HostsAction, HostsDialog, TagField, TagsForm};
use crate::worker::safe_component;

/// Tabs that changed are written once they have stayed put this long.
const SAVE_DELAY: f64 = 1.0;
/// Far bigger than any list of connections: not one of our files.
const MAX_VAULT_FILE: u64 = 64 * 1024 * 1024;

/// What the app keeps for this feature between frames.
#[derive(Default)]
pub struct State {
    /// The tabs as last written or read, so that only changes are written.
    written: Option<SavedTabs>,
    /// When the tabs started to differ from what is written.
    changed_at: Option<f64>,
}

impl App {
    /// Once per frame: what the widgets need to know, and the tabs kept.
    pub(super) fn hosts_frame(&mut self, ctx: &egui::Context, now: f64) {
        crate::ui::hosts::publish(ctx, &self.profiles, self.settings.restore_tabs);
        self.keep_tabs(ctx, now);
    }

    /// The open tabs as they would be reopened.
    fn saved_tabs(&self) -> SavedTabs {
        // Files open in the editor are not reopened: what they showed may
        // have changed by then, and their changes are asked about on exit.
        let kept: Vec<&crate::tabs::Tab> = self.tabs.iter().filter(|tab| tab.editor.is_none()).collect();
        let index_of = |side| self.views.shown(side).and_then(|id| kept.iter().position(|tab| tab.id == id));
        let tabs = kept
            .iter()
            .map(|tab| {
                // This computer and its shells are named in the language in
                // use: a title of their own is only one given by hand.
                let named = self
                    .profiles
                    .iter()
                    .find(|profile| profile.id == tab.profile_id)
                    .map(|profile| profile.title().to_owned())
                    .or_else(|| crate::local::entry(&tab.profile_id).map(|entry| entry.title().to_owned()))
                    .or_else(|| crate::monitor::is_dashboard(&tab.profile_id).then(|| tr!("Surveillance").to_owned()));
                SavedTab {
                    profile: tab.profile_id.clone(),
                    title: (named.as_deref() != Some(tab.title.as_str())).then(|| tab.title.clone()),
                    path: tab.session.known_path().map(|path| path.to_string()),
                    panes: tab.panes,
                    right: tab.side == Side::Right,
                }
            })
            .collect();
        SavedTabs {
            tabs,
            shown: [index_of(Side::Left), index_of(Side::Right)],
            focus_right: self.views.focus == Side::Right,
        }
    }

    /// Writes the tabs once they stop changing, so that they come back even
    /// after a crash; the last state is written on exit as well.
    fn keep_tabs(&mut self, ctx: &egui::Context, now: f64) {
        if !self.settings.restore_tabs {
            return;
        }
        let current = self.saved_tabs();
        if self.hosts.written.as_ref() == Some(&current) {
            self.hosts.changed_at = None;
            return;
        }
        let since = *self.hosts.changed_at.get_or_insert(now);
        if now - since < SAVE_DELAY {
            ctx.request_repaint_after(Duration::from_secs_f64(SAVE_DELAY - (now - since) + 0.05));
            return;
        }
        self.write_tabs(current);
    }

    fn write_tabs(&mut self, current: SavedTabs) {
        // Not worth a toast: the next change tries again.
        let _ = restore::save(&current);
        self.hosts.written = Some(current);
        self.hosts.changed_at = None;
    }

    /// On exit: the tabs as they are now.
    pub(super) fn save_tabs_now(&mut self) {
        if self.settings.restore_tabs {
            let current = self.saved_tabs();
            if self.hosts.written.as_ref() != Some(&current) {
                self.write_tabs(current);
            }
        }
    }

    /// At startup: the tabs open when the app was closed, in their views,
    /// each showing its folder. Nothing connects here; the tabs on screen
    /// connect on the first frame, the others once shown (see
    /// [`App::start_panes`]).
    pub(super) fn restore_tabs(&mut self) {
        if !self.settings.restore_tabs {
            return;
        }
        let Some(saved) = restore::load() else {
            return;
        };
        self.hosts.written = Some(saved.clone());
        // A connection deleted since is skipped without a word.
        let saved = saved.keep(|id| self.profiles.iter().any(|profile| profile.id == id) || crate::monitor::is_dashboard(id));
        if saved.tabs.is_empty() {
            return;
        }
        // What a script opens at startup gives way to what comes back.
        self.tabs.clear();
        self.views = Views::default();

        let mut ids = Vec::with_capacity(saved.tabs.len());
        for saved_tab in &saved.tabs {
            let title = match &saved_tab.title {
                Some(title) => title.clone(),
                None => self
                    .profiles
                    .iter()
                    .find(|profile| profile.id == saved_tab.profile)
                    .map(|profile| profile.title().to_owned())
                    .or_else(|| crate::local::entry(&saved_tab.profile).map(|entry| entry.title().to_owned()))
                    .or_else(|| crate::monitor::is_dashboard(&saved_tab.profile).then(|| tr!("Surveillance").to_owned()))
                    .unwrap_or_default(),
            };
            let like = ClosedTab {
                profile_id: saved_tab.profile.clone(),
                title,
                path: saved_tab.path.as_deref().map(RemotePath::parse),
                panes: saved_tab.panes,
                file: None,
            };
            let tab = self.tab_like(&like, 0.0);
            ids.push(tab.as_ref().map(|tab| tab.id));
            if let Some(mut tab) = tab {
                tab.side = if saved_tab.right { Side::Right } else { Side::Left };
                self.tabs.push(tab);
            }
        }
        for shown in saved.shown.into_iter().flatten() {
            if let Some(id) = ids.get(shown).copied().flatten() {
                self.views.show(&self.tabs, id);
            }
        }
        self.views.focus = if saved.focus_right { Side::Right } else { Side::Left };
        self.settle();
    }

    pub(super) fn apply_hosts(&mut self, action: HostsAction, now: f64) {
        match action {
            HostsAction::SetColor(id, color) => {
                if let Some(profile) = self.profiles.iter_mut().find(|profile| profile.id == id)
                    && profile.color != color
                {
                    profile.color = color;
                    let saved = self.persist_profiles();
                    self.report_save_error(saved, now);
                }
            }
            HostsAction::SetFolderColor(path, color) => {
                let mut count = 0;
                for profile in &mut self.profiles {
                    if profiles::folder_contains(&path, &profile.folder) {
                        profile.color = color;
                        count += 1;
                    }
                }
                if count == 0 {
                    return;
                }
                let saved = self.persist_profiles();
                if saved.is_ok() {
                    let folder = profiles::folder_name(&path);
                    let text = match color {
                        Some(color) => trn!(
                            count,
                            "{n} connexion de « {folder} » en {color}",
                            "{n} connexions de « {folder} » en {color}",
                            folder,
                            color = color.label().to_lowercase()
                        ),
                        None => trn!(
                            count,
                            "{n} connexion de « {folder} » sans couleur",
                            "{n} connexions de « {folder} » sans couleur",
                            folder
                        ),
                    };
                    self.toast(now, text, false);
                }
                self.report_save_error(saved, now);
            }
            HostsAction::AskTags(id) => {
                if let Some(profile) = self.profiles.iter().find(|profile| profile.id == id) {
                    let form = TagsForm {
                        profile_id: id.clone(),
                        title: profile.title().to_owned(),
                        tags: profile.tags.clone(),
                        field: TagField::new(hosts::known_tags(&self.profiles)),
                    };
                    self.open_dialog(Dialog::Hosts(HostsDialog::Tags(form)));
                }
            }
            HostsAction::AskExport(scope) => {
                if self.profiles.is_empty() {
                    self.toast(now, tr!("Aucune connexion à exporter."), true);
                    return;
                }
                let form = ExportForm::new(&self.profiles, &self.settings, &scope);
                self.open_dialog(Dialog::Hosts(HostsDialog::Export(Box::new(form))));
            }
            HostsAction::SetRestoreTabs(enabled) => {
                self.settings.restore_tabs = enabled;
                self.save_settings();
                self.hosts.written = None;
                if !enabled {
                    restore::forget();
                }
                let text = if enabled {
                    tr!("Les onglets seront rouverts au prochain démarrage")
                } else {
                    tr!("Les onglets ne seront plus rouverts au démarrage")
                };
                self.toast(now, text, false);
            }
        }
    }

    /// Asks for an encrypted export to open, then for its password.
    pub(super) fn ask_vault_import(&mut self, now: f64) {
        let path = match dev::vault_file() {
            Some(path) => path,
            // A script never gets a native dialog on screen.
            None if dev::active() => {
                self.toast(now, "S3V_DEV_VAULT doit nommer la sauvegarde à ouvrir.", true);
                return;
            }
            None => {
                let mut dialog = rfd::FileDialog::new()
                    .set_title(tr!("Ouvrir une sauvegarde chiffrée"))
                    .add_filter(tr!("Sauvegarde de Farfile"), &[vault::EXTENSION])
                    .add_filter(tr!("Tous les fichiers"), &["*"]);
                if let Some(documents) = directories::UserDirs::new().and_then(|dirs| dirs.document_dir().map(PathBuf::from)) {
                    dialog = dialog.set_directory(documents);
                }
                let Some(path) = dialog.pick_file() else {
                    return;
                };
                path
            }
        };
        match read_vault(&path) {
            Ok(file) => {
                let form = UnlockForm::new(path, file);
                self.open_dialog(Dialog::Hosts(HostsDialog::Unlock(Box::new(form))));
            }
            Err(error) => self.toast(now, error, true),
        }
    }

    /// A dialog of this feature was confirmed, or a job it waits for ended.
    pub(super) fn hosts_dialog_confirmed(&mut self, dialog: HostsDialog, ctx: &egui::Context, now: f64) {
        match dialog {
            HostsDialog::Tags(mut form) => {
                form.field.commit(&mut form.tags);
                if let Some(profile) = self.profiles.iter_mut().find(|profile| profile.id == form.profile_id)
                    && profile.tags != form.tags
                {
                    profile.tags = form.tags;
                    let saved = self.persist_profiles();
                    self.report_save_error(saved, now);
                }
            }
            HostsDialog::Export(form) => self.export_confirmed(form, ctx, now),
            HostsDialog::Unlock(form) => self.unlock_confirmed(form, ctx),
            HostsDialog::Preview(form) => self.import_vault(*form, now),
        }
    }

    fn keep_dialog(&mut self, dialog: HostsDialog) {
        self.dialog = Some(Dialog::Hosts(dialog));
    }

    /// Asks where to write, then encrypts and writes in the background: the
    /// key derivation is meant to be slow.
    fn export_confirmed(&mut self, mut form: Box<ExportForm>, ctx: &egui::Context, now: f64) {
        match form.done.take() {
            Some(Ok(done)) => return self.exported(&done, now),
            Some(Err(error)) => {
                form.error = Some(error);
                return self.keep_dialog(HostsDialog::Export(form));
            }
            None => {}
        }
        if form.job.is_some() || form.problem().is_some() {
            return self.keep_dialog(HostsDialog::Export(form));
        }
        let Some(path) = self.export_path(now) else {
            return self.keep_dialog(HostsDialog::Export(form));
        };

        let chosen: Vec<Profile> = form
            .chosen()
            .iter()
            .filter_map(|id| self.profiles.iter().find(|profile| profile.id == *id).cloned())
            .collect();
        // Everything goes with its empty folders; a part with its own.
        let folders = if chosen.len() == self.profiles.len() {
            profiles::folders(&self.profiles, &self.settings)
        } else {
            profiles::folders(&chosen, &Settings::default())
        };
        let job = ExportJob {
            path,
            profiles: chosen,
            folders,
            secrets: form.include_secrets,
            keys: form.include_keys,
            // While a script drives the app, its secrets are its own and the
            // credential store is never read.
            dev_secrets: dev::active().then(|| self.dev_secrets.clone()),
            password: Zeroizing::new(form.password.clone()),
        };
        let (sender, receiver) = mpsc::channel();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new().name("export".to_owned()).spawn(move || {
            let result = job.run().map_err(|error| tr!("Export impossible : {error}", error = format!("{error:#}")));
            let _ = sender.send(result);
            ctx.request_repaint();
        });
        match spawned {
            Ok(_) => {
                form.job = Some(receiver);
                form.error = None;
            }
            Err(error) => form.error = Some(tr!("Export impossible : {error}", error)),
        }
        self.keep_dialog(HostsDialog::Export(form));
    }

    fn exported(&mut self, done: &ExportDone, now: f64) {
        let name = done
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut text = trn!(
            done.count,
            "{n} connexion exportée dans « {name} »",
            "{n} connexions exportées dans « {name} »",
            name
        );
        if done.secrets > 0 {
            text.push_str(", ");
            text.push_str(&trn!(done.secrets, "{n} mot de passe", "{n} mots de passe"));
        }
        if done.keys > 0 {
            text.push_str(", ");
            text.push_str(&trn!(done.keys, "{n} clé", "{n} clés"));
        }
        if done.missing_keys > 0 {
            text.push_str(tr!(" ; "));
            text.push_str(&trn!(
                done.missing_keys,
                "{n} clé privée introuvable(s) ou illisible(s), laissée(s) de côté",
                "{n} clés privées introuvable(s) ou illisible(s), laissée(s) de côté"
            ));
        }
        self.toast(now, text, done.missing_keys > 0);
    }

    /// Where to write the export: the file a script names, or the one the
    /// user picks. `None` when cancelled.
    fn export_path(&mut self, now: f64) -> Option<PathBuf> {
        if let Some(path) = dev::vault_file() {
            return Some(path);
        }
        if dev::active() {
            self.toast(now, "S3V_DEV_VAULT doit nommer le fichier d'export.", true);
            return None;
        }
        // A file name: the date stays the way files sort.
        let stem = tr!("connexions-{date}", date = chrono::Local::now().format("%Y-%m-%d"));
        let name = format!("{stem}.{}", vault::EXTENSION);
        let mut dialog = rfd::FileDialog::new()
            .set_title(tr!("Exporter les connexions"))
            .set_file_name(name)
            .add_filter(tr!("Sauvegarde de Farfile"), &[vault::EXTENSION]);
        if let Some(documents) = directories::UserDirs::new().and_then(|dirs| dirs.document_dir().map(PathBuf::from)) {
            dialog = dialog.set_directory(documents);
        }
        let mut path = dialog.save_file()?;
        if path.extension().is_none() {
            path.set_extension(vault::EXTENSION);
        }
        Some(path)
    }

    /// Decrypts in the background, then shows what the file holds. A wrong
    /// password leaves the dialog open with its error, nothing imported.
    fn unlock_confirmed(&mut self, mut form: Box<UnlockForm>, ctx: &egui::Context) {
        match form.done.take() {
            Some(Ok(bundle)) => {
                let preview = PreviewForm::new(form.file_name(), bundle, &self.profiles);
                return self.open_dialog(Dialog::Hosts(HostsDialog::Preview(Box::new(preview))));
            }
            Some(Err(error)) => {
                form.error = Some(error.to_string());
                return self.keep_dialog(HostsDialog::Unlock(form));
            }
            None => {}
        }
        if form.job.is_none() && !form.password.is_empty() {
            let (sender, receiver) = mpsc::channel();
            let (file, password) = (form.file.clone(), Zeroizing::new(form.password.clone()));
            let ctx = ctx.clone();
            let spawned = std::thread::Builder::new().name("import".to_owned()).spawn(move || {
                let result = vault::open(&file, &password).and_then(|plain| Bundle::from_bytes(&plain));
                let _ = sender.send(result);
                ctx.request_repaint();
            });
            match spawned {
                Ok(_) => {
                    form.job = Some(receiver);
                    form.error = None;
                }
                Err(error) => form.error = Some(tr!("Déchiffrement impossible : {error}", error)),
            }
        }
        self.keep_dialog(HostsDialog::Unlock(form));
    }

    /// Adds the ticked connections of an opened export, as the merge choice
    /// says for those already in the list.
    fn import_vault(&mut self, form: PreviewForm, now: f64) {
        let scripted = dev::active();
        let stamp = Profile::new().id;
        let mut serial = 0;
        let chosen = form.chosen();
        // Nothing a script imports may reach the user's own list, nor share
        // an id with it.
        let planned = vault::plan(&chosen, &self.profiles, form.merge, !scripted, || {
            serial += 1;
            if scripted {
                format!("{}vault-{stamp}-{serial}", dev::PROFILE_PREFIX)
            } else {
                format!("{stamp}-{serial}")
            }
        });
        if planned.is_empty() {
            return;
        }

        let keys_dir = keys_dir();
        let (mut added, mut replaced, mut unsaved, mut key_failures) = (0, 0, 0, 0);
        let mut folders: Vec<String> = form.bundle.folders.iter().map(|folder| profiles::folder_path(folder)).collect();
        for item in planned {
            let mut profile = item.profile;
            profile.folder = profiles::folder_path(&profile.folder);
            folders.push(profile.folder.clone());
            if let Some(key) = form.bundle.keys.get(&item.source_id) {
                match keys_dir.as_deref().map(|dir| write_key(dir, key)) {
                    Some(Ok(path)) => profile.key_path = path.display().to_string(),
                    _ => key_failures += 1,
                }
            }
            if form.with_secrets
                && let Some(secret) = form.bundle.secrets.get(&item.source_id)
            {
                if scripted || is_dev(&profile.id) {
                    self.dev_secrets.insert(profile.id.clone(), secret.clone());
                } else if profiles::set_secret(&profile.id, secret).is_err() {
                    unsaved += 1;
                }
            }
            match item.replaces.and_then(|id| self.profiles.iter().position(|saved| saved.id == id)) {
                Some(at) => {
                    self.profiles[at] = profile;
                    replaced += 1;
                }
                None => {
                    self.profiles.push(profile);
                    added += 1;
                }
            }
        }
        // The folders, empty ones included, are kept in one go.
        let before = self.settings.folders.len();
        for folder in folders {
            if !folder.is_empty() && !self.settings.folders.contains(&folder) {
                self.settings.folders.push(folder);
            }
        }
        if self.settings.folders.len() != before {
            self.save_settings();
        }

        let mut text = match (added, replaced) {
            (_, 0) => trn!(added, "{n} connexion importée", "{n} connexions importées"),
            (0, _) => trn!(replaced, "{n} connexion remplacée", "{n} connexions remplacées"),
            _ => tr!("{added} importée(s), {replaced} remplacée(s)", added, replaced),
        };
        let mut failed = false;
        if let Err(error) = self.persist_profiles() {
            text = tr!("Enregistrement impossible : {error}", error = format!("{error:#}"));
            failed = true;
        } else {
            // French says it the same way for one and for more.
            if unsaved > 0 {
                text.push_str(tr!(" ; "));
                text.push_str(&trn!(
                    unsaved,
                    "{n} mot(s) de passe n'ont pas pu être enregistrés et seront à saisir",
                    "{n} mot(s) de passe n'ont pas pu être enregistrés et seront à saisir"
                ));
                failed = true;
            }
            if key_failures > 0 {
                text.push_str(tr!(" ; "));
                text.push_str(&trn!(
                    key_failures,
                    "{n} clé(s) privée(s) n'ont pas pu être recréées",
                    "{n} clé(s) privée(s) n'ont pas pu être recréées"
                ));
                failed = true;
            }
        }
        self.toast(now, text, failed);
    }

    /// Steps of the dev script for this feature (see `App::run_dev_step`).
    pub(super) fn dev_step_hosts(&mut self, verb: &str, argument: &str, _ctx: &egui::Context, now: f64) -> bool {
        let named = |app: &Self, name: &str| app.profile_named(name.trim()).map(|profile| profile.id.clone());
        let (left, right) = argument.split_once('>').map_or((argument, ""), |(a, b)| (a.trim(), b.trim()));
        let on = !matches!(argument, "off" | "non" | "0");
        match verb {
            "hostcolor" => {
                if let Some(id) = named(self, left) {
                    self.apply_hosts(HostsAction::SetColor(id, HostColor::parse(right)), now);
                }
            }
            "foldercolor" => {
                let action = HostsAction::SetFolderColor(profiles::folder_path(left), HostColor::parse(right));
                self.apply_hosts(action, now);
            }
            "hosttags" => {
                if let Some(id) = named(self, left)
                    && let Some(profile) = self.profiles.iter_mut().find(|profile| profile.id == id)
                {
                    profile.tags = hosts::parse_tags(right);
                }
            }
            "tagsdialog" => {
                if let Some(id) = named(self, argument) {
                    self.apply_hosts(HostsAction::AskTags(id), now);
                }
            }
            "formcolor" | "formtags" | "tagtext" => match &mut self.dialog {
                Some(Dialog::Profile(form)) => match verb {
                    "formcolor" => form.draft.color = HostColor::parse(argument),
                    "formtags" => form.draft.tags = hosts::parse_tags(argument),
                    _ => form.hosts.set_input(argument),
                },
                Some(Dialog::Hosts(HostsDialog::Tags(form))) => form.field.set_input(argument),
                _ => {}
            },
            "exportall" => self.apply_hosts(HostsAction::AskExport(ExportScope::All), now),
            "exportfolder" => self.apply_hosts(HostsAction::AskExport(ExportScope::Folder(profiles::folder_path(argument))), now),
            "exporthost" => {
                if let Some(id) = named(self, argument) {
                    self.apply_hosts(HostsAction::AskExport(ExportScope::Host(id)), now);
                }
            }
            "importvault" => self.ask_vault_import(now),
            "vaultpass" | "vaultconfirm" | "vaultsecrets" | "vaultkeys" | "vaulttoggle" | "merge" => {
                match &mut self.dialog {
                    Some(Dialog::Hosts(HostsDialog::Export(form))) => match verb {
                        "vaultpass" => {
                            form.password = argument.to_owned();
                            form.confirm = argument.to_owned();
                        }
                        "vaultconfirm" => form.confirm = argument.to_owned(),
                        "vaultsecrets" => form.include_secrets = on,
                        "vaultkeys" => form.include_keys = on,
                        "vaulttoggle" => form.toggle_named(argument),
                        _ => {}
                    },
                    Some(Dialog::Hosts(HostsDialog::Unlock(form))) if verb == "vaultpass" => {
                        form.password = argument.to_owned();
                    }
                    Some(Dialog::Hosts(HostsDialog::Preview(form))) => match verb {
                        "vaulttoggle" => form.toggle_named(argument),
                        "vaultsecrets" => form.with_secrets = on,
                        "merge" => {
                            form.merge = match argument {
                                "replace" => vault::Merge::Replace,
                                "both" => vault::Merge::KeepBoth,
                                _ => vault::Merge::Skip,
                            }
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
            "restoretabs" => self.apply_hosts(HostsAction::SetRestoreTabs(on), now),
            "savetabs" => {
                let current = self.saved_tabs();
                self.write_tabs(current);
            }
            _ => return false,
        }
        true
    }
}

/// What an export needs, handed to the thread that writes it.
struct ExportJob {
    path: PathBuf,
    profiles: Vec<Profile>,
    folders: Vec<String>,
    secrets: bool,
    keys: bool,
    /// Set while a script drives the app: the secrets to export, in place
    /// of the credential store.
    dev_secrets: Option<HashMap<String, String>>,
    password: Zeroizing<String>,
}

impl ExportJob {
    fn run(self) -> Result<ExportDone> {
        let mut bundle = Bundle {
            exported_at: chrono::Local::now().to_rfc3339(),
            folders: self.folders,
            ..Bundle::default()
        };
        let mut missing_keys = 0;
        for profile in &self.profiles {
            if self.secrets {
                let secret = match &self.dev_secrets {
                    Some(secrets) => secrets.get(&profile.id).cloned(),
                    None => profiles::secret(&profile.id).ok(),
                };
                if let Some(secret) = secret.filter(|secret| !secret.is_empty()) {
                    bundle.secrets.insert(profile.id.clone(), secret);
                }
            }
            // A key that cannot be read is left out, and said so.
            if self.keys {
                match read_key(&profile.key_path, self.dev_secrets.is_some()) {
                    Ok(Some(key)) => {
                        bundle.keys.insert(profile.id.clone(), key);
                    }
                    Ok(None) => {}
                    Err(_) => missing_keys += 1,
                }
            }
        }
        let (count, secrets, keys) = (self.profiles.len(), bundle.secrets.len(), bundle.keys.len());
        bundle.profiles = self.profiles;

        let plain = bundle.to_bytes()?;
        let sealed = vault::seal(&plain, &self.password, KdfParams::STRONG)?;
        // Written next to its place first: a failure never leaves half a file.
        let partial = self.path.with_extension(format!("{}.part", vault::EXTENSION));
        let failed = |path: &Path| tr!("écriture de {path} impossible", path = path.display());
        std::fs::write(&partial, &sealed).with_context(|| failed(&partial))?;
        std::fs::rename(&partial, &self.path).with_context(|| failed(&self.path))?;
        Ok(ExportDone {
            path: self.path,
            count,
            secrets,
            keys,
            missing_keys,
        })
    }
}

/// The private key file a connection names, to travel with it. While a
/// script drives the app, only keys of the temporary folder are read: the
/// user's own never are.
fn read_key(key_path: &str, scripted: bool) -> Result<Option<KeyFile>> {
    let key_path = key_path.trim();
    if key_path.is_empty() {
        return Ok(None);
    }
    let path = Path::new(key_path);
    if scripted && !path.starts_with(std::env::temp_dir()) {
        return Ok(None);
    }
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "cle".to_owned());
    let size = std::fs::metadata(path)
        .with_context(|| tr!("clé privée « {name} » introuvable", name))?
        .len();
    if size > MAX_KEY_FILE {
        bail!(tr!("« {name} » est bien trop gros pour une clé privée", name));
    }
    let content = std::fs::read_to_string(path).with_context(|| tr!("clé privée « {name} » illisible", name))?;
    Ok(Some(KeyFile { name, content }))
}

/// Reads a file to import, after checking that it is an export at all.
fn read_vault(path: &Path) -> Result<Vec<u8>, String> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let unreadable = |error: std::io::Error| tr!("Lecture de « {name} » impossible : {error}", name, error);
    if std::fs::metadata(path).map_err(unreadable)?.len() > MAX_VAULT_FILE {
        return Err(vault::VaultError::NotAVault.to_string());
    }
    let file = std::fs::read(path).map_err(unreadable)?;
    crate::ui::hosts::vault::check(&file).map_err(|error| error.to_string())?;
    Ok(file)
}

/// Where imported key files are recreated: the app's own folder, or, while
/// a script drives the app, next to its export.
fn keys_dir() -> Option<PathBuf> {
    if dev::active() {
        return dev::vault_file().and_then(|file| file.parent().map(|dir| dir.join("keys")));
    }
    let dirs = directories::ProjectDirs::from("", "", crate::APP_ID)?;
    Some(dirs.config_dir().join("keys"))
}

/// Writes an imported key under its own name, or the same name numbered
/// when another key has it. The same key imported twice is written once.
fn write_key(dir: &Path, key: &KeyFile) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let name = safe_component(&key.name);
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem.to_owned(), format!(".{extension}")),
        _ => (name.clone(), String::new()),
    };
    for number in 1..100 {
        let candidate = if number == 1 { name.clone() } else { format!("{stem}-{number}{extension}") };
        let path = dir.join(candidate);
        match std::fs::read(&path) {
            Ok(existing) if existing == key.content.as_bytes() => return Ok(path),
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::write(&path, key.content.as_bytes())?;
                return Ok(path);
            }
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::other(tr!("trop de clés de ce nom")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imported_keys_never_overwrite_another() {
        let dir = std::env::temp_dir().join(format!("s3v-hosts-keys-{}", Profile::new().id));
        let key = |content: &str| KeyFile {
            name: "id_ed25519".to_owned(),
            content: content.to_owned(),
        };
        let first = write_key(&dir, &key("one")).unwrap();
        assert_eq!(first.file_name().unwrap(), "id_ed25519");
        // The same key again: the same file.
        assert_eq!(write_key(&dir, &key("one")).unwrap(), first);
        // Another key of that name: a file of its own.
        let second = write_key(&dir, &key("two")).unwrap();
        assert_eq!(second.file_name().unwrap(), "id_ed25519-2");
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "one");
        let dotted = KeyFile {
            name: "deploy.pem".to_owned(),
            content: "three".to_owned(),
        };
        write_key(&dir, &dotted).unwrap();
        let again = KeyFile {
            content: "four".to_owned(),
            ..dotted
        };
        assert_eq!(write_key(&dir, &again).unwrap().file_name().unwrap(), "deploy-2.pem");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn scripts_never_read_keys_outside_the_temporary_folder() {
        assert!(read_key("C:/Users/someone/.ssh/id_ed25519", true).unwrap().is_none());
        assert!(read_key("", false).unwrap().is_none());
        let path = std::env::temp_dir().join(format!("s3v-hosts-key-{}", Profile::new().id));
        std::fs::write(&path, "-----BEGIN OPENSSH PRIVATE KEY-----").unwrap();
        let key = read_key(&path.display().to_string(), true).unwrap().unwrap();
        assert!(key.content.starts_with("-----BEGIN"));
        let _ = std::fs::remove_file(path);
        assert!(read_key(&std::env::temp_dir().join("s3v-missing-key").display().to_string(), true).is_err());
    }
}
