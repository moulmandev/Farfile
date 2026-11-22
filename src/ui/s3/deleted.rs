//! The files deleted from the folder being browsed, above its listing, on
//! buckets that keep versions: each can be brought back.

use std::collections::HashSet;
use std::sync::atomic::Ordering;

use egui::{Frame, Label, Margin, RichText, ScrollArea, Ui};

use super::S3Action;
use crate::format;
use crate::s3::{Browse, FolderVersions, Versioning};
use crate::session::Session;
use crate::theme::{self, Palette};
use crate::ui::Action;
use crate::ui::widgets::{icon, icon_button, muted, small};

const MAX_HEIGHT: f32 = 168.0;

/// Draws the panel when the user asked for it, and asks for the deleted
/// files again whenever the folder is listed again.
pub fn deleted_panel(ui: &mut Ui, p: &Palette, session: &mut Session, actions: &mut Vec<Action>) {
    if !session.s3.show_deleted || session.storage.s3().is_none() || session.path.is_root() {
        return;
    }
    let wanted = (session.path.clone(), session.generation.load(Ordering::Relaxed));
    if session.s3.asked.as_ref() != Some(&wanted) {
        session.s3.asked = Some(wanted.clone());
        actions.push(Action::S3(S3Action::ListDeleted(wanted.0, wanted.1)));
    }

    // Folders that still hold something are in the listing itself.
    let listed: HashSet<&str> = session
        .listing()
        .map(|listing| {
            listing
                .entries
                .iter()
                .filter(|entry| entry.is_dir())
                .map(|entry| entry.name.as_str())
                .collect()
        })
        .unwrap_or_default();
    let filter = session.filter.trim().to_lowercase();
    let state = &session.s3;
    let current = state
        .deleted
        .as_ref()
        .filter(|known| known.path == session.path)
        .map(|known| &known.result);

    // Outlined rather than filled: its buttons have the surface colour.
    Frame::new()
        .fill(p.bg)
        .stroke(egui::Stroke::new(1.0, p.border))
        .corner_radius(10)
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let rows = match current {
                Some(Ok((Versioning::Never, _))) | Some(Err(_)) | None => None,
                Some(Ok((_, found))) => Some(rows(found, &listed, &filter)),
            };
            header(ui, p, state, rows.as_deref(), actions);
            match current {
                None => {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(14.0).color(p.muted));
                        ui.label(muted(p, tr!("Recherche des fichiers supprimés…")));
                    });
                }
                Some(Err(error)) => {
                    let text = if error.unsupported() {
                        tr!("Ce serveur ne gère pas les versions : les fichiers supprimés ne peuvent pas être retrouvés.")
                            .to_owned()
                    } else {
                        tr!("Impossible de chercher les fichiers supprimés : {error}", error)
                    };
                    ui.add(Label::new(RichText::new(text).size(13.0).color(p.danger)).wrap());
                }
                Some(Ok((Versioning::Never, _))) => {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(muted(
                            p,
                            tr!(
                                "La gestion des versions n'est pas activée sur ce bucket : un fichier supprimé ne \
                                 peut pas être récupéré."
                            ),
                        ));
                        if let Some(bucket) = session.path.segments().first()
                            && ui.small_button(tr!("Réglages du bucket…")).clicked()
                        {
                            actions.push(Action::S3(S3Action::BucketSettings(bucket.clone())));
                        }
                    });
                }
                Some(Ok((_, found))) => {
                    let rows = rows.unwrap_or_default();
                    if rows.is_empty() {
                        let text = if filter.is_empty() {
                            tr!("Aucun fichier supprimé dans ce dossier.")
                        } else {
                            tr!("Aucun fichier supprimé ne correspond au filtre.")
                        };
                        ui.label(muted(p, text));
                    } else {
                        list(ui, p, state, &rows, actions);
                    }
                    if found.truncated {
                        ui.label(small(p, tr!("Trop de versions pour tout parcourir : la liste est incomplète.")));
                    }
                }
            }
        });
    ui.add_space(6.0);
}

/// One deleted entry of the panel.
struct Row {
    /// Folders end with a slash.
    name: String,
    deleted_at: Option<i64>,
    size: Option<u64>,
    restorable: bool,
}

impl Row {
    fn is_folder(&self) -> bool {
        self.name.ends_with('/')
    }
}

fn rows(found: &FolderVersions, listed: &HashSet<&str>, filter: &str) -> Vec<Row> {
    let matches = |name: &str| filter.is_empty() || name.to_lowercase().contains(filter);
    let folders = found
        .folders
        .iter()
        .filter(|name| !listed.contains(name.as_str()) && matches(name))
        .map(|name| Row {
            name: format!("{name}/"),
            deleted_at: None,
            size: None,
            restorable: true,
        });
    let files = found.deleted.iter().filter(|file| matches(&file.name)).map(|file| Row {
        name: file.name.clone(),
        deleted_at: file.deleted_at,
        size: file.size,
        restorable: file.restorable,
    });
    folders.chain(files).collect()
}

fn header(ui: &mut Ui, p: &Palette, state: &Browse, rows: Option<&[Row]>, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(icon::CLOCK_COUNTER_CLOCKWISE).size(15.0).color(p.accent));
        ui.label(RichText::new(tr!("Fichiers supprimés")).font(theme::semibold(13.5)).color(p.text));
        if let Some(rows) = rows
            && !rows.is_empty()
        {
            ui.label(small(p, format!("· {}", rows.len())));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if icon_button(ui, p, icon::X, tr!("Masquer les fichiers supprimés"), true).clicked() {
                actions.push(Action::S3(S3Action::ToggleDeleted));
            }
            let restorable: Vec<String> =
                rows.unwrap_or_default().iter().filter(|row| row.restorable).map(|row| row.name.clone()).collect();
            if restorable.len() > 1 {
                let idle = state.restoring.is_empty();
                if ui
                    .add_enabled(idle, egui::Button::new(format!("{}  {}", icon::ARROW_COUNTER_CLOCKWISE, tr!("Tout restaurer"))))
                    .clicked()
                {
                    actions.push(Action::S3(S3Action::Restore(restorable)));
                }
            }
        });
    });
}

fn list(ui: &mut Ui, p: &Palette, state: &Browse, rows: &[Row], actions: &mut Vec<Action>) {
    ScrollArea::vertical()
        .id_salt("s3-deleted")
        .max_height(MAX_HEIGHT)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            for row in rows {
                ui.horizontal(|ui| {
                    ui.set_min_height(28.0);
                    let (glyph, tint) = if row.is_folder() { (icon::FOLDER, p.folder) } else { (icon::FILE, p.faint) };
                    ui.label(RichText::new(glyph).size(16.0).color(tint));
                    let name = row.name.trim_end_matches('/');
                    ui.add(Label::new(RichText::new(name).color(p.muted).strikethrough()).truncate());
                    let detail = if row.is_folder() {
                        tr!("dossier dont tout le contenu est supprimé").to_owned()
                    } else {
                        let mut text = match row.deleted_at {
                            Some(at) => tr!("supprimé le {date}", date = format::date(at)),
                            None => tr!("supprimé").to_owned(),
                        };
                        if let Some(size) = row.size {
                            text.push_str(&format!(" · {}", format::size(size)));
                        }
                        if !row.restorable {
                            text.push_str(" · ");
                            text.push_str(tr!("aucune version à restaurer"));
                        }
                        text
                    };
                    ui.label(small(p, detail));

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let restoring = state.restoring.contains(&row.name);
                        if restoring {
                            ui.add(egui::Spinner::new().size(14.0).color(p.muted));
                        } else if ui
                            .add_enabled(row.restorable, egui::Button::new(tr!("Restaurer")).small())
                            .on_hover_text(if row.is_folder() {
                                tr!("Fait réapparaître tous les fichiers supprimés de ce dossier")
                            } else {
                                tr!("Retire la suppression : la dernière version redevient le fichier actuel")
                            })
                            .clicked()
                        {
                            actions.push(Action::S3(S3Action::Restore(vec![row.name.clone()])));
                        }
                        if row.is_folder() {
                            if ui.small_button(tr!("Ouvrir")).clicked() {
                                actions.push(Action::S3(S3Action::OpenDeletedFolder(name.to_owned())));
                            }
                        } else if ui.small_button(tr!("Versions…")).clicked() {
                            actions.push(Action::S3(S3Action::Versions(row.name.clone())));
                        }
                    });
                });
            }
        });
}
