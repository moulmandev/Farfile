//! Everything that touches the network runs here, on a Tokio runtime, and
//! reports back to the UI thread through [`Event`]s. The UI never blocks.

pub mod transfer;

use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use futures_util::{StreamExt, stream};
use tokio::runtime::Runtime;

use crate::edits::{Watch, stamp};
use crate::storage::{Entry, Progress, RemotePath, Storage, storage_key};
use crate::transfers::control::Control;
use crate::transfers::slots::Slots;
use crate::transfers::{SMALL_FILE, Shared};
use transfer::Folder;

const PARALLEL_DELETES: usize = 8;
const SHARE_LINK_LIFETIME: Duration = Duration::from_secs(3600);
/// How often the local copy of a file being edited is looked at. A change
/// is reported once two looks in a row agree, i.e. the editor is done.
const WATCH_INTERVAL: Duration = Duration::from_millis(600);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Download,
    Upload,
}

#[derive(Clone, PartialEq, Eq)]
pub enum TransferState {
    Queued,
    Running,
    /// Stopped by the user, to go on later.
    Paused,
    Done,
    Failed(String),
    Cancelled,
    /// The destination existed and was left as it was.
    Skipped,
}

impl TransferState {
    pub fn is_finished(&self) -> bool {
        !matches!(self, Self::Queued | Self::Running | Self::Paused)
    }
}

pub struct Transfer {
    pub id: u64,
    pub name: String,
    /// Where the data is going, for display.
    pub target: String,
    pub direction: Direction,
    /// Part of something else the user asked for, such as opening a file:
    /// listed, but not worth bringing the transfers into view.
    pub quiet: bool,
    pub progress: Progress,
    /// Set for a whole folder, whose files are transfers of their own,
    /// not listed: the folder stands for them.
    pub folder: Option<Folder>,
    state: Mutex<TransferState>,
    /// Pause, resume and retry, for the transfers that allow them.
    control: Option<Arc<Control>>,
}

impl Transfer {
    /// A folder's is that of its files.
    pub fn state(&self) -> TransferState {
        match &self.folder {
            Some(folder) => folder.state(),
            None => self.state.lock().unwrap().clone(),
        }
    }

    fn set_state(&self, state: TransferState) {
        *self.state.lock().unwrap() = state;
    }
}

pub enum Event {
    /// A page of the listing started with the given generation.
    Listed {
        generation: u64,
        entries: Vec<Entry>,
        done: bool,
    },
    ListFailed {
        generation: u64,
        error: String,
    },
    /// Where a connection without start folder should land, for the listing
    /// request of the given generation.
    Home {
        generation: u64,
        path: Result<RemotePath, String>,
    },
    TransferAdded(Arc<Transfer>),
    /// The remote content of this folder changed; listings are stale.
    /// `storage` is the [`storage_key`] of the connection.
    Changed {
        storage: usize,
        path: RemotePath,
    },
    Notice {
        text: String,
        error: bool,
    },
    /// Put `text` in the clipboard and show `notice`.
    Clipboard {
        text: String,
        notice: String,
    },
    /// The local copy of a file opened for editing is there, or is not.
    EditFetched {
        id: u64,
        error: Option<String>,
    },
    /// The local copy was saved by the program editing it.
    EditSaved {
        id: u64,
    },
    /// The local copy was sent back to the server, or could not be.
    EditUploaded {
        id: u64,
        error: Option<String>,
    },
    /// An answer of an S3-only operation (see `crate::s3`).
    S3(crate::s3::S3Event),
    Transfers(transfer::TransferEvent),
}

/// Lets the tasks of other modules report back like the worker's own.
#[derive(Clone)]
pub struct Emitter(Handle);

impl Emitter {
    pub fn emit(&self, event: Event) {
        self.0.emit(event);
    }
}

/// A remote item picked in the UI.
pub struct Item {
    pub path: RemotePath,
    pub is_dir: bool,
    pub size: u64,
}

/// What a background task needs to report back and to queue transfers.
#[derive(Clone)]
struct Handle {
    events: Sender<Event>,
    ctx: egui::Context,
    /// Turns, speed limits and rules shared by every transfer.
    shared: Arc<Shared>,
    next_id: Arc<AtomicU64>,
}

impl Handle {
    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
        self.ctx.request_repaint();
    }

    fn notice(&self, text: impl Into<String>, error: bool) {
        self.emit(Event::Notice {
            text: text.into(),
            error,
        });
    }

    /// Queues a transfer. `job` runs once a slot is free; `done` is then told
    /// how it went, also when the transfer was cancelled before it started.
    #[allow(clippy::too_many_arguments)]
    fn queue<F, Fut, D>(
        &self,
        name: String,
        target: String,
        direction: Direction,
        size: u64,
        quiet: bool,
        job: F,
        done: D,
    ) -> Arc<Transfer>
    where
        F: FnOnce(Arc<Transfer>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<()>> + Send,
        D: FnOnce(&Handle, Result<(), String>) + Send + 'static,
    {
        let slots = self.shared.slots.clone();
        self.queue_on(&slots, name, target, direction, size, quiet, job, done)
    }

    /// Like [`Handle::queue`], waiting for a turn among `slots`.
    #[allow(clippy::too_many_arguments)]
    fn queue_on<F, Fut, D>(
        &self,
        slots: &Arc<Slots>,
        name: String,
        target: String,
        direction: Direction,
        size: u64,
        quiet: bool,
        job: F,
        done: D,
    ) -> Arc<Transfer>
    where
        F: FnOnce(Arc<Transfer>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<()>> + Send,
        D: FnOnce(&Handle, Result<(), String>) + Send + 'static,
    {
        let transfer = Arc::new(Transfer {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            name,
            target,
            direction,
            quiet,
            progress: Progress::default(),
            folder: None,
            state: Mutex::new(TransferState::Queued),
            control: None,
        });
        transfer.progress.set_total(size);
        transfer.progress.set_limiter(transfer::limiter(self, direction));
        self.emit(Event::TransferAdded(transfer.clone()));

        // What the user waits for, such as a file being opened, goes first.
        let ticket = slots.ticket(transfer.id, quiet);
        let handle = self.clone();
        let queued = transfer.clone();
        tokio::spawn(async move {
            let _slot = ticket.wait().await;
            if transfer.progress.is_cancelled() {
                transfer.set_state(TransferState::Cancelled);
                done(&handle, Err(tr!("annulé").to_owned()));
                handle.ctx.request_repaint();
                return;
            }

            transfer.set_state(TransferState::Running);
            handle.ctx.request_repaint();
            let result = job(transfer.clone()).await.map_err(|error| describe(&error));
            transfer.set_state(match &result {
                Ok(()) => TransferState::Done,
                Err(_) if transfer.progress.is_cancelled() => TransferState::Cancelled,
                Err(error) => TransferState::Failed(error.clone()),
            });
            done(&handle, result);
            handle.ctx.request_repaint();
        });
        queued
    }

    /// Tells the tabs showing `path` of this connection to list it again.
    fn changed(&self, storage: &Arc<dyn Storage>, path: RemotePath) {
        self.emit(Event::Changed {
            storage: storage_key(storage),
            path,
        });
    }
}

pub struct Worker {
    runtime: Runtime,
    handle: Handle,
}

impl Worker {
    pub fn new(ctx: egui::Context) -> (Self, Receiver<Event>) {
        let (events, receiver) = channel();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("worker")
            .build()
            .expect("failed to start the background runtime");
        let handle = Handle {
            events,
            ctx,
            shared: Arc::new(Shared::default()),
            next_id: Arc::new(AtomicU64::new(1)),
        };
        (Self { runtime, handle }, receiver)
    }

    /// Runs `build` inside the runtime, for constructors that expect one.
    pub fn within<T>(&self, build: impl FnOnce() -> T) -> T {
        let _guard = self.runtime.enter();
        build()
    }

    /// Runs a background task of another module on the runtime.
    pub fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        self.runtime.spawn(task);
    }

    pub fn ctx(&self) -> egui::Context {
        self.handle.ctx.clone()
    }

    pub fn emitter(&self) -> Emitter {
        Emitter(self.handle.clone())
    }

    /// Queues a download that `job` performs itself, listed with the other
    /// transfers; `done` is told how it went.
    pub fn queue_download<F, Fut, D>(&self, name: String, target: String, size: u64, job: F, done: D)
    where
        F: FnOnce(Arc<Transfer>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<()>> + Send,
        D: FnOnce(&Emitter, Result<(), String>) + Send + 'static,
    {
        let _runtime = self.runtime.enter();
        self.handle.queue(name, target, Direction::Download, size, false, job, move |handle, result| {
            done(&Emitter(handle.clone()), result)
        });
    }

    /// Turns, speed limits and rules of the transfers, to read or change.
    pub fn transfer_rules(&self) -> &Arc<Shared> {
        &self.handle.shared
    }

    /// Asks the connection where to start. Ignored, like a listing, once
    /// `current` no longer holds `generation`.
    pub fn home(&self, storage: Arc<dyn Storage>, generation: u64, current: Arc<AtomicU64>) {
        let handle = self.handle.clone();
        self.runtime.spawn(async move {
            let path = storage.home().await.map_err(|error| format!("{error:#}"));
            if current.load(Ordering::Relaxed) == generation {
                handle.emit(Event::Home { generation, path });
            }
        });
    }

    /// Streams the listing of `path` page by page. Pages are dropped as soon
    /// as `current` no longer holds `generation`, i.e. the user moved on.
    pub fn list(
        &self,
        storage: Arc<dyn Storage>,
        path: RemotePath,
        generation: u64,
        current: Arc<AtomicU64>,
    ) {
        let handle = self.handle.clone();
        self.runtime.spawn(async move {
            let mut cursor = None;
            loop {
                let page = storage.list_page(&path, cursor).await;
                if current.load(Ordering::Relaxed) != generation {
                    return;
                }
                match page {
                    Ok(page) => {
                        let done = page.next.is_none();
                        handle.emit(Event::Listed {
                            generation,
                            entries: page.entries,
                            done,
                        });
                        if done {
                            return;
                        }
                        cursor = page.next;
                    }
                    Err(error) => {
                        handle.emit(Event::ListFailed {
                            generation,
                            error: format!("{error:#}"),
                        });
                        return;
                    }
                }
            }
        });
    }

    /// Downloads files and whole folders into `dest_dir`, following the
    /// rules for files that are there already.
    pub fn download(&self, storage: Arc<dyn Storage>, items: Vec<Item>, dest_dir: PathBuf) {
        let handle = self.handle.clone();
        self.runtime.spawn(transfer::download(handle, storage, items, dest_dir));
    }

    /// Uploads local files and folders (recursively) into `dest`, following
    /// the rules for files that are there already.
    pub fn upload(&self, storage: Arc<dyn Storage>, sources: Vec<PathBuf>, dest: RemotePath) {
        let handle = self.handle.clone();
        self.runtime.spawn(transfer::upload(handle, storage, sources, dest));
    }

    /// Compares a local folder with a remote one. The differences come back
    /// as [`transfer::TransferEvent::Compared`] with this `id`.
    pub fn sync_compare(
        &self,
        id: u64,
        storage: Arc<dyn Storage>,
        remote: RemotePath,
        local: PathBuf,
        rules: crate::transfers::sync::Rules,
    ) {
        let handle = self.handle.clone();
        self.runtime
            .spawn(transfer::sync_compare(handle, id, storage, remote, local, rules));
    }

    /// Carries out the ticked lines of a synchronisation.
    pub fn sync_run(
        &self,
        storage: Arc<dyn Storage>,
        remote: RemotePath,
        local: PathBuf,
        lines: Vec<crate::transfers::sync::SyncLine>,
    ) {
        let handle = self.handle.clone();
        self.runtime.spawn(transfer::sync_run(handle, storage, remote, local, lines));
    }

    /// Deletes `items`, which all live in `parent`.
    pub fn delete(&self, storage: Arc<dyn Storage>, items: Vec<Item>, parent: RemotePath) {
        let handle = self.handle.clone();
        self.runtime.spawn(async move {
            let results: Vec<Result<u64>> = stream::iter(items)
                .map(|item| {
                    let storage = storage.clone();
                    async move { storage.delete(&item.path, item.is_dir).await }
                })
                .buffer_unordered(PARALLEL_DELETES)
                .collect()
                .await;

            let removed: u64 = results.iter().filter_map(|r| r.as_ref().ok()).sum();
            let failures: Vec<String> = results
                .iter()
                .filter_map(|r| r.as_ref().err())
                .map(|error| format!("{error:#}"))
                .collect();

            handle.changed(&storage, parent);
            let removed = trn!(removed, "{n} élément supprimé", "{n} éléments supprimés");
            match failures.first() {
                None => handle.notice(removed, false),
                Some(first) => handle.notice(
                    tr!("Suppression incomplète ({removed}) : {first}", removed, first),
                    true,
                ),
            }
        });
    }

    pub fn create_folder(&self, storage: Arc<dyn Storage>, path: RemotePath) {
        let handle = self.handle.clone();
        self.runtime.spawn(async move {
            match storage.create_folder(&path).await {
                Ok(()) => {
                    if let Some(parent) = path.parent() {
                        handle.changed(&storage, parent);
                    }
                }
                Err(error) => {
                    let error = format!("{error:#}");
                    handle.notice(tr!("Création du dossier impossible : {error}", error), true);
                }
            }
        });
    }

    /// Fetches `remote` into the local file `local`, to be opened there.
    /// While the copy is being written `watch` reports nothing.
    pub fn fetch_edit(
        &self,
        id: u64,
        storage: Arc<dyn Storage>,
        remote: RemotePath,
        local: PathBuf,
        size: u64,
        watch: Arc<Watch>,
    ) {
        let _runtime = self.runtime.enter();
        let (target, written) = (local.display().to_string(), local.clone());
        let fetching = watch.clone();
        self.handle.queue(
            remote.name().to_owned(),
            target,
            Direction::Download,
            size,
            true,
            move |transfer| async move {
                fetching.set_seen(None);
                storage.download(&remote, &local, &transfer.progress).await
            },
            move |handle, result| {
                // Whatever is there now is what the editor starts from.
                watch.set_seen(stamp(&written));
                handle.emit(Event::EditFetched {
                    id,
                    error: result.err(),
                });
            },
        );
    }

    /// Sends the edited copy `local` back as `remote`.
    pub fn upload_edit(&self, id: u64, storage: Arc<dyn Storage>, local: PathBuf, remote: RemotePath) {
        let _runtime = self.runtime.enter();
        let size = std::fs::metadata(&local).map(|meta| meta.len()).unwrap_or(0);
        let (sender, destination) = (storage.clone(), remote.clone());
        self.handle.queue(
            remote.name().to_owned(),
            remote.to_string(),
            Direction::Upload,
            size,
            true,
            move |transfer| async move { sender.upload(&local, &destination, &transfer.progress).await },
            move |handle, result| {
                if result.is_ok()
                    && let Some(folder) = remote.parent()
                {
                    handle.changed(&storage, folder);
                }
                handle.emit(Event::EditUploaded {
                    id,
                    error: result.err(),
                });
            },
        );
    }

    /// Watches the local copy of a file being edited and reports each save,
    /// until `watch` is stopped.
    pub fn watch_edit(&self, id: u64, local: PathBuf, watch: Arc<Watch>) {
        let handle = self.handle.clone();
        self.runtime.spawn(async move {
            // A version seen once: reported when it is still there next time.
            let mut settling = None;
            loop {
                tokio::time::sleep(WATCH_INTERVAL).await;
                if watch.stopped() {
                    return;
                }
                // Editors often save by replacing the file: it may be gone
                // for an instant.
                let (Some(seen), Some(now)) = (watch.seen(), stamp(&local)) else {
                    settling = None;
                    continue;
                };
                if now == seen {
                    settling = None;
                } else if settling == Some(now) {
                    settling = None;
                    watch.set_seen(Some(now));
                    handle.emit(Event::EditSaved { id });
                } else {
                    settling = Some(now);
                }
            }
        });
    }

    /// Creates a temporary download link and copies it to the clipboard.
    pub fn share(&self, storage: Arc<dyn Storage>, path: RemotePath) {
        let handle = self.handle.clone();
        self.runtime.spawn(async move {
            match storage.share_url(&path, SHARE_LINK_LIFETIME).await {
                Ok(text) => handle.emit(Event::Clipboard {
                    text,
                    notice: tr!("Lien temporaire copié (valable 1 h)").to_owned(),
                }),
                Err(error) => {
                    let error = format!("{error:#}");
                    handle.notice(tr!("Lien impossible : {error}", error), true)
                }
            }
        });
    }
}

/// Queues downloads and runs tasks on the worker from threads of other
/// modules: those that serve files dragged out to other programs.
#[derive(Clone)]
pub struct Fetcher {
    runtime: tokio::runtime::Handle,
    handle: Handle,
}

impl Fetcher {
    pub fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        self.runtime.spawn(task);
    }

    pub fn notice(&self, text: impl Into<String>, error: bool) {
        self.handle.notice(text, error);
    }

    /// Queues the download of `remote` into the local file `local`, listed
    /// with the other transfers as going to `target`. The program the files
    /// are dropped on reads them one after the other while the next ones
    /// come: small ones take the extra turns of the small files of folders.
    pub fn download(
        &self,
        storage: Arc<dyn Storage>,
        remote: RemotePath,
        local: PathBuf,
        size: u64,
        target: String,
    ) -> Arc<Transfer> {
        let _runtime = self.runtime.enter();
        let shared = &self.handle.shared;
        let slots = if size < SMALL_FILE { &shared.small } else { &shared.slots };
        self.handle.queue_on(
            slots,
            remote.name().to_owned(),
            target,
            Direction::Download,
            size,
            false,
            move |transfer| async move { storage.download(&remote, &local, &transfer.progress).await },
            |_, _| {},
        )
    }
}

impl Worker {
    pub fn fetcher(&self) -> Fetcher {
        Fetcher {
            runtime: self.runtime.handle().clone(),
            handle: self.handle.clone(),
        }
    }
}

/// What another module needs to queue transfers and report back from the
/// runtime, the way the methods of [`Worker`] do. See [`Worker::jobs`].
#[derive(Clone)]
pub struct Jobs(Handle);

impl Worker {
    pub fn jobs(&self) -> Jobs {
        Jobs(self.handle.clone())
    }
}

impl Jobs {
    /// Queues a transfer, listed in the panel like the others. `done` is
    /// told how it went. To be called from within the runtime.
    pub fn queue<F, Fut, D>(&self, name: String, target: String, direction: Direction, size: u64, job: F, done: D)
    where
        F: FnOnce(Arc<Transfer>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<()>> + Send,
        D: FnOnce(&Jobs, Result<(), String>) + Send + 'static,
    {
        self.0.queue(name, target, direction, size, false, job, move |handle, result| {
            done(&Jobs(handle.clone()), result);
        });
    }

    pub fn notice(&self, text: impl Into<String>, error: bool) {
        self.0.notice(text, error);
    }

    /// Tells the tabs showing `path` of this connection to list it again.
    pub fn changed(&self, storage: &Arc<dyn Storage>, path: RemotePath) {
        self.0.changed(storage, path);
    }
}

/// The error and its causes on one line, without saying the same thing
/// twice: servers often repeat the name of a status as its message.
fn describe(error: &anyhow::Error) -> String {
    tidy(&format!("{error:#}"))
}

/// An error already on one line, without its repetitions.
fn tidy(text: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in text.split(": ") {
        if parts.last() != Some(&part) {
            parts.push(part);
        }
    }
    parts.join(": ")
}

/// Remote names are arbitrary strings: keep them from escaping the chosen
/// download folder or tripping over characters Windows forbids.
pub fn safe_component(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim_end_matches([' ', '.']);
    if cleaned.is_empty() || cleaned == ".." {
        "_".to_owned()
    } else {
        cleaned.to_owned()
    }
}

fn safe_components(relative: &str) -> impl Iterator<Item = String> + '_ {
    relative
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .map(safe_component)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_names_cannot_escape_the_download_folder() {
        let parts: Vec<String> = safe_components("../../etc/./passwd").collect();
        assert_eq!(parts, ["_", "_", "etc", "passwd"]);
        assert_eq!(safe_component("a:b?.txt"), "a_b_.txt");
        assert_eq!(safe_component("trailing. "), "trailing");
        assert_eq!(safe_component("C:\\Windows"), "C__Windows");
    }

    /// What the app relies on to offer sending an edited file back: one
    /// report per save, once the file has stopped changing.
    #[test]
    fn a_save_is_reported_once_the_file_settles() {
        let file = std::env::temp_dir().join(format!("s3viewer-watch-test-{}.txt", std::process::id()));
        std::fs::write(&file, "a").unwrap();
        let (worker, events) = Worker::new(egui::Context::default());
        let watch = Arc::new(Watch::default());
        watch.set_seen(stamp(&file));
        worker.watch_edit(7, file.clone(), watch.clone());

        let saved = |wait: Duration| {
            let deadline = std::time::Instant::now() + wait;
            while std::time::Instant::now() < deadline {
                if let Ok(Event::EditSaved { id }) = events.try_recv() {
                    return Some(id);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            None
        };
        assert_eq!(saved(WATCH_INTERVAL * 3), None, "nothing changed yet");

        std::fs::write(&file, "a longer text").unwrap();
        assert_eq!(saved(WATCH_INTERVAL * 6), Some(7));
        assert_eq!(saved(WATCH_INTERVAL * 3), None, "one save, one report");

        // Being fetched again: whatever happens to the file is not a save.
        watch.set_seen(None);
        std::fs::write(&file, "fetched").unwrap();
        assert_eq!(saved(WATCH_INTERVAL * 4), None);

        watch.stop();
        let _ = std::fs::remove_file(file);
    }

    #[test]
    fn errors_do_not_repeat_themselves() {
        let error = anyhow::anyhow!("Permission denied: Permission denied").context("envoi");
        assert_eq!(describe(&error), "envoi: Permission denied");
    }
}
