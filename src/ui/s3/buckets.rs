//! Creating a bucket, and deleting one, emptying it first if asked.

use std::sync::Arc;

use egui::{Key, Label, RichText, TextEdit, Ui};

use super::{DIALOG_GAP, Request, buttons, failure, field, heading, note};
use crate::s3::names::{REGIONS, bucket_name_problem, bucket_name_warning, clean_region, region_label, region_problem};
use crate::s3::{BucketContents, Op, Reply, S3Error};
use crate::storage::Progress;
use crate::theme::Palette;
use crate::ui::widgets::{danger_button, icon, icon_button, muted, primary_button, small};

const WIDTH: f32 = 420.0;

pub struct NewBucket {
    pub name: String,
    region: String,
    /// The region of the connection, used when the field is left empty.
    default_region: String,
    working: bool,
    error: Option<String>,
}

impl NewBucket {
    pub fn new(region: &str) -> Self {
        Self {
            name: String::new(),
            region: String::new(),
            default_region: region.to_owned(),
            working: false,
            error: None,
        }
    }

    /// Returns true when the bucket was created: the dialog closes.
    pub fn receive(&mut self, reply: Reply) -> bool {
        let Reply::Done { op: Op::CreateBucket, result } = reply else {
            return false;
        };
        self.working = false;
        match result {
            Ok(_) => true,
            Err(error) => {
                self.error = Some(creation_error(&error));
                false
            }
        }
    }

    fn problem(&self) -> Option<&'static str> {
        bucket_name_problem(self.name.trim()).or_else(|| region_problem(&self.region))
    }

    fn create(&mut self, requests: &mut Vec<Request>) {
        if self.working || self.problem().is_some() {
            return;
        }
        self.working = true;
        self.error = None;
        let region = match clean_region(&self.region) {
            region if region.is_empty() => self.default_region.clone(),
            region => region,
        };
        requests.push(Request::CreateBucket {
            name: self.name.trim().to_owned(),
            region,
        });
    }

    pub fn dev_press(&mut self, requests: &mut Vec<Request>) {
        self.create(requests);
    }

    pub fn show(&mut self, ui: &mut Ui, p: &Palette, first_frame: bool, requests: &mut Vec<Request>) -> bool {
        ui.set_width(WIDTH);
        heading(ui, p, tr!("Nouveau bucket"));
        ui.add_space(8.0);
        let name = field(
            ui,
            p,
            tr!("Nom du bucket"),
            TextEdit::singleline(&mut self.name).hint_text(tr!("mon-bucket")),
            WIDTH,
        );
        if first_frame {
            name.request_focus();
        }
        let typed = self.name.trim();
        let (hint, tint) = match (typed.is_empty(), bucket_name_problem(typed), bucket_name_warning(typed)) {
            (true, _, _) => (
                tr!(
                    "3 à 63 caractères : minuscules, chiffres, tirets et points. Le nom est partagé par tous les \
                     clients du fournisseur : choisissez-le distinctif."
                ),
                p.muted,
            ),
            (false, Some(problem), _) => (problem, p.danger),
            (false, None, Some(warning)) => (warning, p.folder),
            (false, None, None) => (tr!("Ce nom est valide."), p.success),
        };
        ui.add(Label::new(RichText::new(hint).size(12.0).color(tint)).wrap());
        ui.add_space(10.0);

        ui.label(small(p, tr!("Région")));
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.add(
                TextEdit::singleline(&mut self.region)
                    .hint_text(tr!("{region} (celle de la connexion)", region = self.default_region))
                    .desired_width(WIDTH - 32.0)
                    .margin(egui::Margin::symmetric(10, 7)),
            );
            let pick = icon_button(ui, p, icon::CARET_DOWN, tr!("Choisir une région d'Amazon S3"), true);
            egui::Popup::menu(&pick).show(|ui| {
                ui.set_min_width(300.0);
                egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                    for (code, label) in REGIONS {
                        let text = format!("{code}   {}", crate::i18n::lookup(label));
                        if ui.button(text).clicked() {
                            self.region = code.to_owned();
                            ui.close();
                        }
                    }
                });
            });
        });
        let region = clean_region(&self.region);
        let region = if region.is_empty() { self.default_region.clone() } else { region };
        let detail = match (region_problem(&self.region), region_label(&region)) {
            (Some(problem), _) => RichText::new(problem).color(p.danger),
            (None, Some(label)) => RichText::new(label).color(p.muted),
            (None, None) => RichText::new(tr!("Région propre au fournisseur")).color(p.muted),
        };
        ui.label(detail.size(12.0));

        if let Some(error) = &self.error {
            ui.add_space(8.0);
            note(ui, p, icon::WARNING_CIRCLE, p.danger, error);
        }
        ui.add_space(DIALOG_GAP);
        let valid = self.problem().is_none();
        if valid && ui.input(|input| input.key_pressed(Key::Enter)) {
            self.create(requests);
        }
        let mut close = false;
        buttons(ui, |ui| {
            let label = if self.working { tr!("Création…") } else { tr!("Créer") };
            if primary_button(ui, p, label, valid && !self.working).clicked() {
                self.create(requests);
            }
            close = ui.button(tr!("Annuler")).clicked();
        });
        close
    }
}

/// Bucket names are global: say so rather than quote the server.
fn creation_error(error: &S3Error) -> String {
    match error.code.as_str() {
        "BucketAlreadyOwnedByYou" => tr!("Vous avez déjà un bucket de ce nom.").to_owned(),
        "BucketAlreadyExists" => {
            tr!("Ce nom est déjà pris par un autre compte du fournisseur : choisissez-en un autre.").to_owned()
        }
        "InvalidBucketName" => tr!("Le serveur refuse ce nom ({message}).", message = error.message),
        "InvalidLocationConstraint" | "IllegalLocationConstraintException" => {
            tr!("Le serveur refuse cette région : {message}", message = error.message)
        }
        "TooManyBuckets" => tr!("Le compte a atteint son nombre maximal de buckets.").to_owned(),
        _ => failure(error),
    }
}

enum Stage {
    Checking,
    Checked(Result<BucketContents, S3Error>),
    Deleting(Arc<Progress>),
}

pub struct DeleteBucket {
    bucket: String,
    stage: Stage,
    /// The name typed to confirm emptying the bucket.
    pub typed: String,
    error: Option<String>,
}

impl DeleteBucket {
    pub fn new(bucket: &str) -> Self {
        Self {
            bucket: bucket.to_owned(),
            stage: Stage::Checking,
            typed: String::new(),
            error: None,
        }
    }

    /// Returns true when the bucket is gone: the dialog closes.
    pub fn receive(&mut self, reply: Reply, requests: &mut Vec<Request>) -> bool {
        match reply {
            Reply::Contents(result) => self.stage = Stage::Checked(result),
            Reply::Done {
                op: Op::DeleteBucket,
                result,
            } => match result {
                Ok(_) => return true,
                Err(error) => {
                    self.error = Some(if error.code == "BucketNotEmpty" {
                        tr!("Le bucket n'est pas vide : des objets ou des versions y restent.").to_owned()
                    } else if error.code.is_empty() && error.message == "annulé" {
                        tr!("Arrêté : une partie du contenu a déjà été supprimée.").to_owned()
                    } else {
                        failure(&error)
                    });
                    // Look again: what is left has changed.
                    self.stage = Stage::Checking;
                    self.typed.clear();
                    requests.push(Request::InspectBucket(self.bucket.clone()));
                }
            },
            _ => {}
        }
        false
    }

    fn delete(&mut self, empty_first: bool, requests: &mut Vec<Request>) {
        let progress = Arc::new(Progress::default());
        self.error = None;
        self.stage = Stage::Deleting(progress.clone());
        requests.push(Request::DeleteBucket {
            bucket: self.bucket.clone(),
            empty_first,
            progress,
        });
    }

    pub fn dev_press(&mut self, requests: &mut Vec<Request>) {
        match &self.stage {
            Stage::Checked(Ok(contents)) if contents.is_empty() => self.delete(false, requests),
            Stage::Checked(Ok(_)) if self.typed == self.bucket => self.delete(true, requests),
            _ => {}
        }
    }

    pub fn show(&mut self, ui: &mut Ui, p: &Palette, first_frame: bool, requests: &mut Vec<Request>) -> bool {
        ui.set_width(WIDTH);
        heading(ui, p, tr!("Supprimer le bucket ?"));
        ui.add_space(8.0);
        let mut close = false;
        match &self.stage {
            Stage::Checking => {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(14.0).color(p.muted));
                    ui.label(muted(p, tr!("Vérification du contenu de « {bucket} »…", bucket = self.bucket)));
                });
                if let Some(error) = &self.error {
                    ui.add_space(6.0);
                    note(ui, p, icon::WARNING_CIRCLE, p.danger, error);
                }
                ui.add_space(DIALOG_GAP);
                buttons(ui, |ui| close = ui.button(tr!("Annuler")).clicked());
            }
            Stage::Checked(Err(error)) => {
                note(ui, p, icon::WARNING_CIRCLE, p.danger, &tr!("Impossible de lire le bucket : {error}", error = failure(error)));
                ui.add_space(DIALOG_GAP);
                buttons(ui, |ui| close = ui.button(tr!("Fermer")).clicked());
            }
            Stage::Checked(Ok(contents)) if contents.is_empty() => {
                ui.add(
                    Label::new(muted(
                        p,
                        tr!("Le bucket « {bucket} » est vide. Il sera supprimé définitivement.", bucket = self.bucket),
                    ))
                    .wrap(),
                );
                if let Some(error) = &self.error {
                    ui.add_space(6.0);
                    note(ui, p, icon::WARNING_CIRCLE, p.danger, error);
                }
                ui.add_space(DIALOG_GAP);
                buttons(ui, |ui| {
                    if danger_button(ui, p, tr!("Supprimer le bucket")).clicked() {
                        self.delete(false, requests);
                    }
                    close = ui.button(tr!("Annuler")).clicked();
                });
            }
            Stage::Checked(Ok(contents)) => {
                let contents = contents.clone();
                let mut held = Vec::new();
                if contents.objects > 0 {
                    held.push(if contents.more {
                        tr!("plus de 1 000 objets").to_owned()
                    } else {
                        trn!(contents.objects, "{n} objet", "{n} objets")
                    });
                }
                if contents.versions {
                    held.push(tr!("des versions anciennes ou des marqueurs de suppression").to_owned());
                }
                if contents.uploads {
                    held.push(tr!("des envois inachevés").to_owned());
                }
                note(
                    ui,
                    p,
                    icon::WARNING,
                    p.danger,
                    &tr!(
                        "« {bucket} » contient {contents}. Un bucket doit être vide pour être supprimé.",
                        bucket = self.bucket,
                        contents = held.join(", ")
                    ),
                );
                ui.add_space(10.0);
                ui.add(
                    Label::new(RichText::new(tr!(
                        "Pour tout effacer, versions comprises, puis supprimer le bucket, tapez son nom :"
                    )))
                    .wrap(),
                );
                ui.add_space(4.0);
                let typed = ui.add(
                    TextEdit::singleline(&mut self.typed)
                        .hint_text(&self.bucket)
                        .desired_width(WIDTH)
                        .margin(egui::Margin::symmetric(10, 7)),
                );
                if first_frame {
                    typed.request_focus();
                }
                if let Some(error) = &self.error {
                    ui.add_space(6.0);
                    note(ui, p, icon::WARNING_CIRCLE, p.danger, error);
                }
                ui.add_space(DIALOG_GAP);
                let confirmed = self.typed.trim() == self.bucket;
                buttons(ui, |ui| {
                    let button = egui::Button::new(RichText::new(tr!("Vider puis supprimer")).color(egui::Color32::WHITE))
                        .fill(p.danger);
                    if ui.add_enabled(confirmed, button).clicked() {
                        self.delete(true, requests);
                    }
                    close = ui.button(tr!("Annuler")).clicked();
                });
            }
            Stage::Deleting(progress) => {
                let (done, _) = progress.snapshot();
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(14.0).color(p.accent));
                    let text = if done == 0 {
                        tr!("Suppression de « {bucket} »…", bucket = self.bucket)
                    } else {
                        trn!(done, "{n} objet ou versions supprimés…", "{n} objets ou versions supprimés…")
                    };
                    ui.label(RichText::new(text).color(p.text));
                });
                ui.label(small(p, tr!("Les objets partent par lots de 1 000, versions comprises.")));
                ui.add_space(DIALOG_GAP);
                let progress = progress.clone();
                buttons(ui, |ui| {
                    if ui.button(tr!("Arrêter")).clicked() {
                        progress.cancel();
                    }
                });
                // The count moves without any event: keep drawing.
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
            }
        }
        close
    }
}
