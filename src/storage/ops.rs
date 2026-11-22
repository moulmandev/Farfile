//! What the file operations (rename, copy, permissions, properties, folder
//! sizes) need from a connection beyond browsing and transfers: the types the
//! [`Storage`] methods exchange, and the walks every backend can share.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::{Result, bail};
use futures_util::{StreamExt, TryStreamExt, stream};

use super::{Entry, Progress, RemotePath, Storage};

/// Folders listed at the same time while walking a tree.
const WALK_CONCURRENCY: usize = 6;

/// What a connection can do to its files, so that the menus only offer that.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Operations {
    pub rename: bool,
    /// Copies on the server itself: the data does not come down here first.
    pub copy: bool,
    pub permissions: bool,
    pub owner: bool,
    /// Listings tell the permissions and the owner of each entry.
    pub details: bool,
}

/// The users and groups of a server, to put names on numeric ids.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Accounts {
    pub users: BTreeMap<u32, String>,
    pub groups: BTreeMap<u32, String>,
}

impl Accounts {
    /// Reads the formats of `/etc/passwd` and `/etc/group`: `name:x:id:…`.
    /// When two names share an id, the first one is kept, as `ls` does.
    pub fn parse(passwd: &str, group: &str) -> Self {
        let ids = |text: &str| {
            let mut ids = BTreeMap::new();
            for line in text.lines().filter(|line| !line.trim_start().starts_with('#')) {
                let mut fields = line.split(':');
                let (Some(name), Some(id)) = (fields.next(), fields.nth(1)) else {
                    continue;
                };
                if let Ok(id) = id.trim().parse::<u32>()
                    && !name.trim().is_empty()
                {
                    ids.entry(id).or_insert_with(|| name.trim().to_owned());
                }
            }
            ids
        };
        Self {
            users: ids(passwd),
            groups: ids(group),
        }
    }

    pub fn user(&self, id: u32) -> Option<&str> {
        self.users.get(&id).map(String::as_str)
    }

    pub fn group(&self, id: u32) -> Option<&str> {
        self.groups.get(&id).map(String::as_str)
    }

    /// The id of a user typed by name or as a number.
    pub fn user_id(&self, text: &str) -> Option<u32> {
        find_id(&self.users, text)
    }

    pub fn group_id(&self, text: &str) -> Option<u32> {
        find_id(&self.groups, text)
    }
}

fn find_id(names: &BTreeMap<u32, String>, text: &str) -> Option<u32> {
    let text = text.trim();
    text.parse().ok().or_else(|| {
        names
            .iter()
            .find(|(_, name)| name.as_str() == text)
            .map(|(id, _)| *id)
    })
}

/// What the server says about one file or folder, beyond its listing.
/// Anything unknown stays `None`: the listing then tells what it can.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Properties {
    pub size: Option<u64>,
    /// Unix timestamp, in seconds.
    pub modified: Option<i64>,
    /// The permission bits, special ones included (`0o2755`).
    pub permissions: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    /// Names of the owner and the group, or their ids when unknown.
    pub owner: Option<String>,
    pub group: Option<String>,
    /// Where a symbolic link points.
    pub link: Option<String>,
    /// Other facts, label and value, in the order they are shown (S3:
    /// content type, storage class, ETag, encryption…).
    pub details: Vec<(String, String)>,
    /// Metadata set by whoever stored the object (S3 `x-amz-meta-*`).
    pub metadata: Vec<(String, String)>,
}

/// What a folder holds, at any depth.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FolderSize {
    pub files: u64,
    pub folders: u64,
    pub bytes: u64,
}

/// A folder size being counted, shared between the worker and the UI.
#[derive(Default)]
pub struct Tally {
    files: AtomicU64,
    folders: AtomicU64,
    bytes: AtomicU64,
    cancel: AtomicBool,
}

impl Tally {
    pub fn add_file(&self, bytes: u64) {
        self.files.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn add_folder(&self) {
        self.folders.fetch_add(1, Ordering::Relaxed);
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    pub fn snapshot(&self) -> FolderSize {
        FolderSize {
            files: self.files.load(Ordering::Relaxed),
            folders: self.folders.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
        }
    }
}

/// What a folder holds, at any depth, as paths relative to it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Tree {
    /// Each folder comes before the ones it holds.
    pub folders: Vec<String>,
    /// With their size.
    pub files: Vec<(String, u64)>,
}

/// Every entry of a folder, all pages of it.
pub async fn list_all<S: Storage + ?Sized>(storage: &S, path: &RemotePath) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    let mut cursor = None;
    loop {
        let page = storage.list_page(path, cursor).await?;
        entries.extend(page.entries);
        match page.next {
            Some(next) => cursor = Some(next),
            None => return Ok(entries),
        }
    }
}

/// Visits everything below `root`, a level at a time, with several folders
/// listed at once. Linked folders are visited but not entered: they could
/// loop, or lead far away.
async fn walk<S, F>(storage: &S, root: &RemotePath, cancelled: &(dyn Fn() -> bool + Sync), mut visit: F) -> Result<()>
where
    S: Storage + ?Sized,
    F: FnMut(&str, &Entry) + Send,
{
    let mut level = vec![String::new()];
    while !level.is_empty() {
        if cancelled() {
            bail!(tr!("annulé"));
        }
        let listed: Vec<(String, Vec<Entry>)> = stream::iter(std::mem::take(&mut level))
            .map(|relative| async move {
                let entries = list_all(storage, &root.child(&relative)).await?;
                Ok::<_, anyhow::Error>((relative, entries))
            })
            .buffer_unordered(WALK_CONCURRENCY)
            .try_collect()
            .await?;
        for (relative, entries) in listed {
            for entry in entries {
                let child = if relative.is_empty() {
                    entry.name.clone()
                } else {
                    format!("{relative}/{}", entry.name)
                };
                visit(&child, &entry);
                if entry.is_dir() && entry.link.is_none() {
                    level.push(child);
                }
            }
        }
    }
    Ok(())
}

/// Counts what `path` holds through its listings: what a backend without a
/// faster way does for [`Storage::folder_size`].
pub async fn walk_size<S: Storage + ?Sized>(storage: &S, path: &RemotePath, tally: &Tally) -> Result<()> {
    walk(storage, path, &|| tally.is_cancelled(), |_, entry| {
        if entry.is_dir() {
            tally.add_folder();
        } else {
            tally.add_file(entry.size.unwrap_or(0));
        }
    })
    .await
}

/// Lists everything below `path`, to copy it or change it entry by entry.
/// Linked folders are left out, linked files are kept.
pub async fn walk_tree<S: Storage + ?Sized>(storage: &S, path: &RemotePath, progress: &Progress) -> Result<Tree> {
    let mut tree = Tree::default();
    walk(storage, path, &|| progress.is_cancelled(), |relative, entry| {
        if !entry.is_dir() {
            tree.files.push((relative.to_owned(), entry.size.unwrap_or(0)));
        } else if entry.link.is_none() {
            tree.folders.push(relative.to_owned());
        }
    })
    .await?;
    Ok(tree)
}

/// `0o755` → `rwxr-xr-x`, with `s`, `S`, `t` and `T` for the special bits,
/// as `ls -l` writes them.
pub fn mode_text(mode: u32) -> String {
    const SPECIAL: [(u32, char, char); 3] = [(0o4000, 's', 'S'), (0o2000, 's', 'S'), (0o1000, 't', 'T')];
    let mut text = String::with_capacity(9);
    for (who, (special, with_exec, without_exec)) in SPECIAL.into_iter().enumerate() {
        let bits = (mode >> (6 - 3 * who)) & 0o7;
        text.push(if bits & 0o4 != 0 { 'r' } else { '-' });
        text.push(if bits & 0o2 != 0 { 'w' } else { '-' });
        text.push(match (mode & special != 0, bits & 0o1 != 0) {
            (true, true) => with_exec,
            (true, false) => without_exec,
            (false, true) => 'x',
            (false, false) => '-',
        });
    }
    text
}

/// Reads `rwxr-xr-x`, or `drwxr-xr-x` as a listing starts it, possibly
/// followed by the `+` or `.` some systems add for ACLs.
pub fn parse_mode_text(text: &str) -> Option<u32> {
    let chars: Vec<char> = text.trim_end_matches(['+', '.', '@']).chars().collect();
    let bits = match chars.len() {
        9 => &chars[..],
        10 => &chars[1..],
        _ => return None,
    };
    let mut mode = 0;
    for (index, c) in bits.iter().enumerate() {
        let (who, what) = (index / 3, index % 3);
        let bit = 1 << (8 - index);
        let special = [0o4000, 0o2000, 0o1000][who];
        let sticky = who == 2;
        match (what, c) {
            (_, '-') => {}
            (0, 'r') | (1, 'w') | (2, 'x') => mode |= bit,
            (2, 's') if !sticky => mode |= bit | special,
            (2, 'S') if !sticky => mode |= special,
            (2, 't') if sticky => mode |= bit | special,
            (2, 'T') if sticky => mode |= special,
            _ => return None,
        }
    }
    Some(mode)
}

/// Reads a mode typed in octal: `755`, `0755`, `2755`.
pub fn parse_octal(text: &str) -> Option<u32> {
    let text = text.trim();
    if text.is_empty() || text.len() > 4 || !text.chars().all(|c| ('0'..='7').contains(&c)) {
        return None;
    }
    u32::from_str_radix(text, 8).ok()
}

/// `0o755` → `755`, `0o2755` → `2755`.
pub fn octal_text(mode: u32) -> String {
    format!("{:03o}", mode & 0o7777)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_read_and_write_like_ls() {
        assert_eq!(mode_text(0o755), "rwxr-xr-x");
        assert_eq!(mode_text(0o640), "rw-r-----");
        assert_eq!(mode_text(0o4755), "rwsr-xr-x");
        assert_eq!(mode_text(0o2745), "rwxr-Sr-x");
        assert_eq!(mode_text(0o1777), "rwxrwxrwt");
        assert_eq!(mode_text(0o1776), "rwxrwxrwT");
        assert_eq!(mode_text(0), "---------");
        for mode in [0o755, 0o640, 0o4755, 0o2745, 0o1777, 0o1776, 0, 0o7777, 0o7000] {
            assert_eq!(parse_mode_text(&mode_text(mode)), Some(mode), "{mode:o}");
        }
        assert_eq!(parse_mode_text("drwxr-xr-x"), Some(0o755));
        assert_eq!(parse_mode_text("-rw-r--r--+"), Some(0o644));
        assert_eq!(parse_mode_text("lrwxrwxrwx"), Some(0o777));
        assert_eq!(parse_mode_text("rwxr-xr-"), None);
        assert_eq!(parse_mode_text("rwxr-xr-q"), None);
        assert_eq!(parse_mode_text("rwtr-xr-x"), None, "sticky is for others only");
    }

    #[test]
    fn octal_is_typed_with_three_or_four_digits() {
        assert_eq!(parse_octal("755"), Some(0o755));
        assert_eq!(parse_octal(" 0644 "), Some(0o644));
        assert_eq!(parse_octal("2755"), Some(0o2755));
        assert_eq!(parse_octal("7"), Some(0o7));
        assert_eq!(parse_octal("788"), None);
        assert_eq!(parse_octal("12345"), None);
        assert_eq!(parse_octal(""), None);
        assert_eq!(parse_octal("-755"), None);
        assert_eq!(octal_text(0o755), "755");
        assert_eq!(octal_text(0o2755), "2755");
        assert_eq!(octal_text(0o40755), "755", "the file type is not part of it");
        assert_eq!(octal_text(0o7), "007");
    }

    #[test]
    fn accounts_put_names_on_ids() {
        let passwd = "root:x:0:0:root:/root:/bin/sh\n# comment\ntoor:x:0:0::/:/bin/sh\ndev:x:1000:1000::/config:/bin/bash\nbroken\n";
        let group = "root:x:0:\nusers:x:100:dev\ndev:x:1000:\n";
        let accounts = Accounts::parse(passwd, group);
        assert_eq!(accounts.user(0), Some("root"), "the first name of an id wins");
        assert_eq!(accounts.user(1000), Some("dev"));
        assert_eq!(accounts.group(100), Some("users"));
        assert_eq!(accounts.user(42), None);
        assert_eq!(accounts.user_id("dev"), Some(1000));
        assert_eq!(accounts.user_id(" 1234 "), Some(1234), "numbers are taken as they are");
        assert_eq!(accounts.group_id("users"), Some(100));
        assert_eq!(accounts.group_id("nobody"), None);
    }
}
