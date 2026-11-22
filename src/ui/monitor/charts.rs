//! What the views of the dashboard draw with: gauges, rings, small curves,
//! time charts with their axes and crosshair, and the colours of series and
//! of the heat map.
//!
//! Colours follow the rules of the charts elsewhere: one series colour per
//! server, in a fixed order checked for colour blindness on both themes; a
//! single hue from dark to light for magnitudes; and the colours of states
//! (green, amber, red) only for states, always with a word or an icon.

use egui::{Align2, Color32, FontId, Id, Pos2, Rect, Response, RichText, Sense, Shape, Stroke, Ui, pos2, vec2};

use crate::monitor::alerts::{Level, Threshold};
use crate::monitor::history::Range;
use crate::theme::Palette;

/// The series, in order; the dark steps are lighter so that each keeps its
/// contrast on a dark background.
const SERIES_DARK: [Color32; 8] = [
    Color32::from_rgb(0x39, 0x87, 0xe5),
    Color32::from_rgb(0xd9, 0x59, 0x26),
    Color32::from_rgb(0x19, 0x9e, 0x70),
    Color32::from_rgb(0xc9, 0x85, 0x00),
    Color32::from_rgb(0xd5, 0x51, 0x81),
    Color32::from_rgb(0x0c, 0xa3, 0x0c),
    Color32::from_rgb(0x90, 0x85, 0xe9),
    Color32::from_rgb(0xe6, 0x67, 0x67),
];
const SERIES_LIGHT: [Color32; 8] = [
    Color32::from_rgb(0x2a, 0x78, 0xd6),
    Color32::from_rgb(0xeb, 0x68, 0x34),
    Color32::from_rgb(0x1b, 0xaf, 0x7a),
    Color32::from_rgb(0xed, 0xa1, 0x00),
    Color32::from_rgb(0xe8, 0x7b, 0xa4),
    Color32::from_rgb(0x00, 0x83, 0x00),
    Color32::from_rgb(0x62, 0x50, 0xd6),
    Color32::from_rgb(0xe3, 0x49, 0x48),
];
/// How many servers one chart can tell apart.
pub const SERIES: usize = SERIES_DARK.len();

/// The colour of the series in this place.
pub fn series(p: &Palette, index: usize) -> Color32 {
    let list = if p.dark { &SERIES_DARK } else { &SERIES_LIGHT };
    list[index % SERIES]
}

/// One hue, from little (close to the background) to a lot.
const HEAT_DARK: [Color32; 7] = [
    Color32::from_rgb(0x1a, 0x24, 0x38),
    Color32::from_rgb(0x10, 0x42, 0x81),
    Color32::from_rgb(0x1c, 0x5c, 0xab),
    Color32::from_rgb(0x2a, 0x78, 0xd6),
    Color32::from_rgb(0x55, 0x98, 0xe7),
    Color32::from_rgb(0x86, 0xb6, 0xef),
    Color32::from_rgb(0xcd, 0xe2, 0xfb),
];
const HEAT_LIGHT: [Color32; 7] = [
    Color32::from_rgb(0xe8, 0xf0, 0xfb),
    Color32::from_rgb(0xb7, 0xd3, 0xf6),
    Color32::from_rgb(0x86, 0xb6, 0xef),
    Color32::from_rgb(0x55, 0x98, 0xe7),
    Color32::from_rgb(0x2a, 0x78, 0xd6),
    Color32::from_rgb(0x1c, 0x5c, 0xab),
    Color32::from_rgb(0x10, 0x42, 0x81),
];

/// The colour of `fraction` (0 to 1) on the heat map.
pub fn heat(p: &Palette, fraction: f32) -> Color32 {
    let ramp = if p.dark { &HEAT_DARK } else { &HEAT_LIGHT };
    let position = fraction.clamp(0.0, 1.0) * (ramp.len() - 1) as f32;
    let index = (position.floor() as usize).min(ramp.len() - 2);
    let t = position - index as f32;
    let (a, b) = (ramp[index], ramp[index + 1]);
    let mix = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    Color32::from_rgb(mix(a.r(), b.r()), mix(a.g(), b.g()), mix(a.b(), b.b()))
}

/// The colour of a state.
pub fn level(p: &Palette, level: Option<Level>) -> Color32 {
    match level {
        Some(Level::Critical) => p.danger,
        Some(Level::Warning) => p.folder,
        None => p.success,
    }
}

/// The colour of a share (0 to 1) on a gauge: that of the state its alert
/// would be in, or the usual steps when it has none.
pub fn tint(p: &Palette, fraction: f32, threshold: Option<&Threshold>) -> Color32 {
    let value = fraction * 100.0;
    match threshold.filter(|threshold| threshold.enabled) {
        Some(threshold) if value >= threshold.limit(Level::Critical) => p.danger,
        Some(threshold) if value >= threshold.warning => p.folder,
        Some(_) => p.success,
        None => super::super::ssh_power::stats::level(p, fraction),
    }
}

/// How values map to heights: shares and loads from 0, rates on a
/// logarithmic scale from 100 bytes a second.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scale {
    pub max: f32,
    pub log: bool,
}

const LOG_FLOOR: f32 = 100.0;

impl Scale {
    pub fn fraction(&self, value: f32) -> f32 {
        if self.log {
            let (lo, hi) = (LOG_FLOOR.log10(), self.max.max(LOG_FLOOR * 10.0).log10());
            ((value.max(LOG_FLOOR).log10() - lo) / (hi - lo)).clamp(0.0, 1.0)
        } else if self.max > 0.0 {
            (value / self.max).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }

    /// Where the grid lines go.
    fn ticks(&self) -> Vec<f32> {
        if self.log {
            let top = self.max.max(LOG_FLOOR * 10.0).log10().floor() as i32;
            (2..=top).step_by(2.max(((top - 2) / 3) as usize)).map(|power| 10f32.powi(power)).collect()
        } else {
            (0..=4).map(|step| self.max * step as f32 / 4.0).collect()
        }
    }
}

/// A gauge in `track`: the share filled in the colour of its state, and a
/// tick where the warning starts.
pub fn gauge(ui: &Ui, p: &Palette, track: Rect, fraction: Option<f32>, threshold: Option<&Threshold>) {
    let painter = ui.painter();
    let radius = track.height() / 2.0;
    painter.rect_filled(track, radius, if p.dark { p.bg } else { p.surface_hover });
    if let Some(fraction) = fraction {
        let shown = fraction.clamp(0.0, 1.0);
        if shown > 0.0 {
            let fill = Rect::from_min_size(track.min, vec2((track.width() * shown).max(track.height()), track.height()));
            painter.rect_filled(fill, radius, tint(p, fraction, threshold));
        }
    }
    if let Some(threshold) = threshold.filter(|threshold| threshold.enabled && threshold.warning < 100.0) {
        let x = track.left() + track.width() * (threshold.warning / 100.0).clamp(0.0, 1.0);
        painter.line_segment([pos2(x, track.top() - 2.0), pos2(x, track.bottom() + 2.0)], Stroke::new(1.0, p.faint));
    }
}

/// A ring around `center`, filled as far as `fraction`, with `text` inside.
pub fn ring(ui: &Ui, p: &Palette, center: Pos2, radius: f32, fraction: Option<f32>, threshold: Option<&Threshold>, text: &str) {
    let painter = ui.painter();
    let width = (radius * 0.24).max(4.0);
    let track = if p.dark { p.bg } else { p.surface_hover };
    painter.circle_stroke(center, radius, Stroke::new(width, track));
    if let Some(fraction) = fraction {
        let shown = fraction.clamp(0.0, 1.0);
        if shown > 0.0 {
            let steps = (64.0 * shown).ceil().max(2.0) as usize;
            let points: Vec<Pos2> = (0..=steps)
                .map(|step| {
                    let angle = -std::f32::consts::FRAC_PI_2 + std::f32::consts::TAU * shown * step as f32 / steps as f32;
                    center + vec2(angle.cos(), angle.sin()) * radius
                })
                .collect();
            painter.add(Shape::line(points, Stroke::new(width, tint(p, fraction, threshold))));
        }
    }
    painter.text(center, Align2::CENTER_CENTER, text, crate::theme::semibold(radius * 0.48), p.text);
}

/// A series of a chart: its points (Unix seconds, value) and how it looks.
pub struct Line<'a> {
    pub name: &'a str,
    pub color: Color32,
    pub points: Vec<(i64, f32)>,
    /// Drawn thinner and paler, another one being looked at.
    pub dim: bool,
}

/// The x of a moment, the newest on the right.
fn x_of(rect: Rect, at: i64, now: i64, span: i64) -> f32 {
    rect.right() - rect.width() * ((now - at) as f32 / span as f32).clamp(0.0, 1.0)
}

/// Draws a line, broken where readings are missing.
#[allow(clippy::too_many_arguments)]
fn draw_line(ui: &Ui, rect: Rect, line: &Line<'_>, scale: Scale, now: i64, span: i64, gap: i64, width: f32, fill: bool) {
    let painter = ui.painter().with_clip_rect(rect.expand(2.0));
    let mut segment: Vec<Pos2> = Vec::new();
    let mut previous: Option<i64> = None;
    let color = if line.dim { line.color.gamma_multiply(0.22) } else { line.color };
    let flush = |segment: &mut Vec<Pos2>| {
        if fill && segment.len() >= 2 {
            // One mesh under the whole segment: shapes side by side would
            // show seams where their smoothed edges meet.
            let shade = color.gamma_multiply(0.13);
            let mut mesh = egui::Mesh::default();
            for point in segment.iter() {
                mesh.colored_vertex(*point, shade);
                mesh.colored_vertex(pos2(point.x, rect.bottom()), shade);
            }
            for index in 0..segment.len() as u32 - 1 {
                let (top, bottom) = (index * 2, index * 2 + 1);
                mesh.add_triangle(top, bottom, top + 2);
                mesh.add_triangle(bottom, top + 3, top + 2);
            }
            painter.add(Shape::mesh(mesh));
        }
        match segment.len() {
            0 => {}
            1 => {
                painter.circle_filled(segment[0], width, color);
            }
            _ => {
                painter.add(Shape::line(std::mem::take(segment), Stroke::new(width, color)));
            }
        }
        segment.clear();
    };
    for &(at, value) in &line.points {
        if now - at > span {
            continue;
        }
        if previous.is_some_and(|before| at - before > gap) {
            flush(&mut segment);
        }
        previous = Some(at);
        let y = rect.bottom() - rect.height() * scale.fraction(value);
        segment.push(pos2(x_of(rect, at, now, span), y));
    }
    flush(&mut segment);
}

/// The point of `line` closest to the moment `at`, when one is close.
fn nearest(line: &Line<'_>, at: i64, gap: i64) -> Option<(i64, f32)> {
    line.points
        .iter()
        .min_by_key(|(when, _)| (when - at).abs())
        .copied()
        .filter(|(when, _)| (when - at).abs() <= gap)
}

/// When a moment was, as the range reads it: "il y a 4 min" for the last
/// half hour, the time of day over a day, the date over a week.
pub fn moment(at: i64, now: i64, range: Range) -> String {
    match range {
        Range::Minutes30 => super::ago((now - at) as f64, true),
        Range::Hours6 | Range::Hours24 => local(at, "%H:%M"),
        Range::Days7 => local(at, &format!("{} %H:%M", day_month())),
    }
}

fn day_month() -> &'static str {
    match crate::i18n::current() {
        crate::i18n::Lang::En => "%m-%d",
        crate::i18n::Lang::De => "%d.%m.",
        _ => "%d/%m",
    }
}

fn local(at: i64, pattern: &str) -> String {
    chrono::DateTime::from_timestamp(at, 0)
        .map(|utc| utc.with_timezone(&chrono::Local).format(pattern).to_string())
        .unwrap_or_default()
}

/// The labels under a time axis: four moments, from the oldest to now.
pub fn time_labels(range: Range, now: i64) -> [String; 4] {
    let span = range.seconds();
    let at = |part: i64| now - span + span * part / 3;
    match range {
        Range::Minutes30 => [
            tr!("−{minutes} min", minutes = 30),
            tr!("−{minutes} min", minutes = 20),
            tr!("−{minutes} min", minutes = 10),
            tr!("maintenant").to_owned(),
        ],
        Range::Hours6 | Range::Hours24 => [local(at(0), "%H:%M"), local(at(1), "%H:%M"), local(at(2), "%H:%M"), tr!("maintenant").to_owned()],
        Range::Days7 => [
            local(at(0), day_month()),
            local(at(1), day_month()),
            local(at(2), day_month()),
            tr!("maintenant").to_owned(),
        ],
    }
}

/// A small chart without axes, in `rect`: the lines over the last `span`
/// seconds. Hovering it reads every line at that moment.
#[allow(clippy::too_many_arguments)]
pub fn mini(
    ui: &mut Ui,
    p: &Palette,
    rect: Rect,
    id: Id,
    lines: &[Line<'_>],
    scale: Scale,
    range: Range,
    now: i64,
    value: &dyn Fn(f32) -> String,
) -> Response {
    // Over the last half hour, the curve covers what was read so far, so
    // that it fills its room from the first readings.
    let span = if range == Range::Minutes30 {
        let oldest = lines.iter().filter_map(|line| line.points.first().map(|(at, _)| now - at)).max().unwrap_or(0);
        oldest.clamp(30, range.seconds())
    } else {
        range.seconds()
    };
    ui.painter().line_segment([rect.left_bottom(), rect.right_bottom()], Stroke::new(1.0, p.border));
    for line in lines {
        draw_line(ui, rect, line, scale, now, span, range.gap(), 1.5, lines.len() == 1);
    }
    let response = ui.interact(rect, id, Sense::hover());
    if let Some(pointer) = response.hover_pos() {
        let at = now - ((rect.right() - pointer.x) / rect.width() * span as f32) as i64;
        let found: Vec<(&Line<'_>, (i64, f32))> =
            lines.iter().filter_map(|line| nearest(line, at, range.gap()).map(|point| (line, point))).collect();
        if let Some((_, (when, _))) = found.first() {
            let x = x_of(rect, *when, now, span);
            ui.painter().line_segment([pos2(x, rect.top()), pos2(x, rect.bottom())], Stroke::new(1.0, p.muted));
            for (line, (when, v)) in &found {
                let y = rect.bottom() - rect.height() * scale.fraction(*v);
                ui.painter().circle_filled(pos2(x_of(rect, *when, now, span), y), 3.0, line.color);
            }
            let when = *when;
            return response.on_hover_ui(|ui| {
                ui.label(RichText::new(moment(when, now, range)).size(11.5).color(p.muted));
                for (line, (_, v)) in &found {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("■").color(line.color));
                        ui.label(format!("{}  {}", line.name, value(*v)));
                    });
                }
            });
        }
    } else if lines.iter().all(|line| line.points.len() < 2) && response.hovered() {
        return response.on_hover_text(tr!("La courbe se dessine au fil des mesures."));
    }
    response
}

/// A time chart with its axes in `rect`: grid lines and their values on the
/// left, moments underneath, and a crosshair that reads every line,
/// highest first.
#[allow(clippy::too_many_arguments)]
pub fn time_chart(
    ui: &mut Ui,
    p: &Palette,
    rect: Rect,
    id: Id,
    lines: &[Line<'_>],
    scale: Scale,
    range: Range,
    now: i64,
    value: &dyn Fn(f32) -> String,
    axis: &dyn Fn(f32) -> String,
) -> Response {
    const LEFT: f32 = 50.0;
    const BOTTOM: f32 = 18.0;
    let plot = Rect::from_min_max(pos2(rect.left() + LEFT, rect.top() + 6.0), pos2(rect.right() - 8.0, rect.bottom() - BOTTOM));
    let painter = ui.painter();
    let grid = if p.dark { Color32::from_rgb(0x2a, 0x2e, 0x37) } else { Color32::from_rgb(0xe3, 0xe5, 0xea) };
    for tick in scale.ticks() {
        let y = plot.bottom() - plot.height() * scale.fraction(tick);
        painter.line_segment([pos2(plot.left(), y), pos2(plot.right(), y)], Stroke::new(1.0, grid));
        painter.text(pos2(plot.left() - 6.0, y), Align2::RIGHT_CENTER, axis(tick), FontId::proportional(10.5), p.faint);
    }
    let labels = time_labels(range, now);
    for (index, label) in labels.iter().enumerate() {
        let x = plot.left() + plot.width() * index as f32 / 3.0;
        let align = match index {
            0 => Align2::LEFT_BOTTOM,
            3 => Align2::RIGHT_BOTTOM,
            _ => Align2::CENTER_BOTTOM,
        };
        painter.text(pos2(x, rect.bottom()), align, label, FontId::proportional(10.5), p.faint);
    }
    let span = range.seconds();
    for line in lines.iter().filter(|line| line.dim) {
        draw_line(ui, plot, line, scale, now, span, range.gap(), 1.5, false);
    }
    for line in lines.iter().filter(|line| !line.dim) {
        draw_line(ui, plot, line, scale, now, span, range.gap(), 2.0, lines.len() == 1);
    }

    let response = ui.interact(rect, id, Sense::hover());
    let Some(pointer) = response.hover_pos().filter(|pointer| pointer.x >= plot.left()) else {
        return response;
    };
    let at = now - ((plot.right() - pointer.x) / plot.width() * span as f32) as i64;
    let mut found: Vec<(&Line<'_>, (i64, f32))> = lines
        .iter()
        .filter(|line| !line.dim)
        .filter_map(|line| nearest(line, at, range.gap()).map(|point| (line, point)))
        .collect();
    let Some(&(_, (when, _))) = found.first() else {
        return response;
    };
    found.sort_by(|a, b| b.1.1.total_cmp(&a.1.1));
    let x = x_of(plot, when, now, span);
    painter.line_segment([pos2(x, plot.top()), pos2(x, plot.bottom())], Stroke::new(1.0, p.muted));
    for (line, (when, v)) in &found {
        let y = plot.bottom() - plot.height() * scale.fraction(*v);
        painter.circle_filled(pos2(x_of(plot, *when, now, span), y), 4.0, line.color);
        painter.circle_stroke(pos2(x_of(plot, *when, now, span), y), 4.0, Stroke::new(1.5, p.surface));
    }
    response.on_hover_ui(|ui| {
        ui.label(RichText::new(moment(when, now, range)).size(11.5).color(p.muted));
        egui::Grid::new("chart-tip").num_columns(2).spacing(vec2(12.0, 2.0)).show(ui, |ui| {
            for (line, (_, v)) in &found {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.label(RichText::new("■").color(line.color));
                    ui.label(line.name);
                });
                ui.label(RichText::new(value(*v)).strong());
                ui.end_row();
            }
        });
    })
}

/// A row of choices, the current one lit. True when it changed.
pub fn choice<T: PartialEq + Copy>(ui: &mut Ui, p: &Palette, current: &mut T, options: &[(T, String)]) -> bool {
    let mut changed = false;
    let frame = egui::Frame::new().fill(p.surface).corner_radius(8).inner_margin(2);
    frame.show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.spacing_mut().button_padding = vec2(10.0, 3.0);
            // In a line laid out from the right, the first choice still
            // comes first.
            let from_right = ui.layout().main_dir() == egui::Direction::RightToLeft;
            let ordered: Vec<&(T, String)> =
                if from_right { options.iter().rev().collect() } else { options.iter().collect() };
            for (value, label) in ordered {
                let on = *current == *value;
                let text = RichText::new(label).size(12.5).color(if on { p.on_accent } else { p.muted });
                let mut button = egui::Button::new(text).corner_radius(6).min_size(vec2(0.0, 24.0));
                // A frame even when not chosen: without one, a button loses its padding.
                button = if on { button.fill(p.accent) } else { button.fill(Color32::TRANSPARENT).stroke(Stroke::NONE) };
                if ui.add(button).clicked() && !on {
                    *current = *value;
                    changed = true;
                }
            }
        });
    });
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scales_map_values_to_heights() {
        let shares = Scale { max: 1.0, log: false };
        assert_eq!(shares.fraction(0.5), 0.5);
        assert_eq!(shares.fraction(2.0), 1.0);
        assert_eq!(shares.ticks(), [0.0, 0.25, 0.5, 0.75, 1.0]);
        let rates = Scale { max: 1e8, log: true };
        assert_eq!(rates.fraction(50.0), 0.0, "below 100 B/s is the floor");
        assert!((rates.fraction(1e5) - 0.5).abs() < 1e-6);
        assert_eq!(rates.fraction(1e9), 1.0);
        assert_eq!(rates.ticks(), [1e2, 1e4, 1e6, 1e8]);
    }

    #[test]
    fn colours_keep_their_order_and_the_ramp_its_ends() {
        let dark = Palette::dark();
        assert_eq!(series(&dark, 0), series(&dark, SERIES), "a ninth server takes the first colour again");
        assert_ne!(series(&dark, 0), series(&dark, 1));
        assert_eq!(heat(&dark, 0.0), HEAT_DARK[0]);
        assert_eq!(heat(&dark, 1.0), HEAT_DARK[6]);
        assert_eq!(heat(&dark, 7.0), HEAT_DARK[6]);
    }
}
