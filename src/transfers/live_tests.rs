//! Resume, speed limits and synchronisation against real servers, in a
//! `transfers-test` folder (or bucket) removed at the end. Skipped unless
//! the variables of `storage/live_tests.rs` name the servers:
//! `S3V_TEST_SFTP`, `S3V_TEST_FTP`, and `S3V_TEST_ENDPOINT`,
//! `S3V_TEST_ACCESS_KEY`, `S3V_TEST_SECRET_KEY` for S3.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::limit::Limiter;
use super::sync::{Rules, SyncAction, SyncDirection, SyncLine};
use crate::storage::ftp::{FtpSettings, FtpStorage};
use crate::storage::s3::{S3Settings, S3Storage};
use crate::storage::sftp::SftpStorage;
use crate::storage::ssh::SshSettings;
use crate::storage::{Progress, RemotePath, Storage};
use crate::worker::transfer::TransferEvent;
use crate::worker::{Event, Transfer, Worker};

const FOLDER: &str = "transfers-test";

fn target(variable: &str) -> Option<(String, String, String, u16)> {
    let value = std::env::var(variable).ok().filter(|value| !value.is_empty())?;
    let (credentials, address) = value.rsplit_once('@')?;
    let (user, password) = credentials.split_once(':')?;
    let (host, port) = address.rsplit_once(':')?;
    Some((user.to_owned(), password.to_owned(), host.to_owned(), port.parse().ok()?))
}

fn sftp() -> Option<Arc<dyn Storage>> {
    let (user, secret, host, port) = target("S3V_TEST_SFTP")?;
    Some(Arc::new(SftpStorage::new(Arc::new(SshSettings {
        extra: Default::default(),
        host,
        port,
        user,
        secret,
        key_path: None,
        known_hosts: None,
    }))))
}

fn ftp() -> Option<Arc<dyn Storage>> {
    let (user, password, host, port) = target("S3V_TEST_FTP")?;
    Some(Arc::new(FtpStorage::new(FtpSettings {
        host,
        port,
        user,
        password,
        tls: false,
    })))
}

/// S3 needs its client built inside a runtime.
fn s3() -> Option<S3Storage> {
    let endpoint = std::env::var("S3V_TEST_ENDPOINT").ok()?;
    let access_key = std::env::var("S3V_TEST_ACCESS_KEY").ok()?;
    let secret_key = std::env::var("S3V_TEST_SECRET_KEY").ok()?;
    Some(S3Storage::new(S3Settings {
        endpoint: &endpoint,
        region: "",
        access_key: &access_key,
        secret_key: &secret_key,
        path_style: true,
    }))
}

fn stamp() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_nanos());
    format!("{nanos:x}")
}

fn content(size: usize) -> Vec<u8> {
    (0..size).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect()
}

/// A local folder of the test, removed at the end.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("{FOLDER}-{name}-{}", stamp()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn limited(rate: u64) -> Progress {
    let progress = Progress::default();
    let limiter = Arc::new(Limiter::default());
    limiter.set_rate(rate);
    progress.set_limiter(limiter);
    progress
}

/// Runs `transfer` and pauses it once `pause_at` bytes went through.
async fn interrupted<F>(progress: Arc<Progress>, pause_at: u64, transfer: F) -> anyhow::Result<()>
where
    F: Future<Output = anyhow::Result<()>>,
{
    let watcher = {
        let progress = progress.clone();
        tokio::spawn(async move {
            while progress.snapshot().0 < pause_at {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            progress.pause();
        })
    };
    let result = transfer.await;
    watcher.abort();
    result
}

async fn remote_size(storage: &dyn Storage, path: &RemotePath) -> Option<u64> {
    let folder = path.parent()?;
    let listed = storage.list_page(&folder, None).await.ok()?;
    listed.entries.into_iter().find(|entry| entry.name == path.name())?.size
}

/// Interrupts a download and an upload halfway, completes them and
/// compares the bytes; then checks the speed limit holds.
async fn resume_and_limits(name: &str, storage: Arc<dyn Storage>, base: RemotePath, s3: Option<&S3Storage>) {
    let local = Scratch::new(name);
    let whole = content(3 * 1024 * 1024);
    let source = local.0.join("source.bin");
    std::fs::write(&source, &whole).unwrap();
    let remote = base.child("big.bin");
    storage.upload(&source, &remote, &Progress::default()).await.expect("upload");

    // Download, paused after a megabyte, then completed.
    assert!(storage.can_resume_download(), "{name} resumes downloads");
    let partial = local.0.join("big.bin.part");
    let progress = Arc::new(limited(4 * 1024 * 1024));
    let first = interrupted(progress.clone(), 1024 * 1024, async {
        storage.download_resumable(&remote, &partial, false, &progress).await
    })
    .await;
    assert!(first.is_err() && progress.is_paused(), "{name}: the download stopped");
    let kept = std::fs::metadata(&partial).expect("partial copy kept").len();
    assert!(kept >= 1024 * 1024 && kept < whole.len() as u64, "{name}: kept {kept}");
    let progress = Progress::default();
    storage.download_resumable(&remote, &partial, true, &progress).await.expect("resumed download");
    assert!(std::fs::read(&partial).unwrap() == whole, "{name}: resumed download differs");
    assert_eq!(progress.snapshot(), (whole.len() as u64, whole.len() as u64));
    println!("{name}: download paused at {kept} bytes and completed");

    // Upload, the same way. S3 only resumes uploads sent in parts.
    if let Some(s3) = s3 {
        let big = content(20 * 1024 * 1024);
        let file = local.0.join("big-up.bin");
        std::fs::write(&file, &big).unwrap();
        let target = base.child("big-up.bin");
        let progress = Arc::new(limited(8 * 1024 * 1024));
        let first = interrupted(progress.clone(), 8 * 1024 * 1024, async {
            storage.upload_resumable(&file, &target, false, &progress).await
        })
        .await;
        assert!(first.is_err() && progress.is_paused(), "s3: the upload stopped");
        let (bucket, key) = target.segments().split_first().expect("bucket");
        let parts = s3.paused_parts_for_tests(bucket, &key.join("/")).await.expect("paused upload");
        assert!((1..3).contains(&parts), "s3: {parts} parts kept on the server");
        assert!(remote_size(storage.as_ref(), &target).await.is_none(), "not an object until completed");
        let progress = Progress::default();
        storage.upload_resumable(&file, &target, true, &progress).await.expect("resumed upload");
        let back = local.0.join("big-back.bin");
        storage.download(&target, &back, &Progress::default()).await.unwrap();
        assert!(std::fs::read(&back).unwrap() == big, "s3: resumed upload differs");
        assert!(s3.paused_parts_for_tests(bucket, &key.join("/")).await.is_err(), "nothing left paused");
        println!("s3: multipart upload paused with {parts} part(s) on the server and completed");
    } else if storage.can_resume_upload() {
        let target = base.child("up.bin");
        let progress = Arc::new(limited(4 * 1024 * 1024));
        let first = interrupted(progress.clone(), 1024 * 1024, async {
            storage.upload_resumable(&source, &target, false, &progress).await
        })
        .await;
        assert!(first.is_err(), "{name}: the upload stopped");
        let sent = remote_size(storage.as_ref(), &target).await.expect("partial upload kept");
        assert!(sent > 0 && sent < whole.len() as u64, "{name}: server has {sent}");
        let progress = Progress::default();
        storage.upload_resumable(&source, &target, true, &progress).await.expect("resumed upload");
        let back = local.0.join("back.bin");
        storage.download(&target, &back, &Progress::default()).await.unwrap();
        assert!(std::fs::read(&back).unwrap() == whole, "{name}: resumed upload differs");
        println!("{name}: upload paused with {sent} bytes on the server and completed");
    }

    // 2 Mo at 1 Mo/s each way: about two seconds, less the first burst.
    let two = content(2 * 1024 * 1024);
    std::fs::write(&source, &two).unwrap();
    let start = Instant::now();
    storage.upload(&source, &base.child("two.bin"), &limited(1024 * 1024)).await.unwrap();
    let up = start.elapsed().as_secs_f64();
    let start = Instant::now();
    storage
        .download(&base.child("two.bin"), &local.0.join("two.bin"), &limited(1024 * 1024))
        .await
        .unwrap();
    let down = start.elapsed().as_secs_f64();
    println!("{name}: 2 Mo limited to 1 Mo/s took {up:.2} s up, {down:.2} s down");
    assert!((1.5..3.0).contains(&up), "{name}: upload took {up} s");
    assert!((1.5..3.0).contains(&down), "{name}: download took {down} s");
}

/// What the worker reports, followed until a condition holds.
struct Probe {
    events: std::sync::mpsc::Receiver<Event>,
    transfers: Vec<Arc<Transfer>>,
    compared: Vec<Result<Vec<SyncLine>, String>>,
    notices: Vec<String>,
}

impl Probe {
    fn pump(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                Event::TransferAdded(transfer) => self.transfers.push(transfer),
                Event::Transfers(TransferEvent::Compared { result, .. }) => self.compared.push(result),
                Event::Notice { text, .. } => self.notices.push(text),
                _ => {}
            }
        }
    }

    async fn until(&mut self, what: &str, mut done: impl FnMut(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            self.pump();
            if done(self) {
                return;
            }
            assert!(Instant::now() < deadline, "still waiting for {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn compare(&mut self, worker: &Worker, storage: &Arc<dyn Storage>, remote: &RemotePath, local: &Path, rules: Rules) -> Vec<SyncLine> {
        worker.sync_compare(1, storage.clone(), remote.clone(), local.to_path_buf(), rules);
        self.until("the comparison", |probe| !probe.compared.is_empty()).await;
        self.compared.remove(0).expect("comparison")
    }
}

fn summary(lines: &[SyncLine]) -> Vec<(String, SyncAction)> {
    lines.iter().map(|line| (line.relative.clone(), line.action)).collect()
}

/// Sends a local tree in mirror mode, checks nothing differs afterwards,
/// then brings back a file deleted locally, with its date.
async fn synchronise(name: &str, storage: Arc<dyn Storage>, remote: RemotePath, worker: &Worker, probe: &mut Probe) {
    let local = Scratch::new(&format!("{name}-sync"));
    std::fs::create_dir_all(local.0.join("sub")).unwrap();
    std::fs::write(local.0.join("index.html"), "<h1>bonjour</h1>").unwrap();
    std::fs::write(local.0.join("sub").join("data.json"), r#"{"a":1}"#).unwrap();
    let orphan = local.0.join("orphan.txt");
    std::fs::write(&orphan, "x").unwrap();
    storage.upload(&orphan, &remote.child("only-remote.txt"), &Progress::default()).await.unwrap();
    std::fs::remove_file(&orphan).unwrap();

    let rules = Rules {
        direction: SyncDirection::Upload,
        mirror: true,
        precision: storage.date_precision(),
        remote_keeps_dates: storage.keeps_dates(),
    };
    let lines = probe.compare(worker, &storage, &remote, &local.0, rules).await;
    assert_eq!(
        summary(&lines),
        [
            ("index.html".to_owned(), SyncAction::Upload),
            ("only-remote.txt".to_owned(), SyncAction::DeleteRemote),
            ("sub/data.json".to_owned(), SyncAction::Upload),
        ],
        "{name}"
    );
    let before = probe.transfers.len();
    probe.notices.clear();
    worker.sync_run(storage.clone(), remote.clone(), local.0.clone(), lines);
    probe
        .until("the uploads", |probe| {
            probe.transfers.len() == before + 2
                && probe.transfers[before..].iter().all(|transfer| transfer.state().is_finished())
        })
        .await;
    probe.until("the deletion", |probe| !probe.notices.is_empty()).await;
    for transfer in &probe.transfers[before..] {
        assert!(transfer.state() == crate::worker::TransferState::Done, "{name}: {} failed", transfer.name);
    }
    let again = probe.compare(worker, &storage, &remote, &local.0, rules).await;
    assert_eq!(summary(&again), [], "{name}: nothing differs after a synchronisation");

    // Deleted here: receiving brings it back, dated like the remote copy.
    std::fs::remove_file(local.0.join("index.html")).unwrap();
    let receive = Rules {
        direction: SyncDirection::Download,
        mirror: false,
        ..rules
    };
    let lines = probe.compare(worker, &storage, &remote, &local.0, receive).await;
    assert_eq!(summary(&lines), [("index.html".to_owned(), SyncAction::Download)], "{name}");
    let remote_date = lines[0].remote.and_then(|file| file.modified);
    let before = probe.transfers.len();
    worker.sync_run(storage.clone(), remote.clone(), local.0.clone(), lines);
    probe
        .until("the download", |probe| {
            probe.transfers.len() == before + 1 && probe.transfers[before].state().is_finished()
        })
        .await;
    assert_eq!(std::fs::read_to_string(local.0.join("index.html")).unwrap(), "<h1>bonjour</h1>");
    let local_date = std::fs::metadata(local.0.join("index.html"))
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    assert_eq!(Some(local_date), remote_date, "{name}: the copy keeps the date of the server");
    let both = Rules {
        direction: SyncDirection::Both,
        ..rules
    };
    assert_eq!(summary(&probe.compare(worker, &storage, &remote, &local.0, both).await), [], "{name}");
    println!("{name}: synchronised both ways");
}

/// Empties `folder`; a bucket cannot be deleted that way, only what it holds.
async fn clear(storage: &dyn Storage, folder: &RemotePath) {
    let Ok(listed) = storage.list_page(folder, None).await else {
        return;
    };
    for entry in listed.entries {
        storage.delete(&folder.child(&entry.name), entry.is_dir()).await.expect("cleanup");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn live_resume_limits_and_sync() {
    let mut backends: Vec<(&str, Arc<dyn Storage>)> = Vec::new();
    if let Some(storage) = sftp() {
        backends.push(("sftp", storage));
    }
    if let Some(storage) = ftp() {
        backends.push(("ftp", storage));
    }
    let s3 = s3().map(Arc::new);
    if let Some(storage) = &s3 {
        let _ = storage.create_bucket_for_tests(FOLDER).await;
        backends.push(("s3", storage.clone()));
    }
    if backends.is_empty() {
        eprintln!("live test skipped: S3V_TEST_* not set");
        return;
    }

    let (worker, events) = tokio::task::block_in_place(|| Worker::new(egui::Context::default()));
    let mut probe = Probe {
        events,
        transfers: Vec::new(),
        compared: Vec::new(),
        notices: Vec::new(),
    };
    for (name, storage) in backends {
        let root = if storage.has_buckets() {
            RemotePath::parse(FOLDER)
        } else {
            storage.home().await.expect("home").child(FOLDER)
        };
        // What a failed run may have left goes first.
        clear(storage.as_ref(), &root).await;
        let base = root.child(&stamp());
        storage.create_folder(&base).await.ok();
        let concrete = s3.as_deref().filter(|_| storage.has_buckets());
        resume_and_limits(name, storage.clone(), base.child("resume"), concrete).await;
        let sync = base.child("sync");
        storage.create_folder(&sync).await.ok();
        synchronise(name, storage.clone(), sync, &worker, &mut probe).await;
        clear(storage.as_ref(), &root).await;
        if !storage.has_buckets() {
            storage.delete(&root, true).await.expect("cleanup");
        }
    }
    if let Some(storage) = &s3 {
        storage.delete_bucket_for_tests(FOLDER).await.expect("bucket removed");
    }
    tokio::task::block_in_place(|| drop(worker));
}

/// What the worker reports about folder transfers.
struct FolderProbe {
    events: std::sync::mpsc::Receiver<Event>,
    transfers: Vec<Arc<Transfer>>,
    questions: Vec<crate::worker::transfer::Question>,
    /// The transfers looked at already.
    seen: usize,
}

impl FolderProbe {
    fn pump(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                Event::TransferAdded(transfer) => self.transfers.push(transfer),
                Event::Transfers(TransferEvent::Ask(question)) => self.questions.push(question),
                _ => {}
            }
        }
    }

    async fn until(&mut self, what: &str, mut done: impl FnMut(&mut Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            self.pump();
            if done(self) {
                return;
            }
            assert!(Instant::now() < deadline, "still waiting for {what}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// The next folder transfer listed, once it reaches `state`.
    async fn folder(&mut self, what: &str, state: crate::worker::TransferState) -> Arc<Transfer> {
        let next = self.seen;
        self.until(what, |probe| probe.transfers.get(next).is_some_and(|transfer| transfer.state() == state)).await;
        self.seen = next + 1;
        let transfer = self.transfers[next].clone();
        assert!(transfer.folder.is_some(), "{what}: one transfer for the folder");
        transfer
    }
}

/// A file as compared: its size and a checksum of its bytes.
fn print(bytes: &[u8]) -> (usize, u64) {
    let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3));
    (bytes.len(), hash)
}

/// The files of a local tree, `/`-separated, with their print.
fn local_tree(dir: &Path) -> std::collections::BTreeMap<String, (usize, u64)> {
    fn walk(dir: &Path, prefix: &str, files: &mut std::collections::BTreeMap<String, (usize, u64)>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let relative = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
            if entry.path().is_dir() {
                walk(&entry.path(), &relative, files);
            } else {
                files.insert(relative, print(&std::fs::read(entry.path()).unwrap()));
            }
        }
    }
    let mut files = std::collections::BTreeMap::new();
    walk(dir, "", &mut files);
    files
}

/// A folder downloaded whole (in one stream from SFTP), downloaded again
/// over itself (one question for all its files), paused while it streams,
/// and uploaded back: one transfer each time.
async fn folders(name: &str, storage: Arc<dyn Storage>, base: RemotePath, worker: &Worker, probe: &mut FolderProbe) {
    use crate::transfers::conflict::ConflictPolicy;
    use crate::worker::transfer::Answer;
    use crate::worker::{Item, TransferState};

    let seed = Scratch::new(&format!("{name}-seed"));
    let tree = base.child("tree");
    let mut expected = std::collections::BTreeMap::new();
    expected.insert("big/large.bin".to_owned(), content(24 * 1024 * 1024 + 7));
    for n in 0..24 {
        expected.insert(format!("many/note {n}.txt"), format!("note {n}\n").into_bytes());
    }
    expected.insert("a.txt".to_owned(), b"alpha\n".to_vec());
    for (relative, bytes) in &expected {
        let file = seed.0.join("file");
        std::fs::write(&file, bytes).unwrap();
        storage.upload(&file, &tree.child(relative), &Progress::default()).await.expect("seed");
    }
    let sizes: Vec<(String, u64)> = expected.iter().map(|(relative, bytes)| (relative.clone(), bytes.len() as u64)).collect();
    let expected: std::collections::BTreeMap<String, (usize, u64)> =
        expected.iter().map(|(relative, bytes)| (relative.clone(), print(bytes))).collect();
    storage.create_folder(&tree.child("empty")).await.expect("empty folder");
    let item = || Item {
        path: tree.clone(),
        is_dir: true,
        size: 0,
    };

    // 1. Not here yet: comes whole, in one stream where the server can.
    let local = Scratch::new(&format!("{name}-folder"));
    worker.download(storage.clone(), vec![item()], local.0.clone());
    let started = Instant::now();
    let folder = probe.folder("the folder", TransferState::Done).await;
    let streamed = folder.folder.as_ref().unwrap().streamed_files();
    println!("{name}: {} files in {:.2} s, {streamed} through the stream", expected.len(), started.elapsed().as_secs_f64());
    assert_eq!(local_tree(&local.0.join("tree")), expected, "{name}");
    assert!(local.0.join("tree").join("empty").is_dir(), "{name}: the empty folder");
    if storage.can_download_tree() {
        assert_eq!(streamed as usize, expected.len(), "{name}: one stream");
    }
    let count = probe.transfers.len();

    // 2. Again, over itself: one question, answered for all.
    worker.download(storage.clone(), vec![item()], local.0.clone());
    probe.until("a question", |probe| !probe.questions.is_empty()).await;
    let question = probe.questions.remove(0);
    question.answer(Answer {
        policy: ConflictPolicy::Skip,
        all: true,
    });
    let again = probe.folder("the folder again", TransferState::Done).await;
    assert_eq!(probe.transfers.len(), count + 1, "{name}: its files are not listed");
    let summary = again.folder.as_ref().unwrap().summary();
    assert_eq!((summary.files, summary.skipped), (expected.len(), expected.len()), "{name}");
    probe.pump();
    assert!(probe.questions.is_empty(), "{name}: one question for the whole folder");

    // 3. Paused as it comes, then resumed: the rest goes file by file.
    let paused = Scratch::new(&format!("{name}-paused"));
    worker.download(storage.clone(), vec![item()], paused.0.clone());
    probe.until("the start", |probe| probe.transfers.len() == count + 2).await;
    let third = probe.transfers.last().cloned().unwrap();
    probe.seen += 1;
    probe.until("some data", |_| third.bytes_done() > 0).await;
    third.pause();
    probe
        .until("the pause", |_| matches!(third.state(), TransferState::Paused | TransferState::Done))
        .await;
    println!("{name}: paused at {} bytes ({})", third.bytes_done(), if third.state() == TransferState::Done { "too late" } else { "in time" });
    third.resume();
    probe.until("the end", |_| third.state() == TransferState::Done).await;
    assert_eq!(local_tree(&paused.0.join("tree")), expected, "{name}: paused and resumed");

    // 4. Under a speed limit, file by file; one set while it streams sends
    // the rest that way.
    if storage.can_download_tree() {
        let limit = &worker.transfer_rules().download;
        limit.set_rate(512 * 1024 * 1024);
        let limited = Scratch::new(&format!("{name}-limited"));
        worker.download(storage.clone(), vec![item()], limited.0.clone());
        let folder = probe.folder("the limited folder", TransferState::Done).await;
        assert_eq!(folder.folder.as_ref().unwrap().streamed_files(), 0, "{name}: no stream under a limit");
        assert_eq!(local_tree(&limited.0.join("tree")), expected, "{name}: limited");

        limit.set_rate(0);
        let switched = Scratch::new(&format!("{name}-switched"));
        worker.download(storage.clone(), vec![item()], switched.0.clone());
        probe.until("the start", |probe| probe.transfers.len() > probe.seen).await;
        let folder = probe.transfers[probe.seen].clone();
        probe.until("some data", |_| folder.bytes_done() > 0).await;
        limit.set_rate(512 * 1024 * 1024);
        let folder = probe.folder("the switched folder", TransferState::Done).await;
        limit.set_rate(0);
        let streamed = folder.folder.as_ref().unwrap().streamed_files();
        println!("{name}: {streamed} files streamed before the limit, the rest one by one");
        assert_eq!(local_tree(&switched.0.join("tree")), expected, "{name}: switched");
    }

    // 5. And back up, as one transfer.
    let up = base.child("up");
    storage.create_folder(&up).await.ok();
    worker.upload(storage.clone(), vec![local.0.join("tree")], up.clone());
    probe.folder("the upload", TransferState::Done).await;
    let sent = crate::storage::files_below(storage.as_ref(), &up.child("tree")).await.unwrap();
    assert_eq!(sent, sizes, "{name}: uploaded");
    println!("{name}: folders as one transfer, questions, pause and upload");
}

#[tokio::test(flavor = "multi_thread")]
async fn live_folders_as_one_transfer() {
    let mut backends: Vec<(&str, Arc<dyn Storage>)> = Vec::new();
    if let Some(storage) = sftp() {
        backends.push(("sftp", storage));
    }
    if let Some(storage) = ftp() {
        backends.push(("ftp", storage));
    }
    let s3 = s3().map(Arc::new);
    const BUCKET: &str = "transfers-test-folders";
    if let Some(storage) = &s3 {
        let _ = storage.create_bucket_for_tests(BUCKET).await;
        backends.push(("s3", storage.clone()));
    }
    if backends.is_empty() {
        eprintln!("live test skipped: S3V_TEST_* not set");
        return;
    }
    let (worker, events) = tokio::task::block_in_place(|| Worker::new(egui::Context::default()));
    let mut probe = FolderProbe {
        events,
        transfers: Vec::new(),
        questions: Vec::new(),
        seen: 0,
    };
    for (name, storage) in backends {
        let root = if storage.has_buckets() {
            RemotePath::parse(BUCKET)
        } else {
            storage.home().await.expect("home").child(&format!("{FOLDER}-folders"))
        };
        clear(storage.as_ref(), &root).await;
        let base = root.child(&stamp());
        storage.create_folder(&base).await.ok();
        folders(name, storage.clone(), base, &worker, &mut probe).await;
        clear(storage.as_ref(), &root).await;
        if !storage.has_buckets() {
            storage.delete(&root, true).await.expect("cleanup");
        }
    }
    if let Some(storage) = &s3 {
        storage.delete_bucket_for_tests(BUCKET).await.expect("bucket removed");
    }
    tokio::task::block_in_place(|| drop(worker));
}
