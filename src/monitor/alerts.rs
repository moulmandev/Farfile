//! When a watched server deserves attention: thresholds on its disks,
//! memory, processor, swap and load, and on how long it stays out of reach.
//!
//! A [`Tracker`] follows one server. Each reading goes through it, and it
//! reports the alerts that start, change level or end. Three things keep it
//! from crying wolf:
//!
//! - a threshold can ask to be crossed for some minutes before it counts,
//!   so that a burst of activity says nothing;
//! - an alert only ends once the figure is clearly back below its threshold
//!   ([`Threshold::margin`]), so that a disk hovering around 85 % does not
//!   raise and clear an alert at every reading;
//! - an alert is reported when it starts, gets worse or ends, never again
//!   while it lasts.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::ssh_power::stats::View;

/// How serious an alert is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Level {
    Warning,
    Critical,
}

/// What an alert is about.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Metric {
    /// A file system, by its mount point.
    Disk(String),
    Memory,
    Cpu,
    Swap,
    /// The load average over 5 minutes, per processor core.
    Load,
    /// The server could not be read.
    Offline,
}

impl Metric {
    /// Whether its figures are shares, in percent; the load is not.
    pub fn is_percent(&self) -> bool {
        !matches!(self, Self::Load | Self::Offline)
    }
}

/// When a figure is worth an alert.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Threshold {
    pub enabled: bool,
    pub warning: f32,
    pub critical: f32,
    /// How long the figure has to stay above before it counts; 0 counts at
    /// once.
    pub minutes: u32,
}

impl Default for Threshold {
    fn default() -> Self {
        Self::percent(true, 85.0, 95.0, 0)
    }
}

impl Threshold {
    const fn percent(enabled: bool, warning: f32, critical: f32, minutes: u32) -> Self {
        Self {
            enabled,
            warning,
            critical,
            minutes,
        }
    }

    /// How far below a threshold a figure has to come back for its alert to
    /// end: 5 points for a share, a tenth of the threshold for the load.
    pub fn margin(&self, metric: &Metric) -> f32 {
        if metric.is_percent() { 5.0 } else { (self.warning * 0.1).max(0.05) }
    }

    /// The level this figure calls for, `active` being the level of the
    /// alert already raised, if any.
    fn level(&self, metric: &Metric, value: f32, active: Option<Level>) -> Option<Level> {
        let margin = self.margin(metric);
        // A critical limit set below the warning one still works.
        let critical = self.critical.max(self.warning);
        if value >= critical || (active == Some(Level::Critical) && value >= critical - margin) {
            Some(Level::Critical)
        } else if value >= self.warning || (active.is_some() && value >= self.warning - margin) {
            Some(Level::Warning)
        } else {
            None
        }
    }

    /// The limit crossed to reach `level`.
    pub fn limit(&self, level: Level) -> f32 {
        match level {
            Level::Warning => self.warning,
            Level::Critical => self.critical.max(self.warning),
        }
    }
}

/// How long a server may stay out of reach.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Offline {
    pub enabled: bool,
    pub minutes: u32,
}

impl Default for Offline {
    fn default() -> Self {
        Self {
            enabled: true,
            minutes: 2,
        }
    }
}

/// The thresholds of one server, or of all of them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rules {
    /// Each file system on its own.
    pub disk: Threshold,
    pub memory: Threshold,
    pub cpu: Threshold,
    pub swap: Threshold,
    /// Per processor core: 2 means twice as much work waiting as there are
    /// cores to do it.
    pub load: Threshold,
    pub offline: Offline,
}

impl Default for Rules {
    fn default() -> Self {
        Self {
            disk: Threshold::percent(true, 85.0, 95.0, 0),
            memory: Threshold::percent(true, 85.0, 95.0, 5),
            cpu: Threshold::percent(true, 85.0, 95.0, 10),
            // Linux keeps pages in swap long after it needed them: a full
            // swap alone often means nothing.
            swap: Threshold::percent(false, 85.0, 95.0, 5),
            load: Threshold::percent(true, 2.0, 4.0, 10),
            offline: Offline::default(),
        }
    }
}

impl Rules {
    pub fn threshold(&self, metric: &Metric) -> Option<&Threshold> {
        match metric {
            Metric::Disk(_) => Some(&self.disk),
            Metric::Memory => Some(&self.memory),
            Metric::Cpu => Some(&self.cpu),
            Metric::Swap => Some(&self.swap),
            Metric::Load => Some(&self.load),
            Metric::Offline => None,
        }
    }

    fn enabled(&self, metric: &Metric) -> bool {
        match metric {
            Metric::Offline => self.offline.enabled,
            _ => self.threshold(metric).is_some_and(|threshold| threshold.enabled),
        }
    }
}

/// The figures of a reading that thresholds apply to: shares in percent,
/// the load per core.
pub fn figures(view: &View) -> Vec<(Metric, f32)> {
    let mut figures = Vec::new();
    let Some(sample) = &view.sample else {
        return figures;
    };
    for disk in &sample.disks {
        figures.push((Metric::Disk(disk.mount.clone()), disk.fraction() * 100.0));
    }
    if let Some(memory) = sample.memory.filter(|memory| memory.total > 0) {
        figures.push((Metric::Memory, memory.used() as f32 / memory.total as f32 * 100.0));
        if memory.swap_total > 0 {
            figures.push((Metric::Swap, memory.swap_used() as f32 / memory.swap_total as f32 * 100.0));
        }
    }
    if let Some(cpu) = view.cpu {
        figures.push((Metric::Cpu, cpu * 100.0));
    }
    if let (Some([_, five, _]), Some(cores)) = (sample.load, sample.cores.filter(|cores| *cores > 0)) {
        figures.push((Metric::Load, (five / f64::from(cores)) as f32));
    }
    figures
}

/// An alert that starts, changes level or ends.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    pub metric: Metric,
    pub from: Option<Level>,
    /// None when it ends.
    pub to: Option<Level>,
    /// The figure that triggered it; for [`Metric::Offline`], the minutes
    /// out of reach.
    pub value: f32,
}

impl Change {
    /// Worth telling the user: it starts or gets worse.
    pub fn is_worse(&self) -> bool {
        self.to > self.from
    }

    pub fn is_recovery(&self) -> bool {
        self.to.is_none()
    }
}

/// An alert under way.
#[derive(Clone, Debug, PartialEq)]
pub struct Active {
    pub level: Level,
    /// The last figure read.
    pub value: f32,
}

#[derive(Default)]
struct State {
    active: Option<Level>,
    /// Since when the figure calls for more than the active level, and for
    /// which level.
    rising: Option<(Level, Instant)>,
    value: f32,
}

/// The alerts of one server.
#[derive(Default)]
pub struct Tracker {
    states: BTreeMap<Metric, State>,
    unreachable_since: Option<Instant>,
}

impl Tracker {
    /// Takes in the figures of a reading that went through.
    pub fn reading(&mut self, rules: &Rules, figures: &[(Metric, f32)], now: Instant) -> Vec<Change> {
        let mut changes = Vec::new();
        if let Some(change) = self.reached(now) {
            changes.push(change);
        }
        for (metric, value) in figures {
            let Some(threshold) = rules.threshold(metric).filter(|threshold| threshold.enabled) else {
                continue;
            };
            let state = self.states.entry(metric.clone()).or_default();
            state.value = *value;
            let wanted = threshold.level(metric, *value, state.active);
            if wanted > state.active {
                let since = match state.rising {
                    Some((_, since)) => since,
                    None => now,
                };
                let wanted = wanted.unwrap_or(Level::Warning);
                state.rising = Some((wanted, since));
                if now.duration_since(since) >= minutes(threshold.minutes) {
                    changes.push(Change {
                        metric: metric.clone(),
                        from: state.active,
                        to: Some(wanted),
                        value: *value,
                    });
                    state.active = Some(wanted);
                    state.rising = None;
                }
            } else {
                state.rising = None;
                if wanted < state.active {
                    changes.push(Change {
                        metric: metric.clone(),
                        from: state.active,
                        to: wanted,
                        value: *value,
                    });
                    state.active = wanted;
                }
            }
        }
        self.forget_disabled(rules);
        self.states.retain(|_, state| state.active.is_some() || state.rising.is_some());
        changes
    }

    /// The server could not be read.
    pub fn unreachable(&mut self, rules: &Rules, now: Instant) -> Option<Change> {
        let since = *self.unreachable_since.get_or_insert(now);
        if !rules.offline.enabled || self.states.get(&Metric::Offline).is_some_and(|state| state.active.is_some()) {
            return None;
        }
        let out = now.duration_since(since);
        if out < minutes(rules.offline.minutes) {
            return None;
        }
        let value = out.as_secs_f32() / 60.0;
        self.states.insert(
            Metric::Offline,
            State {
                active: Some(Level::Critical),
                rising: None,
                value,
            },
        );
        Some(Change {
            metric: Metric::Offline,
            from: None,
            to: Some(Level::Critical),
            value,
        })
    }

    /// The server answers again: an alert about it being out of reach ends.
    fn reached(&mut self, now: Instant) -> Option<Change> {
        let since = self.unreachable_since.take()?;
        let state = self.states.remove(&Metric::Offline)?;
        Some(Change {
            metric: Metric::Offline,
            from: state.active,
            to: None,
            value: now.duration_since(since).as_secs_f32() / 60.0,
        })
    }

    /// Rules turned off since: their alerts end without a word.
    fn forget_disabled(&mut self, rules: &Rules) {
        self.states.retain(|metric, _| rules.enabled(metric));
    }

    /// The alerts under way, the most serious first.
    pub fn active(&self) -> Vec<(Metric, Active)> {
        let mut active: Vec<(Metric, Active)> = self
            .states
            .iter()
            .filter_map(|(metric, state)| {
                let level = state.active?;
                Some((
                    metric.clone(),
                    Active {
                        level,
                        value: state.value,
                    },
                ))
            })
            .collect();
        active.sort_by(|a, b| b.1.level.cmp(&a.1.level).then_with(|| a.0.cmp(&b.0)));
        active
    }

    /// Since when the server is out of reach, if it is.
    pub fn unreachable_since(&self) -> Option<Instant> {
        self.unreachable_since
    }
}

fn minutes(minutes: u32) -> Duration {
    Duration::from_secs(u64::from(minutes) * 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk(value: f32) -> Vec<(Metric, f32)> {
        vec![(Metric::Disk("/".to_owned()), value)]
    }

    fn levels(changes: &[Change]) -> Vec<(Option<Level>, Option<Level>)> {
        changes.iter().map(|change| (change.from, change.to)).collect()
    }

    #[test]
    fn a_filling_disk_warns_then_alarms_then_recovers() {
        let rules = Rules::default();
        let mut tracker = Tracker::default();
        let start = Instant::now();
        assert!(tracker.reading(&rules, &disk(70.0), start).is_empty());
        let changes = tracker.reading(&rules, &disk(86.0), start);
        assert_eq!(levels(&changes), [(None, Some(Level::Warning))]);
        assert!(changes[0].is_worse());
        // Said once, not at every reading.
        assert!(tracker.reading(&rules, &disk(88.0), start).is_empty());
        let changes = tracker.reading(&rules, &disk(96.0), start);
        assert_eq!(levels(&changes), [(Some(Level::Warning), Some(Level::Critical))]);
        // Just below the critical limit: still critical, within the margin.
        assert!(tracker.reading(&rules, &disk(92.0), start).is_empty());
        let changes = tracker.reading(&rules, &disk(89.0), start);
        assert_eq!(levels(&changes), [(Some(Level::Critical), Some(Level::Warning))]);
        assert!(!changes[0].is_worse() && !changes[0].is_recovery());
        assert_eq!(tracker.active()[0].1.level, Level::Warning);
        let changes = tracker.reading(&rules, &disk(70.0), start);
        assert_eq!(levels(&changes), [(Some(Level::Warning), None)]);
        assert!(changes[0].is_recovery());
        assert!(tracker.active().is_empty());
    }

    #[test]
    fn a_figure_hovering_around_its_threshold_does_not_flap() {
        let rules = Rules::default();
        let mut tracker = Tracker::default();
        let now = Instant::now();
        assert_eq!(tracker.reading(&rules, &disk(85.2), now).len(), 1);
        for value in [84.0, 85.5, 83.1, 84.9, 80.5] {
            assert!(tracker.reading(&rules, &disk(value), now).is_empty(), "{value}");
        }
        assert_eq!(levels(&tracker.reading(&rules, &disk(79.9), now)), [(Some(Level::Warning), None)]);
    }

    #[test]
    fn sustained_thresholds_wait_before_counting() {
        let rules = Rules::default();
        let mut tracker = Tracker::default();
        let start = Instant::now();
        let cpu = |value| vec![(Metric::Cpu, value)];
        // Ten minutes of a busy processor are needed.
        assert!(tracker.reading(&rules, &cpu(99.0), start).is_empty());
        assert!(tracker.reading(&rules, &cpu(97.0), start + Duration::from_secs(300)).is_empty());
        // A dip starts the count again.
        assert!(tracker.reading(&rules, &cpu(40.0), start + Duration::from_secs(330)).is_empty());
        let restart = start + Duration::from_secs(360);
        assert!(tracker.reading(&rules, &cpu(90.0), restart).is_empty());
        assert!(tracker.reading(&rules, &cpu(91.0), restart + Duration::from_secs(599)).is_empty());
        let changes = tracker.reading(&rules, &cpu(99.0), restart + Duration::from_secs(600));
        // Busy all along, critical now: straight to critical.
        assert_eq!(levels(&changes), [(None, Some(Level::Critical))]);
    }

    #[test]
    fn every_disk_counts_on_its_own() {
        let rules = Rules::default();
        let mut tracker = Tracker::default();
        let now = Instant::now();
        let figures = vec![(Metric::Disk("/".to_owned()), 50.0), (Metric::Disk("/srv".to_owned()), 97.0)];
        let changes = tracker.reading(&rules, &figures, now);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].metric, Metric::Disk("/srv".to_owned()));
    }

    #[test]
    fn a_server_out_of_reach_alerts_after_a_while_and_says_when_it_is_back() {
        let rules = Rules::default();
        let mut tracker = Tracker::default();
        let start = Instant::now();
        assert!(tracker.unreachable(&rules, start).is_none());
        assert!(tracker.unreachable(&rules, start + Duration::from_secs(60)).is_none());
        let change = tracker.unreachable(&rules, start + Duration::from_secs(120)).unwrap();
        assert_eq!((change.metric.clone(), change.to), (Metric::Offline, Some(Level::Critical)));
        // Once.
        assert!(tracker.unreachable(&rules, start + Duration::from_secs(150)).is_none());
        assert_eq!(tracker.active()[0].0, Metric::Offline);
        let changes = tracker.reading(&rules, &disk(10.0), start + Duration::from_secs(180));
        assert_eq!(levels(&changes), [(Some(Level::Critical), None)]);
        assert_eq!(changes[0].value, 3.0);

        // A short cut says nothing at all.
        assert!(tracker.unreachable(&rules, start + Duration::from_secs(200)).is_none());
        assert!(tracker.reading(&rules, &disk(10.0), start + Duration::from_secs(230)).is_empty());
    }

    #[test]
    fn rules_turned_off_end_their_alerts_quietly() {
        let mut rules = Rules::default();
        let mut tracker = Tracker::default();
        let now = Instant::now();
        assert_eq!(tracker.reading(&rules, &disk(99.0), now).len(), 1);
        rules.disk.enabled = false;
        assert!(tracker.reading(&rules, &disk(99.0), now).is_empty());
        assert!(tracker.active().is_empty());
        // Swap is off by default.
        assert!(Rules::default().threshold(&Metric::Swap).is_some_and(|swap| !swap.enabled));
        assert!(tracker.reading(&Rules::default(), &[(Metric::Swap, 100.0)], now).is_empty());
    }

    #[test]
    fn the_load_is_counted_per_core() {
        use crate::ssh_power::stats::{Disk, Memory, Sample};
        let view = View {
            sample: Some(Sample {
                cores: Some(4),
                load: Some([12.0, 10.0, 9.0]),
                memory: Some(Memory {
                    total: 1000,
                    available: 100,
                    swap_total: 0,
                    swap_free: 0,
                }),
                disks: vec![Disk {
                    mount: "/".to_owned(),
                    size: 100,
                    used: 30,
                    available: 70,
                    ..Disk::default()
                }],
                ..Sample::default()
            }),
            cpu: Some(0.5),
            ..View::default()
        };
        let figures = figures(&view);
        let figure = |wanted: Metric| figures.iter().find(|(metric, _)| *metric == wanted).map(|(_, value)| *value);
        let close = |value: Option<f32>, expected: f32| value.is_some_and(|value| (value - expected).abs() < 1e-3);
        assert!(close(figure(Metric::Load), 2.5));
        assert!(close(figure(Metric::Memory), 90.0));
        assert!(close(figure(Metric::Cpu), 50.0));
        assert!(close(figure(Metric::Disk("/".to_owned())), 30.0));
        // No swap, no figure for it.
        assert_eq!(figure(Metric::Swap), None);
        let load = Rules::default().load;
        assert_eq!(load.margin(&Metric::Load), 0.2);
    }

    #[test]
    fn saved_rules_survive_missing_fields() {
        let rules: Rules = serde_json::from_str(r#"{"disk":{"warning":80.0}}"#).unwrap();
        assert_eq!(rules.disk.warning, 80.0);
        assert!(rules.disk.enabled);
        assert_eq!(rules.memory, Rules::default().memory);
    }
}
