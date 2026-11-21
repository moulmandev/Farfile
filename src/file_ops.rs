//! Rename, move and copy on the server, permissions and owner, properties,
//! folder sizes and bookmarks: the state the screens draw from, the rules
//! (where things may go, what copies are called) and the jobs that run on
//! the worker. The protocols themselves are in `storage`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use futures_util::{StreamExt, stream};
use serde::{Deserialize, Serialize};

use crate::format;
use crate::storage::ops::{Accounts, Properties, Tally, list_all, mode_text, walk_tree};
use crate::storage::{Entry, Progress, RemotePath, Storage, storage_key};

/// Entries changed at the same time by a recursive change of permissions.
const PARALLEL_CHANGES: usize = 8;

/// A folder of a connection kept at hand, saved with the connection.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Bookmark {
    pub name: String,
    /// As [`RemotePath`] writes it: `/bucket/folder`.
    pub path: String,
}

/// The columns of the listing that only show when asked for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Columns {
    pub permissions: bool,
    pub owner: bool,
}

/// The bookmark of this folder, if there is one.
pub fn bookmark_of(bookmarks: &[Bookmark], path: &RemotePath) -> Option<usize> {
    let text = path.to_string();
    bookmarks.iter().position(|bookmark| bookmark.path == text)
}

/// Adds `path`, named after its last folder (or `root_name` at the root), or
/// removes it when it is there already. Returns true when it was added.
pub fn toggle_bookmark(bookmarks: &mut Vec<Bookmark>, path: &RemotePath, root_name: &str) -> bool {
    if let Some(index) = bookmark_of(bookmarks, path) {
        bookmarks.remove(index);
        return false;
    }
    let name = if path.is_root() { root_name } else { path.name() };
    bookmarks.push(Bookmark {
        name: name.to_owned(),
        path: path.to_string(),
    });
    true
}

/// An entry picked in a listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picked {
    pub path: RemotePath,
    pub is_dir: bool,
    pub size: u64,
}

impl Picked {
    pub fn of(folder: &RemotePath, entry: &Entry) -> Self {
        Self {
            path: folder.child(&entry.name),
            is_dir: entry.is_dir(),
            size: entry.size.unwrap_or(0),
        }
    }
}

/// What is being dragged out of a listing: entries of one folder of one
/// connection. Dropped on a folder of the same connection, they move there;
/// elsewhere is for whoever accepts them.
pub struct Dragged {
    pub storage: Arc<dyn Storage>,
    /// The saved connection (profile id): two tabs on the same one reach
    /// the same files through two storages.
    pub connection: String,
    pub folder: RemotePath,
    pub items: Vec<Picked>,
    /// The listing it comes from (see `crate::ui::local::listing_key`):
    /// dropped on a folder of that listing, it moves there; dropped in the
    /// other view, it is copied there.
    pub listing: usize,
}

/// What Ctrl+C or Ctrl+X took. The connection travels with the items, so
/// that pasting into another one can be offered too.
pub struct Clipboard {
    pub storage: Arc<dyn Storage>,
    /// The saved connection (profile id), see [`Dragged::connection`].
    pub connection: String,
    /// The connection, for display.
    pub origin: String,
    pub items: Vec<Picked>,
    /// Ctrl+X: the items move instead of being copied.
    pub cut: bool,
}

/// What a file browser needs to know of the file operations while it is
/// drawn, kept in sync by the app every frame.
#[derive(Default)]
pub struct SessionOps {
    /// The saved connection of the tab (profile id).
    pub connection: String,
    /// The row whose name is being edited in place.
    pub renaming: Option<Renaming>,
    /// The bookmarks of the connection.
    pub bookmarks: Vec<Bookmark>,
    pub columns: Columns,
    /// What Ctrl+V would paste.
    pub clipboard: Option<ClipboardView>,
    /// Names to select once the listing of this folder comes back.
    pub select_after: Option<(RemotePath, Vec<String>)>,
    /// Folders whose size is being counted, by name in the current folder.
    pub sizing: HashSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Renaming {
    pub folder: RemotePath,
    pub name: String,
    /// Tells two renames of the same entry apart, for the text field.
    pub serial: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipboardView {
    pub count: usize,
    pub cut: bool,
    /// From the connection of this browser.
    pub here: bool,
}

/// Whether `path` is `ancestor` or lies below it.
pub fn is_within(path: &RemotePath, ancestor: &RemotePath) -> bool {
    path.segments().starts_with(ancestor.segments())
}

/// Where `path` is once `from` has become `to`; `None` when it is not in it.
pub fn moved_path(path: &RemotePath, from: &RemotePath, to: &RemotePath) -> Option<RemotePath> {
    if !is_within(path, from) {
        return None;
    }
    let below = &path.segments()[from.segments().len()..];
    Some(below.iter().fold(to.clone(), |path, segment| path.child(segment)))
}

/// Why these items cannot go into `dest`, if they cannot.
pub fn refuse(items: &[Picked], dest: &RemotePath, has_buckets: bool) -> Option<String> {
    if has_buckets && dest.is_root() {
        return Some("Choisissez d'abord un bucket comme destination.".to_owned());
    }
    items
        .iter()
        .find(|item| item.is_dir && is_within(dest, &item.path))
        .map(|item| format!("« {} » ne peut pas aller dans lui-même.", item.path.name()))
}

/// A path typed by hand: absolute, or relative to `from`, with `.` and `..`.
pub fn resolve_path(from: &RemotePath, text: &str) -> RemotePath {
    let text = text.trim();
    let mut segments: Vec<String> = if text.starts_with('/') {
        Vec::new()
    } else {
        from.segments().to_vec()
    };
    for segment in text.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            name => segments.push(name.to_owned()),
        }
    }
    RemotePath::parse(&segments.join("/"))
}

/// Checks a name typed for an entry, and returns it trimmed.
pub fn check_name(name: &str) -> Result<&str, String> {
    let name = name.trim();
    if name.is_empty() {
        Err("Le nom ne peut pas être vide.".to_owned())
    } else if name.contains('/') {
        Err("Le nom ne peut pas contenir « / ».".to_owned())
    } else if name == "." || name == ".." {
        Err("Ce nom est réservé.".to_owned())
    } else {
        Ok(name)
    }
}

/// The part of a name before its extension, which a rename selects first:
/// `rapport.final` of `rapport.final.pdf`, all of `.bashrc` or a folder.
pub fn stem(name: &str, is_dir: bool) -> &str {
    if is_dir {
        return name;
    }
    match name.rfind('.') {
        Some(dot) if dot > 0 => &name[..dot],
        _ => name,
    }
}

/// A free name for a copy in the same folder: `rapport (copie).pdf`, then
/// `rapport (copie 2).pdf`…
pub fn copy_name(name: &str, is_dir: bool, taken: &HashSet<String>) -> String {
    let stem = stem(name, is_dir);
    let extension = &name[stem.len()..];
    let mut number = 1;
    loop {
        let candidate = if number == 1 {
            format!("{stem} (copie){extension}")
        } else {
            format!("{stem} (copie {number}){extension}")
        };
        if !taken.contains(&candidate) {
            return candidate;
        }
        number += 1;
    }
}

/// « photos » for one item, « 3 éléments » for more.
pub fn describe(items: &[Picked]) -> String {
    match items {
        [only] => format!("« {} »", only.path.name()),
        many => format!("{} éléments", many.len()),
    }
}

/// How a folder is named in messages.
fn folder_name(path: &RemotePath) -> String {
    if path.is_root() {
        "la racine".to_owned()
    } else {
        format!("« {} »", path.name())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Move,
    Copy,
}

/// What to do with what is already at the destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Existing {
    Ask,
    Replace,
    Skip,
}

/// A move or a copy, ready to run.
#[derive(Clone)]
pub struct Plan {
    pub storage: Arc<dyn Storage>,
    pub mode: Mode,
    pub items: Vec<Picked>,
    pub dest: RemotePath,
    /// The new name of the only item, for a rename.
    pub rename_to: Option<String>,
    pub existing: Existing,
    /// What `dest` holds (name, is a folder), when the listing on screen
    /// tells it; otherwise the job lists it first.
    pub known: Option<HashMap<String, bool>>,
}

/// One item of a plan, with the name it takes at the destination.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub item: Picked,
    pub name: String,
    /// What is already there under that name: `Some(is a folder)`.
    pub clash: Option<bool>,
}

/// Where each item of a plan goes. A copy into the folder it comes from gets
/// a new name; a move there has nothing to do and is left out.
pub fn targets(
    mode: Mode,
    items: &[Picked],
    rename_to: Option<&str>,
    dest: &RemotePath,
    existing: &HashMap<String, bool>,
) -> Vec<Target> {
    let mut taken: HashSet<String> = existing.keys().cloned().collect();
    let mut targets = Vec::new();
    for item in items {
        let name = item.path.name();
        let here = item.path.parent().as_ref() == Some(dest);
        let name = match rename_to {
            Some(new) => new.to_owned(),
            None if here && mode == Mode::Copy => copy_name(name, item.is_dir, &taken),
            None => name.to_owned(),
        };
        if here && mode == Mode::Move && name == item.path.name() {
            continue;
        }
        let clash = existing.get(&name).copied();
        taken.insert(name.clone());
        targets.push(Target {
            item: item.clone(),
            name,
            clash,
        });
    }
    targets
}

/// A long operation, shown in the status bar while it runs.
pub struct Job {
    pub id: u64,
    /// What it does: « Copie de « photos » ».
    pub label: String,
    /// Bytes, when the operation knows them; also where it is cancelled.
    pub progress: Progress,
    pub done: AtomicU64,
    pub count: AtomicU64,
    /// When it started, in the time of the UI.
    pub started: f64,
}

impl Job {
    pub fn cancel(&self) {
        self.progress.cancel();
    }
}

/// A folder size, counted or being counted.
pub struct Sizing {
    pub tally: Tally,
    pub finished: AtomicBool,
    pub error: Mutex<Option<String>>,
    pub started: f64,
}

impl Sizing {
    pub fn is_running(&self) -> bool {
        !self.finished.load(Ordering::Relaxed)
    }

    pub fn failed(&self) -> bool {
        self.error.lock().map(|error| error.is_some()).unwrap_or(false)
    }
}

/// What the jobs tell the app.
pub enum OpsEvent {
    /// A job is over. The listings of `changed` are stale; folders of
    /// `moved` now live elsewhere; `select` names what to select in a folder.
    Finished {
        job: u64,
        storage: usize,
        changed: Vec<RemotePath>,
        moved: Vec<(RemotePath, RemotePath)>,
        select: Option<(RemotePath, Vec<String>)>,
        notice: Option<(String, bool)>,
    },
    /// Part of what a move or copy brings is already at the destination.
    Clashes {
        job: u64,
        plan: Plan,
        clashes: Vec<(String, bool)>,
    },
    Properties {
        request: u64,
        result: Result<Properties, String>,
    },
    Accounts {
        storage: usize,
        accounts: Arc<Accounts>,
    },
    /// A folder size was counted, or could not be.
    Sized {
        error: Option<String>,
    },
}

/// Sends events to the app and wakes it up.
#[derive(Clone)]
pub struct Reply {
    sender: Sender<OpsEvent>,
    ctx: egui::Context,
}

impl Reply {
    fn send(&self, event: OpsEvent) {
        let _ = self.sender.send(event);
        self.ctx.request_repaint();
    }
}

/// Everything the app keeps about the file operations.
pub struct OpsState {
    pub clipboard: Option<Clipboard>,
    pub jobs: Vec<Arc<Job>>,
    /// Folder sizes by connection ([`storage_key`]) and path.
    pub sizes: HashMap<(usize, RemotePath), Arc<Sizing>>,
    /// The listings must be told again about sizes: some were forgotten.
    pub sizes_changed: bool,
    /// User and group names by connection, once asked for.
    pub accounts: HashMap<usize, Option<Arc<Accounts>>>,
    sender: Sender<OpsEvent>,
    pub events: Receiver<OpsEvent>,
    next_id: u64,
    /// Bumped on each rename started, see [`Renaming::serial`].
    pub rename_serial: u64,
    /// Clashes found while a dialog was open, asked about once it closes.
    pub pending_clashes: std::collections::VecDeque<(Plan, Vec<(String, bool)>)>,
}

impl Default for OpsState {
    fn default() -> Self {
        let (sender, events) = channel();
        Self {
            clipboard: None,
            jobs: Vec::new(),
            sizes: HashMap::new(),
            sizes_changed: false,
            accounts: HashMap::new(),
            sender,
            events,
            next_id: 1,
            rename_serial: 0,
            pending_clashes: Default::default(),
        }
    }
}

impl OpsState {
    pub fn reply(&self, ctx: &egui::Context) -> Reply {
        Reply {
            sender: self.sender.clone(),
            ctx: ctx.clone(),
        }
    }

    pub fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// Registers a job, shown once it lasts.
    pub fn start_job(&mut self, label: String, count: usize, now: f64) -> Arc<Job> {
        let job = Arc::new(Job {
            id: self.next_id(),
            label,
            progress: Progress::default(),
            done: AtomicU64::new(0),
            count: AtomicU64::new(count as u64),
            started: now,
        });
        self.jobs.push(job.clone());
        job
    }

    pub fn end_job(&mut self, id: u64) {
        self.jobs.retain(|job| job.id != id);
    }

    /// Forgets the sizes that `path` changing makes wrong: its own, those of
    /// the folders it sits in, and those of what it holds.
    pub fn forget_sizes(&mut self, storage: usize, path: &RemotePath) {
        let before = self.sizes.len();
        self.sizes.retain(|(key, sized), sizing| {
            *key != storage || sizing.is_running() || !(is_within(path, sized) || is_within(sized, path))
        });
        self.sizes_changed |= self.sizes.len() != before;
    }

    /// The size of the folder `path`, while counted and once counted.
    pub fn size_of(&self, storage: usize, path: &RemotePath) -> Option<&Arc<Sizing>> {
        self.sizes.get(&(storage, path.clone()))
    }
}

/// What `dest` holds, by name. A destination that cannot be listed is
/// created, with the folders it needs: « Déplacer vers… » may name new ones.
async fn existing_names(storage: &dyn Storage, dest: &RemotePath) -> Result<HashMap<String, bool>, String> {
    let names = |entries: Vec<Entry>| entries.into_iter().map(|entry| (entry.name.clone(), entry.is_dir())).collect();
    let error = match list_all(storage, dest).await {
        Ok(entries) => return Ok(names(entries)),
        Err(error) => error,
    };
    // Those that exist already refuse, which is fine.
    for depth in 1..=dest.segments().len() {
        let _ = storage.create_folder(&dest.ancestor(depth)).await;
    }
    match list_all(storage, dest).await {
        Ok(entries) => Ok(names(entries)),
        Err(_) => Err(format!("Destination inaccessible : {error:#}")),
    }
}

/// Runs a move or a copy, item after item, and says how it went.
pub async fn run_plan(plan: Plan, job: Arc<Job>, reply: Reply) {
    let storage = storage_key(&plan.storage);
    let existing = match &plan.known {
        Some(known) => known.clone(),
        None => match existing_names(&*plan.storage, &plan.dest).await {
            Ok(existing) => existing,
            Err(error) => {
                return reply.send(OpsEvent::Finished {
                    job: job.id,
                    storage,
                    changed: Vec::new(),
                    moved: Vec::new(),
                    select: None,
                    notice: Some((error, true)),
                });
            }
        },
    };
    let targets = targets(plan.mode, &plan.items, plan.rename_to.as_deref(), &plan.dest, &existing);
    let clashes: Vec<(String, bool)> = targets
        .iter()
        .filter_map(|target| target.clash.map(|is_dir| (target.name.clone(), is_dir)))
        .collect();
    if plan.existing == Existing::Ask && !clashes.is_empty() {
        return reply.send(OpsEvent::Clashes {
            job: job.id,
            plan,
            clashes,
        });
    }

    job.count.store(targets.len() as u64, Ordering::Relaxed);
    let (mut done, mut moved, mut failures) = (Vec::new(), Vec::new(), Vec::new());
    for target in &targets {
        if job.progress.is_cancelled() {
            break;
        }
        let to = plan.dest.child(&target.name);
        let result = async {
            if let Some(is_dir) = target.clash {
                if plan.existing == Existing::Skip {
                    return Ok(false);
                }
                plan.storage.delete(&to, is_dir).await?;
            }
            let (from, is_dir) = (&target.item.path, target.item.is_dir);
            match plan.mode {
                Mode::Move => plan.storage.rename(from, &to, is_dir, &job.progress).await?,
                Mode::Copy => plan.storage.copy(from, &to, is_dir, &job.progress).await?,
            }
            Ok::<_, anyhow::Error>(true)
        }
        .await;
        job.done.fetch_add(1, Ordering::Relaxed);
        match result {
            Ok(true) => {
                if plan.mode == Mode::Move && target.item.is_dir {
                    moved.push((target.item.path.clone(), to.clone()));
                }
                done.push(target.name.clone());
            }
            Ok(false) => {}
            Err(_) if job.progress.is_cancelled() => break,
            Err(error) => failures.push(format!("{} : {error:#}", target.name)),
        }
    }

    let mut changed = vec![plan.dest.clone()];
    if plan.mode == Mode::Move {
        for target in &targets {
            if let Some(parent) = target.item.path.parent()
                && !changed.contains(&parent)
            {
                changed.push(parent);
            }
        }
    }
    let notice = plan_notice(&plan, &targets, &done, &failures, job.progress.is_cancelled());
    let select = (!done.is_empty()).then(|| (plan.dest.clone(), done));
    reply.send(OpsEvent::Finished {
        job: job.id,
        storage,
        changed,
        moved,
        select,
        notice,
    });
}

/// What to tell once a plan ran. A rename that worked shows by itself.
fn plan_notice(plan: &Plan, targets: &[Target], done: &[String], failures: &[String], cancelled: bool) -> Option<(String, bool)> {
    let verb = match (plan.rename_to.is_some(), plan.mode) {
        (true, _) => "Renommage",
        (false, Mode::Move) => "Déplacement",
        (false, Mode::Copy) => "Copie",
    };
    let (singular, plural) = match plan.mode {
        Mode::Move => ("déplacé", "déplacés"),
        Mode::Copy => ("copié", "copiés"),
    };
    let dest = folder_name(&plan.dest);
    if cancelled {
        let text = format!("{verb} interrompu{} : {} sur {}", if plan.mode == Mode::Copy { "e" } else { "" }, done.len(), targets.len());
        return Some((text, false));
    }
    if let Some(first) = failures.first() {
        let text = if done.is_empty() {
            format!("{verb} impossible : {first}")
        } else {
            format!("{} {plural}, {} : {first}", format::count(done.len(), "élément", "éléments"), format::count(failures.len(), "échec", "échecs"))
        };
        return Some((text, true));
    }
    if plan.rename_to.is_some() || done.is_empty() {
        return None;
    }
    let here = targets.iter().all(|target| target.item.path.parent().as_ref() == Some(&plan.dest));
    let text = match done {
        [only] if here && plan.mode == Mode::Copy => format!("« {only} » créé"),
        [only] => format!("« {only} » {singular} dans {dest}"),
        many => format!("{} {plural} dans {dest}", format::count(many.len(), "élément", "éléments")),
    };
    Some((text, false))
}

/// Which entries a recursive change reaches.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Scope {
    #[default]
    All,
    Files,
    Folders,
}

/// User and group ids to give; `None` keeps the current one.
pub type Owner = (Option<u32>, Option<u32>);

/// A change of permissions and/or owner.
pub struct Change {
    pub storage: Arc<dyn Storage>,
    pub items: Vec<Picked>,
    pub mode: Option<u32>,
    pub owner: Option<Owner>,
    /// Into the folders, reaching what this says.
    pub recursive: Option<Scope>,
}

/// The entries a change reaches: files first, then folders deepest first,
/// so that a folder losing its rights is changed after what it holds.
async fn reached(change: &Change, progress: &Progress) -> Result<Vec<(RemotePath, bool)>> {
    let (mut files, mut folders) = (Vec::new(), Vec::new());
    for item in &change.items {
        let Some(scope) = change.recursive.filter(|_| item.is_dir) else {
            if item.is_dir {
                folders.push(item.path.clone());
            } else {
                files.push(item.path.clone());
            }
            continue;
        };
        let tree = walk_tree(&*change.storage, &item.path, progress).await?;
        if scope != Scope::Folders {
            files.extend(tree.files.iter().map(|(file, _)| item.path.child(file)));
        }
        if scope != Scope::Files {
            folders.push(item.path.clone());
            folders.extend(tree.folders.iter().map(|folder| item.path.child(folder)));
        }
    }
    folders.sort_by_key(|folder: &RemotePath| std::cmp::Reverse(folder.segments().len()));
    Ok(files
        .into_iter()
        .map(|file| (file, false))
        .chain(folders.into_iter().map(|folder| (folder, true)))
        .collect())
}

/// Applies a change of permissions or owner, and says how it went.
pub async fn run_change(change: Change, job: Arc<Job>, reply: Reply) {
    let storage = storage_key(&change.storage);
    let mut changed: Vec<RemotePath> = Vec::new();
    for item in &change.items {
        if let Some(parent) = item.path.parent()
            && !changed.contains(&parent)
        {
            changed.push(parent);
        }
        if change.recursive.is_some() && item.is_dir {
            changed.push(item.path.clone());
        }
    }
    let finish = |notice: (String, bool), changed: Vec<RemotePath>| OpsEvent::Finished {
        job: job.id,
        storage,
        changed,
        moved: Vec::new(),
        select: None,
        notice: Some(notice),
    };

    let entries = match reached(&change, &job.progress).await {
        Ok(entries) => entries,
        Err(_) if job.progress.is_cancelled() => return reply.send(finish(("Modification annulée".to_owned(), false), changed)),
        Err(error) => return reply.send(finish((format!("Modification impossible : {error:#}"), true), changed)),
    };
    job.count.store(entries.len() as u64, Ordering::Relaxed);

    // Files at once; folders one depth after the other, deepest first.
    let (target, mode, owner, counter) = (&change.storage, change.mode, change.owner, &job);
    let apply = move |path: RemotePath| async move {
        let applied: Result<()> = async {
            if counter.progress.is_cancelled() {
                anyhow::bail!("annulé");
            }
            if let Some(mode) = mode {
                target.set_permissions(&path, mode).await?;
            }
            if let Some((uid, gid)) = owner {
                target.set_owner(&path, uid, gid).await?;
            }
            counter.done.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        .await;
        applied.map_err(|error| format!("{} : {error:#}", path.name()))
    };
    let mut failures: Vec<String> = Vec::new();
    let mut index = 0;
    while index < entries.len() {
        // A run of files, or of folders at one depth.
        let (_, is_dir) = entries[index];
        let depth = entries[index].0.segments().len();
        let end = entries[index..]
            .iter()
            .position(|(path, dir)| *dir != is_dir || (is_dir && path.segments().len() != depth))
            .map_or(entries.len(), |offset| index + offset);
        let batch: Vec<RemotePath> = entries[index..end].iter().map(|(path, _)| path.clone()).collect();
        let results: Vec<Result<(), String>> = stream::iter(batch)
            .map(apply)
            .buffer_unordered(PARALLEL_CHANGES)
            .collect()
            .await;
        failures.extend(results.into_iter().filter_map(Result::err));
        index = end;
    }

    let count = entries.len();
    let what = match (change.mode, change.owner) {
        (Some(_), Some(_)) => "Permissions et propriétaire",
        (Some(_), None) => "Permissions",
        _ => "Propriétaire",
    };
    let notice = if job.progress.is_cancelled() {
        (format!("Modification interrompue : {} sur {count}", job.done.load(Ordering::Relaxed)), false)
    } else if let Some(first) = failures.first() {
        (format!("{what} : {} sur {count} — {first}", format::count(failures.len(), "échec", "échecs")), true)
    } else {
        match (count, change.mode, &change.items[..]) {
            (1, Some(mode), [only]) => (format!("{what} de « {} » : {}", only.path.name(), mode_text(mode)), false),
            _ => (format!("{what} : {} modifiés", format::count(count, "élément", "éléments")), false),
        }
    };
    reply.send(finish(notice, changed));
}

pub async fn run_properties(storage: Arc<dyn Storage>, item: Picked, request: u64, reply: Reply) {
    let result = storage
        .properties(&item.path, item.is_dir)
        .await
        .map_err(|error| format!("{error:#}"));
    reply.send(OpsEvent::Properties { request, result });
}

/// The user and group names, cached by the connection once read.
pub async fn run_accounts(storage: Arc<dyn Storage>, reply: Reply) {
    let accounts = storage.accounts().await.unwrap_or_default();
    reply.send(OpsEvent::Accounts {
        storage: storage_key(&storage),
        accounts: Arc::new(accounts),
    });
}

pub async fn run_size(storage: Arc<dyn Storage>, path: RemotePath, sizing: Arc<Sizing>, reply: Reply) {
    let result = storage.folder_size(&path, &sizing.tally).await;
    let error = match result {
        Ok(()) => None,
        Err(_) if sizing.tally.is_cancelled() => Some("annulé".to_owned()),
        Err(error) => Some(format!("Taille de « {} » : {error:#}", path.name())),
    };
    if let Ok(mut slot) = sizing.error.lock() {
        slot.clone_from(&error);
    }
    sizing.finished.store(true, Ordering::Relaxed);
    reply.send(OpsEvent::Sized { error });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picked(path: &str, is_dir: bool) -> Picked {
        Picked {
            path: RemotePath::parse(path),
            is_dir,
            size: 0,
        }
    }

    fn existing(names: &[(&str, bool)]) -> HashMap<String, bool> {
        names.iter().map(|(name, is_dir)| (name.to_string(), *is_dir)).collect()
    }

    #[test]
    fn a_folder_cannot_go_inside_itself() {
        let photos = picked("/srv/photos", true);
        let refused = |dest: &str| refuse(std::slice::from_ref(&photos), &RemotePath::parse(dest), false);
        assert!(refused("/srv/photos").is_some(), "onto itself");
        assert!(refused("/srv/photos/2024/été").is_some(), "below itself");
        assert_eq!(refused("/srv/photos 2"), None, "a name that merely starts the same");
        assert_eq!(refused("/srv"), None);
        assert_eq!(refused("/"), None);
        // A file of that name is not a folder.
        assert_eq!(refuse(&[picked("/srv/photos", false)], &RemotePath::parse("/srv/photos/x"), false), None);
        // The root of S3 only holds buckets.
        assert!(refuse(&[picked("/b/x", false)], &RemotePath::default(), true).is_some());
        assert_eq!(refuse(&[picked("/b/x", false)], &RemotePath::parse("/c"), true), None);
    }

    #[test]
    fn moved_paths_follow_their_folder() {
        let (from, to) = (RemotePath::parse("/a/b"), RemotePath::parse("/c/b2"));
        assert_eq!(moved_path(&RemotePath::parse("/a/b"), &from, &to), Some(to.clone()));
        assert_eq!(moved_path(&RemotePath::parse("/a/b/x/y"), &from, &to), Some(RemotePath::parse("/c/b2/x/y")));
        assert_eq!(moved_path(&RemotePath::parse("/a/bc"), &from, &to), None);
        assert_eq!(moved_path(&RemotePath::parse("/a"), &from, &to), None);
    }

    #[test]
    fn copies_in_the_same_folder_get_a_free_name() {
        let taken: HashSet<String> = ["rapport.pdf", "rapport (copie).pdf", "photos"].iter().map(|s| s.to_string()).collect();
        assert_eq!(copy_name("rapport.pdf", false, &taken), "rapport (copie 2).pdf");
        assert_eq!(copy_name("photos", true, &taken), "photos (copie)");
        assert_eq!(copy_name("archive.tar.gz", false, &taken), "archive.tar (copie).gz");
        assert_eq!(copy_name(".bashrc", false, &taken), ".bashrc (copie)");
        assert_eq!(copy_name("v1.2", true, &taken), "v1.2 (copie)", "folders have no extension");
        assert_eq!(stem("notes.final.txt", false), "notes.final");
        assert_eq!(stem("Makefile", false), "Makefile");
    }

    #[test]
    fn targets_name_what_goes_where() {
        let dest = RemotePath::parse("/srv/dest");
        let items = [picked("/srv/a.txt", false), picked("/srv/dir", true), picked("/srv/dest/kept.txt", false)];
        let there = existing(&[("dir", true), ("kept.txt", false)]);

        // A move: clashes are reported, what is already there is left out.
        let moved = targets(Mode::Move, &items, None, &dest, &there);
        let summary: Vec<(&str, Option<bool>)> = moved.iter().map(|t| (t.name.as_str(), t.clash)).collect();
        assert_eq!(summary, [("a.txt", None), ("dir", Some(true))]);

        // A copy into its own folder gets a new name, and two copies of the
        // same name do not take the same one.
        let twice = [picked("/srv/dest/kept.txt", false), picked("/srv/dest/kept.txt", false)];
        let copied = targets(Mode::Copy, &twice, None, &dest, &there);
        let names: Vec<&str> = copied.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["kept (copie).txt", "kept (copie 2).txt"]);
        assert!(copied.iter().all(|t| t.clash.is_none()));

        // A rename in place.
        let renamed = targets(Mode::Move, &items[2..], Some("new.txt"), &dest, &there);
        assert_eq!((renamed[0].name.as_str(), renamed[0].clash), ("new.txt", None));
        let onto = targets(Mode::Move, &items[2..], Some("dir"), &dest, &there);
        assert_eq!(onto[0].clash, Some(true));
        // Renamed to its own name: nothing to do.
        assert!(targets(Mode::Move, &items[2..], Some("kept.txt"), &dest, &there).is_empty());
    }

    #[test]
    fn typed_paths_are_absolute_or_relative() {
        let from = RemotePath::parse("/srv/www/site");
        assert_eq!(resolve_path(&from, "/tmp/x/"), RemotePath::parse("/tmp/x"));
        assert_eq!(resolve_path(&from, "old"), RemotePath::parse("/srv/www/site/old"));
        assert_eq!(resolve_path(&from, "../other/./a"), RemotePath::parse("/srv/www/other/a"));
        assert_eq!(resolve_path(&from, "../../../../.."), RemotePath::default());
        assert_eq!(resolve_path(&from, " / "), RemotePath::default());
    }

    #[test]
    fn names_are_checked() {
        assert_eq!(check_name("  notes.txt "), Ok("notes.txt"));
        assert!(check_name("   ").is_err());
        assert!(check_name("a/b").is_err());
        assert!(check_name("..").is_err());
    }

    #[test]
    fn bookmarks_toggle_and_survive_a_round_trip() {
        let mut bookmarks = Vec::new();
        let logs = RemotePath::parse("/var/log");
        assert!(toggle_bookmark(&mut bookmarks, &logs, "Serveur"));
        assert!(toggle_bookmark(&mut bookmarks, &RemotePath::default(), "Serveur"));
        assert_eq!(bookmarks[0], Bookmark { name: "log".into(), path: "/var/log".into() });
        assert_eq!(bookmarks[1].name, "Serveur", "the root is named after the connection");
        assert_eq!(bookmark_of(&bookmarks, &logs), Some(0));
        assert!(!toggle_bookmark(&mut bookmarks, &logs, "Serveur"), "a second time removes it");
        assert_eq!(bookmark_of(&bookmarks, &logs), None);

        // Saved with the connection, and absent from older files.
        let mut profile = crate::profiles::Profile::new();
        profile.bookmarks = vec![Bookmark { name: "Logs".into(), path: "/var/log".into() }];
        let saved = serde_json::to_string(&[&profile]).unwrap();
        let back: Vec<crate::profiles::Profile> = serde_json::from_str(&saved).unwrap();
        assert_eq!(back[0].bookmarks, profile.bookmarks);
        let old: Vec<crate::profiles::Profile> =
            serde_json::from_str(r#"[{"id":"1","name":"Old","protocol":"sftp","endpoint":"h","access_key":"u"}]"#).unwrap();
        assert!(old[0].bookmarks.is_empty());
        let partial: Bookmark = serde_json::from_str(r#"{"path":"/x"}"#).unwrap();
        assert_eq!(partial.name, "");
    }

    #[test]
    fn columns_are_off_until_asked_for() {
        let settings: crate::profiles::Settings = serde_json::from_str(r#"{"dark": true}"#).unwrap();
        assert_eq!(settings.file_columns, Columns::default());
        assert!(!settings.file_columns.permissions && !settings.file_columns.owner);
    }

    /// What the last job said: its notice, and whether it is an error.
    fn finished(state: &OpsState) -> (String, bool) {
        let mut last = None;
        while let Ok(event) = state.events.try_recv() {
            if let OpsEvent::Finished { notice, .. } = event {
                last = Some(notice.unwrap_or_default());
            }
        }
        last.expect("a finished job")
    }

    async fn modes(storage: &dyn Storage, folder: &RemotePath) -> HashMap<String, u32> {
        list_all(storage, folder)
            .await
            .unwrap()
            .into_iter()
            .map(|entry| (entry.name.clone(), entry.permissions.unwrap_or(0)))
            .collect()
    }

    /// The jobs themselves, against the servers of the live tests:
    /// recursive changes reaching what they should, clashes asked about,
    /// replaced or skipped, and folder sizes.
    async fn jobs(storage: Arc<dyn Storage>) {
        let home = storage.home().await.unwrap();
        let root = home.child("file-ops-test");
        let _ = storage.create_folder(&root).await;
        let base = root.child(&format!("jobs-{}", crate::storage::ops_live_tests::stamp()));
        let local = std::env::temp_dir().join(format!("s3v-jobs-{}", crate::storage::ops_live_tests::stamp()));
        std::fs::write(&local, b"0123456789").unwrap();
        for file in ["d/x.txt", "d/sub/y.txt", "z.txt"] {
            storage.upload(&local, &base.child(file), &Progress::default()).await.unwrap();
        }
        let mut state = OpsState::default();
        let reply = state.reply(&egui::Context::default());
        let (d, sub) = (base.child("d"), base.child("d/sub"));
        let folder = Picked { path: d.clone(), is_dir: true, size: 0 };

        // Files only, then folders only.
        let change = |mode, scope| Change {
            storage: storage.clone(),
            items: vec![folder.clone()],
            mode: Some(mode),
            owner: None,
            recursive: Some(scope),
        };
        run_change(change(0o600, Scope::Files), state.start_job(String::new(), 1, 0.0), reply.clone()).await;
        assert!(!finished(&state).1);
        assert_eq!(modes(&*storage, &d).await["x.txt"], 0o600);
        assert_eq!(modes(&*storage, &sub).await["y.txt"], 0o600);
        assert_eq!(modes(&*storage, &d).await["sub"], 0o755, "folders untouched");
        run_change(change(0o750, Scope::Folders), state.start_job(String::new(), 1, 0.0), reply.clone()).await;
        let (notice, error) = finished(&state);
        assert!(!error, "{notice}");
        assert_eq!(modes(&*storage, &base).await["d"], 0o750);
        assert_eq!(modes(&*storage, &d).await["sub"], 0o750);
        assert_eq!(modes(&*storage, &d).await["x.txt"], 0o600, "files untouched");

        // A copy onto an existing name asks first, then replaces or skips.
        let z = Picked { path: base.child("z.txt"), is_dir: false, size: 10 };
        let plan = |existing| Plan {
            storage: storage.clone(),
            mode: Mode::Copy,
            items: vec![z.clone()],
            dest: d.clone(),
            rename_to: None,
            existing,
            known: None,
        };
        run_plan(plan(Existing::Ask), state.start_job(String::new(), 1, 0.0), reply.clone()).await;
        assert_eq!(finished(&state), ("« z.txt » copié dans « d »".to_owned(), false));
        run_plan(plan(Existing::Ask), state.start_job(String::new(), 1, 0.0), reply.clone()).await;
        match state.events.try_recv() {
            Ok(OpsEvent::Clashes { clashes, .. }) => assert_eq!(clashes, [("z.txt".to_owned(), false)]),
            _ => panic!("the clash is asked about"),
        }
        run_plan(plan(Existing::Skip), state.start_job(String::new(), 1, 0.0), reply.clone()).await;
        assert!(!finished(&state).1);
        run_plan(plan(Existing::Replace), state.start_job(String::new(), 1, 0.0), reply.clone()).await;
        assert_eq!(finished(&state).0, "« z.txt » copié dans « d »");

        // A move into a folder that does not exist yet creates it.
        let moved = Plan {
            mode: Mode::Move,
            dest: base.child("new/place"),
            ..plan(Existing::Ask)
        };
        run_plan(moved, state.start_job(String::new(), 1, 0.0), reply.clone()).await;
        let (notice, error) = finished(&state);
        assert!(!error, "{notice}");
        assert!(modes(&*storage, &base.child("new/place")).await.contains_key("z.txt"));

        let sizing = Arc::new(Sizing {
            tally: Tally::default(),
            finished: AtomicBool::new(false),
            error: Mutex::new(None),
            started: 0.0,
        });
        run_size(storage.clone(), base.clone(), sizing.clone(), reply.clone()).await;
        let size = sizing.tally.snapshot();
        assert_eq!((size.files, size.bytes), (4, 40));
        assert!(!sizing.is_running() && !sizing.failed());

        storage.delete(&base, true).await.unwrap();
        let _ = std::fs::remove_file(local);
    }

    #[tokio::test]
    async fn live_jobs_over_sftp() {
        if let Some(storage) = crate::storage::ops_live_tests::sftp() {
            jobs(Arc::new(storage)).await;
        }
    }

    #[tokio::test]
    async fn live_jobs_over_ftp() {
        if let Some(storage) = crate::storage::ops_live_tests::ftp() {
            jobs(Arc::new(storage)).await;
        }
    }

    #[test]
    fn sizes_are_forgotten_with_what_changes() {
        let mut state = OpsState::default();
        let sized = |path: &str| {
            (
                (7, RemotePath::parse(path)),
                Arc::new(Sizing {
                    tally: Tally::default(),
                    finished: AtomicBool::new(true),
                    error: Mutex::new(None),
                    started: 0.0,
                }),
            )
        };
        state.sizes.extend([sized("/a"), sized("/a/b"), sized("/a/b/c"), sized("/a/d"), sized("/e")]);
        state.sizes.insert((8, RemotePath::parse("/a/b")), sized("/a/b").1);
        state.forget_sizes(7, &RemotePath::parse("/a/b"));
        let mut kept: Vec<String> = state.sizes.keys().map(|(key, path)| format!("{key}{path}")).collect();
        kept.sort();
        assert_eq!(kept, ["7/a/d", "7/e", "8/a/b"]);
        assert!(state.sizes_changed);
    }
}
