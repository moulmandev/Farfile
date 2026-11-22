//! The heat map: a band per server, a cell per slice of time, lighter when
//! busier. Which server works hard, and since when, at a glance, even with
//! many servers.

use egui::{Align2, CursorIcon, FontId, Id, Rect, RichText, ScrollArea, Sense, Stroke, Ui, pos2, vec2};

use super::charts::{self, Scale};
use super::{DashState, Row, measure_label, measure_now, measure_scale, measure_text};
use crate::monitor::dashboard::{DashboardPrefs, Measure};
use crate::monitor::history::{self, Range};
use crate::theme::{Palette, semibold};

const NAME: f32 = 170.0;
const VALUE: f32 = 72.0;
const BAND: f32 = 22.0;

/// How many cells a band is cut into at most: a slice of 30 s over half an
/// hour, 5 min over six hours, 15 min over a day, 2 h over a week.
fn most_cells(range: Range) -> usize {
    match range {
        Range::Minutes30 => 60,
        Range::Hours6 => 72,
        Range::Hours24 => 96,
        Range::Days7 => 84,
    }
}

/// The average of each slice of time; none where nothing was read.
pub(super) fn slices(points: &[(i64, f32)], now: i64, span: i64, cells: usize) -> Vec<Option<f32>> {
    let mut sums = vec![(0.0f64, 0u32); cells];
    let start = now - span;
    for &(at, value) in points {
        if at < start || at > now {
            continue;
        }
        let cell = (((at - start) as f64 / span as f64) * cells as f64) as usize;
        let slot = &mut sums[cell.min(cells - 1)];
        slot.0 += f64::from(value);
        slot.1 += 1;
    }
    sums.into_iter().map(|(sum, count)| (count > 0).then(|| (sum / f64::from(count)) as f32)).collect()
}

pub(super) fn show(ui: &mut Ui, p: &Palette, rows: &[Row<'_>], prefs: &DashboardPrefs, state: &mut DashState) {
    let (measure, range) = (prefs.measure, prefs.range);
    let now = history::now();
    let span = range.seconds();
    ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        let width = ui.available_width();
        let area = (width - NAME - VALUE - 16.0).max(60.0);
        let cells = ((area / 6.0).floor() as usize).clamp(12, most_cells(range));
        let bands: Vec<Vec<Option<f32>>> =
            rows.iter().map(|row| slices(&row.series(measure, range, now), now, span, cells)).collect();
        let highest = bands.iter().flatten().flatten().copied().fold(0.0, f32::max);
        let scale = match measure {
            Measure::Network | Measure::Load => measure_scale(measure, highest),
            _ => Scale { max: 1.0, log: false },
        };

        let mut hovered = None;
        for (row, band) in rows.iter().zip(&bands) {
            let (line, _) = ui.allocate_exact_size(vec2(width, BAND), Sense::hover());
            // The name, which opens the server.
            let name_rect = Rect::from_min_size(line.min, vec2(NAME, BAND));
            let name = ui.interact(name_rect, Id::new(("monitor-heat-name", row.id())), Sense::click());
            ui.painter().circle_filled(pos2(name_rect.left() + 4.0, name_rect.center().y), 4.0, row.snap.dot(p));
            let mut job = egui::text::LayoutJob::simple_singleline(row.name().to_owned(), FontId::proportional(12.5), p.text);
            job.wrap = egui::text::TextWrapping::truncate_at_width(NAME - 24.0);
            let galley = ui.painter().layout_job(job);
            ui.painter().galley(pos2(name_rect.left() + 14.0, name_rect.center().y - galley.size().y / 2.0), galley, p.text);
            if name.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                state.detail = Some(row.id().to_owned());
            }

            let cells_rect = Rect::from_min_max(pos2(line.left() + NAME, line.top()), pos2(line.right() - VALUE - 8.0, line.bottom()));
            let step = cells_rect.width() / cells as f32;
            for (index, value) in band.iter().enumerate() {
                let cell = Rect::from_min_size(pos2(cells_rect.left() + step * index as f32, cells_rect.top()), vec2(step - 1.0, BAND))
                    .shrink2(vec2(0.0, 1.0));
                let color = match value {
                    Some(value) => charts::heat(p, scale.fraction(*value)),
                    None => p.surface,
                };
                ui.painter().rect_filled(cell, 2, color);
                if ui.rect_contains_pointer(cell) {
                    hovered = Some((cell, row, index, *value));
                }
            }
            let now_text = measure_now(measure, &row.snap).map_or_else(String::new, |value| measure_text(measure, value));
            ui.painter().text(line.right_center(), Align2::RIGHT_CENTER, now_text, semibold(12.0), p.text);
            ui.add_space(3.0);
        }

        // Moments, under the cells.
        let (axis, _) = ui.allocate_exact_size(vec2(width, 16.0), Sense::hover());
        let cells_left = axis.left() + NAME;
        let cells_width = axis.width() - NAME - VALUE - 8.0;
        for (index, label) in charts::time_labels(range, now).iter().enumerate() {
            let x = cells_left + cells_width * index as f32 / 3.0;
            let align = match index {
                0 => Align2::LEFT_CENTER,
                3 => Align2::RIGHT_CENTER,
                _ => Align2::CENTER_CENTER,
            };
            ui.painter().text(pos2(x, axis.center().y), align, label, FontId::proportional(10.5), p.faint);
        }

        if let Some((cell, row, index, value)) = hovered {
            ui.painter().rect_stroke(cell.expand(1.0), 2, Stroke::new(1.5, p.text), egui::StrokeKind::Outside);
            let middle = now - span + ((index as f64 + 0.5) / cells as f64 * span as f64) as i64;
            let response = ui.interact(cell, Id::new("monitor-heat-cell"), Sense::hover());
            response.on_hover_ui_at_pointer(|ui| {
                ui.label(RichText::new(row.name()).strong());
                ui.label(RichText::new(charts::moment(middle, now, range)).size(11.5).color(p.muted));
                let text = value.map_or_else(|| tr!("pas de mesure").to_owned(), |value| measure_text(measure, value));
                ui.label(format!("{} : {text}", measure_label(measure)));
            });
        }

        ui.add_space(12.0);
        legend(ui, p, measure, scale);
    });
}

/// The ramp, from little to a lot.
fn legend(ui: &mut Ui, p: &Palette, measure: Measure, scale: Scale) {
    ui.horizontal(|ui| {
        let low = if scale.log { crate::format::speed(100.0) } else { measure_text(measure, 0.0) };
        ui.label(RichText::new(low).size(11.5).color(p.muted));
        let (ramp, _) = ui.allocate_exact_size(vec2(220.0, 10.0), Sense::hover());
        let steps = 32;
        for step in 0..steps {
            let width = ramp.width() / steps as f32;
            let cell = Rect::from_min_size(pos2(ramp.left() + width * step as f32, ramp.top()), vec2(width + 0.5, ramp.height()));
            ui.painter().rect_filled(cell, 0, charts::heat(p, step as f32 / (steps - 1) as f32));
        }
        ui.label(RichText::new(measure_text(measure, scale.max)).size(11.5).color(p.muted));
        ui.add_space(16.0);
        ui.label(RichText::new(tr!("Survolez une case pour sa valeur ; cliquez un nom pour le détail.")).size(11.5).color(p.faint));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readings_fall_into_their_slice_of_time() {
        let now = 1_000_000;
        let points = [(now - 1800, 0.2), (now - 1790, 0.4), (now - 10, 0.9), (now - 5000, 1.0)];
        let cells = slices(&points, now, 1800, 6);
        assert_eq!(cells.len(), 6);
        assert!((cells[0].unwrap() - 0.3).abs() < 1e-6, "averaged");
        assert_eq!(cells[5], Some(0.9));
        assert_eq!(cells[1..5], [None, None, None, None], "nothing read there; older ones left out");
    }
}
