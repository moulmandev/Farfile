//! The screens of SSH authentication: its part of the connection form
//! (identity, method, key, jump host), the keychain and its identities, key
//! generation, the passphrase question asked while connecting, and the
//! installation of a public key on a server. They only edit their own
//! state and leave requests in it; `app::ssh_auth` acts on them.

use std::collections::HashMap;

use egui::{Align, Button, Label, Layout, Margin, Popup, Response, RichText, ScrollArea, Sense, TextEdit, Ui, vec2};

use super::Action;
use super::dialogs::{Outcome, ProfileForm};
use super::widgets::{checkbox, danger_button, icon, icon_button, muted, primary_button, small};
use crate::profiles::Profile;
use crate::ssh_auth::keychain::Identity;
use crate::ssh_auth::keys::{self, KeyType};
use crate::ssh_auth::{AuthMethod, PassphraseRequest};
use crate::theme::{Palette, semibold};

/// As wide as the other dialogs.
const WIDTH: f32 = 380.0;
/// The keychain shows a list: a little more room.
const LIST_WIDTH: f32 = 470.0;
const BROWSE: f32 = 104.0;
/// The lists of the pickers scroll past this height.
const LIST_HEIGHT: f32 = 300.0;

pub enum SshAuthAction {
    OpenKeychain,
    /// Add the public key of an identity to the server of this tab.
    AskInstallOnTab(u64),
    /// … of this saved connection.
    AskInstallOnProfile(String),
}

// ---- Small pieces ----------------------------------------------------------

fn title(ui: &mut Ui, p: &Palette, text: &str) {
    ui.label(RichText::new(text).font(semibold(17.0)).color(p.text));
    ui.add_space(8.0);
}

fn field(ui: &mut Ui, p: &Palette, label: &str, edit: TextEdit<'_>, width: f32, focus: bool) -> Response {
    let response = ui
        .vertical(|ui| {
            ui.label(small(p, label));
            let response = ui.add(edit.desired_width(width).margin(Margin::symmetric(10, 7)));
            if focus {
                response.request_focus();
            }
            response
        })
        .inner;
    ui.add_space(6.0);
    response
}

/// A button showing the current choice, that opens the list to pick from.
fn picker(ui: &mut Ui, p: &Palette, width: f32, text: &str, color: egui::Color32) -> Response {
    let caret = RichText::new(icon::CARET_DOWN).color(p.muted);
    ui.add(
        Button::new(RichText::new(text).color(color))
            .right_text(caret)
            .truncate()
            .min_size(vec2(width, 30.0)),
    )
}

fn link(ui: &mut Ui, p: &Palette, text: &str) -> bool {
    ui.add(Label::new(RichText::new(text).size(12.5).color(p.accent)).sense(Sense::click()))
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
}

fn menu_item(ui: &mut Ui, text: impl Into<egui::WidgetText>) -> bool {
    let clicked = ui.button(text).clicked();
    if clicked {
        ui.close();
    }
    clicked
}

fn error_line(ui: &mut Ui, p: &Palette, error: &Option<String>) {
    if let Some(error) = error {
        ui.add_space(4.0);
        ui.add(Label::new(RichText::new(error).color(p.danger)).wrap());
    }
}

/// A key file field with its "Parcourir…" button. Native pickers never
/// open while a script drives the app.
fn key_field(ui: &mut Ui, p: &Palette, label: &str, path: &mut String, hint: &str, width: f32) {
    ui.label(small(p, label));
    ui.horizontal(|ui| {
        ui.add(
            TextEdit::singleline(path)
                .hint_text(hint)
                .desired_width(width - BROWSE - ui.spacing().item_spacing.x)
                .margin(Margin::symmetric(10, 7)),
        );
        if ui.add_sized([BROWSE, 30.0], Button::new("Parcourir…")).clicked() && !crate::dev::active() {
            let mut dialog = rfd::FileDialog::new().set_title("Clé privée SSH");
            if let Some(home) = directories::UserDirs::new() {
                dialog = dialog.set_directory(home.home_dir().join(".ssh"));
            }
            if let Some(picked) = dialog.pick_file() {
                *path = picked.display().to_string();
            }
        }
    });
    ui.add_space(6.0);
}

fn method_switch(ui: &mut Ui, p: &Palette, current: &mut AuthMethod) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let width = (WIDTH - 3.0 * 4.0) / 4.0;
        for method in AuthMethod::ALL {
            let selected = *current == method;
            let text = RichText::new(method.label()).color(if selected { p.on_accent } else { p.text });
            let mut button = Button::new(text);
            if selected {
                button = button.fill(p.accent);
            }
            if ui.add_sized([width, 28.0], button).on_hover_text(method.hint()).clicked() {
                *current = method;
            }
        }
    });
}

/// The key file's name and type, its fingerprint, and "Copier la clé
/// publique", once the key is read.
fn public_key_line(ui: &mut Ui, p: &Palette, path: &str, public: &Result<String, String>) -> bool {
    let mut copy = false;
    match public {
        Ok(line) => {
            let name = std::path::Path::new(path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            ui.horizontal(|ui| {
                ui.scope(|ui| {
                    ui.set_max_width(WIDTH - 190.0);
                    let title = format!("{}  {name}", icon::KEY);
                    ui.add(Label::new(RichText::new(title).size(12.5).color(p.text)).truncate())
                        .on_hover_text(path);
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    copy = ui.button(format!("{}  Copier la clé publique", icon::COPY_SIMPLE)).clicked();
                });
            });
            let fingerprint = format!("{} · {}", keys::kind(line), keys::fingerprint(line).unwrap_or_default());
            ui.add(Label::new(RichText::new(fingerprint).size(11.5).color(p.muted)).truncate());
        }
        Err(error) => {
            ui.add(Label::new(RichText::new(error).size(12.0).color(p.danger)).wrap());
        }
    }
    ui.add_space(4.0);
    copy
}

// ---- The connection form ---------------------------------------------------

/// What the connection form offers besides its own fields; the app fills it
/// when the form opens.
#[derive(Default)]
pub struct FormExtras {
    pub identities: Vec<Identity>,
    /// The connections this one may go through, without loops, as
    /// (id, name, address).
    pub jumps: Vec<(String, String, String)>,
    /// A keychain screen to open over the form.
    pub request: Option<FormRequest>,
}

pub enum FormRequest {
    Keychain,
    NewIdentity,
    EditIdentity(String),
}

/// The SSH part of the connection form, below the server and its port:
/// who logs in (or which identity), how, and through which jump host.
pub fn credentials(ui: &mut Ui, p: &Palette, form: &mut ProfileForm) {
    let gap = ui.spacing().item_spacing.x;
    let half = (WIDTH - gap) / 2.0;
    let is_new = form.is_new;
    let keep = |otherwise: &'static str| if is_new { otherwise } else { "Laisser vide pour ne pas changer" };
    let ProfileForm { draft, secret, ssh, .. } = form;
    let identity = ssh.identities.iter().find(|identity| identity.id == draft.identity).cloned();

    ui.horizontal_top(|ui| {
        match identity.as_ref().map(|identity| identity.user.trim()).filter(|user| !user.is_empty()) {
            Some(user) => {
                let mut shown = user.to_owned();
                field(ui, p, "Utilisateur (de l'identité)", TextEdit::singleline(&mut shown).interactive(false), half, false);
            }
            None => {
                field(ui, p, "Utilisateur", TextEdit::singleline(&mut draft.user), half, false);
            }
        }
        ui.vertical(|ui| {
            ui.label(small(p, "Identité du trousseau"));
            let (text, color) = match &identity {
                Some(identity) => (format!("{}  {}", icon::IDENTIFICATION_BADGE, identity.title()), p.text),
                None if !draft.identity.is_empty() => ("Identité supprimée".to_owned(), p.danger),
                None => ("Aucune".to_owned(), p.muted),
            };
            let pick = picker(ui, p, half, &text, color);
            Popup::menu(&pick).show(|ui| {
                ui.set_min_width(half);
                if menu_item(ui, RichText::new("Aucune : les champs de la connexion").color(p.muted)) {
                    draft.identity.clear();
                }
                ScrollArea::vertical().max_height(LIST_HEIGHT).show(ui, |ui| {
                    for choice in &ssh.identities {
                        let mark = if choice.id == draft.identity { icon::CHECK } else { icon::IDENTIFICATION_BADGE };
                        let response = ui.button(format!("{mark}  {}", choice.title())).on_hover_text(choice.summary());
                        if response.clicked() {
                            draft.identity.clone_from(&choice.id);
                            ui.close();
                        }
                    }
                });
                ui.separator();
                if menu_item(ui, format!("{}  Nouvelle identité…", icon::PLUS)) {
                    ssh.request = Some(FormRequest::NewIdentity);
                }
                if let Some(identity) = &identity
                    && menu_item(ui, format!("{}  Modifier « {} »…", icon::PENCIL_SIMPLE, identity.title()))
                {
                    ssh.request = Some(FormRequest::EditIdentity(identity.id.clone()));
                }
                if menu_item(ui, format!("{}  Gérer le trousseau…", icon::KEY)) {
                    ssh.request = Some(FormRequest::Keychain);
                }
            });
        });
    });
    ui.add_space(6.0);

    ui.label(small(p, "Authentification"));
    method_switch(ui, p, &mut draft.auth);
    ui.add(Label::new(small(p, draft.auth.hint())).wrap());
    ui.add_space(8.0);

    match &identity {
        Some(identity) => {
            let mut line = format!("{}  L'identité « {} » fournit : {}", icon::IDENTIFICATION_BADGE, identity.title(), identity.summary());
            if draft.auth == AuthMethod::Agent {
                line = format!("{}  L'identité « {} » fournit l'utilisateur ; les clés viennent de l'agent.", icon::IDENTIFICATION_BADGE, identity.title());
            }
            ui.add(Label::new(small(p, line)).wrap());
            ui.add_space(8.0);
        }
        None => match draft.auth {
            AuthMethod::Agent => {}
            AuthMethod::Password => {
                field(ui, p, "Mot de passe", TextEdit::singleline(secret).password(true).hint_text(keep("")), WIDTH, false);
            }
            AuthMethod::Key => {
                key_field(ui, p, "Clé privée", &mut draft.key_path, "Fichier de la clé (OpenSSH, PEM ou PuTTY)", WIDTH);
                let hint = keep("Vide : demandée à la connexion si la clé en a une");
                field(ui, p, "Phrase secrète de la clé", TextEdit::singleline(secret).password(true).hint_text(hint), WIDTH, false);
            }
            AuthMethod::Auto => {
                let with_key = !draft.key_path.trim().is_empty();
                let (label, hint) = if with_key {
                    ("Phrase secrète de la clé", keep("Vide : demandée à la connexion si la clé en a une"))
                } else {
                    ("Mot de passe", keep(""))
                };
                field(ui, p, label, TextEdit::singleline(secret).password(true).hint_text(hint), WIDTH, false);
                key_field(ui, p, "Clé privée (facultatif)", &mut draft.key_path, "Sinon : l'agent SSH, puis les clés de ~/.ssh", WIDTH);
            }
        },
    }

    ui.horizontal_top(|ui| {
        ui.vertical(|ui| {
            ui.label(small(p, "Passer par (rebond)"));
            let chosen = ssh.jumps.iter().find(|(id, ..)| *id == draft.jump);
            let (text, color) = match chosen {
                Some((_, name, _)) => (format!("{}  {name}", icon::FLOW_ARROW), p.text),
                None if !draft.jump.is_empty() => ("Rebond indisponible".to_owned(), p.danger),
                None => ("Connexion directe".to_owned(), p.muted),
            };
            let hover = if chosen.is_none() && !draft.jump.is_empty() {
                "Ce serveur de rebond n'existe plus, ou repasse par cette connexion :\nchoisissez-en un autre."
            } else {
                "Comme ProxyJump : la connexion passe par ce serveur SSH,\nqui doit autoriser la redirection TCP."
            };
            let pick = picker(ui, p, half, &text, color).on_hover_text(hover);
            Popup::menu(&pick).show(|ui| {
                ui.set_min_width(WIDTH);
                if menu_item(ui, RichText::new("Connexion directe").color(p.muted)) {
                    draft.jump.clear();
                }
                if ssh.jumps.is_empty() {
                    ui.label(small(p, "Aucune autre connexion SSH enregistrée."));
                }
                ScrollArea::vertical().max_height(LIST_HEIGHT).show(ui, |ui| {
                    for (id, name, address) in &ssh.jumps {
                        let mark = if *id == draft.jump { icon::CHECK } else { icon::TERMINAL_WINDOW };
                        let text = RichText::new(format!("{mark}  {name}")).color(p.text);
                        let response = ui.add(Button::new(text).right_text(RichText::new(address).size(12.0).color(p.muted)));
                        if response.clicked() {
                            draft.jump.clone_from(id);
                            ui.close();
                        }
                    }
                });
            });
        });
        field(
            ui,
            p,
            "Dossier de départ (facultatif)",
            TextEdit::singleline(&mut draft.initial_path).hint_text("Dossier personnel"),
            half,
            false,
        );
    });
}

/// Changes whenever the SSH part of the form shows other fields, so that
/// the dialog measures itself again.
pub fn form_shape(form: &ProfileForm) -> u64 {
    if form.draft.protocol != crate::profiles::Protocol::Sftp {
        return 0;
    }
    let identity = form.ssh.identities.iter().any(|identity| identity.id == form.draft.identity);
    1 + form.draft.auth as u64 * 2 + u64::from(identity)
}

/// Whether the SSH part of the form says enough to try connecting.
pub fn complete(form: &ProfileForm) -> bool {
    let draft = &form.draft;
    let has_identity = form.ssh.identities.iter().any(|identity| identity.id == draft.identity);
    let user = !draft.user.trim().is_empty() || has_identity;
    let key = draft.auth != AuthMethod::Key || has_identity || !draft.key_path.trim().is_empty();
    user && key
}

// ---- The dialogs ---------------------------------------------------------

pub enum AuthDialog {
    Keychain(KeychainView),
    Identity(Box<IdentityForm>),
    Generate(Box<GenerateForm>),
    DeleteIdentity { id: String, title: String, users: usize },
    Passphrase(Box<PassphraseForm>),
    Install(Box<InstallForm>),
}

/// The list of identities.
pub struct KeychainView {
    pub identities: Vec<Identity>,
    /// How many connections use each, by id.
    pub users: HashMap<String, usize>,
    pub request: Option<KeychainRequest>,
}

pub enum KeychainRequest {
    New,
    Edit(String),
    Delete(String),
}

pub struct IdentityForm {
    pub identity: Identity,
    pub is_new: bool,
    /// As typed; empty keeps what is stored.
    pub password: String,
    pub passphrase: String,
    pub forget_password: bool,
    pub forget_passphrase: bool,
    /// The public key of the key file, and the path it was read from.
    pub public: Option<(String, Result<String, String>)>,
    pub request: Option<IdentityRequest>,
    pub error: Option<String>,
}

pub enum IdentityRequest {
    Generate,
    CopyPublic(String),
}

impl IdentityForm {
    pub fn new(identity: Identity, is_new: bool) -> Self {
        Self {
            identity,
            is_new,
            password: String::new(),
            passphrase: String::new(),
            forget_password: false,
            forget_passphrase: false,
            public: None,
            request: None,
            error: None,
        }
    }

    /// Reads the public half of the key file again when its path changed.
    fn refresh_public(&mut self) {
        let path = self.identity.key_path.trim().to_owned();
        if self.public.as_ref().is_some_and(|(read, _)| *read == path) {
            return;
        }
        let public = if path.is_empty() {
            None
        } else {
            let passphrase = (!self.passphrase.is_empty()).then_some(self.passphrase.as_str());
            let read = keys::public_line(std::path::Path::new(&path), passphrase).map_err(|_| {
                "Clé introuvable ou illisible sans sa phrase secrète (et pas de .pub à côté).".to_owned()
            });
            Some((path, read))
        };
        self.public = public;
    }
}

pub struct GenerateForm {
    pub kind: KeyType,
    pub passphrase: String,
    pub confirm: String,
    /// Where the files go, and the private key's own name.
    pub folder: std::path::PathBuf,
    pub name: String,
    /// The name was not typed by hand: it follows the type of key.
    pub name_auto: bool,
    pub comment: String,
    /// The identity it is for, which names the file.
    pub for_identity: String,
    pub busy: bool,
    pub error: Option<String>,
}

impl GenerateForm {
    pub fn new(identity: &str, user: &str) -> Self {
        let mut form = Self {
            kind: KeyType::Ed25519,
            passphrase: String::new(),
            confirm: String::new(),
            folder: crate::ssh_auth::keychain::key_folder(),
            name: String::new(),
            name_auto: true,
            comment: if user.trim().is_empty() {
                crate::APP_NAME.to_owned()
            } else {
                format!("{}@{}", user.trim(), crate::APP_NAME.replace(' ', "-"))
            },
            for_identity: identity.to_owned(),
            busy: false,
            error: None,
        };
        form.suggest_name();
        form
    }

    /// A free name in the folder, after the type of key and the identity.
    pub fn suggest_name(&mut self) {
        let path = crate::ssh_auth::keychain::new_key_path(&self.folder, self.kind.file_name(), &self.for_identity);
        self.name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    }

    pub fn path(&self) -> std::path::PathBuf {
        self.folder.join(self.name.trim())
    }

    pub fn valid(&self) -> bool {
        let name = self.name.trim();
        !name.is_empty() && !name.contains(['/', '\\']) && self.passphrase == self.confirm
    }
}

pub struct PassphraseForm {
    pub request: PassphraseRequest,
    pub passphrase: String,
    pub save: bool,
}

/// Where a public key is to be installed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallTarget {
    Tab(u64),
    Profile(String),
}

pub struct InstallForm {
    pub target: InstallTarget,
    /// The connection, for display.
    pub title: String,
    /// The account the key is added to, `user@host`.
    pub account: String,
    pub user: String,
    /// The identities that have a key.
    pub identities: Vec<Identity>,
    pub selected: Option<String>,
    /// Then connect to this server with that identity.
    pub use_it: bool,
    pub busy: bool,
    pub error: Option<String>,
    pub request: Option<InstallRequest>,
}

pub enum InstallRequest {
    NewIdentity,
    Keychain,
}

impl InstallForm {
    /// Whether the chosen identity can replace the connection's own
    /// credentials: it must log in as the account the key goes to.
    pub fn can_use(&self) -> bool {
        self.chosen().is_some_and(|identity| {
            let user = identity.user.trim();
            user.is_empty() || user == self.user
        })
    }

    pub fn chosen(&self) -> Option<&Identity> {
        let id = self.selected.as_ref()?;
        self.identities.iter().find(|identity| identity.id == *id)
    }
}

impl AuthDialog {
    /// Changes whenever the dialog shows other rows (see `Dialog::shape`).
    pub fn shape(&self) -> u64 {
        let flag = |on: bool, bit: u64| if on { 1 << bit } else { 0 };
        match self {
            Self::Keychain(view) => (1 << 20) + view.identities.len() as u64,
            Self::Identity(form) => {
                (2 << 20)
                    + flag(!form.identity.key_path.trim().is_empty(), 0)
                    + flag(form.error.is_some(), 1)
                    + flag(form.public.as_ref().is_some_and(|(_, public)| public.is_ok()), 2)
                    + flag(form.identity.has_password && !form.is_new && form.password.is_empty(), 3)
            }
            Self::Generate(form) => {
                (3 << 20)
                    + flag(form.busy, 0)
                    + flag(form.error.is_some(), 1)
                    + flag(form.passphrase != form.confirm && !form.confirm.is_empty(), 2)
            }
            Self::DeleteIdentity { users, .. } => (4 << 20) + *users as u64,
            Self::Passphrase(form) => (5 << 20) + flag(form.request.error.is_some(), 0),
            Self::Install(form) => {
                (6 << 20)
                    + ((form.identities.len() as u64) << 4)
                    + flag(form.busy, 0)
                    + flag(form.error.is_some(), 1)
                    + flag(form.can_use(), 2)
                    + flag(form.chosen().is_some(), 3)
            }
        }
    }
}

pub fn show(ui: &mut Ui, p: &Palette, dialog: &mut AuthDialog, first_frame: bool, outcome: &mut Outcome) {
    match dialog {
        AuthDialog::Keychain(view) => keychain(ui, p, view, outcome),
        AuthDialog::Identity(form) => identity_form(ui, p, form, first_frame, outcome),
        AuthDialog::Generate(form) => generate_form(ui, p, form, first_frame, outcome),
        AuthDialog::DeleteIdentity { title, users, .. } => {
            ui.set_width(WIDTH);
            self::title(ui, p, "Supprimer l'identité ?");
            let mut text = format!("« {title} » sera retirée du trousseau, avec ses secrets.");
            match users {
                0 => {}
                1 => text.push_str(" La connexion qui l'utilise reprendra ses propres identifiants."),
                n => text.push_str(&format!(" Les {n} connexions qui l'utilisent reprendront leurs propres identifiants.")),
            }
            text.push_str(" Le fichier de la clé n'est pas touché.");
            ui.add(Label::new(muted(p, text)).wrap());
            ui.add_space(14.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if danger_button(ui, p, "Supprimer").clicked() {
                    *outcome = Outcome::Confirmed;
                }
                if ui.button("Annuler").clicked() {
                    *outcome = Outcome::Cancelled;
                }
            });
        }
        AuthDialog::Passphrase(form) => passphrase_form(ui, p, form, first_frame, outcome),
        AuthDialog::Install(form) => install_form(ui, p, form, outcome),
    }
}

fn keychain(ui: &mut Ui, p: &Palette, view: &mut KeychainView, outcome: &mut Outcome) {
    ui.set_width(LIST_WIDTH);
    ui.horizontal(|ui| {
        ui.label(RichText::new(icon::KEY).size(19.0).color(p.accent));
        ui.label(RichText::new("Trousseau").font(semibold(17.0)).color(p.text));
    });
    ui.add_space(2.0);
    ui.add(Label::new(muted(p, "Des identités réutilisables : un utilisateur avec son mot de passe et/ou sa clé, à choisir dans plusieurs connexions.")).wrap());
    ui.add_space(10.0);

    if view.identities.is_empty() {
        ui.vertical_centered(|ui| {
            ui.add_space(10.0);
            ui.label(RichText::new(icon::IDENTIFICATION_CARD).size(34.0).color(p.faint));
            ui.label(muted(p, "Aucune identité pour l'instant."));
            ui.add_space(10.0);
        });
    }
    ScrollArea::vertical().max_height(330.0).auto_shrink([false, true]).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 4.0;
        for identity in &view.identities {
            let users = view.users.get(&identity.id).copied().unwrap_or(0);
            egui::Frame::new()
                .fill(p.surface)
                .corner_radius(8)
                .inner_margin(Margin::symmetric(10, 7))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        let glyph = if identity.key().is_some() { icon::KEY } else { icon::PASSWORD };
                        ui.label(RichText::new(glyph).size(18.0).color(p.accent));
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            ui.set_max_width(LIST_WIDTH - 140.0);
                            ui.add(Label::new(RichText::new(identity.title()).color(p.text)).truncate());
                            let mut line = identity.summary();
                            if users > 0 {
                                line.push_str(&format!(" · {}", crate::format::count(users, "connexion", "connexions")));
                            }
                            ui.add(Label::new(RichText::new(line).size(11.5).color(p.muted)).truncate());
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.spacing_mut().item_spacing.x = 0.0;
                            if icon_button(ui, p, icon::TRASH, "Supprimer…", true).clicked() {
                                view.request = Some(KeychainRequest::Delete(identity.id.clone()));
                            }
                            if icon_button(ui, p, icon::PENCIL_SIMPLE, "Modifier…", true).clicked() {
                                view.request = Some(KeychainRequest::Edit(identity.id.clone()));
                            }
                        });
                    });
                });
        }
    });

    ui.add_space(10.0);
    ui.label(small(p, "Les mots de passe et les phrases secrètes sont conservés dans le gestionnaire d'identifiants du système."));
    ui.add_space(10.0);
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if ui.button("Fermer").clicked() {
            *outcome = Outcome::Cancelled;
        }
        if primary_button(ui, p, &format!("{}  Nouvelle identité", icon::PLUS), true).clicked() {
            view.request = Some(KeychainRequest::New);
        }
    });
}

fn identity_form(ui: &mut Ui, p: &Palette, form: &mut IdentityForm, first_frame: bool, outcome: &mut Outcome) {
    ui.set_width(WIDTH);
    title(ui, p, if form.is_new { "Nouvelle identité" } else { "Modifier l'identité" });
    let gap = ui.spacing().item_spacing.x;
    let half = (WIDTH - gap) / 2.0;
    let identity = &mut form.identity;
    ui.horizontal_top(|ui| {
        field(ui, p, "Nom", TextEdit::singleline(&mut identity.name).hint_text("Déploiement, Admin…"), half, first_frame);
        field(ui, p, "Utilisateur", TextEdit::singleline(&mut identity.user).hint_text("Celui de la connexion"), half, false);
    });

    let keep = |stored: bool| if stored { "Enregistré : laisser vide pour ne pas changer" } else { "Facultatif" };
    let password_hint = keep(identity.has_password && !form.forget_password);
    field(ui, p, "Mot de passe", TextEdit::singleline(&mut form.password).password(true).hint_text(password_hint), WIDTH, false);
    if identity.has_password && !form.is_new && form.password.is_empty() {
        let text = if form.forget_password { "Le mot de passe sera oublié · Annuler" } else { "Oublier le mot de passe enregistré" };
        if link(ui, p, text) {
            form.forget_password = !form.forget_password;
        }
        ui.add_space(4.0);
    }

    ui.label(small(p, "Clé privée (facultatif)"));
    ui.horizontal(|ui| {
        let generate = 96.0;
        let spacing = ui.spacing().item_spacing.x;
        ui.add(
            TextEdit::singleline(&mut identity.key_path)
                .hint_text("Fichier de la clé")
                .desired_width(WIDTH - BROWSE - generate - 2.0 * spacing)
                .margin(Margin::symmetric(10, 7)),
        );
        if ui.add_sized([BROWSE, 30.0], Button::new("Parcourir…")).clicked() && !crate::dev::active() {
            let mut dialog = rfd::FileDialog::new().set_title("Clé privée SSH");
            if let Some(home) = directories::UserDirs::new() {
                dialog = dialog.set_directory(home.home_dir().join(".ssh"));
            }
            if let Some(picked) = dialog.pick_file() {
                identity.key_path = picked.display().to_string();
            }
        }
        if ui
            .add_sized([generate, 30.0], Button::new(format!("{}  Générer", icon::SPARKLE)))
            .on_hover_text("Créer une nouvelle paire de clés Ed25519 ou RSA")
            .clicked()
        {
            form.request = Some(IdentityRequest::Generate);
        }
    });
    ui.add_space(6.0);

    if !form.identity.key_path.trim().is_empty() {
        let hint = keep(form.identity.has_passphrase && !form.forget_passphrase);
        let hint = if hint == "Facultatif" { "Vide : demandée à la connexion si la clé en a une" } else { hint };
        field(ui, p, "Phrase secrète de la clé", TextEdit::singleline(&mut form.passphrase).password(true).hint_text(hint), WIDTH, false);
        if form.identity.has_passphrase && !form.is_new && form.passphrase.is_empty() {
            let text = if form.forget_passphrase { "La phrase secrète sera oubliée · Annuler" } else { "Oublier la phrase secrète enregistrée" };
            if link(ui, p, text) {
                form.forget_passphrase = !form.forget_passphrase;
            }
            ui.add_space(4.0);
        }
        form.refresh_public();
        if let Some((path, public)) = &form.public
            && public_key_line(ui, p, path, public)
            && let Ok(line) = public
        {
            form.request = Some(IdentityRequest::CopyPublic(line.clone()));
        }
    } else {
        form.public = None;
    }

    error_line(ui, p, &form.error);
    ui.add_space(12.0);
    let valid = !form.identity.title().is_empty()
        && (!form.identity.name.trim().is_empty() || !form.identity.user.trim().is_empty());
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if primary_button(ui, p, "Enregistrer", valid).clicked() {
            *outcome = Outcome::Confirmed;
        }
        if ui.button("Annuler").clicked() {
            *outcome = Outcome::Cancelled;
        }
    });
}

fn generate_form(ui: &mut Ui, p: &Palette, form: &mut GenerateForm, first_frame: bool, outcome: &mut Outcome) {
    ui.set_width(WIDTH);
    title(ui, p, "Générer une paire de clés");
    ui.add_enabled_ui(!form.busy, |ui| {
        ui.label(small(p, "Type"));
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            for kind in KeyType::ALL {
                let selected = form.kind == kind;
                let text = RichText::new(kind.label()).color(if selected { p.on_accent } else { p.text });
                let mut button = Button::new(text).min_size(vec2((WIDTH - 4.0) / 2.0, 0.0));
                if selected {
                    button = button.fill(p.accent);
                }
                if ui.add(button).clicked() && !selected {
                    form.kind = kind;
                    if form.name_auto {
                        form.suggest_name();
                    }
                }
            }
        });
        ui.add_space(8.0);
        let gap = ui.spacing().item_spacing.x;
        let half = (WIDTH - gap) / 2.0;
        ui.horizontal_top(|ui| {
            field(ui, p, "Phrase secrète", TextEdit::singleline(&mut form.passphrase).password(true).hint_text("Recommandée"), half, first_frame);
            field(ui, p, "Confirmation", TextEdit::singleline(&mut form.confirm).password(true), half, false);
        });
        if form.passphrase != form.confirm && !form.confirm.is_empty() {
            ui.label(RichText::new("Les deux phrases secrètes diffèrent.").size(12.0).color(p.danger));
            ui.add_space(4.0);
        }
        ui.label(small(p, "Nom du fichier de la clé privée"));
        ui.horizontal(|ui| {
            let name = ui.add(
                TextEdit::singleline(&mut form.name)
                    .desired_width(WIDTH - BROWSE - ui.spacing().item_spacing.x)
                    .margin(Margin::symmetric(10, 7)),
            );
            if name.changed() {
                form.name_auto = false;
            }
            let pick = ui.add_sized([BROWSE, 30.0], Button::new(format!("{}  Dossier…", icon::FOLDER)));
            if pick.clicked() && !crate::dev::active() {
                let picked = rfd::FileDialog::new()
                    .set_title("Dossier de la nouvelle clé")
                    .set_directory(&form.folder)
                    .pick_folder();
                if let Some(folder) = picked {
                    form.folder = folder;
                    if form.name_auto {
                        form.suggest_name();
                    }
                }
            }
        });
        let folder = form.folder.display().to_string();
        ui.add(Label::new(small(p, format!("Dans {folder}, avec la clé publique (.pub) à côté."))).truncate())
            .on_hover_text(folder);
        ui.add_space(8.0);
        field(ui, p, "Commentaire", TextEdit::singleline(&mut form.comment), WIDTH, false);
    });
    if form.busy {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new().size(14.0).color(p.accent));
            ui.label(muted(p, "Génération de la clé…"));
        });
    }
    error_line(ui, p, &form.error);
    ui.add_space(12.0);
    let enabled = form.valid() && !form.busy;
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if primary_button(ui, p, &format!("{}  Générer", icon::SPARKLE), enabled).clicked() {
            *outcome = Outcome::Confirmed;
        }
        if ui.button("Annuler").clicked() {
            *outcome = Outcome::Cancelled;
        }
    });
}

fn passphrase_form(ui: &mut Ui, p: &Palette, form: &mut PassphraseForm, first_frame: bool, outcome: &mut Outcome) {
    ui.set_width(WIDTH);
    ui.horizontal(|ui| {
        ui.label(RichText::new(icon::LOCK_KEY).size(19.0).color(p.accent));
        ui.label(RichText::new("Phrase secrète de la clé").font(semibold(17.0)).color(p.text));
    });
    ui.add_space(4.0);
    let request = &form.request;
    let who = if request.label.is_empty() { "La connexion".to_owned() } else { format!("« {} »", request.label) };
    ui.add(Label::new(muted(p, format!("{who} se connecte avec cette clé, protégée par une phrase secrète :"))).wrap());
    ui.horizontal(|ui| {
        ui.label(RichText::new(icon::ARROW_BEND_DOWN_RIGHT).color(p.faint));
        ui.add(Label::new(RichText::new(request.key.display().to_string()).size(12.5).color(p.text)).truncate());
    });
    ui.add_space(8.0);
    let response = ui.add(
        TextEdit::singleline(&mut form.passphrase)
            .password(true)
            .hint_text("Phrase secrète")
            .desired_width(WIDTH)
            .margin(Margin::symmetric(10, 7)),
    );
    if first_frame {
        response.request_focus();
    }
    if let Some(error) = &request.error {
        ui.label(RichText::new(error).size(12.0).color(p.danger));
    }
    ui.add_space(6.0);
    if request.owner != crate::ssh_auth::Owner::Nobody {
        checkbox(ui, p, &mut form.save, "L'enregistrer dans le gestionnaire d'identifiants");
    } else {
        ui.label(small(p, "Elle est retenue jusqu'à la fermeture de l'application."));
    }
    if !form.passphrase.is_empty() && response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)) {
        *outcome = Outcome::Confirmed;
    }
    ui.add_space(12.0);
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if primary_button(ui, p, &format!("{}  Déverrouiller", icon::LOCK_KEY_OPEN), !form.passphrase.is_empty()).clicked() {
            *outcome = Outcome::Confirmed;
        }
        if ui.button("Annuler").clicked() {
            *outcome = Outcome::Cancelled;
        }
    });
}

fn install_form(ui: &mut Ui, p: &Palette, form: &mut InstallForm, outcome: &mut Outcome) {
    ui.set_width(WIDTH + 40.0);
    title(ui, p, "Installer une clé sur ce serveur");
    ui.add(
        Label::new(muted(
            p,
            format!(
                "La clé publique de l'identité choisie est ajoutée à ~/.ssh/authorized_keys de {} sur « {} ». \
                 Elle servira ensuite à se connecter sans mot de passe.",
                form.account, form.title
            ),
        ))
        .wrap(),
    );
    ui.add_space(10.0);

    if form.identities.is_empty() {
        ui.add(Label::new(muted(p, "Aucune identité du trousseau n'a de clé.")).wrap());
        ui.add_space(6.0);
        if primary_button(ui, p, &format!("{}  Nouvelle identité avec une clé…", icon::PLUS), true).clicked() {
            form.request = Some(InstallRequest::NewIdentity);
        }
    }
    ui.add_enabled_ui(!form.busy, |ui| {
        for identity in &form.identities {
            let selected = form.selected.as_deref() == Some(identity.id.as_str());
            let fill = if selected { p.selection } else { p.surface };
            let response = egui::Frame::new()
                .fill(fill)
                .stroke(egui::Stroke::new(1.0, if selected { p.accent } else { fill }))
                .corner_radius(8)
                .inner_margin(Margin::symmetric(10, 7))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        let mark = if selected { icon::CHECK_CIRCLE } else { icon::CIRCLE };
                        ui.label(RichText::new(mark).size(17.0).color(if selected { p.accent } else { p.faint }));
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            ui.add(Label::new(RichText::new(identity.title()).color(p.text)).truncate());
                            ui.add(Label::new(RichText::new(identity.summary()).size(11.5).color(p.muted)).truncate());
                        });
                    });
                })
                .response
                .interact(Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            if response.clicked() {
                form.selected = Some(identity.id.clone());
            }
            ui.add_space(2.0);
        }
        if !form.identities.is_empty() {
            ui.add_space(6.0);
            if form.can_use() {
                checkbox(ui, p, &mut form.use_it, "Se connecter ensuite à ce serveur avec cette identité");
            } else if form.chosen().is_some() {
                ui.add(Label::new(small(p, "Cette identité se connecte sous un autre nom : la connexion garde le sien.")).wrap());
            }
        }
    });
    if form.busy {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new().size(14.0).color(p.accent));
            ui.label(muted(p, "Installation de la clé…"));
        });
    }
    error_line(ui, p, &form.error);
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        if link(ui, p, "Gérer le trousseau…") {
            form.request = Some(InstallRequest::Keychain);
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let enabled = form.chosen().is_some() && !form.busy;
            if primary_button(ui, p, &format!("{}  Installer", icon::UPLOAD_SIMPLE), enabled).clicked() {
                *outcome = Outcome::Confirmed;
            }
            if ui.button("Annuler").clicked() {
                *outcome = Outcome::Cancelled;
            }
        });
    });
}

// ---- Entries in the menus of others ---------------------------------------

/// The keychain button of the sidebar header.
pub fn sidebar_button(ui: &mut Ui, p: &Palette, actions: &mut Vec<Action>) {
    if icon_button(ui, p, icon::KEY, "Trousseau : identités et clés SSH", true).clicked() {
        actions.push(Action::SshAuth(SshAuthAction::OpenKeychain));
    }
}

/// In the menu of a saved SSH connection of the sidebar.
pub fn host_menu_items(ui: &mut Ui, profile: &Profile, actions: &mut Vec<Action>) {
    if menu_item(ui, format!("{}  Installer une clé sur ce serveur…", icon::KEY)) {
        actions.push(Action::SshAuth(SshAuthAction::AskInstallOnProfile(profile.id.clone())));
    }
}

/// In the menu of an SSH tab.
pub fn tab_menu_items(ui: &mut Ui, tab: &crate::tabs::Tab, actions: &mut Vec<Action>) {
    if menu_item(ui, format!("{}  Installer une clé sur ce serveur…", icon::KEY)) {
        actions.push(Action::SshAuth(SshAuthAction::AskInstallOnTab(tab.id)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::Protocol;

    fn form(user: &str, auth: AuthMethod, key: &str, identity: Option<Identity>) -> ProfileForm {
        let mut draft = Profile::new();
        draft.protocol = Protocol::Sftp;
        draft.endpoint = "example.org".to_owned();
        draft.user = user.to_owned();
        draft.auth = auth;
        draft.key_path = key.to_owned();
        let mut form = ProfileForm::new(draft, true);
        if let Some(identity) = identity {
            form.draft.identity.clone_from(&identity.id);
            form.ssh.identities.push(identity);
        }
        form
    }

    #[test]
    fn the_form_asks_for_what_the_method_needs() {
        assert!(complete(&form("root", AuthMethod::Auto, "", None)));
        assert!(!complete(&form("", AuthMethod::Auto, "", None)), "who logs in?");
        assert!(!complete(&form("root", AuthMethod::Key, "", None)), "which key?");
        assert!(complete(&form("root", AuthMethod::Key, "id", None)));
        assert!(complete(&form("root", AuthMethod::Agent, "", None)));
        // An identity stands in for the user and the key.
        let identity = Identity {
            user: "deploy".to_owned(),
            key_path: "id".to_owned(),
            ..Identity::new()
        };
        assert!(complete(&form("", AuthMethod::Key, "", Some(identity))));
        // One that was deleted does not.
        let mut gone = form("", AuthMethod::Auto, "", None);
        gone.draft.identity = "id-gone".to_owned();
        assert!(!complete(&gone));
    }

    #[test]
    fn the_form_measures_itself_again_when_its_fields_change() {
        let auto = form_shape(&form("root", AuthMethod::Auto, "", None));
        assert_ne!(auto, form_shape(&form("root", AuthMethod::Agent, "", None)));
        let with_identity = form_shape(&form("root", AuthMethod::Auto, "", Some(Identity::new())));
        assert_ne!(auto, with_identity);
        let mut s3 = form("root", AuthMethod::Agent, "", None);
        s3.draft.protocol = Protocol::S3;
        assert_eq!(form_shape(&s3), 0, "only SFTP has these fields");

        let mut identity = IdentityForm::new(Identity::new(), true);
        let before = AuthDialog::Identity(Box::new(IdentityForm::new(Identity::new(), true))).shape();
        identity.identity.key_path = "id".to_owned();
        assert_ne!(before, AuthDialog::Identity(Box::new(identity)).shape(), "the passphrase field appears");
    }

    #[test]
    fn an_identity_replaces_the_credentials_only_for_the_same_account() {
        let identity = |user: &str| Identity {
            id: format!("id-{user}"),
            user: user.to_owned(),
            key_path: "k".to_owned(),
            ..Identity::default()
        };
        let mut install = InstallForm {
            target: InstallTarget::Tab(1),
            title: "Serveur".to_owned(),
            account: "dev@example.org".to_owned(),
            user: "dev".to_owned(),
            identities: vec![identity(""), identity("dev"), identity("root")],
            selected: None,
            use_it: false,
            busy: false,
            error: None,
            request: None,
        };
        assert!(!install.can_use(), "nothing chosen yet");
        for (user, usable) in [("", true), ("dev", true), ("root", false)] {
            install.selected = Some(format!("id-{user}"));
            assert_eq!(install.can_use(), usable, "identity of {user:?}");
        }
    }

    #[test]
    fn generated_keys_get_a_free_name_after_their_type() {
        let mut generate = GenerateForm::new("Déploiement", "deploy");
        generate.folder = std::env::temp_dir().join(format!("s3v-generate-test-{}", Profile::new().id));
        generate.suggest_name();
        assert_eq!(generate.name, "id_ed25519_deploiement");
        assert_eq!(generate.comment, "deploy@Farfile");
        generate.kind = KeyType::Rsa4096;
        generate.suggest_name();
        assert_eq!(generate.name, "id_rsa_deploiement");
        assert!(generate.valid());
        generate.confirm = "other".to_owned();
        assert!(!generate.valid(), "the passphrases differ");
        generate.confirm.clear();
        generate.name = r"..\elsewhere".to_owned();
        assert!(!generate.valid(), "a name, not a path");
    }
}
