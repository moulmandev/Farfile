//! The S3-only screens: versions of a file, files deleted from a folder,
//! buckets (creation, deletion, settings), sharing and metadata.
//!
//! The browser reaches them through a few hooks ([`file_menu`],
//! [`bucket_menu`], [`action_bar`], [`deleted_panel`]) that draw nothing on
//! other connections. Dialogs ask the app for work through
//! [`S3Dialog::requests`] and receive the answers in [`S3Dialog::receive`].

mod buckets;
mod deleted;
mod metadata;
mod settings;
mod share;
#[cfg(test)]
mod tests;
mod versions;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use egui::{Align, Button, Color32, Frame, Label, Layout, Margin, RichText, TextEdit, Ui};

pub use deleted::deleted_panel;
pub use settings::SettingsTab;

use super::Action;
use super::dialogs::Outcome;
use super::widgets::{icon, muted, small};
use crate::s3::{Op, PublicAccess, Reply};
use crate::session::Session;
use crate::storage::{Entry, Progress, RemotePath, Storage};
use crate::theme::{Palette, semibold};

/// What the browser asks for, wrapped in `Action::S3`.
pub enum S3Action {
    /// The versions of this file of the current folder.
    Versions(String),
    /// The metadata and storage class of this file of the current folder.
    Metadata(String),
    NewBucket,
    BucketSettings(String),
    DeleteBucket(String),
    /// Show or hide the files deleted from the current folder.
    ToggleDeleted,
    /// Look for the files deleted from this folder, for the browser whose
    /// listing has this generation (the panel asks while being drawn, maybe
    /// in the view not in use).
    ListDeleted(RemotePath, u64),
    /// Bring back these deleted entries of the current folder; folders end
    /// with a slash and come back with everything below them.
    Restore(Vec<String>),
    /// Go into a sub-folder whose content is all deleted, hence not listed.
    OpenDeletedFolder(String),
}

/// The work a dialog asks the app for.
pub enum Request {
    Versions(RemotePath),
    DownloadVersion {
        path: RemotePath,
        version: String,
        modified: Option<i64>,
        size: u64,
    },
    RestoreVersion(RemotePath, String),
    DeleteVersion(RemotePath, String),
    CreateBucket {
        name: String,
        region: String,
    },
    InspectBucket(String),
    DeleteBucket {
        bucket: String,
        empty_first: bool,
        progress: Arc<Progress>,
    },
    /// The settings of a bucket: all of them, or those of one tab.
    LoadSettings(String, Option<SettingsTab>),
    SaveVersioning(String, bool),
    SaveLifecycle(String, crate::s3::lifecycle::LifecycleDoc),
    SaveCors(String, crate::s3::cors::CorsDoc),
    /// `None` removes the policy.
    SavePolicy(String, Option<String>),
    SavePublicAccess(String, PublicAccess),
    Presign {
        path: RemotePath,
        serial: u64,
        expires: Duration,
        attachment: Option<String>,
    },
    LoadAccess(RemotePath),
    SetPublic(RemotePath, bool),
    LoadMeta(RemotePath),
    SaveMeta(RemotePath, crate::s3::ObjectMeta),
    /// Close this dialog for the settings of this bucket.
    OpenSettings(String),
}

pub enum Kind {
    Versions(versions::Versions),
    NewBucket(buckets::NewBucket),
    DeleteBucket(buckets::DeleteBucket),
    Settings(Box<settings::Settings>),
    Share(share::Share),
    Metadata(Box<metadata::Metadata>),
}

pub struct S3Dialog {
    /// Answers carry it, so that they reach this dialog and no other.
    pub id: u64,
    pub storage: Arc<dyn Storage>,
    /// Work to do, taken by the app after each frame.
    pub requests: Vec<Request>,
    /// Set to close the dialog on the next frame.
    pub close: bool,
    pub kind: Kind,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

impl S3Dialog {
    fn new(storage: Arc<dyn Storage>, kind: Kind, first: Option<Request>) -> Self {
        Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            storage,
            requests: first.into_iter().collect(),
            close: false,
            kind,
        }
    }

    pub fn versions(storage: Arc<dyn Storage>, path: RemotePath) -> Self {
        let first = Request::Versions(path.clone());
        Self::new(storage, Kind::Versions(versions::Versions::new(path)), Some(first))
    }

    pub fn new_bucket(storage: Arc<dyn Storage>, region: &str) -> Self {
        Self::new(storage, Kind::NewBucket(buckets::NewBucket::new(region)), None)
    }

    pub fn delete_bucket(storage: Arc<dyn Storage>, bucket: &str) -> Self {
        let first = Request::InspectBucket(bucket.to_owned());
        Self::new(storage, Kind::DeleteBucket(buckets::DeleteBucket::new(bucket)), Some(first))
    }

    pub fn settings(storage: Arc<dyn Storage>, bucket: &str) -> Self {
        let first = Request::LoadSettings(bucket.to_owned(), None);
        Self::new(storage, Kind::Settings(Box::new(settings::Settings::new(bucket))), Some(first))
    }

    pub fn share(storage: Arc<dyn Storage>, path: RemotePath) -> Self {
        let public_url = storage.plain_url(&path);
        let first = Request::LoadAccess(path.clone());
        Self::new(storage, Kind::Share(share::Share::new(path, public_url)), Some(first))
    }

    pub fn metadata(storage: Arc<dyn Storage>, path: RemotePath) -> Self {
        let first = Request::LoadMeta(path.clone());
        Self::new(storage, Kind::Metadata(Box::new(metadata::Metadata::new(path))), Some(first))
    }

    /// Takes an answer of the app.
    pub fn receive(&mut self, reply: Reply, now: f64) {
        let requests = &mut self.requests;
        match &mut self.kind {
            Kind::Versions(dialog) => dialog.receive(reply, requests),
            Kind::NewBucket(dialog) => {
                if dialog.receive(reply) {
                    self.close = true;
                }
            }
            Kind::DeleteBucket(dialog) => {
                if dialog.receive(reply, requests) {
                    self.close = true;
                }
            }
            Kind::Settings(dialog) => dialog.receive(reply, requests),
            Kind::Share(dialog) => dialog.receive(reply, requests, now),
            Kind::Metadata(dialog) => {
                if dialog.receive(reply) {
                    self.close = true;
                }
            }
        }
    }

    /// Scripts: switches the settings to the tab of this name.
    pub fn dev_tab(&mut self, name: &str) {
        if let Kind::Settings(dialog) = &mut self.kind {
            dialog.dev_tab(name);
        }
    }

    /// Scripts: types into the field the open dialog is about.
    pub fn dev_type(&mut self, text: &str) {
        match &mut self.kind {
            Kind::NewBucket(dialog) => dialog.name = text.to_owned(),
            Kind::DeleteBucket(dialog) => dialog.typed = text.to_owned(),
            Kind::Share(dialog) => dialog.dev_type(text),
            Kind::Settings(dialog) => dialog.dev_type(text),
            Kind::Metadata(dialog) => dialog.dev_type(text),
            Kind::Versions(_) => {}
        }
    }

    /// Scripts: presses the main button of the dialog, or the one named.
    pub fn dev_press(&mut self, what: &str) {
        let requests = &mut self.requests;
        match &mut self.kind {
            Kind::Versions(dialog) => dialog.dev_press(what, requests),
            Kind::NewBucket(dialog) => dialog.dev_press(requests),
            Kind::DeleteBucket(dialog) => dialog.dev_press(requests),
            Kind::Settings(dialog) => dialog.dev_press(what, requests),
            Kind::Share(dialog) => dialog.dev_press(what, requests),
            Kind::Metadata(dialog) => dialog.dev_press(what, requests),
        }
    }
}

/// Draws an S3 dialog. These dialogs do their work themselves: they only
/// ever end by being closed.
pub fn show(ui: &mut Ui, p: &Palette, dialog: &mut S3Dialog, first_frame: bool, outcome: &mut Outcome) {
    if dialog.close {
        *outcome = Outcome::Cancelled;
        return;
    }
    let requests = &mut dialog.requests;
    let close = match &mut dialog.kind {
        Kind::Versions(form) => form.show(ui, p, requests),
        Kind::NewBucket(form) => form.show(ui, p, first_frame, requests),
        Kind::DeleteBucket(form) => form.show(ui, p, first_frame, requests),
        Kind::Settings(form) => form.show(ui, p, requests),
        Kind::Share(form) => form.show(ui, p, requests),
        Kind::Metadata(form) => form.show(ui, p, requests),
    };
    if close {
        *outcome = Outcome::Cancelled;
    }
}

/// The S3 entries of the menu of a file: sharing, versions, metadata.
pub fn file_menu(ui: &mut Ui, session: &Session, entry: &Entry, actions: &mut Vec<Action>) {
    if entry.is_dir() {
        return;
    }
    if session.storage.can_share() && ui.button(format!("{}  Partager…", icon::SHARE_NETWORK)).clicked() {
        actions.push(Action::ShareLink(entry.name.clone()));
        ui.close();
    }
    if session.storage.s3().is_none() {
        return;
    }
    if ui.button(format!("{}  Versions…", icon::CLOCK_COUNTER_CLOCKWISE)).clicked() {
        actions.push(Action::S3(S3Action::Versions(entry.name.clone())));
        ui.close();
    }
    if ui.button(format!("{}  Métadonnées…", icon::TAG)).clicked() {
        actions.push(Action::S3(S3Action::Metadata(entry.name.clone())));
        ui.close();
    }
}

/// The menu of a bucket, in the list of buckets.
pub fn bucket_menu(ui: &mut Ui, p: &Palette, session: &Session, entry: &Entry, actions: &mut Vec<Action>) {
    if session.storage.s3().is_none() || entry.kind != crate::storage::EntryKind::Bucket {
        return;
    }
    if ui.button(format!("{}  Réglages du bucket…", icon::GEAR_SIX)).clicked() {
        actions.push(Action::S3(S3Action::BucketSettings(entry.name.clone())));
        ui.close();
    }
    ui.separator();
    let delete = RichText::new(format!("{}  Supprimer le bucket…", icon::TRASH)).color(p.danger);
    if ui.button(delete).clicked() {
        actions.push(Action::S3(S3Action::DeleteBucket(entry.name.clone())));
        ui.close();
    }
}

/// The S3 buttons of the bar above the listing: a new bucket in the list
/// of buckets, the bucket's menu inside one.
pub fn action_bar(ui: &mut Ui, p: &Palette, session: &Session, compact: bool, actions: &mut Vec<Action>) {
    if session.storage.s3().is_none() {
        return;
    }
    let caption = |glyph: &str, text: &str| {
        if compact { glyph.to_owned() } else { format!("{glyph}  {text}") }
    };
    let Some(bucket) = session.path.segments().first() else {
        if ui
            .button(caption(icon::PLUS_CIRCLE, "Nouveau bucket"))
            .on_hover_text("Créer un bucket")
            .clicked()
        {
            actions.push(Action::S3(S3Action::NewBucket));
        }
        return;
    };
    let shown = session.s3.show_deleted;
    let mut text = RichText::new(caption(icon::GEAR_SIX, "Bucket"));
    if shown {
        text = text.color(p.accent);
    }
    let button = ui.button(text).on_hover_text(format!("Réglages et fichiers supprimés de « {bucket} »"));
    egui::Popup::menu(&button).show(|ui| {
        let mut ticked = shown;
        if super::widgets::checkbox(ui, p, &mut ticked, "Afficher les fichiers supprimés").clicked() {
            actions.push(Action::S3(S3Action::ToggleDeleted));
            ui.close();
        }
        ui.separator();
        if ui.button(format!("{}  Réglages du bucket…", icon::GEAR_SIX)).clicked() {
            actions.push(Action::S3(S3Action::BucketSettings(bucket.clone())));
            ui.close();
        }
    });
}

// Pieces shared by the dialogs.

const DIALOG_GAP: f32 = 14.0;
/// Room kept for the line that tells how an operation went.
const STATUS_HEIGHT: f32 = 22.0;

fn heading(ui: &mut Ui, p: &Palette, text: &str) {
    ui.add(Label::new(RichText::new(text).font(semibold(17.0)).color(p.text)).truncate());
}

/// The path of what a dialog is about, under its title.
fn subject(ui: &mut Ui, p: &Palette, glyph: &str, text: &str) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(RichText::new(glyph).size(13.0).color(p.faint));
        ui.add(Label::new(small(p, text)).truncate());
    });
}

/// A label above a field, as in the other dialogs.
fn field(ui: &mut Ui, p: &Palette, label: &str, edit: TextEdit<'_>, width: f32) -> egui::Response {
    ui.vertical(|ui| {
        ui.label(small(p, label));
        ui.add(edit.desired_width(width).margin(Margin::symmetric(10, 7)))
    })
    .inner
}

/// Upper-case small title of a section.
fn section(ui: &mut Ui, p: &Palette, text: &str) {
    ui.label(RichText::new(text.to_uppercase()).size(11.0).color(p.faint));
}

/// A boxed message: information, warning or error.
fn note(ui: &mut Ui, p: &Palette, glyph: &str, tint: Color32, text: &str) {
    Frame::new()
        .fill(tint.gamma_multiply(0.10))
        .corner_radius(8)
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                ui.label(RichText::new(glyph).size(15.0).color(tint));
                ui.add(Label::new(RichText::new(text).color(p.text).size(13.0)).wrap());
            });
        });
}

/// One line saying how an operation went.
fn status(ui: &mut Ui, p: &Palette, message: &Option<(String, bool)>) {
    if let Some((text, error)) = message {
        let (glyph, tint) = if *error { (icon::WARNING_CIRCLE, p.danger) } else { (icon::CHECK_CIRCLE, p.success) };
        ui.horizontal_top(|ui| {
            ui.label(RichText::new(glyph).color(tint));
            ui.add(Label::new(RichText::new(text).size(13.0).color(if *error { p.danger } else { p.text })).wrap());
        });
    }
}

fn busy(ui: &mut Ui, p: &Palette, text: &str) {
    ui.horizontal(|ui| {
        ui.add(egui::Spinner::new().size(14.0).color(p.muted));
        ui.label(muted(p, text));
    });
}

/// A row of buttons of which one is on, as the protocol switch of the
/// connection form. Returns true when the choice changed.
fn segmented<T: PartialEq + Copy>(ui: &mut Ui, p: &Palette, value: &mut T, options: &[(T, String)]) -> bool {
    let mut changed = false;
    // Within a right-to-left layout the row is drawn from its end.
    let reversed = ui.layout().prefer_right_to_left();
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let ordered: Vec<&(T, String)> =
            if reversed { options.iter().rev().collect() } else { options.iter().collect() };
        for (option, label) in ordered {
            let selected = *value == *option;
            let text = RichText::new(label).color(if selected { p.on_accent } else { p.text });
            let mut button = Button::new(text);
            if selected {
                button = button.fill(p.accent);
            }
            if ui.add(button).clicked() && !selected {
                *value = *option;
                changed = true;
            }
        }
    });
    changed
}

/// Buttons aligned on the right, the first one given being the rightmost.
fn buttons<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.with_layout(Layout::right_to_left(Align::Center), add).inner
}

/// A message for an answer that failed, unsupported servers explained.
fn failure(error: &crate::s3::S3Error) -> String {
    if error.unsupported() {
        format!("Ce serveur ne prend pas en charge cette fonction ({}).", code_of(error))
    } else if error.access_denied() {
        format!("Accès refusé : la clé de cette connexion n'en a pas le droit ({}).", code_of(error))
    } else {
        format!("{error}")
    }
}

fn code_of(error: &crate::s3::S3Error) -> String {
    if error.code.is_empty() { format!("HTTP {}", error.status) } else { error.code.clone() }
}

/// A text that cannot be edited, but can be selected and copied, with a
/// button that copies it. Returns true when the button was pressed.
fn copy_field(ui: &mut Ui, p: &Palette, text: &str, width: f32, copied: bool) -> bool {
    let mut shown = text.to_owned();
    let mut pressed = false;
    ui.horizontal(|ui| {
        let button_width = 96.0;
        ui.add(
            TextEdit::singleline(&mut shown)
                .font(egui::TextStyle::Monospace)
                .desired_width(width - button_width - ui.spacing().item_spacing.x)
                .margin(Margin::symmetric(10, 7)),
        );
        let label = if copied {
            RichText::new(format!("{}  Copié", icon::CHECK)).color(p.success)
        } else {
            RichText::new(format!("{}  Copier", icon::COPY_SIMPLE))
        };
        pressed = ui.add_sized([button_width, 30.0], Button::new(label)).clicked();
    });
    pressed
}

/// Readable name of an operation, for the messages of a dialog closed
/// before it ended.
pub fn op_label(op: Op) -> &'static str {
    match op {
        Op::RestoreVersion => "Restauration de la version",
        Op::DeleteVersion => "Suppression de la version",
        Op::CreateBucket => "Création du bucket",
        Op::DeleteBucket => "Suppression du bucket",
        Op::SaveVersioning => "Gestion des versions",
        Op::SaveLifecycle => "Cycle de vie",
        Op::SaveCors => "Règles CORS",
        Op::SavePolicy => "Politique du bucket",
        Op::SavePublicAccess => "Blocage de l'accès public",
        Op::SetPublic => "Accès public",
        Op::SaveMeta => "Métadonnées",
    }
}
