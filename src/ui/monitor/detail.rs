//! A server in full: real charts with their axes over the span chosen,
//! every disk with when it will be full at the pace it fills, and its
//! alerts.

use egui::{Align, Button, Frame, Id, Layout, Rect, RichText, ScrollArea, Sense, Stroke, Ui, pos2, vec2};

use super::charts::{self, Line, Scale};
use super::{Action, DashState, Dashboard, MonitorAction, Row, chips, journal, measure_scale, range_choice, trouble};
use crate::monitor::dashboard::{DashboardPrefs, Measure};
use crate::monitor::history::{self, Point};
use crate::ssh_power::stats::{decimal, percent, uptime_text};
use crate::theme::{Palette, semibold};
use crate::ui::widgets::icon;

const CHART: f32 = 170.0;

/// `plein dans ~34 jours`, `plein dans ~6 h`.
pub(super) fn forecast(days: f64) -> String {
    if days < 1.0 {
        let hours = (days * 24.0).round().max(1.0) as u64;
        trn!(hours, "plein dans ~{n} h", "plein dans ~{n} h")
    } else {
        trn!(days.round() as u64, "plein dans ~{n} jour", "plein dans ~{n} jours")
    }
}

pub(super) fn show(
    ui: &mut Ui,
    p: &Palette,
    dashboard: &Dashboard<'_>,
    row: &Row<'_>,
    prefs: &mut DashboardPrefs,
    state: &mut DashState,
    actions: &mut Vec<Action>,
) {
    let snap = &row.snap;
    ui.horizontal(|ui| {
        if ui.add(Button::new(format!("{}  {}", icon::ARROW_LEFT, tr!("Surveillance"))).frame(false)).clicked() {
            state.detail = None;
        }
        ui.add_space(6.0);
        let (dot, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
        ui.painter().circle_filled(dot.center(), 5.0, snap.dot(p));
        ui.label(RichText::new(row.name()).font(semibold(18.0)).color(p.text));
        ui.add_space(6.0);
        if !trouble(ui, p, row, actions) && !snap.alerts.is_empty() {
            chips(ui, p, &snap.alerts, 3);
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| range_choice(ui, p, &mut prefs.range));
    });

    let mut about: Vec<String> = Vec::new();
    if !row.folder().is_empty() {
        about.push(row.folder().to_owned());
    }
    if let Some(system) = &snap.system {
        about.push(system.clone());
    }
    if let Some(cores) = snap.cores {
        about.push(trn!(cores, "{n} cœur", "{n} cœurs"));
    }
    if let Some(memory) = &snap.memory_text {
        about.push(memory.rsplit(" / ").next().unwrap_or(memory).to_owned());
    }
    if let Some(uptime) = snap.uptime {
        about.push(tr!("allumé depuis {time}", time = uptime_text(uptime)));
    }
    about.push(row.profile.summary());
    ui.label(RichText::new(about.join("  ·  ")).size(12.0).color(p.faint));
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        let id = row.id().to_owned();
        if ui.button(format!("{}  {}", icon::PLUGS_CONNECTED, tr!("Ouvrir"))).clicked() {
            actions.push(Action::Connect(id.clone()));
        }
        if ui.button(format!("{}  {}", icon::TERMINAL_WINDOW, tr!("Nouveau terminal"))).clicked() {
            actions.push(Action::OpenTab(id.clone(), Some(crate::profiles::Panes::Terminal)));
        }
        if ui.button(format!("{}  {}", icon::SLIDERS_HORIZONTAL, tr!("Seuils de cette machine…"))).clicked() {
            actions.push(Action::Monitor(MonitorAction::Settings(Some(id))));
        }
    });
    ui.add_space(10.0);

    let range = prefs.range;
    let now = history::now();
    let points: Vec<Point> = row.host.map(|host| host.read(|state| state.history.points(range, now))).unwrap_or_default();
    let series = |measure: Measure| -> Vec<(i64, f32)> {
        points.iter().filter_map(|point| measure.of(point).map(|value| (point.at, value))).collect()
    };
    let sent: Vec<(i64, f32)> = points.iter().filter_map(|point| point.tx.map(|value| (point.at, value))).collect();

    ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        let width = ui.available_width();
        let two = width > 640.0;
        let cell = if two { (width - 10.0) / 2.0 } else { width };
        let percent_axis = |value: f32| percent(value);
        let load_points = series(Measure::Load);
        let load_highest = load_points.iter().map(|(_, value)| *value).fold(0.0, f32::max);
        let received = series(Measure::Network);
        let rate_highest = received.iter().chain(&sent).map(|(_, value)| *value).fold(0.0, f32::max);
        let speed = |value: f32| crate::format::speed(f64::from(value));
        let load_text = |value: f32| decimal(f64::from(value), 2);
        let load_axis = |value: f32| decimal(f64::from(value), 1);
        let panels: [Panel<'_, '_>; 4] = [
            (
                tr!("Processeur"),
                snap.cpu.map_or_else(String::new, percent),
                vec![Line { name: tr!("Processeur"), color: p.accent, points: series(Measure::Cpu), dim: false }],
                Scale { max: 1.0, log: false },
                &percent_axis,
                &percent_axis,
            ),
            (
                tr!("Mémoire"),
                snap.memory_text.clone().unwrap_or_default(),
                vec![Line { name: tr!("Mémoire"), color: p.accent, points: series(Measure::Memory), dim: false }],
                Scale { max: 1.0, log: false },
                &percent_axis,
                &percent_axis,
            ),
            (
                tr!("Réseau"),
                match (snap.rx, snap.tx) {
                    (Some(rx), Some(tx)) => super::super::ssh_power::stats::rates(f64::from(rx), f64::from(tx)),
                    _ => String::new(),
                },
                vec![
                    Line { name: tr!("reçu"), color: charts::series(p, 0), points: received.clone(), dim: false },
                    Line { name: tr!("envoyé"), color: charts::series(p, 1), points: sent.clone(), dim: false },
                ],
                measure_scale(Measure::Network, rate_highest),
                &speed,
                &speed,
            ),
            (
                tr!("Charge par cœur"),
                snap.load.map_or_else(String::new, |load| decimal(f64::from(load), 2)),
                vec![Line { name: tr!("Charge par cœur"), color: p.accent, points: load_points.clone(), dim: false }],
                measure_scale(Measure::Load, load_highest),
                &load_text,
                &load_axis,
            ),
        ];
        let mut panels = panels.into_iter();
        while let Some(first) = panels.next() {
            let second = if two { panels.next() } else { None };
            let (line, _) = ui.allocate_exact_size(vec2(width, CHART + 44.0), Sense::hover());
            for (column, panel) in [Some(first), second].into_iter().flatten().enumerate() {
                let rect = Rect::from_min_size(pos2(line.left() + column as f32 * (cell + 10.0), line.top()), vec2(cell, CHART + 44.0));
                chart_card(ui, p, rect, row, panel, range, now);
            }
            ui.add_space(10.0);
        }

        disks(ui, p, row);
        ui.add_space(12.0);
        journal(ui, p, dashboard, Some(row.id()), state, actions);
    });
}

type Panel<'a, 'b> = (&'a str, String, Vec<Line<'a>>, Scale, &'b dyn Fn(f32) -> String, &'b dyn Fn(f32) -> String);

fn chart_card(ui: &mut Ui, p: &Palette, rect: Rect, row: &Row<'_>, panel: Panel<'_, '_>, range: history::Range, now: i64) {
    let (title, current, lines, scale, value, axis) = panel;
    ui.painter().rect(rect, 9, p.surface, Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
    let inner = rect.shrink2(vec2(12.0, 10.0));
    ui.painter().text(inner.left_top(), egui::Align2::LEFT_TOP, title, egui::FontId::proportional(12.0), p.muted);
    ui.painter().text(inner.right_top(), egui::Align2::RIGHT_TOP, current, semibold(13.0), p.text);
    if lines.len() > 1 {
        // Two lines: their names next to the title, with their colours.
        let mut x = inner.left() + ui.painter().layout_no_wrap(title.to_owned(), egui::FontId::proportional(12.0), p.muted).size().x + 14.0;
        for line in &lines {
            ui.painter().rect_filled(Rect::from_center_size(pos2(x + 5.0, inner.top() + 8.0), vec2(10.0, 3.0)), 1.5, line.color);
            let galley = ui.painter().layout_no_wrap(line.name.to_owned(), egui::FontId::proportional(11.5), p.muted);
            let width = galley.size().x;
            ui.painter().galley(pos2(x + 14.0, inner.top() + 1.0), galley, p.muted);
            x += width + 26.0;
        }
    }
    let area = Rect::from_min_max(pos2(inner.left(), inner.top() + 24.0), inner.max);
    let empty = lines.iter().all(|line| line.points.is_empty());
    charts::time_chart(ui, p, area, Id::new(("monitor-detail", row.id(), title)), &lines, scale, range, now, value, axis);
    if empty {
        ui.painter().text(
            area.center(),
            egui::Align2::CENTER_CENTER,
            tr!("Pas encore de mesures sur cette période"),
            egui::FontId::proportional(12.0),
            p.faint,
        );
    }
}

/// Every disk: how full, what is left, and when it will be full.
fn disks(ui: &mut Ui, p: &Palette, row: &Row<'_>) {
    let frame = Frame::new().fill(p.surface).stroke(Stroke::new(1.0, p.border)).corner_radius(9).inner_margin(12);
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(tr!("Disques")).size(12.0).color(p.muted));
        ui.add_space(4.0);
        if row.snap.disks.is_empty() {
            ui.label(RichText::new("…").color(p.muted));
            return;
        }
        for (mount, fraction, available, size) in &row.snap.disks {
            let days = row.host.and_then(|host| host.read(|state| state.history.days_until_full(mount)));
            let (line, _) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::hover());
            let painter = ui.painter();
            let mut job = egui::text::LayoutJob::simple_singleline(mount.clone(), egui::FontId::proportional(12.5), p.text);
            job.wrap = egui::text::TextWrapping::truncate_at_width(150.0);
            painter.galley(pos2(line.left(), line.center().y - 8.0), painter.layout_job(job), p.text);
            let track_right = (line.left() + 160.0 + (line.width() - 160.0) * 0.4).min(line.right());
            let track = Rect::from_min_max(pos2(line.left() + 160.0, line.center().y - 3.0), pos2(track_right, line.center().y + 3.0));
            charts::gauge(ui, p, track, Some(*fraction), Some(&row.rules.disk));
            let mut text = format!(
                "{}   {}",
                percent(*fraction),
                tr!("{free} libres sur {size}", free = crate::format::size(*available), size = crate::format::size(*size))
            );
            if let Some(days) = days {
                text.push_str(&format!("  ·  {}", forecast(days)));
            }
            let color = if days.is_some_and(|days| days < 14.0) { p.folder } else { p.muted };
            ui.painter().text(pos2(track.right() + 12.0, line.center().y), egui::Align2::LEFT_CENTER, text, egui::FontId::proportional(12.0), color);
        }
        ui.add_space(4.0);
        ui.label(
            RichText::new(tr!(
                "Le moment où un disque sera plein suit sa pente des derniers jours, mesurée tant que Farfile surveille."
            ))
            .size(11.0)
            .color(p.faint),
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forecasts_read_in_days_or_hours() {
        assert_eq!(forecast(34.2), "plein dans ~34 jours");
        assert_eq!(forecast(1.2), "plein dans ~1 jour");
        assert_eq!(forecast(0.25), "plein dans ~6 h");
        assert_eq!(forecast(0.001), "plein dans ~1 h");
    }
}
