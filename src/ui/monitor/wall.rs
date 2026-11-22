//! The wall: large tiles for a screen of their own, the state in words and
//! the figure that matters in big, the rest small. With the window in full
//! screen, it reads from across a room.

use egui::{Align2, Color32, CursorIcon, FontId, Id, Rect, ScrollArea, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

use super::charts;
use super::{Action, DashState, Row, State, server_menu};
use crate::monitor::alerts::Level;
use crate::monitor::dashboard::{DashboardPrefs, Order};
use crate::ssh_power::stats::percent;
use crate::theme::{Palette, semibold};
use crate::ui::widgets::icon;

const MIN_WIDTH: f32 = 230.0;
const HEIGHT: f32 = 156.0;
const GAP: f32 = 10.0;

/// What a tile says first: the highest share, with its name.
fn headline(row: &Row<'_>) -> Option<(String, f32)> {
    let snap = &row.snap;
    let mut figures: Vec<(String, f32)> = Vec::new();
    if let Some(cpu) = snap.cpu {
        figures.push((tr!("Processeur").to_owned(), cpu));
    }
    if let Some(memory) = snap.memory {
        figures.push((tr!("Mémoire").to_owned(), memory));
    }
    if let Some((mount, fraction)) = &snap.disk {
        figures.push((tr!("Disque {mount}", mount), *fraction));
    }
    figures.into_iter().max_by(|a, b| a.1.total_cmp(&b.1))
}

pub(super) fn show(ui: &mut Ui, p: &Palette, rows: &mut [Row<'_>], prefs: &DashboardPrefs, state: &mut DashState, actions: &mut Vec<Action>) {
    super::sort(rows, Order::Severity);
    let shown: Vec<&Row<'_>> = rows.iter().filter(|row| !prefs.only_alerts || row.snap.rank() >= 2).collect();
    ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        if shown.is_empty() {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new(format!("{}  {}", icon::CHECK_CIRCLE, tr!("Aucune alerte"))).size(20.0).color(p.success));
            });
            return;
        }
        let width = ui.available_width();
        let columns = ((width + GAP) / (MIN_WIDTH + GAP)).floor().max(1.0) as usize;
        let tile_width = (width - GAP * (columns - 1) as f32) / columns as f32;
        for chunk in shown.chunks(columns) {
            let (line, _) = ui.allocate_exact_size(vec2(width, HEIGHT), Sense::hover());
            for (column, row) in chunk.iter().enumerate() {
                let rect = Rect::from_min_size(pos2(line.left() + column as f32 * (tile_width + GAP), line.top()), vec2(tile_width, HEIGHT));
                tile(ui, p, rect, row, state, actions);
            }
            ui.add_space(GAP);
        }
    });
}

fn tile(ui: &mut Ui, p: &Palette, rect: Rect, row: &Row<'_>, state: &mut DashState, actions: &mut Vec<Action>) {
    let snap = &row.snap;
    let level = match &snap.state {
        State::Paused(_) | State::Unreachable(_) => Some(Level::Critical),
        _ => snap.worst,
    };
    let response = ui.interact(rect, Id::new(("monitor-tile", row.id())), Sense::click());
    let (fill, border) = match level {
        Some(level) => {
            let color = charts::level(p, Some(level));
            (blend(p.surface, color, if response.hovered() { 0.2 } else { 0.14 }), color.gamma_multiply(0.75))
        }
        None => (if response.hovered() { p.surface_hover } else { p.surface }, p.border),
    };
    let painter = ui.painter();
    painter.rect(rect, 10, fill, Stroke::new(1.0, border), StrokeKind::Inside);
    let inner = rect.shrink2(vec2(14.0, 12.0));

    // The state, in words and with its icon, never by its colour alone.
    let (glyph, word) = match (&snap.state, level) {
        (State::Paused(_), _) => (icon::PAUSE_CIRCLE, tr!("En pause")),
        (State::Unreachable(_), _) => (icon::WARNING_CIRCLE, tr!("Injoignable")),
        (_, Some(Level::Critical)) => (icon::WARNING_OCTAGON, tr!("Critique")),
        (_, Some(Level::Warning)) => (icon::WARNING, tr!("À surveiller")),
        (State::Ready, None) => (icon::CHECK_CIRCLE, tr!("OK")),
        _ => (icon::HOURGLASS, tr!("Connexion…")),
    };
    let state_color = match (&snap.state, level) {
        (State::Ready, None) => p.success,
        (_, Some(level)) => charts::level(p, Some(level)),
        _ => p.muted,
    };
    let word = painter.layout_no_wrap(format!("{glyph} {}", word.to_uppercase()), semibold(11.0), state_color);
    let word_width = word.size().x;
    painter.galley(pos2(inner.right() - word_width, inner.top() + 2.0), word, state_color);
    let mut job = egui::text::LayoutJob::simple_singleline(row.name().to_owned(), semibold(15.0), p.text);
    job.wrap = egui::text::TextWrapping::truncate_at_width(inner.width() - word_width - 10.0);
    painter.galley(inner.min, painter.layout_job(job), p.text);

    // The figure that matters, in big.
    match headline(row) {
        Some((what, fraction)) => {
            painter.text(pos2(inner.left(), inner.top() + 26.0), Align2::LEFT_TOP, percent(fraction), FontId::proportional(34.0), p.text);
            let what = if level.is_some() { what } else { tr!("le plus haut : {what}", what) };
            painter.text(pos2(inner.left(), inner.top() + 68.0), Align2::LEFT_TOP, what, FontId::proportional(12.0), p.muted);
        }
        None => {
            let text = super::waiting(snap).unwrap_or_default();
            painter.text(pos2(inner.left(), inner.top() + 40.0), Align2::LEFT_TOP, text, FontId::proportional(13.0), p.muted);
        }
    }

    // The three shares, small.
    let figures = [
        (tr!("CPU").to_owned(), snap.cpu, Some(&row.rules.cpu)),
        (tr!("Mém.").to_owned(), snap.memory, Some(&row.rules.memory)),
        (tr!("Disque").to_owned(), snap.disk.as_ref().map(|(_, fraction)| *fraction), Some(&row.rules.disk)),
    ];
    let column = (inner.width() - 16.0) / 3.0;
    for (index, (label, fraction, threshold)) in figures.into_iter().enumerate() {
        let left = inner.left() + index as f32 * (column + 8.0);
        let text = match fraction {
            Some(fraction) => format!("{label} {}", percent(fraction)),
            None => label,
        };
        ui.painter().text(pos2(left, inner.bottom() - 14.0), Align2::LEFT_BOTTOM, text, FontId::proportional(11.0), p.muted);
        let track = Rect::from_min_size(pos2(left, inner.bottom() - 5.0), vec2(column, 4.0));
        charts::gauge(ui, p, track, fraction, threshold);
    }

    if response.clicked() {
        state.detail = Some(row.id().to_owned());
    }
    response.on_hover_cursor(CursorIcon::PointingHand).context_menu(|ui| server_menu(ui, row.profile, state, actions));
}

/// `a` tinted towards `b`.
fn blend(a: Color32, b: Color32, amount: f32) -> Color32 {
    let mix = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * amount).round() as u8;
    Color32::from_rgb(mix(a.r(), b.r()), mix(a.g(), b.g()), mix(a.b(), b.b()))
}
