//! The questions transfers ask: what to do with a file that exists, and
//! the preview of a folder synchronisation.

use std::path::PathBuf;
use std::sync::Arc;

use egui::{Align, Button, Label, Layout, RichText, Sense, Ui, vec2};
use egui_extras::{Column, TableBuilder};

use super::dialogs::Outcome;
use super::widgets::{checkbox, danger_button, icon, muted, primary_button, small};
use crate::format;
use crate::storage::{RemotePath, Storage};
use crate::theme::{Palette, semibold};
use crate::transfers::conflict::{ConflictPolicy, Version};
use crate::transfers::sync::{self, Reason, Rules, SyncAction, SyncDirection, SyncLine};
use crate::worker::Direction;
use crate::worker::transfer::Question;

const CONFLICT_WIDTH: f32 = 560.0;
const SYNC_WIDTH: f32 = 780.0;
/// The preview keeps this height whatever it holds, so the dialog does
/// not jump as the comparison comes in.
const PREVIEW_HEIGHT: f32 = 300.0;

pub enum TransferDialog {
    Conflict(ConflictForm),
    Sync(Box<SyncForm>),
}

/// "Ce fichier existe déjà".
pub struct ConflictForm {
    pub question: Question,
    /// For the other conflicts of the same batch too.
    pub all: bool,
    /// The button clicked.
    pub choice: Option<ConflictPolicy>,
}

impl ConflictForm {
    pub fn new(question: Question) -> Self {
        Self {
            question,
            all: false,
            choice: None,
        }
    }
}

pub enum SyncState {
    /// Nothing compared yet, or the options changed since.
    Idle,
    Comparing(u64),
    Compared(Vec<SyncLine>),
    Failed(String),
}

pub struct SyncForm {
    pub storage: Arc<dyn Storage>,
    /// The connection, to remember the local folder with.
    pub profile_id: String,
    /// The connection as the user named it.
    pub title: String,
    pub remote: RemotePath,
    pub local: Option<PathBuf>,
    pub direction: SyncDirection,
    pub mirror: bool,
    pub state: SyncState,
    /// Set when something changed that calls for a new comparison.
    pub stale: bool,
}

impl SyncForm {
    pub fn rules(&self) -> Rules {
        Rules {
            direction: self.direction,
            mirror: self.mirror && self.direction.can_mirror(),
            precision: self.storage.date_precision(),
            // S3 dates an object with its upload, whatever the original.
            remote_keeps_dates: self.storage.keeps_dates(),
        }
    }

    pub fn lines(&self) -> &[SyncLine] {
        match &self.state {
            SyncState::Compared(lines) => lines,
            _ => &[],
        }
    }
}

/// Draws the dialog; sets `outcome` once it is answered.
pub fn show(ui: &mut Ui, p: &Palette, dialog: &mut TransferDialog, outcome: &mut Outcome) {
    match dialog {
        TransferDialog::Conflict(form) => conflict(ui, p, form, outcome),
        TransferDialog::Sync(form) => sync_form(ui, p, form, outcome),
    }
}

fn title(ui: &mut Ui, p: &Palette, text: &str) {
    ui.label(RichText::new(text).font(semibold(17.0)).color(p.text));
    ui.add_space(8.0);
}

fn describe(version: Version) -> String {
    let mut text = format::size(version.size);
    if let Some(modified) = version.modified {
        text.push_str(&format!(" · {}", format::date(modified)));
    }
    text
}

fn conflict(ui: &mut Ui, p: &Palette, form: &mut ConflictForm, outcome: &mut Outcome) {
    ui.set_width(CONFLICT_WIDTH);
    let question = &form.question;
    title(ui, p, "Ce fichier existe déjà");
    ui.add(
        Label::new(
            RichText::new(format!("« {} » est déjà présent dans {}.", question.name, short_folder(&question.folder)))
                .color(p.text),
        )
        .wrap(),
    )
    .on_hover_text(&question.folder);
    ui.add_space(10.0);

    let (source_place, existing_place) = match question.direction {
        Direction::Download => ("Sur le serveur", "Sur cet ordinateur"),
        Direction::Upload => ("Sur cet ordinateur", "Sur le serveur"),
    };
    let newer = match (question.source.modified, question.existing.modified) {
        (Some(source), Some(existing)) if source > existing + 1 => Some(true),
        (Some(source), Some(existing)) if existing > source + 1 => Some(false),
        _ => None,
    };
    let size_note = |this: u64, other: u64| match this.cmp(&other) {
        std::cmp::Ordering::Greater => Some("plus grand"),
        std::cmp::Ordering::Less => Some("plus petit"),
        std::cmp::Ordering::Equal => None,
    };
    let source_glyph = match question.direction {
        Direction::Download => icon::CLOUD_ARROW_DOWN,
        Direction::Upload => icon::CLOUD_ARROW_UP,
    };
    version_card(
        ui,
        p,
        source_glyph,
        &format!("Nouveau · {source_place}"),
        question.source,
        [newer.filter(|newer| *newer).map(|_| "plus récent"), size_note(question.source.size, question.existing.size)],
    );
    ui.add_space(4.0);
    version_card(
        ui,
        p,
        icon::FILE,
        &format!("Présent · {existing_place}"),
        question.existing,
        [newer.filter(|newer| !newer).map(|_| "plus récent"), size_note(question.existing.size, question.source.size)],
    );
    ui.add_space(14.0);

    let choices: [(ConflictPolicy, &str, bool); 6] = [
        (ConflictPolicy::Overwrite, "Le fichier présent est remplacé.", true),
        (ConflictPolicy::OverwriteIfNewer, "Remplacé seulement si le nouveau est plus récent.", true),
        (
            ConflictPolicy::OverwriteIfSizeDiffers,
            "Remplacé seulement si les tailles diffèrent.",
            true,
        ),
        (
            ConflictPolicy::Resume,
            if question.can_resume {
                "Le fichier présent est le début du nouveau, interrompu : seule la suite est copiée."
            } else {
                "Possible seulement quand le fichier présent est plus petit, et que la connexion sait reprendre."
            },
            question.can_resume,
        ),
        (ConflictPolicy::Rename, "", true),
        (ConflictPolicy::Skip, "Le fichier présent est laissé tel quel.", true),
    ];
    let renamed = format!("La copie s'appellera « {} ».", question.renamed);
    let gap = ui.spacing().item_spacing.x;
    let width = (CONFLICT_WIDTH - 2.0 * gap) / 3.0;
    for row in choices.chunks(3) {
        ui.horizontal(|ui| {
            for (policy, hint, enabled) in row {
                let hint = if *policy == ConflictPolicy::Rename { renamed.as_str() } else { hint };
                let button = ui
                    .add_enabled(*enabled, Button::new(policy.label()).min_size(vec2(width, 34.0)))
                    .on_hover_text(hint)
                    .on_disabled_hover_text(hint);
                if button.clicked() {
                    form.choice = Some(*policy);
                    *outcome = Outcome::Confirmed;
                }
            }
        });
    }
    ui.add_space(10.0);
    if question.others > 0 || question.more {
        let label = if question.more {
            // The files of a folder are still being found: how many is not known.
            "Appliquer à tous les fichiers déjà présents".to_owned()
        } else if question.others > 1 {
            format!("Appliquer aux {} autres fichiers déjà présents", question.others)
        } else {
            "Appliquer aussi à l'autre fichier déjà présent".to_owned()
        };
        checkbox(ui, p, &mut form.all, label);
        ui.add_space(4.0);
    }
    ui.label(small(p, "Échap ignore ce fichier. La règle par défaut se change dans le panneau des transferts."));
}

/// A long folder keeps its start and its last two levels: the drive and
/// where it ends are what tell it apart.
fn short_folder(folder: &str) -> String {
    const LONGEST: usize = 56;
    if folder.chars().count() <= LONGEST {
        return folder.to_owned();
    }
    let separator = if folder.contains('\\') { '\\' } else { '/' };
    let parts: Vec<&str> = folder.split(separator).collect();
    if parts.len() <= 4 {
        return folder.to_owned();
    }
    let tail = parts[parts.len() - 2..].join(&separator.to_string());
    format!("{}{separator}…{separator}{tail}", parts[0])
}

fn version_card(ui: &mut Ui, p: &Palette, glyph: &str, place: &str, version: Version, notes: [Option<&str>; 2]) {
    egui::Frame::new()
        .fill(p.surface)
        .corner_radius(8)
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(CONFLICT_WIDTH - 24.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(glyph).size(18.0).color(p.accent));
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.label(RichText::new(place).size(12.0).color(p.muted));
                    ui.label(RichText::new(describe(version)).color(p.text));
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let notes: Vec<&str> = notes.iter().flatten().copied().collect();
                    ui.label(RichText::new(notes.join(" · ")).size(12.0).color(p.folder));
                });
            });
        });
}

fn sync_form(ui: &mut Ui, p: &Palette, form: &mut SyncForm, outcome: &mut Outcome) {
    ui.set_width(SYNC_WIDTH);
    title(ui, p, "Synchroniser un dossier");

    egui::Grid::new("sync-folders").num_columns(2).spacing([14.0, 6.0]).show(ui, |ui| {
        ui.label(small(p, "Distant"));
        ui.horizontal(|ui| {
            ui.label(RichText::new(icon::HARD_DRIVES).color(p.accent));
            ui.add(Label::new(RichText::new(format!("{} · {}", form.title, form.remote)).color(p.text)).truncate());
        });
        ui.end_row();
        ui.label(small(p, "Local"));
        ui.horizontal(|ui| {
            ui.label(RichText::new(icon::LAPTOP).color(p.accent));
            let full = form
                .local
                .as_ref()
                .map_or_else(|| "Aucun dossier choisi".to_owned(), |local| local.display().to_string());
            ui.add(Label::new(RichText::new(short_folder(&full)).color(p.text)).truncate())
                .on_hover_text(full);
            if !crate::dev::active() && ui.button("Changer…").clicked() {
                let mut dialog = rfd::FileDialog::new().set_title("Dossier local à synchroniser");
                if let Some(local) = &form.local {
                    dialog = dialog.set_directory(local);
                }
                if let Some(folder) = dialog.pick_folder() {
                    form.local = Some(folder);
                    form.stale = true;
                }
            }
        });
        ui.end_row();
    });
    ui.add_space(10.0);

    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for direction in SyncDirection::ALL {
            let glyph = match direction {
                SyncDirection::Upload => icon::ARROW_UP,
                SyncDirection::Download => icon::ARROW_DOWN,
                SyncDirection::Both => icon::ARROWS_DOWN_UP,
            };
            let selected = form.direction == direction;
            let text = RichText::new(format!("{glyph}  {}", direction.label()))
                .color(if selected { p.on_accent } else { p.text });
            let mut button = Button::new(text).min_size(vec2(150.0, 0.0));
            if selected {
                button = button.fill(p.accent);
            }
            if ui.add(button).clicked() && !selected {
                form.direction = direction;
                form.stale = true;
            }
        }
    });
    ui.label(small(p, form.direction.hint()));
    ui.add_space(4.0);
    ui.add_enabled_ui(form.direction.can_mirror(), |ui| {
        let label = "Miroir : supprimer aussi ce qui n'existe que du côté de la destination";
        if checkbox(ui, p, &mut form.mirror, label).changed() {
            form.stale = true;
        }
    });
    ui.add_space(10.0);

    preview(ui, p, form);
    ui.add_space(12.0);

    let totals = sync::totals(form.lines());
    let count = totals.uploads + totals.downloads + totals.deletions;
    ui.horizontal(|ui| {
        let mut parts = Vec::new();
        if totals.uploads > 0 {
            parts.push(format::count(totals.uploads, "envoi", "envois"));
        }
        if totals.downloads > 0 {
            parts.push(format::count(totals.downloads, "téléchargement", "téléchargements"));
        }
        if totals.bytes > 0 {
            parts.push(format::size(totals.bytes));
        }
        ui.label(small(p, parts.join(" · ")));
        if totals.deletions > 0 {
            ui.label(
                RichText::new(format!("{}  {}", icon::TRASH, format::count(totals.deletions, "suppression", "suppressions")))
                    .size(12.0)
                    .color(p.danger),
            );
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let label = format!("Synchroniser ({count})");
            let clicked = if totals.deletions > 0 {
                count > 0 && danger_button(ui, p, &label).clicked()
            } else {
                primary_button(ui, p, &label, count > 0).clicked()
            };
            if clicked {
                *outcome = Outcome::Confirmed;
            }
            if ui.button("Annuler").clicked() {
                *outcome = Outcome::Cancelled;
            }
        });
    });
}

fn preview(ui: &mut Ui, p: &Palette, form: &mut SyncForm) {
    let lines = match &mut form.state {
        SyncState::Compared(lines) if !lines.is_empty() => lines,
        state => {
            let (glyph, tint, text) = match state {
                SyncState::Idle if form.local.is_none() => (icon::FOLDER_OPEN, p.faint, "Choisissez un dossier local.".to_owned()),
                SyncState::Idle | SyncState::Comparing(_) => ("", p.accent, "Comparaison des deux dossiers…".to_owned()),
                SyncState::Failed(error) => (icon::WARNING_CIRCLE, p.danger, format!("Comparaison impossible : {error}")),
                SyncState::Compared(_) => (icon::CHECK_CIRCLE, p.success, "Les deux dossiers sont identiques.".to_owned()),
            };
            let (rect, _) = ui.allocate_exact_size(vec2(SYNC_WIDTH, PREVIEW_HEIGHT), Sense::hover());
            ui.painter().rect_filled(rect, 8, p.surface);
            let mut inner = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink(16.0)));
            inner.vertical_centered(|ui| {
                ui.add_space(PREVIEW_HEIGHT * 0.32);
                if glyph.is_empty() {
                    ui.add(egui::Spinner::new().size(24.0).color(tint));
                } else {
                    ui.label(RichText::new(glyph).size(30.0).color(tint));
                }
                ui.add_space(4.0);
                ui.add(Label::new(muted(p, text)).wrap());
            });
            return;
        }
    };

    ui.horizontal(|ui| {
        ui.label(small(p, format::count(lines.len(), "différence", "différences")));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let link = |ui: &mut Ui, text: &str| {
                ui.add(Label::new(RichText::new(text).size(12.5).color(p.accent)).sense(Sense::click()))
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
            };
            if link(ui, "Tout décocher") {
                lines.iter_mut().for_each(|line| line.selected = false);
            }
            if link(ui, "Tout cocher") {
                lines.iter_mut().for_each(|line| line.selected = true);
            }
        });
    });

    egui::Frame::new().fill(p.surface).corner_radius(8).show(ui, |ui| {
        ui.set_width(SYNC_WIDTH);
        ui.set_height(PREVIEW_HEIGHT - 26.0);
        TableBuilder::new(ui)
            .id_salt("sync-preview")
            .striped(false)
            .resizable(false)
            .auto_shrink([false, false])
            .max_scroll_height(PREVIEW_HEIGHT - 26.0 - 26.0)
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::exact(34.0))
            .column(Column::exact(178.0))
            .column(Column::remainder().clip(true))
            .column(Column::exact(150.0))
            .column(Column::exact(150.0))
            .header(24.0, |mut header| {
                for title in ["", "Action", "Fichier", "Local", "Distant"] {
                    header.col(|ui| {
                        ui.label(RichText::new(title).size(12.0).color(p.muted));
                    });
                }
            })
            .body(|body| {
                body.rows(40.0, lines.len(), |mut row| {
                    let line = &mut lines[row.index()];
                    row.col(|ui| {
                        ui.add_space(8.0);
                        checkbox(ui, p, &mut line.selected, "");
                    });
                    let (glyph, label, tint) = action_look(p, line);
                    row.col(|ui| {
                        ui.label(RichText::new(glyph).color(tint));
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            ui.label(RichText::new(label).size(12.5).color(tint));
                            let reason = line.reason.label(line.action);
                            let reason_tint = match line.reason {
                                Reason::DestinationNewer | Reason::Undecided => p.folder,
                                Reason::Orphan => p.danger,
                                _ => p.muted,
                            };
                            ui.label(RichText::new(reason).size(11.0).color(reason_tint));
                        });
                    });
                    row.col(|ui| {
                        let color = if line.selected { p.text } else { p.muted };
                        ui.add(Label::new(RichText::new(&line.relative).color(color)).truncate());
                    });
                    for side in [line.local, line.remote] {
                        row.col(|ui| {
                            let text = side.map_or_else(
                                || "—".to_owned(),
                                |file| {
                                    let date = file.modified.map(format::date).unwrap_or_default();
                                    format!("{} · {date}", format::size(file.size))
                                },
                            );
                            ui.add(Label::new(RichText::new(text).size(11.5).color(p.muted)).truncate());
                        });
                    }
                });
            });
    });
}

fn action_look(p: &Palette, line: &SyncLine) -> (&'static str, &'static str, egui::Color32) {
    match line.action {
        SyncAction::Upload => (icon::ARROW_UP, "Envoyer", p.success),
        SyncAction::Download => (icon::ARROW_DOWN, "Recevoir", p.accent),
        SyncAction::DeleteRemote => (icon::TRASH, "Supprimer à distance", p.danger),
        SyncAction::DeleteLocal => (icon::TRASH, "Supprimer en local", p.danger),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_folders_keep_their_ends() {
        assert_eq!(short_folder(r"C:\Users\Theo\Downloads"), r"C:\Users\Theo\Downloads");
        assert_eq!(
            short_folder(r"C:\Users\Theo\AppData\Local\Temp\claude\0a57d62260a3dabf\scratchpad\agents\transfers\downloads"),
            r"C:\…\transfers\downloads"
        );
        assert_eq!(
            short_folder("/config/a-rather-long-folder-name/with/many/levels/below/it/website"),
            "/…/it/website"
        );
    }
}
