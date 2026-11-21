//! Rename, copy and permissions over FTP.
//!
//! FTP cannot copy on the server: a file is read through one connection and
//! written through another at the same time, so nothing is stored here.
//! Permissions go through `SITE CHMOD`, which most Unix servers know.

use std::sync::atomic::Ordering;

use anyhow::{Context, Result, bail};
use suppaftp::Status;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::{FtpStorage, MAX_TRANSFERS};
use crate::storage::ops::{parse_mode_text, parse_octal, walk_tree};
use crate::storage::{Entry, Progress, RemotePath};

const CHUNK: usize = 256 * 1024;

impl FtpStorage {
    pub(super) async fn rename_entry(&self, from: &RemotePath, to: &RemotePath) -> Result<()> {
        let mut lease = self.lease().await?;
        let renamed = lease.ftp().rename(from.to_string(), to.to_string()).await;
        lease.settle(renamed)
    }

    pub(super) async fn copy_entry(&self, from: &RemotePath, to: &RemotePath, is_dir: bool, progress: &Progress) -> Result<()> {
        if !is_dir {
            let mut lease = self.lease().await?;
            let size = lease.ftp().size(from.to_string()).await;
            let size = lease.settle(size).unwrap_or(0) as u64;
            progress.total.fetch_add(size, Ordering::Relaxed);
            return self.copy_file(&from.to_string(), &to.to_string(), progress).await;
        }

        let tree = walk_tree(self, from, progress).await?;
        let total: u64 = tree.files.iter().map(|(_, size)| size).sum();
        progress.total.fetch_add(total, Ordering::Relaxed);
        let mut lease = self.lease().await?;
        for folder in std::iter::once(to.clone()).chain(tree.folders.iter().map(|folder| to.child(folder))) {
            if let Err(error) = lease.ftp().mkdir(folder.to_string()).await {
                return lease
                    .settle::<()>(Err(error))
                    .with_context(|| format!("impossible de créer {folder}"));
            }
        }
        lease.release();
        for (file, _) in &tree.files {
            self.copy_file(&from.child(file).to_string(), &to.child(file).to_string(), progress)
                .await?;
        }
        Ok(())
    }

    /// Reads `from` and writes `to` at the same time, through two
    /// connections. A copy that fails midway is removed.
    async fn copy_file(&self, from: &str, to: &str, progress: &Progress) -> Result<()> {
        // Two connections at once count as two transfers: one connection
        // stays free for browsing.
        let _turns = self.transfers.acquire_many(MAX_TRANSFERS as u32).await?;
        let mut source = self.lease().await?;
        let mut reader = match source.ftp().retr_as_stream(from).await {
            Ok(stream) => stream,
            Err(error) => return source.settle(Err(error)),
        };
        let mut target = self.lease().await?;
        let mut writer = match target.ftp().put_with_stream(to).await {
            Ok(stream) => stream,
            Err(error) => {
                // The server is still sending: that connection goes.
                drop(reader);
                drop(source);
                return target.settle(Err(error));
            }
        };

        match pipe(&mut reader, &mut writer, progress).await {
            Ok(()) => {
                let written = writer.finish().await;
                let read = reader.finish().await;
                target.settle(written)?;
                source.settle(read)
            }
            Err(error) => {
                drop((writer, reader, target, source));
                if let Ok(mut cleanup) = self.lease().await {
                    let removed = cleanup.ftp().rm(to).await;
                    let _ = cleanup.settle(removed);
                }
                Err(error)
            }
        }
    }

    pub(super) async fn chmod(&self, path: &RemotePath, mode: u32) -> Result<()> {
        let mut lease = self.lease().await?;
        let command = format!("SITE CHMOD {:o} {path}", mode & 0o7777);
        // Servers answer 200, some 250.
        let changed = lease
            .ftp()
            .custom_command(command, &[Status::CommandOk, Status::RequestedFileActionOk])
            .await
            .map(|_| ());
        lease.settle(changed)
    }
}

async fn pipe<R, W>(reader: &mut R, writer: &mut W, progress: &Progress) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buffer = vec![0u8; CHUNK];
    loop {
        if progress.is_cancelled() {
            bail!("annulé");
        }
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        writer.write_all(&buffer[..read]).await?;
        progress.add(read as u64);
    }
    writer.flush().await?;
    Ok(())
}

/// Permissions, owner and group from a listing line, which the parser of
/// the FTP library leaves out: `-rw-r--r-- 1 dev ftp 42 Nov 5 13:46 a.txt`,
/// or the facts of a machine listing (`UNIX.mode=0644;UNIX.owner=dev;`).
pub(super) fn listing_details(line: &str, mlsd: bool) -> Entry {
    let mut entry = Entry::default();
    if mlsd {
        let facts = if line.starts_with(' ') {
            ""
        } else {
            line.split_once("; ").map_or(line, |(facts, _)| facts)
        };
        for fact in facts.split(';') {
            let Some((key, value)) = fact.split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim().to_ascii_lowercase().as_str() {
                "unix.mode" => entry.permissions = parse_octal(value),
                "unix.owner" | "unix.ownername" => entry.owner = Some(value.to_owned()),
                "unix.group" | "unix.groupname" => entry.group = Some(value.to_owned()),
                // Ids only stand in for names.
                "unix.uid" if entry.owner.is_none() => entry.owner = Some(value.to_owned()),
                "unix.gid" if entry.group.is_none() => entry.group = Some(value.to_owned()),
                _ => {}
            }
        }
        return entry;
    }
    let mut fields = line.split_whitespace();
    if let Some(permissions) = fields.next().and_then(parse_mode_text) {
        entry.permissions = Some(permissions);
        let _links = fields.next();
        entry.owner = fields.next().map(str::to_owned);
        entry.group = fields.next().map(str::to_owned);
    }
    entry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listings_tell_permissions_and_owners() {
        let posix = listing_details("drwxr-x--- 2 dev www-data 4096 Nov 05 13:46 site web", false);
        assert_eq!(posix.permissions, Some(0o750));
        assert_eq!((posix.owner.as_deref(), posix.group.as_deref()), (Some("dev"), Some("www-data")));

        let setgid = listing_details("-rwxr-sr-x 1 0 0 42 Jan 01 2024 tool", false);
        assert_eq!(setgid.permissions, Some(0o2755));
        assert_eq!(setgid.owner.as_deref(), Some("0"));

        let dos = listing_details("10-19-20  03:19PM <DIR> pub", false);
        assert_eq!((dos.permissions, dos.owner), (None, None));

        let mlsd = listing_details("type=file;size=42;UNIX.mode=0640;UNIX.uid=1000;UNIX.owner=dev;UNIX.gid=100; a b;c.txt", true);
        assert_eq!(mlsd.permissions, Some(0o640));
        assert_eq!((mlsd.owner.as_deref(), mlsd.group.as_deref()), (Some("dev"), Some("100")));

        let bare = listing_details(" name-only", true);
        assert_eq!(bare.permissions, None);
    }
}
