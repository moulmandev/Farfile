//! Modal dialogs.

use std::collections::{HashMap, HashSet};

use egui::{
    Align, Frame, Id, Key, Label, Layout, Modal, Popup, RichText, ScrollArea, Sense, Stroke, TextEdit, Ui,
};

use super::widgets::{checkbox, danger_button, icon, icon_button, muted, primary_button, protocol_icon, small};
use crate::format;
use crate::import::{self, Candidate, Source};
use crate::launch::Launch;
use crate::profiles::{self, Profile, Protocol};
use crate::storage::RemotePath;
use crate::theme::{Palette, semibold};
use crate::worker::Item;

const FIELD_WIDTH: f32 = 380.0;

pub struct ProfileForm {
    pub draft: Profile,
    pub secret: String,
    /// The port as typed; empty means the default one.
    pub port: String,
    pub is_new: bool,
    pub error: Option<String>,
    /// The folders that exist, offered next to the folder field.
    pub folders: Vec<String>,
    /// SFTP: the identities and jump hosts to pick from.
    pub ssh: super::ssh_auth::FormExtras,
    /// The tag being typed, and the known tags it can complete to.
    pub hosts: super::hosts::TagField,
    /// SFTP only: the editor of its port forwardings.
    pub tunnels: super::tunnels::FormSection,
}

impl ProfileForm {
    pub fn new(draft: Profile, is_new: bool) -> Self {
        Self {
            port: if draft.port == 0 { String::new() } else { draft.port.to_string() },
            draft,
            secret: String::new(),
            is_new,
            error: None,
            folders: Vec::new(),
            ssh: Default::default(),
            hosts: super::hosts::TagField::default(),
            tunnels: Default::default(),
        }
    }

    /// The profile to save, with the typed port folded in.
    pub fn profile(&self) -> Profile {
        let mut profile = self.draft.clone();
        profile.port = self.port.trim().parse().unwrap_or(0);
        self.hosts.finish(&mut profile.tags);
        profile
    }
}

/// One connection found in another tool, and whether to import it.
pub struct ImportItem {
    pub candidate: Candidate,
    pub selected: bool,
    /// Why it starts unticked, when it does.
    pub duplicate: Option<&'static str>,
    /// The saved connection it duplicates, when that one sits in no folder
    /// while this one comes from a folder of the other tool.
    pub refile: Option<String>,
}

pub struct ImportForm {
    pub items: Vec<ImportItem>,
    /// File the saved connections in the folders they have in the other
    /// tool (see [`ImportItem::refile`]).
    pub apply_folders: bool,
    /// The user chose to import an encrypted export instead.
    pub vault: bool,
}

/// A folder of the list of connections being created or renamed.
pub struct FolderForm {
    /// The folder's own name. Slashes in it make further levels.
    pub name: String,
    /// The path of the folder being renamed; none when creating one.
    pub renaming: Option<String>,
    /// The path of the folder it sits in; empty at the first level.
    pub parent: String,
    /// The connection to file in the new folder, if any.
    pub profile: Option<String>,
    /// The paths already taken.
    pub existing: Vec<String>,
}

impl FolderForm {
    /// The path of the folder as typed, if it can be used.
    pub fn path(&self) -> Option<String> {
        profiles::folder_parts(&self.name).next()?;
        let path = profiles::folder_join(&self.parent, &self.name);
        let unchanged = self.renaming.as_deref() == Some(path.as_str());
        let taken = self.existing.contains(&path);
        (unchanged || !taken).then_some(path)
    }
}

impl ImportForm {
    /// Ticks everything that is new: neither saved already, nor found
    /// twice (the first occurrence is kept).
    pub fn new(found: Vec<Candidate>, existing: &[Profile]) -> Self {
        let saved: HashMap<_, _> = existing
            .iter()
            .map(|profile| (import::identity(profile), profile))
            .collect();
        let mut listed = HashSet::new();
        let mut refiled = HashSet::new();
        let items = found
            .into_iter()
            .map(|candidate| {
                let key = candidate.key();
                let mut refile = None;
                let duplicate = if let Some(profile) = saved.get(&key) {
                    if !candidate.group.is_empty()
                        && profile.folder.trim().is_empty()
                        && refiled.insert(profile.id.clone())
                    {
                        refile = Some(profile.id.clone());
                    }
                    Some(tr_key!("déjà enregistrée"))
                } else if !listed.insert(key) {
                    Some(tr_key!("en double"))
                } else {
                    None
                };
                ImportItem {
                    candidate,
                    selected: duplicate.is_none(),
                    duplicate,
                    refile,
                }
            })
            .collect();
        Self {
            items,
            apply_folders: false,
            vault: false,
        }
    }

    /// How many saved connections could be filed in the folder they have
    /// in the tool they came from.
    pub fn refilable(&self) -> usize {
        self.items.iter().filter(|item| item.refile.is_some()).count()
    }
}

pub enum Dialog {
    /// Boxed: the form is much larger than the other dialogs.
    Profile(Box<ProfileForm>),
    Import(ImportForm),
    DeleteProfile { id: String, title: String },
    /// Name or rename a folder of the list of connections.
    Folder(FolderForm),
    /// `name` is the folder's path; `count` and `subfolders` say what it
    /// holds, at any depth.
    DeleteFolder { name: String, count: usize, subfolders: usize },
    DeleteItems { items: Vec<Item>, parent: RemotePath, summary: String },
    NewFolder { name: String },
    /// Give a tab a name of its own; an empty one brings back the
    /// connection's.
    RenameTab { id: u64, name: String },
    /// A file opened in a local program was saved there: send it back?
    PushEdit {
        id: u64,
        name: String,
        /// Where it goes, for display.
        target: String,
        /// Send the next saves of this file without asking.
        always: bool,
    },
    /// Opening a file this big means downloading all of it first.
    OpenLarge { name: String, size: u64, how: Launch },
    /// Leaving while some edited files were never sent.
    Quit { unsent: usize },
    /// The keychain, a passphrase to type, a key to install.
    SshAuth(super::ssh_auth::AuthDialog),
    /// Tags of a connection, encrypted export and import.
    Hosts(super::hosts::HostsDialog),
    /// The appearance of terminals, or a paste to confirm.
    Terminal(super::terminal_look::TerminalDialog),
    /// The snippets, and the values of their variables.
    SshPower(super::ssh_power::PowerDialog),
    /// Add or edit a port forwarding of a connection.
    Tunnel(Box<super::tunnels::RuleDialog>),
    /// Versions, buckets, sharing, metadata: see `crate::ui::s3`.
    S3(Box<super::s3::S3Dialog>),
    /// Move to, clashes, permissions, properties, bookmark name.
    FileOps(super::file_ops::OpsDialog),
    /// Deleting files of this computer, copying to the other view.
    Local(super::local::LocalDialog),
    /// A file in the way of a transfer; the preview of a synchronisation.
    Transfers(crate::ui::transfer_dialogs::TransferDialog),
    /// Unsaved changes, a file changed on the server, the editor chosen.
    Editor(super::editor::EditorDialog),
    /// A newer Farfile, to install.
    Update(super::update::UpdateDialog),
    /// The servers to watch, and the preferences of the monitoring.
    Monitor(super::monitor::MonitorDialog),
}

impl Dialog {
    /// Changes when the dialog needs another height: each protocol has its
    /// own fields, and so has each way of logging in over SSH.
    fn shape(&self) -> Option<(Protocol, u64)> {
        match self {
            Self::Profile(form) => Some((form.draft.protocol, super::ssh_auth::form_shape(form))),
            Self::SshAuth(dialog) => Some((Protocol::Sftp, dialog.shape())),
            _ => None,
        }
    }
}

pub enum Outcome {
    /// Still open.
    Pending,
    Cancelled,
    Confirmed,
    /// Profile form only: save, then open the connection.
    ConfirmedAndConnect,
}

/// Draws the dialog. `first_frame` is true right after it opened, to give
/// the keyboard focus to its first field.
pub fn show(ctx: &egui::Context, p: &Palette, dialog: &mut Dialog, first_frame: bool) -> Outcome {
    let mut outcome = Outcome::Pending;
    let frame = Frame::new()
        .fill(p.bg)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(12)
        .inner_margin(22)
        .shadow(ctx.global_style().visuals.window_shadow);
    // The modal remembers the size of what it showed last, and the rows of
    // buttons stretch to fill it: measure again whenever another dialog
    // opens or the form changes shape. That frame draws nothing, so the
    // first field gets the keyboard on the next one.
    let (shown, focus) = (Id::new("dialog-shape"), Id::new("dialog-focus"));
    let shape = dialog.shape();
    let sizing = ctx.data_mut(|data| {
        let changed = data.get_temp::<Option<(Protocol, u64)>>(shown) != Some(shape);
        data.insert_temp(shown, shape);
        if first_frame {
            data.insert_temp(focus, true);
        }
        first_frame || changed
    });
    let first_frame = !sizing && ctx.data_mut(|data| data.remove_temp::<bool>(focus).unwrap_or(false));

    let id = Id::new("dialog");
    let area = Modal::default_area(id).sizing_pass(sizing);
    let modal = Modal::new(id).area(area).frame(frame).show(ctx, |ui| match dialog {
        Dialog::Profile(form) => profile_form(ui, p, form, first_frame, &mut outcome),
        Dialog::Import(form) => import_form(ui, p, form, &mut outcome),
        Dialog::DeleteProfile { title, .. } => confirm(
            ui,
            p,
            tr!("Supprimer la connexion ?"),
            &tr!("« {title} » sera retirée de la liste. Vos fichiers ne sont pas touchés.", title),
            tr!("Supprimer"),
            &mut outcome,
        ),
        Dialog::Folder(form) => folder_form(ui, p, form, first_frame, &mut outcome),
        Dialog::DeleteFolder {
            name,
            count,
            subfolders,
        } => confirm(
            ui,
            p,
            tr!("Supprimer le dossier ?"),
            &delete_folder_text(name, *count, *subfolders),
            tr!("Supprimer le dossier"),
            &mut outcome,
        ),
        Dialog::DeleteItems { summary, .. } => confirm(
            ui,
            p,
            tr!("Supprimer définitivement ?"),
            &format!("{summary}\n{}", tr!("Cette action est irréversible.")),
            tr!("Supprimer"),
            &mut outcome,
        ),
        Dialog::NewFolder { name } => new_folder(ui, p, name, first_frame, &mut outcome),
        Dialog::RenameTab { name, .. } => rename_tab(ui, p, name, first_frame, &mut outcome),
        Dialog::PushEdit {
            name, target, always, ..
        } => push_edit(ui, p, name, target, always, &mut outcome),
        Dialog::OpenLarge { name, size, .. } => ask(
            ui,
            p,
            tr!("Ouvrir ce gros fichier ?"),
            &tr!(
                "« {name} » pèse {size}. Il sera téléchargé en entier dans un dossier temporaire avant de s'ouvrir.",
                name,
                size = format::size(*size)
            ),
            tr!("Ouvrir"),
            false,
            &mut outcome,
        ),
        Dialog::Quit { unsent } => ask(
            ui,
            p,
            tr!("Quitter sans envoyer ?"),
            &trn!(
                *unsent,
                "Un fichier modifié n'a pas été renvoyé au serveur.\n\
                 Sa copie locale sera conservée une semaine.",
                "{n} fichiers modifiés n'ont pas été renvoyés au serveur.\n\
                 Leurs copies locales seront conservées une semaine."
            ),
            tr!("Quitter"),
            true,
            &mut outcome,
        ),
        Dialog::SshAuth(dialog) => super::ssh_auth::show(ui, p, dialog, first_frame, &mut outcome),
        Dialog::Hosts(dialog) => super::hosts::show_dialog(ui, p, dialog, first_frame, &mut outcome),
        Dialog::Terminal(dialog) => super::terminal_look::show(ui, p, dialog, &mut outcome),
        Dialog::SshPower(dialog) => super::ssh_power::dialog(ui, p, dialog, first_frame, &mut outcome),
        Dialog::Tunnel(form) => super::tunnels::rule_dialog(ui, p, form, first_frame, &mut outcome),
        Dialog::S3(dialog) => super::s3::show(ui, p, dialog, first_frame, &mut outcome),
        Dialog::FileOps(dialog) => super::file_ops::dialog(ui, p, dialog, first_frame, &mut outcome),
        Dialog::Local(dialog) => super::local::dialog(ui, p, dialog, &mut outcome),
        Dialog::Transfers(dialog) => crate::ui::transfer_dialogs::show(ui, p, dialog, &mut outcome),
        Dialog::Editor(dialog) => super::editor::dialog(ui, p, dialog, first_frame, &mut outcome),
        Dialog::Update(dialog) => super::update::dialog(ui, p, dialog, &mut outcome),
        Dialog::Monitor(dialog) => super::monitor::dialog(ui, p, dialog, first_frame, &mut outcome),
    });

    if sizing || dialog.shape() != shape {
        ctx.request_repaint();
    }
    if modal.should_close() && matches!(outcome, Outcome::Pending) {
        outcome = Outcome::Cancelled;
    }
    outcome
}

fn title(ui: &mut Ui, p: &Palette, text: &str) {
    ui.label(RichText::new(text).font(semibold(17.0)).color(p.text));
    ui.add_space(8.0);
}

fn field(ui: &mut Ui, p: &Palette, label: &str, edit: TextEdit<'_>, focus: bool) {
    field_of_width(ui, p, label, edit, FIELD_WIDTH, focus);
}

fn field_of_width(ui: &mut Ui, p: &Palette, label: &str, edit: TextEdit<'_>, width: f32, focus: bool) {
    ui.vertical(|ui| {
        ui.label(small(p, label));
        let response = ui.add(edit.desired_width(width).margin(egui::Margin::symmetric(10, 7)));
        if focus {
            response.request_focus();
        }
    });
    ui.add_space(6.0);
}

/// Host and port side by side.
fn host_and_port(ui: &mut Ui, p: &Palette, host: &mut String, port: &mut String, default_port: u16) {
    const PORT_WIDTH: f32 = 84.0;
    ui.horizontal_top(|ui| {
        field_of_width(
            ui,
            p,
            tr!("Serveur"),
            TextEdit::singleline(host).hint_text(tr!("exemple.fr ou 203.0.113.10")),
            FIELD_WIDTH - PORT_WIDTH - ui.spacing().item_spacing.x,
            false,
        );
        field_of_width(
            ui,
            p,
            tr!("Port"),
            TextEdit::singleline(port).hint_text(default_port.to_string()),
            PORT_WIDTH,
            false,
        );
    });
    port.retain(|c| c.is_ascii_digit());
    port.truncate(5);
}

/// The name, and the folder of the list the connection is filed in. The
/// folder can be typed, to create one, or picked among the existing ones.
fn name_and_folder(
    ui: &mut Ui,
    p: &Palette,
    name: &mut String,
    folder: &mut String,
    folders: &[String],
    focus: bool,
) {
    const FOLDER_WIDTH: f32 = 150.0;
    const PICK_WIDTH: f32 = 30.0;
    let gap = ui.spacing().item_spacing.x;
    ui.horizontal_top(|ui| {
        field_of_width(
            ui,
            p,
            tr!("Nom"),
            TextEdit::singleline(name).hint_text(tr!("Mon serveur")),
            FIELD_WIDTH - FOLDER_WIDTH - gap,
            focus,
        );
        ui.vertical(|ui| {
            ui.label(small(p, tr!("Dossier (facultatif)")));
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                let picking = !folders.is_empty();
                let width = if picking { FOLDER_WIDTH - PICK_WIDTH - 2.0 } else { FOLDER_WIDTH };
                ui.add(
                    TextEdit::singleline(folder)
                        .hint_text(tr!("Aucun"))
                        .desired_width(width)
                        .margin(egui::Margin::symmetric(10, 7)),
                );
                if picking {
                    let pick = icon_button(ui, p, icon::CARET_DOWN, tr!("Choisir un dossier existant"), true);
                    Popup::menu(&pick).show(|ui| {
                        ui.set_min_width(FOLDER_WIDTH);
                        if ui.button(RichText::new(tr!("Aucun")).color(p.muted)).clicked() {
                            folder.clear();
                            ui.close();
                        }
                        for existing in folders {
                            if ui.button(format!("{}  {existing}", icon::FOLDER)).clicked() {
                                folder.clone_from(existing);
                                ui.close();
                            }
                        }
                    });
                }
            });
        });
    });
    ui.add_space(6.0);
}

fn profile_form(ui: &mut Ui, p: &Palette, form: &mut ProfileForm, first_frame: bool, outcome: &mut Outcome) {
    ui.set_width(FIELD_WIDTH);
    title(
        ui,
        p,
        if form.is_new {
            tr!("Nouvelle connexion")
        } else {
            tr!("Modifier la connexion")
        },
    );

    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for protocol in Protocol::ALL {
            let selected = form.draft.protocol == protocol;
            let text = RichText::new(protocol.label()).color(if selected { p.on_accent } else { p.text });
            let mut button = egui::Button::new(text).min_size(egui::vec2(72.0, 0.0));
            if selected {
                button = button.fill(p.accent);
            }
            if ui.add(button).clicked() {
                form.draft.protocol = protocol;
            }
        }
    });
    ui.add_space(8.0);

    let is_new = form.is_new;
    let keep_hint = |otherwise: &'static str| if is_new { otherwise } else { tr!("Laisser vide pour ne pas changer") };
    let draft = &mut form.draft;
    name_and_folder(ui, p, &mut draft.name, &mut draft.folder, &form.folders, first_frame);
    super::hosts::form_section(ui, p, draft, &mut form.hosts);

    match draft.protocol {
        Protocol::S3 => {
            field(
                ui,
                p,
                tr!("Serveur (endpoint)"),
                TextEdit::singleline(&mut draft.endpoint)
                    .hint_text(tr!("s3.eu-west-2.wasabisys.com — vide pour Amazon S3")),
                false,
            );
            field(
                ui,
                p,
                tr!("Région"),
                TextEdit::singleline(&mut draft.region).hint_text(trc!("région", "Automatique")),
                false,
            );
            field(ui, p, tr!("Clé d'accès"), TextEdit::singleline(&mut draft.user), false);
            field(
                ui,
                p,
                tr!("Clé secrète"),
                TextEdit::singleline(&mut form.secret)
                    .password(true)
                    .hint_text(keep_hint("")),
                false,
            );
            field(
                ui,
                p,
                tr!("Dossier de départ (facultatif)"),
                TextEdit::singleline(&mut draft.initial_path).hint_text(tr!("/mon-bucket/dossier")),
                false,
            );
            checkbox(ui, p, &mut draft.path_style, tr!("Adressage « path-style »")).on_hover_text(tr!(
                "Recommandé pour la plupart des fournisseurs compatibles S3,\n\
                 et indispensable pour les buckets dont le nom contient des points."
            ));
        }
        Protocol::Sftp => {
            host_and_port(ui, p, &mut draft.endpoint, &mut form.port, Protocol::Sftp.default_port());
            // User or identity, method, secrets, jump host, start folder.
            super::ssh_auth::credentials(ui, p, form);
            super::terminal_ext::profile_fields(ui, p, &mut form.draft);
            super::ssh_power::startup_field(ui, p, &mut form.draft.startup);
            super::tunnels::form_section(ui, p, &mut form.draft.forwards, &mut form.tunnels);
        }
        Protocol::Ftp => {
            host_and_port(ui, p, &mut draft.endpoint, &mut form.port, Protocol::Ftp.default_port());
            field(
                ui,
                p,
                tr!("Utilisateur"),
                TextEdit::singleline(&mut draft.user).hint_text(tr!("Vide pour une session anonyme")),
                false,
            );
            field(
                ui,
                p,
                tr!("Mot de passe"),
                TextEdit::singleline(&mut form.secret)
                    .password(true)
                    .hint_text(keep_hint("")),
                false,
            );
            field(
                ui,
                p,
                tr!("Dossier de départ (facultatif)"),
                TextEdit::singleline(&mut draft.initial_path).hint_text(tr!("Dossier personnel")),
                false,
            );
            checkbox(ui, p, &mut draft.tls, tr!("Chiffrer la session (FTPS explicite)")).on_hover_text(tr!(
                "Sans chiffrement, FTP envoie le mot de passe et les fichiers en clair.\n\
                 Le certificat du serveur doit être reconnu par le système."
            ));
        }
        // Built in, never edited here.
        Protocol::Local => {}
    }
    ui.label(small(
        p,
        tr!("Le secret est conservé dans le gestionnaire d'identifiants du système."),
    ));

    if let Some(error) = &form.error {
        ui.add_space(4.0);
        ui.label(RichText::new(error).color(p.danger));
    }

    ui.add_space(12.0);
    let draft = &form.draft;
    let complete = match draft.protocol {
        Protocol::S3 => {
            !draft.user.trim().is_empty() && (!form.is_new || !form.secret.trim().is_empty())
        }
        Protocol::Sftp => !draft.endpoint.trim().is_empty() && super::ssh_auth::complete(form),
        Protocol::Ftp => !draft.endpoint.trim().is_empty(),
        Protocol::Local => false,
    };
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if primary_button(ui, p, tr!("Enregistrer et ouvrir"), complete).clicked() {
            *outcome = Outcome::ConfirmedAndConnect;
        }
        if ui.add_enabled(complete, egui::Button::new(tr!("Enregistrer"))).clicked() {
            *outcome = Outcome::Confirmed;
        }
        if ui.button(tr!("Annuler")).clicked() {
            *outcome = Outcome::Cancelled;
        }
    });
}

const IMPORT_WIDTH: f32 = 560.0;

fn import_form(ui: &mut Ui, p: &Palette, form: &mut ImportForm, outcome: &mut Outcome) {
    ui.set_width(IMPORT_WIDTH);
    title(ui, p, tr!("Importer des connexions"));
    super::hosts::vault_source(ui, p, &mut form.vault, outcome);

    if form.items.is_empty() {
        ui.label(muted(
            p,
            tr!(
                "Aucune connexion trouvée dans FileZilla, dans les profils SSH de Windows Terminal\n\
                 ni dans ~/.ssh/config."
            ),
        ));
        ui.add_space(14.0);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.button(tr!("Fermer")).clicked() {
                *outcome = Outcome::Cancelled;
            }
        });
        return;
    }

    let selected = form.items.iter().filter(|item| item.selected).count();
    ui.horizontal(|ui| {
        ui.label(muted(
            p,
            tr!("{found} trouvées · {selected} sélectionnées", found = form.items.len(), selected),
        ));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let link = |ui: &mut Ui, text: &str| {
                ui.add(Label::new(RichText::new(text).size(12.5).color(p.accent)).sense(Sense::click()))
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
            };
            if link(ui, tr!("Tout décocher")) {
                form.items.iter_mut().for_each(|item| item.selected = false);
            }
            if link(ui, tr!("Tout cocher")) {
                form.items.iter_mut().for_each(|item| item.selected = true);
            }
        });
    });
    ui.add_space(4.0);

    ScrollArea::vertical()
        .max_height(400.0)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            let mut current: Option<Source> = None;
            for item in &mut form.items {
                if current != Some(item.candidate.source) {
                    current = Some(item.candidate.source);
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(item.candidate.source.label().to_uppercase())
                            .size(11.0)
                            .color(p.faint),
                    );
                    ui.add_space(2.0);
                }
                import_row(ui, p, item);
            }
        });

    ui.add_space(10.0);
    let refilable = form.refilable();
    if refilable > 0 {
        let label = trn!(
            refilable,
            "Ranger aussi {n} connexion déjà enregistrée dans son dossier d'origine",
            "Ranger aussi {n} connexions déjà enregistrées dans leur dossier d'origine"
        );
        checkbox(ui, p, &mut form.apply_folders, label).on_hover_text(tr!(
            "Les connexions importées avant l'arrivée des dossiers retrouvent\n\
             celui qu'elles ont dans FileZilla. Celles déjà rangées ne bougent pas."
        ));
        ui.add_space(6.0);
    }
    ui.label(small(
        p,
        tr!(
            "Les mots de passe importés sont rangés dans le gestionnaire d'identifiants du système.\n\
             Rien n'est modifié dans les applications d'origine."
        ),
    ));
    ui.add_space(10.0);
    let refiling = form.apply_folders && refilable > 0;
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        let label = if selected > 1 {
            tr!("Importer {selected} connexions", selected)
        } else if selected == 0 && refiling {
            tr!("Ranger").to_owned()
        } else {
            tr!("Importer").to_owned()
        };
        if primary_button(ui, p, &label, selected > 0 || refiling).clicked() {
            *outcome = Outcome::Confirmed;
        }
        if ui.button(tr!("Annuler")).clicked() {
            *outcome = Outcome::Cancelled;
        }
    });
}

fn import_row(ui: &mut Ui, p: &Palette, item: &mut ImportItem) {
    let candidate = &item.candidate;
    let profile = &candidate.profile;

    let mut where_to = format!("{} · {}", profile.protocol.label(), profile.summary());
    if profile.port != 0 {
        where_to.push_str(&format!(":{}", profile.port));
    }
    if candidate.secret.is_some() {
        where_to.push_str(&format!(" · {}", tr!("mot de passe")));
    } else if !profile.key_path.is_empty() {
        let key = profile.key_path.rsplit(['/', '\\']).next().unwrap_or("");
        where_to.push_str(&format!(" · {}", tr!("clé {key}", key)));
    }
    if let Some(reason) = item.duplicate {
        where_to.push_str(&format!(" · {}", crate::i18n::lookup(reason)));
    }

    ui.horizontal(|ui| {
        checkbox(ui, p, &mut item.selected, "");
        ui.label(
            RichText::new(protocol_icon(profile.protocol))
                .size(17.0)
                .color(if item.selected { super::hosts::icon_color(p, profile, true) } else { p.faint }),
        );
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let mut name = RichText::new(profile.title()).color(if item.selected { p.text } else { p.muted });
            if !candidate.group.is_empty() {
                name = RichText::new(format!("{}   ·   {}", profile.title(), candidate.group))
                    .color(if item.selected { p.text } else { p.muted });
            }
            // Clicking the name ticks the box too.
            if ui
                .add(Label::new(name).truncate().sense(Sense::click()))
                .clicked()
            {
                item.selected = !item.selected;
            }
            ui.add(Label::new(RichText::new(where_to).size(11.5).color(p.muted)).truncate());
            if let Some(note) = &candidate.note {
                ui.add(Label::new(RichText::new(note).size(11.5).color(p.folder)).wrap());
            }
        });
    });
    ui.add_space(4.0);
}

fn confirm(ui: &mut Ui, p: &Palette, heading: &str, body: &str, action: &str, outcome: &mut Outcome) {
    ask(ui, p, heading, body, action, true, outcome);
}

/// A question with one way forward; `destructive` paints it as a warning.
fn ask(
    ui: &mut Ui,
    p: &Palette,
    heading: &str,
    body: &str,
    action: &str,
    destructive: bool,
    outcome: &mut Outcome,
) {
    ui.set_width(FIELD_WIDTH);
    title(ui, p, heading);
    ui.add(Label::new(muted(p, body)).wrap());
    ui.add_space(14.0);
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        let confirmed = if destructive {
            danger_button(ui, p, action).clicked()
        } else {
            primary_button(ui, p, action, true).clicked()
        };
        if confirmed {
            *outcome = Outcome::Confirmed;
        }
        if ui.button(tr!("Annuler")).clicked() {
            *outcome = Outcome::Cancelled;
        }
    });
}

fn push_edit(ui: &mut Ui, p: &Palette, name: &str, target: &str, always: &mut bool, outcome: &mut Outcome) {
    ui.set_width(FIELD_WIDTH + 40.0);
    title(ui, p, tr!("Envoyer les modifications ?"));
    ui.add(Label::new(RichText::new(tr!("« {name} » vient d'être enregistré.", name)).color(p.text)).wrap());
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new(icon::ARROW_BEND_DOWN_RIGHT).color(p.faint));
        ui.add(Label::new(muted(p, target)).truncate());
    });
    ui.add_space(10.0);
    checkbox(ui, p, always, tr!("Envoyer les prochains enregistrements sans demander"));
    ui.add_space(12.0);
    if ui.input(|input| input.key_pressed(Key::Enter)) {
        *outcome = Outcome::Confirmed;
    }
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if primary_button(ui, p, &format!("{}  {}", icon::UPLOAD_SIMPLE, tr!("Envoyer")), true).clicked() {
            *outcome = Outcome::Confirmed;
        }
        if ui
            .button(tr!("Plus tard"))
            .on_hover_text(tr!(
                "Les modifications restent sur cet ordinateur ; le bouton en bas de la fenêtre permet de les envoyer."
            ))
            .clicked()
        {
            *outcome = Outcome::Cancelled;
        }
    });
}

/// What deleting a folder does: only the folders go, never a connection.
fn delete_folder_text(path: &str, count: usize, subfolders: usize) -> String {
    let name = profiles::folder_name(path);
    let parent = profiles::folder_parent(path);
    let removed = match subfolders {
        0 => tr!("« {name} » sera retiré de la liste.", name),
        _ => trn!(
            subfolders,
            "« {name} » et le dossier qu'il contient seront retirés de la liste.",
            "« {name} » et les {n} dossiers qu'il contient seront retirés de la liste.",
            name
        ),
    };
    if count == 0 {
        return removed;
    }
    let kept = if parent.is_empty() {
        trn!(
            count,
            "La connexion qui s'y trouve est conservée, hors dossier.",
            "Les {n} connexions qui s'y trouvent sont conservées, hors dossier."
        )
    } else {
        trn!(
            count,
            "La connexion qui s'y trouve est conservée, dans « {parent} ».",
            "Les {n} connexions qui s'y trouvent sont conservées, dans « {parent} ».",
            parent = profiles::folder_name(&parent)
        )
    };
    format!("{removed} {kept}")
}

fn folder_form(ui: &mut Ui, p: &Palette, form: &mut FolderForm, first_frame: bool, outcome: &mut Outcome) {
    ui.set_width(FIELD_WIDTH);
    let renaming = form.renaming.is_some();
    let heading = if renaming {
        tr!("Renommer le dossier")
    } else if form.parent.is_empty() {
        tr!("Nouveau dossier")
    } else {
        tr!("Nouveau sous-dossier")
    };
    title(ui, p, heading);
    if !form.parent.is_empty() {
        ui.label(small(p, format!("{}  {}", icon::FOLDER, tr!("Dans {folder}", folder = form.parent))));
        ui.add_space(4.0);
    }
    field(
        ui,
        p,
        tr!("Nom du dossier"),
        TextEdit::singleline(&mut form.name).hint_text(tr!("Production, Clients, Jeux…")),
        first_frame,
    );

    let valid = form.path().is_some();
    if !valid && !form.name.trim().is_empty() {
        ui.label(RichText::new(tr!("Un dossier porte déjà ce nom.")).size(12.0).color(p.danger));
    } else if form.profile.is_some() {
        ui.label(small(p, tr!("La connexion y sera rangée.")));
    }
    if valid && ui.input(|input| input.key_pressed(Key::Enter)) {
        *outcome = Outcome::Confirmed;
    }
    ui.add_space(8.0);
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if primary_button(ui, p, if renaming { tr!("Renommer") } else { tr!("Créer") }, valid).clicked() {
            *outcome = Outcome::Confirmed;
        }
        if ui.button(tr!("Annuler")).clicked() {
            *outcome = Outcome::Cancelled;
        }
    });
}

fn rename_tab(ui: &mut Ui, p: &Palette, name: &mut String, first_frame: bool, outcome: &mut Outcome) {
    ui.set_width(FIELD_WIDTH);
    title(ui, p, tr!("Renommer l'onglet"));
    field(
        ui,
        p,
        tr!("Nom de l'onglet"),
        TextEdit::singleline(name).hint_text(tr!("Vide pour reprendre le nom de la connexion")),
        first_frame,
    );
    ui.label(small(p, tr!("Seul cet onglet change de nom, pas la connexion.")));
    if ui.input(|input| input.key_pressed(Key::Enter)) {
        *outcome = Outcome::Confirmed;
    }
    ui.add_space(8.0);
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if primary_button(ui, p, tr!("Renommer"), true).clicked() {
            *outcome = Outcome::Confirmed;
        }
        if ui.button(tr!("Annuler")).clicked() {
            *outcome = Outcome::Cancelled;
        }
    });
}

fn new_folder(ui: &mut Ui, p: &Palette, name: &mut String, first_frame: bool, outcome: &mut Outcome) {
    ui.set_width(FIELD_WIDTH);
    title(ui, p, tr!("Nouveau dossier"));
    field(
        ui,
        p,
        tr!("Nom du dossier"),
        TextEdit::singleline(name),
        first_frame,
    );

    let valid = !name.trim().is_empty() && !name.contains('/');
    if valid && ui.input(|input| input.key_pressed(Key::Enter)) {
        *outcome = Outcome::Confirmed;
    }
    ui.add_space(8.0);
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if primary_button(ui, p, tr!("Créer"), valid).clicked() {
            *outcome = Outcome::Confirmed;
        }
        if ui.button(tr!("Annuler")).clicked() {
            *outcome = Outcome::Cancelled;
        }
    });
}
