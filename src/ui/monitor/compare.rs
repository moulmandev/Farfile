//! The servers side by side on the same chart, a colour each: which one
//! works the most, which one started first. Then a small chart per server,
//! on the same scale, to compare them without lines on top of each other.

use egui::{CursorIcon, Frame, Id, Rect, RichText, ScrollArea, Sense, Stroke, Ui, pos2, vec2};

use super::charts::{self, Line, SERIES};
use super::{DashState, Row, measure_axis, measure_label, measure_now, measure_scale, measure_text};
use crate::monitor::dashboard::DashboardPrefs;
use crate::monitor::history;
use crate::theme::{Palette, semibold};

const CHART: f32 = 260.0;
const SMALL: f32 = 210.0;

fn lit_id() -> Id {
    Id::new("monitor-compare-lit")
}

pub(super) fn show(ui: &mut Ui, p: &Palette, rows: &[Row<'_>], prefs: &DashboardPrefs, state: &mut DashState) {
    let (measure, range) = (prefs.measure, prefs.range);
    let now = history::now();
    let series: Vec<Vec<(i64, f32)>> = rows.iter().map(|row| row.series(measure, range, now)).collect();
    let highest = series.iter().flatten().map(|(_, value)| *value).fold(0.0, f32::max);
    let scale = measure_scale(measure, highest);
    let value = |value: f32| measure_text(measure, value);
    let axis = |value: f32| measure_axis(measure, value);
    // The legend is drawn after the chart: it lights a server up from the
    // frame before.
    let lit: Option<String> = ui.ctx().data(|data| data.get_temp(lit_id()));
    ui.ctx().data_mut(|data| data.remove::<String>(lit_id()));

    ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        let charted: Vec<usize> = (0..rows.len()).filter(|&index| rows[index].index < SERIES).collect();
        let lines: Vec<Line<'_>> = charted
            .iter()
            .filter(|&&index| !state.hidden.contains(rows[index].id()))
            .map(|&index| Line {
                name: rows[index].name(),
                color: charts::series(p, rows[index].index),
                points: series[index].clone(),
                dim: lit.as_deref().is_some_and(|lit| lit != rows[index].id()),
            })
            .collect();
        let frame = Frame::new().fill(p.surface).stroke(Stroke::new(1.0, p.border)).corner_radius(9).inner_margin(10);
        frame.show(ui, |ui| {
            ui.label(RichText::new(measure_label(measure)).font(semibold(14.0)).color(p.text));
            let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), CHART), Sense::hover());
            charts::time_chart(ui, p, rect, Id::new("monitor-compare"), &lines, scale, range, now, &value, &axis);
        });

        // The legend: a click hides or shows a server, hovering lights it up.
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
            for &index in &charted {
                let row = &rows[index];
                let hidden = state.hidden.contains(row.id());
                let now_text = measure_now(measure, &row.snap).map_or_else(String::new, |value| measure_text(measure, value));
                let color = charts::series(p, row.index);
                let frame = Frame::new()
                    .fill(p.surface)
                    .stroke(Stroke::new(1.0, p.border))
                    .corner_radius(7)
                    .inner_margin(egui::Margin::symmetric(8, 3))
                    .multiply_with_opacity(if hidden { 0.45 } else { 1.0 });
                let response = frame
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            let (swatch, _) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
                            ui.painter().rect_filled(Rect::from_center_size(swatch.center(), vec2(12.0, 3.0)), 1.5, color);
                            ui.label(RichText::new(row.name()).size(12.0).color(p.text));
                            ui.label(RichText::new(now_text).size(12.0).strong().color(p.text));
                        });
                    })
                    .response;
                let response = ui.interact(response.rect, Id::new(("monitor-legend", row.id())), Sense::click());
                if response.hovered() && !hidden {
                    ui.ctx().data_mut(|data| data.insert_temp(lit_id(), row.id().to_owned()));
                }
                if response.on_hover_cursor(CursorIcon::PointingHand).clicked() && !state.hidden.remove(row.id()) {
                    state.hidden.insert(row.id().to_owned());
                }
            }
        });
        if rows.len() > SERIES {
            ui.label(
                RichText::new(tr!(
                    "Les {count} premières machines sur le graphique ; toutes dans les petits graphiques ci-dessous.",
                    count = SERIES
                ))
                .size(11.5)
                .color(p.faint),
            );
        }

        // A small chart per server, on the same scale.
        ui.add_space(14.0);
        ui.label(RichText::new(tr!("Une machine par graphique, à la même échelle")).font(semibold(13.0)).color(p.text));
        ui.add_space(6.0);
        let width = ui.available_width();
        let columns = ((width + 8.0) / (SMALL + 8.0)).floor().max(1.0) as usize;
        let cell = (width - 8.0 * (columns - 1) as f32) / columns as f32;
        for chunk in (0..rows.len()).collect::<Vec<_>>().chunks(columns) {
            let (line, _) = ui.allocate_exact_size(vec2(width, 74.0), Sense::hover());
            for (column, &index) in chunk.iter().enumerate() {
                let row = &rows[index];
                let rect = Rect::from_min_size(pos2(line.left() + column as f32 * (cell + 8.0), line.top()), vec2(cell, 74.0));
                let response = ui.interact(rect, Id::new(("monitor-small", row.id())), Sense::click());
                let fill = if response.hovered() { p.surface_hover } else { p.surface };
                ui.painter().rect(rect, 8, fill, Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
                let inner = rect.shrink2(vec2(9.0, 7.0));
                let mut job = egui::text::LayoutJob::simple_singleline(row.name().to_owned(), egui::FontId::proportional(12.0), p.text);
                job.wrap = egui::text::TextWrapping::truncate_at_width(inner.width() - 60.0);
                let galley = ui.painter().layout_job(job);
                ui.painter().galley(inner.min, galley, p.text);
                let now_text = measure_now(measure, &row.snap).map_or_else(String::new, |value| measure_text(measure, value));
                ui.painter().text(inner.right_top(), egui::Align2::RIGHT_TOP, now_text, semibold(12.0), p.text);
                let area = Rect::from_min_max(pos2(inner.left(), inner.top() + 22.0), inner.max);
                let lines = [Line {
                    name: row.name(),
                    color: p.accent,
                    points: series[index].clone(),
                    dim: false,
                }];
                charts::mini(ui, p, area, Id::new(("monitor-small-chart", row.id())), &lines, scale, range, now, &value);
                if response.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                    state.detail = Some(row.id().to_owned());
                }
            }
            ui.add_space(8.0);
        }
    });
}
