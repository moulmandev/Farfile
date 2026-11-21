//! Updates in the app: reacting to what the updater finds, the offer
//! dialog's answers, and putting a downloaded update in place on the way
//! out (see `crate::update` for how each kind of copy updates).

use std::sync::Arc;

use super::App;
use crate::dev;
use crate::ui::dialogs::Dialog;
use crate::ui::update::{Choice, UpdateAction, UpdateDialog};
use crate::update::{self, Kind, Notice, Release, Status, Version};

impl App {
    /// Once a frame: runs the timer and reacts to what the updater found.
    pub(super) fn update_frame(&mut self, ctx: &egui::Context, now: f64) {
        self.updater.tick(ctx, self.settings.check_updates);

        for notice in self.updater.notices() {
            match notice {
                Notice::Found { release, asked } => self.update_found(ctx, release, asked),
                Notice::UpToDate => {
                    self.toast(now, format!("Farfile est à jour : la version {} est la dernière.", Version::current()), false);
                }
                Notice::Downloaded(release, file) => match self.updater.kind {
                    Kind::Installed => self.toast(
                        now,
                        format!("Farfile {} est prêt : il s'installera à la fermeture.", release.version),
                        false,
                    ),
                    Kind::Portable => match update::replace_self(&file) {
                        Ok(()) => {
                            // The new executable is in place; this one
                            // closes and starts it (see `on_exit`).
                            self.updater.replaced(release.clone());
                            self.updater.restart_on_exit = true;
                            self.toast(now, format!("Farfile {} est installé : redémarrage…", release.version), false);
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        Err(message) => self.toast(now, format!("Mise à jour impossible : {message}"), true),
                    },
                    Kind::Browser | Kind::Off => {}
                },
                Notice::Failed { message, asked } => {
                    if asked {
                        self.toast(now, format!("Mise à jour impossible : {message}"), true);
                    }
                }
            }
        }

        // The dialog follows the download; an offer waits for the dialog
        // in front to close.
        let status = self.updater.status();
        if let Some(Dialog::Update(dialog)) = &mut self.dialog {
            dialog.status = status;
        } else if self.dialog.is_none()
            && self.quick_open.is_none()
            && let Some(release) = self.updater.offer.take()
        {
            self.open_dialog(Dialog::Update(UpdateDialog::new(&self.updater, release)));
        }
    }

    /// A newer release: an installed copy fetches it straight away; the
    /// others ask, unless the user skipped this version (a check they ran
    /// still asks).
    fn update_found(&mut self, ctx: &egui::Context, release: Arc<Release>, asked: bool) {
        match self.updater.kind {
            Kind::Installed => {
                // A check the user ran shows the download's progress.
                if asked {
                    self.updater.offer = Some(release.clone());
                }
                self.updater.download(ctx, release, asked);
            }
            Kind::Portable | Kind::Browser => {
                let skipped = self.settings.skipped_update.as_deref() == Some(&release.version.to_string());
                if asked || !skipped {
                    self.updater.offer = Some(release);
                }
            }
            Kind::Off => {}
        }
    }

    pub(super) fn apply_update(&mut self, action: UpdateAction, ctx: &egui::Context) {
        match action {
            UpdateAction::Check => match self.updater.status() {
                // Already known: show it again rather than asking GitHub.
                Status::Available(release) | Status::Ready { release, .. } | Status::Downloading { release, .. } => {
                    self.open_dialog(Dialog::Update(UpdateDialog::new(&self.updater, release)));
                }
                _ => self.updater.check(ctx, true),
            },
            UpdateAction::Show(release) => self.open_dialog(Dialog::Update(UpdateDialog::new(&self.updater, release))),
            UpdateAction::RestartToUpdate => self.restart_to_update(ctx),
            UpdateAction::SetAutomatic(on) => {
                self.settings.check_updates = on;
                self.save_settings();
            }
        }
    }

    /// The offer dialog was answered.
    pub(super) fn answer_update(&mut self, mut dialog: UpdateDialog, ctx: &egui::Context, now: f64) {
        // Without a choice, a script confirmed it: the main button.
        match dialog.choice.take().unwrap_or(Choice::Update) {
            Choice::Skip => {
                self.settings.skipped_update = Some(dialog.release.version.to_string());
                self.save_settings();
            }
            Choice::ReadMore => {
                self.open_release_page(&dialog.page, now);
                self.dialog = Some(Dialog::Update(dialog));
            }
            Choice::Update => match dialog.kind {
                Kind::Installed if dialog.ready() => self.restart_to_update(ctx),
                Kind::Installed | Kind::Portable => {
                    self.updater.download(ctx, dialog.release.clone(), true);
                    dialog.status = self.updater.status();
                    self.dialog = Some(Dialog::Update(dialog));
                }
                Kind::Browser | Kind::Off => self.open_release_page(&dialog.page, now),
            },
        }
    }

    /// Installed copies: close, and run the waiting installer on the way
    /// out so that it starts Farfile again once done. Files with changes
    /// still get their question before leaving.
    fn restart_to_update(&mut self, ctx: &egui::Context) {
        if self.updater.kind == Kind::Installed && matches!(self.updater.status(), Status::Ready { .. }) {
            self.updater.restart_on_exit = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn open_release_page(&mut self, page: &str, now: f64) {
        if dev::active() {
            self.toast(now, format!("Lien à ouvrir : {page}"), false);
        } else if let Err(error) = open::that_detached(page) {
            self.toast(now, format!("Ouverture du lien impossible : {error}"), true);
        }
    }

    /// The app is closing: an update waiting is put in place.
    pub(super) fn update_on_exit(&mut self) {
        if let Err(message) = self.updater.on_exit() {
            eprintln!("update: {message}");
        }
    }
}
