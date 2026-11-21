//! SFTP backend, on top of an SSH connection.

use std::io::SeekFrom;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use russh::ChannelMsg;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::{FileAttributes, OpenFlags};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::Mutex;
use tokio::sync::mpsc;

use super::ssh::{self, DEFAULT_PORT, SshHandle, SshSettings};
use super::{
    DeepFile, Entry, EntryKind, Found, FoundSink, ListPage, Progress, RemotePath, Storage, TreeProgress, TreeReport,
    append_stream, archive, below, local_len, save_stream, send_file, send_file_from,
};
use crate::tabs::shell_quote;

/// Rename, copy, permissions, owner and properties.
mod ops;

/// Seconds before a request without answer is given up.
const REQUEST_TIMEOUT: u64 = 30;
/// Folders read at the same time by a recursive listing.
const PARALLEL_LISTINGS: usize = 8;
/// How long the count of a folder's files may take on the server.
const COUNT_TIMEOUT: Duration = Duration::from_secs(60);

/// Counts the files of the folder `$2` of `$1` and adds up their sizes, on
/// the server: with `find -printf` where it exists (GNU), `ls -ln`
/// elsewhere. `find` gets `./$2`: a bare name starting with `-` would be
/// read as part of its expression, and `-delete` is one.
const COUNT_SCRIPT: &str = r#"cd -- "$1" || exit 3; if find . -maxdepth 0 -printf '' >/dev/null 2>&1; then find "./$2" -type f -printf '%s\n'; else find "./$2" -type f -exec ls -ln {} + | awk '{print $5}'; fi | awk '{n++; s+=$1} END {printf "%d %.0f\n", n, s}'"#;

/// Sends the folder `$2` of `$1` as one tar stream, compressed when the
/// server has gzip. Behind gzip, the exit status of tar is told on the
/// error output.
const TAR_SCRIPT: &str = r#"cd -- "$1" || exit 3; if command -v gzip >/dev/null 2>&1; then { tar -cf - -- "./$2"; echo "s3viewer-tar-exit:$?" >&2; } | gzip -1; else tar -cf - -- "./$2"; fi"#;
const TAR_EXIT: &str = "s3viewer-tar-exit:";

struct Live {
    handle: SshHandle,
    sftp: SftpSession,
}

pub struct SftpStorage {
    settings: Arc<SshSettings>,
    /// Opened on first use, and again whenever the server dropped it.
    live: Mutex<Option<Arc<Live>>>,
    /// The server's user and group names, read once asked for.
    accounts: tokio::sync::OnceCell<Arc<super::ops::Accounts>>,
}

impl SftpStorage {
    pub fn new(settings: Arc<SshSettings>) -> Self {
        Self {
            settings,
            live: Mutex::new(None),
            accounts: tokio::sync::OnceCell::new(),
        }
    }

    async fn session(&self) -> Result<Arc<Live>> {
        let mut live = self.live.lock().await;
        if let Some(current) = live.as_ref()
            && !current.handle.is_closed()
        {
            return Ok(current.clone());
        }

        let handle = ssh::connect(&self.settings).await?;
        let channel = handle.channel_open_session().await?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .context("ce serveur n'accepte pas SFTP")?;
        let sftp = SftpSession::new(channel.into_stream())
            .await
            .context("ouverture de la session SFTP impossible")?;
        sftp.set_timeout(REQUEST_TIMEOUT);

        let opened = Arc::new(Live { handle, sftp });
        *live = Some(opened.clone());
        Ok(opened)
    }

    /// Creates `dir` and any missing parent. Errors are ignored: a folder
    /// that exists already fails too, and the caller retries its own
    /// operation anyway.
    async fn create_parents(sftp: &SftpSession, dir: &RemotePath) {
        for depth in 1..=dir.segments().len() {
            let _ = sftp.create_dir(dir.ancestor(depth).to_string()).await;
        }
    }
}

/// Runs `script` with the POSIX shell, whatever the account's own shell
/// is, with `dir` as `$1` and `name` as `$2`. They are handed over as
/// arguments, never written into the script: whatever they hold stays a
/// name.
fn script(script: &str, dir: &str, name: &str) -> String {
    format!("sh -c {} sh {} {}", shell_quote(script), shell_quote(dir), shell_quote(name))
}

/// Runs `command` on the server for what it prints and its exit status.
/// `None` when the account may not run commands.
async fn run(handle: &ssh::SshHandle, command: String) -> Option<(Vec<u8>, Option<u32>)> {
    let mut channel = handle.channel_open_session().await.ok()?;
    channel.exec(true, command).await.ok()?;
    let mut output = Vec::new();
    let mut status = None;
    while let Some(message) = channel.wait().await {
        match message {
            ChannelMsg::Data { data } => output.extend_from_slice(&data),
            ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
            ChannelMsg::Failure => return None,
            ChannelMsg::Close => break,
            _ => {}
        }
    }
    Some((output, status))
}

/// Hands what a command prints to `sender`, and returns its error output
/// and exit status once it is over, or once nobody listens any more.
async fn pump(mut channel: russh::Channel<russh::client::Msg>, sender: mpsc::Sender<Vec<u8>>) -> (Vec<u8>, Option<u32>) {
    let mut errors = Vec::new();
    let mut status = None;
    while let Some(message) = channel.wait().await {
        match message {
            ChannelMsg::Data { data } => {
                if sender.send(data.to_vec()).await.is_err() {
                    break;
                }
            }
            ChannelMsg::ExtendedData { data, .. } => errors.extend_from_slice(&data),
            ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
            ChannelMsg::Failure | ChannelMsg::Close => break,
            _ => {}
        }
    }
    let _ = channel.close().await;
    (errors, status)
}

/// What tar said went wrong, as `path : why`. Folders it could not read
/// came along empty: they are not kept, as with a download file by file.
fn tar_complaints(errors: &str, dest: &Path) -> Vec<String> {
    let mut complaints = Vec::new();
    for line in errors.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let line = line.strip_prefix("tar: ").unwrap_or(line);
        if line.starts_with(TAR_EXIT)
            || line.starts_with("Exiting with failure status")
            || line.contains("error exit delayed")
            || line.contains("file changed as we read it")
            || line.contains("socket ignored")
        {
            continue;
        }
        // GNU tar: `blog/private: Cannot open: Permission denied`.
        match line.split_once(": ") {
            Some((name, why)) if name.contains('/') || !name.contains(' ') => {
                let local = archive::local_path(dest, name);
                if local != dest {
                    let _ = std::fs::remove_dir(&local);
                }
                complaints.push(format!("{} : {why}", archive::below(name)));
            }
            _ => complaints.push(line.to_owned()),
        }
    }
    complaints
}

fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// The content of `dir` for a recursive listing: the size of each file,
/// `None` for each folder. Linked files count as files; linked folders are
/// left out, since following them could loop.
#[cfg(test)]
impl SftpStorage {
    /// Makes `link` a symbolic link to `target`, for the tests. OpenSSH
    /// takes the two the other way round from the SFTP draft.
    pub(crate) async fn make_link(&self, link: &RemotePath, target: &str) -> Result<()> {
        let live = self.session().await?;
        live.sftp.symlink(target.to_owned(), link.to_string()).await?;
        Ok(())
    }

    /// Whether `path` itself is a symbolic link.
    pub(crate) async fn is_link(&self, path: &RemotePath) -> Result<bool> {
        let live = self.session().await?;
        Ok(live.sftp.symlink_metadata(path.to_string()).await?.file_type().is_symlink())
    }
}

/// Writes `src` over the file at `remote`, keeping whatever was written if
/// it fails halfway: unlike an upload, nothing is removed then, since what
/// was there is the user's file.
async fn write_in_place(live: &Live, src: &Path, remote: &str) -> Result<()> {
    let mut file = live.sftp.create(remote).await?;
    send_file(src, &mut file, &Progress::default())
        .await
        .context("l'enregistrement s'est arrêté en cours : le fichier sur le serveur est peut-être incomplet")?;
    file.shutdown().await?;
    Ok(())
}

async fn children(sftp: &SftpSession, dir: &str) -> Result<Vec<(String, Option<u64>)>> {
    let mut children = Vec::new();
    for item in sftp.read_dir(dir).await? {
        let name = item.file_name();
        let meta = item.metadata();
        if meta.is_dir() {
            children.push((name, None));
        } else if meta.is_symlink() {
            match sftp.metadata(join(dir, &name)).await {
                Ok(target) if !target.is_dir() => children.push((name, Some(target.size.unwrap_or(0)))),
                _ => {}
            }
        } else {
            children.push((name, Some(meta.size.unwrap_or(0))));
        }
    }
    Ok(children)
}

#[async_trait]
impl Storage for SftpStorage {
    async fn list_page(&self, path: &RemotePath, _cursor: Option<String>) -> Result<ListPage> {
        let live = self.session().await?;
        let dir = path.to_string();
        let mut entries = Vec::new();
        for item in live.sftp.read_dir(dir.as_str()).await? {
            let name = item.file_name();
            let mut meta = item.metadata();
            let link = self.link_target(&live, &meta, &join(&dir, &name)).await;
            // Links are shown as what they point to.
            if meta.is_symlink()
                && let Ok(target) = live.sftp.metadata(join(&dir, &name)).await
            {
                meta = target;
            }
            let is_dir = meta.is_dir();
            entries.push(Entry {
                name,
                kind: if is_dir { EntryKind::Folder } else { EntryKind::File },
                size: if is_dir { None } else { meta.size },
                modified: meta.mtime.map(i64::from),
                ..self.details(&meta, link)
            });
        }
        Ok(ListPage { entries, next: None })
    }

    async fn walk(&self, path: &RemotePath, found: FoundSink<'_>) -> Result<()> {
        let live = self.session().await?;
        let sftp = &live.sftp;
        // Each folder costs a few round trips: on a distant server, reading
        // them one after the other is what makes a big tree slow.
        let mut waiting = vec![(path.to_string(), String::new())];
        let mut reading = FuturesUnordered::new();
        loop {
            while reading.len() < PARALLEL_LISTINGS
                && let Some((dir, prefix)) = waiting.pop()
            {
                reading.push(async move {
                    let children = children(sftp, &dir).await;
                    (dir, prefix, children)
                });
            }
            let Some((dir, prefix, children)) = reading.next().await else {
                return Ok(());
            };
            let children = match children {
                Ok(children) => children,
                // The folder asked for, or every folder from now on.
                Err(error) if prefix.is_empty() || live.handle.is_closed() => return Err(error),
                Err(error) => {
                    let error = format!("{error:#}");
                    if !found(Found::Unreadable { relative: prefix, error }) {
                        return Ok(());
                    }
                    continue;
                }
            };
            for (name, size) in children {
                let relative = below(&prefix, &name);
                let keep_going = match size {
                    None => {
                        waiting.push((join(&dir, &name), relative.clone()));
                        found(Found::Folder { relative })
                    }
                    Some(size) => found(Found::File { relative, size }),
                };
                if !keep_going {
                    return Ok(());
                }
            }
        }
    }

    async fn download(&self, path: &RemotePath, dest: &Path, progress: &Progress) -> Result<()> {
        let live = self.session().await?;
        let mut file = live.sftp.open(path.to_string()).await?;
        if let Ok(meta) = file.metadata().await
            && let Some(size) = meta.size
        {
            progress.set_total(size);
        }
        save_stream(&mut file, dest, progress).await
    }

    fn can_download_tree(&self) -> bool {
        true
    }

    async fn download_tree(&self, path: &RemotePath, dest: &Path, progress: &TreeProgress) -> Result<Option<TreeReport>> {
        let Some(parent) = path.parent() else {
            return Ok(None);
        };
        let (dir, name) = (parent.to_string(), path.name());
        let live = self.session().await?;

        // What to expect, for the progress. No count, no commands here.
        let counted = tokio::time::timeout(COUNT_TIMEOUT, run(&live.handle, script(COUNT_SCRIPT, &dir, name))).await;
        let Ok(Some((output, Some(0)))) = counted else {
            return Ok(None);
        };
        let output = String::from_utf8_lossy(&output);
        let mut numbers = output.lines().last().unwrap_or("").split_whitespace().map(str::parse::<u64>);
        let (Some(Ok(files)), Some(Ok(bytes))) = (numbers.next(), numbers.next()) else {
            return Ok(None);
        };
        progress.files.store(files, Ordering::Relaxed);
        progress.bytes.set_total(bytes);

        let Ok(channel) = live.handle.channel_open_session().await else {
            return Ok(None);
        };
        if channel.exec(true, script(TAR_SCRIPT, &dir, name)).await.is_err() {
            return Ok(None);
        }
        let (sender, receiver) = mpsc::channel(64);
        let pumping = tokio::spawn(pump(channel, sender));
        let (mut report, unpacked) = tokio::task::block_in_place(|| {
            let mut incoming = archive::Incoming::new(receiver);
            let unpacked = archive::unpack(&mut incoming, dest, progress);
            // Past the end of the archive come the last words of the server.
            if unpacked.1.is_ok() {
                let _ = std::io::copy(&mut incoming, &mut std::io::sink());
            }
            unpacked
        });
        let (errors, status) = pumping.await.unwrap_or_default();

        // Nothing usable came: no tar, or a shell that talks first.
        if report.entries == 0 && !progress.bytes.is_cancelled() {
            return Ok(None);
        }
        unpacked?;
        let errors = String::from_utf8_lossy(&errors);
        let status = errors
            .lines()
            .find_map(|line| line.trim().strip_prefix(TAR_EXIT))
            .and_then(|code| code.trim().parse().ok())
            .or(status);
        report.unreadable = tar_complaints(&errors, dest);
        if report.unreadable.is_empty() && status.is_some_and(|code| code > 1) {
            report.unreadable.push(format!("tar s'est arrêté avec le code {}", status.unwrap_or_default()));
        }
        Ok(Some(report))
    }

    async fn upload(&self, src: &Path, dest: &RemotePath, progress: &Progress) -> Result<()> {
        let live = self.session().await?;
        let remote = dest.to_string();
        let mut file = match live.sftp.create(remote.as_str()).await {
            Ok(file) => file,
            Err(_) => {
                // Most likely the folder does not exist yet.
                if let Some(parent) = dest.parent() {
                    Self::create_parents(&live.sftp, &parent).await;
                }
                live.sftp.create(remote.as_str()).await?
            }
        };

        let sent = async {
            send_file(src, &mut file, progress).await?;
            file.shutdown().await?;
            Ok(())
        }
        .await;
        if sent.is_err() {
            // Don't leave half a file behind.
            drop(file);
            let _ = live.sftp.remove_file(remote).await;
        }
        sent
    }

    async fn delete(&self, path: &RemotePath, is_dir: bool) -> Result<u64> {
        let live = self.session().await?;
        if !is_dir {
            live.sftp.remove_file(path.to_string()).await?;
            return Ok(1);
        }

        let mut removed = 0;
        let mut folders = Vec::new();
        let mut pending = vec![path.to_string()];
        while let Some(dir) = pending.pop() {
            for item in live.sftp.read_dir(dir.as_str()).await? {
                let child = join(&dir, &item.file_name());
                // Links are removed, never followed.
                if item.metadata().is_dir() {
                    pending.push(child);
                } else {
                    live.sftp.remove_file(child).await?;
                    removed += 1;
                }
            }
            folders.push(dir);
        }
        // Parents were found before their children: empty them deepest first.
        for dir in folders.into_iter().rev() {
            live.sftp.remove_dir(dir).await?;
            removed += 1;
        }
        Ok(removed)
    }

    async fn create_folder(&self, path: &RemotePath) -> Result<()> {
        let live = self.session().await?;
        live.sftp.create_dir(path.to_string()).await?;
        Ok(())
    }

    fn plain_url(&self, path: &RemotePath) -> Option<String> {
        let settings = &self.settings;
        let port = if settings.port == DEFAULT_PORT {
            String::new()
        } else {
            format!(":{}", settings.port)
        };
        Some(format!("sftp://{}@{}{port}{path}", settings.user, settings.host))
    }

    async fn home(&self) -> Result<RemotePath> {
        let live = self.session().await?;
        let home = live.sftp.canonicalize(".").await?;
        Ok(RemotePath::parse(&home))
    }

    async fn read_range(&self, path: &RemotePath, offset: u64, len: u64) -> Result<super::Chunk> {
        use std::io::SeekFrom;
        use tokio::io::{AsyncReadExt, AsyncSeekExt};

        let live = self.session().await?;
        let mut file = live.sftp.open(path.to_string()).await?;
        let size = file.metadata().await?.size.unwrap_or(0);
        let wanted = len.min(size.saturating_sub(offset));
        let mut bytes = Vec::with_capacity(wanted as usize);
        if wanted > 0 {
            file.seek(SeekFrom::Start(offset)).await?;
            // What is appended meanwhile waits for the next read.
            (&mut file).take(wanted).read_to_end(&mut bytes).await?;
        }
        Ok(super::Chunk { bytes, size })
    }

    fn operations(&self) -> super::ops::Operations {
        super::ops::Operations {
            rename: true,
            copy: true,
            permissions: true,
            owner: true,
            details: true,
        }
    }

    async fn rename(&self, from: &RemotePath, to: &RemotePath, _is_dir: bool, _progress: &Progress) -> Result<()> {
        self.rename_entry(from, to).await
    }

    async fn copy(&self, from: &RemotePath, to: &RemotePath, is_dir: bool, progress: &Progress) -> Result<()> {
        self.copy_entry(from, to, is_dir, progress).await
    }

    async fn set_permissions(&self, path: &RemotePath, mode: u32) -> Result<()> {
        self.chmod(path, mode).await
    }

    async fn set_owner(&self, path: &RemotePath, uid: Option<u32>, gid: Option<u32>) -> Result<()> {
        self.chown(path, uid, gid).await
    }

    async fn accounts(&self) -> Result<super::ops::Accounts> {
        Ok(self.load_accounts().await?.as_ref().clone())
    }

    async fn properties(&self, path: &RemotePath, _is_dir: bool) -> Result<super::ops::Properties> {
        self.stat_entry(path).await
    }

    /// The new content goes to a hidden file next to the old one, which it
    /// then takes the place of, with its permissions and, when the server
    /// allows, its owner. A link stays a link: the file it points to is
    /// the one replaced. Where no file can be made next to it, the file is
    /// written in place, and left as it is if that fails.
    async fn replace(&self, src: &Path, dest: &RemotePath) -> Result<()> {
        let live = self.session().await?;
        let mut target = dest.to_string();
        match live.sftp.symlink_metadata(target.as_str()).await {
            Ok(own) if own.file_type().is_symlink() => target = live.sftp.canonicalize(target).await?,
            Ok(_) => {}
            // A new file: there is nothing to keep.
            Err(_) => return self.upload(src, dest, &Progress::default()).await,
        }
        let original = live.sftp.metadata(target.as_str()).await?;
        let real = RemotePath::parse(&target);
        let (Some(folder), true) = (real.parent(), original.file_type().is_file()) else {
            return write_in_place(&live, src, &target).await;
        };
        let tag = crate::profiles::Profile::new().id;
        let temp = folder.child(&format!(".{}.s3v-{tag}", real.name())).to_string();
        if self.upload(src, &RemotePath::parse(&temp), &Progress::default()).await.is_err() {
            return write_in_place(&live, src, &target).await;
        }
        let mode = FileAttributes {
            permissions: original.permissions.map(|mode| mode & 0o7777),
            ..FileAttributes::default()
        };
        let _ = live.sftp.set_metadata(temp.as_str(), mode).await;
        let owner = FileAttributes {
            uid: original.uid,
            gid: original.gid,
            ..FileAttributes::default()
        };
        let _ = live.sftp.set_metadata(temp.as_str(), owner).await;

        // Servers that follow POSIX replace the file at once; plain SFTP
        // refuses to rename over a file, so the old one steps aside first.
        if live.sftp.rename(temp.as_str(), target.as_str()).await.is_ok() {
            return Ok(());
        }
        let aside = folder.child(&format!(".{}.s3v-{tag}.old", real.name())).to_string();
        if let Err(error) = live.sftp.rename(target.as_str(), aside.as_str()).await {
            let _ = live.sftp.remove_file(temp).await;
            return Err(error).context("impossible de remplacer le fichier");
        }
        if let Err(error) = live.sftp.rename(temp.as_str(), target.as_str()).await {
            let _ = live.sftp.rename(aside, target).await;
            let _ = live.sftp.remove_file(temp).await;
            return Err(error).context("impossible de remplacer le fichier");
        }
        let _ = live.sftp.remove_file(aside).await;
        Ok(())
    }

    fn can_resume_download(&self) -> bool {
        true
    }

    async fn download_resumable(&self, path: &RemotePath, partial: &Path, resume: bool, progress: &Progress) -> Result<()> {
        let live = self.session().await?;
        let mut file = live.sftp.open(path.to_string()).await?;
        let size = file.metadata().await.ok().and_then(|meta| meta.size);
        if let Some(size) = size {
            progress.set_total(size);
        }
        let mut offset = if resume { local_len(partial).await } else { 0 };
        if size.is_some_and(|size| offset > size) {
            offset = 0;
        }
        if offset > 0 {
            file.seek(SeekFrom::Start(offset)).await?;
        }
        append_stream(&mut file, partial, offset, progress).await
    }

    fn can_resume_upload(&self) -> bool {
        true
    }

    async fn upload_resumable(&self, src: &Path, dest: &RemotePath, resume: bool, progress: &Progress) -> Result<()> {
        let live = self.session().await?;
        let remote = dest.to_string();
        let local = tokio::fs::metadata(src)
            .await
            .with_context(|| format!("impossible de lire {}", src.display()))?
            .len();
        progress.set_total(local);
        let mut offset = 0;
        if resume && let Ok(meta) = live.sftp.metadata(remote.as_str()).await {
            offset = meta.size.unwrap_or(0);
        }
        // Longer than the original: not the start of it.
        if offset > local {
            offset = 0;
        }
        let flags = if offset > 0 {
            OpenFlags::WRITE
        } else {
            OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE
        };
        let mut file = match live.sftp.open_with_flags(remote.as_str(), flags).await {
            Ok(file) => file,
            Err(_) => {
                if let Some(parent) = dest.parent() {
                    Self::create_parents(&live.sftp, &parent).await;
                }
                live.sftp.open_with_flags(remote.as_str(), flags).await?
            }
        };
        if offset > 0 {
            file.seek(SeekFrom::Start(offset)).await?;
        }
        progress.set_done(offset);
        send_file_from(src, offset, &mut file, progress).await?;
        file.shutdown().await?;
        Ok(())
    }

    /// Like [`Storage::walk`]: linked folders are not followed.
    async fn list_deep_dated(&self, path: &RemotePath) -> Result<Vec<DeepFile>> {
        let live = self.session().await?;
        let mut files = Vec::new();
        let mut pending = vec![(path.to_string(), String::new())];
        while let Some((dir, prefix)) = pending.pop() {
            for item in live.sftp.read_dir(dir.as_str()).await? {
                let name = item.file_name();
                let mut meta = item.metadata();
                let relative = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}/{name}")
                };
                if meta.is_dir() {
                    pending.push((join(&dir, &name), relative));
                    continue;
                }
                if meta.is_symlink() {
                    match live.sftp.metadata(join(&dir, &name)).await {
                        Ok(target) if !target.is_dir() => meta = target,
                        _ => continue,
                    }
                }
                files.push(DeepFile {
                    relative,
                    size: meta.size.unwrap_or(0),
                    modified: meta.mtime.map(i64::from),
                });
            }
        }
        Ok(files)
    }

    async fn set_mtime(&self, path: &RemotePath, modified: i64) -> Result<()> {
        let live = self.session().await?;
        let seconds = u32::try_from(modified.max(0)).unwrap_or(u32::MAX);
        // Only the dates: sending the other attributes would change them.
        let dates = FileAttributes {
            atime: Some(seconds),
            mtime: Some(seconds),
            ..FileAttributes::empty()
        };
        live.sftp.set_metadata(path.to_string(), dates).await?;
        Ok(())
    }

    fn keeps_dates(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Names come from the server: whatever they hold, the shell only ever
    /// sees them as quoted arguments, never as part of the script.
    #[test]
    fn names_are_arguments_never_script() {
        assert_eq!(
            script("run \"$1\" \"$2\"", "/srv/@NAME@", "-delete"),
            r#"sh -c 'run "$1" "$2"' sh '/srv/@NAME@' '-delete'"#
        );
        assert_eq!(script("x", "/a'b", "$(touch pwned)"), r#"sh -c 'x' sh '/a'\''b' '$(touch pwned)'"#);
        for template in [COUNT_SCRIPT, TAR_SCRIPT] {
            assert!(!template.contains("@NAME@") && !template.contains("@DIR@"));
            // find and tar are never handed a bare name, which could read
            // as one of their options.
            assert!(!template.contains(" \"$2\""), "{template}");
            assert!(template.contains("cd -- \"$1\""));
        }
        assert_eq!(COUNT_SCRIPT.matches("find \"./$2\"").count(), 2);
        assert_eq!(TAR_SCRIPT.matches("tar -cf - -- \"./$2\"").count(), 2);
    }

    #[test]
    fn tar_complaints_name_the_file_below_the_folder() {
        let dest = std::env::temp_dir().join(format!("s3viewer-complaints-{}", std::process::id()));
        let errors = "tar: ./-delete/private: Cannot open: Permission denied\n\
                      tar: Exiting with failure status due to previous errors\n\
                      s3viewer-tar-exit:2\n";
        assert_eq!(tar_complaints(errors, &dest), ["private : Cannot open: Permission denied"]);
    }
}
