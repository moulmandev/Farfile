//! The metadata and storage class of an object, rewritten in place.

use egui::{Label, TextEdit, Ui};

use super::{DIALOG_GAP, Request, buttons, failure, heading, note, section, subject};
use crate::format;
use crate::s3::{ObjectMeta, Op, Reply, S3Error};
use crate::storage::RemotePath;
use crate::theme::Palette;
use crate::ui::widgets::{icon, icon_button, muted, placeholder, primary_button, small};

const WIDTH: f32 = 560.0;
const LABEL_WIDTH: f32 = 150.0;

const CONTENT_TYPES: [&str; 12] = [
    "application/octet-stream",
    "application/json",
    "application/pdf",
    "application/zip",
    "text/plain; charset=utf-8",
    "text/html; charset=utf-8",
    "text/css",
    "text/javascript",
    "image/png",
    "image/jpeg",
    "image/svg+xml",
    "video/mp4",
];

const CACHE_CONTROLS: [(&str, &str); 4] = [
    ("no-cache", "Toujours revalider"),
    ("no-store", "Ne jamais garder en cache"),
    ("public, max-age=3600", "Une heure"),
    ("public, max-age=31536000, immutable", "Un an, sans revalider"),
];

/// The classes of Amazon S3; other providers accept some of them.
const STORAGE_CLASSES: [(&str, &str); 8] = [
    ("STANDARD", "Standard"),
    ("INTELLIGENT_TIERING", "Hiérarchisation intelligente"),
    ("STANDARD_IA", "Accès peu fréquent"),
    ("ONEZONE_IA", "Accès peu fréquent, une zone"),
    ("GLACIER_IR", "Glacier, récupération instantanée"),
    ("GLACIER", "Glacier, récupération flexible"),
    ("DEEP_ARCHIVE", "Glacier Deep Archive"),
    ("REDUCED_REDUNDANCY", "Redondance réduite (ancienne)"),
];

enum State {
    Loading,
    /// Boxed: much larger than the other states.
    Ready { meta: Box<ObjectMeta>, saved: Box<ObjectMeta> },
    Failed(S3Error),
}

pub struct Metadata {
    path: RemotePath,
    state: State,
    saving: bool,
    error: Option<String>,
}

impl Metadata {
    pub fn new(path: RemotePath) -> Self {
        Self {
            path,
            state: State::Loading,
            saving: false,
            error: None,
        }
    }

    /// Returns true once saved: the dialog closes.
    pub fn receive(&mut self, reply: Reply) -> bool {
        match reply {
            Reply::Meta(Ok(meta)) => {
                self.state = State::Ready {
                    saved: Box::new(meta.clone()),
                    meta: Box::new(meta),
                }
            }
            Reply::Meta(Err(error)) => self.state = State::Failed(error),
            Reply::Done { op: Op::SaveMeta, result } => {
                self.saving = false;
                match result {
                    Ok(_) => return true,
                    Err(error) => {
                        self.error = Some(match error.code.as_str() {
                            "InvalidStorageClass" => {
                                "Le serveur ne propose pas cette classe de stockage.".to_owned()
                            }
                            "InvalidObjectState" => {
                                "L'objet est archivé : restaurez-le avant de le modifier.".to_owned()
                            }
                            _ => failure(&error),
                        })
                    }
                }
            }
            _ => {}
        }
        false
    }

    fn save(&mut self, requests: &mut Vec<Request>) {
        let State::Ready { meta, saved } = &mut self.state else {
            return;
        };
        if meta == saved || self.saving {
            return;
        }
        // Names are case-insensitive and travel as HTTP headers.
        meta.user.retain(|(key, _)| !key.trim().is_empty());
        for (key, value) in &mut meta.user {
            *key = key.trim().to_ascii_lowercase();
            *value = value.trim().to_owned();
        }
        if let Some((key, _)) = meta.user.iter().find(|(key, _)| !header_safe(key)) {
            self.error = Some(format!(
                "« {key} » ne peut pas servir de nom : lettres sans accent, chiffres et tirets seulement."
            ));
            return;
        }
        self.saving = true;
        self.error = None;
        requests.push(Request::SaveMeta(self.path.clone(), (**meta).clone()));
    }

    pub fn dev_type(&mut self, text: &str) {
        if let State::Ready { meta, .. } = &mut self.state {
            meta.cache_control = text.to_owned();
        }
    }

    /// `save`, `class NAME`, `type MIME`, `addmeta`.
    pub fn dev_press(&mut self, what: &str, requests: &mut Vec<Request>) {
        let (verb, rest) = what.split_once(' ').unwrap_or((what, ""));
        if verb == "save" {
            return self.save(requests);
        }
        if let State::Ready { meta, .. } = &mut self.state {
            match verb {
                "class" => meta.storage_class = rest.to_owned(),
                "type" => meta.content_type = rest.to_owned(),
                "addmeta" => {
                    let (key, value) = rest.split_once('=').unwrap_or((rest, ""));
                    meta.user.push((key.to_owned(), value.to_owned()));
                }
                _ => {}
            }
        }
    }

    /// Returns true to close.
    pub fn show(&mut self, ui: &mut Ui, p: &Palette, requests: &mut Vec<Request>) -> bool {
        ui.set_width(WIDTH);
        heading(ui, p, &format!("Métadonnées de « {} »", self.path.name()));
        subject(ui, p, icon::FILE, &self.path.to_string());
        ui.add_space(DIALOG_GAP);

        let mut close = false;
        let (meta, saved) = match &mut self.state {
            State::Loading => {
                ui.add_space(60.0);
                ui.vertical_centered(|ui| ui.add(egui::Spinner::new().size(24.0).color(p.accent)));
                ui.add_space(60.0);
                buttons(ui, |ui| close = ui.button("Annuler").clicked());
                return close;
            }
            State::Failed(error) => {
                placeholder(ui, p, icon::WARNING_CIRCLE, p.danger, "Impossible de lire les métadonnées", "");
                ui.vertical_centered(|ui| ui.add(Label::new(muted(p, failure(error))).wrap()));
                ui.add_space(DIALOG_GAP);
                buttons(ui, |ui| close = ui.button("Fermer").clicked());
                return close;
            }
            State::Ready { meta, saved } => (meta, saved),
        };

        let mut facts = vec![format::size(meta.size)];
        if let Some(modified) = meta.modified {
            facts.push(format!("modifié le {}", format::date(modified)));
        }
        if !meta.etag.is_empty() {
            facts.push(format!("ETag {}", meta.etag));
        }
        ui.add(Label::new(small(p, facts.join(" · "))).truncate());
        ui.add_space(10.0);

        section(ui, p, "En-têtes");
        ui.add_space(2.0);
        egui::Grid::new("s3-meta")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .min_col_width(LABEL_WIDTH)
            .show(ui, |ui| {
                let width = WIDTH - LABEL_WIDTH - 12.0 - 34.0;
                ui.label(muted(p, "Type de contenu"));
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    ui.add(
                        TextEdit::singleline(&mut meta.content_type)
                            .hint_text("application/octet-stream")
                            .desired_width(width)
                            .margin(egui::Margin::symmetric(10, 6)),
                    );
                    let pick = icon_button(ui, p, icon::CARET_DOWN, "Types courants", true);
                    egui::Popup::menu(&pick).show(|ui| {
                        for kind in CONTENT_TYPES {
                            if ui.button(kind).clicked() {
                                meta.content_type = kind.to_owned();
                                ui.close();
                            }
                        }
                    });
                });
                ui.end_row();

                ui.label(muted(p, "Cache-Control"));
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    ui.add(
                        TextEdit::singleline(&mut meta.cache_control)
                            .hint_text("aucun — le navigateur décide")
                            .desired_width(width)
                            .margin(egui::Margin::symmetric(10, 6)),
                    );
                    let pick = icon_button(ui, p, icon::CARET_DOWN, "Réglages courants", true);
                    egui::Popup::menu(&pick).show(|ui| {
                        for (value, label) in CACHE_CONTROLS {
                            if ui.button(format!("{label}   ·   {value}")).clicked() {
                                meta.cache_control = value.to_owned();
                                ui.close();
                            }
                        }
                    });
                });
                ui.end_row();

                let full = width + 32.0;
                for (label, value, hint) in [
                    ("Content-Disposition", &mut meta.content_disposition, "aucun — ex. attachment"),
                    ("Content-Encoding", &mut meta.content_encoding, "aucun — ex. gzip"),
                    ("Content-Language", &mut meta.content_language, "aucun — ex. fr-FR"),
                ] {
                    ui.label(muted(p, label));
                    ui.add(TextEdit::singleline(value).hint_text(hint).desired_width(full).margin(egui::Margin::symmetric(10, 6)));
                    ui.end_row();
                }

                ui.label(muted(p, "Classe de stockage"));
                let current = if meta.storage_class.is_empty() { "STANDARD".to_owned() } else { meta.storage_class.clone() };
                let shown = STORAGE_CLASSES
                    .iter()
                    .find(|(code, _)| *code == current)
                    .map_or(current.clone(), |(code, label)| format!("{label}  ·  {code}"));
                egui::ComboBox::from_id_salt("s3-storage-class")
                    .selected_text(shown)
                    .width(full)
                    .show_ui(ui, |ui| {
                        for (code, label) in STORAGE_CLASSES {
                            let selected = current == code;
                            if ui.selectable_label(selected, format!("{label}  ·  {code}")).clicked() {
                                meta.storage_class = code.to_owned();
                            }
                        }
                    });
                ui.end_row();
            });
        if matches!(meta.storage_class.as_str(), "GLACIER" | "DEEP_ARCHIVE") && meta.storage_class != saved.storage_class {
            ui.add_space(6.0);
            note(
                ui,
                p,
                icon::WARNING,
                p.folder,
                "Dans cette classe, le fichier n'est plus lisible directement : il faut d'abord demander sa \
                 restauration, qui prend des heures.",
            );
        }

        ui.add_space(12.0);
        section(ui, p, "Métadonnées personnalisées (x-amz-meta-*)");
        ui.add_space(2.0);
        let mut remove = None;
        for (index, (key, value)) in meta.user.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.add(TextEdit::singleline(key).hint_text("nom").desired_width(LABEL_WIDTH).margin(egui::Margin::symmetric(10, 6)));
                ui.add(
                    TextEdit::singleline(value)
                        .hint_text("valeur")
                        .desired_width(WIDTH - LABEL_WIDTH - 52.0)
                        .margin(egui::Margin::symmetric(10, 6)),
                );
                if icon_button(ui, p, icon::X, "Retirer", true).clicked() {
                    remove = Some(index);
                }
            });
        }
        if let Some(index) = remove {
            meta.user.remove(index);
        }
        if meta.user.is_empty() {
            ui.label(small(p, "Aucune."));
        }
        if ui.small_button(format!("{}  Ajouter", icon::PLUS)).clicked() {
            meta.user.push((String::new(), String::new()));
        }

        ui.add_space(10.0);
        ui.add(
            Label::new(small(
                p,
                "L'objet est réécrit sur place par une copie : une nouvelle version apparaît si le bucket les \
                 conserve, et son ACL redevient celle par défaut.",
            ))
            .wrap(),
        );
        if let Some(error) = &self.error {
            ui.add_space(6.0);
            note(ui, p, icon::WARNING_CIRCLE, p.danger, error);
        }
        ui.add_space(DIALOG_GAP);
        let changed = meta != saved;
        let mut save = false;
        buttons(ui, |ui| {
            let label = if self.saving { "Enregistrement…" } else { "Enregistrer" };
            save = primary_button(ui, p, label, changed && !self.saving).clicked();
            close = ui.button("Annuler").clicked();
        });
        if save {
            self.save(requests);
        }
        close
    }
}

/// What an HTTP header name may hold, as S3 takes it for metadata.
fn header_safe(key: &str) -> bool {
    !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_names_must_be_headers() {
        assert!(header_safe("auteur"));
        assert!(header_safe("build-id_2"));
        assert!(!header_safe("auteur principal"));
        assert!(!header_safe("été"));
        assert!(!header_safe(""));
    }
}
