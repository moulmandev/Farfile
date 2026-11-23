//! The dashboard of the watched servers (see [`crate::monitor`]), in five
//! views to choose from: cards made of the widgets chosen ([`cards`]), a
//! sortable list ([`list`]), a heat map ([`heat`]), every server on the same
//! charts ([`compare`]) and large tiles for a screen of their own
//! ([`wall`]). A click on a server shows it in full ([`detail`]).
//!
//! Also its entry in the sidebar, what it adds to the menu of a connection,
//! and its dialogs ([`dialogs`], and [`collector`] for the history kept on
//! a server while Farfile is closed).

mod cards;
mod charts;
mod collector;
mod compare;
mod detail;
mod dialogs;
mod heat;
mod list;
mod wall;

use std::collections::HashSet;
use std::sync::Arc;

use egui::{
    Align, Align2, Button, CursorIcon, FontId, Grid, Id, Label, Layout, Rect, RichText, Sense, Stroke, StrokeKind, Ui,
    UiBuilder, pos2, vec2,
};

pub use collector::CollectorForm;
pub use dialogs::{MonitorDialog, PickForm, SettingsForm, dialog};

use super::Action;
use super::widgets::{icon, icon_button, muted, primary_button};
use crate::monitor::alerts::{Active, Level, Metric, Rules};
use crate::monitor::dashboard::{DashboardPrefs, Measure, Order, Range, View};
use crate::monitor::{self, Alert, Host, HostState, MonitorSettings, metric_name, value_text};
use crate::profiles::{Profile, Protocol};
use crate::ssh_power::stats::{Status, decimal, fullest, percent};
use crate::theme::{Palette, RADIUS, semibold};
use charts::Scale;

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
    /// The dialog of the collector of this server: the history it keeps
    /// while Farfile is closed.
    Collector(String),
    ClearJournal,
    /// The view or the widgets changed: keep them.
    Prefs(Box<DashboardPrefs>),
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

/// What the dashboard remembers from one frame to the next, unsaved.
#[derive(Default)]
pub struct DashState {
    /// The server shown in full, by connection id.
    pub detail: Option<String>,
    /// The panel that chooses the widgets of the cards is open.
    pub customizing: bool,
    /// Servers left out of the compared charts.
    pub hidden: HashSet<String>,
}

/// Where a watched server stands.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) enum State {
    /// Its watch is about to start.
    #[default]
    Starting,
    Connecting,
    Ready,
    /// Not a system the script reads, with its name when known.
    Unsupported(Option<String>),
    /// Out of reach, with why.
    Unreachable(String),
    /// Its login was refused, with why: the watch waits for the user.
    Paused(String),
}

/// The figures of a server, read once for the frame. Shares from 0 to 1.
#[derive(Clone, Debug, Default)]
pub(super) struct Snap {
    pub state: State,
    pub cpu: Option<f32>,
    pub memory: Option<f32>,
    /// `7,5 / 30,9 Go`.
    pub memory_text: Option<String>,
    /// Mount point, share used, bytes left, size.
    pub disks: Vec<(String, f32, u64, u64)>,
    /// The fullest disk.
    pub disk: Option<(String, f32)>,
    /// Over 5 minutes, per core.
    pub load: Option<f32>,
    pub load_total: Option<f64>,
    pub cores: Option<u32>,
    pub rx: Option<f32>,
    pub tx: Option<f32>,
    pub uptime: Option<f64>,
    pub system: Option<String>,
    pub worst: Option<Level>,
    pub alerts: Vec<(Metric, Active)>,
    /// Seconds out of reach.
    pub unreachable_for: Option<f64>,
    pub collector: crate::monitor::collector::Collector,
}

impl Snap {
    fn of(state: &HostState) -> Self {
        let view = &state.view;
        let sample = view.sample.as_ref();
        let memory = sample.and_then(|sample| sample.memory).filter(|memory| memory.total > 0);
        let mut snap = Snap {
            state: match (&state.paused, &view.status) {
                (Some(error), _) => State::Paused(error.clone()),
                (None, Status::Failed(error)) => State::Unreachable(error.clone()),
                (None, Status::Unsupported(system)) => State::Unsupported(system.clone()),
                (None, Status::Ready) => State::Ready,
                (None, Status::Connecting) => State::Connecting,
            },
            cpu: view.cpu,
            memory: memory.map(|memory| memory.used() as f32 / memory.total as f32),
            memory_text: memory.map(|memory| super::ssh_power::stats::size_pair(memory.used(), memory.total)),
            rx: view.network.map(|(received, _)| received as f32),
            tx: view.network.map(|(_, sent)| sent as f32),
            worst: state.worst(),
            alerts: state.alerts.clone(),
            unreachable_for: state.unreachable_since.map(|since| since.elapsed().as_secs_f64()),
            collector: state.collector.clone(),
            ..Snap::default()
        };
        if let Some(sample) = sample {
            snap.disks = sample
                .disks
                .iter()
                .map(|disk| (disk.mount.clone(), disk.fraction(), disk.available, disk.size))
                .collect();
            snap.disk = fullest(&sample.disks).map(|disk| (disk.mount.clone(), disk.fraction()));
            snap.cores = sample.cores;
            snap.load_total = sample.load.map(|[_, five, _]| five);
            if let (Some(five), Some(cores)) = (snap.load_total, sample.cores.filter(|cores| *cores > 0)) {
                snap.load = Some((five / f64::from(cores)) as f32);
            }
            snap.uptime = sample.uptime;
            snap.system.clone_from(&sample.system);
        }
        snap
    }

    /// How worrying it is, to put the worst first.
    fn rank(&self) -> u8 {
        match (&self.state, self.worst) {
            (State::Paused(_), _) | (_, Some(Level::Critical)) => 4,
            (_, Some(Level::Warning)) => 3,
            (State::Unreachable(_), None) => 2,
            (State::Starting | State::Connecting, None) => 1,
            _ => 0,
        }
    }

    /// The colour of its dot.
    fn dot(&self, p: &Palette) -> egui::Color32 {
        match (&self.state, self.worst) {
            (_, Some(level)) => charts::level(p, Some(level)),
            (State::Ready, None) => p.success,
            (State::Unreachable(_), None) => p.danger,
            _ => p.faint,
        }
    }

    /// Readings have come: there are figures to show.
    fn has_figures(&self) -> bool {
        self.cpu.is_some() || self.memory.is_some()
    }
}

/// A watched server as the views get it.
pub(super) struct Row<'a> {
    pub profile: &'a Profile,
    pub host: Option<&'a Arc<Host>>,
    pub rules: &'a Rules,
    /// Its place among the watched servers, which gives its colour.
    pub index: usize,
    pub snap: Snap,
}

impl Row<'_> {
    pub fn id(&self) -> &str {
        &self.profile.id
    }

    pub fn name(&self) -> &str {
        self.profile.title()
    }

    /// Its last folder, for grouping: `Moon`.
    pub fn folder(&self) -> &str {
        self.profile.folder.rsplit('/').next().unwrap_or_default()
    }

    /// Its history over `range`, as (moment, value) pairs of `measure`.
    pub fn series(&self, measure: Measure, range: Range, now: i64) -> Vec<(i64, f32)> {
        self.host.map_or_else(Vec::new, |host| {
            host.read(|state| {
                state
                    .history
                    .points(range, now)
                    .iter()
                    .filter_map(|point| measure.of(point).map(|value| (point.at, value)))
                    .collect()
            })
        })
    }
}

/// The rows in `order`.
pub(super) fn sort(rows: &mut [Row<'_>], order: Order) {
    let name = |row: &Row<'_>| crate::profiles::sort_key(row.name());
    match order {
        Order::Severity => rows.sort_by(|a, b| b.snap.rank().cmp(&a.snap.rank()).then_with(|| name(a).cmp(&name(b)))),
        Order::Name => rows.sort_by_key(name),
        Order::Folder => rows.sort_by_cached_key(|row| (crate::profiles::sort_key(&row.profile.folder), name(row))),
    }
}

/// What a measure is called.
pub(super) fn measure_label(measure: Measure) -> &'static str {
    match measure {
        Measure::Cpu => tr!("Processeur"),
        Measure::Memory => tr!("Mémoire"),
        Measure::Disk => tr!("Disque le plus plein"),
        Measure::Load => tr!("Charge par cœur"),
        Measure::Network => tr!("Réseau reçu"),
    }
}

/// Its value now.
pub(super) fn measure_now(measure: Measure, snap: &Snap) -> Option<f32> {
    match measure {
        Measure::Cpu => snap.cpu,
        Measure::Memory => snap.memory,
        Measure::Disk => snap.disk.as_ref().map(|(_, fraction)| *fraction),
        Measure::Load => snap.load,
        Measure::Network => snap.rx,
    }
}

/// A value of it, in words: `46 %`, `0,44`, `1,4 Mo/s`.
pub(super) fn measure_text(measure: Measure, value: f32) -> String {
    match measure {
        Measure::Cpu | Measure::Memory | Measure::Disk => percent(value),
        Measure::Load => decimal(f64::from(value), 2),
        Measure::Network => crate::format::speed(f64::from(value)),
    }
}

/// The same on an axis, shorter.
pub(super) fn measure_axis(measure: Measure, value: f32) -> String {
    match measure {
        Measure::Load => decimal(f64::from(value), 1),
        _ => measure_text(measure, value),
    }
}

/// How its values map to heights, `highest` being the highest seen.
pub(super) fn measure_scale(measure: Measure, highest: f32) -> Scale {
    match measure {
        Measure::Cpu | Measure::Memory | Measure::Disk => Scale { max: 1.0, log: false },
        Measure::Load => Scale {
            max: ((highest * 1.1) * 2.0).ceil().max(2.0) / 2.0,
            log: false,
        },
        Measure::Network => Scale {
            max: (highest * 2.0).max(1e4),
            log: true,
        },
    }
}

pub(super) fn range_label(range: Range) -> String {
    match range {
        Range::Minutes30 => "30 min".to_owned(),
        Range::Hours6 => "6 h".to_owned(),
        Range::Hours24 => "24 h".to_owned(),
        Range::Days7 => tr!("7 j").to_owned(),
    }
}

/// The choice of the span of time.
pub(super) fn range_choice(ui: &mut Ui, p: &Palette, range: &mut Range) {
    let options: Vec<(Range, String)> = Range::ALL.iter().map(|range| (*range, range_label(*range))).collect();
    charts::choice(ui, p, range, &options);
}

/// The choice of the measure, as a short list.
pub(super) fn measure_choice(ui: &mut Ui, measure: &mut Measure) {
    egui::ComboBox::from_id_salt("monitor-measure")
        .width(170.0)
        .selected_text(measure_label(*measure))
        .show_ui(ui, |ui| {
            for option in Measure::ALL {
                ui.selectable_value(measure, option, measure_label(option));
            }
        });
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

/// `2 min`, or `il y a 2 min`.
pub(super) fn ago(seconds: f64, with_ago: bool) -> String {
    let minutes = (seconds / 60.0).floor() as u64;
    match (minutes, with_ago) {
        (0, true) => tr!("à l'instant").to_owned(),
        (0, false) => trn!(seconds.max(1.0) as u64, "{n} seconde", "{n} secondes"),
        (_, true) => trn!(minutes, "il y a {n} min", "il y a {n} min"),
        (_, false) => trn!(minutes, "{n} min", "{n} min"),
    }
}

/// The whole tab.
pub fn show(ui: &mut Ui, p: &Palette, dashboard: &Dashboard<'_>, state: &mut DashState, actions: &mut Vec<Action>) {
    dashboard.monitor.mark_seen();
    // The curves move with time, and "il y a 2 min" changes.
    ui.ctx().request_repaint_after(std::time::Duration::from_secs(5));

    let mut rows: Vec<Row<'_>> = dashboard
        .settings
        .watched
        .iter()
        .filter_map(|id| dashboard.profiles.iter().find(|profile| profile.id == *id))
        .filter(|profile| profile.protocol == Protocol::Sftp)
        .enumerate()
        .map(|(index, profile)| {
            let host = dashboard.monitor.host(&profile.id);
            Row {
                profile,
                host,
                rules: dashboard.settings.rules_for(&profile.id),
                index,
                snap: host.map(|host| host.read(Snap::of)).unwrap_or_default(),
            }
        })
        .collect();
    let mut prefs = dashboard.settings.dashboard.clone();
    prefs.normalize();
    let before = prefs.clone();

    let detail = state.detail.clone().and_then(|id| rows.iter().position(|row| row.id() == id));
    match detail {
        Some(index) => detail::show(ui, p, dashboard, &rows[index], &mut prefs, state, actions),
        None => {
            state.detail = None;
            header(ui, p, &rows, &mut prefs, state, actions);
            if rows.is_empty() {
                empty(ui, p, actions);
            } else {
                match prefs.view {
                    View::Cards => cards::show(ui, p, dashboard, &mut rows, &mut prefs, state, actions),
                    View::List => list::show(ui, p, dashboard, &mut rows, &mut prefs, state, actions),
                    View::Heat => heat::show(ui, p, &rows, &prefs, state),
                    View::Charts => compare::show(ui, p, &rows, &prefs, state),
                    View::Wall => wall::show(ui, p, &mut rows, &prefs, state, actions),
                }
            }
        }
    }
    if prefs != before {
        actions.push(Action::Monitor(MonitorAction::Prefs(Box::new(prefs))));
    }
}

/// The title, the views, and what the view chosen offers.
fn header(ui: &mut Ui, p: &Palette, rows: &[Row<'_>], prefs: &mut DashboardPrefs, state: &mut DashState, actions: &mut Vec<Action>) {
    let critical = rows.iter().filter(|row| row.snap.rank() == 4).count();
    let warning = rows.iter().filter(|row| row.snap.rank() == 3).count();
    ui.horizontal(|ui| {
        ui.label(RichText::new(icon::GAUGE).size(22.0).color(p.accent));
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.label(RichText::new(tr!("Surveillance")).font(semibold(18.0)).color(p.text));
            ui.label(RichText::new(summary(rows.len(), critical, warning)).size(12.0).color(p.muted));
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
    ui.add_space(8.0);
    if rows.is_empty() {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        let views = [
            (View::Cards, format!("{}  {}", icon::SQUARES_FOUR, tr!("Cartes"))),
            (View::List, format!("{}  {}", icon::ROWS, tr!("Liste"))),
            (View::Heat, format!("{}  {}", icon::GRID_FOUR, tr!("Chaleur"))),
            (View::Charts, format!("{}  {}", icon::CHART_LINE, tr!("Graphiques"))),
            (View::Wall, format!("{}  {}", icon::MONITOR, tr!("Mur"))),
        ];
        charts::choice(ui, p, &mut prefs.view, &views);
        ui.add_space(10.0);
        match prefs.view {
            View::Cards => {
                let text = if state.customizing { tr!("Terminé") } else { tr!("Personnaliser…") };
                let mut button = Button::new(format!("{}  {text}", icon::SLIDERS_HORIZONTAL));
                if state.customizing {
                    button = button.fill(p.selection);
                }
                if ui.add(button).clicked() {
                    state.customizing = !state.customizing;
                }
            }
            View::List => {
                ui.label(RichText::new(tr!("Grouper")).size(12.0).color(p.muted));
                let options = [
                    (crate::monitor::dashboard::Grouping::None, tr!("non").to_owned()),
                    (crate::monitor::dashboard::Grouping::Folder, tr!("par dossier").to_owned()),
                    (crate::monitor::dashboard::Grouping::State, tr!("par état").to_owned()),
                ];
                charts::choice(ui, p, &mut prefs.grouping, &options);
            }
            View::Heat | View::Charts => {
                measure_choice(ui, &mut prefs.measure);
                range_choice(ui, p, &mut prefs.range);
            }
            View::Wall => {
                let options = [(false, tr!("Tout").to_owned()), (true, tr!("Seulement les alertes").to_owned())];
                charts::choice(ui, p, &mut prefs.only_alerts, &options);
                let fullscreen = ui.ctx().input(|input| input.viewport().fullscreen.unwrap_or(false));
                let (glyph, text) = if fullscreen {
                    (icon::ARROWS_IN, tr!("Quitter le plein écran"))
                } else {
                    (icon::ARROWS_OUT, tr!("Plein écran"))
                };
                if ui.button(format!("{glyph}  {text}")).clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fullscreen));
                }
            }
        }
    });
    ui.add_space(10.0);
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

/// The menu of a server, on its card, row or tile.
pub(super) fn server_menu(ui: &mut Ui, profile: &Profile, state: &mut DashState, actions: &mut Vec<Action>) {
    ui.set_min_width(230.0);
    let id = profile.id.clone();
    if ui.button(format!("{}  {}", icon::CHART_LINE, tr!("Voir en détail"))).clicked() {
        state.detail = Some(id.clone());
        ui.close();
    }
    let entries = [
        (format!("{}  {}", icon::PLUGS_CONNECTED, tr!("Ouvrir")), Action::Connect(id.clone())),
        (
            format!("{}  {}", icon::TERMINAL_WINDOW, tr!("Nouveau terminal")),
            Action::OpenTab(id.clone(), Some(crate::profiles::Panes::Terminal)),
        ),
        (
            format!("{}  {}", icon::SLIDERS_HORIZONTAL, tr!("Seuils de cette machine…")),
            Action::Monitor(MonitorAction::Settings(Some(id.clone()))),
        ),
        (
            format!("{}  {}", icon::CLOCK_COUNTER_CLOCKWISE, tr!("Historique hors connexion…")),
            Action::Monitor(MonitorAction::Collector(id.clone())),
        ),
    ];
    for (text, action) in entries {
        if ui.button(text).clicked() {
            actions.push(action);
            ui.close();
        }
    }
    ui.separator();
    if ui.button(format!("{}  {}", icon::EYE_SLASH, tr!("Ne plus surveiller"))).clicked() {
        actions.push(Action::Monitor(MonitorAction::Watch(id, false)));
        ui.close();
    }
}

/// The alerts under way, as small labels; `limit` of them at most.
pub(super) fn chips(ui: &mut Ui, p: &Palette, alerts: &[(Metric, Active)], limit: usize) {
    if alerts.is_empty() {
        ui.label(RichText::new(format!("{}  {}", icon::CHECK_CIRCLE, tr!("Aucune alerte"))).size(11.5).color(p.faint));
        return;
    }
    for (metric, active) in alerts.iter().take(limit) {
        let color = charts::level(p, Some(active.level));
        let text = format!("{}  {} {}", level_icon(Some(active.level)), metric_name(metric), value_text(metric, active.value));
        let galley = ui.painter().layout_no_wrap(text, FontId::proportional(11.5), p.text);
        let (rect, _) = ui.allocate_exact_size(galley.size() + vec2(14.0, 6.0), Sense::hover());
        ui.painter().rect(rect, 6, color.gamma_multiply(0.16), Stroke::new(1.0, color.gamma_multiply(0.6)), StrokeKind::Inside);
        ui.painter().galley(rect.min + vec2(7.0, 3.0), galley, p.text);
    }
    if alerts.len() > limit {
        let more: Vec<String> = alerts
            .iter()
            .skip(limit)
            .map(|(metric, active)| format!("{} {}", metric_name(metric), value_text(metric, active.value)))
            .collect();
        ui.label(RichText::new(format!("+{}", alerts.len() - limit)).size(11.5).color(p.muted))
            .on_hover_text(more.join("\n"));
    }
}

/// What is wrong with the watch, when something is: a line in red, with
/// "Réessayer" after a refused login. True when it drew one.
pub(super) fn trouble(ui: &mut Ui, p: &Palette, row: &Row<'_>, actions: &mut Vec<Action>) -> bool {
    match &row.snap.state {
        State::Paused(error) => {
            let retry = ui.add(Button::new(format!("{}  {}", icon::ARROW_CLOCKWISE, tr!("Réessayer"))).small());
            if retry.clicked() {
                actions.push(Action::Monitor(MonitorAction::Retry(row.id().to_owned())));
            }
            ui.add(Label::new(RichText::new(tr!("Connexion refusée : surveillance en pause")).size(11.5).color(p.danger)).truncate())
                .on_hover_text(error);
            true
        }
        State::Unreachable(error) => {
            let text = match row.snap.unreachable_for {
                Some(seconds) => tr!("Injoignable depuis {time}", time = ago(seconds, false)),
                None => tr!("Injoignable").to_owned(),
            };
            ui.add(Label::new(RichText::new(format!("{}  {text}", icon::WARNING_CIRCLE)).size(11.5).color(p.danger)).truncate())
                .on_hover_text(error);
            true
        }
        _ => false,
    }
}

/// Why a server shows no figures yet, when it does not.
pub(super) fn waiting(snap: &Snap) -> Option<String> {
    match &snap.state {
        State::Starting => Some(tr!("Démarrage de la surveillance…").to_owned()),
        State::Connecting => Some(tr!("Connexion…").to_owned()),
        State::Unsupported(Some(system)) => Some(tr!("Mesures non disponibles sur ce système ({system})", system)),
        State::Unsupported(None) => Some(tr!("Mesures non disponibles sur ce système").to_owned()),
        _ if !snap.has_figures() => Some(tr!("En attente d'une première mesure…").to_owned()),
        _ => None,
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

/// The latest alerts: all of them, or those of one server.
pub(super) fn journal(
    ui: &mut Ui,
    p: &Palette,
    dashboard: &Dashboard<'_>,
    only: Option<&str>,
    state: &mut DashState,
    actions: &mut Vec<Action>,
) {
    const SHOWN: usize = 60;
    let alerts: Vec<&Alert> = dashboard
        .monitor
        .journal
        .iter()
        .filter(|alert| only.is_none_or(|id| alert.profile_id == id))
        .take(SHOWN)
        .collect();
    ui.horizontal(|ui| {
        ui.label(RichText::new(tr!("ALERTES RÉCENTES")).size(11.0).color(p.faint));
        if only.is_none()
            && !alerts.is_empty()
            && ui.add(Button::new(RichText::new(tr!("Effacer")).size(11.5)).small().frame(false)).clicked()
        {
            actions.push(Action::Monitor(MonitorAction::ClearJournal));
        }
    });
    ui.add_space(4.0);
    if alerts.is_empty() {
        ui.label(RichText::new(tr!("Rien à signaler depuis l'ouverture de Farfile.")).size(12.0).color(p.muted));
        return;
    }
    Grid::new(("monitor-journal", only)).num_columns(4).spacing(vec2(12.0, 6.0)).show(ui, |ui| {
        for alert in alerts {
            let level = alert.level();
            ui.label(RichText::new(when(alert)).size(12.0).color(p.faint));
            ui.label(RichText::new(level_icon(level)).size(13.0).color(charts::level(p, level)));
            if only.is_none() {
                let name = ui.add(
                    Label::new(RichText::new(&alert.name).font(semibold(12.5)).color(p.text))
                        .sense(Sense::click())
                        .selectable(false),
                );
                if name.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                    state.detail = Some(alert.profile_id.clone());
                }
            } else {
                ui.label("");
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::alerts::Threshold;

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
        assert_eq!(charts::tint(&p, 0.5, Some(&disk)), p.success);
        assert_eq!(charts::tint(&p, 0.86, Some(&disk)), p.folder);
        assert_eq!(charts::tint(&p, 0.96, Some(&disk)), p.danger);
        let off = Threshold { enabled: false, ..disk };
        assert_eq!(charts::tint(&p, 0.96, Some(&off)), super::super::ssh_power::stats::level(&p, 0.96));
    }

    #[test]
    fn servers_come_the_most_worrying_first() {
        let profiles: Vec<Profile> = ["b", "a", "c", "d"]
            .iter()
            .map(|name| Profile {
                id: (*name).to_owned(),
                name: (*name).to_owned(),
                protocol: Protocol::Sftp,
                ..Profile::default()
            })
            .collect();
        let rules = Rules::default();
        let mut rows: Vec<Row<'_>> = profiles
            .iter()
            .enumerate()
            .map(|(index, profile)| Row {
                profile,
                host: None,
                rules: &rules,
                index,
                snap: Snap::default(),
            })
            .collect();
        rows[0].snap.state = State::Ready;
        rows[1].snap.worst = Some(Level::Warning);
        rows[2].snap.state = State::Paused("refusé".to_owned());
        rows[3].snap.state = State::Unreachable("coupé".to_owned());
        sort(&mut rows, Order::Severity);
        let names = |rows: &[Row<'_>]| rows.iter().map(|row| row.name().to_owned()).collect::<Vec<_>>();
        assert_eq!(names(&rows), ["c", "a", "d", "b"]);
        sort(&mut rows, Order::Name);
        assert_eq!(names(&rows), ["a", "b", "c", "d"]);
    }

    #[test]
    fn measures_read_and_scale_as_they_should() {
        assert_eq!(measure_text(Measure::Cpu, 0.46), "46 %");
        assert_eq!(measure_text(Measure::Load, 0.4412), "0,44");
        assert_eq!(measure_scale(Measure::Load, 0.3).max, 1.0);
        assert_eq!(measure_scale(Measure::Load, 2.6).max, 3.0);
        assert!(measure_scale(Measure::Network, 5e6).log);
        assert_eq!(measure_scale(Measure::Memory, 0.2).max, 1.0);
    }
}
