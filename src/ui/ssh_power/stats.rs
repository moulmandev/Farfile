//! The overview of a server, drawn in the bar above its terminal: the main
//! figures side by side, as many as fit, and all of them on hover.

use egui::{Align2, Color32, FontId, Grid, Id, Rect, RichText, Sense, Ui, pos2, vec2};

use crate::format;
use crate::ssh_power::stats::{Disk, Status, View, decimal, percent, uptime_text};
use crate::theme::{Palette, semibold};
use crate::ui::widgets::icon;

const GAP: f32 = 16.0;
const BAR_WIDTH: f32 = 34.0;

/// The colour of a gauge: calm, then warning, then alarm.
fn level(p: &Palette, fraction: f32) -> Color32 {
    if fraction >= 0.9 {
        p.danger
    } else if fraction >= 0.75 {
        p.folder
    } else {
        p.success
    }
}

/// One figure of the bar.
struct Figure {
    label: String,
    value: String,
    gauge: Option<f32>,
}

fn figure(label: impl Into<String>, value: impl Into<String>, gauge: Option<f32>) -> Figure {
    Figure {
        label: label.into(),
        value: value.into(),
        gauge,
    }
}

/// The fullest of the disks, which is the one to watch; the root one, which
/// comes first, when they are as full.
fn fullest(disks: &[Disk]) -> Option<&Disk> {
    disks
        .iter()
        .reduce(|best, disk| if disk.fraction() > best.fraction() + 0.005 { disk } else { best })
}

/// `7,5 / 30,9 Go`: the unit once when both sizes share it.
fn size_pair(used: u64, total: u64) -> String {
    let (used, total) = (format::size(used), format::size(total));
    match (used.rsplit_once(' '), total.rsplit_once(' ')) {
        (Some((number, unit)), Some((_, same))) if unit == same => format!("{number} / {total}"),
        _ => format!("{used} / {total}"),
    }
}

/// `↓ 12 Ko/s  ↑ 3 Ko/s`.
fn rates(received: f64, sent: f64) -> String {
    let (down, up) = (format::speed(received), format::speed(sent));
    format!("{} {down}  {} {up}", icon::ARROW_DOWN, icon::ARROW_UP)
}

/// Below this width, sizes give way to shares so that more figures fit.
const COMPACT_WIDTH: f32 = 520.0;

/// The figures, most useful first; `compact` keeps them short.
fn figures(view: &View, compact: bool) -> Vec<Figure> {
    let Some(sample) = &view.sample else {
        return Vec::new();
    };
    let mut figures = Vec::new();
    figures.push(match view.cpu {
        Some(cpu) => figure("CPU", percent(cpu), Some(cpu)),
        None => figure("CPU", "…", None),
    });
    if let Some(memory) = sample.memory.filter(|memory| memory.total > 0) {
        let used = memory.used() as f32 / memory.total as f32;
        let value = if compact { percent(used) } else { size_pair(memory.used(), memory.total) };
        figures.push(figure("Mém.", value, Some(used)));
    }
    if let Some(disk) = fullest(&sample.disks) {
        figures.push(figure(format!("Disque {}", disk.mount), percent(disk.fraction()), Some(disk.fraction())));
    }
    if let Some([one, five, fifteen]) = sample.load {
        let value = format!("{} {} {}", decimal(one, 2), decimal(five, 2), decimal(fifteen, 2));
        figures.push(figure("Charge", value, None));
    }
    if let Some((received, sent)) = view.network {
        figures.push(figure("Réseau", rates(received, sent), None));
    }
    if let Some(memory) = sample.memory.filter(|memory| memory.swap_used() > 0) {
        let used = memory.swap_used() as f32 / memory.swap_total.max(1) as f32;
        figures.push(figure("Swap", format::size(memory.swap_used()), Some(used)));
    }
    if let Some(uptime) = sample.uptime {
        figures.push(figure("Allumé depuis", uptime_text(uptime), None));
    }
    figures
}

/// Draws the overview in `rect`, with the details on hover.
pub fn strip(ui: &mut Ui, p: &Palette, rect: Rect, view: &View, id: Id) {
    let painter = ui.painter().with_clip_rect(rect);
    let y = rect.center().y;
    // One line, cut with an ellipsis: the whole of it is in the tooltip.
    let line = |glyph: &str, text: &str, color: Color32| {
        painter.text(pos2(rect.left() + 8.0, y), Align2::CENTER_CENTER, glyph, FontId::proportional(14.0), color);
        let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), FontId::proportional(12.5), color);
        job.wrap = egui::text::TextWrapping::truncate_at_width((rect.width() - 22.0).max(0.0));
        let galley = painter.layout_job(job);
        painter.galley(pos2(rect.left() + 22.0, y - galley.size().y / 2.0), galley, color);
    };
    match &view.status {
        Status::Connecting => line(icon::HOURGLASS, "Connexion pour l'aperçu du serveur…", p.muted),
        Status::Failed(error) => line(icon::WARNING_CIRCLE, &format!("Aperçu indisponible : {error}"), p.danger),
        Status::Unsupported(system) => {
            let text = match system {
                Some(system) => format!("Aperçu non disponible sur ce système ({system})"),
                None => "Aperçu non disponible sur ce système".to_owned(),
            };
            line(icon::INFO, &text, p.muted);
        }
        Status::Ready => {
            let mut x = rect.left();
            for figure in figures(view, rect.width() < COMPACT_WIDTH) {
                let label = painter.layout_no_wrap(figure.label, FontId::proportional(11.5), p.muted);
                let value = painter.layout_no_wrap(figure.value, semibold(12.5), p.text);
                let gauge = if figure.gauge.is_some() { BAR_WIDTH + 6.0 } else { 0.0 };
                let width = label.size().x + 5.0 + value.size().x + gauge;
                if x + width > rect.right() {
                    break;
                }
                painter.galley(pos2(x, y - label.size().y / 2.0), label.clone(), p.muted);
                x += label.size().x + 5.0;
                painter.galley(pos2(x, y - value.size().y / 2.0), value.clone(), p.text);
                x += value.size().x;
                if let Some(fraction) = figure.gauge {
                    let track = Rect::from_min_size(pos2(x + 6.0, y - 2.0), vec2(BAR_WIDTH, 4.0));
                    painter.rect_filled(track, 2, p.surface_hover);
                    let fill = Rect::from_min_size(track.min, vec2(BAR_WIDTH * fraction.clamp(0.0, 1.0), 4.0));
                    painter.rect_filled(fill, 2, level(p, fraction));
                    x += gauge;
                }
                x += GAP;
            }
        }
    }
    ui.interact(rect, id, Sense::hover())
        .on_hover_ui(|ui| details(ui, p, view));
}

/// Everything known about the server, for the tooltip.
fn details(ui: &mut Ui, p: &Palette, view: &View) {
    ui.set_min_width(300.0);
    let Some(sample) = &view.sample else {
        if let Status::Failed(error) = &view.status {
            ui.label(RichText::new(error).color(p.danger));
        } else {
            ui.label(RichText::new("En attente d'une première mesure…").color(p.muted));
        }
        return;
    };
    let title = match (&sample.host, &sample.system) {
        (Some(host), Some(system)) => format!("{host}  ·  {system}"),
        (Some(name), None) | (None, Some(name)) => name.clone(),
        (None, None) => "Serveur".to_owned(),
    };
    ui.label(RichText::new(title).font(semibold(13.5)).color(p.text));
    if let Status::Failed(error) = &view.status {
        ui.label(RichText::new(format!("Dernière mesure : {error}")).size(12.0).color(p.danger));
    }
    ui.add_space(4.0);

    let key = |ui: &mut Ui, text: &str| ui.label(RichText::new(text).size(12.0).color(p.muted));
    let value = |ui: &mut Ui, text: String| ui.label(RichText::new(text).size(12.0).color(p.text));
    let grid = Grid::new("server-overview").num_columns(2).spacing(vec2(14.0, 4.0)).min_row_height(16.0);
    grid.show(ui, |ui| {
        let mut processor = view.cpu.map(percent).unwrap_or_else(|| "…".to_owned());
        if let Some(cores) = sample.cores {
            processor.push_str(&format!(" · {}", format::count(cores as usize, "cœur", "cœurs")));
        }
        key(ui, "Processeur");
        value(ui, processor);
        ui.end_row();
        if let Some([one, five, fifteen]) = sample.load {
            key(ui, "Charge (1, 5, 15 min)");
            value(ui, format!("{}   {}   {}", decimal(one, 2), decimal(five, 2), decimal(fifteen, 2)));
            ui.end_row();
        }
        if let Some(memory) = sample.memory {
            key(ui, "Mémoire");
            let share = if memory.total > 0 { memory.used() as f32 / memory.total as f32 } else { 0.0 };
            value(ui, format!("{}  ({})", size_pair(memory.used(), memory.total), percent(share)));
            ui.end_row();
            key(ui, "Swap");
            value(
                ui,
                if memory.swap_total == 0 {
                    "aucun".to_owned()
                } else {
                    size_pair(memory.swap_used(), memory.swap_total)
                },
            );
            ui.end_row();
        }
        if let Some((received, sent)) = view.network {
            key(ui, "Réseau");
            value(ui, rates(received, sent));
            ui.end_row();
        }
        if let Some(uptime) = sample.uptime {
            key(ui, "Allumé depuis");
            value(ui, uptime_text(uptime));
            ui.end_row();
        }
        for (index, disk) in sample.disks.iter().enumerate() {
            key(ui, if index == 0 { "Disques" } else { "" });
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let (rect, _) = ui.allocate_exact_size(vec2(BAR_WIDTH, 12.0), Sense::hover());
                let track = Rect::from_center_size(rect.center(), vec2(BAR_WIDTH, 4.0));
                ui.painter().rect_filled(track, 2, p.surface_hover);
                let fill = Rect::from_min_size(track.min, vec2(BAR_WIDTH * disk.fraction(), 4.0));
                ui.painter().rect_filled(fill, 2, level(p, disk.fraction()));
                value(
                    ui,
                    format!(
                        "{}  {}  ·  {} libres sur {}",
                        disk.mount,
                        percent(disk.fraction()),
                        format::size(disk.available),
                        format::size(disk.size)
                    ),
                );
            });
            ui.end_row();
        }
    });
    ui.add_space(4.0);
    ui.label(
        RichText::new(format!(
            "Mesuré toutes les {} s sur une connexion à part, sans rien modifier.",
            crate::ssh_power::stats::INTERVAL.as_secs()
        ))
        .size(11.0)
        .color(p.faint),
    );
}
