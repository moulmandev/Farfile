//! Round trips against real servers. They are skipped unless the matching
//! variable names one, as `user:password@host:port`:
//!
//! - `S3V_TEST_SFTP`, e.g. `dev:devpass123@127.0.0.1:2222`
//! - `S3V_TEST_FTP`, e.g. `dev:devpass123@127.0.0.1:2121`
//!
//! `scripts/dev-run.ps1` documents how to start such servers locally.

use std::sync::Arc;

use super::ftp::{FtpSettings, FtpStorage};
use super::sftp::SftpStorage;
use super::ssh::SshSettings;
use super::{Progress, RemotePath, Storage};

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

/// Exercises every operation of the trait below the account's home folder.
async fn round_trip(storage: &dyn Storage) {
    let home = storage.home().await.expect("home folder");
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let base = home.child(&format!("s3v-test-{stamp:x}"));
    let local = std::env::temp_dir().join(format!("s3v-test-{stamp:x}"));
    std::fs::create_dir_all(&local).unwrap();

    // A file large enough to span several chunks, with a recognisable content.
    let content: Vec<u8> = (0..1_500_000u32).map(|i| (i % 251) as u8).collect();
    let source = local.join("source.bin");
    std::fs::write(&source, &content).unwrap();

    storage.create_folder(&base).await.expect("create folder");
    let listed = storage.list_page(&home, None).await.expect("list home");
    let created = listed.entries.iter().find(|entry| entry.name == base.name());
    assert!(created.is_some_and(|entry| entry.is_dir()), "the new folder is listed as a folder");

    // Uploading into folders that do not exist yet creates them.
    let nested = base.child("a b/deeper/data.bin");
    let progress = Progress::default();
    storage.upload(&source, &nested, &progress).await.expect("upload");
    assert_eq!(progress.snapshot().0, content.len() as u64);
    storage
        .upload(&source, &base.child("top.bin"), &Progress::default())
        .await
        .expect("second upload");

    let listed = storage.list_page(&base, None).await.expect("list");
    let mut names: Vec<(String, bool, Option<u64>)> = listed
        .entries
        .iter()
        .map(|entry| (entry.name.clone(), entry.is_dir(), entry.size))
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            ("a b".to_owned(), true, None),
            ("top.bin".to_owned(), false, Some(content.len() as u64)),
        ]
    );
    assert!(listed.entries.iter().all(|entry| entry.modified.is_some()));

    let deep = super::files_below(storage, &base).await.expect("deep listing");
    assert_eq!(
        deep,
        [
            ("a b/deeper/data.bin".to_owned(), content.len() as u64),
            ("top.bin".to_owned(), content.len() as u64),
        ]
    );

    let copy = local.join("copy.bin");
    let progress = Progress::default();
    storage.download(&nested, &copy, &progress).await.expect("download");
    assert_eq!(std::fs::read(&copy).unwrap(), content);
    assert_eq!(progress.snapshot().0, content.len() as u64);

    // A cancelled download leaves nothing behind.
    let cancelled = Progress::default();
    cancelled.cancel();
    let partial = local.join("cancelled.bin");
    assert!(storage.download(&nested, &partial, &cancelled).await.is_err());
    assert!(!partial.exists());
    assert!(!local.join("cancelled.bin.part").exists());

    assert!(storage.download(&base.child("missing"), &partial, &Progress::default()).await.is_err());
    // The connection must still be usable after an error.
    assert_eq!(storage.delete(&base.child("top.bin"), false).await.expect("delete file"), 1);
    // Two folders and one file remain.
    assert_eq!(storage.delete(&base, true).await.expect("delete folder"), 4);
    let listed = storage.list_page(&home, None).await.expect("list home again");
    assert!(listed.entries.iter().all(|entry| entry.name != base.name()));

    let _ = std::fs::remove_dir_all(&local);
}

#[tokio::test]
async fn sftp_round_trip() {
    let Some(target) = target("S3V_TEST_SFTP") else {
        return;
    };
    let storage = SftpStorage::new(Arc::new(SshSettings {
        host: target.host,
        port: target.port,
        user: target.user,
        secret: target.password,
        key_path: None,
        known_hosts: None,
        ..SshSettings::default()
    }));
    round_trip(&storage).await;
    assert!(storage.plain_url(&RemotePath::parse("/a/b")).unwrap().starts_with("sftp://"));
}

#[tokio::test]
async fn ftp_round_trip() {
    let Some(target) = target("S3V_TEST_FTP") else {
        return;
    };
    let storage = FtpStorage::new(FtpSettings {
        host: target.host,
        port: target.port,
        user: target.user,
        password: target.password,
        tls: false,
    });
    round_trip(&storage).await;
}

fn ssh_settings(target: &Target, known_hosts: Option<std::path::PathBuf>) -> Arc<SshSettings> {
    Arc::new(SshSettings {
        host: target.host.clone(),
        port: target.port,
        user: target.user.clone(),
        secret: target.password.clone(),
        key_path: None,
        known_hosts,
        ..SshSettings::default()
    })
}

/// A host is recorded on the first visit, accepted afterwards, and refused
/// as soon as it presents another key.
#[tokio::test]
async fn sftp_remembers_host_keys() {
    let Some(target) = target("S3V_TEST_SFTP") else {
        return;
    };
    let known = std::env::temp_dir().join(format!("s3v-known-hosts-{}", std::process::id()));
    let _ = std::fs::remove_file(&known);
    let visit = || SftpStorage::new(ssh_settings(&target, Some(known.clone())));

    visit().home().await.expect("the first visit records the key");
    let recorded = std::fs::read_to_string(&known).expect("the key was written");
    let entries = recorded.lines().filter(|line| !line.trim().is_empty()).count();
    assert_eq!(entries, 1, "one entry for this host: {recorded}");

    visit().home().await.expect("a known host is accepted");
    assert_eq!(std::fs::read_to_string(&known).unwrap(), recorded, "nothing was added");

    // Same host, same key type, another key.
    let (head, key) = recorded.trim_end().rsplit_once(' ').unwrap();
    let position = key.len() - 12;
    let swapped = if key.as_bytes()[position] == b'A' { "B" } else { "A" };
    let forged = format!("{head} {}{swapped}{}\n", &key[..position], &key[position + 1..]);
    std::fs::write(&known, forged).unwrap();
    let error = visit().home().await.expect_err("a changed key must be refused");
    assert!(format!("{error:#}").contains("a changé"), "unexpected error: {error:#}");

    let _ = std::fs::remove_file(&known);
}

/// `S3V_TEST_SFTP_KEY` names a private key the test server accepts, and
/// `S3V_TEST_SFTP_KEY_PASSPHRASE` its passphrase if it has one.
#[tokio::test]
async fn sftp_key_authentication() {
    let (Some(target), Ok(key)) = (target("S3V_TEST_SFTP"), std::env::var("S3V_TEST_SFTP_KEY")) else {
        return;
    };
    let with = |secret: &str| {
        SftpStorage::new(Arc::new(SshSettings {
            host: target.host.clone(),
            port: target.port,
            user: target.user.clone(),
            secret: secret.to_owned(),
            key_path: Some(key.clone().into()),
            known_hosts: None,
            ..SshSettings::default()
        }))
    };
    let passphrase = std::env::var("S3V_TEST_SFTP_KEY_PASSPHRASE").unwrap_or_default();
    with(&passphrase).home().await.expect("key authentication");
    if !passphrase.is_empty() {
        let error = with("not the passphrase").home().await.expect_err("wrong passphrase");
        assert!(format!("{error:#}").contains("clé privée illisible"), "unexpected error: {error:#}");
    }
}

/// `S3V_TEST_FTPS` names a server speaking explicit TLS with a certificate
/// the system trusts. Read-only: lists the start folder.
#[tokio::test]
async fn ftps_listing() {
    let Some(target) = target("S3V_TEST_FTPS") else {
        return;
    };
    let storage = FtpStorage::new(FtpSettings {
        host: target.host,
        port: target.port,
        user: target.user,
        password: target.password,
        tls: true,
    });
    let home = storage.home().await.expect("TLS session");
    let listed = storage.list_page(&home, None).await.expect("listing over TLS");
    assert!(!listed.entries.is_empty());
    if let Some(file) = listed.entries.iter().find(|entry| !entry.is_dir()) {
        let dest = std::env::temp_dir().join(format!("s3v-ftps-{}", std::process::id()));
        storage
            .download(&home.child(&file.name), &dest, &Progress::default())
            .await
            .expect("download over TLS");
        assert_eq!(std::fs::metadata(&dest).unwrap().len(), file.size.unwrap_or(0));
        let _ = std::fs::remove_file(&dest);
    }
}

/// Not a test: prints how fast a large file goes up and down.
/// `cargo test throughput -- --ignored --nocapture`
#[tokio::test]
#[ignore]
async fn throughput() {
    const SIZE: usize = 200 * 1024 * 1024;
    let source = std::env::temp_dir().join(format!("s3v-speed-{}.bin", std::process::id()));
    let block: Vec<u8> = (0..1024 * 1024u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
    std::fs::write(&source, block.repeat(SIZE / block.len())).unwrap();

    let mut backends: Vec<(&str, Box<dyn Storage>)> = Vec::new();
    if let Some(target) = target("S3V_TEST_SFTP") {
        backends.push(("sftp", Box::new(SftpStorage::new(ssh_settings(&target, None)))));
    }
    if let Some(target) = target("S3V_TEST_FTP") {
        backends.push((
            "ftp",
            Box::new(FtpStorage::new(FtpSettings {
                host: target.host,
                port: target.port,
                user: target.user,
                password: target.password,
                tls: false,
            })),
        ));
    }

    let megabytes = (SIZE / (1024 * 1024)) as f64;
    for (name, storage) in backends {
        let remote = storage.home().await.unwrap().child("s3v-speed.bin");
        let copy = source.with_extension("copy");

        let start = std::time::Instant::now();
        storage.upload(&source, &remote, &Progress::default()).await.unwrap();
        let up = megabytes / start.elapsed().as_secs_f64();

        let start = std::time::Instant::now();
        storage.download(&remote, &copy, &Progress::default()).await.unwrap();
        let down = megabytes / start.elapsed().as_secs_f64();

        assert_eq!(std::fs::metadata(&copy).unwrap().len(), SIZE as u64);
        storage.delete(&remote, false).await.unwrap();
        let _ = std::fs::remove_file(&copy);
        println!("{name}: upload {up:.0} Mo/s, download {down:.0} Mo/s");
    }
    let _ = std::fs::remove_file(&source);
}

/// A folder of many small files, in one stream and then file by file, 4
/// and 16 at a time, with the time each way takes. `S3V_TEST_TREE` names
/// such a folder on the SFTP server; `S3V_TEST_EDGE` one holding `a.txt`,
/// a link to it, an empty folder and an unreadable one, `private`.
#[tokio::test(flavor = "multi_thread")]
async fn sftp_folder_in_one_stream() {
    use std::sync::atomic::Ordering;

    use futures_util::StreamExt;

    use super::{TreeProgress, files_below};

    let (Some(target), Ok(tree)) = (target("S3V_TEST_SFTP"), std::env::var("S3V_TEST_TREE")) else {
        return;
    };
    let storage = SftpStorage::new(ssh_settings(&target, None));
    let remote = RemotePath::parse(&tree);
    let local = std::env::temp_dir().join(format!("s3v-tree-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&local);
    let files = files_below(&storage, &remote).await.unwrap();

    let start = std::time::Instant::now();
    let progress = TreeProgress::default();
    let streamed = local.join("stream");
    let report = storage
        .download_tree(&remote, &streamed, &progress)
        .await
        .unwrap()
        .expect("this server runs tar");
    let stream_time = start.elapsed();
    assert!(report.unreadable.is_empty() && report.unwritable.is_empty());
    assert_eq!(progress.files_done.load(Ordering::Relaxed), files.len() as u64);
    assert_eq!(progress.files.load(Ordering::Relaxed), files.len() as u64);

    for parallel in [4, 16] {
        let copy = local.join(format!("files-{parallel}"));
        let start = std::time::Instant::now();
        futures_util::stream::iter(&files)
            .map(|(relative, _)| {
                let (storage, dest, source) = (&storage, copy.join(relative), remote.child(relative));
                async move { storage.download(&source, &dest, &Progress::default()).await }
            })
            .buffer_unordered(parallel)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<anyhow::Result<Vec<()>>>()
            .unwrap();
        println!("file by file, {parallel} at a time: {:.1} s", start.elapsed().as_secs_f64());
        for (relative, size) in &files {
            let one = std::fs::read(streamed.join(relative)).unwrap();
            assert_eq!(one.len() as u64, *size, "{relative}");
            assert_eq!(one, std::fs::read(copy.join(relative)).unwrap(), "{relative}");
        }
    }
    println!("{} files in one stream: {:.1} s", files.len(), stream_time.as_secs_f64());

    if let Ok(edge) = std::env::var("S3V_TEST_EDGE") {
        let dest = local.join("edge");
        let report = storage
            .download_tree(&RemotePath::parse(&edge), &dest, &TreeProgress::default())
            .await
            .unwrap()
            .expect("this server runs tar");
        assert_eq!(std::fs::read_to_string(dest.join("a.txt")).unwrap(), "alpha\n");
        assert!(dest.join("empty").is_dir());
        assert!(!dest.join("private").exists(), "an unreadable folder is not made");
        assert_eq!(report.links, 1);
        assert_eq!(report.unreadable.len(), 1, "{:?}", report.unreadable);
        println!("unreadable: {:?}", report.unreadable);
    }
    let _ = std::fs::remove_dir_all(&local);
}

/// Folder names that a shell or `find` could take for something else are
/// fetched as names, and nothing next to them is touched.
#[tokio::test(flavor = "multi_thread")]
async fn sftp_tree_names_stay_names() {
    use super::TreeProgress;

    let Some(target) = target("S3V_TEST_SFTP") else {
        return;
    };
    let storage = SftpStorage::new(ssh_settings(&target, None));
    let home = storage.home().await.unwrap();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let base = home.child(&format!("s3v-names-{stamp:x}"));
    let local = std::env::temp_dir().join(format!("s3v-names-{stamp:x}"));
    let source = local.join("source.txt");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::write(&source, "kept").unwrap();

    let put = |path: RemotePath| {
        let (storage, source) = (&storage, &source);
        async move { storage.upload(source, &path, &Progress::default()).await.unwrap() }
    };
    put(base.child("sibling.txt")).await;
    put(base.child("-delete").child("a.txt")).await;
    let tricky = base.child("@NAME@").child("$(touch pwned)");
    put(tricky.child("b.txt")).await;

    for (remote, file) in [(base.child("-delete"), "a.txt"), (tricky.clone(), "b.txt")] {
        let dest = local.join(remote.name().replace(['$', '(', ')', ' '], "_"));
        storage
            .download_tree(&remote, &dest, &TreeProgress::default())
            .await
            .unwrap()
            .expect("this server runs tar");
        assert_eq!(std::fs::read_to_string(dest.join(file)).unwrap(), "kept", "{remote}");
    }

    let names = |path: RemotePath| {
        let storage = &storage;
        async move {
            let page = storage.list_page(&path, None).await.unwrap();
            page.entries.into_iter().map(|entry| entry.name).collect::<Vec<_>>()
        }
    };
    assert!(names(base.clone()).await.contains(&"sibling.txt".to_owned()), "find deleted what was next to the folder");
    assert!(!names(home.clone()).await.contains(&"pwned".to_owned()), "a name ran as a command");
    assert!(!names(base.child("@NAME@")).await.contains(&"pwned".to_owned()), "a name ran as a command");

    storage.delete(&base, true).await.unwrap();
    let _ = std::fs::remove_dir_all(&local);
}
