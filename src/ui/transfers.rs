//! Bottom panel: the transfer queue, its speed limits and its rules; and
//! the entries that open a folder synchronisation.

use std::collections::HashMap;
use std::sync::Arc;

use egui::{Align, Button, DragValue, Label, Layout, Popup, ProgressBar, RichText, ScrollArea, Ui, vec2};

use super::Action;
use super::widgets::{icon, icon_button, small};
use crate::format;
use crate::session::Session;
use crate::storage::Entry;
use crate::theme::{Palette, semibold};
use crate::transfers::conflict::ConflictPolicy;
use crate::transfers::{MAX_PARALLEL, SMALL_FILE, SMALL_PARALLEL, Shared, TransferSettings};
use crate::worker::{Direction, Transfer, TransferState};

const ROW_HEIGHT: f32 = 44.0;
/// Width kept on the right of each row for the bar, the figures and the buttons.
const STATUS_WIDTH: f32 = 500.0;
const KIB: u64 = 1024;
const MIB: u64 = 1024 * 1024;

/// What the panel asks of the app.
pub enum TransferAction {
    /// Bytes per second; 0 lifts the limit.
    SetDownloadLimit(u64),
    SetUploadLimit(u64),
    SetParallel(usize),
    SetPolicy(ConflictPolicy),
    PauseAll,
    ResumeAll,
    /// Synchronise this folder of the current listing with a local one;
    /// an empty name for the folder being shown.
    AskSync(String),
}

/// Smoothed speed of a running transfer.
#[derive(Default)]
pub struct Meter {
    pub sampled_at: f64,
    pub sampled_bytes: u64,
    pub bytes_per_second: f64,
}

impl Meter {
    /// Feeds a new measurement; samples closer than half a second are skipped.
    pub fn sample(&mut self, now: f64, bytes: u64) {
        if self.sampled_at == 0.0 {
            self.sampled_at = now;
            self.sampled_bytes = bytes;
            return;
        }
        let elapsed = now - self.sampled_at;
        if elapsed < 0.5 {
            return;
        }
        let instant = bytes.saturating_sub(self.sampled_bytes) as f64 / elapsed;
        self.bytes_per_second = if self.bytes_per_second == 0.0 {
            instant
        } else {
            self.bytes_per_second * 0.6 + instant * 0.4
        };
        self.sampled_at = now;
        self.sampled_bytes = bytes;
    }
}

/// What the panel shows.
pub struct Panel<'a> {
    pub transfers: &'a [Arc<Transfer>],
    pub meters: &'a HashMap<u64, Meter>,
    pub settings: &'a TransferSettings,
    /// Where waiting transfers stand in the queue.
    pub rules: &'a Shared,
}

fn settings_popup_id() -> egui::Id {
    egui::Id::new("transfer-settings")
}

/// Opens the settings of the panel, as clicking their button does.
pub fn open_settings(ctx: &egui::Context) {
    Popup::open_id(ctx, settings_popup_id());
}

fn rank(state: &TransferState) -> u8 {
    match state {
        TransferState::Running => 0,
        TransferState::Queued | TransferState::Paused => 1,
        TransferState::Failed(_) => 2,
        TransferState::Done | TransferState::Cancelled | TransferState::Skipped => 3,
    }
}

pub fn show(ui: &mut Ui, p: &Palette, panel: &Panel<'_>, actions: &mut Vec<Action>) {
    let transfers = panel.transfers;
    // Running first, then waiting, then failures, then the rest.
    let mut order: Vec<(u8, usize, TransferState)> = transfers
        .iter()
        .enumerate()
        .map(|(index, transfer)| {
            let state = transfer.state();
            (rank(&state), index, state)
        })
        .collect();
    order.sort_by_key(|(rank, index, _)| (*rank, *index));

    let active = order.iter().filter(|(rank, ..)| *rank <= 1).count();
    let failed = order.iter().filter(|(rank, ..)| *rank == 2).count();
    let paused = order.iter().filter(|(.., state)| *state == TransferState::Paused).count();
    let pausable = order
        .iter()
        .filter(|(_, index, state)| {
            matches!(state, TransferState::Queued | TransferState::Running) && transfers[*index].can_pause()
        })
        .count();

    ui.horizontal(|ui| {
        ui.label(RichText::new("Transferts").font(semibold(14.0)).color(p.text));
        let mut parts = Vec::new();
        if active > paused {
            parts.push(format!("{} en cours", active - paused));
        }
        if paused > 0 {
            parts.push(format::count(paused, "en pause", "en pause"));
        }
        if failed > 0 {
            parts.push(format::count(failed, "échec", "échecs"));
        }
        let finished = transfers.len() - active - failed;
        if finished > 0 {
            parts.push(format::count(finished, "terminé", "terminés"));
        }
        ui.label(small(p, parts.join(" · ")));

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if icon_button(ui, p, icon::X, "Masquer", true).clicked() {
                actions.push(Action::ToggleTransfers);
            }
            if transfers.len() > active && ui.button("Effacer les terminés").clicked() {
                actions.push(Action::ClearFinishedTransfers);
            }
            if paused > 0 && ui.button(format!("{}  Tout reprendre", icon::PLAY)).clicked() {
                actions.push(Action::Transfers(TransferAction::ResumeAll));
            } else if paused == 0
                && pausable > 1
                && ui.button(format!("{}  Tout suspendre", icon::PAUSE)).clicked()
            {
                actions.push(Action::Transfers(TransferAction::PauseAll));
            }
            settings_button(ui, p, panel.settings, actions);
        });
    });
    ui.add_space(2.0);

    if order.is_empty() {
        ui.add_space(12.0);
        ui.vertical_centered(|ui| ui.label(small(p, "Aucun transfert pour l'instant.")));
        return;
    }

    ScrollArea::vertical().auto_shrink([false, false]).show_rows(
        ui,
        ROW_HEIGHT,
        order.len(),
        |ui, range| {
            for (_, index, state) in &order[range] {
                let transfer = &transfers[*index];
                let place = if *state == TransferState::Queued {
                    panel.rules.slots.position(transfer.id)
                } else {
                    None
                };
                row(ui, p, transfer, state, panel.meters.get(&transfer.id), place);
            }
        },
    );
}

/// `1er`, `2e`, `3e`…
fn ordinal(n: usize) -> String {
    if n == 1 { "1er".to_owned() } else { format!("{n}e") }
}

/// A speed limit as the panel says it.
pub fn limit_label(bytes_per_second: u64) -> String {
    if bytes_per_second == 0 {
        "illimité".to_owned()
    } else {
        format::speed(bytes_per_second as f64)
    }
}

/// The button of the header that shows the limits in force and opens
/// their settings.
fn settings_button(ui: &mut Ui, p: &Palette, settings: &TransferSettings, actions: &mut Vec<Action>) {
    let limited = settings.download_limit > 0 || settings.upload_limit > 0;
    let mut text = format!("{}  {} à la fois", icon::GAUGE, settings.parallel);
    if limited {
        text.push_str(&format!(
            " · {} {} · {} {}",
            icon::ARROW_DOWN,
            limit_label(settings.download_limit),
            icon::ARROW_UP,
            limit_label(settings.upload_limit)
        ));
    }
    let color = if limited { p.accent } else { p.muted };
    let button = ui
        .add(Button::new(RichText::new(text).size(12.5).color(color)).frame(false))
        .on_hover_text("Vitesse, transferts simultanés, fichiers déjà présents");
    Popup::menu(&button)
        .id(settings_popup_id())
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| settings_form(ui, p, settings, actions));
}

fn settings_form(ui: &mut Ui, p: &Palette, settings: &TransferSettings, actions: &mut Vec<Action>) {
    ui.set_width(300.0);
    ui.spacing_mut().item_spacing.y = 6.0;
    ui.label(RichText::new("Vitesse maximale").font(semibold(13.0)).color(p.text));
    let download = limit_row(ui, p, "down", icon::ARROW_DOWN, "Téléchargement", settings.download_limit);
    if let Some(rate) = download {
        actions.push(Action::Transfers(TransferAction::SetDownloadLimit(rate)));
    }
    if let Some(rate) = limit_row(ui, p, "up", icon::ARROW_UP, "Envoi", settings.upload_limit) {
        actions.push(Action::Transfers(TransferAction::SetUploadLimit(rate)));
    }
    ui.label(small(p, "0 : sans limite. S'applique aussi aux transferts en cours."));
    ui.separator();

    ui.horizontal(|ui| {
        ui.label(RichText::new("Transferts simultanés").color(p.text));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let parallel = settings.parallel;
            if icon_button(ui, p, icon::PLUS, "Un de plus", parallel < MAX_PARALLEL).clicked() {
                actions.push(Action::Transfers(TransferAction::SetParallel(parallel + 1)));
            }
            ui.label(RichText::new(parallel.to_string()).font(semibold(14.0)).color(p.text));
            if icon_button(ui, p, icon::MINUS, "Un de moins", parallel > 1).clicked() {
                actions.push(Action::Transfers(TransferAction::SetParallel(parallel - 1)));
            }
        });
    });
    let small_files = format!(
        "Les autres attendent leur tour, dans l'ordre. Dans un dossier, les fichiers de moins de {} Mo \
         passent en plus, jusqu'à {SMALL_PARALLEL} à la fois.",
        SMALL_FILE / MIB
    );
    // Menus justify what wraps: not this sentence.
    ui.with_layout(Layout::top_down(Align::Min), |ui| ui.add(Label::new(small(p, small_files)).wrap()));
    ui.separator();

    ui.label(RichText::new("Si le fichier existe déjà").color(p.text));
    let mut policy = settings.on_conflict;
    egui::ComboBox::from_id_salt("conflict-policy")
        .width(ui.available_width())
        .selected_text(policy.label())
        .show_ui(ui, |ui| {
            for choice in ConflictPolicy::ALL {
                ui.selectable_value(&mut policy, choice, choice.label());
            }
        });
    if policy != settings.on_conflict {
        actions.push(Action::Transfers(TransferAction::SetPolicy(policy)));
    }
    ui.add(Label::new(small(p, settings.on_conflict.hint())).wrap());
}

/// One speed limit: a number and its unit. Returns the new limit, in bytes
/// per second, when it changed.
fn limit_row(ui: &mut Ui, p: &Palette, key: &str, glyph: &str, label: &str, current: u64) -> Option<u64> {
    let unit_id = ui.id().with(("limit-unit", key));
    let default_unit = if current >= MIB && current.is_multiple_of(MIB) { MIB } else { KIB };
    let mut unit: u64 = ui.data(|data| data.get_temp(unit_id)).unwrap_or(default_unit);
    let mut changed = None;
    ui.horizontal(|ui| {
        ui.label(RichText::new(glyph).color(p.muted));
        ui.label(RichText::new(label).color(p.text));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            for (size, name) in [(MIB, "Mo/s"), (KIB, "Ko/s")] {
                if ui.add(Button::selectable(unit == size, name)).clicked() && unit != size {
                    unit = size;
                    // The same figure in the new unit, as typed.
                    let figure = current.div_ceil(if size == MIB { KIB } else { MIB });
                    changed = Some(if current == 0 { 0 } else { figure * size });
                }
            }
            ui.add_space(6.0);
            let mut figure = current.div_ceil(unit);
            // Menus draw their widgets without a frame: this one is a field.
            let field = egui::Frame::new()
                .fill(p.surface)
                .stroke(egui::Stroke::new(1.0, p.border))
                .corner_radius(6)
                .inner_margin(egui::Margin::symmetric(4, 0))
                .show(ui, |ui| {
                    ui.add_sized(
                        vec2(64.0, 24.0),
                        DragValue::new(&mut figure)
                            .range(0..=999_999)
                            .speed(if unit == MIB { 0.2 } else { 10.0 })
                            .custom_formatter(|value, _| {
                                if value == 0.0 { "illimité".to_owned() } else { format!("{value}") }
                            })
                            .custom_parser(|text| match text.trim() {
                                "" | "illimité" => Some(0.0),
                                text => text.replace(',', ".").parse().ok(),
                            }),
                    )
                    .on_hover_text("Glisser ou cliquer pour saisir ; 0 pour aucune limite")
                })
                .inner;
            if field.changed() {
                changed = Some(figure * unit);
            }
        });
    });
    ui.data_mut(|data| data.insert_temp(unit_id, unit));
    changed
}

fn row(
    ui: &mut Ui,
    p: &Palette,
    transfer: &Transfer,
    state: &TransferState,
    meter: Option<&Meter>,
    place: Option<usize>,
) {
    let folder = transfer.folder.as_ref().map(|folder| folder.summary());
    let (done, total) = match &folder {
        Some(folder) => (folder.bytes_done, folder.bytes),
        None => transfer.progress.snapshot(),
    };
    let fraction = match state {
        TransferState::Done => 1.0,
        _ if total == 0 => 0.0,
        _ => (done as f64 / total as f64).min(1.0) as f32,
    };
    // A folder may be running some files while one of them is asked about.
    let asking = !state.is_finished() && *state != TransferState::Paused && transfer.asking();

    ui.allocate_ui_with_layout(
        vec2(ui.available_width(), ROW_HEIGHT),
        Layout::left_to_right(Align::Center),
        |ui| {
            let (glyph, tint) = match transfer.direction {
                Direction::Download => (icon::ARROW_DOWN, p.accent),
                Direction::Upload => (icon::ARROW_UP, p.success),
            };
            ui.add_space(4.0);
            ui.label(RichText::new(glyph).size(17.0).color(tint));

            let name_width = (ui.available_width() - STATUS_WIDTH).max(120.0);
            ui.allocate_ui_with_layout(
                vec2(name_width, ROW_HEIGHT - 8.0),
                Layout::top_down(Align::Min),
                |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.set_width(name_width);
                    let name = match &folder {
                        Some(_) => format!("{}  {}", icon::FOLDER_SIMPLE, transfer.name),
                        None => transfer.name.clone(),
                    };
                    ui.add(Label::new(RichText::new(name).color(p.text)).truncate());
                    let detail = match state {
                        TransferState::Failed(error) => RichText::new(error).color(p.danger),
                        _ => RichText::new(transfer.note().unwrap_or_else(|| transfer.target.clone())).color(p.muted),
                    };
                    ui.add(Label::new(detail.size(11.5)).truncate());
                },
            );

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                buttons(ui, p, transfer, state);
                ui.add_space(6.0);

                let figures = match (state, &folder) {
                    _ if asking => RichText::new("Votre choix est attendu").color(p.accent),
                    (TransferState::Queued, _) => {
                        let text = match place {
                            Some(place) => format!("En attente · {}", ordinal(place)),
                            None => "En attente".to_owned(),
                        };
                        RichText::new(text).color(p.muted)
                    }
                    (TransferState::Paused, Some(folder)) => {
                        RichText::new(format!("En pause · {} / {} fichiers", folder.finished, folder.files))
                            .color(p.folder)
                    }
                    (TransferState::Paused, None) => RichText::new(format!(
                        "En pause · {} / {}",
                        format::size(done),
                        format::size(total)
                    ))
                    .color(p.folder),
                    (TransferState::Running, Some(folder)) if folder.files == 0 => {
                        RichText::new("Recherche des fichiers…").color(p.muted)
                    }
                    (TransferState::Running, Some(folder)) => {
                        let mut text = format!("{} / {} fichiers", folder.finished, folder.files);
                        if folder.exploring {
                            text.push_str(" · recherche…");
                        } else {
                            text.push_str(&format!(" · {} / {}", format::size(done), format::size(total)));
                        }
                        if let Some(meter) = meter.filter(|meter| meter.bytes_per_second > 0.0) {
                            text.push_str(&format!(" · {}", format::speed(meter.bytes_per_second)));
                        }
                        RichText::new(text).color(p.muted)
                    }
                    (TransferState::Running, None) => {
                        let mut text = format!("{} / {}", format::size(done), format::size(total));
                        if let Some(meter) = meter.filter(|meter| meter.bytes_per_second > 0.0) {
                            text.push_str(&format!(" · {}", format::speed(meter.bytes_per_second)));
                            let remaining = total.saturating_sub(done) as f64;
                            text.push_str(&format!(
                                " · {}",
                                format::duration(remaining / meter.bytes_per_second)
                            ));
                        }
                        RichText::new(text).color(p.muted)
                    }
                    // Everything was there already.
                    (TransferState::Done, Some(folder)) if folder.arrived == 0 && folder.skipped > 0 => {
                        RichText::new(format::count(folder.skipped, "fichier ignoré", "fichiers ignorés")).color(p.muted)
                    }
                    (TransferState::Done, Some(folder)) => {
                        let mut text = format!(
                            "{} · {}",
                            format::count(folder.arrived, "fichier", "fichiers"),
                            format::size(total)
                        );
                        if folder.skipped > 0 {
                            text.push_str(&format!(" · {}", format::count(folder.skipped, "ignoré", "ignorés")));
                        }
                        RichText::new(text).color(p.muted)
                    }
                    (TransferState::Failed(_), Some(folder)) if folder.arrived > 0 => {
                        RichText::new(format!("Incomplet · {} / {} fichiers", folder.arrived, folder.files))
                            .color(p.muted)
                    }
                    (TransferState::Done, None) => RichText::new(format::size(total.max(done))).color(p.muted),
                    (TransferState::Failed(_), _) => RichText::new("Échec").color(p.muted),
                    (TransferState::Cancelled, _) => RichText::new("Annulé").color(p.muted),
                    (TransferState::Skipped, _) => RichText::new("Ignoré").color(p.muted),
                };
                ui.allocate_ui_with_layout(
                    vec2(250.0, 20.0),
                    Layout::right_to_left(Align::Center),
                    |ui| {
                        ui.set_width(250.0);
                        ui.add(Label::new(figures.size(12.0)).truncate());
                    },
                );

                let fill = match state {
                    TransferState::Failed(_) => p.danger,
                    TransferState::Cancelled | TransferState::Skipped => p.faint,
                    TransferState::Paused => p.folder,
                    TransferState::Done => p.success,
                    _ => p.accent,
                };
                let searching = *state == TransferState::Running
                    && folder.as_ref().is_some_and(|folder| folder.exploring);
                ui.add(
                    ProgressBar::new(fraction)
                        .desired_width(150.0)
                        .desired_height(6.0)
                        .fill(fill)
                        .animate(searching),
                );
            });
        },
    );
}

/// What can be done with a transfer, right-aligned: the last one drawn
/// first.
fn buttons(ui: &mut Ui, p: &Palette, transfer: &Transfer, state: &TransferState) {
    let status = |ui: &mut Ui, glyph: &str, tint| {
        ui.add_space(6.0);
        ui.label(RichText::new(glyph).size(17.0).color(tint));
        ui.add_space(7.0);
    };
    match state {
        TransferState::Queued | TransferState::Running => {
            if icon_button(ui, p, icon::X, "Annuler", true).clicked() {
                transfer.cancel();
            }
            if transfer.can_pause_now() {
                let hint = if transfer.resumable() {
                    "Suspendre"
                } else {
                    "Suspendre (reprendra depuis le début : ce stockage ne sait pas continuer un envoi)"
                };
                if icon_button(ui, p, icon::PAUSE, hint, true).clicked() {
                    transfer.pause();
                }
            }
        }
        TransferState::Paused => {
            if icon_button(ui, p, icon::X, "Annuler", true).clicked() {
                transfer.cancel();
            }
            if icon_button(ui, p, icon::PLAY, "Reprendre", true).clicked() {
                transfer.resume();
            }
        }
        TransferState::Failed(_) if transfer.can_pause() => {
            status(ui, icon::WARNING_CIRCLE, p.danger);
            let hint = if transfer.resumable() {
                "Réessayer, à partir de ce qui est déjà passé"
            } else {
                "Réessayer"
            };
            if icon_button(ui, p, icon::ARROW_CLOCKWISE, hint, true).clicked() {
                transfer.resume();
            }
        }
        TransferState::Done => status(ui, icon::CHECK_CIRCLE, p.success),
        TransferState::Failed(_) => status(ui, icon::WARNING_CIRCLE, p.danger),
        TransferState::Cancelled => status(ui, icon::PROHIBIT, p.faint),
        TransferState::Skipped => status(ui, icon::ARROW_BEND_UP_RIGHT, p.faint),
    }
}

/// The entry of the file menu that synchronises a folder.
pub fn folder_menu(ui: &mut Ui, session: &Session, entry: &Entry, many: bool, actions: &mut Vec<Action>) {
    if many || !entry.is_dir() || !session.writable() {
        return;
    }
    if ui
        .button(format!("{}  Synchroniser…", icon::ARROWS_LEFT_RIGHT))
        .on_hover_text("Comparer avec un dossier de cet ordinateur et copier ce qui diffère")
        .clicked()
    {
        actions.push(Action::Transfers(TransferAction::AskSync(entry.name.clone())));
        ui.close();
    }
}

/// The button of the action bar that synchronises the folder shown.
pub fn sync_button(ui: &mut Ui, p: &Palette, session: &Session, actions: &mut Vec<Action>) {
    let can = session.writable() && !session.path.is_root();
    if icon_button(ui, p, icon::ARROWS_LEFT_RIGHT, "Synchroniser ce dossier avec un dossier local…", can).clicked() {
        actions.push(Action::Transfers(TransferAction::AskSync(String::new())));
    }
}

#[cfg(test)]
mod tests;
