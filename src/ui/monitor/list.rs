//! A line per server: sortable by any column, grouped by folder or by
//! state if wanted. Thirty servers fit on a screen.

use std::cmp::Ordering;

use egui::{Align2, CursorIcon, FontId, Id, Rect, RichText, ScrollArea, Sense, Ui, pos2, vec2};

use super::charts::{self, Line};
use super::{Action, DashState, Dashboard, Row, journal, server_menu, waiting};
use crate::monitor::alerts::Level;
use crate::monitor::dashboard::{Column, DashboardPrefs, Grouping, Measure};
use crate::monitor::history::{self, Range};
use crate::ssh_power::stats::{decimal, percent, uptime_text};
use crate::theme::{Palette, semibold};
use crate::ui::widgets::icon;

const ROW: f32 = 32.0;
const HEAD: f32 = 24.0;

fn label(column: Column) -> &'static str {
    match column {
        Column::Name => tr!("Machine"),
        Column::Cpu => tr!("Processeur"),
        Column::Memory => tr!("Mémoire"),
        Column::Disk => tr!("Disque"),
        Column::Load => tr!("Charge/cœur"),
        Column::Network => tr!("Réseau ↓"),
        Column::Uptime => tr!("Allumé depuis"),
    }
}

/// The columns that fit, with their share of the width.
fn columns(width: f32) -> Vec<(Column, f32)> {
    let mut columns = vec![
        (Column::Name, 0.22),
        (Column::Cpu, 0.20),
        (Column::Memory, 0.14),
        (Column::Disk, 0.14),
        (Column::Load, 0.10),
        (Column::Network, 0.11),
        (Column::Uptime, 0.09),
    ];
    if width < 720.0 {
        columns.retain(|(column, _)| *column != Column::Uptime);
    }
    if width < 560.0 {
        columns.retain(|(column, _)| *column != Column::Load);
    }
    let total: f32 = columns.iter().map(|(_, share)| share).sum();
    columns.into_iter().map(|(column, share)| (column, share / total)).collect()
}

/// What a row is sorted by in that column; unknown last.
fn key(row: &Row<'_>, column: Column) -> Option<f64> {
    let snap = &row.snap;
    match column {
        Column::Name => None,
        Column::Cpu => snap.cpu.map(f64::from),
        Column::Memory => snap.memory.map(f64::from),
        Column::Disk => snap.disk.as_ref().map(|(_, fraction)| f64::from(*fraction)),
        Column::Load => snap.load.map(f64::from),
        Column::Network => snap.rx.map(f64::from),
        Column::Uptime => snap.uptime,
    }
}

fn compare(a: &Row<'_>, b: &Row<'_>, column: Column, descending: bool) -> Ordering {
    let order = if column == Column::Name {
        crate::profiles::sort_key(a.name()).cmp(&crate::profiles::sort_key(b.name()))
    } else {
        match (key(a, column), key(b, column)) {
            (Some(x), Some(y)) => x.total_cmp(&y),
            (Some(_), None) => return Ordering::Less,
            (None, Some(_)) => return Ordering::Greater,
            (None, None) => Ordering::Equal,
        }
    };
    if descending { order.reverse() } else { order }
}

pub(super) fn show(
    ui: &mut Ui,
    p: &Palette,
    dashboard: &Dashboard<'_>,
    rows: &mut [Row<'_>],
    prefs: &mut DashboardPrefs,
    state: &mut DashState,
    actions: &mut Vec<Action>,
) {
    let (sort, descending) = (prefs.sort, prefs.descending);
    rows.sort_by(|a, b| compare(a, b, sort, descending).then_with(|| compare(a, b, Column::Name, false)));
    ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        let width = ui.available_width();
        let columns = columns(width);
        head(ui, p, &columns, prefs);
        let groups: Vec<(Option<String>, Vec<usize>)> = match prefs.grouping {
            Grouping::None => vec![(None, (0..rows.len()).collect())],
            Grouping::Folder => {
                let mut folders: Vec<String> = rows.iter().map(|row| row.folder().to_owned()).collect();
                folders.sort_by_key(|folder| crate::profiles::sort_key(folder));
                folders.dedup();
                folders
                    .into_iter()
                    .map(|folder| {
                        let members = (0..rows.len()).filter(|&index| rows[index].folder() == folder).collect();
                        let title = if folder.is_empty() { tr!("Sans dossier").to_owned() } else { folder };
                        (Some(format!("{}  {title}", icon::FOLDER)), members)
                    })
                    .collect()
            }
            Grouping::State => [
                (Some(Level::Critical), format!("{}  {}", icon::WARNING_OCTAGON, tr!("Critique"))),
                (Some(Level::Warning), format!("{}  {}", icon::WARNING, tr!("À surveiller"))),
                (None, format!("{}  {}", icon::CHECK_CIRCLE, tr!("Le reste"))),
            ]
            .into_iter()
            .map(|(level, title)| {
                let members = (0..rows.len())
                    .filter(|&index| match level {
                        Some(Level::Critical) => rows[index].snap.rank() == 4,
                        Some(Level::Warning) => rows[index].snap.rank() == 3,
                        None => rows[index].snap.rank() < 3,
                    })
                    .collect();
                (Some(title), members)
            })
            .collect(),
        };
        let now = history::now();
        for (title, members) in groups {
            if members.is_empty() {
                continue;
            }
            if let Some(title) = title {
                ui.add_space(6.0);
                ui.label(RichText::new(format!("{title}  ·  {}", members.len())).size(12.0).color(p.muted));
                ui.add_space(2.0);
            }
            for index in members {
                line(ui, p, &columns, &rows[index], now, state, actions);
            }
        }
        ui.add_space(16.0);
        journal(ui, p, dashboard, None, state, actions);
    });
}

/// The titles of the columns; a click sorts by one, a second the other way.
fn head(ui: &mut Ui, p: &Palette, columns: &[(Column, f32)], prefs: &mut DashboardPrefs) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), HEAD), Sense::hover());
    let mut x = rect.left();
    for (column, share) in columns {
        let cell = Rect::from_min_size(pos2(x, rect.top()), vec2(rect.width() * share, HEAD));
        x = cell.right();
        let response = ui.interact(cell, Id::new(("monitor-list-head", *column)), Sense::click());
        let current = prefs.sort == *column;
        let mut text = label(*column).to_uppercase();
        if current {
            text.push_str(if prefs.descending { "  \u{2193}" } else { "  \u{2191}" });
        }
        let color = if current || response.hovered() { p.text } else { p.faint };
        ui.painter().text(cell.left_center() + vec2(8.0, 0.0), Align2::LEFT_CENTER, text, FontId::proportional(11.0), color);
        if response.on_hover_cursor(CursorIcon::PointingHand).clicked() {
            if current {
                prefs.descending = !prefs.descending;
            } else {
                prefs.sort = *column;
                prefs.descending = *column != Column::Name;
            }
        }
    }
}

fn line(ui: &mut Ui, p: &Palette, columns: &[(Column, f32)], row: &Row<'_>, now: i64, state: &mut DashState, actions: &mut Vec<Action>) {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), ROW), Sense::click());
    if response.hovered() {
        ui.painter().rect_filled(rect, 6, p.surface);
    }
    ui.painter().line_segment([rect.left_top(), rect.right_top()], egui::Stroke::new(1.0, p.border));
    let waiting = waiting(&row.snap);
    let mut x = rect.left();
    for (column, share) in columns {
        let cell = Rect::from_min_size(pos2(x, rect.top()), vec2(rect.width() * share, ROW)).shrink2(vec2(8.0, 0.0));
        x += rect.width() * share;
        if *column != Column::Name
            && let Some(text) = &waiting
        {
            if *column == Column::Cpu {
                let galley = ui.painter().layout_no_wrap(text.clone(), FontId::proportional(12.0), p.muted);
                ui.painter().galley(pos2(cell.left(), cell.center().y - galley.size().y / 2.0), galley, p.muted);
            }
            continue;
        }
        cell_content(ui, p, *column, cell, row, now);
    }
    if response.clicked() {
        state.detail = Some(row.id().to_owned());
    }
    response.on_hover_cursor(CursorIcon::PointingHand).context_menu(|ui| server_menu(ui, row.profile, state, actions));
}

/// A share: its value, then a gauge in what is left of the cell.
fn share(ui: &Ui, p: &Palette, cell: Rect, fraction: Option<f32>, threshold: Option<&crate::monitor::alerts::Threshold>) {
    let Some(fraction) = fraction else {
        return;
    };
    let value = ui.painter().layout_no_wrap(percent(fraction), semibold(12.0), p.text);
    ui.painter().galley(pos2(cell.left() + 36.0 - value.size().x, cell.center().y - value.size().y / 2.0), value, p.text);
    if cell.width() > 70.0 {
        let track = Rect::from_min_max(pos2(cell.left() + 44.0, cell.center().y - 3.0), pos2(cell.right(), cell.center().y + 3.0));
        charts::gauge(ui, p, track, Some(fraction), threshold);
    }
}

fn cell_content(ui: &mut Ui, p: &Palette, column: Column, cell: Rect, row: &Row<'_>, now: i64) {
    let snap = &row.snap;
    let text = |ui: &Ui, text: String, color| {
        let mut job = egui::text::LayoutJob::simple_singleline(text, FontId::proportional(12.5), color);
        job.wrap = egui::text::TextWrapping::truncate_at_width(cell.width());
        let galley = ui.painter().layout_job(job);
        ui.painter().galley(pos2(cell.left(), cell.center().y - galley.size().y / 2.0), galley, color);
    };
    match column {
        Column::Name => {
            ui.painter().circle_filled(pos2(cell.left() + 4.0, cell.center().y), 4.0, snap.dot(p));
            let mut job = egui::text::LayoutJob::simple_singleline(row.name().to_owned(), semibold(12.5), p.text);
            job.wrap = egui::text::TextWrapping::truncate_at_width(cell.width() - 34.0);
            let galley = ui.painter().layout_job(job);
            let right = cell.left() + 14.0 + galley.size().x;
            ui.painter().galley(pos2(cell.left() + 14.0, cell.center().y - galley.size().y / 2.0), galley, p.text);
            if let Some(level) = snap.worst {
                let glyph = if level == Level::Critical { icon::WARNING_OCTAGON } else { icon::WARNING };
                ui.painter().text(pos2(right + 6.0, cell.center().y), Align2::LEFT_CENTER, glyph, FontId::proportional(13.0), charts::level(p, Some(level)));
            }
        }
        Column::Cpu => {
            let Some(cpu) = snap.cpu else {
                return;
            };
            let value = ui.painter().layout_no_wrap(percent(cpu), semibold(12.0), p.text);
            ui.painter().galley(pos2(cell.left() + 36.0 - value.size().x, cell.center().y - value.size().y / 2.0), value, p.text);
            if cell.width() > 90.0 {
                let area = Rect::from_min_max(pos2(cell.left() + 44.0, cell.top() + 7.0), pos2(cell.right(), cell.bottom() - 7.0));
                let points = row.series(Measure::Cpu, Range::Minutes30, now);
                let lines = [Line { name: tr!("Processeur"), color: p.accent, points, dim: false }];
                let scale = charts::Scale { max: 1.0, log: false };
                charts::mini(ui, p, area, Id::new(("monitor-list-cpu", row.id())), &lines, scale, Range::Minutes30, now, &|v: f32| percent(v));
            }
        }
        Column::Memory => share(ui, p, cell, snap.memory, Some(&row.rules.memory)),
        Column::Disk => {
            share(ui, p, cell, snap.disk.as_ref().map(|(_, fraction)| *fraction), Some(&row.rules.disk));
        }
        Column::Load => text(ui, snap.load.map_or_else(String::new, |load| decimal(f64::from(load), 2)), p.text),
        Column::Network => text(
            ui,
            snap.rx.map_or_else(String::new, |rx| format!("{} {}", icon::ARROW_DOWN, crate::format::speed(f64::from(rx)))),
            p.text,
        ),
        Column::Uptime => text(ui, snap.uptime.map_or_else(String::new, uptime_text), p.muted),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narrow_lists_drop_the_least_useful_columns() {
        let wide: Vec<Column> = columns(1000.0).into_iter().map(|(column, _)| column).collect();
        assert_eq!(wide.len(), 7);
        let narrow: Vec<Column> = columns(500.0).into_iter().map(|(column, _)| column).collect();
        assert_eq!(narrow, [Column::Name, Column::Cpu, Column::Memory, Column::Disk, Column::Network]);
        let total: f32 = columns(500.0).iter().map(|(_, share)| share).sum();
        assert!((total - 1.0).abs() < 1e-5);
    }
}
