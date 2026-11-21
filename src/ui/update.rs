//! What the user sees of updates: the offer dialog, the status bar's
//! "Redémarrer pour mettre à jour", the version menu at the foot of the
//! sidebar (see `crate::update` for how each kind of copy updates, and
//! `crate::app::update` for what the app does about it).

use std::sync::Arc;

use egui::{Align, Button, Frame, Label, Layout, Popup, RichText, Ui, vec2};

use super::Action;
use super::dialogs::Outcome;
use super::widgets::{checkbox, icon, icon_button, muted, primary_button};
use crate::theme::{Palette, RADIUS, semibold};
use crate::update::{self, Kind, Note, Release, Status, Updater, Version};

#[derive(Clone, Debug, PartialEq)]
pub enum UpdateAction {
    /// The user asked: says what it found, even when it is nothing.
    Check,
    /// Show the release on offer, or the one waiting to be installed.
    Show(Arc<Release>),
    /// Installed copies: close, and let the installer start Farfile again.
    RestartToUpdate,
    /// Look for updates by itself, shortly after start and every few hours.
    SetAutomatic(bool),
}

/// The offer of a newer release.
pub struct UpdateDialog {
    pub release: Arc<Release>,
    pub kind: Kind,
    /// The updater's state, brought up to date by the app every frame.
    pub status: Status,
    /// The release's page on GitHub.
    pub page: String,
    /// What the user chose, with [`Outcome::Confirmed`].
    pub choice: Option<Choice>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    /// Download, install, restart or open the page, as this copy updates.
    Update,
    /// Do not offer this version again by itself.
    Skip,
    /// Open the release's page and keep the dialog.
    ReadMore,
}

impl UpdateDialog {
    pub fn new(updater: &Updater, release: Arc<Release>) -> Self {
        Self {
            page: updater.source().page(&release),
            kind: updater.kind,
            status: updater.status(),
            release,
            choice: None,
        }
    }

    fn downloading(&self) -> Option<(u64, Option<u64>)> {
        match &self.status {
            Status::Downloading { release, received, total } if release.version == self.release.version => {
                Some((*received, *total))
            }
            _ => None,
        }
    }

    pub fn ready(&self) -> bool {
        matches!(&self.status, Status::Ready { release, .. } if release.version == self.release.version)
    }
}

const WIDTH: f32 = 440.0;

/// The offer: what is new, and what to do about it.
pub fn dialog(ui: &mut Ui, p: &Palette, dialog: &mut UpdateDialog, outcome: &mut Outcome) {
    ui.set_width(WIDTH);
    ui.label(
        RichText::new(format!("Farfile {} est disponible", dialog.release.version))
            .font(semibold(17.0))
            .color(p.text),
    );
    ui.add_space(2.0);
    ui.label(muted(p, format!("Vous avez la version {}.", Version::current())));
    ui.add_space(10.0);

    let notes = update::plain_notes(&dialog.release.notes);
    if !notes.is_empty() {
        Frame::new().fill(p.surface).corner_radius(RADIUS).inner_margin(12).show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical().max_height(200.0).show(ui, |ui| {
                for note in &notes {
                    let text = match note {
                        Note::Heading(text) => RichText::new(text).strong().color(p.text),
                        Note::Bullet(text) => RichText::new(format!("•  {text}")).color(p.text),
                        Note::Text(text) => RichText::new(text).color(p.muted),
                    };
                    ui.add(Label::new(text).wrap());
                }
            });
        });
        ui.add_space(6.0);
    }
    if ui.link(format!("{}  Nouveautés sur GitHub", icon::ARROW_SQUARE_OUT)).clicked() {
        dialog.choice = Some(Choice::ReadMore);
        *outcome = Outcome::Confirmed;
    }

    // Progress, or why the last attempt failed.
    let downloading = dialog.downloading();
    if let Some((received, total)) = downloading {
        let mb = |bytes: u64| bytes as f32 / (1024.0 * 1024.0);
        let text = match total {
            Some(total) => format!("Téléchargement… {:.1} sur {:.1} Mo", mb(received), mb(total)),
            None => format!("Téléchargement… {:.1} Mo", mb(received)),
        };
        let fraction = total.map_or(0.0, |total| received as f32 / total.max(1) as f32);
        ui.add_space(8.0);
        ui.add(egui::ProgressBar::new(fraction).text(text).desired_height(18.0));
    } else if let Status::Failed(message) = &dialog.status {
        ui.add_space(8.0);
        ui.add(
            Label::new(RichText::new(format!("{}  Mise à jour impossible : {message}", icon::WARNING_CIRCLE)).color(p.danger))
                .wrap(),
        );
    }
    let busy = downloading.is_some();
    let ready = dialog.ready();

    ui.add_space(14.0);
    ui.horizontal(|ui| {
        if dialog.kind != Kind::Installed && ui.add_enabled(!busy, Button::new("Ignorer cette version")).clicked() {
            dialog.choice = Some(Choice::Skip);
            *outcome = Outcome::Confirmed;
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let (label, enabled) = match dialog.kind {
                Kind::Portable if busy || ready => ("Mise à jour…", false),
                Kind::Portable => ("Mettre à jour et redémarrer", true),
                Kind::Installed if ready => ("Redémarrer pour mettre à jour", true),
                Kind::Installed if busy => ("Téléchargement…", false),
                Kind::Installed => ("Mettre à jour", true),
                Kind::Browser | Kind::Off => ("Télécharger", true),
            };
            if primary_button(ui, p, label, enabled).clicked() {
                dialog.choice = Some(Choice::Update);
                *outcome = Outcome::Confirmed;
            }
            if ui.button("Plus tard").clicked() {
                *outcome = Outcome::Cancelled;
            }
        });
    });
}

/// The status bar's update indicator: an update waiting to be installed,
/// one being downloaded, or one on offer.
pub fn status_item(ui: &mut Ui, p: &Palette, updater: &Updater, actions: &mut Vec<Action>) {
    let pill = |ui: &mut Ui, glyph: &str, text: String| {
        let label = RichText::new(format!("{glyph}  {text}")).size(12.0).color(p.on_accent);
        // Lower than the other buttons: it sits in the status bar.
        ui.scope(|ui| {
            ui.spacing_mut().button_padding = vec2(9.0, 2.0);
            ui.spacing_mut().interact_size.y = 20.0;
            ui.add(Button::new(label).fill(p.accent).min_size(vec2(0.0, 20.0)))
        })
        .inner
    };
    match updater.status() {
        Status::Ready { release, .. } if updater.kind == Kind::Installed => {
            let response = pill(ui, icon::ARROW_CLOCKWISE, format!("Redémarrer pour passer à la {}", release.version));
            if response.on_hover_text("Installe la mise à jour et rouvre Farfile").clicked() {
                actions.push(Action::Update(UpdateAction::RestartToUpdate));
            }
            ui.add_space(8.0);
        }
        Status::Ready { release, .. } if updater.kind == Kind::Portable => {
            let text = format!("{}  Farfile {} au prochain lancement", icon::CHECK_CIRCLE, release.version);
            ui.label(RichText::new(text).size(12.0).color(p.success));
            ui.add_space(8.0);
        }
        Status::Downloading { release, received, total } => {
            let percent = total.map(|total| format!(" · {} %", received * 100 / total.max(1))).unwrap_or_default();
            let text = format!("Téléchargement de Farfile {}{percent}", release.version);
            let response = ui.add(
                Button::new(RichText::new(text).size(12.0).color(p.muted)).frame(false),
            );
            ui.add(egui::Spinner::new().size(12.0).color(p.accent));
            if response.clicked() {
                actions.push(Action::Update(UpdateAction::Show(release)));
            }
            ui.add_space(8.0);
        }
        Status::Available(release) if updater.kind != Kind::Installed => {
            let response = pill(ui, icon::ARROW_CIRCLE_UP, format!("Farfile {} disponible", release.version));
            if response.clicked() {
                actions.push(Action::Update(UpdateAction::Show(release)));
            }
            ui.add_space(8.0);
        }
        _ => {}
    }
}

/// The button at the foot of the sidebar: the version, and looking for a
/// newer one. Tinted while one is on offer.
pub fn version_button(ui: &mut Ui, p: &Palette, updater: &Updater, automatic: bool, actions: &mut Vec<Action>) {
    let status = updater.status();
    let news = matches!(status, Status::Available(_) | Status::Ready { .. });
    let tooltip = format!("Farfile {} · mises à jour", Version::current());
    let response = icon_button(ui, p, icon::INFO, &tooltip, true);
    if news {
        let dot = response.rect.right_top() + vec2(-8.0, 8.0);
        ui.painter().circle_filled(dot, 3.5, p.accent);
    }
    Popup::menu(&response).show(|ui| {
        ui.set_min_width(250.0);
        ui.label(RichText::new(format!("Farfile {}", Version::current())).font(semibold(14.0)).color(p.text));
        let how = match updater.kind {
            Kind::Installed => "Installé : se met à jour à la fermeture",
            Kind::Portable => "Version portable : demande avant de se mettre à jour",
            Kind::Browser => "Les nouvelles versions s'ouvrent sur GitHub",
            Kind::Off => "Compilé depuis les sources : pas de mises à jour",
        };
        ui.label(RichText::new(how).size(12.0).color(p.muted));
        ui.separator();

        if let Status::Available(release) | Status::Ready { release, .. } | Status::Downloading { release, .. } = &status
            && ui.button(format!("{}  Farfile {} disponible…", icon::ARROW_CIRCLE_UP, release.version)).clicked()
        {
            ui.close();
            actions.push(Action::Update(UpdateAction::Show(release.clone())));
        }
        let checking = matches!(status, Status::Checking);
        let label = if checking { "Recherche en cours…" } else { "Rechercher des mises à jour…" };
        let check = ui
            .add_enabled(updater.kind != Kind::Off && !checking, Button::new(format!("{}  {label}", icon::ARROWS_CLOCKWISE)))
            .on_disabled_hover_text(match updater.kind {
                Kind::Off => "Les versions compilées depuis les sources ne se mettent pas à jour",
                _ => "Recherche en cours…",
            });
        if check.clicked() {
            ui.close();
            actions.push(Action::Update(UpdateAction::Check));
        }
        if updater.kind != Kind::Off {
            ui.add_space(2.0);
            let mut on = automatic;
            if checkbox(ui, p, &mut on, "Rechercher automatiquement").changed() {
                actions.push(Action::Update(UpdateAction::SetAutomatic(on)));
            }
        }
        if let Status::Failed(message) = &status {
            ui.add_space(4.0);
            ui.add(Label::new(RichText::new(format!("Dernière recherche : {message}")).size(12.0).color(p.muted)).wrap());
        }
    });
}
