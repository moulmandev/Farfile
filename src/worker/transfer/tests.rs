//! The rules at work, against a storage kept in memory: questions about
//! existing files, renaming, completing, pausing, cancelling, synchronising.

use std::collections::VecDeque;
use std::sync::mpsc::Receiver;
use std::time::Instant;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;

use super::*;
use crate::storage::{EntryKind, FoundSink, ListPage, append_stream, local_len, save_stream};
use crate::transfers::TransferSettings;
use crate::transfers::sync::SyncDirection;
use crate::worker::Worker;

const T: i64 = 1_700_000_000;
const CHUNK: usize = 16 * 1024;

/// Files in memory, by path. With a delay, data moves one chunk at a time,
/// so that a transfer can be caught halfway.
#[derive(Default)]
struct Memory {
    files: Mutex<BTreeMap<String, (Vec<u8>, i64)>>,
    delay: Option<Duration>,
    /// Where each resumable transfer started.
    starts: Mutex<Vec<u64>>,
}

impl Memory {
    fn slow() -> Self {
        Self {
            delay: Some(Duration::from_millis(8)),
            ..Self::default()
        }
    }

    fn put(&self, path: &str, bytes: &[u8], modified: i64) {
        lock(&self.files).insert(path.to_owned(), (bytes.to_vec(), modified));
    }

    fn get(&self, path: &str) -> Option<Vec<u8>> {
        lock(&self.files).get(path).map(|(bytes, _)| bytes.clone())
    }

    fn bytes(&self, path: &RemotePath) -> Result<Vec<u8>> {
        self.get(&path.to_string()).ok_or_else(|| anyhow::anyhow!("{path} : introuvable"))
    }

    fn starts(&self) -> Vec<u64> {
        lock(&self.starts).clone()
    }

    fn prefix(path: &RemotePath) -> String {
        if path.is_root() { "/".to_owned() } else { format!("{path}/") }
    }
}

#[async_trait]
impl Storage for Memory {
    async fn list_page(&self, path: &RemotePath, _cursor: Option<String>) -> Result<ListPage> {
        let prefix = Self::prefix(path);
        let mut entries: Vec<Entry> = Vec::new();
        for (key, (bytes, modified)) in lock(&self.files).iter() {
            let Some(rest) = key.strip_prefix(&prefix).filter(|rest| !rest.is_empty()) else {
                continue;
            };
            match rest.split_once('/') {
                Some((folder, _)) => {
                    if !entries.iter().any(|entry| entry.name == folder) {
                        entries.push(Entry {
                            permissions: None,
                            owner: None,
                            group: None,
                            link: None,
                            name: folder.to_owned(),
                            kind: EntryKind::Folder,
                            size: None,
                            modified: None,
                        });
                    }
                }
                None => entries.push(Entry {
                    permissions: None,
                    owner: None,
                    group: None,
                    link: None,
                    name: rest.to_owned(),
                    kind: EntryKind::File,
                    size: Some(bytes.len() as u64),
                    modified: Some(*modified),
                }),
            }
        }
        Ok(ListPage { entries, next: None })
    }

    async fn walk(&self, path: &RemotePath, found: FoundSink<'_>) -> Result<()> {
        let prefix = Self::prefix(path);
        let files: Vec<(String, u64)> = lock(&self.files)
            .iter()
            .filter_map(|(key, (bytes, _))| Some((key.strip_prefix(&prefix)?.to_owned(), bytes.len() as u64)))
            .collect();
        let mut folders = BTreeSet::new();
        for (relative, size) in files {
            // A key ending with `/` marks an empty folder.
            let (relative, folder) = match relative.strip_suffix('/') {
                Some(folder) => (format!("{folder}/"), true),
                None => (relative, false),
            };
            for (at, _) in relative.match_indices('/') {
                if folders.insert(relative[..at].to_owned()) && !found(Found::Folder { relative: relative[..at].to_owned() }) {
                    return Ok(());
                }
            }
            if !folder && !found(Found::File { relative, size }) {
                return Ok(());
            }
        }
        Ok(())
    }

    async fn download(&self, path: &RemotePath, dest: &Path, progress: &Progress) -> Result<()> {
        let bytes = self.bytes(path)?;
        save_stream(&mut bytes.as_slice(), dest, progress).await
    }

    async fn upload(&self, src: &Path, dest: &RemotePath, progress: &Progress) -> Result<()> {
        let bytes = tokio::fs::read(src).await?;
        progress.add(bytes.len() as u64);
        self.put(&dest.to_string(), &bytes, T + 10_000);
        Ok(())
    }

    async fn delete(&self, path: &RemotePath, _is_dir: bool) -> Result<u64> {
        Ok(lock(&self.files).remove(&path.to_string()).map_or(0, |_| 1))
    }

    async fn create_folder(&self, path: &RemotePath) -> Result<()> {
        self.put(&format!("{path}/"), b"", T);
        Ok(())
    }

    fn plain_url(&self, _path: &RemotePath) -> Option<String> {
        None
    }

    fn can_resume_download(&self) -> bool {
        true
    }

    async fn download_resumable(&self, path: &RemotePath, partial: &Path, resume: bool, progress: &Progress) -> Result<()> {
        let bytes = self.bytes(path)?;
        let offset = if resume { local_len(partial).await } else { 0 };
        lock(&self.starts).push(offset);
        progress.set_total(bytes.len() as u64);
        let (mut writer, mut reader) = tokio::io::duplex(CHUNK);
        let (rest, delay) = (bytes[offset as usize..].to_vec(), self.delay);
        let feeding = tokio::spawn(async move {
            for chunk in rest.chunks(CHUNK) {
                if let Some(delay) = delay {
                    tokio::time::sleep(delay).await;
                }
                if writer.write_all(chunk).await.is_err() {
                    return;
                }
            }
        });
        let result = append_stream(&mut reader, partial, offset, progress).await;
        feeding.abort();
        result
    }

    fn can_resume_upload(&self) -> bool {
        true
    }

    /// What arrived stays when interrupted, as on a real server.
    async fn upload_resumable(&self, src: &Path, dest: &RemotePath, resume: bool, progress: &Progress) -> Result<()> {
        let local = tokio::fs::read(src).await?;
        let key = dest.to_string();
        let mut sent = if resume { self.get(&key).unwrap_or_default() } else { Vec::new() };
        lock(&self.starts).push(sent.len() as u64);
        progress.set_total(local.len() as u64);
        progress.set_done(sent.len() as u64);
        for chunk in local[sent.len()..].chunks(CHUNK) {
            if progress.is_cancelled() {
                self.put(&key, &sent, T + 10_000);
                anyhow::bail!("annulé");
            }
            if let Some(delay) = self.delay {
                tokio::time::sleep(delay).await;
            }
            sent.extend_from_slice(chunk);
            progress.add(chunk.len() as u64);
        }
        self.put(&key, &sent, T + 10_000);
        Ok(())
    }

    async fn set_mtime(&self, path: &RemotePath, modified: i64) -> Result<()> {
        if let Some(file) = lock(&self.files).get_mut(&path.to_string()) {
            file.1 = modified;
        }
        Ok(())
    }

    fn keeps_dates(&self) -> bool {
        true
    }
}

/// Follows what the worker reports.
struct Probe {
    events: Receiver<Event>,
    transfers: Vec<Arc<Transfer>>,
    questions: VecDeque<Question>,
    compared: Vec<(u64, Result<Vec<SyncLine>, String>)>,
    notices: Vec<String>,
}

impl Probe {
    fn new(events: Receiver<Event>) -> Self {
        Self {
            events,
            transfers: Vec::new(),
            questions: VecDeque::new(),
            compared: Vec::new(),
            notices: Vec::new(),
        }
    }

    fn pump(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                Event::TransferAdded(transfer) => self.transfers.push(transfer),
                Event::Transfers(TransferEvent::Ask(question)) => self.questions.push_back(question),
                Event::Transfers(TransferEvent::Compared { id, result }) => self.compared.push((id, result)),
                Event::Notice { text, .. } => self.notices.push(text),
                _ => {}
            }
        }
    }

    /// Waits for `done` to hold, failing after a few seconds.
    fn until(&mut self, what: &str, mut done: impl FnMut(&mut Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.pump();
            if done(self) {
                return;
            }
            assert!(Instant::now() < deadline, "still waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn question(&mut self) -> Question {
        self.until("a question", |probe| !probe.questions.is_empty());
        self.questions.pop_front().expect("just seen")
    }

    fn transfer(&mut self, name: &str) -> Arc<Transfer> {
        self.until(name, |probe| probe.transfers.iter().any(|transfer| transfer.name == name));
        self.transfers
            .iter()
            .find(|transfer| transfer.name == name)
            .cloned()
            .expect("just seen")
    }

    fn all_finished(&mut self, count: usize) {
        self.until("the transfers to finish", |probe| {
            probe.transfers.len() >= count && probe.transfers.iter().all(|transfer| transfer.state().is_finished())
        });
    }
}

fn state_of(transfer: &Transfer) -> TransferState {
    transfer.state()
}

fn wait_state(transfer: &Transfer, wanted: TransferState) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while transfer.state() != wanted {
        let (now, wanted) = (debug(&transfer.state()), debug(&wanted));
        assert!(Instant::now() < deadline, "{} is {now}, not {wanted}", transfer.name);
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn debug(state: &TransferState) -> String {
    match state {
        TransferState::Queued => "Queued".to_owned(),
        TransferState::Running => "Running".to_owned(),
        TransferState::Paused => "Paused".to_owned(),
        TransferState::Done => "Done".to_owned(),
        TransferState::Failed(error) => format!("Failed({error})"),
        TransferState::Cancelled => "Cancelled".to_owned(),
        TransferState::Skipped => "Skipped".to_owned(),
    }
}

/// A folder of its own, removed at the end.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_nanos());
        let dir = std::env::temp_dir().join(format!("s3v-transfers-{name}-{stamp:x}"));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn item(path: &str, size: usize) -> Item {
    Item {
        path: RemotePath::parse(path),
        is_dir: false,
        size: size as u64,
    }
}

fn content(size: usize) -> Vec<u8> {
    (0..size).map(|i| (i * 7 % 251) as u8).collect()
}

fn with_policy(worker: &Worker, policy: ConflictPolicy) {
    worker.transfer_rules().apply(&TransferSettings {
        on_conflict: policy,
        ..TransferSettings::default()
    });
}

#[test]
fn a_file_in_the_way_is_asked_about_and_can_be_renamed() {
    let dir = Scratch::new("rename");
    std::fs::write(dir.path("a.txt"), "old").unwrap();
    std::fs::write(dir.path("a (1).txt"), "older").unwrap();
    let memory = Arc::new(Memory::default());
    memory.put("/docs/a.txt", b"new content", T);
    let (worker, events) = Worker::new(egui::Context::default());
    let mut probe = Probe::new(events);

    worker.download(memory.clone(), vec![item("/docs/a.txt", 11)], dir.0.clone());
    let question = probe.question();
    assert_eq!((question.name.as_str(), question.renamed.as_str()), ("a.txt", "a (2).txt"));
    assert_eq!(question.existing.size, 3);
    assert_eq!(question.source, Version { size: 11, modified: Some(T) }, "the date comes from the listing");
    assert!(question.can_resume, "a smaller copy may be the start of the file");
    assert_eq!(question.others, 0);
    let transfer = question.transfer.clone();
    assert!(transfer.asking());
    question.answer(Answer {
        policy: ConflictPolicy::Rename,
        all: false,
    });
    wait_state(&transfer, TransferState::Done);
    assert_eq!(std::fs::read(dir.path("a (2).txt")).unwrap(), b"new content");
    assert_eq!(std::fs::read(dir.path("a.txt")).unwrap(), b"old");
    assert!(transfer.note().unwrap().contains("a (2).txt"));
}

#[test]
fn an_answer_for_all_settles_the_rest_of_the_batch() {
    let dir = Scratch::new("all");
    for name in ["a", "b", "c"] {
        std::fs::write(dir.path(name), "here").unwrap();
    }
    let memory = Arc::new(Memory::default());
    for name in ["a", "b", "c", "d"] {
        memory.put(&format!("/x/{name}"), b"remote", T);
    }
    let (worker, events) = Worker::new(egui::Context::default());
    let mut probe = Probe::new(events);
    let items = ["a", "b", "c", "d"].map(|name| item(&format!("/x/{name}"), 6)).into();
    worker.download(memory, items, dir.0.clone());

    let question = probe.question();
    assert_eq!(question.others, 2);
    question.answer(Answer {
        policy: ConflictPolicy::Skip,
        all: true,
    });
    probe.all_finished(4);
    assert!(probe.questions.is_empty(), "one question for the whole batch");
    let states: Vec<String> = probe.transfers.iter().map(|transfer| debug(&state_of(transfer))).collect();
    assert_eq!(states, ["Skipped", "Skipped", "Skipped", "Done"]);
    assert_eq!(std::fs::read(dir.path("a")).unwrap(), b"here");
    assert_eq!(std::fs::read(dir.path("d")).unwrap(), b"remote");
}

#[test]
fn the_default_rule_decides_without_asking() {
    let dir = Scratch::new("rule");
    std::fs::write(dir.path("same.bin"), content(100)).unwrap();
    std::fs::write(dir.path("other.bin"), content(50)).unwrap();
    let memory = Arc::new(Memory::default());
    memory.put("/r/same.bin", &content(100), T);
    memory.put("/r/other.bin", &content(80), T);
    let (worker, events) = Worker::new(egui::Context::default());
    with_policy(&worker, ConflictPolicy::OverwriteIfSizeDiffers);
    let mut probe = Probe::new(events);
    worker.download(memory, vec![item("/r/same.bin", 100), item("/r/other.bin", 80)], dir.0.clone());
    probe.all_finished(2);
    assert!(probe.questions.is_empty());
    assert_eq!(debug(&state_of(&probe.transfer("same.bin"))), "Skipped");
    assert_eq!(debug(&state_of(&probe.transfer("other.bin"))), "Done");
    assert_eq!(std::fs::read(dir.path("other.bin")).unwrap(), content(80));
}

#[test]
fn a_smaller_copy_is_completed_rather_than_replaced() {
    let dir = Scratch::new("resume");
    let whole = content(300 * 1024);
    // The start of the file, as an interrupted download leaves it.
    std::fs::write(dir.path("big.bin"), &whole[..100 * 1024]).unwrap();
    let memory = Arc::new(Memory::default());
    memory.put("/big.bin", &whole, T);
    let (worker, events) = Worker::new(egui::Context::default());
    let mut probe = Probe::new(events);
    worker.download(memory.clone(), vec![item("/big.bin", whole.len())], dir.0.clone());
    let question = probe.question();
    assert!(question.can_resume);
    let transfer = question.transfer.clone();
    question.answer(Answer {
        policy: ConflictPolicy::Resume,
        all: false,
    });
    wait_state(&transfer, TransferState::Done);
    assert!(std::fs::read(dir.path("big.bin")).unwrap() == whole, "completed bytes differ");
    assert_eq!(memory.starts(), [100 * 1024], "only the rest was fetched");
}

#[test]
fn a_paused_download_goes_on_where_it_stopped() {
    let dir = Scratch::new("pause");
    let whole = content(1024 * 1024);
    let memory = Arc::new(Memory::slow());
    memory.put("/slow.bin", &whole, T);
    let (worker, events) = Worker::new(egui::Context::default());
    let mut probe = Probe::new(events);
    worker.download(memory.clone(), vec![item("/slow.bin", whole.len())], dir.0.clone());
    let transfer = probe.transfer("slow.bin");
    assert!(transfer.can_pause() && transfer.resumable());
    probe.until("a third of the file", |_| transfer.progress.snapshot().0 > 300 * 1024);

    transfer.pause();
    wait_state(&transfer, TransferState::Paused);
    let partial = local_version(&partial_path(&dir.path("slow.bin"))).expect("the partial copy is kept").size;
    assert!(partial > 0 && partial < whole.len() as u64, "{partial}");
    assert!(!dir.path("slow.bin").exists());

    transfer.resume();
    wait_state(&transfer, TransferState::Done);
    assert!(std::fs::read(dir.path("slow.bin")).unwrap() == whole, "resumed bytes differ");
    let starts = memory.starts();
    assert_eq!(starts.len(), 2);
    assert_eq!(starts[1], partial, "the second attempt started from the partial copy");
    assert!(!partial_path(&dir.path("slow.bin")).exists());
}

#[test]
fn a_cancelled_download_leaves_nothing_behind() {
    let dir = Scratch::new("cancel");
    let memory = Arc::new(Memory::slow());
    memory.put("/slow.bin", &content(1024 * 1024), T);
    let (worker, events) = Worker::new(egui::Context::default());
    let mut probe = Probe::new(events);
    worker.download(memory, vec![item("/slow.bin", 1024 * 1024)], dir.0.clone());
    let transfer = probe.transfer("slow.bin");
    probe.until("some data", |_| transfer.progress.snapshot().0 > 100 * 1024);
    transfer.cancel();
    wait_state(&transfer, TransferState::Cancelled);
    probe.until("the partial copy to go", |_| !partial_path(&dir.path("slow.bin")).exists());
    assert!(!dir.path("slow.bin").exists());
}

#[test]
fn an_interrupted_download_left_by_an_earlier_run_is_completed() {
    let dir = Scratch::new("leftover");
    let whole = content(200 * 1024);
    std::fs::write(partial_path(&dir.path("doc.pdf")), &whole[..64 * 1024]).unwrap();
    let memory = Arc::new(Memory::default());
    memory.put("/doc.pdf", &whole, T);
    let (worker, events) = Worker::new(egui::Context::default());
    let mut probe = Probe::new(events);
    worker.download(memory.clone(), vec![item("/doc.pdf", whole.len())], dir.0.clone());
    probe.all_finished(1);
    assert!(probe.questions.is_empty(), "a .part file is ours: no question");
    assert!(std::fs::read(dir.path("doc.pdf")).unwrap() == whole);
    assert_eq!(memory.starts(), [64 * 1024]);
}

#[test]
fn uploads_follow_the_rules_and_resume_after_a_pause() {
    let dir = Scratch::new("upload");
    std::fs::write(dir.path("old.txt"), "local and older").unwrap();
    let whole = content(1024 * 1024);
    std::fs::write(dir.path("big.bin"), &whole).unwrap();
    let memory = Arc::new(Memory::slow());
    // Dated far ahead of the local file: the server's copy is newer.
    memory.put("/up/old.txt", b"remote", i64::from(u32::MAX));
    let (worker, events) = Worker::new(egui::Context::default());
    with_policy(&worker, ConflictPolicy::OverwriteIfNewer);
    let mut probe = Probe::new(events);
    worker.upload(memory.clone(), vec![dir.path("old.txt"), dir.path("big.bin")], RemotePath::parse("/up"));

    let old = probe.transfer("old.txt");
    wait_state(&old, TransferState::Skipped);
    assert_eq!(memory.get("/up/old.txt").unwrap(), b"remote");

    let big = probe.transfer("big.bin");
    probe.until("a third of the upload", |_| big.progress.snapshot().0 > 300 * 1024);
    big.pause();
    wait_state(&big, TransferState::Paused);
    let sent = memory.get("/up/big.bin").map_or(0, |bytes| bytes.len());
    assert!(sent > 0 && sent < whole.len(), "{sent}");
    big.resume();
    wait_state(&big, TransferState::Done);
    assert!(memory.get("/up/big.bin").unwrap() == whole, "uploaded bytes differ");
    assert_eq!(memory.starts()[1], sent as u64, "the upload went on from what the server had");
}

#[test]
fn queued_transfers_wait_their_turn_and_follow_the_limit_of_simultaneous_ones() {
    let dir = Scratch::new("turns");
    let memory = Arc::new(Memory::slow());
    let names: Vec<String> = (1..=4).map(|n| format!("f{n}.bin")).collect();
    for name in &names {
        memory.put(&format!("/q/{name}"), &content(1024 * 1024), T);
    }
    let (worker, events) = Worker::new(egui::Context::default());
    worker.transfer_rules().apply(&TransferSettings {
        parallel: 1,
        ..TransferSettings::default()
    });
    let mut probe = Probe::new(events);
    let items = names.iter().map(|name| item(&format!("/q/{name}"), 1024 * 1024)).collect();
    worker.download(memory, items, dir.0.clone());
    probe.until("all four", |probe| probe.transfers.len() == 4);
    probe.until("the first to run", |probe| state_of(&probe.transfers[0]) == TransferState::Running);
    let rules = worker.transfer_rules().clone();
    let queued: Vec<Option<usize>> = probe.transfers.iter().map(|transfer| rules.slots.position(transfer.id)).collect();
    assert_eq!(queued, [None, Some(1), Some(2), Some(3)]);
    let running = |probe: &Probe| {
        probe
            .transfers
            .iter()
            .filter(|transfer| state_of(transfer) == TransferState::Running)
            .count()
    };
    assert_eq!(running(&probe), 1);
    // Raised while they wait: the others start at once.
    rules.slots.set_max(4);
    probe.until("everything to run", |probe| {
        let done = probe.transfers.iter().filter(|transfer| state_of(transfer) == TransferState::Done);
        running(probe) + done.count() == 4
    });
    probe.all_finished(4);
}

#[test]
fn a_synchronisation_compares_then_makes_both_sides_match() {
    let dir = Scratch::new("sync");
    std::fs::create_dir_all(dir.path("sub")).unwrap();
    std::fs::write(dir.path("here.txt"), "only here").unwrap();
    std::fs::write(dir.path("sub").join("same.txt"), "same").unwrap();
    let memory = Arc::new(Memory::default());
    memory.put("/site/there.txt", b"only there", T);
    let same_date = local_version(&dir.path("sub").join("same.txt")).unwrap().modified.unwrap();
    memory.put("/site/sub/same.txt", b"same", same_date);
    let (worker, events) = Worker::new(egui::Context::default());
    let mut probe = Probe::new(events);
    let rules = Rules {
        direction: SyncDirection::Upload,
        mirror: true,
        precision: 2,
        remote_keeps_dates: true,
    };
    let remote = RemotePath::parse("/site");
    worker.sync_compare(7, memory.clone(), remote.clone(), dir.0.clone(), rules);
    probe.until("the comparison", |probe| !probe.compared.is_empty());
    let (id, result) = probe.compared.remove(0);
    assert_eq!(id, 7);
    let lines = result.unwrap();
    let summary: Vec<(String, SyncAction)> = lines.iter().map(|line| (line.relative.clone(), line.action)).collect();
    assert_eq!(
        summary,
        [("here.txt".to_owned(), SyncAction::Upload), ("there.txt".to_owned(), SyncAction::DeleteRemote)]
    );

    worker.sync_run(memory.clone(), remote.clone(), dir.0.clone(), lines);
    probe.all_finished(1);
    probe.until("the deletion", |probe| !probe.notices.is_empty());
    assert_eq!(memory.get("/site/here.txt").unwrap(), b"only here");
    assert!(memory.get("/site/there.txt").is_none());
    // The copy got the date of its original: nothing differs any more.
    worker.sync_compare(8, memory, remote, dir.0.clone(), rules);
    probe.until("the second comparison", |probe| !probe.compared.is_empty());
    assert_eq!(probe.compared.remove(0).1.unwrap(), []);
}

fn folder_item(path: &str) -> Item {
    Item {
        path: RemotePath::parse(path),
        is_dir: true,
        size: 0,
    }
}

fn summary_of(transfer: &Transfer) -> FolderSummary {
    transfer.folder.as_ref().expect("a folder").summary()
}

#[test]
fn a_new_folder_is_one_transfer_with_its_empty_folders() {
    let dir = Scratch::new("folder");
    let memory = Arc::new(Memory::default());
    memory.put("/site/index.html", b"<html>", T);
    memory.put("/site/css/main.css", b"body {}", T);
    memory.put("/site/empty/", b"", T);
    let (worker, events) = Worker::new(egui::Context::default());
    let mut probe = Probe::new(events);
    worker.download(memory, vec![folder_item("/site")], dir.0.clone());
    let folder = probe.transfer("site");
    wait_state(&folder, TransferState::Done);
    probe.pump();
    assert_eq!(probe.transfers.len(), 1, "its files are not listed");
    let local = dir.path("site");
    assert_eq!(std::fs::read(local.join("index.html")).unwrap(), b"<html>");
    assert_eq!(std::fs::read(local.join("css").join("main.css")).unwrap(), b"body {}");
    assert!(local.join("empty").is_dir());
    let summary = summary_of(&folder);
    assert_eq!((summary.files, summary.finished, summary.arrived), (2, 2, 2));
    assert_eq!(summary.bytes_done, 13);
}

#[test]
fn files_of_a_folder_in_the_way_are_asked_about_once_for_all() {
    let dir = Scratch::new("folder-again");
    let local = dir.path("site");
    std::fs::create_dir_all(local.join("css")).unwrap();
    std::fs::write(local.join("index.html"), "mine").unwrap();
    std::fs::write(local.join("css").join("main.css"), "mine too").unwrap();
    let memory = Arc::new(Memory::default());
    memory.put("/site/index.html", b"<html>", T);
    memory.put("/site/css/main.css", b"body {}", T);
    memory.put("/site/new.txt", b"new", T);
    let (worker, events) = Worker::new(egui::Context::default());
    let mut probe = Probe::new(events);
    worker.download(memory, vec![folder_item("/site")], dir.0.clone());
    let question = probe.question();
    assert_eq!(question.source.modified, Some(T), "the date comes from a listing of its folder");
    question.answer(Answer {
        policy: ConflictPolicy::Skip,
        all: true,
    });
    let folder = probe.transfer("site");
    wait_state(&folder, TransferState::Done);
    assert!(probe.questions.is_empty(), "one question for the whole folder");
    assert_eq!(std::fs::read(local.join("index.html")).unwrap(), b"mine");
    assert_eq!(std::fs::read(local.join("css").join("main.css")).unwrap(), b"mine too");
    assert_eq!(std::fs::read(local.join("new.txt")).unwrap(), b"new");
    let summary = summary_of(&folder);
    assert_eq!((summary.files, summary.arrived, summary.skipped), (3, 1, 2));
}

#[test]
fn a_paused_folder_holds_its_files_and_goes_on_when_resumed() {
    let dir = Scratch::new("folder-pause");
    let memory = Arc::new(Memory::slow());
    let whole = content(1024 * 1024 + 1);
    for name in ["a.bin", "b.bin", "c.bin"] {
        memory.put(&format!("/big/{name}"), &whole, T);
    }
    let (worker, events) = Worker::new(egui::Context::default());
    let mut probe = Probe::new(events);
    worker.download(memory.clone(), vec![folder_item("/big")], dir.0.clone());
    let folder = probe.transfer("big");
    assert!(folder.can_pause() && folder.resumable());
    probe.until("some data", |_| folder.bytes_done() > 300 * 1024);
    folder.pause();
    wait_state(&folder, TransferState::Paused);
    let held = folder.bytes_done();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(folder.bytes_done(), held, "nothing moves while paused");
    folder.resume();
    wait_state(&folder, TransferState::Done);
    for name in ["a.bin", "b.bin", "c.bin"] {
        assert!(std::fs::read(dir.path("big").join(name)).unwrap() == whole, "{name} differs");
    }
    assert!(memory.starts().iter().any(|start| *start > 0), "a paused file went on from its partial copy");
}

#[test]
fn small_files_of_a_folder_run_beside_the_limit() {
    let dir = Scratch::new("folder-small");
    let memory = Arc::new(Memory::slow());
    let size = 16 * 1024 * 1024;
    memory.put("/big.bin", &content(size), T);
    let (worker, events) = Worker::new(egui::Context::default());
    worker.transfer_rules().apply(&TransferSettings {
        parallel: 1,
        ..TransferSettings::default()
    });
    let mut probe = Probe::new(events);
    worker.download(memory, vec![item("/big.bin", size)], dir.0.clone());
    let big = probe.transfer("big.bin");
    wait_state(&big, TransferState::Running);
    let notes = Arc::new(Memory::default());
    for n in 1..=5 {
        notes.put(&format!("/notes/{n}.txt"), b"a short note", T);
    }
    worker.download(notes, vec![folder_item("/notes")], dir.0.clone());
    let folder = probe.transfer("notes");
    wait_state(&folder, TransferState::Done);
    assert_eq!(debug(&state_of(&big)), "Running", "the one turn the user allows is still taken");
    big.cancel();
}

#[test]
fn a_folder_upload_is_one_transfer_and_follows_the_rules() {
    let dir = Scratch::new("folder-up");
    let source = dir.path("photos");
    std::fs::create_dir_all(source.join("2024")).unwrap();
    std::fs::create_dir_all(source.join("vide")).unwrap();
    std::fs::write(source.join("a.jpg"), "aaaa").unwrap();
    std::fs::write(source.join("2024").join("b.jpg"), "bbbb").unwrap();
    let memory = Arc::new(Memory::default());
    memory.put("/up/photos/a.jpg", b"already", T);
    let (worker, events) = Worker::new(egui::Context::default());
    with_policy(&worker, ConflictPolicy::Overwrite);
    let mut probe = Probe::new(events);
    worker.upload(memory.clone(), vec![source], RemotePath::parse("/up"));
    let folder = probe.transfer("photos");
    wait_state(&folder, TransferState::Done);
    probe.pump();
    assert_eq!(probe.transfers.len(), 1);
    assert!(probe.questions.is_empty());
    assert_eq!(memory.get("/up/photos/a.jpg").unwrap(), b"aaaa");
    assert_eq!(memory.get("/up/photos/2024/b.jpg").unwrap(), b"bbbb");
    assert!(memory.get("/up/photos/vide/").is_some(), "the empty folder was made");
    assert_eq!(summary_of(&folder).arrived, 2);
}

#[test]
fn a_cancelled_folder_stops_its_files() {
    let dir = Scratch::new("folder-cancel");
    let memory = Arc::new(Memory::slow());
    for name in ["a.bin", "b.bin"] {
        memory.put(&format!("/slow/{name}"), &content(2 * 1024 * 1024), T);
    }
    let (worker, events) = Worker::new(egui::Context::default());
    let mut probe = Probe::new(events);
    worker.download(memory, vec![folder_item("/slow")], dir.0.clone());
    let folder = probe.transfer("slow");
    probe.until("some data", |_| folder.bytes_done() > 100 * 1024);
    folder.cancel();
    wait_state(&folder, TransferState::Cancelled);
    let parts = folder.folder.as_ref().unwrap();
    let files: Vec<Arc<Transfer>> = lock(&parts.files).clone();
    for file in files {
        wait_state(&file, TransferState::Cancelled);
    }
    probe.until("the partial copies to go", |_| {
        ["a.bin", "b.bin"].iter().all(|name| !partial_path(&dir.path("slow").join(name)).exists())
    });
}
