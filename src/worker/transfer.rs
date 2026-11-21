//! Downloads and uploads that follow the rules of [`crate::transfers`]:
//! existing destinations, partial copies that are completed, pause and
//! resume, retries, speed limits; folder synchronisation.
//!
//! A folder is one transfer in the list, whatever it holds: its files are
//! transfers of their own that follow the same rules, but are not listed
//! (see [`Folder`]). A folder that is not here yet may come down from an
//! SFTP server in one stream (see `Storage::download_tree`).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use futures_util::{StreamExt, stream};
use tokio::sync::{Notify, oneshot};

use super::{
    Direction, Event, Handle, Item, Transfer, TransferState, describe, safe_component, safe_components, tidy,
};
use crate::format;
use crate::storage::{Entry, Found, Progress, RemotePath, Storage, TreeProgress, TreeReport, partial_path};
use crate::transfers::conflict::{self, ConflictPolicy, Resolution, Version};
use crate::transfers::control::{Control, Request};
use crate::transfers::limit::Limiter;
use crate::transfers::lock;
use crate::transfers::slots::{Slots, Ticket};
use crate::transfers::sync::{FileInfo, Rules, SyncAction, SyncLine};
use crate::transfers::SMALL_FILE;

/// Deletions of a synchronisation that run side by side.
const PARALLEL_DELETES: usize = 8;
/// How often a folder coming down in one stream looks at the speed limit:
/// one set meanwhile sends the rest file by file, each under the limit.
const STREAM_WATCH: Duration = Duration::from_millis(200);

/// What the worker tells the app about transfers.
pub enum TransferEvent {
    /// The destination of a transfer exists: the user decides.
    Ask(Question),
    /// The comparison of a synchronisation, for its preview.
    Compared {
        id: u64,
        result: Result<Vec<SyncLine>, String>,
    },
}

/// The answer to a [`Question`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Answer {
    /// Anything but [`ConflictPolicy::Ask`].
    pub policy: ConflictPolicy,
    /// For the other conflicts of the same batch too.
    pub all: bool,
}

/// "This file exists already": asked about one transfer at a time. A
/// question dropped without an answer skips the file.
pub struct Question {
    pub transfer: Arc<Transfer>,
    pub batch: u64,
    pub name: String,
    /// The folder the copy goes to, for display.
    pub folder: String,
    pub direction: Direction,
    pub source: Version,
    pub existing: Version,
    /// Whether the existing file can be completed rather than replaced.
    pub can_resume: bool,
    /// The name a renamed copy gets.
    pub renamed: String,
    /// Other conflicts of the same batch still to settle.
    pub others: usize,
    /// Folders of the batch are still gone through: more conflicts may come.
    pub more: bool,
    answer: Option<oneshot::Sender<Answer>>,
}

impl Question {
    pub fn answer(mut self, answer: Answer) {
        if let Some(sender) = self.answer.take() {
            let _ = sender.send(answer);
        }
    }

    /// Answered elsewhere, cancelled or gone: no longer worth asking.
    pub fn is_stale(&self) -> bool {
        self.answer.as_ref().is_none_or(|sender| sender.is_closed()) || self.transfer.state().is_finished()
    }
}

/// Transfers asked for together: what "Appliquer à tous" applies to. The
/// files of the folders asked for belong to it too.
struct Batch {
    id: u64,
    /// The rule in force: the default one, until a question is answered for
    /// all.
    policy: Mutex<ConflictPolicy>,
    /// One question at a time, so that an answer for all reaches the rest.
    asking: tokio::sync::Mutex<()>,
    /// Conflicts not settled yet.
    unsettled: AtomicUsize,
    /// Names given to renamed copies, so that two never get the same.
    reserved: Mutex<HashSet<String>>,
    /// Folders of the batch whose files are still being found.
    searching: AtomicUsize,
}

/// Counts a folder of the batch as being gone through, while it lives.
struct Searching(Arc<Batch>);

impl Searching {
    fn new(batch: &Arc<Batch>) -> Self {
        batch.searching.fetch_add(1, Ordering::Relaxed);
        Self(batch.clone())
    }
}

impl Drop for Searching {
    fn drop(&mut self) {
        self.0.searching.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Batch {
    fn new(handle: &Handle, policy: ConflictPolicy) -> Arc<Self> {
        Arc::new(Self {
            id: handle.next_id.fetch_add(1, Ordering::Relaxed),
            policy: Mutex::new(policy),
            asking: tokio::sync::Mutex::new(()),
            unsettled: AtomicUsize::new(0),
            reserved: Mutex::new(HashSet::new()),
            searching: AtomicUsize::new(0),
        })
    }

    fn policy(&self) -> ConflictPolicy {
        *lock(&self.policy)
    }

    fn settled(&self) {
        let _ = self
            .unsettled
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |left| left.checked_sub(1));
    }
}

/// How a transfer starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Fresh,
    /// Complete what an earlier attempt left: a `.part` file for a
    /// download, the remote file for an upload.
    Continue,
    /// Complete the destination itself, a partial copy that was there.
    ContinueInPlace,
}

/// One file to transfer.
struct Job {
    storage: Arc<dyn Storage>,
    direction: Direction,
    remote: RemotePath,
    local: PathBuf,
    source: Version,
    /// What is in the way at the destination.
    existing: Option<Version>,
    /// Starts by completing a partial copy left by an earlier attempt.
    resume: bool,
    /// Gives the copy the date of its original: synchronisations compare
    /// dates.
    keep_date: bool,
    /// A small file of a folder: it waits for one of the extra turns of
    /// the small files rather than for one of those the user counts.
    small: bool,
}

impl Job {
    fn can_resume(&self) -> bool {
        match self.direction {
            Direction::Download => self.storage.can_resume_download(),
            Direction::Upload => self.storage.can_resume_upload(),
        }
    }

    fn name(&self) -> String {
        self.remote.name().to_owned()
    }

    fn target(&self) -> String {
        match self.direction {
            Direction::Download => self.local.display().to_string(),
            Direction::Upload => self.remote.to_string(),
        }
    }

    /// The turns it waits for.
    fn slots<'a>(&self, handle: &'a Handle) -> &'a Arc<Slots> {
        if self.small { &handle.shared.small } else { &handle.shared.slots }
    }
}

impl Transfer {
    /// Stops it, keeping what was transferred, until it is resumed.
    pub fn pause(&self) {
        if let Some(folder) = &self.folder {
            return folder.pause();
        }
        if let Some(control) = &self.control {
            self.progress.pause();
            control.send(Request::Pause);
        }
    }

    /// Goes on after a pause, or tries again after a failure.
    pub fn resume(&self) {
        if let Some(folder) = &self.folder {
            return folder.resume();
        }
        if let Some(control) = &self.control {
            control.send(Request::Resume);
        }
    }

    /// Stops the transfer; for a folder, the search and every file of it.
    pub fn cancel(&self) {
        self.progress.cancel();
        if let Some(folder) = &self.folder {
            return folder.cancel();
        }
        if let Some(control) = &self.control {
            control.send(Request::Cancel);
        }
    }

    /// Out of the list: nothing will be asked of it any more.
    pub fn dismiss(&self) {
        if let Some(folder) = &self.folder {
            return folder.dismiss();
        }
        if let Some(control) = &self.control {
            control.send(Request::Dismiss);
        }
    }

    /// Whether the panel can pause it: only transfers of this module can.
    pub fn can_pause(&self) -> bool {
        self.control.is_some() || self.folder.is_some()
    }

    /// Whether pausing it now means something. A file waiting for an answer
    /// is not paused (the question would go); a folder is, those of its
    /// files being asked about aside.
    pub fn can_pause_now(&self) -> bool {
        self.can_pause() && (self.folder.is_some() || !self.asking())
    }

    /// What it stops at is kept when paused.
    pub fn resumable(&self) -> bool {
        match &self.folder {
            Some(folder) => folder.resumable,
            None => self.control.as_ref().is_some_and(|control| control.resumable()),
        }
    }

    /// Waiting for the user to say what to do with an existing file; for a
    /// folder, with one of its files.
    pub fn asking(&self) -> bool {
        match &self.folder {
            Some(folder) => lock(&folder.files).iter().any(|file| file.asking()),
            None => self.control.as_ref().is_some_and(|control| control.asking()),
        }
    }

    /// Shown in place of the destination: a new name, a resumed copy…
    pub fn note(&self) -> Option<String> {
        self.control.as_ref().and_then(|control| control.note())
    }

    /// The bytes transferred so far, those of the files of a folder
    /// included: what its speed is measured on.
    pub fn bytes_done(&self) -> u64 {
        match &self.folder {
            Some(folder) => folder.summary().bytes_done,
            None => self.progress.snapshot().0,
        }
    }
}

pub(super) fn limiter(handle: &Handle, direction: Direction) -> Arc<Limiter> {
    match direction {
        Direction::Download => handle.shared.download.clone(),
        Direction::Upload => handle.shared.upload.clone(),
    }
}

fn unix(time: SystemTime) -> Option<i64> {
    time.duration_since(UNIX_EPOCH).ok().map(|elapsed| elapsed.as_secs() as i64)
}

/// The local file at `path`, if there is one and it is a file.
fn local_version(path: &Path) -> Option<Version> {
    let meta = std::fs::metadata(path).ok().filter(|meta| meta.is_file())?;
    Some(Version {
        size: meta.len(),
        modified: meta.modified().ok().and_then(unix),
    })
}

/// Everything `storage` lists in `path`, every page of it.
async fn list_all(storage: &Arc<dyn Storage>, path: &RemotePath) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    let mut cursor = None;
    loop {
        let page = storage.list_page(path, cursor).await?;
        entries.extend(page.entries);
        cursor = page.next;
        if cursor.is_none() {
            return Ok(entries);
        }
    }
}

/// The dates of remote files: the rules compare them, and recursive
/// listings leave them out. Each folder is listed once, and only for the
/// files that are in the way.
#[derive(Default)]
struct Dates(HashMap<RemotePath, HashMap<String, Option<i64>>>);

impl Dates {
    async fn of(&mut self, storage: &Arc<dyn Storage>, path: &RemotePath) -> Option<i64> {
        let folder = path.parent()?;
        if !self.0.contains_key(&folder) {
            let listed = list_all(storage, &folder).await.unwrap_or_default();
            let named = listed.into_iter().map(|entry| (entry.name, entry.modified)).collect();
            self.0.insert(folder.clone(), named);
        }
        self.0.get(&folder)?.get(path.name()).copied().flatten()
    }
}

/// What the destination folders of an upload hold already. A folder is
/// only listed when the one above has it, so a new tree costs no listing
/// below where it goes.
struct Listings {
    storage: Arc<dyn Storage>,
    /// Where the upload goes: a folder that exists.
    base: RemotePath,
    folders: HashMap<RemotePath, HashMap<String, Entry>>,
}

impl Listings {
    fn new(storage: Arc<dyn Storage>, base: RemotePath) -> Self {
        Self {
            storage,
            base,
            folders: HashMap::new(),
        }
    }

    /// What is at `path` already, if anything.
    async fn at(&mut self, path: &RemotePath) -> Option<&Entry> {
        let folder = path.parent()?;
        self.load(&folder).await;
        self.folders.get(&folder)?.get(path.name())
    }

    async fn load(&mut self, folder: &RemotePath) {
        if self.folders.contains_key(folder) {
            return;
        }
        let top = self.base.segments().len().min(folder.segments().len());
        for depth in top..=folder.segments().len() {
            let level = folder.ancestor(depth);
            if self.folders.contains_key(&level) {
                continue;
            }
            let there = depth == top
                || level.parent().is_some_and(|parent| {
                    self.folders
                        .get(&parent)
                        .is_some_and(|entries| entries.get(level.name()).is_some_and(Entry::is_dir))
                });
            let entries = if there {
                let listed = list_all(&self.storage, &level).await.unwrap_or_default();
                listed.into_iter().map(|entry| (entry.name.clone(), entry)).collect()
            } else {
                HashMap::new()
            };
            self.folders.insert(level, entries);
        }
    }
}

/// Downloads files and whole folders into `dest_dir`.
pub(super) async fn download(handle: Handle, storage: Arc<dyn Storage>, items: Vec<Item>, dest_dir: PathBuf) {
    let batch = Batch::new(&handle, handle.shared.policy());
    let mut files: Vec<(RemotePath, PathBuf, Version)> = Vec::new();
    for item in items {
        let local = dest_dir.join(safe_component(item.path.name()));
        if item.is_dir {
            let target = local.display().to_string();
            let transfer = add_folder(&handle, item.path.name(), target, Direction::Download, &storage);
            let (handle, searching, storage) = (handle.clone(), Searching::new(&batch), storage.clone());
            tokio::spawn(download_folder(handle, searching, storage, transfer, item.path, local));
            continue;
        }
        let version = Version {
            size: item.size,
            modified: None,
        };
        files.push((item.path, local, version));
    }

    let mut dates = Dates::default();
    let mut jobs = Vec::new();
    for (remote, local, mut source) in files {
        if local.is_dir() {
            handle.notice(
                format!("« {} » n'est pas téléchargé : un dossier porte ce nom.", local.display()),
                true,
            );
            continue;
        }
        let existing = local_version(&local);
        if existing.is_some() {
            source.modified = dates.of(&storage, &remote).await;
        }
        // A partial copy of an earlier attempt is completed, as long as it
        // can be the start of the file.
        let partial = local_version(&partial_path(&local)).map_or(0, |part| part.size);
        let resume = existing.is_none() && storage.can_resume_download() && partial > 0 && partial < source.size;
        jobs.push(Job {
            storage: storage.clone(),
            direction: Direction::Download,
            remote,
            local,
            source,
            existing,
            resume,
            keep_date: false,
            small: false,
        });
    }
    start(&handle, &batch, jobs);
}

/// Uploads local files and folders (recursively) into `dest`.
pub(super) async fn upload(handle: Handle, storage: Arc<dyn Storage>, sources: Vec<PathBuf>, dest: RemotePath) {
    let batch = Batch::new(&handle, handle.shared.policy());
    let mut files = Vec::new();
    let mut folders = 0;
    for source in sources {
        let Some(name) = source.file_name() else {
            continue;
        };
        let name = name.to_string_lossy().into_owned();
        if source.is_dir() {
            folders += 1;
            let remote = dest.child(&name);
            let transfer = add_folder(&handle, &name, remote.to_string(), Direction::Upload, &storage);
            let (handle, searching, storage) = (handle.clone(), Searching::new(&batch), storage.clone());
            tokio::spawn(upload_folder(handle, searching, storage, transfer, source, remote));
        } else if let Some(version) = local_version(&source) {
            files.push((source, name, version));
        }
    }
    if files.is_empty() {
        if folders == 0 {
            handle.notice("Aucun fichier à importer", false);
        }
        return;
    }

    let mut listings = Listings::new(storage.clone(), dest.clone());
    let mut jobs = Vec::new();
    for (local, name, source) in files {
        let remote = dest.child(&name);
        let there = listings.at(&remote).await;
        if there.is_some_and(Entry::is_dir) {
            handle.notice(format!("« {remote} » n'est pas envoyé : un dossier porte ce nom."), true);
            continue;
        }
        let existing = there.map(|entry| Version {
            size: entry.size.unwrap_or(0),
            modified: entry.modified,
        });
        jobs.push(Job {
            storage: storage.clone(),
            direction: Direction::Upload,
            remote,
            local,
            source,
            existing,
            resume: false,
            keep_date: false,
            small: false,
        });
    }
    start(&handle, &batch, jobs);
}

/// Every regular file below `dir`, with its path relative to the parent of
/// `dir` (which starts with `prefix`), its size and its date.
fn walk_local(dir: &Path, prefix: &str, files: &mut Vec<(PathBuf, String, Version)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let relative = format!("{prefix}/{}", entry.file_name().to_string_lossy());
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => walk_local(&entry.path(), &relative, files),
            Ok(kind) if kind.is_file() => {
                if let Some(version) = local_version(&entry.path()) {
                    files.push((entry.path(), relative, version));
                }
            }
            _ => {}
        }
    }
}

/// What a local folder to upload holds.
#[derive(Debug, Default, PartialEq)]
struct Scan {
    /// With their path relative to the folder, `/`-separated.
    files: Vec<(PathBuf, String, Version)>,
    /// The folders that hold nothing at all, the folder itself (`""`)
    /// included: the files bring the others along.
    empty: Vec<String>,
}

fn scan_folder(dir: &Path) -> Scan {
    let mut scan = Scan::default();
    scan_into(dir, "", &mut scan);
    scan
}

fn scan_into(dir: &Path, prefix: &str, scan: &mut Scan) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut holds = false;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let relative = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => {
                holds = true;
                scan_into(&entry.path(), &relative, scan);
            }
            Ok(kind) if kind.is_file() => {
                if let Some(version) = local_version(&entry.path()) {
                    holds = true;
                    scan.files.push((entry.path(), relative, version));
                }
            }
            _ => {}
        }
    }
    if !holds {
        scan.empty.push(prefix.to_owned());
    }
}

/// Queues the jobs of one batch.
fn start(handle: &Handle, batch: &Arc<Batch>, jobs: Vec<Job>) {
    let conflicts = jobs.iter().filter(|job| job.existing.is_some()).count();
    batch.unsettled.fetch_add(conflicts, Ordering::Relaxed);
    for job in jobs {
        spawn(handle, batch, job);
    }
}

/// The transfer of one file, not started yet.
fn new_transfer(handle: &Handle, job: &Job) -> (Arc<Transfer>, Arc<Control>) {
    let control = Arc::new(Control::default());
    control.set_resumable(job.can_resume());
    if job.resume {
        control.set_note("Reprise de la copie partielle");
    }
    let transfer = Arc::new(Transfer {
        id: handle.next_id.fetch_add(1, Ordering::Relaxed),
        name: job.name(),
        target: job.target(),
        direction: job.direction,
        quiet: false,
        progress: Progress::default(),
        folder: None,
        state: Mutex::new(TransferState::Queued),
        control: Some(control.clone()),
    });
    transfer.progress.set_total(job.source.size);
    transfer.progress.set_limiter(limiter(handle, job.direction));
    (transfer, control)
}

/// Lists one file and starts it.
fn spawn(handle: &Handle, batch: &Arc<Batch>, job: Job) {
    let (transfer, control) = new_transfer(handle, &job);
    handle.emit(Event::TransferAdded(transfer.clone()));
    // Its place in the queue is taken now, in the order asked for. Not when
    // there is a question first: the answer may be long to come.
    let ticket = job.existing.is_none().then(|| job.slots(handle).ticket(transfer.id, false));
    tokio::spawn(run(handle.clone(), batch.clone(), transfer, control, job, ticket));
}

/// What a folder transfer is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Waiting for its turn to come down in one stream.
    Waiting,
    /// Coming down in one stream.
    Streaming,
    /// Its files are looked for, and go as they are found.
    Walking,
    /// Stopped before every file was found: paused, or the search failed.
    /// It goes on once resumed.
    Held,
    /// Every file is known.
    Over,
}

/// A folder being transferred. Its files are found while the first ones
/// are already on their way, a big tree can take minutes to go through.
/// Each is a transfer of its own, which follows the rules like any other
/// but is not listed; small ones get extra turns (see
/// [`crate::transfers::SMALL_PARALLEL`]).
pub struct Folder {
    phase: Mutex<Phase>,
    files: Mutex<Vec<Arc<Transfer>>>,
    /// Subfolders that could not be read, as `path : why`.
    unreadable: Mutex<Vec<String>>,
    /// Why the search, or the stream, stopped short.
    error: Mutex<Option<String>>,
    /// The folder coming down in one stream, when the connection can: the
    /// files that came that way have no transfer of their own.
    stream: TreeProgress,
    /// The stream was stopped to go on file by file: paused, or limited.
    stream_stopped: AtomicBool,
    /// Paused by the user: the files found meanwhile wait.
    paused: AtomicBool,
    cancelled: AtomicBool,
    dismissed: AtomicBool,
    /// Counts the requests to go on, to wake a search that failed.
    resumed: AtomicU64,
    /// Wakes up the folder's task when one of the above changes.
    wake: Notify,
    /// What a paused file stops at is kept.
    resumable: bool,
}

/// Where a folder transfer stands, for display.
pub struct FolderSummary {
    /// Files are still being looked for.
    pub exploring: bool,
    pub files: usize,
    pub finished: usize,
    /// Of the finished files, those that arrived.
    pub arrived: usize,
    /// Of the finished files, those left as they were at the destination.
    pub skipped: usize,
    /// Of the files not given up.
    pub bytes: u64,
    pub bytes_done: u64,
}

impl Folder {
    fn new(resumable: bool) -> Self {
        Self {
            phase: Mutex::new(Phase::Walking),
            files: Mutex::default(),
            unreadable: Mutex::default(),
            error: Mutex::default(),
            stream: TreeProgress::default(),
            stream_stopped: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            dismissed: AtomicBool::new(false),
            resumed: AtomicU64::new(0),
            wake: Notify::new(),
            resumable,
        }
    }

    fn phase(&self) -> Phase {
        *lock(&self.phase)
    }

    fn set_phase(&self, phase: Phase) {
        *lock(&self.phase) = phase;
    }

    fn paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// Cancelled, or out of the list: nothing more is to be done.
    fn ended(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed) || self.dismissed.load(Ordering::Relaxed)
    }

    /// Waits until `ready` holds, looking again whenever something changes.
    async fn until(&self, ready: impl Fn(&Self) -> bool) {
        loop {
            let notified = self.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if ready(self) {
                return;
            }
            notified.await;
        }
    }

    fn poke(&self) {
        self.wake.notify_waiters();
    }

    fn pause(&self) {
        self.paused.store(true, Ordering::Relaxed);
        if self.phase() == Phase::Streaming {
            self.stop_stream();
        }
        // Files are only added under this lock, after a look at the pause.
        for file in lock(&self.files).iter() {
            if matches!(file.state(), TransferState::Queued | TransferState::Running) && !file.asking() {
                file.pause();
            }
        }
        self.poke();
    }

    /// Goes on after a pause; after a failure, tries again what failed.
    fn resume(&self) {
        self.paused.store(false, Ordering::Relaxed);
        self.resumed.fetch_add(1, Ordering::Relaxed);
        for file in lock(&self.files).iter() {
            if matches!(file.state(), TransferState::Paused | TransferState::Failed(_)) {
                file.resume();
            }
        }
        self.poke();
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
        self.stream.bytes.cancel();
        for file in lock(&self.files).iter() {
            file.cancel();
        }
        self.poke();
    }

    fn dismiss(&self) {
        self.dismissed.store(true, Ordering::Relaxed);
        for file in lock(&self.files).iter() {
            file.dismiss();
        }
        self.poke();
    }

    /// Stops the stream; what came stays, the rest goes file by file.
    fn stop_stream(&self) {
        self.stream_stopped.store(true, Ordering::Relaxed);
        self.stream.bytes.pause();
    }

    /// That of its files, while it is going on; then how it went.
    pub(super) fn state(&self) -> TransferState {
        if self.cancelled.load(Ordering::Relaxed) {
            return TransferState::Cancelled;
        }
        let phase = self.phase();
        let (mut running, mut waiting, mut paused) = (0, 0, 0);
        for file in lock(&self.files).iter() {
            match file.state() {
                TransferState::Running => running += 1,
                TransferState::Queued => waiting += 1,
                TransferState::Paused => paused += 1,
                _ => {}
            }
        }
        let searching = matches!(phase, Phase::Walking | Phase::Waiting);
        // Held without a failure, and not paused: resumed, about to go on.
        let resuming = phase == Phase::Held && lock(&self.error).is_none();
        let walking = phase == Phase::Walking || resuming;
        if phase == Phase::Streaming || running > 0 || (walking && !self.paused()) {
            TransferState::Running
        } else if self.paused() && (waiting + paused > 0 || phase != Phase::Over) {
            TransferState::Paused
        } else if waiting > 0 || searching {
            TransferState::Queued
        } else if paused > 0 {
            TransferState::Paused
        } else {
            match self.problems() {
                None => TransferState::Done,
                Some(problems) => TransferState::Failed(problems),
            }
        }
    }

    pub fn summary(&self) -> FolderSummary {
        let phase = self.phase();
        let counted = self.stream.files.load(Ordering::Relaxed) as usize;
        let streamed = self.stream.files_done.load(Ordering::Relaxed) as usize;
        let (done, total) = self.stream.bytes.snapshot();
        let mut summary = FolderSummary {
            // Counted beforehand, the files of a stream are all known.
            exploring: phase == Phase::Walking || (phase == Phase::Streaming && counted == 0),
            files: counted.max(streamed),
            finished: streamed,
            arrived: streamed,
            skipped: 0,
            bytes: total.max(done),
            bytes_done: done,
        };
        let files = lock(&self.files);
        summary.files += files.len();
        for file in files.iter() {
            let (done, total) = file.progress.snapshot();
            let state = file.state();
            if state.is_finished() {
                summary.finished += 1;
            }
            match state {
                TransferState::Failed(_) | TransferState::Cancelled => {}
                TransferState::Skipped => summary.skipped += 1,
                TransferState::Done => {
                    summary.arrived += 1;
                    summary.bytes += total.max(done);
                    summary.bytes_done += total.max(done);
                }
                _ => {
                    summary.bytes += total;
                    summary.bytes_done += done.min(total);
                }
            }
        }
        summary
    }

    /// How many files came through the stream.
    #[cfg(test)]
    pub fn streamed_files(&self) -> u64 {
        self.stream.files_done.load(Ordering::Relaxed)
    }

    /// What went wrong, once everything is over.
    fn problems(&self) -> Option<String> {
        let mut problems: Vec<String> = lock(&self.error).iter().cloned().collect();
        let files = lock(&self.files);
        let mut failed = files.iter().filter_map(|file| match file.state() {
            TransferState::Failed(error) => Some((file.name.clone(), error)),
            _ => None,
        });
        if let Some((name, error)) = failed.next() {
            problems.push(match failed.count() {
                0 => format!("{name} : {error}"),
                more => format!(
                    "{}, dont {name} : {error}",
                    format::count(more + 1, "fichier en échec", "fichiers en échec")
                ),
            });
        }
        drop(files);
        match lock(&self.unreadable).as_slice() {
            [] => {}
            [only] => problems.push(format!("dossier illisible : {only}")),
            [first, rest @ ..] => problems.push(format!("{} dossiers illisibles, dont {first}", rest.len() + 1)),
        }
        (!problems.is_empty()).then(|| problems.join(" · "))
    }
}

/// Lists a folder transfer; its task fills it.
fn add_folder(handle: &Handle, name: &str, target: String, direction: Direction, storage: &Arc<dyn Storage>) -> Arc<Transfer> {
    let resumable = match direction {
        Direction::Download => storage.can_resume_download(),
        Direction::Upload => storage.can_resume_upload(),
    };
    let transfer = Arc::new(Transfer {
        id: handle.next_id.fetch_add(1, Ordering::Relaxed),
        name: name.to_owned(),
        target,
        direction,
        quiet: false,
        progress: Progress::default(),
        folder: Some(Folder::new(resumable)),
        state: Mutex::new(TransferState::Running),
        control: None,
    });
    handle.emit(Event::TransferAdded(transfer.clone()));
    transfer
}

/// Starts one file of a folder, unlisted. Nothing more starts once the
/// folder is cancelled.
fn spawn_part(handle: &Handle, batch: &Arc<Batch>, parent: &Transfer, mut job: Job) {
    let Some(folder) = &parent.folder else {
        return;
    };
    job.small = job.source.size < SMALL_FILE;
    let (transfer, control) = new_transfer(handle, &job);
    {
        let mut files = lock(&folder.files);
        if folder.ended() {
            return;
        }
        if job.existing.is_some() {
            batch.unsettled.fetch_add(1, Ordering::Relaxed);
        }
        files.push(transfer.clone());
    }
    if folder.paused() && job.existing.is_none() {
        transfer.pause();
    }
    let ticket = job.existing.is_none().then(|| job.slots(handle).ticket(transfer.id, false));
    tokio::spawn(run(handle.clone(), batch.clone(), transfer, control, job, ticket));
    handle.ctx.request_repaint();
}

/// How a folder sent in one stream ended.
enum Streamed {
    Done(TreeReport),
    /// The connection cannot, and nothing was written.
    Unable,
    /// Paused, or a speed limit was set: the rest goes file by file.
    Stopped,
    Cancelled,
    Failed(String),
}

/// Downloads the folder `remote` as `local`: in one stream when it is not
/// here yet and the connection can, otherwise file by file, each queued as
/// soon as it is found so that the first ones arrive while the rest of the
/// tree is still gone through.
async fn download_folder(
    handle: Handle,
    searching: Searching,
    storage: Arc<dyn Storage>,
    transfer: Arc<Transfer>,
    remote: RemotePath,
    local: PathBuf,
) {
    let Some(folder) = &transfer.folder else {
        return;
    };
    let batch = searching.0.clone();
    let repaint = || handle.ctx.request_repaint();
    // Nothing of it here yet: no file can be in the way, and whatever turns
    // up there from now on came with this transfer.
    let fresh = !tokio::fs::try_exists(&local).await.unwrap_or(true);
    // Many small files then cost no more than one big one. Not under a
    // speed limit: a stream cannot be slowed without holding up everything
    // else on the connection.
    if fresh && storage.can_download_tree() && handle.shared.download.rate() == 0 {
        match stream_folder(&handle, &transfer, folder, &storage, &remote, &local).await {
            Streamed::Done(report) => return finish_tree(&handle, &transfer, folder, report),
            Streamed::Cancelled => {
                folder.set_phase(Phase::Over);
                return repaint();
            }
            Streamed::Failed(error) => {
                *lock(&folder.error) = Some(error);
                folder.set_phase(Phase::Held);
                repaint();
                let resumed = folder.resumed.load(Ordering::Relaxed);
                folder.until(|folder| folder.ended() || folder.resumed.load(Ordering::Relaxed) != resumed).await;
                *lock(&folder.error) = None;
            }
            Streamed::Stopped | Streamed::Unable => {}
        }
    }

    // Files already gone for, through an earlier search.
    let mut known = HashSet::new();
    loop {
        if folder.paused() {
            folder.set_phase(Phase::Held);
            repaint();
            folder.until(|folder| folder.ended() || !folder.paused()).await;
        }
        if folder.ended() {
            folder.set_phase(Phase::Over);
            return repaint();
        }
        folder.set_phase(Phase::Walking);
        lock(&folder.unreadable).clear();
        repaint();
        let walked = walk_download(&handle, &batch, &transfer, &storage, &remote, &local, fresh, &mut known).await;
        match walked {
            Ok(()) => break,
            Err(_) if folder.ended() => break,
            Err(error) => {
                *lock(&folder.error) = Some(describe(&error));
                folder.set_phase(Phase::Held);
                repaint();
                // "Réessayer" goes through the tree again, for what is not
                // on its way yet.
                let resumed = folder.resumed.load(Ordering::Relaxed);
                folder.until(|folder| folder.ended() || folder.resumed.load(Ordering::Relaxed) != resumed).await;
                *lock(&folder.error) = None;
            }
        }
    }
    folder.set_phase(Phase::Over);
    repaint();
}

/// The folder in one stream, once its turn comes.
async fn stream_folder(
    handle: &Handle,
    transfer: &Arc<Transfer>,
    folder: &Folder,
    storage: &Arc<dyn Storage>,
    remote: &RemotePath,
    local: &Path,
) -> Streamed {
    folder.set_phase(Phase::Waiting);
    handle.ctx.request_repaint();
    let ticket = handle.shared.slots.ticket(transfer.id, false);
    let slot = tokio::select! {
        slot = ticket.wait() => slot,
        _ = folder.until(|folder| folder.ended() || folder.paused()) => {
            return if folder.ended() { Streamed::Cancelled } else { Streamed::Stopped };
        }
    };
    if folder.ended() {
        return Streamed::Cancelled;
    }
    if folder.paused() {
        return Streamed::Stopped;
    }
    folder.set_phase(Phase::Streaming);
    handle.ctx.request_repaint();
    let watching = tokio::spawn(watch_stream(handle.shared.download.clone(), transfer.clone()));
    let result = storage.download_tree(remote, local, &folder.stream).await;
    watching.abort();
    drop(slot);

    if folder.ended() {
        return Streamed::Cancelled;
    }
    let stopped = folder.stream_stopped.load(Ordering::Relaxed);
    if stopped || result.is_err() {
        // What came is kept and counted as such; the rest is found again.
        let stream = &folder.stream;
        stream.files.store(stream.files_done.load(Ordering::Relaxed), Ordering::Relaxed);
        stream.bytes.set_total(stream.bytes.snapshot().0);
        stream.bytes.unpause();
    }
    match result {
        _ if stopped => Streamed::Stopped,
        Ok(Some(report)) => Streamed::Done(report),
        Ok(None) => Streamed::Unable,
        Err(error) => Streamed::Failed(describe(&error)),
    }
}

/// Stops a stream once a download speed limit is set.
async fn watch_stream(limit: Arc<Limiter>, transfer: Arc<Transfer>) {
    let Some(folder) = &transfer.folder else {
        return;
    };
    while folder.phase() == Phase::Streaming {
        if limit.rate() > 0 {
            folder.stop_stream();
            return;
        }
        tokio::time::sleep(STREAM_WATCH).await;
    }
}

/// Settles a folder that came in one stream, with what it left aside.
fn finish_tree(handle: &Handle, transfer: &Transfer, folder: &Folder, report: TreeReport) {
    lock(&folder.unreadable).extend(report.unreadable);
    if let Some(first) = report.unwritable.first() {
        let why = match report.unwritable.len() {
            1 => format!("impossible d'écrire {first}"),
            count => format!("{count} fichiers impossibles à écrire, dont {first}"),
        };
        *lock(&folder.error) = Some(why);
    }
    if report.links > 0 {
        handle.notice(
            format!(
                "{} : {} non copiés",
                transfer.name,
                format::count(report.links, "lien symbolique", "liens symboliques")
            ),
            false,
        );
    }
    folder.set_phase(Phase::Over);
    handle.ctx.request_repaint();
}

/// Goes through the folder `remote`, queueing each file found that is not
/// in `known` yet. In a `fresh` folder, a file already here with the right
/// size came with this transfer, through the stream.
#[allow(clippy::too_many_arguments)]
async fn walk_download(
    handle: &Handle,
    batch: &Arc<Batch>,
    transfer: &Arc<Transfer>,
    storage: &Arc<dyn Storage>,
    remote: &RemotePath,
    local: &Path,
    fresh: bool,
    known: &mut HashSet<String>,
) -> Result<()> {
    let Some(folder) = &transfer.folder else {
        return Ok(());
    };
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let walking = async {
        let walked = storage
            .walk(remote, &|found| sender.send(found).is_ok() && !folder.ended())
            .await;
        drop(sender);
        walked
    };
    let planning = async {
        let mut dates = Dates::default();
        let mut subfolders = Vec::new();
        let mut unreadable = HashSet::new();
        while let Some(found) = receiver.recv().await {
            match found {
                Found::File { relative, size } => {
                    // Paused: what is found waits.
                    folder.until(|folder| folder.ended() || !folder.paused()).await;
                    if folder.ended() {
                        break;
                    }
                    if known.insert(relative.clone()) {
                        let file = remote.child(&relative);
                        plan_download(handle, batch, transfer, storage, &mut dates, file, local, &relative, size, fresh)
                            .await;
                    }
                }
                Found::Folder { relative } => subfolders.push(relative),
                Found::Unreadable { relative, error } => {
                    lock(&folder.unreadable).push(format!("{relative} : {}", tidy(&error)));
                    unreadable.insert(relative);
                }
            }
        }
        (subfolders, unreadable)
    };
    let (walked, (subfolders, unreadable)) = tokio::join!(walking, planning);
    walked?;
    if folder.ended() {
        return Ok(());
    }
    // Files bring their folders along; empty ones have to be made, but not
    // those that could not be read.
    let mut folders = vec![local.to_path_buf()];
    for relative in subfolders.into_iter().filter(|relative| !unreadable.contains(relative)) {
        let mut dir = local.to_path_buf();
        dir.extend(safe_components(&relative));
        folders.push(dir);
    }
    let _ = tokio::task::spawn_blocking(move || {
        for dir in folders {
            let _ = std::fs::create_dir_all(dir);
        }
    })
    .await;
    Ok(())
}

/// Queues one file found in a folder being downloaded.
#[allow(clippy::too_many_arguments)]
async fn plan_download(
    handle: &Handle,
    batch: &Arc<Batch>,
    transfer: &Arc<Transfer>,
    storage: &Arc<dyn Storage>,
    dates: &mut Dates,
    remote: RemotePath,
    local: &Path,
    relative: &str,
    size: u64,
    fresh: bool,
) {
    let mut dest = local.to_path_buf();
    dest.extend(safe_components(relative));
    if dest.is_dir() {
        handle.notice(
            format!("« {} » n'est pas téléchargé : un dossier porte ce nom.", dest.display()),
            true,
        );
        return;
    }
    let mut existing = local_version(&dest);
    if fresh && let Some(there) = existing {
        if there.size == size {
            return;
        }
        // Cut short by this transfer itself: nobody else's.
        existing = None;
    }
    let mut source = Version { size, modified: None };
    if existing.is_some() {
        source.modified = dates.of(storage, &remote).await;
    }
    let partial = local_version(&partial_path(&dest)).map_or(0, |part| part.size);
    let resume = existing.is_none() && storage.can_resume_download() && partial > 0 && partial < size;
    let job = Job {
        storage: storage.clone(),
        direction: Direction::Download,
        remote,
        local: dest,
        source,
        existing,
        resume,
        keep_date: false,
        small: false,
    };
    spawn_part(handle, batch, transfer, job);
}

/// Uploads the local folder `source` as `remote`, its files queued as they
/// are planned.
async fn upload_folder(
    handle: Handle,
    searching: Searching,
    storage: Arc<dyn Storage>,
    transfer: Arc<Transfer>,
    source: PathBuf,
    remote: RemotePath,
) {
    let Some(folder) = &transfer.folder else {
        return;
    };
    let batch = searching.0.clone();
    let scan = tokio::task::spawn_blocking(move || scan_folder(&source)).await.unwrap_or_default();
    let base = remote.parent().unwrap_or_default();
    let mut listings = Listings::new(storage.clone(), base);
    for empty in &scan.empty {
        let path = remote.child(empty);
        if listings.at(&path).await.is_none() {
            create_folders(&*storage, &path).await;
        }
    }
    if !scan.empty.is_empty() {
        handle.changed(&storage, remote.parent().unwrap_or_default());
    }
    for (local, relative, version) in scan.files {
        folder.until(|folder| folder.ended() || !folder.paused()).await;
        if folder.ended() {
            break;
        }
        let path = remote.child(&relative);
        let there = listings.at(&path).await;
        if there.is_some_and(Entry::is_dir) {
            handle.notice(format!("« {path} » n'est pas envoyé : un dossier porte ce nom."), true);
            continue;
        }
        let existing = there.map(|entry| Version {
            size: entry.size.unwrap_or(0),
            modified: entry.modified,
        });
        let job = Job {
            storage: storage.clone(),
            direction: Direction::Upload,
            remote: path,
            local,
            source: version,
            existing,
            resume: false,
            keep_date: false,
            small: false,
        };
        spawn_part(&handle, &batch, &transfer, job);
    }
    folder.set_phase(Phase::Over);
    handle.ctx.request_repaint();
}

/// Creates `path` and the folders leading to it, which the files sent
/// alongside may not have made yet. Those that exist already fail, which is
/// fine; S3 needs no parents.
async fn create_folders(storage: &dyn Storage, path: &RemotePath) {
    if storage.has_buckets() {
        let _ = storage.create_folder(path).await;
        return;
    }
    for depth in 1..=path.segments().len() {
        let _ = storage.create_folder(&path.ancestor(depth)).await;
    }
}

/// What became of a wait.
enum Waited<T> {
    Done(T),
    Stopped(Request),
}

/// Waits for `future`, unless the user pauses, cancels or dismisses the
/// transfer first. A request to resume while waiting changes nothing.
async fn unless_stopped<T>(control: &Control, future: impl Future<Output = T>) -> Waited<T> {
    tokio::pin!(future);
    loop {
        tokio::select! {
            value = &mut future => return Waited::Done(value),
            request = control.next() => {
                if request != Request::Resume {
                    return Waited::Stopped(request);
                }
            }
        }
    }
}

/// Waits for a paused or failed transfer to be resumed. Returns false when
/// it is cancelled or dismissed instead.
async fn until_resumed(control: &Control) -> Option<Request> {
    loop {
        match control.next().await {
            Request::Resume => return None,
            Request::Pause => {}
            other => return Some(other),
        }
    }
}

async fn run(
    handle: Handle,
    batch: Arc<Batch>,
    transfer: Arc<Transfer>,
    control: Arc<Control>,
    mut job: Job,
    mut ticket: Option<Ticket>,
) {
    let repaint = || handle.ctx.request_repaint();
    let mut mode = if job.resume { Mode::Continue } else { Mode::Fresh };

    if let Some(existing) = job.existing {
        let Some(resolution) = settle(&handle, &batch, &transfer, &control, &job, existing).await else {
            transfer.set_state(TransferState::Cancelled);
            return repaint();
        };
        match resolution {
            Resolution::Skip => {
                control.set_note("Déjà présent : laissé tel quel");
                transfer.set_state(TransferState::Skipped);
                return repaint();
            }
            Resolution::Overwrite => control.set_note(format!("Remplace le fichier présent · {}", job.target())),
            Resolution::Resume => {
                mode = Mode::ContinueInPlace;
                control.set_note(format!("Complète le fichier présent · {}", job.target()));
            }
            Resolution::Rename => {
                rename(&batch, &mut job).await;
                control.set_note(format!("Renommé en « {} » · {}", job.name_at_destination(), job.target()));
            }
        }
    }

    // Whether an attempt began: until then, nothing was touched.
    let mut attempted = false;
    loop {
        transfer.set_state(TransferState::Queued);
        repaint();
        let ticket = ticket.take().unwrap_or_else(|| job.slots(&handle).ticket(transfer.id, false));
        let slot = match unless_stopped(&control, ticket.wait()).await {
            Waited::Done(slot) => slot,
            Waited::Stopped(Request::Pause) => {
                transfer.set_state(TransferState::Paused);
                repaint();
                if let Some(request) = until_resumed(&control).await {
                    return stopped(&handle, &transfer, &job, mode, request, attempted).await;
                }
                transfer.progress.unpause();
                continue;
            }
            Waited::Stopped(request) => return stopped(&handle, &transfer, &job, mode, request, attempted).await,
        };

        transfer.set_state(TransferState::Running);
        repaint();
        // Paused or cancelled right as its turn came: no need to start.
        let result = if transfer.progress.is_cancelled() {
            Err(anyhow::anyhow!("annulé"))
        } else {
            attempted = true;
            attempt(&job, mode, &transfer.progress).await
        };
        drop(slot);

        match result {
            Ok(()) => {
                finish(&handle, &job).await;
                transfer.set_state(TransferState::Done);
                return repaint();
            }
            Err(_) if transfer.progress.is_paused() => {
                // The request that paused it has been carried out.
                let _ = control.take();
                transfer.set_state(TransferState::Paused);
                repaint();
                if let Some(request) = until_resumed(&control).await {
                    return stopped(&handle, &transfer, &job, mode, request, attempted).await;
                }
                transfer.progress.unpause();
            }
            Err(_) if transfer.progress.is_cancelled() => {
                return stopped(&handle, &transfer, &job, mode, Request::Cancel, attempted).await;
            }
            Err(error) => {
                transfer.set_state(TransferState::Failed(describe(&error)));
                repaint();
                // "Réessayer" goes on from what arrived, where possible.
                if let Some(request) = until_resumed(&control).await {
                    return stopped(&handle, &transfer, &job, mode, request, attempted).await;
                }
            }
        }
        if attempted {
            mode = next_mode(&job, mode);
        }
    }
}

/// How the next attempt starts, after one that was paused or failed.
fn next_mode(job: &Job, mode: Mode) -> Mode {
    match mode {
        _ if !job.can_resume() => Mode::Fresh,
        Mode::ContinueInPlace => Mode::ContinueInPlace,
        Mode::Fresh | Mode::Continue => Mode::Continue,
    }
}

/// Ends a transfer that was cancelled or dismissed, removing what a
/// cancelled one had made.
async fn stopped(handle: &Handle, transfer: &Transfer, job: &Job, mode: Mode, request: Request, attempted: bool) {
    // Out of the list after a failure: what arrived stays, to be completed
    // by the next transfer of the same file.
    if request == Request::Dismiss && transfer.state().is_finished() {
        return;
    }
    transfer.progress.cancel();
    transfer.set_state(TransferState::Cancelled);
    handle.ctx.request_repaint();
    // A file that was there before is left as it is: completing it was the
    // point. Otherwise half a file is worse than none. Without resuming,
    // the connection cleaned up after itself.
    if mode == Mode::ContinueInPlace || !job.can_resume() {
        return;
    }
    match job.direction {
        Direction::Download => {
            let _ = tokio::fs::remove_file(partial_path(&job.local)).await;
        }
        Direction::Upload if attempted => {
            let _ = job.storage.abandon_upload(&job.remote).await;
            if let Some(folder) = job.remote.parent() {
                handle.changed(&job.storage, folder);
            }
        }
        Direction::Upload => {}
    }
}

/// Decides what to do with what is in the way, asking the user when the
/// rule says so. `None` when the transfer was cancelled meanwhile.
async fn settle(
    handle: &Handle,
    batch: &Batch,
    transfer: &Arc<Transfer>,
    control: &Control,
    job: &Job,
    existing: Version,
) -> Option<Resolution> {
    let can_resume = job.can_resume();
    let precision = job.storage.date_precision();
    let decide = |policy| conflict::resolve(policy, job.source, existing, can_resume, precision);
    if let Some(resolution) = decide(batch.policy()) {
        batch.settled();
        return Some(resolution);
    }

    let _turn = match unless_stopped(control, batch.asking.lock()).await {
        Waited::Done(turn) => turn,
        Waited::Stopped(_) => {
            batch.settled();
            return None;
        }
    };
    // Answered for all while this one waited its turn.
    if let Some(resolution) = decide(batch.policy()) {
        batch.settled();
        return Some(resolution);
    }

    let (sender, receiver) = oneshot::channel();
    let question = Question {
        transfer: transfer.clone(),
        batch: batch.id,
        name: job.name_at_destination(),
        folder: job.folder(),
        direction: job.direction,
        source: job.source,
        existing,
        can_resume: conflict::can_offer_resume(job.source, existing, can_resume),
        renamed: free_name(batch, job).await,
        others: batch.unsettled.load(Ordering::Relaxed).saturating_sub(1),
        more: batch.searching.load(Ordering::Relaxed) > 0,
        answer: Some(sender),
    };
    control.set_asking(true);
    handle.emit(Event::Transfers(TransferEvent::Ask(question)));
    let answer = unless_stopped(control, receiver).await;
    control.set_asking(false);
    batch.settled();

    let answer = match answer {
        Waited::Done(Ok(answer)) => answer,
        // Closed without an answer: left as it is.
        Waited::Done(Err(_)) => Answer {
            policy: ConflictPolicy::Skip,
            all: false,
        },
        Waited::Stopped(_) => return None,
    };
    if answer.all {
        *lock(&batch.policy) = answer.policy;
    }
    decide(answer.policy).or(Some(Resolution::Skip))
}

impl Job {
    /// The name the file has, or will have, where it goes.
    fn name_at_destination(&self) -> String {
        match self.direction {
            Direction::Download => self
                .local
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            Direction::Upload => self.remote.name().to_owned(),
        }
    }

    fn folder(&self) -> String {
        match self.direction {
            Direction::Download => self.local.parent().map(|dir| dir.display().to_string()).unwrap_or_default(),
            Direction::Upload => self.remote.parent().map(|dir| dir.to_string()).unwrap_or_default(),
        }
    }
}

/// The first `name (n).ext` free at the destination, among what is there
/// and what this batch gave out already.
async fn free_name(batch: &Batch, job: &Job) -> String {
    let name = job.name_at_destination();
    match job.direction {
        Direction::Download => {
            let folder = job.local.parent().map(Path::to_path_buf).unwrap_or_default();
            let reserved = lock(&batch.reserved).clone();
            conflict::free_name(&name, |candidate| {
                let path = folder.join(candidate);
                reserved.contains(&path.display().to_string()) || path.exists()
            })
        }
        Direction::Upload => {
            let folder = job.remote.parent().unwrap_or_default();
            let there: HashSet<String> = list_all(&job.storage, &folder)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|entry| entry.name)
                .collect();
            let reserved = lock(&batch.reserved).clone();
            conflict::free_name(&name, |candidate| {
                there.contains(candidate) || reserved.contains(&folder.child(candidate).to_string())
            })
        }
    }
}

/// Points the job at a free name, and keeps that name for it.
async fn rename(batch: &Batch, job: &mut Job) {
    let name = free_name(batch, job).await;
    match job.direction {
        Direction::Download => {
            job.local.set_file_name(&name);
            lock(&batch.reserved).insert(job.local.display().to_string());
        }
        Direction::Upload => {
            if let Some(folder) = job.remote.parent() {
                job.remote = folder.child(&name);
            }
            lock(&batch.reserved).insert(job.remote.to_string());
        }
    }
}

/// One go at the transfer.
async fn attempt(job: &Job, mode: Mode, progress: &Progress) -> Result<()> {
    let storage = &job.storage;
    match job.direction {
        Direction::Download => {
            if mode == Mode::ContinueInPlace {
                return storage.download_resumable(&job.remote, &job.local, true, progress).await;
            }
            if !storage.can_resume_download() {
                return storage.download(&job.remote, &job.local, progress).await;
            }
            let partial = partial_path(&job.local);
            let mut result = storage
                .download_resumable(&job.remote, &partial, mode == Mode::Continue, progress)
                .await;
            // A server that refuses to start in the middle gets a fresh start.
            let start = crate::storage::local_len(&partial).await;
            if mode == Mode::Continue && result.is_err() && !progress.is_cancelled() && progress.snapshot().0 <= start {
                result = storage.download_resumable(&job.remote, &partial, false, progress).await;
            }
            result?;
            tokio::fs::rename(&partial, &job.local)
                .await
                .map_err(|error| anyhow::anyhow!("impossible d'écrire {} : {error}", job.local.display()))
        }
        Direction::Upload => {
            if storage.can_resume_upload() {
                storage
                    .upload_resumable(&job.local, &job.remote, mode != Mode::Fresh, progress)
                    .await
            } else {
                progress.set_done(0);
                storage.upload(&job.local, &job.remote, progress).await
            }
        }
    }
}

/// What follows a transfer that went well.
async fn finish(handle: &Handle, job: &Job) {
    match job.direction {
        Direction::Download => {
            if job.keep_date
                && let Some(modified) = job.source.modified
            {
                set_local_mtime(&job.local, modified);
            }
        }
        Direction::Upload => {
            if job.keep_date
                && let Some(modified) = job.source.modified
            {
                // Not every server allows it; the comparison copes.
                let _ = job.storage.set_mtime(&job.remote, modified).await;
            }
            if let Some(folder) = job.remote.parent() {
                handle.changed(&job.storage, folder);
            }
        }
    }
}

fn set_local_mtime(path: &Path, modified: i64) {
    let Ok(seconds) = u64::try_from(modified) else {
        return;
    };
    if let Ok(file) = std::fs::File::options().write(true).open(path) {
        let _ = file.set_modified(UNIX_EPOCH + Duration::from_secs(seconds));
    }
}

/// Compares a local folder with a remote one, for the preview of a
/// synchronisation.
pub(super) async fn sync_compare(
    handle: Handle,
    id: u64,
    storage: Arc<dyn Storage>,
    remote: RemotePath,
    local: PathBuf,
    rules: Rules,
) {
    let result = async {
        let remote_files = storage.list_deep_dated(&remote).await?;
        let remote_files: BTreeMap<String, FileInfo> = remote_files
            .into_iter()
            .map(|file| {
                let info = FileInfo {
                    size: file.size,
                    modified: file.modified,
                };
                (file.relative, info)
            })
            .collect();
        let root = local.clone();
        let local_files = tokio::task::spawn_blocking(move || {
            let mut files = Vec::new();
            walk_local(&root, "", &mut files);
            files
        })
        .await?;
        let mut local_map = BTreeMap::new();
        for (_, relative, version) in local_files {
            // `walk_local` starts every path with the prefix and a slash.
            let relative = relative.trim_start_matches('/').to_owned();
            // What an interrupted download left is not a file of the user.
            if let Some(original) = relative.strip_suffix(".part")
                && remote_files.contains_key(original)
            {
                continue;
            }
            local_map.insert(
                relative,
                FileInfo {
                    size: version.size,
                    modified: version.modified,
                },
            );
        }
        let rules = Rules {
            precision: rules.precision.max(storage.date_precision()),
            ..rules
        };
        anyhow::Ok(crate::transfers::sync::compare(&local_map, &remote_files, rules))
    }
    .await
    .map_err(|error| describe(&error));
    handle.emit(Event::Transfers(TransferEvent::Compared { id, result }));
}

/// Does what the ticked lines of a synchronisation say.
pub(super) async fn sync_run(
    handle: Handle,
    storage: Arc<dyn Storage>,
    remote: RemotePath,
    local: PathBuf,
    lines: Vec<SyncLine>,
) {
    let local_path = |relative: &str| {
        let mut path = local.clone();
        path.extend(safe_components(relative));
        path
    };
    // Decided line by line in the preview: nothing more to ask.
    let batch = Batch::new(&handle, ConflictPolicy::Overwrite);
    let mut jobs = Vec::new();
    let mut deletions = Vec::new();
    for line in lines.into_iter().filter(|line| line.selected) {
        match line.action {
            SyncAction::Upload => {
                let Some(file) = line.local else {
                    continue;
                };
                jobs.push(Job {
                    storage: storage.clone(),
                    direction: Direction::Upload,
                    remote: remote.child(&line.relative),
                    local: local_path(&line.relative),
                    source: Version {
                        size: file.size,
                        modified: file.modified,
                    },
                    existing: None,
                    resume: false,
                    keep_date: true,
                    small: false,
                });
            }
            SyncAction::Download => {
                let Some(file) = line.remote else {
                    continue;
                };
                jobs.push(Job {
                    storage: storage.clone(),
                    direction: Direction::Download,
                    remote: remote.child(&line.relative),
                    local: local_path(&line.relative),
                    source: Version {
                        size: file.size,
                        modified: file.modified,
                    },
                    existing: None,
                    resume: false,
                    keep_date: true,
                    small: false,
                });
            }
            _ if line.action.is_delete() => deletions.push(line),
            _ => {}
        }
    }
    let transfers = jobs.len();
    start(&handle, &batch, jobs);
    if deletions.is_empty() {
        if transfers == 0 {
            handle.notice("Rien à synchroniser", false);
        }
        return;
    }

    let results: Vec<(SyncAction, String, Result<()>)> = stream::iter(deletions)
        .map(|line| {
            let storage = storage.clone();
            let path = local_path(&line.relative);
            let target = remote.child(&line.relative);
            async move {
                let result = match line.action {
                    SyncAction::DeleteRemote => storage.delete(&target, false).await.map(|_| ()),
                    _ => tokio::fs::remove_file(&path).await.map_err(anyhow::Error::from),
                };
                (line.action, line.relative, result)
            }
        })
        .buffer_unordered(PARALLEL_DELETES)
        .collect()
        .await;

    let mut touched = BTreeSet::new();
    let mut failures = Vec::new();
    for (action, relative, result) in &results {
        match result {
            Ok(()) if *action == SyncAction::DeleteRemote => {
                if let Some(folder) = remote.child(relative).parent() {
                    touched.insert(folder.to_string());
                }
            }
            Ok(()) => {}
            Err(error) => failures.push(format!("{relative} : {}", describe(error))),
        }
    }
    for folder in touched {
        handle.changed(&storage, RemotePath::parse(&folder));
    }
    let removed = results.len() - failures.len();
    let text = crate::format::count(removed, "fichier supprimé", "fichiers supprimés");
    match failures.first() {
        None => handle.notice(format!("Synchronisation : {text}"), false),
        Some(first) => handle.notice(format!("Synchronisation : {text}, {} échec(s) — {first}", failures.len()), true),
    }
}

#[cfg(test)]
mod tests;
