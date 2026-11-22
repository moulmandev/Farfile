//! Watching several SSH servers at once, for the dashboard and its alerts.
//!
//! Each watched connection gets a task of its own, which keeps one SSH
//! connection open and runs the script of the server overview on it (see
//! [`crate::ssh_power::stats`]): every few seconds while the dashboard is on
//! screen, every [`MonitorSettings::interval`] seconds otherwise. Unlike the
//! overview above a terminal, it never stops on its own: alerts have to come
//! while the dashboard is hidden, and even while the window is closed to the
//! notification area (see [`tray`]).
//!
//! A server out of reach is tried again and again, since that costs it
//! nothing. A login it refuses is not: the watch then pauses until the user
//! asks for another try, so that a wrong password never adds up to a ban
//! (fail2ban and the like).

pub mod alerts;
pub mod dashboard;
pub mod history;
pub mod notify;
#[cfg(windows)]
pub mod tray;

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::anyhow;
use russh::Disconnect;
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, oneshot};

use crate::ssh_power::stats::{self, Sample, Status, View};
use crate::storage::ssh::{self, SshHandle, SshSettings};
use crate::worker::Worker;
use alerts::{Active, Change, Level, Metric, Rules, Tracker};

/// The id of the dashboard's tab, where a connection's would be.
pub const ID: &str = "monitor";

pub fn is_dashboard(id: &str) -> bool {
    id == ID
}

/// Between two readings while the dashboard is on screen.
pub const FAST_INTERVAL: Duration = Duration::from_secs(3);
/// The second reading comes sooner: the processor and network figures
/// need two readings to compare.
const FIRST_INTERVAL: Duration = Duration::from_millis(1200);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
/// After a server could not be reached, then twice as long after each
/// other failure, up to [`MAX_RETRY`]: no login is tried, so trying often
/// harms nobody, and tells soon enough when the server is back.
const RETRY: Duration = Duration::from_secs(10);
const MAX_RETRY: Duration = Duration::from_secs(60);
/// A server the script does not understand will not start to.
const UNSUPPORTED_INTERVAL: Duration = Duration::from_secs(300);
/// A paused watch waits for the user.
const PAUSED: Duration = Duration::from_secs(24 * 3600);
/// Between two savings of the history of a server.
const SAVE_EVERY: Duration = Duration::from_secs(5 * 60);
/// Alerts kept in the journal of the dashboard.
const JOURNAL: usize = 200;
/// The dashboard counts as shown this long after a frame drew it.
const SHOWN_FOR: Duration = Duration::from_millis(1500);

/// How alerts reach the user.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Delivery {
    /// A message in a corner of Farfile's window.
    pub in_app: bool,
    /// A notification of the system, which shows even when Farfile is
    /// hidden.
    pub system: bool,
    /// The taskbar button flashes until the window is brought up.
    pub flash: bool,
    /// Also tell when an alert ends.
    pub recoveries: bool,
}

impl Default for Delivery {
    fn default() -> Self {
        Self {
            in_app: true,
            system: true,
            flash: true,
            recoveries: true,
        }
    }
}

/// The preferences of the monitoring, kept with the others.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MonitorSettings {
    /// The connections watched, by id, in the order they were added.
    pub watched: Vec<String>,
    /// The thresholds of every server…
    pub rules: Rules,
    /// …but these, which have their own.
    pub overrides: BTreeMap<String, Rules>,
    pub delivery: Delivery,
    /// Closing the window keeps watching, from the notification area,
    /// while some servers are watched.
    pub background: bool,
    /// Seconds between two readings while the dashboard is not on screen.
    pub interval: u32,
    /// The view of the dashboard and what its cards show.
    pub dashboard: dashboard::DashboardPrefs,
}

impl Default for MonitorSettings {
    fn default() -> Self {
        Self {
            watched: Vec::new(),
            rules: Rules::default(),
            overrides: BTreeMap::new(),
            delivery: Delivery::default(),
            background: false,
            interval: 30,
            dashboard: dashboard::DashboardPrefs::default(),
        }
    }
}

/// The choices offered for [`MonitorSettings::interval`].
pub const INTERVALS: [u32; 6] = [10, 15, 30, 60, 120, 300];

impl MonitorSettings {
    pub fn rules_for(&self, id: &str) -> &Rules {
        self.overrides.get(id).unwrap_or(&self.rules)
    }

    pub fn is_watched(&self, id: &str) -> bool {
        self.watched.iter().any(|watched| watched == id)
    }

    pub fn set_watched(&mut self, id: &str, watched: bool) {
        if watched && !self.is_watched(id) {
            self.watched.push(id.to_owned());
        } else if !watched {
            self.watched.retain(|known| known != id);
        }
    }

    fn background_interval(&self) -> Duration {
        Duration::from_secs(u64::from(self.interval.clamp(5, 3600)))
    }
}

/// What a watched server looks like now.
#[derive(Default)]
pub struct HostState {
    pub view: View,
    /// What it did: the last half hour, day and week.
    pub history: history::History,
    /// The alerts under way, the most serious first.
    pub alerts: Vec<(Metric, Active)>,
    /// When it was last read.
    pub read_at: Option<Instant>,
    /// Since when it is out of reach.
    pub unreachable_since: Option<Instant>,
    /// The login was refused: the watch waits for the user, with why.
    pub paused: Option<String>,
}

impl HostState {
    /// The most serious alert under way; a watch paused after a refused
    /// login is as bad as it gets, since nothing is watched any more.
    pub fn worst(&self) -> Option<Level> {
        if self.paused.is_some() {
            return Some(Level::Critical);
        }
        self.alerts.iter().map(|(_, active)| active.level).max()
    }
}

/// One watched server, as its task and the dashboard share it.
pub struct Host {
    pub id: String,
    name: Mutex<String>,
    state: Mutex<HostState>,
    rules: Mutex<Rules>,
}

impl Host {
    pub fn name(&self) -> String {
        lock(&self.name).clone()
    }

    /// What it looks like now; the lock is held while `read` runs.
    pub fn read<R>(&self, read: impl FnOnce(&HostState) -> R) -> R {
        read(&lock(&self.state))
    }

    fn rules(&self) -> Rules {
        lock(&self.rules).clone()
    }
}

/// A poisoned lock only means a task panicked while holding it: what it
/// guards is still worth showing.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What happened to an alert of a server.
#[derive(Clone, Debug, PartialEq)]
pub enum Happening {
    Changed(Change),
    /// The server refused the login: its watch is paused.
    Refused(String),
}

/// An entry of the journal of the dashboard.
#[derive(Clone, Debug)]
pub struct Alert {
    pub profile_id: String,
    pub name: String,
    pub happening: Happening,
    pub at: chrono::DateTime<chrono::Local>,
    /// Told the user, rather than only written in the journal.
    pub told: bool,
}

impl Alert {
    /// How serious it is: none for good news.
    pub fn level(&self) -> Option<Level> {
        match &self.happening {
            Happening::Changed(change) => change.to,
            Happening::Refused(_) => Some(Level::Critical),
        }
    }

    /// One line about it, without the server's name.
    pub fn text(&self) -> String {
        match &self.happening {
            Happening::Changed(change) => change_text(change),
            Happening::Refused(error) => tr!("Surveillance en pause, connexion refusée : {error}", error),
        }
    }
}

/// `85 %`, or `2,5` for the load.
pub fn value_text(metric: &Metric, value: f32) -> String {
    if metric.is_percent() {
        stats::percent(value / 100.0)
    } else {
        stats::decimal(f64::from(value), 1)
    }
}

/// What the alert is about, in a few words: `Disque /`, `Mémoire`.
pub fn metric_name(metric: &Metric) -> String {
    match metric {
        Metric::Disk(mount) => tr!("Disque {mount}", mount),
        Metric::Memory => tr!("Mémoire").to_owned(),
        Metric::Cpu => tr!("Processeur").to_owned(),
        Metric::Swap => "Swap".to_owned(),
        Metric::Load => tr!("Charge").to_owned(),
        Metric::Offline => tr!("Injoignable").to_owned(),
    }
}

/// The sentence of a change: what crossed which line, or came back.
fn change_text(change: &Change) -> String {
    let value = value_text(&change.metric, change.value);
    let minutes = change.value.round().max(1.0) as u64;
    if change.to.is_none() {
        return match &change.metric {
            Metric::Disk(mount) => tr!("Disque {mount} revenu à {value}", mount, value),
            Metric::Memory => tr!("Mémoire revenue à {value}", value),
            Metric::Cpu => tr!("Processeur revenu à {value}", value),
            Metric::Swap => tr!("Swap revenu à {value}", value),
            Metric::Load => tr!("Charge revenue à {value} par cœur", value),
            Metric::Offline => trn!(minutes, "Répond à nouveau, après {n} minute", "Répond à nouveau, après {n} minutes"),
        };
    }
    let level = match change.to {
        Some(Level::Critical) => tr!("Critique"),
        _ => tr!("Attention"),
    };
    let what = match &change.metric {
        Metric::Disk(mount) => tr!("disque {mount} plein à {value}", mount, value),
        Metric::Memory => tr!("mémoire utilisée à {value}", value),
        Metric::Cpu => tr!("processeur occupé à {value}", value),
        Metric::Swap => tr!("swap utilisé à {value}", value),
        Metric::Load => tr!("charge de {value} par cœur", value),
        Metric::Offline => trn!(minutes, "ne répond plus depuis {n} minute", "ne répond plus depuis {n} minutes"),
    };
    tr!("{level} : {what}", level, what)
}

/// What all the tasks share with the app.
struct Hub {
    start: Instant,
    /// When a frame last drew the dashboard, in milliseconds since `start`
    /// (0: never).
    shown_at: AtomicU64,
    /// Rings when the dashboard comes on screen: every server is read at
    /// once.
    wake: Notify,
    settings: Mutex<HubSettings>,
    events: Mutex<Sender<Alert>>,
    ctx: egui::Context,
}

struct HubSettings {
    delivery: Delivery,
    interval: Duration,
}

impl Hub {
    fn shown(&self) -> bool {
        let at = self.shown_at.load(Ordering::Relaxed);
        at > 0 && self.start.elapsed().as_millis() as u64 <= at + SHOWN_FOR.as_millis() as u64
    }

    fn interval(&self) -> Duration {
        if self.shown() { FAST_INTERVAL } else { lock(&self.settings).interval }
    }

    /// Writes the journal, and tells the user when it is worth it.
    fn report(&self, host: &Host, happening: Happening) {
        let delivery = lock(&self.settings).delivery.clone();
        let told = match &happening {
            Happening::Changed(change) => change.is_worse() || (change.is_recovery() && delivery.recoveries),
            Happening::Refused(_) => true,
        };
        let alert = Alert {
            profile_id: host.id.clone(),
            name: host.name(),
            happening,
            at: chrono::Local::now(),
            told,
        };
        if told {
            let critical = alert.level() == Some(Level::Critical);
            if delivery.system {
                notify::show(&alert.name, &alert.text(), critical);
            }
            if delivery.flash && alert.level().is_some() {
                notify::flash();
            }
        }
        let _ = lock(&self.events).send(alert);
        self.ctx.request_repaint();
    }
}

/// A watch under way. Dropping it stops its task and closes its
/// connection.
struct Watch {
    host: Arc<Host>,
    /// What the connection was opened with: a change starts it again.
    fingerprint: String,
    _stop: oneshot::Sender<()>,
}

/// The watched servers, and what they said.
pub struct Monitor {
    hub: Arc<Hub>,
    watches: BTreeMap<String, Watch>,
    events: Receiver<Alert>,
    /// The latest alerts first.
    pub journal: VecDeque<Alert>,
}

impl Monitor {
    pub fn new(ctx: &egui::Context, settings: &MonitorSettings) -> Self {
        let (sender, events) = channel();
        notify::init(ctx);
        Self {
            hub: Arc::new(Hub {
                start: Instant::now(),
                shown_at: AtomicU64::new(0),
                wake: Notify::new(),
                settings: Mutex::new(HubSettings {
                    delivery: settings.delivery.clone(),
                    interval: settings.background_interval(),
                }),
                events: Mutex::new(sender),
                ctx: ctx.clone(),
            }),
            watches: BTreeMap::new(),
            events,
            journal: VecDeque::new(),
        }
    }

    /// Goes on watching this connection with this name and these rules,
    /// when it is watched already with the same `fingerprint` (what its
    /// connection is made of). False when it has to be [`started`] first.
    ///
    /// [`started`]: Monitor::start
    pub fn refresh(&self, id: &str, name: &str, fingerprint: &str, rules: &Rules) -> bool {
        let Some(watch) = self.watches.get(id).filter(|watch| watch.fingerprint == fingerprint) else {
            return false;
        };
        let mut known = lock(&watch.host.name);
        if *known != name {
            *known = name.to_owned();
        }
        drop(known);
        let mut known = lock(&watch.host.rules);
        if *known != *rules {
            *known = rules.clone();
        }
        true
    }

    /// Starts watching this connection, in place of the watch it had.
    pub fn start(
        &mut self,
        worker: &Worker,
        id: &str,
        name: &str,
        fingerprint: String,
        rules: &Rules,
        settings: Arc<SshSettings>,
    ) {
        let host = Arc::new(Host {
            id: id.to_owned(),
            name: Mutex::new(name.to_owned()),
            state: Mutex::new(HostState::default()),
            rules: Mutex::new(rules.clone()),
        });
        let (stop, stopped) = oneshot::channel();
        worker.spawn(watch(settings, host.clone(), self.hub.clone(), stopped));
        self.watches.insert(
            id.to_owned(),
            Watch {
                host,
                fingerprint,
                _stop: stop,
            },
        );
    }

    /// Stops watching what is not in `keep`.
    pub fn keep_only(&mut self, keep: &[String]) {
        self.watches.retain(|id, _| keep.contains(id));
    }

    /// Stops this watch, so that the next [`Monitor::watch`] starts it
    /// afresh: after a refused login, or a change of password.
    pub fn restart(&mut self, id: &str) {
        self.watches.remove(id);
    }

    /// The delivery and pace chosen in the preferences.
    pub fn apply(&self, settings: &MonitorSettings) {
        let mut shared = lock(&self.hub.settings);
        shared.delivery = settings.delivery.clone();
        let interval = settings.background_interval();
        if shared.interval != interval {
            shared.interval = interval;
            drop(shared);
            // The new pace applies from the next reading on.
            self.hub.wake.notify_waiters();
        }
    }

    /// A frame drew the dashboard: read faster, and at once when it just
    /// came on screen.
    pub fn mark_seen(&self) {
        let was_shown = self.hub.shown();
        let now = self.hub.start.elapsed().as_millis().max(1) as u64;
        self.hub.shown_at.store(now, Ordering::Relaxed);
        if !was_shown {
            self.hub.wake.notify_waiters();
        }
    }

    /// The watched servers, by connection id.
    pub fn host(&self, id: &str) -> Option<&Arc<Host>> {
        self.watches.get(id).map(|watch| &watch.host)
    }

    pub fn is_empty(&self) -> bool {
        self.watches.is_empty()
    }

    /// The alerts that came since the last call, oldest first; they go to
    /// the journal too.
    pub fn take_alerts(&mut self) -> Vec<Alert> {
        let fresh: Vec<Alert> = self.events.try_iter().collect();
        for alert in &fresh {
            self.journal.push_front(alert.clone());
        }
        self.journal.truncate(JOURNAL);
        fresh
    }

    /// The servers with an alert under way: critical ones, then warnings.
    pub fn counts(&self) -> (usize, usize) {
        let mut counts = (0, 0);
        for watch in self.watches.values() {
            match watch.host.read(HostState::worst) {
                Some(Level::Critical) => counts.0 += 1,
                Some(Level::Warning) => counts.1 += 1,
                None => {}
            }
        }
        counts
    }

    /// Scripts only: a made-up week behind this server, to look at the
    /// longer spans of the charts without waiting for them.
    pub fn simulate_history(&self, id: &str, seed: u64) {
        if let Some(watch) = self.watches.get(id) {
            let mut state = lock(&watch.host.state);
            let disks = state.view.sample.as_ref().map(|sample| sample.disks.clone()).unwrap_or_default();
            state.history = history::History::default();
            state.history.simulate(history::now(), seed, &disks);
        }
    }

    /// Scripts only: an alert as a server would raise it, to see how it is
    /// told without waiting for a disk to fill.
    pub fn simulate(&self, id: &str, change: Change) {
        if let Some(watch) = self.watches.get(id) {
            let mut state = lock(&watch.host.state);
            state.alerts.retain(|(metric, _)| *metric != change.metric);
            if let Some(level) = change.to {
                state.alerts.push((
                    change.metric.clone(),
                    Active {
                        level,
                        value: change.value,
                    },
                ));
                state.alerts.sort_by_key(|alert| std::cmp::Reverse(alert.1.level));
            }
            drop(state);
            self.hub.report(&watch.host, Happening::Changed(change));
        }
    }
}

/// Why a reading failed.
enum Failure {
    /// The server could not be reached, or stopped answering.
    Unreachable(anyhow::Error),
    /// The server answered but refused the login.
    Refused(anyhow::Error),
}

/// Reads one server until its watch is dropped.
async fn watch(settings: Arc<SshSettings>, host: Arc<Host>, hub: Arc<Hub>, mut stop: oneshot::Receiver<()>) {
    let mut connection: Option<SshHandle> = None;
    let mut previous: Option<(Sample, Instant)> = None;
    let mut tracker = Tracker::default();
    let mut failures = 0u32;
    let mut delay = Duration::ZERO;
    let id = host.id.clone();
    let loaded = tokio::task::spawn_blocking(move || history::load(&id, history::now())).await.unwrap_or_default();
    lock(&host.state).history = loaded;
    let mut saved_at = Instant::now();
    loop {
        tokio::select! {
            _ = &mut stop => break,
            _ = tokio::time::sleep(delay) => {}
            _ = hub.wake.notified() => {}
        }
        if lock(&host.state).paused.is_some() {
            delay = PAUSED;
            continue;
        }

        let rules = host.rules();
        let reading = read(&settings, &mut connection).await;
        let now = Instant::now();
        // The lock ends with this block, before anything is awaited.
        let happenings = {
            let mut state = lock(&host.state);
            let mut happenings = Vec::new();
            match reading {
                Ok(output) => {
                    failures = 0;
                    let sample = stats::parse(&output);
                    state.view.update(sample.clone(), previous.as_ref(), now);
                    state.read_at = Some(now);
                    if matches!(state.view.status, Status::Unsupported(_)) {
                        tracker = Tracker::default();
                        delay = UNSUPPORTED_INTERVAL;
                    } else {
                        let figures = alerts::figures(&state.view);
                        happenings.extend(tracker.reading(&rules, &figures, now).into_iter().map(Happening::Changed));
                        remember(&mut state);
                        delay = if previous.is_none() { FIRST_INTERVAL } else { hub.interval() };
                    }
                    previous = Some((sample, now));
                }
                Err(Failure::Unreachable(error)) => {
                    failures += 1;
                    previous = None;
                    state.view.status = Status::Failed(format!("{error:#}"));
                    happenings.extend(tracker.unreachable(&rules, now).map(Happening::Changed));
                    let backoff = RETRY.saturating_mul(2u32.saturating_pow(failures.saturating_sub(1).min(8)));
                    delay = backoff.min(MAX_RETRY).min(hub.interval().max(RETRY));
                }
                Err(Failure::Refused(error)) => {
                    previous = None;
                    let text = format!("{error:#}");
                    state.view.status = Status::Failed(text.clone());
                    state.paused = Some(text.clone());
                    happenings.push(Happening::Refused(text));
                    delay = PAUSED;
                }
            }
            if connection.as_ref().is_some_and(|handle| handle.is_closed()) {
                connection = None;
            }
            state.alerts = tracker.active();
            state.unreachable_since = tracker.unreachable_since();
            happenings
        };
        for happening in happenings {
            hub.report(&host, happening);
        }
        hub.ctx.request_repaint();
        if saved_at.elapsed() >= SAVE_EVERY {
            saved_at = Instant::now();
            save(&host).await;
        }
    }
    save(&host).await;
    if let Some(handle) = connection {
        let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
    }
}

/// Writes the history of a server, off the threads of the tasks. Not worth
/// a word when it fails: the next time tries again.
async fn save(host: &Host) {
    let Some(bytes) = history::serialize(&lock(&host.state).history) else {
        return;
    };
    let id = host.id.clone();
    let _ = tokio::task::spawn_blocking(move || history::write(&id, &bytes)).await;
}

/// Adds the figures just read to the history.
fn remember(state: &mut HostState) {
    let Some(sample) = state.view.sample.as_ref() else {
        return;
    };
    let memory = sample.memory.filter(|memory| memory.total > 0);
    let load = match (sample.load, sample.cores.filter(|cores| *cores > 0)) {
        (Some([_, five, _]), Some(cores)) => Some((five / f64::from(cores)) as f32),
        _ => None,
    };
    let point = history::Point {
        at: history::now(),
        cpu: state.view.cpu,
        memory: memory.map(|memory| memory.used() as f32 / memory.total as f32),
        disk: stats::fullest(&sample.disks).map(|disk| disk.fraction()),
        load,
        rx: state.view.network.map(|(received, _)| received as f32),
        tx: state.view.network.map(|(_, sent)| sent as f32),
    };
    let disks = sample.disks.clone();
    state.history.add(point, &disks);
}

/// Runs the script, connecting and logging in first when needed.
async fn read(settings: &SshSettings, connection: &mut Option<SshHandle>) -> Result<String, Failure> {
    if connection.as_ref().is_none_or(|handle| handle.is_closed()) {
        *connection = None;
        // A connection that cannot be built is not a refusal of the server,
        // but no more worth trying again by itself.
        if let Some(problem) = &settings.extra.problem {
            return Err(Failure::Refused(anyhow!("{problem}")));
        }
        let mut handle = tokio::time::timeout(CONNECT_TIMEOUT, ssh::open(settings))
            .await
            .map_err(|_| Failure::Unreachable(anyhow!(tr!("le serveur {host} ne répond pas", host = settings.host))))?
            .map_err(Failure::Unreachable)?;
        crate::ssh_auth::authenticate(&mut handle, settings).await.map_err(Failure::Refused)?;
        *connection = Some(handle);
    }
    let Some(handle) = connection.as_ref() else {
        return Err(Failure::Unreachable(anyhow!(tr!("connexion perdue"))));
    };
    let output = tokio::time::timeout(COMMAND_TIMEOUT, ssh::run_command(handle, stats::COMMAND))
        .await
        .map_err(|_| Failure::Unreachable(anyhow!(tr!("le serveur ne répond plus"))))?;
    match output {
        Ok(output) => Ok(String::from_utf8_lossy(&output.stdout).into_owned()),
        Err(error) => {
            *connection = None;
            Err(Failure::Unreachable(error))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_written_before_the_monitoring_still_load() {
        let settings: MonitorSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings, MonitorSettings::default());
        assert!(!settings.background);
        assert_eq!(settings.interval, 30);
        assert!(settings.delivery.system && settings.delivery.in_app);

        let mut settings = MonitorSettings::default();
        settings.set_watched("a", true);
        settings.set_watched("a", true);
        settings.set_watched("b", true);
        assert_eq!(settings.watched, ["a", "b"]);
        settings.set_watched("a", false);
        assert_eq!(settings.watched, ["b"]);

        let mut own = Rules::default();
        own.disk.warning = 92.0;
        settings.overrides.insert("b".to_owned(), own);
        assert_eq!(settings.rules_for("b").disk.warning, 92.0);
        assert_eq!(settings.rules_for("c").disk.warning, 85.0);
        let back: MonitorSettings = serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!(back, settings);
    }

    #[test]
    fn alerts_read_as_sentences() {
        let change = |metric, from, to, value| Change { metric, from, to, value };
        let disk = Metric::Disk("/srv".to_owned());
        let text = change_text(&change(disk.clone(), None, Some(Level::Warning), 87.4));
        assert_eq!(text, "Attention : disque /srv plein à 87 %");
        let text = change_text(&change(disk, Some(Level::Warning), None, 70.0));
        assert_eq!(text, "Disque /srv revenu à 70 %");
        let text = change_text(&change(Metric::Load, None, Some(Level::Critical), 4.31));
        assert_eq!(text, "Critique : charge de 4,3 par cœur");
        let text = change_text(&change(Metric::Offline, None, Some(Level::Critical), 2.0));
        assert_eq!(text, "Critique : ne répond plus depuis 2 minutes");
        let text = change_text(&change(Metric::Offline, Some(Level::Critical), None, 0.4));
        assert_eq!(text, "Répond à nouveau, après 1 minute");
    }

    /// The connection settings of `S3V_TEST_SFTP` (`user:password@host:port`),
    /// as in `storage::live_tests`.
    fn live_server() -> Option<SshSettings> {
        let value = std::env::var("S3V_TEST_SFTP").ok().filter(|value| !value.is_empty())?;
        let (credentials, address) = value.rsplit_once('@')?;
        let (user, secret) = credentials.split_once(':')?;
        let (host, port) = address.rsplit_once(':')?;
        Some(SshSettings {
            extra: Default::default(),
            host: host.to_owned(),
            port: port.parse().ok()?,
            user: user.to_owned(),
            secret: secret.to_owned(),
            key_path: None,
            known_hosts: None,
        })
    }

    /// Against a real server: the watch reads it, raises an alert on a
    /// threshold every disk crosses, and pauses on a refused password
    /// without trying it again.
    #[test]
    fn a_live_server_is_watched() {
        let Some(settings) = live_server() else {
            eprintln!("S3V_TEST_SFTP not set: skipped");
            return;
        };
        let ctx = egui::Context::default();
        let (worker, _events) = Worker::new(ctx.clone());
        let mut monitor = Monitor::new(&ctx, &MonitorSettings::default());
        let mut rules = Rules::default();
        rules.disk.warning = 0.0;
        rules.disk.critical = 101.0;
        monitor.start(&worker, "good", "Bon", "1".to_owned(), &rules, Arc::new(settings));
        assert!(monitor.refresh("good", "Bon", "1", &rules));
        assert!(!monitor.refresh("good", "Bon", "2", &rules), "another connection");
        let mut wrong = live_server().unwrap();
        wrong.secret = "not-the-password".to_owned();
        monitor.start(&worker, "wrong", "Mauvais", "1".to_owned(), &rules, Arc::new(wrong));

        let deadline = Instant::now() + Duration::from_secs(30);
        let mut alerts = Vec::new();
        loop {
            monitor.mark_seen();
            alerts.extend(monitor.take_alerts());
            let good = monitor.host("good").unwrap().read(|state| state.view.cpu.is_some() && state.history.recent().len() >= 2);
            let paused = monitor.host("wrong").unwrap().read(|state| state.paused.is_some());
            if (good && paused) || Instant::now() > deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let good = monitor.host("good").unwrap();
        good.read(|state| {
            assert_eq!(state.view.status, Status::Ready);
            assert!(state.view.cpu.is_some(), "two readings compared");
            assert!(state.history.recent().len() >= 2);
            let last = state.history.recent().back().unwrap();
            assert!(last.cpu.is_some() && last.memory.is_some() && last.disk.is_some() && last.rx.is_some());
            assert!(matches!(state.alerts.first(), Some((Metric::Disk(_), Active { level: Level::Warning, .. }))));
        });
        assert!(alerts.iter().any(|alert| alert.profile_id == "good" && alert.told), "{alerts:?}");
        let refused = alerts.iter().find(|alert| alert.profile_id == "wrong").expect("refusal reported");
        assert!(matches!(refused.happening, Happening::Refused(_)));
        assert_eq!(alerts.iter().filter(|alert| alert.profile_id == "wrong").count(), 1, "tried once");
        assert_eq!(monitor.counts(), (1, 1));
    }
}
