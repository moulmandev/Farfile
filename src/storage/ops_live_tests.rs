//! The file operations against real servers, skipped unless the variables
//! of `live_tests` name them (`S3V_TEST_SFTP`, `S3V_TEST_FTP`; S3 runs from
//! `s3::ops`). Everything happens in a `file-ops-test` folder, removed at
//! the end.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use super::ftp::{FtpSettings, FtpStorage};
use super::ops::{Tally, list_all, walk_size, walk_tree};
use super::sftp::SftpStorage;
use super::ssh::SshSettings;
use super::{Progress, RemotePath, Storage};

/// `user:password@host:port`.
fn target(variable: &str) -> Option<(String, String, String, u16)> {
    let value = std::env::var(variable).ok().filter(|value| !value.is_empty())?;
    let (credentials, address) = value.rsplit_once('@')?;
    let (user, password) = credentials.split_once(':')?;
    let (host, port) = address.rsplit_once(':')?;
    Some((user.to_owned(), password.to_owned(), host.to_owned(), port.parse().ok()?))
}

pub(crate) fn stamp() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{nanos:x}")
}

async fn names(storage: &dyn Storage, path: &RemotePath) -> BTreeMap<String, bool> {
    list_all(storage, path)
        .await
        .expect("listing")
        .into_iter()
        .map(|entry| (entry.name.clone(), entry.is_dir()))
        .collect()
}

async fn put(storage: &dyn Storage, local: &Path, remote: &RemotePath, bytes: &[u8]) {
    std::fs::write(local, bytes).unwrap();
    storage.upload(local, remote, &Progress::default()).await.expect("upload");
}

async fn fetch(storage: &dyn Storage, local: &Path, remote: &RemotePath) -> Vec<u8> {
    storage.download(remote, local, &Progress::default()).await.expect("download");
    std::fs::read(local).unwrap()
}

/// Rename, move, copy, folder size, permissions, owner and properties, as far
/// as the connection supports them. `base` must not exist yet.
pub(super) async fn exercise(storage: &dyn Storage, base: &RemotePath) {
    let ops = storage.operations();
    let local = std::env::temp_dir().join(format!("s3v-file-ops-{}", stamp()));
    std::fs::create_dir_all(&local).unwrap();
    let big: Vec<u8> = (0..700_000u32).map(|i| (i % 249) as u8).collect();

    storage.create_folder(base).await.expect("base folder");
    put(storage, &local.join("a.txt"), &base.child("a.txt"), b"alpha").await;
    put(storage, &local.join("b.bin"), &base.child("b.bin"), &big).await;
    put(storage, &local.join("c.txt"), &base.child("dir/inner/c.txt"), b"gamma").await;
    let listed = names(storage, base).await;
    assert_eq!(listed.keys().collect::<Vec<_>>(), ["a.txt", "b.bin", "dir"]);

    // Rename a file, then a folder.
    let progress = Progress::default();
    storage.rename(&base.child("a.txt"), &base.child("a 2.txt"), false, &progress).await.expect("rename file");
    storage.rename(&base.child("dir"), &base.child("dir 2"), true, &progress).await.expect("rename folder");
    let listed = names(storage, base).await;
    assert_eq!(listed.keys().collect::<Vec<_>>(), ["a 2.txt", "b.bin", "dir 2"]);
    assert_eq!(fetch(storage, &local.join("c.back"), &base.child("dir 2/inner/c.txt")).await, b"gamma");

    // Move a file into a folder, and a folder into another one.
    storage.create_folder(&base.child("other")).await.expect("second folder");
    storage
        .rename(&base.child("a 2.txt"), &base.child("dir 2/a 2.txt"), false, &progress)
        .await
        .expect("move file");
    storage
        .rename(&base.child("dir 2/inner"), &base.child("other/inner"), true, &progress)
        .await
        .expect("move folder");
    assert_eq!(names(storage, &base.child("dir 2")).await.keys().collect::<Vec<_>>(), ["a 2.txt"]);
    assert_eq!(names(storage, &base.child("other/inner")).await.keys().collect::<Vec<_>>(), ["c.txt"]);
    // A folder cannot go inside itself, whatever the protocol says.
    assert!(
        storage
            .rename(&base.child("other"), &base.child("other/inner/other"), true, &progress)
            .await
            .is_err()
    );

    // Copy a file and a whole folder, on the server.
    let progress = Progress::default();
    storage.copy(&base.child("b.bin"), &base.child("b copy.bin"), false, &progress).await.expect("copy file");
    assert_eq!(fetch(storage, &local.join("b.back"), &base.child("b copy.bin")).await, big);
    storage.copy(&base.child("other"), &base.child("other copy"), true, &progress).await.expect("copy folder");
    let sorted = |mut tree: super::ops::Tree| {
        tree.folders.sort();
        tree.files.sort();
        tree
    };
    let original = walk_tree(storage, &base.child("other"), &Progress::default()).await.unwrap();
    let copied = walk_tree(storage, &base.child("other copy"), &Progress::default()).await.unwrap();
    assert_eq!(sorted(original), sorted(copied));
    assert_eq!(fetch(storage, &local.join("c2.back"), &base.child("other copy/inner/c.txt")).await, b"gamma");

    // What the base holds now: b.bin, b copy.bin, dir 2/a 2.txt,
    // other/inner/c.txt, other copy/inner/c.txt.
    let tally = Tally::default();
    storage.folder_size(base, &tally).await.expect("folder size");
    let size = tally.snapshot();
    assert_eq!(size.files, 5);
    assert_eq!(size.bytes, 2 * big.len() as u64 + 5 + 5 + 5);
    assert_eq!(size.folders, 5, "dir 2, other, other/inner, other copy, other copy/inner");
    assert_eq!(size.unreadable, 0);
    let cancelled = Tally::default();
    cancelled.cancel();
    assert!(storage.folder_size(base, &cancelled).await.is_err());

    // A folder below that may not be read is left out, as `du` does, and
    // said so; the count goes on without it. Both ways of counting.
    let locked = base.child("other/inner");
    if ops.permissions {
        storage.set_permissions(&locked, 0o000).await.expect("lock a folder");
        // Unless the server runs as root, which reads anything.
        if list_all(storage, &locked).await.is_err() {
            let fast = Tally::default();
            storage.folder_size(base, &fast).await.expect("size around a locked folder");
            let walked = Tally::default();
            walk_size(storage, base, &walked).await.expect("walk around a locked folder");
            for size in [fast.snapshot(), walked.snapshot()] {
                assert_eq!(size.files, 4, "c.txt of the locked folder is missing: {size:?}");
                assert_eq!(size.bytes, 2 * big.len() as u64 + 5 + 5, "{size:?}");
                assert_eq!(size.folders, 5, "{size:?}");
                assert_eq!(size.unreadable, 1, "{size:?}");
            }
            // The folder asked for itself must be read.
            assert!(storage.folder_size(&locked, &Tally::default()).await.is_err());        }
        storage.set_permissions(&locked, 0o755).await.expect("unlock the folder");
    }

    let properties = storage.properties(&base.child("b.bin"), false).await.expect("properties");
    if properties.size.is_some() {
        assert_eq!(properties.size, Some(big.len() as u64));
    }

    if ops.permissions {
        storage.set_permissions(&base.child("b.bin"), 0o640).await.expect("chmod");
        let entries = list_all(storage, base).await.unwrap();
        let file = entries.iter().find(|entry| entry.name == "b.bin").unwrap();
        assert_eq!(file.permissions, Some(0o640));
        storage.set_permissions(&base.child("dir 2"), 0o750).await.expect("chmod folder");
        let entries = list_all(storage, base).await.unwrap();
        let folder = entries.iter().find(|entry| entry.name == "dir 2").unwrap();
        assert_eq!(folder.permissions, Some(0o750));
    }
    if ops.details {
        let entries = list_all(storage, base).await.unwrap();
        assert!(entries.iter().all(|entry| entry.owner.is_some() && entry.permissions.is_some()));
    }
    if ops.owner {
        let accounts = storage.accounts().await.expect("accounts");
        let properties = storage.properties(&base.child("b.bin"), false).await.unwrap();
        let (uid, gid) = (properties.uid.unwrap(), properties.gid.unwrap());
        let owner = accounts.user(uid).map_or_else(|| uid.to_string(), str::to_owned);
        assert_eq!(properties.owner.as_deref(), Some(owner.as_str()));
        assert_eq!(properties.permissions, Some(0o640));
        // Giving a file to its own owner works without being root, giving
        // it to root does not.
        storage.set_owner(&base.child("b.bin"), Some(uid), None).await.expect("chown to self");
        storage.set_owner(&base.child("b.bin"), None, Some(gid)).await.expect("chgrp to own group");
        assert!(storage.set_owner(&base.child("b.bin"), Some(0), None).await.is_err());
        // Names show in listings once the accounts are known.
        let entries = list_all(storage, base).await.unwrap();
        let file = entries.iter().find(|entry| entry.name == "b.bin").unwrap();
        assert_eq!(file.owner.as_deref(), Some(owner.as_str()));
    }

    let removed = storage.delete(base, true).await.expect("cleanup");
    assert!(removed >= 5);
    let _ = std::fs::remove_dir_all(&local);
}

/// Saving over a file (`Storage::replace`) keeps its permissions and leaves
/// no temporary file behind; a file that did not exist is simply made.
/// `base` must not exist yet.
async fn replace_whole(storage: &dyn Storage, base: &RemotePath) {
    let local = std::env::temp_dir().join(format!("s3v-replace-{}", stamp()));
    std::fs::create_dir_all(&local).unwrap();
    storage.create_folder(base).await.expect("base folder");
    let file = base.child("run.sh");
    put(storage, &local.join("a"), &file, b"echo old\n").await;
    storage.set_permissions(&file, 0o750).await.expect("chmod");
    std::fs::write(local.join("b"), b"echo new\n").unwrap();
    storage.replace(&local.join("b"), &file).await.expect("replace");
    assert_eq!(fetch(storage, &local.join("c"), &file).await, b"echo new\n");
    let entries = list_all(storage, base).await.expect("listing");
    let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(names, ["run.sh"], "nothing left behind");
    assert_eq!(entries[0].permissions.map(|mode| mode & 0o777), Some(0o750), "permissions kept");

    let fresh = base.child("new.txt");
    storage.replace(&local.join("b"), &fresh).await.expect("a new file");
    assert_eq!(fetch(storage, &local.join("d"), &fresh).await, b"echo new\n");
    storage.delete(base, true).await.expect("cleanup");
    let _ = std::fs::remove_dir_all(&local);
}

#[tokio::test]
async fn sftp_saves_replace_files_whole() {
    let Some(storage) = sftp() else {
        return;
    };
    let home = storage.home().await.expect("home");
    replace_whole(&storage, &home.child(&format!("replace-test-{}", stamp()))).await;

    // Saved through a link, the file it points to changes and the link
    // stays a link.
    let base = home.child(&format!("replace-link-{}", stamp()));
    let local = std::env::temp_dir().join(format!("s3v-replace-link-{}", stamp()));
    std::fs::create_dir_all(&local).unwrap();
    storage.create_folder(&base).await.expect("base folder");
    let real = base.child("real.conf");
    put(&storage, &local.join("a"), &real, b"old\n").await;
    let link = base.child("link.conf");
    storage.make_link(&link, "real.conf").await.expect("symlink");
    std::fs::write(local.join("b"), b"new\n").unwrap();
    storage.replace(&local.join("b"), &link).await.expect("replace through the link");
    assert!(storage.is_link(&link).await.expect("lstat"), "still a link");
    assert_eq!(fetch(&storage, &local.join("c"), &real).await, b"new\n");
    storage.delete(&base, true).await.expect("cleanup");
    let _ = std::fs::remove_dir_all(&local);
}

#[tokio::test]
async fn ftp_saves_replace_files_whole() {
    let Some(storage) = ftp() else {
        return;
    };
    let home = storage.home().await.expect("home");
    replace_whole(&storage, &home.child(&format!("replace-test-{}", stamp()))).await;
}

pub(crate) fn sftp() -> Option<SftpStorage> {
    let (user, password, host, port) = target("S3V_TEST_SFTP")?;
    Some(SftpStorage::new(Arc::new(SshSettings {
        extra: Default::default(),
        host,
        port,
        user,
        secret: password,
        key_path: None,
        known_hosts: None,
    })))
}

#[tokio::test]
async fn sftp_file_operations() {
    let Some(storage) = sftp() else {
        return;
    };
    let home = storage.home().await.expect("home");
    let root = home.child("file-ops-test");
    let _ = storage.create_folder(&root).await;
    exercise(&storage, &root.child(&stamp())).await;
}

pub(crate) fn ftp() -> Option<FtpStorage> {
    let (user, password, host, port) = target("S3V_TEST_FTP")?;
    Some(FtpStorage::new(FtpSettings {
        host,
        port,
        user,
        password,
        tls: false,
    }))
}

#[tokio::test]
async fn ftp_file_operations() {
    let Some((user, password, host, port)) = target("S3V_TEST_FTP") else {
        return;
    };
    let storage = FtpStorage::new(FtpSettings {
        host,
        port,
        user,
        password,
        tls: false,
    });
    let home = storage.home().await.expect("home");
    let root = home.child("file-ops-test");
    let _ = storage.create_folder(&root).await;
    exercise(&storage, &root.child(&stamp())).await;
}
