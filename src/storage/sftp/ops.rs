//! Rename, copy, permissions, owner and properties over SFTP.
//!
//! SFTP itself cannot copy: the server's shell is asked to (`cp`), so that
//! nothing goes through the network. Accounts limited to SFTP have no shell;
//! the data then goes through here, read on one side and written back.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use russh::{ChannelMsg, Sig};
use russh_sftp::client::SftpSession;
use russh_sftp::client::fs::Metadata;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{Live, SftpStorage};
use crate::storage::ops::{Accounts, Properties, walk_tree};
use crate::storage::{Entry, Progress, RemotePath};
use crate::tabs::shell_quote;

/// Printed by the copy command once `cp` succeeded. A server limited to
/// SFTP accepts any command and runs its SFTP service instead, silently:
/// without this, that would look like success.
const COPIED: &str = "s3v-copied";
const CHUNK: usize = 256 * 1024;

/// How the server's shell took the copy.
enum Copied {
    Done,
    /// No shell, or no `cp`: the copy has to go through here.
    Unavailable,
}

impl SftpStorage {
    /// The fields of a listed entry beyond name, size and date. Names are
    /// only put on ids once the server's were read (see [`Self::load_accounts`]):
    /// listing never waits for them.
    pub(super) fn details(&self, meta: &Metadata, link: Option<String>) -> Entry {
        let accounts = self.accounts.get();
        let name = |id: Option<u32>, pick: fn(&Accounts, u32) -> Option<&str>| {
            id.map(|id| {
                accounts
                    .and_then(|accounts| pick(accounts, id))
                    .map_or_else(|| id.to_string(), str::to_owned)
            })
        };
        Entry {
            permissions: meta.permissions.map(|mode| mode & 0o7777),
            owner: name(meta.uid, Accounts::user),
            group: name(meta.gid, Accounts::group),
            link,
            ..Entry::default()
        }
    }

    /// Where `path` points, when `meta` says it is a link.
    pub(super) async fn link_target(&self, live: &Live, meta: &Metadata, path: &str) -> Option<String> {
        if !meta.is_symlink() {
            return None;
        }
        live.sftp.read_link(path).await.ok()
    }

    /// The server's users and groups, read once from `/etc/passwd` and
    /// `/etc/group`. A server that keeps them out of reach (chroot) leaves
    /// the ids without names.
    pub(super) async fn load_accounts(&self) -> Result<Arc<Accounts>> {
        let live = self.session().await?;
        let accounts = self
            .accounts
            .get_or_init(|| async {
                let (passwd, group) = tokio::join!(read_text(&live.sftp, "/etc/passwd"), read_text(&live.sftp, "/etc/group"));
                Arc::new(Accounts::parse(&passwd, &group))
            })
            .await;
        Ok(accounts.clone())
    }

    pub(super) async fn rename_entry(&self, from: &RemotePath, to: &RemotePath) -> Result<()> {
        let live = self.session().await?;
        live.sftp.rename(from.to_string(), to.to_string()).await?;
        Ok(())
    }

    pub(super) async fn copy_entry(&self, from: &RemotePath, to: &RemotePath, is_dir: bool, progress: &Progress) -> Result<()> {
        let live = self.session().await?;
        match self.copy_on_server(&live, from, to, progress).await? {
            Copied::Done => Ok(()),
            Copied::Unavailable => self.copy_through(&live, from, to, is_dir, progress).await,
        }
    }

    /// Runs `cp` on the server. Fails with its own words when it ran and
    /// refused, e.g. for lack of rights.
    async fn copy_on_server(&self, live: &Live, from: &RemotePath, to: &RemotePath, progress: &Progress) -> Result<Copied> {
        let Ok(mut channel) = live.handle.channel_open_session().await else {
            return Ok(Copied::Unavailable);
        };
        let command = format!(
            "cp -Rp -- {} {} && echo {COPIED}",
            shell_quote(&from.to_string()),
            shell_quote(&to.to_string())
        );
        if channel.exec(true, command).await.is_err() {
            return Ok(Copied::Unavailable);
        }
        // Nothing to read on its input: an SFTP service started in place of
        // the command then stops at once instead of waiting.
        let _ = channel.eof().await;

        let (mut out, mut err, mut status) = (Vec::new(), Vec::new(), None);
        loop {
            let message = tokio::select! {
                message = channel.wait() => message,
                () = cancelled(progress) => {
                    let _ = channel.signal(Sig::TERM).await;
                    let _ = channel.close().await;
                    bail!(tr!("annulé"));
                }
            };
            match message {
                None | Some(ChannelMsg::Close) => break,
                // The server refused to run a command at all.
                Some(ChannelMsg::Failure) => return Ok(Copied::Unavailable),
                Some(ChannelMsg::Data { data }) => out.extend_from_slice(&data),
                Some(ChannelMsg::ExtendedData { data, .. }) => err.extend_from_slice(&data),
                Some(ChannelMsg::ExitStatus { exit_status }) => status = Some(exit_status),
                Some(_) => {}
            }
        }
        if String::from_utf8_lossy(&out).contains(COPIED) {
            return Ok(Copied::Done);
        }
        let error = String::from_utf8_lossy(&err);
        let error = error.lines().map(str::trim).filter(|line| !line.is_empty()).collect::<Vec<_>>().join(" ; ");
        match status {
            // 126 and 127: the shell found no `cp` to run.
            Some(code) if code != 0 && code != 126 && code != 127 && !error.is_empty() => bail!("{error}"),
            _ => Ok(Copied::Unavailable),
        }
    }

    /// Copies by reading the files and writing them back, folders recreated
    /// on the way. Linked folders are left out, linked files are copied as
    /// regular files.
    async fn copy_through(&self, live: &Live, from: &RemotePath, to: &RemotePath, is_dir: bool, progress: &Progress) -> Result<()> {
        if !is_dir {
            let size = live.sftp.metadata(from.to_string()).await.ok().and_then(|meta| meta.size);
            progress.total.fetch_add(size.unwrap_or(0), Ordering::Relaxed);
            return copy_file(live, &from.to_string(), &to.to_string(), progress).await;
        }
        let tree = walk_tree(self, from, progress).await?;
        let total: u64 = tree.files.iter().map(|(_, size)| size).sum();
        progress.total.fetch_add(total, Ordering::Relaxed);
        for folder in std::iter::once(to.clone()).chain(tree.folders.iter().map(|folder| to.child(folder))) {
            live.sftp
                .create_dir(folder.to_string())
                .await
                .with_context(|| tr!("impossible de créer {path}", path = folder))?;
        }
        for (file, _) in &tree.files {
            copy_file(live, &from.child(file).to_string(), &to.child(file).to_string(), progress).await?;
        }
        Ok(())
    }

    pub(super) async fn chmod(&self, path: &RemotePath, mode: u32) -> Result<()> {
        let live = self.session().await?;
        set_mode(&live, &path.to_string(), mode).await
    }

    pub(super) async fn chown(&self, path: &RemotePath, uid: Option<u32>, gid: Option<u32>) -> Result<()> {
        let live = self.session().await?;
        let path = path.to_string();
        // SFTP sets both at once: the one that stays is read first.
        let (uid, gid) = match (uid, gid) {
            (Some(uid), Some(gid)) => (uid, gid),
            _ => {
                let current = live.sftp.metadata(path.as_str()).await?;
                (
                    uid.or(current.uid).context(tr!("le propriétaire actuel est inconnu"))?,
                    gid.or(current.gid).context(tr!("le groupe actuel est inconnu"))?,
                )
            }
        };
        let attributes = Metadata {
            uid: Some(uid),
            gid: Some(gid),
            ..Metadata::default()
        };
        live.sftp.set_metadata(path, attributes).await?;
        Ok(())
    }

    /// A link is described by what it points to, as in the listing, with
    /// where it points.
    pub(super) async fn stat_entry(&self, path: &RemotePath) -> Result<Properties> {
        let live = self.session().await?;
        let accounts = self.load_accounts().await.ok();
        let text = path.to_string();
        let own = live.sftp.symlink_metadata(text.as_str()).await?;
        let link = self.link_target(&live, &own, &text).await;
        let meta = match link {
            Some(_) => live.sftp.metadata(text.as_str()).await.unwrap_or(own),
            None => own,
        };
        let named = |id: Option<u32>, pick: fn(&Accounts, u32) -> Option<&str>| {
            id.map(|id| {
                accounts
                    .as_deref()
                    .and_then(|accounts| pick(accounts, id))
                    .map_or_else(|| id.to_string(), str::to_owned)
            })
        };
        Ok(Properties {
            size: if meta.is_dir() { None } else { meta.size },
            modified: meta.mtime.map(i64::from),
            permissions: meta.permissions.map(|mode| mode & 0o7777),
            uid: meta.uid,
            gid: meta.gid,
            owner: named(meta.uid, Accounts::user),
            group: named(meta.gid, Accounts::group),
            link,
            ..Properties::default()
        })
    }
}

async fn read_text(sftp: &SftpSession, path: &str) -> String {
    match sftp.read(path).await {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(_) => String::new(),
    }
}

async fn set_mode(live: &Live, path: &str, mode: u32) -> Result<()> {
    let attributes = Metadata {
        permissions: Some(mode & 0o7777),
        ..Metadata::default()
    };
    live.sftp.set_metadata(path, attributes).await?;
    Ok(())
}

/// One file, read and written back, with the permissions of the original.
/// A copy that fails midway is removed.
async fn copy_file(live: &Live, from: &str, to: &str, progress: &Progress) -> Result<()> {
    let mut source = live.sftp.open(from).await.with_context(|| tr!("impossible de lire {path}", path = from))?;
    let mode = source.metadata().await.ok().and_then(|meta| meta.permissions);
    let mut target = live.sftp.create(to).await.with_context(|| tr!("impossible de créer {path}", path = to))?;
    let copied = async {
        let mut buffer = vec![0u8; CHUNK];
        loop {
            if progress.is_cancelled() {
                bail!(tr!("annulé"));
            }
            let read = source.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            target.write_all(&buffer[..read]).await?;
            progress.add(read as u64);
        }
        target.shutdown().await?;
        Ok(())
    }
    .await;
    if copied.is_err() {
        drop(target);
        let _ = live.sftp.remove_file(to).await;
        return copied;
    }
    if let Some(mode) = mode {
        let _ = set_mode(live, to, mode).await;
    }
    Ok(())
}

/// Resolves once the user cancelled.
async fn cancelled(progress: &Progress) {
    while !progress.is_cancelled() {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Storage;
    use crate::storage::ops_live_tests::{sftp, stamp};

    /// The copy that servers without a shell get, which the test server
    /// would otherwise never take.
    #[tokio::test]
    async fn live_copy_through_the_client() {
        let Some(storage) = sftp() else {
            return;
        };
        let home = storage.home().await.unwrap();
        let base = home.child("file-ops-test").child(&format!("through-{}", stamp()));
        let local = std::env::temp_dir().join(format!("s3v-through-{}", stamp()));
        std::fs::write(&local, b"through the client").unwrap();
        storage.upload(&local, &base.child("src/deep/x.txt"), &Progress::default()).await.unwrap();
        storage.set_permissions(&base.child("src/deep/x.txt"), 0o600).await.unwrap();

        let live = storage.session().await.unwrap();
        let progress = Progress::default();
        storage
            .copy_through(&live, &base.child("src"), &base.child("dst"), true, &progress)
            .await
            .expect("copy through the client");
        assert_eq!(progress.snapshot(), (18, 18));
        let copied = storage.properties(&base.child("dst/deep/x.txt"), false).await.unwrap();
        assert_eq!((copied.size, copied.permissions), (Some(18), Some(0o600)));

        // Cancelled: nothing is left half written.
        let cancelled = Progress::default();
        cancelled.cancel();
        let single = storage
            .copy_through(&live, &base.child("src/deep/x.txt"), &base.child("y.txt"), false, &cancelled)
            .await;
        assert!(single.is_err());
        assert!(!live.sftp.try_exists(base.child("y.txt").to_string()).await.unwrap());

        storage.delete(&base, true).await.unwrap();
        let _ = std::fs::remove_file(local);
    }
}
