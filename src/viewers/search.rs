//! Finding what lies below a folder by name, kind, size and date. Folders
//! are listed several at a time and the finds reach the UI as they come.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use chrono::{Local, NaiveDate, TimeZone};
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;

use super::task::{Job, Spawner};
use crate::format::natural_cmp;
use crate::storage::{Entry, RemotePath, Storage};

/// Past this many finds the search stops: it is too broad to be useful.
pub const MAX_HITS: usize = 10_000;
/// Linked folders can loop: nothing deeper than this is listed.
const MAX_DEPTH: usize = 48;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Kinds {
    #[default]
    All,
    Files,
    Folders,
}

/// What the user asked for.
#[derive(Clone, Debug, Default)]
pub struct Criteria {
    pub pattern: String,
    pub kinds: Kinds,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    /// Unix seconds.
    pub modified_after: Option<i64>,
}

enum Alternative {
    /// Words found anywhere in the name.
    Contains(String),
    /// The shape of the whole name, with `*` and `?`.
    Wildcard(Vec<char>),
}

/// A name pattern, case-insensitive: words to find in the name or, with
/// `*` and `?`, the shape of the whole name. Several patterns separated by
/// `;` match a name that any of them matches.
pub struct NamePattern(Vec<Alternative>);

impl NamePattern {
    pub fn parse(text: &str) -> Self {
        let alternatives = text
            .split(';')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(|part| {
                let lower = part.to_lowercase();
                if lower.contains(['*', '?']) {
                    Alternative::Wildcard(lower.chars().collect())
                } else {
                    Alternative::Contains(lower)
                }
            })
            .collect();
        Self(alternatives)
    }

    /// An empty pattern matches every name.
    pub fn matches(&self, name: &str) -> bool {
        if self.0.is_empty() {
            return true;
        }
        let lower = name.to_lowercase();
        let chars: Vec<char> = lower.chars().collect();
        self.0.iter().any(|alternative| match alternative {
            Alternative::Contains(words) => lower.contains(words.as_str()),
            Alternative::Wildcard(pattern) => wildcard(pattern, &chars),
        })
    }
}

/// Whether `name` has the shape of `pattern`, where `*` stands for any run
/// of characters and `?` for one. Linear, with a single step back.
pub fn wildcard(pattern: &[char], name: &[char]) -> bool {
    let (mut p, mut n) = (0, 0);
    // The last `*` seen, and where in the name it started matching.
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some((p, n));
                p += 1;
            }
            Some(&c) if c == '?' || c == name[n] => {
                p += 1;
                n += 1;
            }
            _ => match star {
                Some((at, from)) => {
                    p = at + 1;
                    n = from + 1;
                    star = Some((at, from + 1));
                }
                None => return false,
            },
        }
    }
    pattern[p.min(pattern.len())..].iter().all(|&c| c == '*')
}

/// The criteria, ready to test entries.
pub struct Matcher {
    names: NamePattern,
    criteria: Criteria,
}

impl Criteria {
    pub fn matcher(&self) -> Matcher {
        Matcher {
            names: NamePattern::parse(&self.pattern),
            criteria: self.clone(),
        }
    }
}

impl Matcher {
    /// Sizes only apply to files: asking for one leaves folders out, as
    /// does asking for a date to an entry that has none.
    pub fn matches(&self, entry: &Entry) -> bool {
        let criteria = &self.criteria;
        let kind = match criteria.kinds {
            Kinds::All => true,
            Kinds::Files => !entry.is_dir(),
            Kinds::Folders => entry.is_dir(),
        };
        let sized = criteria.min_size.is_none() && criteria.max_size.is_none()
            || entry.size.is_some_and(|size| {
                criteria.min_size.is_none_or(|min| size >= min) && criteria.max_size.is_none_or(|max| size <= max)
            });
        let dated = criteria
            .modified_after
            .is_none_or(|after| entry.modified.is_some_and(|modified| modified >= after));
        kind && sized && dated && self.names.matches(&entry.name)
    }
}

/// `10`, `10 Ko`, `1,5 Mo`, `2G`, `512 KiB`: a size in bytes, in the binary
/// units the app shows sizes with. Empty is no size; `Err` is nonsense.
pub fn parse_size(text: &str) -> Result<Option<u64>, ()> {
    let text = text.trim().replace(',', ".");
    if text.is_empty() {
        return Ok(None);
    }
    let split = text.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: f64 = number.parse().map_err(|_| ())?;
    let power = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "o" | "b" | "octets" => 0,
        "k" | "ko" | "kb" | "kio" | "kib" => 1,
        "m" | "mo" | "mb" | "mio" | "mib" => 2,
        "g" | "go" | "gb" | "gio" | "gib" => 3,
        "t" | "to" | "tb" | "tio" | "tib" => 4,
        _ => return Err(()),
    };
    Ok(Some((number * 1024f64.powi(power)).round() as u64))
}

/// `20/11/2026` or `2026-11-20`: the start of that day, local time.
pub fn parse_date(text: &str) -> Result<Option<i64>, ()> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let day = ["%d/%m/%Y", "%Y-%m-%d", "%d/%m/%y", "%d.%m.%Y"]
        .iter()
        .find_map(|format| NaiveDate::parse_from_str(text, format).ok())
        .ok_or(())?;
    let midnight = day.and_hms_opt(0, 0, 0).ok_or(())?;
    Local
        .from_local_datetime(&midnight)
        .earliest()
        .map(|start| Some(start.timestamp()))
        .ok_or(())
}

/// The start of the day `days` days before today, as the date field shows it.
pub fn days_ago(days: i64) -> String {
    (Local::now() - chrono::Duration::days(days)).format("%d/%m/%Y").to_string()
}

/// A find: an entry and the folder it is in, relative to the one searched
/// (empty for that folder itself).
#[derive(Clone, Debug)]
pub struct Hit {
    pub folder: String,
    pub entry: Entry,
}

impl Hit {
    pub fn path(&self, root: &RemotePath) -> RemotePath {
        root.child(&self.folder).child(&self.entry.name)
    }
}

fn below(folder: &str, name: &str) -> String {
    if folder.is_empty() {
        name.to_owned()
    } else {
        format!("{folder}/{name}")
    }
}

/// What a running search shares with the UI.
#[derive(Default)]
pub struct Shared {
    /// Finds not taken by the UI yet.
    pub hits: Vec<Hit>,
    pub found: usize,
    pub folders: usize,
    /// Folders below the searched one that could not be listed, with why.
    pub unreadable: Vec<(String, String)>,
    /// The searched folder itself could not be listed.
    pub failed: Option<String>,
    /// Stopped at [`MAX_HITS`].
    pub truncated: bool,
    pub done: bool,
}

/// Starts searching below `root`, reading up to `parallel` folders at once.
pub fn start(
    spawner: &Spawner,
    storage: Arc<dyn Storage>,
    root: RemotePath,
    criteria: &Criteria,
    parallel: usize,
) -> (Arc<Mutex<Shared>>, Job) {
    let shared = Arc::new(Mutex::new(Shared::default()));
    let matcher = criteria.matcher();
    let (reported, ctx) = (shared.clone(), spawner.ctx().clone());
    let job = spawner.spawn(async move {
        walk(storage, root, matcher, parallel.max(1), &reported, &ctx).await;
        if let Ok(mut shared) = reported.lock() {
            shared.done = true;
        }
        ctx.request_repaint();
    });
    (shared, job)
}

/// Every entry of `dir`, all pages of it.
async fn list_all(storage: &dyn Storage, dir: &RemotePath) -> anyhow::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    let mut cursor = None;
    loop {
        let page = storage.list_page(dir, cursor).await?;
        entries.extend(page.entries);
        match page.next {
            Some(next) => cursor = Some(next),
            None => return Ok(entries),
        }
    }
}

/// Lists folders level by level, so that the nearest finds come first.
async fn walk(
    storage: Arc<dyn Storage>,
    root: RemotePath,
    matcher: Matcher,
    parallel: usize,
    shared: &Mutex<Shared>,
    ctx: &egui::Context,
) {
    let mut waiting = VecDeque::from([(root, String::new(), 0usize)]);
    let mut reading = FuturesUnordered::new();
    loop {
        while reading.len() < parallel
            && let Some((dir, folder, depth)) = waiting.pop_front()
        {
            let storage = storage.clone();
            reading.push(async move {
                let listed = list_all(&*storage, &dir).await;
                (dir, folder, depth, listed)
            });
        }
        let Some((dir, folder, depth, listed)) = reading.next().await else {
            return;
        };
        let entries = match listed {
            Ok(entries) => entries,
            Err(error) => {
                let error = format!("{error:#}");
                let Ok(mut state) = shared.lock() else {
                    return;
                };
                if folder.is_empty() && depth == 0 {
                    state.failed = Some(error);
                    return;
                }
                state.unreadable.push((folder, error));
                drop(state);
                ctx.request_repaint();
                continue;
            }
        };

        let mut hits = Vec::new();
        for entry in entries {
            if entry.is_dir() && depth < MAX_DEPTH {
                waiting.push_back((dir.child(&entry.name), below(&folder, &entry.name), depth + 1));
            }
            if matcher.matches(&entry) {
                hits.push(Hit {
                    folder: folder.clone(),
                    entry,
                });
            }
        }
        let Ok(mut state) = shared.lock() else {
            return;
        };
        state.folders += 1;
        let room = MAX_HITS.saturating_sub(state.found);
        if hits.len() > room {
            hits.truncate(room);
            state.truncated = true;
        }
        state.found += hits.len();
        state.hits.extend(hits);
        if state.truncated {
            return;
        }
        drop(state);
        ctx.request_repaint();
    }
}

/// How the results are sorted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sort {
    /// In the order they were found: nearest first.
    Found,
    Name,
    Folder,
    Size,
    Modified,
}

/// A search that ran, or runs.
pub struct Run {
    /// The folder searched.
    pub root: RemotePath,
    pub started: f64,
    pub finished: Option<f64>,
    pub folders: usize,
    pub unreadable: Vec<(String, String)>,
    pub failed: Option<String>,
    pub truncated: bool,
    job: Option<(Arc<Mutex<Shared>>, Job)>,
}

/// The search of a tab: the form, and the results of its last run.
pub struct Search {
    /// Where the next run searches.
    pub root: RemotePath,
    pub pattern: String,
    pub kinds: Kinds,
    pub min_size: String,
    pub max_size: String,
    pub after: String,
    /// Give the keyboard to the pattern on the next frame.
    pub focus: bool,
    /// What is wrong with the form.
    pub error: Option<String>,
    pub run: Option<Run>,
    pub hits: Vec<Hit>,
    pub sort: Sort,
    pub ascending: bool,
    /// Indices into `hits`, sorted.
    order: Vec<usize>,
    order_stale: bool,
    /// Index into `hits` of the selected result.
    pub selected: Option<usize>,
}

impl Search {
    pub fn new(root: RemotePath, pattern: &str) -> Self {
        Self {
            root,
            pattern: pattern.trim().to_owned(),
            kinds: Kinds::All,
            min_size: String::new(),
            max_size: String::new(),
            after: String::new(),
            focus: true,
            error: None,
            run: None,
            hits: Vec::new(),
            sort: Sort::Found,
            ascending: true,
            order: Vec::new(),
            order_stale: false,
            selected: None,
        }
    }

    /// The form, read; `Err` says which field is wrong.
    pub fn criteria(&self) -> Result<Criteria, String> {
        let size = |text: &str, what: &str| {
            parse_size(text).map_err(|()| format!("{what} : « {} » n'est pas une taille (ex. 10 Mo)", text.trim()))
        };
        Ok(Criteria {
            pattern: self.pattern.trim().to_owned(),
            kinds: self.kinds,
            min_size: size(&self.min_size, "Taille minimale")?,
            max_size: size(&self.max_size, "Taille maximale")?,
            modified_after: parse_date(&self.after)
                .map_err(|()| format!("« {} » n'est pas une date (JJ/MM/AAAA)", self.after.trim()))?,
        })
    }

    /// Starts a run with the form as it is; false when the form is wrong.
    pub fn start(&mut self, spawner: &Spawner, storage: Arc<dyn Storage>, parallel: usize, now: f64) -> bool {
        let criteria = match self.criteria() {
            Ok(criteria) => criteria,
            Err(error) => {
                self.error = Some(error);
                return false;
            }
        };
        self.error = None;
        let job = start(spawner, storage, self.root.clone(), &criteria, parallel);
        self.run = Some(Run {
            root: self.root.clone(),
            started: now,
            finished: None,
            folders: 0,
            unreadable: Vec::new(),
            failed: None,
            truncated: false,
            job: Some(job),
        });
        self.hits.clear();
        self.order.clear();
        self.selected = None;
        true
    }

    pub fn running(&self) -> bool {
        self.run.as_ref().is_some_and(|run| run.job.is_some())
    }

    /// Stops the run; what was found stays.
    pub fn stop(&mut self, now: f64) {
        if let Some(run) = &mut self.run
            && run.job.take().is_some()
        {
            run.finished = Some(now);
        }
    }

    /// Takes in what the run found since the last frame.
    pub fn pump(&mut self, now: f64) {
        let Some(run) = &mut self.run else {
            return;
        };
        let Some((shared, _)) = &run.job else {
            return;
        };
        let Ok(mut shared) = shared.lock() else {
            return;
        };
        let fresh = std::mem::take(&mut shared.hits);
        run.folders = shared.folders;
        run.unreadable.clone_from(&shared.unreadable);
        run.failed.clone_from(&shared.failed);
        run.truncated = shared.truncated;
        let done = shared.done;
        drop(shared);
        if done {
            run.job = None;
            run.finished = Some(now);
        }
        if !fresh.is_empty() {
            self.hits.extend(fresh);
            self.order_stale = true;
        }
    }

    pub fn sort_by(&mut self, sort: Sort) {
        if self.sort == sort {
            self.ascending = !self.ascending;
        } else {
            self.sort = sort;
            // Biggest and most recent first is what one usually wants.
            self.ascending = !matches!(sort, Sort::Size | Sort::Modified);
        }
        self.order_stale = true;
    }

    /// The results in the order shown, as indices into `hits`.
    pub fn order(&mut self) -> &[usize] {
        if self.order_stale || self.order.len() != self.hits.len() {
            self.order_stale = false;
            let hits = &self.hits;
            let mut order: Vec<usize> = (0..hits.len()).collect();
            let by = |a: &Hit, b: &Hit| match self.sort {
                Sort::Found => std::cmp::Ordering::Equal,
                Sort::Name => natural_cmp(&a.entry.name, &b.entry.name),
                Sort::Folder => {
                    natural_cmp(&a.folder, &b.folder).then_with(|| natural_cmp(&a.entry.name, &b.entry.name))
                }
                Sort::Size => a.entry.size.cmp(&b.entry.size),
                Sort::Modified => a.entry.modified.cmp(&b.entry.modified),
            };
            order.sort_by(|&a, &b| {
                let order = by(&hits[a], &hits[b]).then(a.cmp(&b));
                if self.ascending { order } else { order.reverse() }
            });
            self.order = order;
        }
        &self.order
    }

    /// The selected result, as the path of its file.
    pub fn selected_hit(&self) -> Option<(&Hit, RemotePath)> {
        let hit = self.hits.get(self.selected?)?;
        let root = &self.run.as_ref()?.root;
        Some((hit, hit.path(root)))
    }

    /// The order of the results and the results, to draw them.
    pub fn view(&mut self) -> (&[usize], &[Hit]) {
        self.order();
        (&self.order, &self.hits)
    }

    /// The files among the results, in the order shown.
    fn files(&mut self) -> Vec<usize> {
        let order = self.order().to_vec();
        order.into_iter().filter(|&index| !self.hits[index].entry.is_dir()).collect()
    }

    /// Moves the selection by `step` among the files of the results.
    /// Returns the newly selected one.
    pub fn step_file(&mut self, step: isize) -> Option<usize> {
        let current = self.selected?;
        let files = self.files();
        let at = files.iter().position(|&index| index == current)?;
        let next = *files.get(at.checked_add_signed(step)?)?;
        self.selected = Some(next);
        Some(next)
    }

    /// Where the selected result stands among the files, and their count.
    pub fn file_position(&mut self) -> Option<(usize, usize)> {
        let current = self.selected?;
        let files = self.files();
        Some((files.iter().position(|&index| index == current)? + 1, files.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::EntryKind;

    fn matches(pattern: &str, name: &str) -> bool {
        NamePattern::parse(pattern).matches(name)
    }

    fn entry(name: &str, kind: EntryKind, size: Option<u64>, modified: Option<i64>) -> Entry {
        Entry {
            permissions: None,
            owner: None,
            group: None,
            link: None,
            name: name.to_owned(),
            kind,
            size,
            modified,
        }
    }

    #[test]
    fn wildcards_match_whole_names() {
        assert!(matches("*.log", "server.log"));
        assert!(matches("*.LOG", "Server.Log"));
        assert!(!matches("*.log", "server.log.gz"));
        assert!(matches("*.log*", "server.log.gz"));
        assert!(matches("app-??.txt", "app-12.txt"));
        assert!(!matches("app-??.txt", "app-123.txt"));
        assert!(matches("a*b*c", "aXXbYYbZZc"));
        assert!(!matches("a*b*c", "aXXbYYbZZ"));
        assert!(matches("*", ""));
        assert!(matches("**x", "x"));
        assert!(matches("été*", "Été 2026.jpg"));
    }

    #[test]
    fn words_match_anywhere_and_alternatives_any() {
        assert!(matches("rapport", "Mon RAPPORT final.pdf"));
        assert!(!matches("rapport", "report.pdf"));
        assert!(matches("*.png; *.jpg", "photo.jpg"));
        assert!(matches("facture;*.csv", "export.csv"));
        assert!(matches("", "anything"));
        assert!(matches(" ; ", "anything"));
    }

    #[test]
    fn criteria_combine() {
        let file = entry("big.log", EntryKind::File, Some(5 * 1024 * 1024), Some(2_000));
        let folder = entry("logs", EntryKind::Folder, None, None);
        let all = Criteria::default();
        assert!(all.matcher().matches(&file) && all.matcher().matches(&folder));

        let files = Criteria { kinds: Kinds::Files, ..Criteria::default() };
        assert!(files.matcher().matches(&file) && !files.matcher().matches(&folder));
        let folders = Criteria { kinds: Kinds::Folders, pattern: "log".to_owned(), ..Criteria::default() };
        assert!(!folders.matcher().matches(&file) && folders.matcher().matches(&folder));

        let sized = Criteria { min_size: Some(1024 * 1024), max_size: Some(8 * 1024 * 1024), ..Criteria::default() };
        assert!(sized.matcher().matches(&file));
        assert!(!sized.matcher().matches(&folder), "folders have no size");
        let small = Criteria { max_size: Some(1024), ..Criteria::default() };
        assert!(!small.matcher().matches(&file));

        let recent = Criteria { modified_after: Some(1_000), ..Criteria::default() };
        assert!(recent.matcher().matches(&file));
        assert!(!recent.matcher().matches(&folder), "no date, no match");
        let later = Criteria { modified_after: Some(3_000), ..Criteria::default() };
        assert!(!later.matcher().matches(&file));
    }

    #[test]
    fn sizes_and_dates_are_read() {
        assert_eq!(parse_size(""), Ok(None));
        assert_eq!(parse_size("512"), Ok(Some(512)));
        assert_eq!(parse_size("10 Ko"), Ok(Some(10 * 1024)));
        assert_eq!(parse_size("1,5 Mo"), Ok(Some(1_572_864)));
        assert_eq!(parse_size("2G"), Ok(Some(2 * 1024 * 1024 * 1024)));
        assert_eq!(parse_size("3 kib"), Ok(Some(3072)));
        assert_eq!(parse_size("dix"), Err(()));
        assert_eq!(parse_size("10 parsecs"), Err(()));

        assert_eq!(parse_date(""), Ok(None));
        let day = parse_date("20/11/2026").unwrap().unwrap();
        assert_eq!(parse_date("2026-11-20"), Ok(Some(day)));
        assert_eq!(parse_date("21/11/2026"), Ok(Some(day + 86_400)));
        assert_eq!(parse_date("31/02/2026"), Err(()));
        assert_eq!(parse_date("hier"), Err(()));
        assert!(parse_date(&days_ago(7)).unwrap().is_some());
    }

    /// Runs a search to its end on a storage in memory.
    async fn search(memory: super::super::memory::Memory, root: &str, criteria: Criteria) -> Shared {
        let storage: Arc<dyn Storage> = Arc::new(memory);
        let spawner = Spawner::on(tokio::runtime::Handle::current());
        let (shared, _job) = start(&spawner, storage, RemotePath::parse(root), &criteria, 2);
        for _ in 0..200 {
            if shared.lock().unwrap().done {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let mut shared = shared.lock().unwrap();
        assert!(shared.done, "the search ends");
        std::mem::take(&mut *shared)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_walk_finds_nearest_first_and_goes_past_unreadable_folders() {
        let mut memory = super::super::memory::Memory::with(&[
            ("/top/logs/2026/c.log", b"xyz"),
            ("/top/logs/b.log", b"xy"),
            ("/top/a.log", b"x"),
            ("/top/notes.txt", b"hello"),
            ("/top/secret/d.log", b"x"),
            ("/elsewhere/e.log", b"x"),
        ]);
        memory.unreadable.insert("/top/secret".to_owned());
        let criteria = Criteria {
            pattern: "*.log".to_owned(),
            ..Criteria::default()
        };
        let found = search(memory, "/top", criteria).await;
        let root = RemotePath::parse("/top");
        let paths: Vec<String> = found.hits.iter().map(|hit| hit.path(&root).to_string()).collect();
        assert_eq!(paths, ["/top/a.log", "/top/logs/b.log", "/top/logs/2026/c.log"]);
        assert_eq!(found.folders, 3, "top, logs and logs/2026");
        assert_eq!(found.unreadable.len(), 1);
        assert_eq!(found.unreadable[0].0, "secret");
        assert!(found.failed.is_none() && !found.truncated);

        let folders = Criteria {
            kinds: Kinds::Folders,
            ..Criteria::default()
        };
        let memory = super::super::memory::Memory::with(&[("/top/logs/2026/c.log", b"xyz")]);
        let found = search(memory, "/top", folders).await;
        let names: Vec<&str> = found.hits.iter().map(|hit| hit.entry.name.as_str()).collect();
        assert_eq!(names, ["logs", "2026"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_search_fails_on_its_folder_and_stops_when_too_broad() {
        let mut memory = super::super::memory::Memory::with(&[("/top/a.log", b"x")]);
        memory.unreadable.insert("/top".to_owned());
        let found = search(memory, "/top", Criteria::default()).await;
        assert_eq!(found.failed.as_deref(), Some("permission refusée"));

        let memory = super::super::memory::Memory::default();
        for index in 0..MAX_HITS + 50 {
            memory.put(&format!("/many/file-{index}.txt"), b"");
        }
        let found = search(memory, "/many", Criteria::default()).await;
        assert!(found.truncated);
        assert_eq!(found.hits.len(), MAX_HITS);
    }

    #[test]
    fn hits_know_their_path() {
        let hit = Hit {
            folder: "a b/c".to_owned(),
            entry: entry("x.txt", EntryKind::File, Some(1), None),
        };
        let root = RemotePath::parse("/bucket/top");
        assert_eq!(hit.path(&root).to_string(), "/bucket/top/a b/c/x.txt");
        let top = Hit { folder: String::new(), ..hit };
        assert_eq!(top.path(&root).to_string(), "/bucket/top/x.txt");
    }
}
