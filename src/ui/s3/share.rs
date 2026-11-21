//! Sharing a file: a signed link of the chosen lifetime, and making the
//! file public (or private again) through its ACL.

use std::time::Duration;

use egui::{Label, RichText, TextEdit, Ui};

use super::{DIALOG_GAP, Request, busy, buttons, copy_field, failure, heading, note, section, segmented, status, subject};
use crate::s3::share::{Expiry, Unit, attachment, custom_duration, expiry_text};
use crate::s3::{Access, Op, Reply, S3Error};
use crate::storage::RemotePath;
use crate::theme::Palette;
use crate::ui::widgets::{checkbox, icon, small};

const WIDTH: f32 = 540.0;
/// How long a copy button says it copied.
const COPIED_FOR: f64 = 2.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Copied {
    Link,
    Public,
}

pub struct Share {
    path: RemotePath,
    public_url: Option<String>,
    expiry: Expiry,
    amount: String,
    unit: Unit,
    download: bool,
    /// Numbers the link requests: only the answer to the last one counts.
    serial: u64,
    /// What the current link was asked for.
    asked: Option<(Duration, bool)>,
    link: Option<Result<String, S3Error>>,
    signed_at: i64,
    access: Option<Box<Access>>,
    changing: bool,
    message: Option<(String, bool)>,
    copied: Option<(Copied, f64)>,
}

impl Share {
    pub fn new(path: RemotePath, public_url: Option<String>) -> Self {
        Self {
            path,
            public_url,
            expiry: Expiry::default(),
            amount: "3".to_owned(),
            unit: Unit::Hours,
            download: false,
            serial: 0,
            asked: None,
            link: None,
            signed_at: 0,
            access: None,
            changing: false,
            message: None,
            copied: None,
        }
    }

    pub fn receive(&mut self, reply: Reply, requests: &mut Vec<Request>, _now: f64) {
        match reply {
            Reply::Link { serial, result } if serial == self.serial => {
                self.link = Some(result);
                self.signed_at = chrono::Utc::now().timestamp();
            }
            Reply::Access(access) => self.access = Some(access),
            Reply::Done { op: Op::SetPublic, result } => {
                self.changing = false;
                self.message = Some(match result {
                    Ok(text) => (text, false),
                    Err(error) => (acl_error(&error), true),
                });
                requests.push(Request::LoadAccess(self.path.clone()));
            }
            _ => {}
        }
    }

    #[cfg(test)]
    pub fn link_for_tests(&self) -> Option<&Result<String, S3Error>> {
        self.link.as_ref()
    }

    fn duration(&self) -> Result<Duration, &'static str> {
        match self.expiry.duration() {
            Some(duration) => Ok(duration),
            None => custom_duration(&self.amount, self.unit),
        }
    }

    /// Asks for a new link whenever what it should be changes.
    fn sign(&mut self, requests: &mut Vec<Request>) {
        let Ok(duration) = self.duration() else {
            return;
        };
        let wanted = (duration, self.download);
        if self.asked == Some(wanted) {
            return;
        }
        self.asked = Some(wanted);
        self.serial += 1;
        self.link = None;
        requests.push(Request::Presign {
            path: self.path.clone(),
            serial: self.serial,
            expires: duration,
            attachment: self.download.then(|| attachment(self.path.name())),
        });
    }

    fn set_public(&mut self, public: bool, requests: &mut Vec<Request>) {
        self.changing = true;
        self.message = None;
        requests.push(Request::SetPublic(self.path.clone(), public));
    }

    pub fn dev_type(&mut self, text: &str) {
        self.expiry = Expiry::Custom;
        let (amount, unit) = text.split_once(' ').unwrap_or((text, "minutes"));
        self.amount = amount.to_owned();
        self.unit = match unit.trim() {
            "heures" | "h" => Unit::Hours,
            "jours" | "j" => Unit::Days,
            _ => Unit::Minutes,
        };
    }

    /// `15min`, `1h`, `1j`, `7j`, `download`, `public`, `private`, `copy`.
    pub fn dev_press(&mut self, what: &str, requests: &mut Vec<Request>) {
        match what {
            "15min" => self.expiry = Expiry::Minutes15,
            "1h" => self.expiry = Expiry::Hour1,
            "1j" => self.expiry = Expiry::Day1,
            "7j" => self.expiry = Expiry::Days7,
            "download" => self.download = !self.download,
            "public" => self.set_public(true, requests),
            "private" => self.set_public(false, requests),
            _ => {}
        }
    }

    /// Returns true to close.
    pub fn show(&mut self, ui: &mut Ui, p: &Palette, requests: &mut Vec<Request>) -> bool {
        ui.set_width(WIDTH);
        let now = ui.input(|input| input.time);
        if self.copied.is_some_and(|(_, at)| now - at > COPIED_FOR) {
            self.copied = None;
        }
        heading(ui, p, &format!("Partager « {} »", self.path.name()));
        subject(ui, p, icon::FILE, &self.path.to_string());
        ui.add_space(DIALOG_GAP);

        section(ui, p, "Lien temporaire");
        ui.add_space(2.0);
        let options: Vec<(Expiry, String)> =
            Expiry::ALL.iter().map(|expiry| (*expiry, expiry.label().to_owned())).collect();
        ui.horizontal(|ui| {
            ui.label(RichText::new("Valable").color(p.text));
            segmented(ui, p, &mut self.expiry, &options);
        });
        if self.expiry == Expiry::Custom {
            ui.horizontal(|ui| {
                ui.add_space(60.0);
                ui.add(TextEdit::singleline(&mut self.amount).desired_width(60.0).margin(egui::Margin::symmetric(8, 5)));
                egui::ComboBox::from_id_salt("s3-share-unit")
                    .selected_text(self.unit.label())
                    .width(100.0)
                    .show_ui(ui, |ui| {
                        for unit in Unit::ALL {
                            ui.selectable_value(&mut self.unit, unit, unit.label());
                        }
                    });
                ui.label(small(p, "7 jours au plus"));
            });
        }
        ui.add_space(4.0);
        checkbox(ui, p, &mut self.download, "Forcer le téléchargement").on_hover_text(
            "Le navigateur enregistre le fichier sous son nom au lieu de l'afficher\n(response-content-disposition: attachment).",
        );
        ui.add_space(6.0);
        self.sign(requests);

        match (self.duration(), &self.link) {
            (Err(problem), _) => {
                ui.label(RichText::new(problem).color(p.danger));
            }
            (Ok(_), None) => busy(ui, p, "Signature du lien…"),
            (Ok(_), Some(Err(error))) => {
                ui.add(Label::new(RichText::new(format!("Lien impossible : {}", failure(error))).color(p.danger)).wrap());
            }
            (Ok(duration), Some(Ok(url))) => {
                let copied = self.copied.is_some_and(|(what, _)| what == Copied::Link);
                if copy_field(ui, p, url, WIDTH, copied) {
                    ui.ctx().copy_text(url.clone());
                    self.copied = Some((Copied::Link, now));
                }
                ui.label(small(
                    p,
                    format!(
                        "{}. Signé avec la clé de cette connexion : il cesse de marcher si elle est révoquée.",
                        expiry_text(duration, self.signed_at)
                    ),
                ));
            }
        }

        ui.add_space(DIALOG_GAP);
        section(ui, p, "Accès public");
        ui.add_space(2.0);
        self.access_part(ui, p, requests);

        if self.copied.is_some() {
            ui.ctx().request_repaint_after(Duration::from_secs_f64(COPIED_FOR));
        }
        ui.add_space(DIALOG_GAP);
        let mut close = false;
        buttons(ui, |ui| close = ui.button("Fermer").clicked());
        close
    }

    fn access_part(&mut self, ui: &mut Ui, p: &Palette, requests: &mut Vec<Request>) {
        let Some(access) = &self.access else {
            busy(ui, p, "Lecture des droits du fichier…");
            return;
        };
        let enforced = matches!(&access.ownership, Ok(Some(owner)) if owner == "BucketOwnerEnforced");
        let mut settings = false;
        let mut wanted = None;
        match &access.public {
            _ if enforced => {
                note(
                    ui,
                    p,
                    icon::INFO,
                    p.accent,
                    "Les ACL sont désactivées sur ce bucket (propriété des objets « BucketOwnerEnforced ») : un \
                     fichier ne peut pas être rendu public seul. Une politique de bucket peut ouvrir la lecture \
                     à tous (Réglages du bucket › Politique).",
                );
                settings = true;
            }
            Err(error) if error.acl_disabled() => {
                note(ui, p, icon::INFO, p.accent, &acl_error(error));
                settings = true;
            }
            Err(error) if error.unsupported() => {
                note(
                    ui,
                    p,
                    icon::INFO,
                    p.muted,
                    "Ce serveur ne gère pas les ACL d'objet : l'accès public se règle par la politique du bucket.",
                );
                settings = true;
            }
            Err(error) => {
                ui.add(Label::new(RichText::new(failure(error)).color(p.danger)).wrap());
            }
            Ok(public) => {
                let blocked = matches!(&access.block, Ok(Some(block)) if block.block_public_acls);
                ui.horizontal(|ui| {
                    let (glyph, tint, text) = if *public {
                        (icon::GLOBE, p.success, "Public : n'importe qui peut le lire à son adresse.")
                    } else {
                        (icon::LOCK_SIMPLE, p.muted, "Privé : seuls les comptes autorisés peuvent le lire.")
                    };
                    ui.label(RichText::new(glyph).size(16.0).color(tint));
                    ui.label(RichText::new(text).color(p.text));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.changing {
                            ui.add(egui::Spinner::new().size(14.0).color(p.muted));
                        } else if *public {
                            if ui.button(format!("{}  Rendre privé", icon::LOCK_SIMPLE)).clicked() {
                                wanted = Some(false);
                            }
                        } else if ui
                            .add_enabled(!blocked, egui::Button::new(format!("{}  Rendre public", icon::GLOBE)))
                            .on_disabled_hover_text("Le blocage de l'accès public du bucket l'interdit")
                            .clicked()
                        {
                            wanted = Some(true);
                        }
                    });
                });
                if blocked && !*public {
                    note(
                        ui,
                        p,
                        icon::SHIELD_CHECK,
                        p.folder,
                        "Le blocage de l'accès public du bucket refuse les ACL publiques (BlockPublicAcls) : \
                         levez-le dans les réglages du bucket pour rendre ce fichier public.",
                    );
                    settings = true;
                }
            }
        }
        status(ui, p, &self.message);
        // A refusal is usually solved in the bucket's settings.
        settings |= self.message.as_ref().is_some_and(|(_, error)| *error);

        if let Some(url) = &self.public_url {
            ui.add_space(6.0);
            ui.label(small(p, "Adresse publique (ne s'ouvre que si le fichier est public)"));
            let now = ui.input(|input| input.time);
            let copied = self.copied.is_some_and(|(what, _)| what == Copied::Public);
            if copy_field(ui, p, url, WIDTH, copied) {
                ui.ctx().copy_text(url.clone());
                self.copied = Some((Copied::Public, now));
            }
        }
        if settings && let Some(bucket) = self.path.segments().first() {
            ui.add_space(4.0);
            if ui.button(format!("{}  Réglages du bucket…", icon::GEAR_SIX)).clicked() {
                requests.push(Request::OpenSettings(bucket.clone()));
            }
        }
        if let Some(public) = wanted {
            self.set_public(public, requests);
        }
    }
}

/// Why changing the ACL failed, the usual reasons spelled out.
fn acl_error(error: &S3Error) -> String {
    if error.acl_disabled() {
        "Les ACL sont désactivées sur ce bucket (AccessControlListNotSupported : propriété des objets \
         « BucketOwnerEnforced »). Rendez le fichier public par une politique de bucket."
            .to_owned()
    } else if error.access_denied() {
        format!(
            "Refusé ({}) : le blocage de l'accès public du bucket ou du compte, ou les droits de la clé, \
             l'interdisent.",
            if error.code.is_empty() { "HTTP 403" } else { &error.code }
        )
    } else {
        failure(error)
    }
}
