//! Every version of a file: download one, make it current again, delete it.

use egui::{Align, Label, Layout, RichText, Sense, Ui};
use egui_extras::{Column, TableBuilder};

use super::{DIALOG_GAP, Request, busy, buttons, failure, heading, note, status, subject};
use crate::format;
use crate::s3::{Op, Reply, S3Error, Version, Versioning};
use crate::storage::RemotePath;
use crate::theme::Palette;
use crate::ui::widgets::{danger_button, icon, icon_button, muted, placeholder, small};

const WIDTH: f32 = 660.0;
const LIST_HEIGHT: f32 = 300.0;

enum State {
    Loading,
    Ready(Versioning, Vec<Version>),
    Failed(S3Error),
}

pub struct Versions {
    path: RemotePath,
    state: State,
    /// What is being done, while it is.
    working: Option<String>,
    /// The version whose deletion awaits confirmation.
    confirm: Option<Version>,
    message: Option<(String, bool)>,
}

impl Versions {
    pub fn new(path: RemotePath) -> Self {
        Self {
            path,
            state: State::Loading,
            working: None,
            confirm: None,
            message: None,
        }
    }

    pub fn receive(&mut self, reply: Reply, requests: &mut Vec<Request>) {
        match reply {
            Reply::Versions(Ok((versioning, versions))) => self.state = State::Ready(versioning, versions),
            Reply::Versions(Err(error)) => self.state = State::Failed(error),
            Reply::Done {
                op: Op::RestoreVersion | Op::DeleteVersion,
                result,
            } => {
                self.working = None;
                self.message = Some(match result {
                    Ok(text) => (text, false),
                    Err(error) => (failure(&error), true),
                });
                requests.push(Request::Versions(self.path.clone()));
            }
            _ => {}
        }
    }

    fn reload(&mut self, requests: &mut Vec<Request>) {
        self.message = None;
        self.state = State::Loading;
        requests.push(Request::Versions(self.path.clone()));
    }

    /// Returns true to close.
    pub fn show(&mut self, ui: &mut Ui, p: &Palette, requests: &mut Vec<Request>) -> bool {
        ui.set_width(WIDTH);
        heading(ui, p, &tr!("Versions de « {name} »", name = self.path.name()));
        subject(ui, p, icon::FILE, &self.path.to_string());
        ui.add_space(DIALOG_GAP);

        let mut pressed: Option<(Press, Version)> = None;
        let mut settings = false;
        match &self.state {
            State::Loading => {
                ui.allocate_ui(egui::vec2(WIDTH, LIST_HEIGHT), |ui| {
                    ui.centered_and_justified(|ui| {
                        ui.add(egui::Spinner::new().size(24.0).color(p.accent));
                    });
                });
            }
            State::Failed(error) => {
                ui.allocate_ui(egui::vec2(WIDTH, LIST_HEIGHT), |ui| {
                    ui.set_min_height(LIST_HEIGHT);
                    let title = if error.unsupported() {
                        tr!("Ce serveur ne gère pas les versions")
                    } else {
                        tr!("Impossible de lister les versions")
                    };
                    placeholder(ui, p, icon::WARNING_CIRCLE, p.danger, title, "");
                    ui.vertical_centered(|ui| ui.add(Label::new(muted(p, failure(error))).wrap()));
                });
            }
            State::Ready(Versioning::Never, _) => {
                ui.allocate_ui(egui::vec2(WIDTH, LIST_HEIGHT), |ui| {
                    ui.set_min_height(LIST_HEIGHT);
                    placeholder(
                        ui,
                        p,
                        icon::CLOCK_COUNTER_CLOCKWISE,
                        p.faint,
                        tr!("La gestion des versions n'est pas activée sur ce bucket"),
                        tr!("Une fois activée, chaque modification ou suppression conserve l'état précédent du fichier."),
                    );
                    ui.add_space(10.0);
                    ui.vertical_centered(|ui| {
                        settings = ui.button(format!("{}  {}", icon::GEAR_SIX, tr!("Réglages du bucket…"))).clicked();
                    });
                });
            }
            State::Ready(versioning, versions) => {
                if *versioning == Versioning::Suspended {
                    note(
                        ui,
                        p,
                        icon::INFO,
                        p.folder,
                        tr!(
                            "La gestion des versions est suspendue : les versions passées restent, mais la \
                             prochaine modification remplacera la version « null »."
                        ),
                    );
                    ui.add_space(8.0);
                }
                let enabled = self.working.is_none() && self.confirm.is_none();
                ui.add_enabled_ui(enabled, |ui| {
                    pressed = table(ui, p, versions);
                });
            }
        }

        ui.add_space(10.0);
        if let Some(version) = self.confirm.clone() {
            let when = version.modified.map(precise_date).unwrap_or_else(|| "?".to_owned());
            let what = if version.delete_marker {
                if version.latest {
                    tr!("Le marqueur de suppression du {when} sera retiré : le fichier réapparaîtra.", when)
                } else {
                    tr!("Le marqueur de suppression du {when} sera retiré.", when)
                }
            } else {
                tr!("La version du {when} sera supprimée définitivement. Cette action est irréversible.", when)
            };
            note(ui, p, icon::WARNING, p.danger, &what);
            ui.add_space(8.0);
            buttons(ui, |ui| {
                let label = if version.delete_marker { tr!("Retirer le marqueur") } else { tr!("Supprimer la version") };
                if danger_button(ui, p, label).clicked() {
                    self.working = Some(tr!("Suppression…").to_owned());
                    requests.push(Request::DeleteVersion(self.path.clone(), version.id.clone()));
                    self.confirm = None;
                } else if ui.button(tr!("Annuler")).clicked() {
                    self.confirm = None;
                }
            });
            return false;
        }

        // A line kept for the outcome, so that the dialog keeps its size.
        ui.allocate_ui_with_layout(
            egui::vec2(WIDTH, super::STATUS_HEIGHT),
            Layout::top_down(Align::Min),
            |ui| {
                ui.set_min_size(egui::vec2(WIDTH, super::STATUS_HEIGHT));
                if let Some(text) = &self.working {
                    busy(ui, p, text);
                } else {
                    status(ui, p, &self.message);
                }
            },
        );
        ui.add_space(6.0);
        let mut close = false;
        buttons(ui, |ui| {
            close = ui.button(tr!("Fermer")).clicked();
            let ready = self.working.is_none() && !matches!(self.state, State::Loading);
            if ui
                .add_enabled(ready, egui::Button::new(format!("{}  {}", icon::ARROWS_CLOCKWISE, tr!("Actualiser"))))
                .clicked()
            {
                self.reload(requests);
            }
        });

        if settings && let Some(bucket) = self.path.segments().first() {
            requests.push(Request::OpenSettings(bucket.clone()));
        }
        if let Some((press, version)) = pressed {
            self.press(press, version, requests);
        }
        close
    }

    fn press(&mut self, press: Press, version: Version, requests: &mut Vec<Request>) {
        self.message = None;
        match press {
            Press::Download => requests.push(Request::DownloadVersion {
                path: self.path.clone(),
                version: version.id.clone(),
                modified: version.modified,
                size: version.size.unwrap_or(0),
            }),
            Press::Restore => {
                self.working = Some(tr!("Restauration…").to_owned());
                requests.push(Request::RestoreVersion(self.path.clone(), version.id));
            }
            Press::Delete => self.confirm = Some(version),
        }
    }

    pub fn dev_press(&mut self, what: &str, requests: &mut Vec<Request>) {
        // `restore 1`, `delete 1`, `download 1`: the version on that row, from 0.
        let (verb, row) = what.split_once(' ').unwrap_or((what, "0"));
        let row: usize = row.trim().parse().unwrap_or(0);
        let picked = match &self.state {
            State::Ready(_, versions) => versions.get(row).cloned(),
            _ => None,
        };
        match (verb, picked) {
            ("confirm", _) => {
                if let Some(version) = self.confirm.take() {
                    self.working = Some(tr!("Suppression…").to_owned());
                    requests.push(Request::DeleteVersion(self.path.clone(), version.id));
                }
            }
            ("restore", Some(version)) => self.press(Press::Restore, version, requests),
            ("delete", Some(version)) => self.press(Press::Delete, version, requests),
            ("download", Some(version)) => self.press(Press::Download, version, requests),
            ("settings", _) => {
                if let Some(bucket) = self.path.segments().first() {
                    requests.push(Request::OpenSettings(bucket.clone()));
                }
            }
            _ => self.reload(requests),
        }
    }
}

#[derive(Clone, Copy)]
enum Press {
    Download,
    Restore,
    Delete,
}

fn table(ui: &mut Ui, p: &Palette, versions: &[Version]) -> Option<(Press, Version)> {
    let mut pressed = None;
    let count = versions.iter().filter(|version| !version.delete_marker).count();
    ui.label(small(
        p,
        format!(
            "{} · {}",
            trn!(count, "{n} version", "{n} versions"),
            trn!(versions.len() - count, "{n} marqueur de suppression", "{n} marqueurs de suppression")
        ),
    ));
    ui.add_space(4.0);
    let height = LIST_HEIGHT - 22.0;
    ui.allocate_ui(egui::vec2(WIDTH, height), |ui| {
        ui.set_min_height(height);
        let top = ui.cursor().top() + 25.0;
        ui.painter().hline(ui.max_rect().x_range(), top, egui::Stroke::new(1.0, p.border));
        TableBuilder::new(ui)
            .id_salt("s3-versions")
            .striped(false)
            .resizable(false)
            .cell_layout(Layout::left_to_right(Align::Center))
            .auto_shrink([false, false])
            .max_scroll_height(height - 26.0)
            .min_scrolled_height(height - 26.0)
            .column(Column::exact(150.0))
            .column(Column::exact(82.0))
            .column(Column::exact(72.0))
            .column(Column::remainder().clip(true))
            .column(Column::exact(104.0))
            .header(24.0, |mut header| {
                let titles = [(tr!("Date"), false), ("", false), (tr!("Taille"), true), (tr!("Version"), false), ("", false)];
                for (title, right) in titles {
                    header.col(|ui| {
                        let text = RichText::new(title).size(12.0).color(p.muted);
                        if right {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| ui.label(text));
                        } else {
                            ui.label(text);
                        }
                    });
                }
            })
            .body(|body| {
                body.rows(34.0, versions.len(), |mut row| {
                    let version = &versions[row.index()];
                    row.col(|ui| {
                        let date = version.modified.map(precise_date).unwrap_or_else(|| "—".to_owned());
                        let color = if version.delete_marker { p.muted } else { p.text };
                        ui.label(RichText::new(date).color(color));
                    });
                    row.col(|ui| {
                        if version.latest {
                            let text = if version.delete_marker { tr!("Supprimé") } else { tr!("Actuelle") };
                            badge(ui, p, text, version.delete_marker);
                        }
                    });
                    row.col(|ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let size = version.size.map(format::size).unwrap_or_else(|| "—".to_owned());
                            ui.label(muted(p, size));
                        });
                    });
                    row.col(|ui| {
                        ui.add_space(10.0);
                        if version.delete_marker {
                            let text = format!("{}  {}", icon::TRASH, tr!("Marqueur de suppression"));
                            ui.label(RichText::new(text).color(p.muted));
                        } else {
                            let id = RichText::new(&version.id).monospace().size(12.0).color(p.muted);
                            let mut hover = tr!("Identifiant : {id}", id = version.id);
                            if let Some(class) = &version.storage_class {
                                hover.push('\n');
                                hover.push_str(&tr!("Classe de stockage : {class}", class));
                            }
                            ui.add(Label::new(id).truncate().sense(Sense::hover())).on_hover_text(hover);
                        }
                    });
                    row.col(|ui| {
                        // From the right, so that the bins line up.
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.spacing_mut().item_spacing.x = 2.0;
                            let tooltip = if version.delete_marker {
                                tr!("Retirer ce marqueur de suppression")
                            } else {
                                tr!("Supprimer définitivement cette version")
                            };
                            if icon_button(ui, p, icon::TRASH, tooltip, true).clicked() {
                                pressed = Some((Press::Delete, version.clone()));
                            }
                            if version.delete_marker {
                                return;
                            }
                            let restore = icon_button(
                                ui,
                                p,
                                icon::ARROW_COUNTER_CLOCKWISE,
                                if version.latest {
                                    tr!("C'est déjà la version actuelle")
                                } else {
                                    tr!("Restaurer : en faire la version actuelle (copie)")
                                },
                                !version.latest,
                            );
                            if restore.clicked() {
                                pressed = Some((Press::Restore, version.clone()));
                            }
                            if icon_button(ui, p, icon::DOWNLOAD_SIMPLE, tr!("Télécharger cette version"), true).clicked() {
                                pressed = Some((Press::Download, version.clone()));
                            }
                        });
                    });
                });
            });
    });
    pressed
}

/// A small pill: painted, so that it keeps its height in a table cell.
fn badge(ui: &mut Ui, p: &Palette, text: &str, danger: bool) {
    let tint = if danger { p.danger } else { p.accent };
    let galley = ui.painter().layout_no_wrap(text.to_owned(), egui::FontId::proportional(11.5), tint);
    let size = galley.size() + egui::vec2(14.0, 5.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter().rect_filled(rect, 6, tint.gamma_multiply(0.16));
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, tint);
}

/// Versions are often minutes apart: the seconds tell them apart.
fn precise_date(unix_seconds: i64) -> String {
    chrono::DateTime::from_timestamp(unix_seconds, 0)
        .map(|utc| utc.with_timezone(&chrono::Local).format(format::date_seconds_pattern()).to_string())
        .unwrap_or_default()
}
