//! The cards: one per server, made of the widgets chosen in the order
//! chosen, each in its style; and the panel that chooses them.

use egui::{
    Align, Align2, Button, ComboBox, CursorIcon, FontId, Frame, Id, Layout, Rect, RichText, ScrollArea, Sense, Stroke,
    StrokeKind, Ui, UiBuilder, pos2, vec2,
};

use super::charts::{self, Line};
use super::{Action, DashState, Dashboard, Row, State, chips, journal, server_menu, sort, trouble, waiting};
use crate::monitor::dashboard::{DashboardPrefs, Measure, Order, Slot, Style, Widget, presets};
use crate::monitor::history::{self, Point, Range};
use crate::ssh_power::stats::{decimal, percent, uptime_text};
use crate::theme::{Palette, RADIUS, semibold};
use crate::ui::widgets::{checkbox, icon};

/// Narrower than this, a card does not fit its widgets.
const MIN_WIDTH: f32 = 290.0;
const GAP: f32 = 12.0;
const PAD: egui::Vec2 = vec2(14.0, 12.0);
/// The name of a figure, then its value, at the start of their lines.
const LABEL: f32 = 80.0;
const VALUE: f32 = 92.0;
const HEADER: f32 = 22.0 + 16.0 + 8.0;
const RING: f32 = 74.0;
const NUMBER: f32 = 48.0;
/// The widths of the tiles, so that a card knows how many lines they take.
const RING_WIDTH: f32 = 66.0;
const NUMBER_WIDTH: f32 = 92.0;
const TILE_GAP: f32 = 8.0;

/// The tiles cut into lines that fit `width`; the height of each line.
fn tile_lines(tiles: &[Slot], width: f32) -> Vec<(Vec<Slot>, f32)> {
    let mut lines: Vec<(Vec<Slot>, f32)> = Vec::new();
    let mut used = f32::INFINITY;
    for slot in tiles {
        let size = if slot.style == Style::Ring { RING_WIDTH } else { NUMBER_WIDTH };
        if used + TILE_GAP + size > width {
            lines.push((Vec::new(), 0.0));
            used = -TILE_GAP;
        }
        used += TILE_GAP + size;
        if let Some((line, height)) = lines.last_mut() {
            line.push(*slot);
            *height = height.max(if slot.style == Style::Ring { RING } else { NUMBER });
        }
    }
    lines
}

pub(super) fn widget_label(widget: Widget) -> &'static str {
    match widget {
        Widget::Cpu => tr!("Processeur"),
        Widget::Memory => tr!("Mémoire"),
        Widget::Disk => tr!("Disques"),
        Widget::Load => tr!("Charge par cœur"),
        Widget::Network => tr!("Réseau"),
        Widget::Alerts => tr!("Alertes"),
        Widget::Uptime => tr!("Allumé depuis"),
    }
}

fn style_label(widget: Widget, style: Style) -> &'static str {
    match (widget, style) {
        (Widget::Disk, Style::Bar) => tr!("le plus plein"),
        (_, Style::Bar) => tr!("jauge"),
        (_, Style::Curve) => tr!("courbe"),
        (_, Style::Ring) => tr!("anneau"),
        (_, Style::Number) => tr!("chiffre"),
        (Widget::Alerts, Style::Text) => tr!("étiquettes"),
        (_, Style::Text) => tr!("texte"),
        (_, Style::Every) => tr!("tous les disques"),
    }
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
    ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        if state.customizing {
            panel(ui, p, prefs);
            ui.add_space(12.0);
        }
        sort(rows, prefs.order);
        let width = ui.available_width();
        let columns = match prefs.columns {
            0 => ((width + GAP) / (MIN_WIDTH + GAP)).floor().max(1.0) as usize,
            fixed => usize::from(fixed),
        };
        let card_width = ((width - GAP * (columns - 1) as f32) / columns as f32).floor();
        let now = history::now();
        for chunk in rows.chunks(columns) {
            let inner = card_width - PAD.x * 2.0;
            let height = chunk.iter().map(|row| height(&prefs.slots, row, inner)).fold(0.0, f32::max);
            let (line, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
            for (index, row) in chunk.iter().enumerate() {
                let left = line.left() + index as f32 * (card_width + GAP);
                let rect = Rect::from_min_size(pos2(left, line.top()), vec2(card_width, height));
                card(ui, p, rect, row, &prefs.slots, now, state, actions);
            }
            ui.add_space(GAP);
        }
        ui.add_space(8.0);
        journal(ui, p, dashboard, None, state, actions);
    });
}

/// How tall a card is with these widgets, `width` inside it.
fn height(slots: &[Slot], row: &Row<'_>, width: f32) -> f32 {
    let mut height = PAD.y * 2.0 + HEADER;
    if waiting(&row.snap).is_some() {
        return height + 24.0 + 26.0;
    }
    let mut tiles: Vec<Slot> = Vec::new();
    let mut alerts = false;
    let lines_height = |tiles: &[Slot]| -> f32 { tile_lines(tiles, width).iter().map(|(_, line)| line + 8.0).sum() };
    for slot in slots.iter().filter(|slot| slot.on) {
        if slot.style.is_tile() {
            tiles.push(*slot);
            continue;
        }
        height += lines_height(&tiles);
        tiles.clear();
        alerts |= slot.widget == Widget::Alerts;
        height += match (slot.widget, slot.style) {
            (Widget::Network, Style::Curve) => 36.0,
            (_, Style::Curve) => 30.0,
            (Widget::Disk, Style::Every) => 22.0 * row.snap.disks.len().max(1) as f32,
            (Widget::Alerts, _) => 32.0,
            (_, Style::Bar) => 22.0,
            _ => 20.0,
        };
    }
    height += lines_height(&tiles);
    if !alerts && matches!(row.snap.state, State::Paused(_) | State::Unreachable(_)) {
        height += 28.0;
    }
    height
}

fn inner_hover_id() -> Id {
    Id::new("monitor-inner-hover")
}

/// The pointer is over a part of a card that says something of its own:
/// the card keeps its own details to itself.
fn inner_hovered(ui: &Ui) {
    ui.ctx().data_mut(|data| data.insert_temp(inner_hover_id(), true));
}

#[allow(clippy::too_many_arguments)]
fn card(
    ui: &mut Ui,
    p: &Palette,
    rect: Rect,
    row: &Row<'_>,
    slots: &[Slot],
    now: i64,
    state: &mut DashState,
    actions: &mut Vec<Action>,
) {
    let response = ui.interact(rect, Id::new(("monitor-card", row.id())), Sense::click());
    let border = match row.snap.worst {
        Some(level) => charts::level(p, Some(level)).gamma_multiply(0.7),
        None => p.border,
    };
    let fill = if response.hovered() { p.surface_hover } else { p.surface };
    ui.painter().rect(rect, RADIUS, fill, Stroke::new(1.0, border), StrokeKind::Inside);

    let mut content = ui.new_child(UiBuilder::new().max_rect(rect.shrink2(PAD)).layout(Layout::top_down(Align::Min)));
    content.set_clip_rect(rect.intersect(ui.clip_rect()));
    content.spacing_mut().item_spacing = vec2(8.0, 0.0);
    title(&mut content, p, row, state, actions);
    content.add_space(8.0);

    let mut alerts_shown = false;
    if let Some(text) = waiting(&row.snap) {
        content.label(RichText::new(text).size(12.0).color(p.muted));
    } else {
        alerts_shown = widgets(&mut content, p, row, slots, now, actions);
    }
    if !alerts_shown && matches!(row.snap.state, State::Paused(_) | State::Unreachable(_)) {
        let foot = Rect::from_min_max(pos2(content.max_rect().left(), content.max_rect().bottom() - 24.0), content.max_rect().max);
        let mut foot = content.new_child(UiBuilder::new().max_rect(foot).layout(Layout::left_to_right(Align::Center)));
        trouble(&mut foot, p, row, actions);
    }

    let response = response.on_hover_cursor(CursorIcon::PointingHand);
    let inner = ui.ctx().data_mut(|data| data.remove_temp::<bool>(inner_hover_id())).unwrap_or(false);
    if let Some(host) = row.host
        && !inner
    {
        response.clone().on_hover_ui(|ui| host.read(|state| super::super::ssh_power::stats::details(ui, p, &state.view)));
    }
    if response.clicked() {
        state.detail = Some(row.id().to_owned());
    }
    response.context_menu(|ui| server_menu(ui, row.profile, state, actions));
}

/// The dot, the name, its state, the menu; then where it is and what it runs.
fn title(ui: &mut Ui, p: &Palette, row: &Row<'_>, state: &mut DashState, actions: &mut Vec<Action>) {
    let (line, _) = ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::hover());
    ui.painter().circle_filled(pos2(line.left() + 4.0, line.center().y), 4.0, row.snap.dot(p));
    let menu = Rect::from_center_size(pos2(line.right() - 10.0, line.center().y), vec2(22.0, 22.0));
    let name = Rect::from_min_max(pos2(line.left() + 16.0, line.top()), pos2(menu.left() - 6.0, line.bottom()));
    let mut label = ui.new_child(UiBuilder::new().max_rect(name).layout(Layout::left_to_right(Align::Center)));
    label.add(egui::Label::new(RichText::new(row.name()).font(semibold(14.0)).color(p.text)).truncate().selectable(false));
    if let Some(level) = row.snap.worst {
        let text = match level {
            crate::monitor::alerts::Level::Critical => tr!("critique"),
            crate::monitor::alerts::Level::Warning => tr!("à surveiller"),
        };
        label.add_space(4.0);
        label.label(RichText::new(text).size(11.5).color(p.muted));
    }
    let button = ui.interact(menu, Id::new(("monitor-card-menu", row.id())), Sense::click());
    if button.hovered() {
        inner_hovered(ui);
        ui.painter().rect_filled(menu, 6, p.bg);
    }
    ui.painter().text(menu.center(), Align2::CENTER_CENTER, icon::DOTS_THREE, FontId::proportional(16.0), p.muted);
    let button = button.on_hover_cursor(CursorIcon::PointingHand);
    egui::Popup::menu(&button).show(|ui| server_menu(ui, row.profile, state, actions));

    let mut about: Vec<String> = Vec::new();
    let folder = row.folder();
    if !folder.is_empty() {
        about.push(folder.to_owned());
    }
    if let Some(system) = &row.snap.system {
        about.push(system.clone());
    }
    if about.is_empty() {
        about.push(row.profile.summary());
    }
    ui.add(
        egui::Label::new(RichText::new(about.join("  ·  ")).size(11.5).color(p.faint))
            .truncate()
            .selectable(false),
    );
}

/// The widgets turned on, in order; tiles side by side. True when the
/// alerts had their place.
fn widgets(ui: &mut Ui, p: &Palette, row: &Row<'_>, slots: &[Slot], now: i64, actions: &mut Vec<Action>) -> bool {
    let recent: Vec<Point> = row
        .host
        .map(|host| host.read(|state| state.history.recent().iter().copied().collect()))
        .unwrap_or_default();
    let series = |measure: Measure| -> Vec<(i64, f32)> {
        recent.iter().filter_map(|point| measure.of(point).map(|value| (point.at, value))).collect()
    };
    let snap = &row.snap;
    let mut tiles: Vec<Slot> = Vec::new();
    let mut alerts = false;
    for slot in slots.iter().filter(|slot| slot.on) {
        if slot.style.is_tile() {
            tiles.push(*slot);
            continue;
        }
        tile_row(ui, p, row, &mut tiles);
        match (slot.widget, slot.style) {
            (Widget::Cpu, Style::Curve) => {
                let value = snap.cpu.map_or_else(|| "…".to_owned(), percent);
                curve_row(ui, p, row, tr!("Processeur"), value, series(Measure::Cpu), Measure::Cpu, now);
            }
            (Widget::Cpu, _) => {
                let value = snap.cpu.map_or_else(|| "…".to_owned(), percent);
                gauge_row(ui, p, tr!("Processeur"), snap.cpu, value, Some(&row.rules.cpu));
            }
            (Widget::Memory, Style::Curve) => {
                let value = snap.memory.map_or_else(|| "…".to_owned(), percent);
                curve_row(ui, p, row, tr!("Mémoire"), value, series(Measure::Memory), Measure::Memory, now);
            }
            (Widget::Memory, _) => {
                let value = snap.memory_text.clone().unwrap_or_else(|| "…".to_owned());
                gauge_row(ui, p, tr!("Mémoire"), snap.memory, value, Some(&row.rules.memory));
            }
            (Widget::Disk, Style::Every) => {
                for (mount, fraction, _, _) in &snap.disks {
                    gauge_row(ui, p, &tr!("Disque {mount}", mount), Some(*fraction), percent(*fraction), Some(&row.rules.disk));
                }
            }
            (Widget::Disk, _) => match &snap.disk {
                Some((mount, fraction)) => {
                    gauge_row(ui, p, &tr!("Disque {mount}", mount), Some(*fraction), percent(*fraction), Some(&row.rules.disk));
                }
                None => text_row(ui, p, tr!("Disques"), "…".to_owned()),
            },
            (Widget::Load, Style::Curve) => {
                let value = snap.load.map_or_else(|| "…".to_owned(), |load| decimal(f64::from(load), 2));
                curve_row(ui, p, row, tr!("Charge/cœur"), value, series(Measure::Load), Measure::Load, now);
            }
            (Widget::Load, _) => {
                let text = match (snap.load_total, snap.load, snap.cores) {
                    (Some(total), Some(per_core), Some(cores)) => tr!(
                        "{total} · {per_core} par cœur ({cores} cœurs)",
                        total = decimal(total, 2),
                        per_core = decimal(f64::from(per_core), 2),
                        cores
                    ),
                    (Some(total), _, _) => decimal(total, 2),
                    _ => "…".to_owned(),
                };
                text_row(ui, p, tr!("Charge"), text);
            }
            (Widget::Network, Style::Curve) => network_curve(ui, p, row, &series(Measure::Network), &recent, now),
            (Widget::Network, _) => {
                let text = match (snap.rx, snap.tx) {
                    (Some(rx), Some(tx)) => super::super::ssh_power::stats::rates(f64::from(rx), f64::from(tx)),
                    _ => "…".to_owned(),
                };
                text_row(ui, p, tr!("Réseau"), text);
            }
            (Widget::Alerts, _) => {
                alerts = true;
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.set_height(24.0);
                    ui.spacing_mut().item_spacing.x = 6.0;
                    if !trouble(ui, p, row, actions) {
                        chips(ui, p, &snap.alerts, 2);
                    }
                });
                ui.add_space(2.0);
            }
            (Widget::Uptime, _) => {
                let text = snap.uptime.map_or_else(|| "…".to_owned(), uptime_text);
                text_row(ui, p, tr!("Allumé depuis"), text);
            }
        }
    }
    tile_row(ui, p, row, &mut tiles);
    alerts
}

/// A figure on its line: its name, a gauge and its value.
fn gauge_row(ui: &mut Ui, p: &Palette, label: &str, fraction: Option<f32>, value: String, threshold: Option<&crate::monitor::alerts::Threshold>) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::hover());
    let y = rect.center().y;
    name(ui, p, rect, label);
    let right = rect.right() - VALUE - 8.0;
    let track = Rect::from_min_max(pos2(rect.left() + LABEL, y - 3.0), pos2(right.max(rect.left() + LABEL + 20.0), y + 3.0));
    charts::gauge(ui, p, track, fraction, threshold);
    figure(ui, p, Rect::from_min_max(pos2(rect.right() - VALUE, rect.top()), rect.max), value, Align2::RIGHT_CENTER);
}

/// The name of a figure, cut to its room.
fn name(ui: &Ui, p: &Palette, rect: Rect, label: &str) {
    let mut job = egui::text::LayoutJob::simple_singleline(label.to_owned(), FontId::proportional(12.0), p.muted);
    job.wrap = egui::text::TextWrapping::truncate_at_width(LABEL - 6.0);
    let galley = ui.painter().layout_job(job);
    ui.painter().galley(pos2(rect.left(), rect.center().y - galley.size().y / 2.0), galley, p.muted);
}

/// A value, in bold, cut to its room.
fn figure(ui: &Ui, p: &Palette, rect: Rect, value: String, align: Align2) {
    let mut job = egui::text::LayoutJob::simple_singleline(value, semibold(12.0), p.text);
    job.wrap = egui::text::TextWrapping::truncate_at_width(rect.width());
    let galley = ui.painter().layout_job(job);
    let x = match align {
        Align2::RIGHT_CENTER => rect.right() - galley.size().x,
        _ => rect.left(),
    };
    ui.painter().galley(pos2(x, rect.center().y - galley.size().y / 2.0), galley, p.text);
}

/// A figure with the curve of its last half hour.
#[allow(clippy::too_many_arguments)]
fn curve_row(ui: &mut Ui, p: &Palette, row: &Row<'_>, label: &str, value: String, points: Vec<(i64, f32)>, measure: Measure, now: i64) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::hover());
    name(ui, p, rect, label);
    figure(ui, p, Rect::from_min_max(pos2(rect.left() + LABEL, rect.top()), pos2(rect.left() + LABEL + 64.0, rect.bottom())), value, Align2::LEFT_CENTER);
    let area = Rect::from_min_max(pos2(rect.left() + LABEL + 70.0, rect.top() + 3.0), pos2(rect.right(), rect.bottom() - 3.0));
    if area.width() < 30.0 {
        return;
    }
    let highest = points.iter().map(|(_, value)| *value).fold(0.0, f32::max);
    let lines = [Line {
        name: label,
        color: p.accent,
        points,
        dim: false,
    }];
    let scale = super::measure_scale(measure, highest);
    let text = |value: f32| super::measure_text(measure, value);
    let response = charts::mini(ui, p, area, Id::new(("monitor-curve", row.id(), label)), &lines, scale, Range::Minutes30, now, &text);
    if response.hovered() {
        inner_hovered(ui);
    }
}

/// Received and sent: both figures, and both curves on one scale.
fn network_curve(ui: &mut Ui, p: &Palette, row: &Row<'_>, received: &[(i64, f32)], recent: &[Point], now: i64) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), Sense::hover());
    name(ui, p, rect, tr!("Réseau"));
    let (down, up) = (charts::series(p, 0), charts::series(p, 1));
    let speed = |value: Option<f32>| value.map_or_else(|| "…".to_owned(), |value| crate::format::speed(f64::from(value)));
    let painter = ui.painter();
    let x = rect.left() + LABEL;
    for (index, (glyph, color, value)) in [(icon::ARROW_DOWN, down, row.snap.rx), (icon::ARROW_UP, up, row.snap.tx)].into_iter().enumerate() {
        let y = rect.top() + 9.0 + index as f32 * 16.0;
        painter.rect_filled(Rect::from_center_size(pos2(x + 5.0, y), vec2(10.0, 3.0)), 1.5, color);
        painter.text(pos2(x + 14.0, y), Align2::LEFT_CENTER, format!("{glyph} {}", speed(value)), FontId::proportional(11.5), p.text);
    }
    let area = Rect::from_min_max(pos2(rect.left() + LABEL + 104.0, rect.top() + 3.0), pos2(rect.right(), rect.bottom() - 3.0));
    if area.width() < 30.0 {
        return;
    }
    let sent: Vec<(i64, f32)> = recent.iter().filter_map(|point| point.tx.map(|value| (point.at, value))).collect();
    let highest = received.iter().chain(&sent).map(|(_, value)| *value).fold(0.0, f32::max);
    let lines = [
        Line {
            name: tr!("reçu"),
            color: down,
            points: received.to_vec(),
            dim: false,
        },
        Line {
            name: tr!("envoyé"),
            color: up,
            points: sent,
            dim: false,
        },
    ];
    let scale = super::measure_scale(Measure::Network, highest);
    let text = |value: f32| crate::format::speed(f64::from(value));
    let response = charts::mini(ui, p, area, Id::new(("monitor-network", row.id())), &lines, scale, Range::Minutes30, now, &text);
    if response.hovered() {
        inner_hovered(ui);
    }
}

/// A figure said in words.
fn text_row(ui: &mut Ui, p: &Palette, label: &str, text: String) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 20.0), Sense::hover());
    name(ui, p, rect, label);
    let mut job = egui::text::LayoutJob::simple_singleline(text, FontId::proportional(12.0), p.muted);
    job.wrap = egui::text::TextWrapping::truncate_at_width((rect.width() - LABEL).max(0.0));
    let galley = ui.painter().layout_job(job);
    ui.painter().galley(pos2(rect.left() + LABEL, rect.center().y - galley.size().y / 2.0), galley, p.muted);
}

/// The rings and figures gathered so far, side by side, on as many lines
/// as they need.
fn tile_row(ui: &mut Ui, p: &Palette, row: &Row<'_>, tiles: &mut Vec<Slot>) {
    for (line, height) in tile_lines(tiles, ui.available_width()) {
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.set_height(height);
            ui.spacing_mut().item_spacing.x = TILE_GAP;
            for slot in line {
                tile(ui, p, row, slot, height);
            }
        });
        ui.add_space(6.0);
    }
    tiles.clear();
}

fn tile(ui: &mut Ui, p: &Palette, row: &Row<'_>, slot: Slot, height: f32) {
    let snap = &row.snap;
    let (label, fraction, threshold, value) = match slot.widget {
        Widget::Cpu => (tr!("CPU").to_owned(), snap.cpu, Some(&row.rules.cpu), snap.cpu.map(percent)),
        Widget::Memory => (tr!("Mémoire").to_owned(), snap.memory, Some(&row.rules.memory), snap.memory.map(percent)),
        Widget::Disk => match &snap.disk {
            Some((mount, fraction)) => (tr!("Disque {mount}", mount), Some(*fraction), Some(&row.rules.disk), Some(percent(*fraction))),
            None => (tr!("Disques").to_owned(), None, None, None),
        },
        Widget::Load => (tr!("Charge/cœur").to_owned(), None, None, snap.load.map(|load| decimal(f64::from(load), 2))),
        Widget::Network => (tr!("Réseau ↓").to_owned(), None, None, snap.rx.map(|rx| crate::format::speed(f64::from(rx)))),
        Widget::Uptime => (tr!("Allumé depuis").to_owned(), None, None, snap.uptime.map(uptime_text)),
        Widget::Alerts => return,
    };
    let value = value.unwrap_or_else(|| "…".to_owned());
    if slot.style == Style::Ring {
        let (rect, _) = ui.allocate_exact_size(vec2(RING_WIDTH, height), Sense::hover());
        let center = pos2(rect.center().x, rect.top() + 28.0);
        charts::ring(ui, p, center, 24.0, fraction, threshold, &value);
        let mut job = egui::text::LayoutJob::simple_singleline(label, FontId::proportional(11.0), p.muted);
        job.wrap = egui::text::TextWrapping::truncate_at_width(rect.width());
        let galley = ui.painter().layout_job(job);
        ui.painter().galley(pos2(rect.center().x - galley.size().x / 2.0, rect.top() + 56.0), galley, p.muted);
        return;
    }
    let (rect, _) = ui.allocate_exact_size(vec2(NUMBER_WIDTH, NUMBER), Sense::hover());
    let fit = |text: String, font: FontId, color| {
        let mut job = egui::text::LayoutJob::simple_singleline(text, font, color);
        job.wrap = egui::text::TextWrapping::truncate_at_width(NUMBER_WIDTH - 16.0);
        ui.painter().layout_job(job)
    };
    let small = fit(label, FontId::proportional(11.0), p.muted);
    let big = fit(value, semibold(17.0), p.text);
    ui.painter().rect_filled(rect, 7, if p.dark { p.bg } else { p.surface_hover });
    ui.painter().galley(rect.min + vec2(9.0, 6.0), small, p.muted);
    ui.painter().galley(rect.min + vec2(9.0, 21.0), big, p.text);
}

/// The panel that chooses the widgets of the cards, their order and style,
/// how many cards side by side, and in what order.
fn panel(ui: &mut Ui, p: &Palette, prefs: &mut DashboardPrefs) {
    let frame = Frame::new().fill(p.surface).stroke(Stroke::new(1.0, p.border)).corner_radius(10).inner_margin(12);
    frame.show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new(tr!("Widgets des cartes")).font(semibold(14.0)).color(p.text));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                for (name, slots) in presets().into_iter().rev() {
                    if ui.button(crate::i18n::lookup(name)).clicked() {
                        prefs.slots = slots;
                    }
                }
                ui.label(RichText::new(tr!("Modèles")).size(12.0).color(p.muted));
            });
        });
        ui.label(
            RichText::new(tr!("Cochez, glissez la poignée pour changer l'ordre, et choisissez le style de chacun."))
                .size(12.0)
                .color(p.muted),
        );
        ui.add_space(8.0);

        let mut moved: Option<(usize, usize)> = None;
        let count = prefs.slots.len();
        let columns = if ui.available_width() > 560.0 { 2 } else { 1 };
        ui.columns(columns, |columns_ui| {
            for (index, slot) in prefs.slots.iter_mut().enumerate() {
                let ui = &mut columns_ui[index % columns];
                if let Some(to) = slot_row(ui, p, index, count, slot) {
                    moved = Some(to);
                }
                ui.add_space(4.0);
            }
        });
        if let Some((from, to)) = moved {
            prefs.move_slot(from, to);
        }

        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(tr!("Cartes côte à côte")).size(12.0).color(p.muted));
            let columns = [(0u8, tr!("Auto").to_owned()), (2, "2".to_owned()), (3, "3".to_owned()), (4, "4".to_owned())];
            charts::choice(ui, p, &mut prefs.columns, &columns);
            ui.add_space(12.0);
            ui.label(RichText::new(tr!("Ordre")).size(12.0).color(p.muted));
            let orders = [
                (Order::Severity, tr!("les plus graves d'abord").to_owned()),
                (Order::Name, tr!("par nom").to_owned()),
                (Order::Folder, tr!("par dossier").to_owned()),
            ];
            charts::choice(ui, p, &mut prefs.order, &orders);
        });
    });
}

/// One widget in the panel. Returns a move asked for: by its arrows, or
/// by dropping another widget on it.
fn slot_row(ui: &mut Ui, p: &Palette, index: usize, count: usize, slot: &mut Slot) -> Option<(usize, usize)> {
    let mut moved = None;
    let fill = if slot.on { p.bg } else { p.surface };
    let frame = Frame::new().fill(fill).stroke(Stroke::new(1.0, p.border)).corner_radius(8).inner_margin(egui::Margin::symmetric(8, 4));
    let (_, dropped) = ui.dnd_drop_zone::<usize, _>(frame, |ui| {
        ui.set_min_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let grip = ui.dnd_drag_source(Id::new(("monitor-slot", index)), index, |ui| {
                ui.label(RichText::new(icon::DOTS_SIX_VERTICAL).size(15.0).color(p.faint));
            });
            grip.response.on_hover_cursor(CursorIcon::Grab);
            checkbox(ui, p, &mut slot.on, widget_label(slot.widget));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                if ui.add_enabled(index + 1 < count, Button::new(icon::ARROW_DOWN).frame(false)).clicked() {
                    moved = Some((index, index + 1));
                }
                if ui.add_enabled(index > 0, Button::new(icon::ARROW_UP).frame(false)).clicked() {
                    moved = Some((index, index - 1));
                }
                let styles = slot.widget.styles();
                if styles.len() > 1 {
                    ui.add_space(4.0);
                    ComboBox::from_id_salt(("monitor-style", index))
                        .width(120.0)
                        .selected_text(style_label(slot.widget, slot.style))
                        .show_ui(ui, |ui| {
                            for style in styles {
                                ui.selectable_value(&mut slot.style, *style, style_label(slot.widget, *style));
                            }
                        });
                }
            });
        });
    });
    if let Some(from) = dropped {
        moved = Some((*from, index));
    }
    moved
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiles_wrap_to_the_width_of_the_card() {
        let ring = |widget| Slot { widget, on: true, style: Style::Ring };
        let tiles = [ring(Widget::Cpu), ring(Widget::Memory), ring(Widget::Disk), Slot { widget: Widget::Load, on: true, style: Style::Number }];
        let lines = tile_lines(&tiles, 290.0);
        assert_eq!(lines.len(), 2);
        assert_eq!((lines[0].0.len(), lines[0].1), (3, RING));
        assert_eq!((lines[1].0.len(), lines[1].1), (1, NUMBER));
        assert_eq!(tile_lines(&tiles, 400.0).len(), 1, "room enough");
        assert!(tile_lines(&[], 400.0).is_empty());
    }
}
