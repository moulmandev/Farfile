//! What a watched server did, kept at three resolutions so that the
//! dashboard can draw the last half hour reading by reading, the last day
//! minute by minute and the last week ten minutes at a time; and how full
//! its disks were, every quarter of an hour, to tell when one will be full.
//!
//! It is saved next to the other local data of the app (see [`write`]), so
//! that the curves survive a restart. Only what Farfile saw is there, and
//! what the collector of the server noted meanwhile when it has one (see
//! [`super::collector`]): otherwise hours it was closed leave a gap, which
//! the charts draw as one.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::ssh_power::stats::Disk;

/// The spans of time the dashboard offers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Range {
    #[default]
    #[serde(rename = "30m")]
    Minutes30,
    #[serde(rename = "6h")]
    Hours6,
    #[serde(rename = "24h")]
    Hours24,
    #[serde(rename = "7d")]
    Days7,
}

impl Range {
    pub const ALL: [Range; 4] = [Range::Minutes30, Range::Hours6, Range::Hours24, Range::Days7];

    pub fn seconds(self) -> i64 {
        match self {
            Range::Minutes30 => 30 * 60,
            Range::Hours6 => 6 * 3600,
            Range::Hours24 => 24 * 3600,
            Range::Days7 => 7 * 24 * 3600,
        }
    }

    /// Two points further apart than this have a gap between them.
    pub fn gap(self) -> i64 {
        match self {
            Range::Minutes30 => 120,
            Range::Hours6 | Range::Hours24 => 5 * 60,
            Range::Days7 => 40 * 60,
        }
    }
}

/// One moment of a server: shares from 0 to 1, the load per core, rates in
/// bytes per second; none when it was not known. Short names keep the file
/// small.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Point {
    /// Unix seconds.
    #[serde(rename = "t")]
    pub at: i64,
    #[serde(rename = "c", default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<f32>,
    #[serde(rename = "m", default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<f32>,
    /// The fullest disk.
    #[serde(rename = "d", default, skip_serializing_if = "Option::is_none")]
    pub disk: Option<f32>,
    #[serde(rename = "l", default, skip_serializing_if = "Option::is_none")]
    pub load: Option<f32>,
    #[serde(rename = "r", default, skip_serializing_if = "Option::is_none")]
    pub rx: Option<f32>,
    #[serde(rename = "s", default, skip_serializing_if = "Option::is_none")]
    pub tx: Option<f32>,
}

impl Point {
    fn values(&self) -> [Option<f32>; 6] {
        [self.cpu, self.memory, self.disk, self.load, self.rx, self.tx]
    }

    fn from_values(at: i64, values: [Option<f32>; 6]) -> Self {
        let [cpu, memory, disk, load, rx, tx] = values;
        Self {
            at,
            cpu,
            memory,
            disk,
            load,
            rx,
            tx,
        }
    }
}

/// Readings being averaged into one point.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Bucket {
    start: i64,
    sums: [f64; 6],
    counts: [u32; 6],
}

impl Bucket {
    fn add(&mut self, point: &Point) {
        for (index, value) in point.values().into_iter().enumerate() {
            if let Some(value) = value {
                self.sums[index] += f64::from(value);
                self.counts[index] += 1;
            }
        }
    }

    /// The average, in the middle of the bucket.
    fn point(&self, width: i64) -> Point {
        let mut values = [None; 6];
        for (index, value) in values.iter_mut().enumerate() {
            if self.counts[index] > 0 {
                *value = Some((self.sums[index] / f64::from(self.counts[index])) as f32);
            }
        }
        Point::from_values(self.start + width / 2, values)
    }
}

/// How full a disk was.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskSample {
    #[serde(rename = "t")]
    pub at: i64,
    #[serde(rename = "u")]
    pub used: u64,
    #[serde(rename = "a")]
    pub available: u64,
}

const RECENT: i64 = 30 * 60;
const MINUTE: i64 = 60;
const MINUTES: i64 = 24 * 3600;
const TEN: i64 = 600;
const TENS: i64 = 7 * 24 * 3600;
/// Between two samples of a disk.
const DISK_EVERY: i64 = 15 * 60;
/// A forecast needs this much history behind it.
const FORECAST_SPAN: i64 = 6 * 3600;
/// Further than this, "full in 3 years" says nothing useful.
const FORECAST_MAX_DAYS: f64 = 365.0;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct History {
    /// Every reading of the last half hour.
    recent: VecDeque<Point>,
    /// One point a minute over the last day…
    minutes: VecDeque<Point>,
    minute: Option<Bucket>,
    /// …and one every ten minutes over the last week.
    tens: VecDeque<Point>,
    ten: Option<Bucket>,
    /// By mount point.
    disks: BTreeMap<String, VecDeque<DiskSample>>,
    /// When the latest reading was taken, in Unix seconds (0: none, or a
    /// file written before it was kept).
    last: i64,
}

fn trim(points: &mut VecDeque<Point>, keep: i64, now: i64) {
    while points.front().is_some_and(|point| point.at < now - keep) {
        points.pop_front();
    }
}

/// Adds `point` to the bucket of its `width`, moving the finished bucket to
/// `points` when the point starts a new one.
fn roll(bucket: &mut Option<Bucket>, points: &mut VecDeque<Point>, point: &Point, width: i64) {
    let start = point.at - point.at.rem_euclid(width);
    match bucket {
        Some(current) if current.start == start => current.add(point),
        _ => {
            if let Some(done) = bucket.take() {
                points.push_back(done.point(width));
            }
            let mut fresh = Bucket {
                start,
                ..Bucket::default()
            };
            fresh.add(point);
            *bucket = Some(fresh);
        }
    }
}

impl History {
    /// Takes in a reading, and how full the disks were then.
    pub fn add(&mut self, point: Point, disks: &[Disk]) {
        let now = point.at;
        // A clock put back: start the finer series again rather than mix.
        if self.recent.back().is_some_and(|last| last.at > now) {
            self.recent.clear();
        }
        self.recent.push_back(point);
        self.last = now;
        trim(&mut self.recent, RECENT, now);
        roll(&mut self.minute, &mut self.minutes, &point, MINUTE);
        trim(&mut self.minutes, MINUTES, now);
        roll(&mut self.ten, &mut self.tens, &point, TEN);
        trim(&mut self.tens, TENS, now);

        for disk in disks {
            let samples = self.disks.entry(disk.mount.clone()).or_default();
            if samples.back().is_none_or(|last| now - last.at >= DISK_EVERY) {
                samples.push_back(DiskSample {
                    at: now,
                    used: disk.used,
                    available: disk.available,
                });
            }
        }
        for samples in self.disks.values_mut() {
            while samples.front().is_some_and(|sample| sample.at < now - TENS) {
                samples.pop_front();
            }
        }
        self.disks.retain(|_, samples| !samples.is_empty());
    }

    /// The points of the last `range` before `now`, oldest first; the
    /// coarser series end with the bucket being filled, so that they reach
    /// the present.
    pub fn points(&self, range: Range, now: i64) -> Vec<Point> {
        let since = now - range.seconds();
        let (points, current, width) = match range {
            Range::Minutes30 => return self.recent.iter().filter(|point| point.at >= since).copied().collect(),
            Range::Hours6 | Range::Hours24 => (&self.minutes, &self.minute, MINUTE),
            Range::Days7 => (&self.tens, &self.ten, TEN),
        };
        let mut chosen: Vec<Point> = points.iter().filter(|point| point.at >= since).copied().collect();
        if let Some(bucket) = current {
            chosen.push(bucket.point(width));
        }
        chosen
    }

    /// When the latest reading was taken: what came after is missing.
    pub fn last_at(&self) -> Option<i64> {
        if self.last > 0 {
            return Some(self.last);
        }
        // A file of an older version: the end of its finest series.
        let bucket = |bucket: &Option<Bucket>, width: i64| bucket.as_ref().map(|bucket| bucket.start + width - 1);
        self.recent
            .back()
            .map(|point| point.at)
            .or_else(|| bucket(&self.minute, MINUTE))
            .or_else(|| bucket(&self.ten, TEN))
    }

    /// The readings of the last half hour.
    pub fn recent(&self) -> &VecDeque<Point> {
        &self.recent
    }

    /// In how many days the disk mounted there will be full, going the way
    /// it went: a straight line through its samples. None when it is not
    /// filling, when there is too little to tell, or when it is far off.
    pub fn days_until_full(&self, mount: &str) -> Option<f64> {
        let samples = self.disks.get(mount)?;
        let (first, last) = (samples.front()?, samples.back()?);
        if samples.len() < 4 || last.at - first.at < FORECAST_SPAN {
            return None;
        }
        // Least squares, time relative to the first sample.
        let n = samples.len() as f64;
        let (mut sx, mut sy, mut sxx, mut sxy) = (0.0, 0.0, 0.0, 0.0);
        for sample in samples {
            let (x, y) = ((sample.at - first.at) as f64, sample.used as f64);
            sx += x;
            sy += y;
            sxx += x * x;
            sxy += x * y;
        }
        let denominator = n * sxx - sx * sx;
        if denominator <= 0.0 {
            return None;
        }
        let per_second = (n * sxy - sx * sy) / denominator;
        if per_second <= 0.0 {
            return None;
        }
        let days = last.available as f64 / per_second / 86_400.0;
        (days <= FORECAST_MAX_DAYS).then_some(days)
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.recent.is_empty() && self.minutes.is_empty() && self.tens.is_empty()
    }

    /// Drops what got too old while the app was closed.
    fn forget_before(&mut self, now: i64) {
        trim(&mut self.recent, RECENT, now);
        trim(&mut self.minutes, MINUTES, now);
        trim(&mut self.tens, TENS, now);
        if self.minute.as_ref().is_some_and(|bucket| bucket.start < now - MINUTES) {
            self.minute = None;
        }
        if self.ten.as_ref().is_some_and(|bucket| bucket.start < now - TENS) {
            self.ten = None;
        }
        for samples in self.disks.values_mut() {
            while samples.front().is_some_and(|sample| sample.at < now - TENS) {
                samples.pop_front();
            }
        }
        self.disks.retain(|_, samples| !samples.is_empty());
    }

    /// Scripts only: a week of made-up readings, to look at the longer
    /// ranges without waiting for them.
    pub fn simulate(&mut self, now: i64, seed: u64, disks: &[Disk]) {
        let mut state = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let mut random = move || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((state >> 33) as f64 / (1u64 << 31) as f64) as f32
        };
        let start = now - TENS;
        let mut at = start;
        let (mut cpu, mut memory) = (0.2 + random() * 0.3, 0.4 + random() * 0.3);
        while at < now {
            let day = ((at % 86_400) as f32 / 86_400.0 * std::f32::consts::TAU).sin();
            cpu = (cpu + (random() - 0.5) * 0.08 + day * 0.01).clamp(0.01, 0.98);
            memory = (memory + (random() - 0.5) * 0.02).clamp(0.05, 0.97);
            let progress = (at - start) as f32 / TENS as f32;
            let point = Point {
                at,
                cpu: Some(cpu),
                memory: Some(memory),
                disk: Some(0.6 + progress * 0.25),
                load: Some(cpu * 1.2),
                rx: Some(1e5 + random() * 5e6 * (1.0 + day)),
                tx: Some(5e4 + random() * 3e6 * (1.0 + day)),
            };
            let filling: Vec<Disk> = disks
                .iter()
                .map(|disk| {
                    let used = (disk.size as f64 * f64::from(0.6 + progress * 0.25)) as u64;
                    Disk {
                        used,
                        available: disk.size.saturating_sub(used),
                        ..disk.clone()
                    }
                })
                .collect();
            self.add(point, &filling);
            at += if now - at > 3600 { 60 } else { 15 };
        }
    }
}

/// Where the history of a connection is kept.
fn file(profile_id: &str) -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", crate::APP_ID)?;
    let name: String = profile_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    Some(dirs.data_local_dir().join("monitor").join(format!("{name}.json")))
}

/// Scripts and tests keep it in memory: the user's files are theirs.
fn kept() -> bool {
    !cfg!(test) && !crate::dev::active()
}

/// The history saved for this connection, without what got too old.
pub fn load(profile_id: &str, now: i64) -> History {
    let mut history: History = file(profile_id)
        .filter(|_| kept())
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    history.forget_before(now);
    history
}

/// The history of this connection, serialized for [`write`].
pub fn serialize(history: &History) -> Option<Vec<u8>> {
    if !kept() {
        return None;
    }
    serde_json::to_vec(history).ok()
}

/// Writes what [`serialize`] made, through a temporary file so that a
/// crash never leaves half a file.
pub fn write(profile_id: &str, bytes: &[u8]) -> std::io::Result<()> {
    let Some(path) = file(profile_id) else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(&temporary, &path)
}

/// The current time, in Unix seconds.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(at: i64, cpu: f32) -> Point {
        Point {
            at,
            cpu: Some(cpu),
            ..Point::default()
        }
    }

    #[test]
    fn readings_are_kept_at_three_resolutions() {
        let mut history = History::default();
        let start = 1_700_000_400; // on ten minutes
        for second in (0..3 * 3600).step_by(3) {
            history.add(reading(start + second, if second < 3600 { 0.2 } else { 0.6 }), &[]);
        }
        let now = start + 3 * 3600 - 3;
        let recent = history.points(Range::Minutes30, now);
        assert_eq!(recent.len(), 601, "every reading of the last half hour, both ends included");
        assert!(recent.iter().all(|point| point.cpu == Some(0.6)));

        let hours = history.points(Range::Hours6, now);
        assert_eq!(hours.len(), 180, "one per minute, the current one included");
        assert!(hours.windows(2).all(|pair| pair[1].at - pair[0].at == 60));
        assert!((hours[0].cpu.unwrap() - 0.2).abs() < 1e-6);
        assert!((hours[179].cpu.unwrap() - 0.6).abs() < 1e-6);
        assert_eq!(hours[0].at, start + 30, "in the middle of its minute");

        let week = history.points(Range::Days7, now);
        assert_eq!(week.len(), 18);
        // The change at the hour falls between two buckets, which follow the clock.
        assert!(week.iter().all(|point| [0.2, 0.6].iter().any(|v| (point.cpu.unwrap() - v).abs() < 1e-6)));
    }

    #[test]
    fn old_readings_go_and_values_unknown_stay_unknown() {
        let mut history = History::default();
        let start = 1_700_000_000;
        history.add(reading(start, 0.5), &[]);
        history.add(
            Point {
                at: start + 30,
                memory: Some(0.4),
                ..Point::default()
            },
            &[],
        );
        let minute = history.points(Range::Hours24, start + 30);
        let last = minute.last().unwrap();
        assert_eq!((last.cpu, last.memory), (Some(0.5), Some(0.4)), "averaged over what was known");
        assert_eq!(last.rx, None);

        history.add(reading(start + 3 * 3600, 0.1), &[]);
        assert_eq!(history.recent().len(), 1, "the half hour moved on");
        history.forget_before(start + 9 * 24 * 3600);
        assert!(history.is_empty(), "a week and more later, nothing is left");
    }

    fn disk(used: u64, available: u64) -> Disk {
        Disk {
            mount: "/".to_owned(),
            size: used + available,
            used,
            available,
            ..Disk::default()
        }
    }

    #[test]
    fn a_filling_disk_says_when_it_will_be_full() {
        let mut history = History::default();
        let start = 1_700_000_000;
        const GB: u64 = 1 << 30;
        // 1 GB a day more, 30 GB left.
        for hour in 0..=48 {
            let used = 100 * GB + hour * GB / 24;
            let available = 130 * GB - used;
            history.add(reading(start + (hour as i64) * 3600, 0.1), &[disk(used, available)]);
        }
        let days = history.days_until_full("/").unwrap();
        assert!((days - 28.0).abs() < 0.1, "{days}");
        assert_eq!(history.days_until_full("/srv"), None);

        // Steady, or too little to tell: nothing said.
        let mut steady = History::default();
        for hour in 0..48 {
            steady.add(reading(start + hour * 3600, 0.1), &[disk(50 * GB, 50 * GB)]);
        }
        assert_eq!(steady.days_until_full("/"), None);
        let mut young = History::default();
        for quarter in 0..8 {
            young.add(reading(start + quarter * 900, 0.1), &[disk(GB + quarter as u64 * GB, 10 * GB)]);
        }
        assert_eq!(young.days_until_full("/"), None, "two hours are not enough");
    }

    #[test]
    fn the_file_round_trips_and_ignores_what_it_does_not_know() {
        let mut history = History::default();
        history.add(reading(1_700_000_000, 0.25), &[disk(1, 2)]);
        let json = serde_json::to_string(&history).unwrap();
        assert!(json.contains("\"c\":0.25"));
        let back: History = serde_json::from_str(&json).unwrap();
        assert_eq!(back, history);
        let lenient: History = serde_json::from_str(r#"{"recent":[{"t":5}],"future":true}"#).unwrap();
        assert_eq!(lenient.recent().len(), 1);
        // Tests never write the user's files.
        assert_eq!(serialize(&history), None);
    }

    #[test]
    fn the_last_reading_is_known_even_from_an_older_file() {
        let mut history = History::default();
        assert_eq!(history.last_at(), None);
        history.add(reading(1_700_000_010, 0.5), &[]);
        history.add(reading(1_700_000_070, 0.5), &[]);
        assert_eq!(history.last_at(), Some(1_700_000_070));
        let back: History = serde_json::from_str(&serde_json::to_string(&history).unwrap()).unwrap();
        assert_eq!(back.last_at(), Some(1_700_000_070));

        let mut older: History = serde_json::from_str(r#"{"minute":{"start":1700000040,"sums":[0,0,0,0,0,0],"counts":[0,0,0,0,0,0]}}"#).unwrap();
        assert_eq!(older.last_at(), Some(1_700_000_099), "the end of its minute");
        older.recent.push_back(reading(1_700_000_070, 0.1));
        assert_eq!(older.last_at(), Some(1_700_000_070));
    }

    #[test]
    fn a_simulated_week_fills_every_range() {
        let mut history = History::default();
        let now = 1_700_000_000;
        history.simulate(now, 7, &[disk(10, 90)]);
        assert!(history.points(Range::Days7, now).len() > 1000);
        assert!(history.points(Range::Hours24, now).len() > 1400);
        assert!(history.points(Range::Minutes30, now).len() > 100);
        assert!(history.days_until_full("/").is_some());
    }
}
