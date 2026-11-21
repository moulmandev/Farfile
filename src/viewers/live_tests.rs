//! The viewers against real servers: ranged reads on the three protocols,
//! and followers seeing a file grow, shrink and be replaced. Skipped unless
//! the variables of `storage/live_tests.rs` name the servers. Each protocol
//! works in a `viewers-test` folder (a bucket for S3), removed at the end.
//!
//! With `S3V_TEST_SSH_CONTAINER` naming the Docker container of the SFTP
//! server (`s3v-ssh` locally), lines are also appended from inside it, as
//! a program writing its log would.

use std::sync::Arc;
use std::time::Duration;

use super::follow::{Follow, Status};
use super::task::Spawner;
use crate::storage::ftp::{FtpSettings, FtpStorage};
use crate::storage::s3::{S3Settings, S3Storage};
use crate::storage::sftp::SftpStorage;
use crate::storage::ssh::SshSettings;
use crate::storage::{Progress, RemotePath, Storage};

const FOLDER: &str = "viewers-test";
const SIZE: usize = 3_000_000;

struct Target {
    user: String,
    password: String,
    host: String,
    port: u16,
}

fn target(variable: &str) -> Option<Target> {
    let value = std::env::var(variable).ok().filter(|value| !value.is_empty())?;
    let (credentials, address) = value.rsplit_once('@')?;
    let (user, password) = credentials.split_once(':')?;
    let (host, port) = address.rsplit_once(':')?;
    Some(Target {
        user: user.to_owned(),
        password: password.to_owned(),
        host: host.to_owned(),
        port: port.parse().ok()?,
    })
}

fn sample(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// Puts `content` on the server as `remote`, through a local file.
async fn put(storage: &dyn Storage, remote: &RemotePath, content: &[u8]) {
    // The tests of the three protocols run side by side.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let number = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let local = std::env::temp_dir().join(format!("s3v-viewers-{}-{number}", std::process::id()));
    std::fs::write(&local, content).unwrap();
    storage.upload(&local, remote, &Progress::default()).await.expect("upload");
    let _ = std::fs::remove_file(&local);
}

async fn check_ranges(storage: &dyn Storage, dir: &RemotePath) {
    let content = sample(SIZE);
    let remote = dir.child("ranges.bin");
    put(storage, &remote, &content).await;

    for (offset, len) in [(0, 10), (1_000_000, 70_000), (SIZE as u64 - 10, 100), (0, SIZE as u64)] {
        let chunk = storage.read_range(&remote, offset, len).await.expect("ranged read");
        assert_eq!(chunk.size, SIZE as u64);
        let end = (offset + len).min(SIZE as u64) as usize;
        assert!(chunk.bytes == content[offset as usize..end], "bytes from {offset}, {len} long");
    }
    // Past the end, or nothing asked: only the size comes back.
    for (offset, len) in [(SIZE as u64, 10), (2 * SIZE as u64, 10), (100, 0)] {
        let chunk = storage.read_range(&remote, offset, len).await.expect("read past the end");
        assert!(chunk.bytes.is_empty(), "nothing at {offset}");
        assert_eq!(chunk.size, SIZE as u64);
    }
    // Reads that stop before the end leave the connection usable.
    for round in 0..4u64 {
        let chunk = storage.read_range(&remote, 400_000 + round, 1_000).await.expect("read again");
        assert!(chunk.bytes == content[400_000 + round as usize..401_000 + round as usize]);
    }
    let listed = storage.list_page(dir, None).await.expect("listing after reads");
    assert!(listed.entries.iter().any(|entry| entry.name == "ranges.bin"));

    let empty = dir.child("empty.txt");
    put(storage, &empty, b"").await;
    let chunk = storage.read_range(&empty, 0, 10).await.expect("empty file");
    assert_eq!((chunk.bytes.len(), chunk.size), (0, 0));
    assert!(storage.read_range(&dir.child("missing.txt"), 0, 10).await.is_err());
}

/// Waits until the follower shows what `want` looks for.
async fn wait_for(follow: &mut Follow, what: &str, want: impl Fn(&str) -> bool) {
    for _ in 0..120 {
        follow.pump(0.0);
        if want(&follow.log.shown_text()) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("{what}: {:?}, shown: {:?}", follow.status, follow.log.shown_text());
}

/// A file rewritten longer, then shorter, as uploads do.
async fn check_follow(storage: Arc<dyn Storage>, dir: &RemotePath) {
    let remote = dir.child("rewritten.log");
    put(&*storage, &remote, b"INFO a\nWARN b\n").await;
    let spawner = Spawner::on(tokio::runtime::Handle::current());
    let mut follow = Follow::start(&spawner, storage.clone(), remote.clone(), 14, 1);
    wait_for(&mut follow, "first lines", |text| text == "INFO a\nWARN b").await;
    assert_eq!(follow.status, Status::Live);

    put(&*storage, &remote, b"INFO a\nWARN b\nERROR c\n").await;
    wait_for(&mut follow, "appended line", |text| text.ends_with("WARN b\nERROR c")).await;

    put(&*storage, &remote, b"fresh\n").await;
    wait_for(&mut follow, "rotation", |text| text.contains("tronqué ou remplacé") && text.ends_with("\nfresh")).await;
    drop(follow);
}

/// Lines appended by a program on the server itself.
async fn check_follow_appends(storage: Arc<dyn Storage>, dir: &RemotePath, container: &str) {
    let remote = dir.child("app.log");
    put(&*storage, &remote, b"starting\n").await;
    let spawner = Spawner::on(tokio::runtime::Handle::current());
    let mut follow = Follow::start(&spawner, storage, remote.clone(), 9, 2);
    wait_for(&mut follow, "start", |text| text == "starting").await;

    let file = remote.to_string();
    let shell = |script: String| {
        let mut command = std::process::Command::new("docker");
        command.args(["exec", container, "sh", "-c", &script]);
        // Never a console window on the desktop.
        #[cfg(windows)]
        std::os::windows::process::CommandExt::creation_flags(&mut command, 0x0800_0000);
        let status = command.status().expect("docker");
        assert!(status.success(), "{script}");
    };
    shell(format!("printf '2026-11-20 ERROR disk full\\nhalf a li' >> '{file}'"));
    wait_for(&mut follow, "appended lines", |text| text.ends_with("ERROR disk full\nhalf a li")).await;
    shell(format!("printf 'ne\\n' >> '{file}'"));
    wait_for(&mut follow, "the line completed", |text| text.ends_with("ERROR disk full\nhalf a line")).await;
    // Rotated the way `logrotate copytruncate` does it.
    shell(format!(": > '{file}' && printf 'after rotation\\n' >> '{file}'"));
    wait_for(&mut follow, "rotation", |text| text.ends_with("remplacé : suite du nouveau contenu\nafter rotation")).await;
}

async fn workspace(storage: &dyn Storage) -> RemotePath {
    let dir = storage.home().await.expect("home").child(FOLDER);
    // Left by an interrupted run, maybe.
    let _ = storage.create_folder(&dir).await;
    dir
}

#[tokio::test(flavor = "multi_thread")]
async fn sftp_viewers() {
    let Some(target) = target("S3V_TEST_SFTP") else {
        return;
    };
    let storage: Arc<dyn Storage> = Arc::new(SftpStorage::new(Arc::new(SshSettings {
        extra: Default::default(),
        host: target.host,
        port: target.port,
        user: target.user,
        secret: target.password,
        key_path: None,
        known_hosts: None,
    })));
    let dir = workspace(&*storage).await;
    check_ranges(&*storage, &dir).await;
    check_follow(storage.clone(), &dir).await;
    if let Ok(container) = std::env::var("S3V_TEST_SSH_CONTAINER") {
        check_follow_appends(storage.clone(), &dir, &container).await;
    }
    storage.delete(&dir, true).await.expect("clean up");
}

#[tokio::test(flavor = "multi_thread")]
async fn ftp_viewers() {
    let Some(target) = target("S3V_TEST_FTP") else {
        return;
    };
    let storage: Arc<dyn Storage> = Arc::new(FtpStorage::new(FtpSettings {
        host: target.host,
        port: target.port,
        user: target.user,
        password: target.password,
        tls: false,
    }));
    let dir = workspace(&*storage).await;
    check_ranges(&*storage, &dir).await;
    check_follow(storage.clone(), &dir).await;
    storage.delete(&dir, true).await.expect("clean up");
}

/// The S3 server of the tests, and a client of its own to make buckets.
fn s3() -> Option<(Arc<dyn Storage>, aws_sdk_s3::Client)> {
    use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
    let (Ok(endpoint), Ok(access_key), Ok(secret_key)) = (
        std::env::var("S3V_TEST_ENDPOINT"),
        std::env::var("S3V_TEST_ACCESS_KEY"),
        std::env::var("S3V_TEST_SECRET_KEY"),
    ) else {
        return None;
    };
    let config = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .credentials_provider(Credentials::new(&access_key, &secret_key, None, None, "test"))
        .force_path_style(true)
        .endpoint_url(&endpoint)
        .build();
    let storage: Arc<dyn Storage> = Arc::new(S3Storage::new(S3Settings {
        endpoint: &endpoint,
        region: "",
        access_key: &access_key,
        secret_key: &secret_key,
        path_style: true,
    }));
    Some((storage, aws_sdk_s3::Client::from_conf(config)))
}

#[tokio::test(flavor = "multi_thread")]
async fn s3_viewers() {
    let Some((storage, client)) = s3() else {
        return;
    };
    // Left by an interrupted run, maybe.
    let _ = client.create_bucket().bucket(FOLDER).send().await;
    let dir = RemotePath::parse(FOLDER).child("files");
    check_ranges(&*storage, &dir).await;
    check_follow(storage.clone(), &dir).await;
    storage.delete(&dir, true).await.expect("clean up");
    // Kept while `seed_viewers` put files in it.
    let _ = client.delete_bucket().bucket(FOLDER).send().await;
}

/// The test servers, with where `viewers-test` goes on each.
async fn servers() -> Vec<(Arc<dyn Storage>, RemotePath)> {
    let mut servers: Vec<(Arc<dyn Storage>, RemotePath)> = Vec::new();
    if let Some((storage, client)) = s3() {
        let _ = client.create_bucket().bucket(FOLDER).send().await;
        servers.push((storage, RemotePath::parse(FOLDER)));
    }
    if let Some(target) = target("S3V_TEST_SFTP") {
        let storage: Arc<dyn Storage> = Arc::new(SftpStorage::new(Arc::new(SshSettings {
            extra: Default::default(),
            host: target.host,
            port: target.port,
            user: target.user,
            secret: target.password,
            key_path: None,
            known_hosts: None,
        })));
        let dir = workspace(&*storage).await;
        servers.push((storage, dir));
    }
    if let Some(target) = target("S3V_TEST_FTP") {
        let storage: Arc<dyn Storage> = Arc::new(FtpStorage::new(FtpSettings {
            host: target.host,
            port: target.port,
            user: target.user,
            password: target.password,
            tls: false,
        }));
        let dir = workspace(&*storage).await;
        servers.push((storage, dir));
    }
    servers
}

/// Not a test: puts the folder named by `S3V_VIEWERS_SEED` on each test
/// server as `viewers-test`, to try the viewers in the app.
/// `cargo test seed_viewers -- --ignored`, and `clean_viewers` after.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn seed_viewers() {
    let source = std::path::PathBuf::from(std::env::var("S3V_VIEWERS_SEED").expect("S3V_VIEWERS_SEED"));
    let mut files = Vec::new();
    let mut pending = vec![(source, String::new())];
    while let Some((dir, prefix)) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let relative = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
            if entry.path().is_dir() {
                pending.push((entry.path(), relative));
            } else {
                files.push((entry.path(), relative));
            }
        }
    }
    for (storage, dir) in servers().await {
        for (local, relative) in &files {
            storage.upload(local, &dir.child(relative), &Progress::default()).await.expect("seed");
        }
    }
}

/// Not a test: removes what `seed_viewers` put on the servers.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn clean_viewers() {
    for (storage, dir) in servers().await {
        if storage.has_buckets() {
            let listed = storage.list_page(&dir, None).await.map(|page| page.entries).unwrap_or_default();
            for entry in listed {
                let _ = storage.delete(&dir.child(&entry.name), entry.is_dir()).await;
            }
        } else {
            let _ = storage.delete(&dir, true).await;
        }
    }
    if let Some((_, client)) = s3() {
        let _ = client.delete_bucket().bucket(FOLDER).send().await;
    }
}
