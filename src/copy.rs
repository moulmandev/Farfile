//! Copying between the two views: from any connection to any other, this
//! computer included. A file goes straight from or to the disk when one side
//! is this computer; between two servers, or two folders of the same one, it
//! passes through a temporary file. Each file is a transfer of the queue.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;

use crate::format;
use crate::storage::{Progress, RemotePath, Storage, storage_key};
use crate::worker::{Direction, Jobs, Worker, safe_component};

/// How often a copy through a temporary file reports its progress.
const FOLLOW_EVERY: Duration = Duration::from_millis(100);

/// What the other view shows, for "Copier vers l'autre vue".
#[derive(Clone, Debug)]
pub struct OtherView {
    /// Where the files would land, for display.
    pub label: String,
    /// It is a folder of this computer.
    pub local: bool,
    /// Files can land there: not the list of buckets or drives.
    pub writable: bool,
}

/// An entry picked in a listing.
#[derive(Clone, Debug)]
pub struct Picked {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

/// What is copied: entries of one folder of a connection.
#[derive(Clone)]
pub struct Source {
    pub storage: Arc<dyn Storage>,
    pub folder: RemotePath,
    pub items: Vec<Picked>,
}

/// Where it goes: a folder of a connection.
#[derive(Clone)]
pub struct Destination {
    pub storage: Arc<dyn Storage>,
    pub folder: RemotePath,
    /// The name of the tab showing it.
    pub title: String,
}

impl Destination {
    /// The folder, the way the user knows it: the path of the disk for this
    /// computer, the tab and the path otherwise.
    pub fn label(&self) -> String {
        place_label(&self.storage, &self.title, &self.folder)
    }
}

/// Where a path of a connection is, for display.
pub fn place_label(storage: &Arc<dyn Storage>, title: &str, path: &RemotePath) -> String {
    match storage.local().and_then(|local| local.resolve(path).ok()) {
        Some(real) => real.display().to_string(),
        None => format!("{title} · {path}"),
    }
}

/// Why this copy cannot be done, if it cannot.
pub fn refusal(source: &Source, dest: &Destination) -> Option<String> {
    if source.items.is_empty() {
        return Some("Rien à copier.".to_owned());
    }
    if dest.storage.has_buckets() && dest.folder.is_root() {
        return Some("Ouvrez d'abord un dossier dans l'autre vue pour y copier.".to_owned());
    }
    if storage_key(&source.storage) != storage_key(&dest.storage) {
        return None;
    }
    let target = parts(&*dest.storage, &dest.folder)?;
    if parts(&*source.storage, &source.folder).as_ref() == Some(&target) {
        return Some("Ces éléments sont déjà dans ce dossier.".to_owned());
    }
    source.items.iter().filter(|item| item.is_dir).find_map(|item| {
        let folder = parts(&*source.storage, &source.folder.child(&item.name))?;
        target
            .starts_with(&folder)
            .then(|| format!("« {} » ne peut pas être copié dans lui-même.", item.name))
    })
}

/// The levels of a path, to compare two of them. Two paths of this computer
/// may name the same folder (Bureau, and C: › Users › … › Desktop): theirs
/// are those of the disk.
fn parts(storage: &dyn Storage, path: &RemotePath) -> Option<Vec<String>> {
    match storage.local() {
        Some(local) => {
            let real = local.resolve(path).ok()?;
            Some(real.components().map(|part| part.as_os_str().to_string_lossy().to_lowercase()).collect())
        }
        None => Some(path.segments().to_vec()),
    }
}

/// What a copy is about to do, in a sentence.
pub fn summary(source: &Source, dest: &Destination) -> String {
    let folders = source.items.iter().filter(|item| item.is_dir).count();
    let place = dest.label();
    match (source.items.as_slice(), folders) {
        ([only], 0) => format!("« {} » sera copié dans {place}.", only.name),
        ([only], _) => format!("Le dossier « {} » et tout son contenu seront copiés dans {place}.", only.name),
        (many, 0) => {
            let size: u64 = many.iter().map(|item| item.size).sum();
            format!("{} éléments ({}) seront copiés dans {place}.", many.len(), format::size(size))
        }
        (many, _) => format!(
            "{} éléments, dont {}, seront copiés dans {place}.",
            many.len(),
            format::count(folders, "dossier", "dossiers"),
        ),
    }
}

/// Copies the entries of `source` into the folder of `dest`, each file as a
/// transfer of the queue. The tabs showing that folder refresh as files
/// arrive.
pub fn start(worker: &Worker, source: Source, dest: Destination) {
    let jobs = worker.jobs();
    worker.spawn(async move { plan(&jobs, source, dest).await });
}

/// A name that is safe on the destination: this computer has rules of its
/// own about names, servers take what they get.
fn landing(dest: &Destination, relative: &str) -> String {
    let parts = relative.split('/').filter(|part| !part.is_empty() && *part != ".");
    if dest.storage.local().is_some() {
        parts.map(safe_component).collect::<Vec<_>>().join("/")
    } else {
        parts.collect::<Vec<_>>().join("/")
    }
}

async fn plan(jobs: &Jobs, source: Source, dest: Destination) {
    for item in &source.items {
        let from = source.folder.child(&item.name);
        let to = dest.folder.child(&landing(&dest, &item.name));
        if !item.is_dir {
            queue_file(jobs, &source.storage, from, &dest, to, item.size);
            continue;
        }
        let found = match crate::storage::walk_all(&*source.storage, &from).await {
            Ok(found) => found,
            Err(error) => {
                jobs.notice(format!("Impossible de lister « {} » : {error:#}", item.name), true);
                continue;
            }
        };
        let tree = Tree::of(found);
        if tree.unreadable > 0 {
            jobs.notice(
                format!(
                    "« {} » : {} laissé(s) de côté",
                    item.name,
                    format::count(tree.unreadable, "dossier illisible", "dossiers illisibles")
                ),
                true,
            );
        }
        if tree.files.is_empty() && tree.empty.is_empty() {
            match dest.storage.create_folder(&to).await {
                Ok(()) => jobs.changed(&dest.storage, dest.folder.clone()),
                Err(error) => jobs.notice(format!("Création de « {} » impossible : {error:#}", item.name), true),
            }
            continue;
        }
        for (relative, size) in tree.files {
            let to = to.child(&landing(&dest, &relative));
            queue_file(jobs, &source.storage, from.child(&relative), &dest, to, size);
        }
        for folder in &tree.empty {
            create_folders(&*dest.storage, &to.child(&landing(&dest, folder))).await;
        }
        if !tree.empty.is_empty() {
            jobs.changed(&dest.storage, dest.folder.clone());
        }
    }
}

/// What a folder to copy holds, from its walk.
#[derive(Debug, Default, PartialEq)]
struct Tree {
    /// With their size.
    files: Vec<(String, u64)>,
    /// The folders that hold nothing at all: files bring the others along.
    empty: Vec<String>,
    unreadable: usize,
}

impl Tree {
    fn of(found: Vec<crate::storage::Found>) -> Self {
        use crate::storage::Found;
        let mut tree = Self::default();
        let mut folders = Vec::new();
        for find in found {
            match find {
                Found::File { relative, size } => tree.files.push((relative, size)),
                Found::Folder { relative } => folders.push(relative),
                Found::Unreadable { relative, .. } => {
                    tree.unreadable += 1;
                    // Not made at the destination: it is not known empty.
                    folders.retain(|folder| *folder != relative);
                }
            }
        }
        let holders: std::collections::HashSet<&str> = tree
            .files
            .iter()
            .map(|(relative, _)| relative.as_str())
            .chain(folders.iter().map(String::as_str))
            .flat_map(|relative| relative.match_indices('/').map(move |(at, _)| &relative[..at]))
            .collect();
        tree.empty = folders.iter().filter(|folder| !holders.contains(folder.as_str())).cloned().collect();
        tree
    }
}

/// Creates `path` and the folders leading to it, which the files being
/// copied alongside may not have created yet. Those that exist already
/// fail, which is fine; S3 needs no parents.
async fn create_folders(storage: &dyn Storage, path: &RemotePath) {
    if storage.has_buckets() && storage.local().is_none() {
        let _ = storage.create_folder(path).await;
        return;
    }
    for depth in 1..=path.segments().len() {
        let _ = storage.create_folder(&path.ancestor(depth)).await;
    }
}

fn queue_file(jobs: &Jobs, storage: &Arc<dyn Storage>, from: RemotePath, dest: &Destination, to: RemotePath, size: u64) {
    let target = place_label(&dest.storage, &dest.title, &to);
    // The arrow says where the data goes: down to this computer, or up.
    let direction = if dest.storage.local().is_some() { Direction::Download } else { Direction::Upload };
    let (source, into) = (storage.clone(), dest.storage.clone());
    let (refresh, folder) = (dest.storage.clone(), dest.folder.clone());
    jobs.queue(
        to.name().to_owned(),
        target,
        direction,
        size,
        move |transfer| async move { copy_file(&*source, &from, &*into, &to, &transfer.progress).await },
        move |jobs, result| {
            if result.is_ok() {
                jobs.changed(&refresh, folder);
            }
        },
    );
}

/// Copies one file from a connection to another.
pub async fn copy_file(
    source: &dyn Storage,
    from: &RemotePath,
    dest: &dyn Storage,
    to: &RemotePath,
    progress: &Progress,
) -> Result<()> {
    if let Some(local) = source.local() {
        return dest.upload(&local.resolve(from)?, to, progress).await;
    }
    if let Some(local) = dest.local() {
        return source.download(from, &local.resolve(to)?, progress).await;
    }
    relay(source, from, dest, to, progress).await
}

/// A file of its own in the temporary folder, for one copy.
fn relay_file(name: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let number = NEXT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir()
        .join("s3viewer-copies")
        .join(format!("{}-{number}-{}", std::process::id(), safe_component(name)))
}

/// Between two servers: down to a temporary file, then up. The transfer
/// counts each byte once, half on the way down and half on the way up, so
/// that its figures are those of the file.
async fn relay(source: &dyn Storage, from: &RemotePath, dest: &dyn Storage, to: &RemotePath, progress: &Progress) -> Result<()> {
    let temp = relay_file(to.name());
    let (down, up) = (Progress::default(), Progress::default());
    let work = async {
        source.download(from, &temp, &down).await?;
        dest.upload(&temp, to, &up).await
    };
    tokio::pin!(work);
    let mut tick = tokio::time::interval(FOLLOW_EVERY);
    let result = loop {
        tokio::select! {
            result = &mut work => break result,
            _ = tick.tick() => {
                if progress.is_cancelled() {
                    down.cancel();
                    up.cancel();
                }
                follow(progress, &down, &up);
            }
        }
    };
    follow(progress, &down, &up);
    let _ = tokio::fs::remove_file(&temp).await;
    result
}

fn follow(progress: &Progress, down: &Progress, up: &Progress) {
    let (fetched, size) = down.snapshot();
    let (sent, _) = up.snapshot();
    if size > 0 {
        progress.set_total(size);
    }
    progress.done.store((fetched + sent) / 2, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::storage::local::tests::{Scratch, storage_in};
    use crate::worker::{Event, Transfer, TransferState};

    fn picked(name: &str, is_dir: bool) -> Picked {
        Picked {
            name: name.to_owned(),
            is_dir,
            size: 0,
        }
    }

    fn local(dir: &Path) -> Arc<dyn Storage> {
        Arc::new(storage_in(dir))
    }

    /// Plays a copy through the queue and waits until every transfer ended.
    /// Returns them, and the folders reported as changed.
    fn run(source: Source, dest: Destination) -> (Vec<Arc<Transfer>>, Vec<RemotePath>) {
        let (worker, events) = Worker::new(egui::Context::default());
        start(&worker, source, dest);
        let (mut transfers, mut changed) = (Vec::new(), Vec::new());
        let mut quiet_since = std::time::Instant::now();
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        loop {
            while let Ok(event) = events.try_recv() {
                quiet_since = std::time::Instant::now();
                match event {
                    Event::TransferAdded(transfer) => transfers.push(transfer),
                    Event::Changed { path, .. } => changed.push(path),
                    Event::Notice { text, error: true } => panic!("{text}"),
                    _ => {}
                }
            }
            let finished = transfers.iter().all(|transfer: &Arc<Transfer>| transfer.state().is_finished());
            if finished && quiet_since.elapsed() > Duration::from_millis(700) {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "the copy never ended");
            std::thread::sleep(Duration::from_millis(20));
        }
        (transfers, changed)
    }

    fn all_done(transfers: &[Arc<Transfer>]) {
        for transfer in transfers {
            assert!(transfer.state() == TransferState::Done, "{}: {:?}", transfer.name, describe(transfer));
        }
    }

    fn describe(transfer: &Transfer) -> String {
        match transfer.state() {
            TransferState::Failed(error) => error,
            _ => "not done".to_owned(),
        }
    }

    /// A small tree: a file, a nested file, and an empty folder.
    fn fixture(dir: &Path) -> Vec<u8> {
        let big: Vec<u8> = (0..1_200_000u32).map(|i| (i * 7 % 253) as u8).collect();
        std::fs::create_dir_all(dir.join("projet/sous dossier")).unwrap();
        std::fs::create_dir_all(dir.join("projet/vide")).unwrap();
        std::fs::write(dir.join("projet/lisez-moi.txt"), "bonjour").unwrap();
        std::fs::write(dir.join("projet/sous dossier/données.bin"), &big).unwrap();
        big
    }

    fn same_tree(copy: &Path, big: &[u8]) {
        assert_eq!(std::fs::read_to_string(copy.join("lisez-moi.txt")).unwrap(), "bonjour");
        assert_eq!(std::fs::read(copy.join("sous dossier/données.bin")).unwrap(), big);
        assert!(copy.join("vide").is_dir(), "the empty folder came along");
    }

    #[test]
    fn folders_copy_between_two_places_of_this_computer() {
        let (from, to) = (Scratch::new("copy-from"), Scratch::new("copy-to"));
        let big = fixture(&from.0);
        let source = Source {
            storage: local(&from.0),
            folder: RemotePath::parse("Essai"),
            items: vec![picked("projet", true)],
        };
        let dest = Destination {
            storage: local(&to.0),
            folder: RemotePath::parse("Essai"),
            title: "Cet ordinateur".to_owned(),
        };
        assert!(refusal(&source, &dest).is_none());
        assert!(summary(&source, &dest).starts_with("Le dossier « projet » et tout son contenu seront copiés dans"));
        let (transfers, changed) = run(source, dest);
        assert_eq!(transfers.len(), 2, "one transfer per file");
        all_done(&transfers);
        assert!(transfers.iter().all(|transfer| transfer.direction == Direction::Download));
        same_tree(&to.0.join("projet"), &big);
        // The folder shown on the other side refreshes.
        assert!(changed.contains(&RemotePath::parse("Essai")));
    }

    /// Of a walk, the files are copied and the folders that hold nothing
    /// are made; unreadable ones are not.
    #[test]
    fn a_walk_tells_what_to_copy_and_what_to_make() {
        use crate::storage::Found;
        let folder = |relative: &str| Found::Folder { relative: relative.to_owned() };
        let tree = Tree::of(vec![
            folder("docs"),
            Found::File { relative: "docs/a.txt".to_owned(), size: 3 },
            folder("docs/vide"),
            folder("plein"),
            folder("plein/sous"),
            folder("plein/sous/fond"),
            folder("secret"),
            Found::Unreadable { relative: "secret".to_owned(), error: "refusé".to_owned() },
        ]);
        assert_eq!(tree.files, [("docs/a.txt".to_owned(), 3)]);
        assert_eq!(tree.empty, ["docs/vide", "plein/sous/fond"]);
        assert_eq!(tree.unreadable, 1);
    }

    #[test]
    fn a_folder_cannot_go_inside_itself() {
        let scratch = Scratch::new("copy-self");
        let storage = local(&scratch.0);
        let source = Source {
            storage: storage.clone(),
            folder: RemotePath::parse("Essai"),
            items: vec![picked("projet", true)],
        };
        let into = |folder: &str| Destination {
            storage: storage.clone(),
            folder: RemotePath::parse(folder),
            title: String::new(),
        };
        assert!(refusal(&source, &into("Essai")).unwrap().contains("déjà dans ce dossier"));
        assert!(refusal(&source, &into("Essai/projet/sous")).unwrap().contains("dans lui-même"));
        assert!(refusal(&source, &into("Essai/projets")).is_none(), "a mere prefix of the name is another folder");
        // Nothing lands at the root of this computer.
        assert!(refusal(&source, &into("")).is_some());
    }

    /// Names that Windows refuses are made safe on the way to the disk.
    #[test]
    fn names_are_made_safe_for_this_computer() {
        let scratch = Scratch::new("copy-names");
        let dest = Destination {
            storage: local(&scratch.0),
            folder: RemotePath::parse("Essai"),
            title: String::new(),
        };
        assert_eq!(landing(&dest, "a:b/c?.txt"), "a_b/c_.txt");
        assert_eq!(landing(&dest, "../x"), "_/x");
    }

    // Live copies against the test servers, skipped unless the variables of
    // `storage::live_tests` (and `S3V_TEST_ENDPOINT`…) are set. Everything
    // happens below `local-test`, removed at the end.

    struct Remote {
        name: &'static str,
        storage: Arc<dyn Storage>,
        base: RemotePath,
    }

    fn server(variable: &str) -> Option<(String, String, String, u16)> {
        let value = std::env::var(variable).ok().filter(|value| !value.is_empty())?;
        let (credentials, address) = value.rsplit_once('@')?;
        let (user, password) = credentials.split_once(':')?;
        let (host, port) = address.rsplit_once(':')?;
        Some((user.to_owned(), password.to_owned(), host.to_owned(), port.parse().ok()?))
    }

    async fn remotes(stamp: &str) -> Vec<Remote> {
        use crate::storage::ftp::{FtpSettings, FtpStorage};
        use crate::storage::s3::{S3Settings, S3Storage};
        use crate::storage::sftp::SftpStorage;
        use crate::storage::ssh::SshSettings;

        let mut found = Vec::new();
        if let Some((user, secret, host, port)) = server("S3V_TEST_SFTP") {
            let storage: Arc<dyn Storage> = Arc::new(SftpStorage::new(Arc::new(SshSettings {
                extra: Default::default(),
                host,
                port,
                user,
                secret,
                key_path: None,
                known_hosts: None,
            })));
            let base = storage.home().await.unwrap().child("local-test").child(stamp);
            found.push(Remote { name: "sftp", storage, base });
        }
        if let Some((user, password, host, port)) = server("S3V_TEST_FTP") {
            let storage: Arc<dyn Storage> = Arc::new(FtpStorage::new(FtpSettings {
                host,
                port,
                user,
                password,
                tls: false,
            }));
            let base = storage.home().await.unwrap().child("local-test").child(stamp);
            found.push(Remote { name: "ftp", storage, base });
        }
        if let (Ok(endpoint), Ok(access_key), Ok(secret_key)) = (
            std::env::var("S3V_TEST_ENDPOINT"),
            std::env::var("S3V_TEST_ACCESS_KEY"),
            std::env::var("S3V_TEST_SECRET_KEY"),
        ) {
            create_bucket(&endpoint, &access_key, &secret_key).await;
            let storage: Arc<dyn Storage> = Arc::new(S3Storage::new(S3Settings {
                endpoint: &endpoint,
                region: "",
                access_key: &access_key,
                secret_key: &secret_key,
                path_style: true,
            }));
            found.push(Remote {
                name: "s3",
                storage,
                base: RemotePath::parse("local-test").child(stamp),
            });
        }
        found
    }

    fn s3_client(endpoint: &str, access_key: &str, secret_key: &str) -> aws_sdk_s3::Client {
        let config = aws_sdk_s3::Config::builder()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .endpoint_url(endpoint)
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .credentials_provider(aws_sdk_s3::config::Credentials::new(access_key, secret_key, None, None, "test"))
            .force_path_style(true)
            .build();
        aws_sdk_s3::Client::from_conf(config)
    }

    async fn create_bucket(endpoint: &str, access_key: &str, secret_key: &str) {
        // Already there from an earlier run: fine.
        let _ = s3_client(endpoint, access_key, secret_key).create_bucket().bucket("local-test").send().await;
    }

    async fn remove_bucket() {
        if let (Ok(endpoint), Ok(access_key), Ok(secret_key)) = (
            std::env::var("S3V_TEST_ENDPOINT"),
            std::env::var("S3V_TEST_ACCESS_KEY"),
            std::env::var("S3V_TEST_SECRET_KEY"),
        ) {
            let _ = s3_client(&endpoint, &access_key, &secret_key).delete_bucket().bucket("local-test").send().await;
        }
    }

    /// The folder `local-test` itself, once the run's own folder is gone.
    async fn remove_namespace(remote: &Remote) {
        if remote.name != "s3"
            && let Some(namespace) = remote.base.parent()
        {
            let _ = remote.storage.delete(&namespace, true).await;
        }
    }

    #[test]
    fn live_copies_between_this_computer_and_servers() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let stamp = format!(
            "{:x}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis()
        );
        let remotes = runtime.block_on(remotes(&stamp));
        if remotes.is_empty() {
            eprintln!("live copies skipped: no test server configured");
            return;
        }
        let scratch = Scratch::new("live-copy");
        let big = fixture(&scratch.0);
        let disk = local(&scratch.0);

        for remote in &remotes {
            // Up: this computer to the server.
            let source = Source {
                storage: disk.clone(),
                folder: RemotePath::parse("Essai"),
                items: vec![picked("projet", true)],
            };
            let dest = Destination {
                storage: remote.storage.clone(),
                folder: remote.base.clone(),
                title: remote.name.to_owned(),
            };
            let (transfers, _) = run(source, dest);
            all_done(&transfers);
            assert!(transfers.iter().all(|transfer| transfer.direction == Direction::Upload));
            let listed = runtime.block_on(remote.storage.list_page(&remote.base.child("projet"), None)).unwrap();
            let mut names: Vec<(String, bool)> =
                listed.entries.into_iter().map(|entry| (entry.name.clone(), entry.is_dir())).collect();
            names.sort();
            let expected = [("lisez-moi.txt", false), ("sous dossier", true), ("vide", true)];
            assert_eq!(names, expected.map(|(name, dir)| (name.to_owned(), dir)), "{}", remote.name);

            // Down again, elsewhere.
            let back = format!("retour-{}", remote.name);
            std::fs::create_dir_all(scratch.0.join(&back)).unwrap();
            let source = Source {
                storage: remote.storage.clone(),
                folder: remote.base.clone(),
                items: vec![picked("projet", true)],
            };
            let dest = Destination {
                storage: disk.clone(),
                folder: RemotePath::parse("Essai").child(&back),
                title: "Cet ordinateur".to_owned(),
            };
            let (transfers, _) = run(source, dest);
            all_done(&transfers);
            let copy = scratch.0.join(&back).join("projet");
            assert_eq!(std::fs::read_to_string(copy.join("lisez-moi.txt")).unwrap(), "bonjour", "{}", remote.name);
            assert_eq!(std::fs::read(copy.join("sous dossier/données.bin")).unwrap(), big, "{}", remote.name);
        }

        // Server to server, through the client: every pair, both ways.
        let pairs = remotes
            .iter()
            .flat_map(|from| remotes.iter().filter(move |to| to.name != from.name).map(move |to| (from, to)));
        for (from, to) in pairs {
            let into = to.base.child(&format!("de-{}", from.name));
            runtime.block_on(to.storage.create_folder(&into)).ok();
            let source = Source {
                storage: from.storage.clone(),
                folder: from.base.child("projet"),
                items: vec![picked("sous dossier", true), picked("lisez-moi.txt", false)],
            };
            let dest = Destination {
                storage: to.storage.clone(),
                folder: into.clone(),
                title: to.name.to_owned(),
            };
            let (transfers, changed) = run(source, dest);
            all_done(&transfers);
            assert!(changed.contains(&into), "{} → {}: {changed:?}", from.name, to.name);
            let big_transfer = transfers.iter().find(|transfer| transfer.name == "données.bin").unwrap();
            assert_eq!(big_transfer.progress.snapshot(), (big.len() as u64, big.len() as u64), "counted once");

            let check = scratch.0.join(format!("verif-{}-{}", from.name, to.name));
            let fetched = check.join("données.bin");
            runtime
                .block_on(to.storage.download(&into.child("sous dossier/données.bin"), &fetched, &Progress::default()))
                .unwrap();
            assert_eq!(std::fs::read(&fetched).unwrap(), big, "{} → {}", from.name, to.name);
        }
        // No temporary copy is left behind.
        let leftovers = std::fs::read_dir(std::env::temp_dir().join("s3viewer-copies"))
            .map(|dir| {
                dir.flatten()
                    .filter(|entry| entry.file_name().to_string_lossy().starts_with(&format!("{}-", std::process::id())))
                    .count()
            })
            .unwrap_or(0);
        assert_eq!(leftovers, 0);

        runtime.block_on(async {
            for remote in &remotes {
                remote.storage.delete(&remote.base, true).await.expect("cleaning up");
                remove_namespace(remote).await;
                if remote.name != "s3" {
                    let home = remote.storage.home().await.unwrap();
                    let left = remote.storage.list_page(&home, None).await.unwrap().entries;
                    assert!(left.iter().all(|entry| entry.name != "local-test"), "{} keeps local-test", remote.name);
                }
            }
            remove_bucket().await;
        });
    }
}
