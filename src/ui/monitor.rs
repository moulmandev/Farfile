//! The dashboard of the watched servers (see [`crate::monitor`]): a card per
//! server, the most worrying first, with its gauges and the curves of the
//! last half hour, then the journal of the alerts. Also its entry in the
//! sidebar, what it adds to the menu of a connection, and its two dialogs:
//! which servers to watch, and the preferences of the monitoring.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use egui::{
    Align, Align2, Button, Color32, CursorIcon, DragValue, FontId, Grid, Id, Label, Layout, Popup, Rect, RichText,
    ScrollArea, Sense, Stroke, StrokeKind, TextEdit, Ui, UiBuilder, pos2, vec2,
};

use super::Action;
use super::dialogs::Outcome;
use super::ssh_power::stats::{details, fullest, rates};
use super::widgets::{checkbox, icon, icon_button, muted, primary_button, small};
use crate::monitor::alerts::{Level, Metric, Rules, Threshold};
use crate::monitor::{self, Alert, HISTORY, Host, HostState, INTERVALS, MonitorSettings, Point, metric_name, value_text};
use crate::profiles::{Profile, Protocol};
use crate::ssh_power::stats::{Status, decimal, percent, uptime_text};
use crate::theme::{Palette, RADIUS, semibold};

pub enum MonitorAction {
    /// Show the dashboard, opening its tab when there is none.
    Open,
    /// Choose the servers to watch.
    Pick,
    /// The preferences, on the thresholds of this server or of all of them.
    Settings(Option<String>),
    /// Start or stop watching this connection.
    Watch(String, bool),
    /// Try again a watch paused after a refused login.
    Retry(String),
    ClearJournal,
}

pub enum MonitorDialog {
    Pick(PickForm),
    Settings(Box<SettingsForm>),
}

/// What the sidebar and the menus need to know, published once per frame
/// (see [`publish`]).
#[derive(Clone, Default)]
pub struct Published {
    pub watched: Vec<String>,
    /// Servers with a critical alert, then with a warning.
    pub critical: usize,
    pub warning: usize,
}

fn published_id() -> Id {
    Id::new("monitor-published")
}

pub fn publish(ctx: &egui::Context, published: Published) {
    ctx.data_mut(|data| data.insert_temp(published_id(), Arc::new(published)));
}

fn published(ctx: &egui::Context) -> Arc<Published> {
    ctx.data(|data| data.get_temp::<Arc<Published>>(published_id())).unwrap_or_default()
}

/// What the dashboard shows, borrowed from the app.
pub struct Dashboard<'a> {
    pub monitor: &'a monitor::Monitor,
    pub settings: &'a MonitorSettings,
    pub profiles: &'a [Profile],
}

const CARD_WIDTH: f32 = 310.0;
const CARD_HEIGHT: f32 = 190.0;
const GAP: f32 = 12.0;
const LABEL_WIDTH: f32 = 82.0;
const VALUE_WIDTH: f32 = 92.0;
const SPARK_WIDTH: f32 = 84.0;

/// How worrying a server is, to put the worst first.
fn rank(state: Option<&HostState>) -> u8 {
    let Some(state) = state else {
        return 1;
    };
    match state.worst() {
        Some(Level::Critical) => 4,
        Some(Level::Warning) => 3,
        None if matches!(state.view.status, Status::Failed(_)) => 2,
        None => 0,
    }
}

/// The colour of a gauge, from the thresholds of its alert when it has one.
fn tint(p: &Palette, value: f32, threshold: Option<&Threshold>) -> Color32 {
    match threshold.filter(|threshold| threshold.enabled) {
        Some(threshold) if value >= threshold.limit(Level::Critical) => p.danger,
        Some(threshold) if value >= threshold.warning => p.folder,
        Some(_) => p.success,
        None => super::ssh_power::stats::level(p, value / 100.0),
    }
}

fn level_color(p: &Palette, level: Option<Level>) -> Color32 {
    match level {
        Some(Level::Critical) => p.danger,
        Some(Level::Warning) => p.folder,
        None => p.success,
    }
}

fn level_icon(level: Option<Level>) -> &'static str {
    match level {
        Some(Level::Critical) => icon::WARNING_OCTAGON,
        Some(Level::Warning) => icon::WARNING,
        None => icon::CHECK_CIRCLE,
    }
}

/// `3 machines · 1 critique · tout va bien`.
fn summary(count: usize, critical: usize, warning: usize) -> String {
    let mut parts = vec![trn!(count, "{n} machine surveillée", "{n} machines surveillées")];
    if critical > 0 {
        parts.push(trn!(critical, "{n} critique", "{n} critiques"));
    }
    if warning > 0 {
        parts.push(trn!(warning, "{n} à surveiller", "{n} à surveiller"));
    }
    if critical == 0 && warning == 0 && count > 0 {
        parts.push(tr!("tout va bien").to_owned());
    }
    parts.join("  ·  ")
}

/// The whole tab.
pub fn show(ui: &mut Ui, p: &Palette, dashboard: &Dashboard<'_>, actions: &mut Vec<Action>) {
    dashboard.monitor.mark_seen();
    // The curves move with time, and "il y a 2 min" changes.
    ui.ctx().request_repaint_after(std::time::Duration::from_secs(5));

    let watched: Vec<&Profile> = dashboard
        .settings
        .watched
        .iter()
        .filter_map(|id| dashboard.profiles.iter().find(|profile| profile.id == *id))
        .filter(|profile| profile.protocol == Protocol::Sftp)
        .collect();
    let (critical, warning) = dashboard.monitor.counts();

    ui.horizontal(|ui| {
        ui.label(RichText::new(icon::GAUGE).size(22.0).color(p.accent));
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.label(RichText::new(tr!("Surveillance")).font(semibold(18.0)).color(p.text));
            ui.label(RichText::new(summary(watched.len(), critical, warning)).size(12.0).color(p.muted));
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if icon_button(ui, p, icon::GEAR, tr!("Seuils, notifications et fermeture de la fenêtre"), true).clicked() {
                actions.push(Action::Monitor(MonitorAction::Settings(None)));
            }
            let pick = Button::new(format!("{}  {}", icon::LIST_CHECKS, tr!("Choisir les machines…")));
            if ui.add(pick).clicked() {
                actions.push(Action::Monitor(MonitorAction::Pick));
            }
        });
    });
    ui.add_space(10.0);

    if watched.is_empty() {
        empty(ui, p, actions);
        return;
    }

    // Read once, sorted the worst first.
    let mut cards: Vec<(&Profile, Option<&Arc<Host>>, u8)> = watched
        .iter()
        .map(|profile| {
            let host = dashboard.monitor.host(&profile.id);
            let rank = host.map_or(1, |host| host.read(|state| rank(Some(state))));
            (*profile, host, rank)
        })
        .collect();
    cards.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| crate::format::natural_cmp(a.0.title(), b.0.title())));

    ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        let width = ui.available_width();
        let columns = (((width + GAP) / (CARD_WIDTH + GAP)).floor() as usize).max(1);
        let card_width = ((width - GAP * (columns - 1) as f32) / columns as f32).floor();
        for row in cards.chunks(columns) {
            let (line, _) = ui.allocate_exact_size(vec2(width, CARD_HEIGHT), Sense::hover());
            for (index, (profile, host, _)) in row.iter().enumerate() {
                let x = line.left() + index as f32 * (card_width + GAP);
                let rect = Rect::from_min_size(pos2(x, line.top()), vec2(card_width, CARD_HEIGHT));
                let rules = dashboard.settings.rules_for(&profile.id);
                card(ui, p, rect, profile, *host, rules, actions);
            }
            ui.add_space(GAP);
        }
        ui.add_space(8.0);
        journal(ui, p, dashboard, actions);
    });
}

fn empty(ui: &mut Ui, p: &Palette, actions: &mut Vec<Action>) {
    ui.vertical_centered(|ui| {
        ui.add_space((ui.available_height() * 0.2).max(24.0));
        ui.label(RichText::new(icon::GAUGE).size(48.0).color(p.accent));
        ui.add_space(6.0);
        ui.label(RichText::new(tr!("Aucune machine surveillée")).font(semibold(16.0)).color(p.text));
        ui.add_space(2.0);
        ui.add(
            Label::new(muted(
                p,
                tr!(
                    "Choisissez des serveurs SSH : leur processeur, leur mémoire et leurs disques s'affichent \
                     ici côte à côte, et Farfile vous prévient quand un seuil est dépassé."
                ),
            ))
            .wrap(),
        );
        ui.add_space(12.0);
        if primary_button(ui, p, tr!("Choisir les machines…"), true).clicked() {
            actions.push(Action::Monitor(MonitorAction::Pick));
        }
    });
}

/// The card of one server. A click opens its tab.
fn card(
    ui: &mut Ui,
    p: &Palette,
    rect: Rect,
    profile: &Profile,
    host: Option<&Arc<Host>>,
    rules: &Rules,
    actions: &mut Vec<Action>,
) {
    let id = Id::new(("monitor-card", &profile.id));
    let response = ui.interact(rect, id, Sense::click());
    let worst = host.and_then(|host| host.read(HostState::worst));
    let border = match worst {
        Some(level) => level_color(p, Some(level)).gamma_multiply(0.7),
        None => p.border,
    };
    let fill = if response.hovered() { p.surface_hover } else { p.surface };
    ui.painter().rect(rect, RADIUS, fill, Stroke::new(1.0, border), StrokeKind::Inside);

    let inner = rect.shrink2(vec2(14.0, 12.0));
    let mut content = ui.new_child(UiBuilder::new().max_rect(inner).layout(Layout::top_down(Align::Min)));
    content.set_clip_rect(rect);
    content.spacing_mut().item_spacing.y = 0.0;
    match host {
        Some(host) => host.read(|state| card_content(&mut content, p, profile, state, rules, actions)),
        None => {
            title_row(&mut content, p, profile, None, p.faint, actions);
            content.add_space(30.0);
            content.label(RichText::new(tr!("Démarrage de la surveillance…")).size(12.0).color(p.muted));
        }
    }

    let response = response.on_hover_cursor(CursorIcon::PointingHand);
    // The details of the server, unless a curve or a button has its own.
    let inner = ui.ctx().data_mut(|data| data.remove_temp::<bool>(inner_hover_id())).unwrap_or(false);
    if let Some(host) = host
        && !inner
    {
        response.clone().on_hover_ui(|ui| host.read(|state| details(ui, p, &state.view)));
    }
    if response.clicked() {
        actions.push(Action::Connect(profile.id.clone()));
    }
    response.context_menu(|ui| card_menu(ui, profile, actions));
}

fn card_menu(ui: &mut Ui, profile: &Profile, actions: &mut Vec<Action>) {
    ui.set_min_width(230.0);
    let mut item = |ui: &mut Ui, text: String, action: Action| {
        if ui.button(text).clicked() {
            actions.push(action);
            ui.close();
        }
    };
    let id = profile.id.clone();
    item(ui, format!("{}  {}", icon::PLUGS_CONNECTED, tr!("Ouvrir")), Action::Connect(id.clone()));
    item(
        ui,
        format!("{}  {}", icon::TERMINAL_WINDOW, tr!("Nouveau terminal")),
        Action::OpenTab(id.clone(), Some(crate::profiles::Panes::Terminal)),
    );
    item(
        ui,
        format!("{}  {}", icon::SLIDERS_HORIZONTAL, tr!("Seuils de cette machine…")),
        Action::Monitor(MonitorAction::Settings(Some(id.clone()))),
    );
    ui.separator();
    item(
        ui,
        format!("{}  {}", icon::EYE_SLASH, tr!("Ne plus surveiller")),
        Action::Monitor(MonitorAction::Watch(id, false)),
    );
}

fn inner_hover_id() -> Id {
    Id::new("monitor-inner-hover")
}

/// The pointer is over a part of a card that says something of its own.
fn inner_hovered(ctx: &egui::Context) {
    ctx.data_mut(|data| data.insert_temp(inner_hover_id(), true));
}

/// The dot, the name and the menu button.
fn title_row(ui: &mut Ui, p: &Palette, profile: &Profile, level: Option<Level>, dot: Color32, actions: &mut Vec<Action>) {
    let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::hover());
    ui.painter().circle_filled(pos2(row.left() + 4.0, row.center().y), 4.0, dot);
    let menu = Rect::from_center_size(pos2(row.right() - 10.0, row.center().y), vec2(22.0, 22.0));
    let name = Rect::from_min_max(pos2(row.left() + 16.0, row.top()), pos2(menu.left() - 6.0, row.bottom()));
    let mut label = ui.new_child(UiBuilder::new().max_rect(name).layout(Layout::left_to_right(Align::Center)));
    label.add(Label::new(RichText::new(profile.title()).font(semibold(14.0)).color(p.text)).truncate().selectable(false));
    if let Some(level) = level {
        let text = match level {
            Level::Critical => tr!("critique"),
            Level::Warning => tr!("à surveiller"),
        };
        label.add_space(4.0);
        label.label(RichText::new(text).size(11.5).color(p.muted));
    }

    let button = ui.interact(menu, Id::new(("monitor-card-menu", &profile.id)), Sense::click());
    if button.hovered() {
        inner_hovered(ui.ctx());
        ui.painter().rect_filled(menu, 6, p.bg);
    }
    ui.painter().text(menu.center(), Align2::CENTER_CENTER, icon::DOTS_THREE, FontId::proportional(16.0), p.muted);
    let button = button.on_hover_cursor(CursorIcon::PointingHand);
    Popup::menu(&button).show(|ui| card_menu(ui, profile, actions));
}

fn card_content(ui: &mut Ui, p: &Palette, profile: &Profile, state: &HostState, rules: &Rules, actions: &mut Vec<Action>) {
    let worst = state.worst();
    let dot = match (&state.view.status, worst) {
        (_, Some(level)) => level_color(p, Some(level)),
        (Status::Ready, None) => p.success,
        (Status::Failed(_), None) => p.danger,
        _ => p.faint,
    };
    title_row(ui, p, profile, worst, dot, actions);

    // Where it is filed, what it runs, since when.
    let sample = state.view.sample.as_ref();
    let mut about: Vec<String> = Vec::new();
    if !profile.folder.is_empty() {
        about.push(profile.folder.rsplit('/').next().unwrap_or(&profile.folder).to_owned());
    }
    if let Some(system) = sample.and_then(|sample| sample.system.clone()) {
        about.push(system);
    }
    if let Some(uptime) = sample.and_then(|sample| sample.uptime) {
        about.push(tr!("allumé depuis {time}", time = uptime_text(uptime)));
    }
    if about.is_empty() {
        about.push(profile.summary());
    }
    ui.add(Label::new(RichText::new(about.join("  ·  ")).size(11.5).color(p.faint)).truncate().selectable(false));
    ui.add_space(10.0);

    match &state.view.status {
        Status::Connecting => {
            ui.label(RichText::new(format!("{}  {}", icon::HOURGLASS, tr!("Connexion…"))).size(12.0).color(p.muted));
        }
        Status::Unsupported(system) => {
            let text = match system {
                Some(system) => tr!("Mesures non disponibles sur ce système ({system})", system),
                None => tr!("Mesures non disponibles sur ce système").to_owned(),
            };
            ui.add(Label::new(RichText::new(format!("{}  {text}", icon::INFO)).size(12.0).color(p.muted)).wrap());
        }
        Status::Ready | Status::Failed(_) => figures(ui, p, state, rules),
    }

    // The foot: what is wrong, or why nothing is read.
    let foot = Rect::from_min_max(pos2(ui.max_rect().left(), ui.max_rect().bottom() - 24.0), ui.max_rect().max);
    let mut foot_ui = ui.new_child(UiBuilder::new().max_rect(foot).layout(Layout::left_to_right(Align::Center)));
    foot_ui.spacing_mut().item_spacing.x = 6.0;
    if let Some(error) = &state.paused {
        let retry = foot_ui.add(Button::new(format!("{}  {}", icon::ARROW_CLOCKWISE, tr!("Réessayer"))).small());
        if retry.clicked() {
            actions.push(Action::Monitor(MonitorAction::Retry(profile.id.clone())));
        }
        foot_ui
            .add(
                Label::new(RichText::new(tr!("Connexion refusée : surveillance en pause")).size(11.5).color(p.danger))
                    .truncate(),
            )
            .on_hover_text(error);
    } else if let Status::Failed(error) = &state.view.status {
        let text = match state.unreachable_since {
            Some(since) => tr!("Injoignable depuis {time}", time = ago(since.elapsed().as_secs_f64(), false)),
            None => tr!("Injoignable").to_owned(),
        };
        foot_ui
            .add(Label::new(RichText::new(format!("{}  {text}", icon::WARNING_CIRCLE)).size(11.5).color(p.danger)).truncate())
            .on_hover_text(error);
    } else {
        chips(&mut foot_ui, p, &state.alerts);
    }
}

/// `2 min`, or `il y a 2 min`.
fn ago(seconds: f64, with_ago: bool) -> String {
    let minutes = (seconds / 60.0).floor() as u64;
    match (minutes, with_ago) {
        (0, true) => tr!("à l'instant").to_owned(),
        (0, false) => trn!(seconds.max(1.0) as u64, "{n} seconde", "{n} secondes"),
        (_, true) => trn!(minutes, "il y a {n} min", "il y a {n} min"),
        (_, false) => trn!(minutes, "{n} min", "{n} min"),
    }
}

/// The gauges, and the line of the load and the network.
fn figures(ui: &mut Ui, p: &Palette, state: &HostState, rules: &Rules) {
    let Some(sample) = &state.view.sample else {
        ui.label(RichText::new(tr!("En attente d'une première mesure…")).size(12.0).color(p.muted));
        return;
    };
    let history: Vec<Point> = state.history.iter().copied().collect();
    let cpu = state.view.cpu.map(|cpu| cpu * 100.0);
    gauge(
        ui,
        p,
        tr!("Processeur"),
        cpu,
        cpu.map_or_else(|| "…".to_owned(), |cpu| percent(cpu / 100.0)),
        Some(&rules.cpu),
        Some((history.as_slice(), |point: &Point| point.cpu)),
    );
    let memory = sample.memory.filter(|memory| memory.total > 0);
    let used = memory.map(|memory| memory.used() as f32 / memory.total as f32 * 100.0);
    let text = memory.map_or_else(
        || "…".to_owned(),
        |memory| super::ssh_power::stats::size_pair(memory.used(), memory.total),
    );
    let curve = Some((history.as_slice(), |point: &Point| point.memory));
    gauge(ui, p, tr!("Mémoire"), used, text, Some(&rules.memory), curve);
    if let Some(disk) = fullest(&sample.disks) {
        let value = disk.fraction() * 100.0;
        let label = tr!("Disque {mount}", mount = disk.mount);
        // What is left, and the other disks, are in the details on hover.
        let text = percent(disk.fraction());
        gauge(ui, p, &label, Some(value), text, Some(&rules.disk), None::<(&[Point], fn(&Point) -> Option<f32>)>);
    }

    // Load and network on one line.
    ui.add_space(4.0);
    let mut parts = Vec::new();
    if let Some([_, five, _]) = sample.load {
        let mut load = tr!("Charge {value}", value = decimal(five, 2));
        if let Some(cores) = sample.cores.filter(|cores| *cores > 0) {
            load.push_str(&tr!(" ({value} par cœur)", value = decimal(five / f64::from(cores), 2)));
        }
        parts.push(load);
    }
    if let Some((received, sent)) = state.view.network {
        parts.push(rates(received, sent));
    }
    if !parts.is_empty() {
        ui.add(Label::new(RichText::new(parts.join("    ")).size(11.5).color(p.muted)).truncate().selectable(false));
    }
}

/// One figure: its name, its gauge, its value and, when given, its curve.
fn gauge<F: Fn(&Point) -> Option<f32>>(
    ui: &mut Ui,
    p: &Palette,
    label: &str,
    value: Option<f32>,
    text: String,
    threshold: Option<&Threshold>,
    curve: Option<(&[Point], F)>,
) {
    let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::hover());
    let painter = ui.painter().with_clip_rect(row);
    let y = row.center().y;

    let mut job = egui::text::LayoutJob::simple_singleline(label.to_owned(), FontId::proportional(12.0), p.muted);
    job.wrap = egui::text::TextWrapping::truncate_at_width(LABEL_WIDTH - 6.0);
    let galley = painter.layout_job(job);
    painter.galley(pos2(row.left(), y - galley.size().y / 2.0), galley, p.muted);

    let spark = if row.width() > LABEL_WIDTH + VALUE_WIDTH + SPARK_WIDTH + 60.0 { SPARK_WIDTH } else { 0.0 };
    let bar_left = row.left() + LABEL_WIDTH;
    let bar_right = row.right() - VALUE_WIDTH - spark - if spark > 0.0 { 10.0 } else { 0.0 };
    let track = Rect::from_min_max(pos2(bar_left, y - 3.0), pos2(bar_right, y + 3.0));
    painter.rect_filled(track, 3, p.bg);
    if let Some(value) = value {
        let fraction = (value / 100.0).clamp(0.0, 1.0);
        if fraction > 0.0 {
            let fill = Rect::from_min_size(track.min, vec2((track.width() * fraction).max(4.0), track.height()));
            painter.rect_filled(fill, 3, tint(p, value, threshold));
        }
        // Where the warning starts, as a tick on the track.
        if let Some(threshold) = threshold.filter(|threshold| threshold.enabled && threshold.warning < 100.0) {
            let x = track.left() + track.width() * (threshold.warning / 100.0).clamp(0.0, 1.0);
            painter.line_segment([pos2(x, track.top() - 2.0), pos2(x, track.bottom() + 2.0)], Stroke::new(1.0, p.faint));
        }
    }
    let mut job = egui::text::LayoutJob::simple_singleline(text, semibold(12.0), p.text);
    job.wrap = egui::text::TextWrapping::truncate_at_width(VALUE_WIDTH - 4.0);
    let galley = painter.layout_job(job);
    painter.galley(pos2(bar_right + VALUE_WIDTH - galley.size().x, y - galley.size().y / 2.0), galley, p.text);

    if spark > 0.0
        && let Some((points, pick)) = curve
    {
        let area = Rect::from_min_max(pos2(row.right() - spark, row.top() + 2.0), pos2(row.right(), row.bottom() - 2.0));
        sparkline(ui, p, area, label, points, pick);
    }
}

/// The last half hour of a figure, from 0 to 100 %, the newest on the
/// right. Hovering it reads the value at that moment.
fn sparkline<F: Fn(&Point) -> Option<f32>>(ui: &mut Ui, p: &Palette, area: Rect, label: &str, points: &[Point], pick: F) {
    let now = Instant::now();
    // The time the curve covers: what was read so far, from half a minute
    // up to the half hour kept, so that it fills its room from the start.
    let oldest = points.first().map_or(0.0, |point| now.duration_since(point.at).as_secs_f32());
    let span = oldest.clamp(30.0, HISTORY.as_secs_f32());
    let at = |point: &Point| -> Option<(egui::Pos2, f32)> {
        let value = pick(point)?;
        let age = now.duration_since(point.at).as_secs_f32();
        let x = area.right() - area.width() * (age / span).clamp(0.0, 1.0);
        let y = area.bottom() - area.height() * value.clamp(0.0, 1.0);
        Some((pos2(x, y), value))
    };
    let line: Vec<(egui::Pos2, f32, Instant)> =
        points.iter().filter_map(|point| at(point).map(|(pos, value)| (pos, value, point.at))).collect();
    let painter = ui.painter().with_clip_rect(area.expand(2.0));
    painter.line_segment([area.left_bottom(), area.right_bottom()], Stroke::new(1.0, p.border));
    if line.len() >= 2 {
        // A filled area under a line that goes up and down is not convex:
        // one thin quad per step instead.
        for pair in line.windows(2) {
            let (a, b) = (pair[0].0, pair[1].0);
            let quad = vec![a, b, pos2(b.x, area.bottom()), pos2(a.x, area.bottom())];
            painter.add(egui::Shape::convex_polygon(quad, p.accent.gamma_multiply(0.12), Stroke::NONE));
        }
        let path: Vec<egui::Pos2> = line.iter().map(|(pos, _, _)| *pos).collect();
        painter.add(egui::Shape::line(path, Stroke::new(1.5, p.accent)));
    }

    let response = ui.interact(area, ui.id().with(("spark", label, area.min.x as i32, area.min.y as i32)), Sense::hover());
    if response.hovered() {
        inner_hovered(ui.ctx());
    }
    if let Some(pointer) = response.hover_pos()
        && let Some((pos, value, when)) = line
            .iter()
            .min_by(|a, b| (a.0.x - pointer.x).abs().total_cmp(&(b.0.x - pointer.x).abs()))
            .copied()
    {
        painter.line_segment([pos2(pos.x, area.top()), pos2(pos.x, area.bottom())], Stroke::new(1.0, p.muted));
        painter.circle_filled(pos, 3.0, p.accent);
        response.on_hover_text(format!(
            "{label} : {}  ·  {}",
            percent(value),
            ago(now.duration_since(when).as_secs_f64(), true)
        ));
    } else if line.len() < 2 {
        response.on_hover_text(tr!("La courbe se dessine au fil des mesures."));
    }
}

/// The alerts under way, as small labels.
fn chips(ui: &mut Ui, p: &Palette, alerts: &[(Metric, monitor::alerts::Active)]) {
    if alerts.is_empty() {
        ui.label(RichText::new(format!("{}  {}", icon::CHECK_CIRCLE, tr!("Aucune alerte"))).size(11.5).color(p.faint));
        return;
    }
    const SHOWN: usize = 2;
    for (metric, active) in alerts.iter().take(SHOWN) {
        let color = level_color(p, Some(active.level));
        let text = format!("{}  {} {}", level_icon(Some(active.level)), metric_name(metric), value_text(metric, active.value));
        let galley = ui.painter().layout_no_wrap(text, FontId::proportional(11.5), p.text);
        let (rect, _) = ui.allocate_exact_size(galley.size() + vec2(14.0, 6.0), Sense::hover());
        ui.painter().rect(rect, 6, color.gamma_multiply(0.16), Stroke::new(1.0, color.gamma_multiply(0.6)), StrokeKind::Inside);
        ui.painter().galley(rect.min + vec2(7.0, 3.0), galley, p.text);
    }
    if alerts.len() > SHOWN {
        let more: Vec<String> = alerts
            .iter()
            .skip(SHOWN)
            .map(|(metric, active)| format!("{} {}", metric_name(metric), value_text(metric, active.value)))
            .collect();
        ui.label(RichText::new(format!("+{}", alerts.len() - SHOWN)).size(11.5).color(p.muted))
            .on_hover_text(more.join("\n"));
    }
}

/// When an alert came: the time today, the date before.
fn when(alert: &Alert) -> String {
    let today = chrono::Local::now().date_naive();
    if alert.at.date_naive() == today {
        alert.at.format("%H:%M:%S").to_string()
    } else {
        alert.at.format(crate::format::date_pattern()).to_string()
    }
}

/// The latest alerts, under the cards.
fn journal(ui: &mut Ui, p: &Palette, dashboard: &Dashboard<'_>, actions: &mut Vec<Action>) {
    const SHOWN: usize = 60;
    ui.horizontal(|ui| {
        ui.label(RichText::new(tr!("ALERTES RÉCENTES")).size(11.0).color(p.faint));
        if !dashboard.monitor.journal.is_empty()
            && ui.add(Button::new(RichText::new(tr!("Effacer")).size(11.5)).small().frame(false)).clicked()
        {
            actions.push(Action::Monitor(MonitorAction::ClearJournal));
        }
    });
    ui.add_space(4.0);
    if dashboard.monitor.journal.is_empty() {
        ui.label(RichText::new(tr!("Rien à signaler depuis l'ouverture de Farfile.")).size(12.0).color(p.muted));
        return;
    }
    Grid::new("monitor-journal").num_columns(4).spacing(vec2(12.0, 6.0)).show(ui, |ui| {
        for alert in dashboard.monitor.journal.iter().take(SHOWN) {
            let level = alert.level();
            ui.label(RichText::new(when(alert)).size(12.0).color(p.faint));
            ui.label(RichText::new(level_icon(level)).size(13.0).color(level_color(p, level)));
            let name = ui.add(
                Label::new(RichText::new(&alert.name).font(semibold(12.5)).color(p.text))
                    .sense(Sense::click())
                    .selectable(false),
            );
            if name.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                actions.push(Action::Connect(alert.profile_id.clone()));
            }
            let color = if alert.told { p.text } else { p.muted };
            ui.label(RichText::new(alert.text()).size(12.5).color(color));
            ui.end_row();
        }
    });
    ui.add_space(12.0);
}

/// "Surveillance", under "Cet ordinateur" at the top of the sidebar, with
/// the number of servers in trouble.
pub fn sidebar_entry(ui: &mut Ui, p: &Palette, active: Option<&str>, actions: &mut Vec<Action>) {
    const ROW_HEIGHT: f32 = 28.0;
    let published = published(ui.ctx());
    let (_, rect) = ui.allocate_space(vec2(ui.available_width(), ROW_HEIGHT));
    let response = ui.interact(rect, Id::new("monitor-entry"), Sense::click());
    let is_active = active.is_some_and(monitor::is_dashboard);
    if ui.is_rect_visible(rect) {
        if is_active {
            ui.painter().rect_filled(rect, RADIUS, p.surface);
        } else if response.hovered() {
            ui.painter().rect_filled(rect, RADIUS, p.surface.gamma_multiply(0.6));
        }
        ui.painter().text(
            pos2(rect.left() + 28.0, rect.center().y),
            Align2::CENTER_CENTER,
            icon::GAUGE,
            FontId::proportional(16.0),
            if is_active { p.accent } else { p.muted },
        );
        let mut right = rect.right() - 8.0;
        let troubled = published.critical + published.warning;
        if troubled > 0 {
            let color = if published.critical > 0 { p.danger } else { p.folder };
            let galley = ui.painter().layout_no_wrap(troubled.to_string(), semibold(11.0), p.on_accent);
            let badge = Rect::from_min_max(
                pos2(right - galley.size().x - 12.0, rect.center().y - 9.0),
                pos2(right, rect.center().y + 9.0),
            );
            ui.painter().rect_filled(badge, 9, color);
            ui.painter().galley(badge.center() - galley.size() / 2.0, galley, p.on_accent);
            right = badge.left() - 6.0;
        } else if !published.watched.is_empty() {
            let count = ui.painter().layout_no_wrap(published.watched.len().to_string(), FontId::proportional(11.0), p.faint);
            let at = pos2(right - count.size().x - 4.0, rect.center().y - count.size().y / 2.0);
            right = at.x - 6.0;
            ui.painter().galley(at, count, p.faint);
        }
        let text = Rect::from_min_max(pos2(rect.left() + 44.0, rect.top()), pos2(right, rect.bottom()));
        let mut label = ui.new_child(UiBuilder::new().max_rect(text).layout(Layout::left_to_right(Align::Center)));
        label.add(Label::new(RichText::new(tr!("Surveillance")).color(p.text)).truncate().selectable(false));
    }
    let response = response.on_hover_cursor(CursorIcon::PointingHand);
    if response.clicked() {
        actions.push(Action::Monitor(MonitorAction::Open));
    }
    response.context_menu(|ui| entry_menu(ui, actions));
    let tip = match (published.critical, published.warning, published.watched.len()) {
        (0, 0, 0) => tr!("Suivre plusieurs serveurs d'un coup d'œil, et être prévenu des disques pleins").to_owned(),
        (critical, warning, count) => summary(count, critical, warning),
    };
    response.on_hover_text(tip);
}

fn entry_menu(ui: &mut Ui, actions: &mut Vec<Action>) {
    ui.set_min_width(220.0);
    let entries = [
        (icon::GAUGE, tr!("Ouvrir"), MonitorAction::Open),
        (icon::LIST_CHECKS, tr!("Choisir les machines…"), MonitorAction::Pick),
        (icon::GEAR, tr!("Préférences de la surveillance…"), MonitorAction::Settings(None)),
    ];
    for (glyph, text, action) in entries {
        if ui.button(format!("{glyph}  {text}")).clicked() {
            actions.push(Action::Monitor(action));
            ui.close();
        }
    }
}

/// The same, in the folded sidebar.
pub fn rail_button(ui: &mut Ui, p: &Palette, actions: &mut Vec<Action>) {
    let button = icon_button(ui, p, icon::GAUGE, tr!("Surveillance"), true);
    let published = published(ui.ctx());
    if published.critical + published.warning > 0 {
        let color = if published.critical > 0 { p.danger } else { p.folder };
        ui.painter().circle_filled(button.rect.right_top() + vec2(-7.0, 7.0), 4.0, color);
    }
    if button.clicked() {
        actions.push(Action::Monitor(MonitorAction::Open));
    }
    button.context_menu(|ui| entry_menu(ui, actions));
}

/// "Surveiller", in the menu of an SSH connection.
pub fn host_menu_item(ui: &mut Ui, profile: &Profile, actions: &mut Vec<Action>) {
    if profile.protocol != Protocol::Sftp {
        return;
    }
    let watched = published(ui.ctx()).watched.contains(&profile.id);
    let text = if watched {
        format!("{}  {}", icon::EYE_SLASH, tr!("Ne plus surveiller"))
    } else {
        format!("{}  {}", icon::GAUGE, tr!("Surveiller dans le tableau de bord"))
    };
    if ui.button(text).clicked() {
        actions.push(Action::Monitor(MonitorAction::Watch(profile.id.clone(), !watched)));
        ui.close();
    }
}

// The dialogs.

pub fn dialog(ui: &mut Ui, p: &Palette, dialog: &mut MonitorDialog, first_frame: bool, outcome: &mut Outcome) {
    match dialog {
        MonitorDialog::Pick(form) => pick_form(ui, p, form, first_frame, outcome),
        MonitorDialog::Settings(form) => settings_form(ui, p, form, outcome),
    }
}

fn title(ui: &mut Ui, p: &Palette, text: &str) {
    ui.label(RichText::new(text).font(semibold(17.0)).color(p.text));
    ui.add_space(8.0);
}

fn section(ui: &mut Ui, p: &Palette, text: &str) {
    ui.add_space(12.0);
    ui.label(RichText::new(text.to_uppercase()).size(11.0).color(p.faint));
    ui.add_space(4.0);
}

fn buttons(ui: &mut Ui, p: &Palette, confirm: &str, outcome: &mut Outcome, extra: impl FnOnce(&mut Ui)) {
    ui.add_space(14.0);
    ui.horizontal(|ui| {
        extra(ui);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if primary_button(ui, p, confirm, true).clicked() {
                *outcome = Outcome::Confirmed;
            }
            if ui.button(tr!("Annuler")).clicked() {
                *outcome = Outcome::Cancelled;
            }
        });
    });
}

/// One connection offered by [`PickForm`].
pub struct PickRow {
    pub id: String,
    name: String,
    folder: String,
    summary: String,
    pub checked: bool,
}

/// Which SSH connections to watch.
pub struct PickForm {
    pub rows: Vec<PickRow>,
    filter: String,
}

impl PickForm {
    pub fn new(profiles: &[Profile], watched: &[String]) -> Self {
        let mut rows: Vec<PickRow> = profiles
            .iter()
            .filter(|profile| profile.protocol == Protocol::Sftp)
            .map(|profile| PickRow {
                id: profile.id.clone(),
                name: profile.title().to_owned(),
                folder: profile.folder.clone(),
                summary: profile.summary(),
                checked: watched.contains(&profile.id),
            })
            .collect();
        rows.sort_by(|a, b| {
            crate::profiles::sort_key(&a.folder)
                .cmp(&crate::profiles::sort_key(&b.folder))
                .then_with(|| crate::format::natural_cmp(&a.name, &b.name))
        });
        Self {
            rows,
            filter: String::new(),
        }
    }

    /// The connections ticked, those watched already keeping their place.
    pub fn chosen(&self, watched: &[String]) -> Vec<String> {
        let ticked = |id: &String| self.rows.iter().any(|row| row.id == *id && row.checked);
        let mut chosen: Vec<String> = watched.iter().filter(|id| ticked(id)).cloned().collect();
        for row in self.rows.iter().filter(|row| row.checked) {
            if !chosen.contains(&row.id) {
                chosen.push(row.id.clone());
            }
        }
        chosen
    }

    /// Scripts only: ticks the connection of this name.
    pub fn tick(&mut self, name: &str, checked: bool) {
        for row in self.rows.iter_mut().filter(|row| row.name == name) {
            row.checked = checked;
        }
    }
}

fn pick_form(ui: &mut Ui, p: &Palette, form: &mut PickForm, first_frame: bool, outcome: &mut Outcome) {
    ui.set_width(520.0);
    title(ui, p, tr!("Machines à surveiller"));
    ui.add(
        Label::new(small(
            p,
            tr!(
                "Chaque serveur coché est lu sur une connexion SSH à part, avec un court script en lecture seule \
                 (Linux). Les seuils d'alerte se règlent dans les préférences de la surveillance."
            ),
        ))
        .wrap(),
    );
    ui.add_space(8.0);
    if form.rows.is_empty() {
        ui.label(muted(p, tr!("Aucune connexion SSH enregistrée : ajoutez-en une avec le bouton +.")));
        buttons(ui, p, tr!("Fermer"), outcome, |_| {});
        return;
    }
    let search = ui.add(
        TextEdit::singleline(&mut form.filter)
            .hint_text(format!("{}  {}", icon::MAGNIFYING_GLASS, tr!("Rechercher")))
            .margin(egui::Margin::symmetric(8, 5))
            .desired_width(f32::INFINITY),
    );
    if first_frame {
        search.request_focus();
    }
    ui.add_space(6.0);

    let needle = crate::profiles::fold(form.filter.trim());
    let shown: Vec<usize> = (0..form.rows.len())
        .filter(|&index| {
            let row = &form.rows[index];
            needle.is_empty()
                || crate::profiles::fold(&row.name).contains(&needle)
                || crate::profiles::fold(&row.folder).contains(&needle)
                || crate::profiles::fold(&row.summary).contains(&needle)
        })
        .collect();
    let mut folders: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    for &index in &shown {
        let folder = form.rows[index].folder.as_str();
        if !order.iter().any(|known| known == folder) {
            order.push(folder.to_owned());
        }
        folders.entry(folder).or_default().push(index);
    }
    let groups: Vec<(String, Vec<usize>)> =
        order.iter().map(|folder| (folder.clone(), folders.get(folder.as_str()).cloned().unwrap_or_default())).collect();

    ScrollArea::vertical().max_height(360.0).auto_shrink([false, true]).show(ui, |ui| {
        for (folder, indexes) in groups {
            if !folder.is_empty() {
                let ticked = indexes.iter().filter(|&&index| form.rows[index].checked).count();
                let mut all = ticked == indexes.len();
                ui.add_space(4.0);
                let label = format!("{}  {}   {ticked}/{}", icon::FOLDER, folder, indexes.len());
                if checkbox(ui, p, &mut all, label).changed() {
                    for &index in &indexes {
                        form.rows[index].checked = all;
                    }
                }
            }
            for index in indexes {
                let row = &mut form.rows[index];
                ui.horizontal(|ui| {
                    ui.add_space(if folder.is_empty() { 0.0 } else { 22.0 });
                    checkbox(ui, p, &mut row.checked, row.name.clone());
                    ui.add(Label::new(RichText::new(&row.summary).size(11.5).color(p.faint)).truncate());
                });
            }
        }
        if shown.is_empty() {
            ui.label(muted(p, tr!("Aucune connexion ne correspond.")));
        }
    });

    let count = form.rows.iter().filter(|row| row.checked).count();
    buttons(ui, p, tr!("Surveiller"), outcome, |ui| {
        ui.label(small(p, trn!(count, "{n} machine cochée", "{n} machines cochées")));
    });
}

/// The two pages of the preferences.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Page {
    #[default]
    Thresholds,
    Alerts,
}

/// The preferences of the monitoring being edited; nothing changes until
/// they are saved.
pub struct SettingsForm {
    pub draft: MonitorSettings,
    /// Whose thresholds are shown: none for every server.
    scope: Option<String>,
    /// The watched servers, by id, with their names.
    hosts: Vec<(String, String)>,
    page: Page,
    /// A test notification went out.
    tested: bool,
}

impl SettingsForm {
    pub fn new(settings: &MonitorSettings, profiles: &[Profile], scope: Option<String>) -> Self {
        let hosts: Vec<(String, String)> = settings
            .watched
            .iter()
            .filter_map(|id| profiles.iter().find(|profile| profile.id == *id))
            .map(|profile| (profile.id.clone(), profile.title().to_owned()))
            .collect();
        let scope = scope.filter(|id| hosts.iter().any(|(known, _)| known == id));
        Self {
            draft: settings.clone(),
            scope,
            hosts,
            page: Page::Thresholds,
            tested: false,
        }
    }

    /// Scripts only: shows the page of the alerts.
    pub fn show_alerts(&mut self) {
        self.page = Page::Alerts;
    }
}

fn settings_form(ui: &mut Ui, p: &Palette, form: &mut SettingsForm, outcome: &mut Outcome) {
    ui.set_width(640.0);
    title(ui, p, tr!("Préférences de la surveillance"));
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let pages = [(Page::Thresholds, tr!("Seuils d'alerte")), (Page::Alerts, tr!("Prévenir et fermer"))];
        for (page, label) in pages {
            let selected = form.page == page;
            let text = RichText::new(label).color(if selected { p.on_accent } else { p.text });
            let mut button = Button::new(text);
            if selected {
                button = button.fill(p.accent);
            }
            if ui.add(button).clicked() {
                form.page = page;
            }
        }
    });
    ui.add_space(4.0);
    let room = ui.ctx().content_rect().height() - 260.0;
    let height = room.clamp(200.0, 400.0);
    ScrollArea::vertical()
        .id_salt("monitor-settings-page")
        .min_scrolled_height(height)
        .max_height(height)
        .auto_shrink([false, false])
        .show(ui, |ui| match form.page {
            Page::Thresholds => thresholds_page(ui, p, form),
            Page::Alerts => alerts_page(ui, p, form),
        });
    buttons(ui, p, tr!("Enregistrer"), outcome, |ui| {
        if form.page == Page::Thresholds && ui.button(tr!("Valeurs par défaut")).clicked() {
            match &form.scope {
                Some(id) => {
                    form.draft.overrides.remove(id);
                }
                None => form.draft.rules = Rules::default(),
            }
        }
    });
}

fn thresholds_page(ui: &mut Ui, p: &Palette, form: &mut SettingsForm) {
    section(ui, p, tr!("Seuils de"));
    let scope_name = |scope: &Option<String>| match scope {
        None => tr!("Toutes les machines").to_owned(),
        Some(id) => form.hosts.iter().find(|(known, _)| known == id).map_or_else(|| id.clone(), |(_, name)| name.clone()),
    };
    let shown = scope_name(&form.scope);
    let mut scope = form.scope.clone();
    egui::ComboBox::from_id_salt("monitor-scope").width(280.0).selected_text(shown).show_ui(ui, |ui| {
        ui.selectable_value(&mut scope, None, tr!("Toutes les machines"));
        for (id, name) in &form.hosts {
            let own = if form.draft.overrides.contains_key(id) { tr!(" (seuils propres)") } else { "" };
            ui.selectable_value(&mut scope, Some(id.clone()), format!("{name}{own}"));
        }
    });
    form.scope = scope;

    let editable = match form.scope.clone() {
        None => true,
        Some(id) => {
            ui.add_space(6.0);
            let mut own = form.draft.overrides.contains_key(&id);
            let name = scope_name(&form.scope);
            if checkbox(ui, p, &mut own, tr!("Des seuils propres à « {name} »", name)).changed() {
                if own {
                    let shared = form.draft.rules.clone();
                    form.draft.overrides.insert(id.clone(), shared);
                } else {
                    form.draft.overrides.remove(&id);
                }
            }
            if !own {
                ui.label(small(p, tr!("Elle suit les seuils de toutes les machines, ci-dessous.")));
            }
            own
        }
    };

    ui.add_space(8.0);
    let rules = match &form.scope {
        Some(id) if editable => form.draft.overrides.entry(id.clone()).or_default(),
        _ => &mut form.draft.rules,
    };
    ui.add_enabled_ui(editable || form.scope.is_none(), |ui| {
        Grid::new("monitor-thresholds").num_columns(4).spacing(vec2(14.0, 10.0)).show(ui, |ui| {
            ui.label("");
            ui.label(small(p, tr!("Attention dès")));
            ui.label(small(p, tr!("Critique dès")));
            ui.label(small(p, tr!("Pendant au moins")));
            ui.end_row();
            threshold_row(ui, p, tr!("Disques (chacun)"), &mut rules.disk, false);
            threshold_row(ui, p, tr!("Mémoire"), &mut rules.memory, false);
            threshold_row(ui, p, tr!("Processeur"), &mut rules.cpu, false);
            threshold_row(ui, p, "Swap", &mut rules.swap, false);
            threshold_row(ui, p, tr!("Charge par cœur"), &mut rules.load, true);
            named_checkbox(ui, p, &mut rules.offline.enabled, tr!("Injoignable"));
            ui.label(small(p, tr!("toujours critique")));
            ui.label("");
            ui.add_enabled(rules.offline.enabled, DragValue::new(&mut rules.offline.minutes).range(1..=240).suffix(" min"));
            ui.end_row();
        });
    });
    ui.add_space(10.0);
    ui.add(
        Label::new(small(
            p,
            tr!(
                "Une alerte prévient une fois quand elle commence ou s'aggrave, puis se termine quand la valeur \
                 redescend nettement sous son seuil (5 points, ou un dixième pour la charge). La charge compte le \
                 travail en attente sur 5 minutes : 1 par cœur occupe tout le processeur."
            ),
        ))
        .wrap(),
    );
}

/// The first column of the thresholds: a grid cell has no width of its own
/// to wrap the text in, so it gets one.
fn named_checkbox(ui: &mut Ui, p: &Palette, checked: &mut bool, label: &str) {
    const WIDTH: f32 = 170.0;
    ui.allocate_ui_with_layout(vec2(WIDTH, 24.0), Layout::left_to_right(Align::Center), |ui| {
        ui.set_min_width(WIDTH);
        checkbox(ui, p, checked, label);
    });
}

fn threshold_row(ui: &mut Ui, p: &Palette, label: &str, threshold: &mut Threshold, load: bool) {
    named_checkbox(ui, p, &mut threshold.enabled, label);
    let enabled = threshold.enabled;
    ui.add_enabled(enabled, limit(&mut threshold.warning, load));
    ui.add_enabled(enabled, limit(&mut threshold.critical, load));
    ui.add_enabled(enabled, DragValue::new(&mut threshold.minutes).range(0..=240).suffix(" min"));
    ui.end_row();
}

/// A limit: a share in percent, or a load per core.
fn limit(value: &mut f32, load: bool) -> DragValue<'_> {
    let drag = DragValue::new(value)
        .custom_formatter(move |value, _| if load { decimal(value, 1) } else { format!("{} %", decimal(value, 0)) })
        .custom_parser(|text| text.trim_end_matches('%').trim().replace(',', ".").parse().ok());
    if load { drag.range(0.1..=64.0).speed(0.05) } else { drag.range(1.0..=100.0).speed(0.5) }
}

fn alerts_page(ui: &mut Ui, p: &Palette, form: &mut SettingsForm) {
    let delivery = &mut form.draft.delivery;
    section(ui, p, tr!("Prévenir par"));
    checkbox(ui, p, &mut delivery.in_app, tr!("Un message dans la fenêtre de Farfile"));
    ui.add_space(4.0);
    if cfg!(windows) {
        ui.horizontal(|ui| {
            checkbox(ui, p, &mut delivery.system, tr!("Une notification de Windows, même Farfile caché"));
            if ui.add_enabled(delivery.system, Button::new(tr!("Essayer")).small()).clicked() {
                crate::monitor::notify::test();
                form.tested = true;
            }
        });
        if form.tested {
            ui.add(
                Label::new(small(
                    p,
                    tr!(
                        "Envoyée. Si rien ne s'affiche, autorisez les notifications de Farfile dans Paramètres > \
                         Système > Notifications de Windows."
                    ),
                ))
                .wrap(),
            );
        }
        ui.add_space(4.0);
        checkbox(ui, p, &mut delivery.flash, tr!("Faire clignoter Farfile dans la barre des tâches"));
        ui.add_space(4.0);
    }
    checkbox(ui, p, &mut delivery.recoveries, tr!("Prévenir aussi quand une alerte se termine"));

    section(ui, p, tr!("En fermant la fenêtre"));
    ui.radio_value(&mut form.draft.background, false, tr!("Quitter Farfile"));
    let keep = if cfg!(windows) {
        tr!("Continuer la surveillance depuis la zone de notification, près de l'horloge")
    } else {
        tr!("Continuer la surveillance, fenêtre réduite")
    };
    ui.radio_value(&mut form.draft.background, true, keep);
    ui.add(
        Label::new(small(
            p,
            tr!(
                "Seulement quand des machines sont surveillées. Un clic sur l'icône rouvre la fenêtre, son menu \
                 quitte Farfile."
            ),
        ))
        .wrap(),
    );

    section(ui, p, tr!("Fréquence des mesures"));
    ui.horizontal(|ui| {
        ui.label(muted(p, tr!("Hors du tableau de bord, toutes les")));
        let label = |seconds: u32| {
            if seconds < 60 { format!("{seconds} s") } else { format!("{} min", seconds / 60) }
        };
        egui::ComboBox::from_id_salt("monitor-interval")
            .width(90.0)
            .selected_text(label(form.draft.interval))
            .show_ui(ui, |ui| {
                for seconds in INTERVALS {
                    ui.selectable_value(&mut form.draft.interval, seconds, label(seconds));
                }
            });
    });
    ui.label(small(
        p,
        tr!(
            "Toutes les {seconds} s quand le tableau de bord est affiché.",
            seconds = monitor::FAST_INTERVAL.as_secs()
        ),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(id: &str, name: &str, folder: &str, protocol: Protocol) -> Profile {
        Profile {
            id: id.to_owned(),
            name: name.to_owned(),
            folder: folder.to_owned(),
            protocol,
            endpoint: format!("{id}.example.org"),
            ..Profile::default()
        }
    }

    #[test]
    fn only_ssh_connections_are_offered_and_the_order_is_kept() {
        let profiles = [
            profile("s3", "Bucket", "", Protocol::S3),
            profile("b", "Web 2", "Prod", Protocol::Sftp),
            profile("a", "Web 10", "Prod", Protocol::Sftp),
            profile("c", "Laptop", "", Protocol::Sftp),
        ];
        let watched = vec!["a".to_owned()];
        let mut form = PickForm::new(&profiles, &watched);
        let names: Vec<&str> = form.rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, ["Laptop", "Web 2", "Web 10"]);
        assert!(form.rows.iter().find(|row| row.id == "a").unwrap().checked);
        form.tick("Laptop", true);
        form.tick("Web 2", true);
        // Already watched first, then the new ones in the order listed.
        assert_eq!(form.chosen(&watched), ["a", "c", "b"]);
        form.tick("Web 10", false);
        assert_eq!(form.chosen(&watched), ["c", "b"]);
    }

    #[test]
    fn the_summary_counts_what_matters() {
        assert_eq!(summary(3, 0, 0), "3 machines surveillées  ·  tout va bien");
        assert_eq!(summary(3, 1, 2), "3 machines surveillées  ·  1 critique  ·  2 à surveiller");
        assert_eq!(summary(1, 1, 0), "1 machine surveillée  ·  1 critique");
    }

    #[test]
    fn gauges_follow_the_thresholds_of_their_alert() {
        let p = Palette::dark();
        let disk = Rules::default().disk;
        assert_eq!(tint(&p, 50.0, Some(&disk)), p.success);
        assert_eq!(tint(&p, 86.0, Some(&disk)), p.folder);
        assert_eq!(tint(&p, 95.0, Some(&disk)), p.danger);
        let off = Threshold { enabled: false, ..disk };
        assert_eq!(tint(&p, 95.0, Some(&off)), super::super::ssh_power::stats::level(&p, 0.95));
    }
}
