//! Backend-agnostic storage layer.
//!
//! The UI only ever talks to the [`Storage`] trait, so adding another protocol
//! means writing one more implementation of it. So far: S3, SFTP and FTP.

pub mod archive;
pub mod ftp;
#[cfg(test)]
mod live_tests;
pub mod ops;
#[cfg(test)]
pub(crate) mod ops_live_tests;
pub mod local;
pub mod s3;
pub mod sftp;
pub mod ssh;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt};

/// A location on the remote side, as a list of segments.
///
/// The empty path is the root of the connection. For S3 the root lists the
/// buckets, the first segment is the bucket and the rest is the key prefix.
/// For SFTP and FTP it is the root directory of the server.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct RemotePath {
    segments: Vec<String>,
}

impl RemotePath {
    /// Parses `/bucket/some/prefix`, ignoring empty segments.
    pub fn parse(text: &str) -> Self {
        Self {
            segments: text
                .split('/')
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect(),
        }
    }

    pub fn is_root(&self) -> bool {
        self.segments.is_empty()
    }

    pub fn segments(&self) -> &[String] {
        &self.segments
    }

    pub fn name(&self) -> &str {
        self.segments.last().map(String::as_str).unwrap_or("")
    }

    pub fn child(&self, name: &str) -> Self {
        let mut segments = self.segments.clone();
        segments.extend(name.split('/').filter(|s| !s.is_empty()).map(str::to_owned));
        Self { segments }
    }

    pub fn parent(&self) -> Option<Self> {
        if self.is_root() {
            return None;
        }
        Some(Self {
            segments: self.segments[..self.segments.len() - 1].to_vec(),
        })
    }

    /// The first `depth` segments of this path.
    pub fn ancestor(&self, depth: usize) -> Self {
        Self {
            segments: self.segments[..depth.min(self.segments.len())].to_vec(),
        }
    }
}

impl std::fmt::Display for RemotePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "/{}", self.segments.join("/"))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EntryKind {
    Bucket,
    Folder,
    #[default]
    File,
}

#[derive(Clone, Debug, Default)]
pub struct Entry {
    pub name: String,
    pub kind: EntryKind,
    pub size: Option<u64>,
    /// Unix timestamp, in seconds.
    pub modified: Option<i64>,
    /// The permission bits (`0o755`), for protocols that tell them.
    pub permissions: Option<u32>,
    /// Names of the owner and the group, or their ids when the server's
    /// names are not known.
    pub owner: Option<String>,
    pub group: Option<String>,
    /// Where a symbolic link points. Linked folders are listed as folders.
    pub link: Option<String>,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.kind != EntryKind::File
    }
}

/// One page of a directory listing.
pub struct ListPage {
    pub entries: Vec<Entry>,
    /// Opaque cursor for the next page, `None` when the listing is complete.
    pub next: Option<String>,
}

/// What a recursive listing finds, handed over as soon as it is known.
/// Paths are relative to the listed folder and `/`-separated.
#[derive(Debug, PartialEq, Eq)]
pub enum Found {
    File { relative: String, size: u64 },
    /// A folder, so that empty ones can be recreated too.
    Folder { relative: String },
    /// A folder below the listed one that could not be read. It is left
    /// out and the listing goes on.
    Unreadable { relative: String, error: String },
}

/// Receives the finds of [`Storage::walk`]; returning false stops it.
pub type FoundSink<'a> = &'a (dyn Fn(Found) -> bool + Send + Sync);

/// The relative path of `name` in a folder at `prefix` of a walk.
fn below(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}/{name}")
    }
}

/// Everything [`Storage::walk`] finds below `path`, once it is all known:
/// for those that need the whole tree before they start.
pub async fn walk_all(storage: &dyn Storage, path: &RemotePath) -> Result<Vec<Found>> {
    let found = std::sync::Mutex::new(Vec::new());
    storage
        .walk(path, &|find| {
            found.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(find);
            true
        })
        .await?;
    Ok(found.into_inner().unwrap_or_else(|poisoned| poisoned.into_inner()))
}

/// Every file below `path`, sorted, as `(relative path, size)`.
#[cfg(test)]
pub async fn files_below(storage: &dyn Storage, path: &RemotePath) -> Result<Vec<(String, u64)>> {
    let mut files: Vec<(String, u64)> = walk_all(storage, path)
        .await?
        .into_iter()
        .filter_map(|found| match found {
            Found::File { relative, size } => Some((relative, size)),
            _ => None,
        })
        .collect();
    files.sort();
    Ok(files)
}

/// Progress of a folder fetched in one stream (see [`Storage::download_tree`]).
#[derive(Default)]
pub struct TreeProgress {
    /// Bytes of all the files, and the cancellation.
    pub bytes: Progress,
    pub files: AtomicU64,
    pub files_done: AtomicU64,
}

/// How a folder fetched in one stream went.
#[derive(Default)]
pub struct TreeReport {
    /// What the archive held, the folder itself included.
    pub entries: usize,
    /// Symbolic links, which are not copied.
    pub links: usize,
    /// What the server could not read, as `path : why`.
    pub unreadable: Vec<String>,
    /// What could not be written here, as `path : why`.
    pub unwritable: Vec<String>,
}

/// Part of a file read by [`Storage::read_range`].
#[derive(Debug, Default)]
pub struct Chunk {
    pub bytes: Vec<u8>,
    /// The size of the whole file when it was read: a file that is being
    /// written to grows between two reads, and one that was rotated shrinks.
    pub size: u64,
}

/// Progress of a running transfer, shared between the worker and the UI.
#[derive(Default)]
pub struct Progress {
    pub done: AtomicU64,
    pub total: AtomicU64,
    cancel: AtomicBool,
    /// Stopped to go on later: what was transferred is kept.
    paused: AtomicBool,
    /// The speed limit of its direction, when the transfer is queued.
    limiter: std::sync::OnceLock<Arc<crate::transfers::limit::Limiter>>,
}

impl Progress {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// True once the transfer must stop: cancelled, or paused.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed) || self.paused.load(Ordering::Relaxed)
    }

    pub fn pause(&self) {
        self.paused.store(true, Ordering::Relaxed);
    }

    pub fn unpause(&self) {
        self.paused.store(false, Ordering::Relaxed);
    }

    /// Paused rather than cancelled: partial data must be kept.
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed) && !self.cancel.load(Ordering::Relaxed)
    }

    pub fn set_limiter(&self, limiter: Arc<crate::transfers::limit::Limiter>) {
        let _ = self.limiter.set(limiter);
    }

    /// Waits until `bytes` more may go, under the speed limit.
    pub async fn throttle(&self, bytes: u64) {
        if let Some(limiter) = self.limiter.get() {
            limiter.take(bytes).await;
        }
    }

    /// Starts counting from `bytes`: what a resumed transfer already has.
    pub fn set_done(&self, bytes: u64) {
        self.done.store(bytes, Ordering::Relaxed);
    }

    pub fn add(&self, bytes: u64) {
        self.done.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn set_total(&self, bytes: u64) {
        self.total.store(bytes, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> (u64, u64) {
        (
            self.done.load(Ordering::Relaxed),
            self.total.load(Ordering::Relaxed),
        )
    }
}

#[async_trait]
pub trait Storage: Send + Sync + 'static {
    /// Lists one page of `path`. Pass the previous page's cursor to continue.
    async fn list_page(&self, path: &RemotePath, cursor: Option<String>) -> Result<ListPage>;

    /// Lists everything below `path`, at any depth, handing each find to
    /// `found` right away so that the caller can act before the end, which
    /// may be minutes away on a big tree. Fails only when `path` itself
    /// cannot be listed or the connection is lost; stops early, without
    /// error, once `found` returns false.
    async fn walk(&self, path: &RemotePath, found: FoundSink<'_>) -> Result<()>;

    async fn download(&self, path: &RemotePath, dest: &Path, progress: &Progress) -> Result<()>;

    async fn upload(&self, src: &Path, dest: &RemotePath, progress: &Progress) -> Result<()>;

    /// Downloads the folder `path` as the local folder `dest` in a single
    /// stream, which is far faster than file by file when there are many
    /// small ones. `Ok(None)` when the connection cannot, before anything
    /// is written: the files are then fetched one by one. An error once
    /// part of the folder arrived is reported as is.
    async fn download_tree(&self, _path: &RemotePath, _dest: &Path, _progress: &TreeProgress) -> Result<Option<TreeReport>> {
        Ok(None)
    }

    /// Whether [`Storage::download_tree`] may work here: a folder then
    /// waits for its turn to try, instead of going file by file at once.
    fn can_download_tree(&self) -> bool {
        false
    }

    /// Deletes a file, or a folder and everything below it.
    /// Returns the number of objects removed.
    async fn delete(&self, path: &RemotePath, is_dir: bool) -> Result<u64>;

    /// Writes `src` over the file `dest` so that a failure halfway leaves
    /// `dest` as it was, which saving an edited file needs: an upload that
    /// fails removes what it half wrote. By default a plain upload, which is
    /// all or nothing on S3.
    async fn replace(&self, src: &Path, dest: &RemotePath) -> Result<()> {
        self.upload(src, dest, &Progress::default()).await
    }

    async fn create_folder(&self, path: &RemotePath) -> Result<()>;

    /// A time-limited link anyone can use to download the file.
    /// Only meaningful when [`Storage::can_share`] is true.
    async fn share_url(&self, _path: &RemotePath, _expires: Duration) -> Result<String> {
        bail!("ce type de connexion ne permet pas de créer des liens")
    }

    fn can_share(&self) -> bool {
        false
    }

    /// The plain (unsigned) address of the file, if the backend has one.
    fn plain_url(&self, path: &RemotePath) -> Option<String>;

    /// Where to land when the connection has no start folder: the account's
    /// home directory for protocols that have one.
    async fn home(&self) -> Result<RemotePath> {
        Ok(RemotePath::default())
    }

    /// True when the root only lists containers (S3 buckets) and cannot hold
    /// files itself.
    fn has_buckets(&self) -> bool {
        false
    }

    /// The S3 connection behind this storage, for what only S3 has
    /// (versions, bucket settings, ACLs…): see `crate::s3`.
    fn s3(&self) -> Option<&s3::S3Storage> {
        None
    }

    /// Reads at most `len` bytes of the file `path` from `offset`, without
    /// fetching the rest: what the previews and the log follower show.
    /// Fewer bytes come back at the end of the file, none past it.
    async fn read_range(&self, _path: &RemotePath, _offset: u64, _len: u64) -> Result<Chunk> {
        bail!("ce type de connexion ne permet pas de lire une partie d'un fichier")
    }

    /// Which of the operations below this connection supports.
    fn operations(&self) -> ops::Operations {
        ops::Operations::default()
    }

    /// Renames `from` to `to`, which may be in another folder of this
    /// connection. What happens when `to` exists depends on the protocol:
    /// callers check first. `progress` counts the bytes when the backend has
    /// to copy them (S3), and is checked for cancellation.
    async fn rename(&self, _from: &RemotePath, _to: &RemotePath, _is_dir: bool, _progress: &Progress) -> Result<()> {
        bail!("ce type de connexion ne permet pas de renommer")
    }

    /// Copies `from` to `to` on this connection, a folder with everything it
    /// holds. `progress` counts the bytes copied, when known.
    async fn copy(&self, _from: &RemotePath, _to: &RemotePath, _is_dir: bool, _progress: &Progress) -> Result<()> {
        bail!("ce type de connexion ne permet pas de copier sur le serveur")
    }

    /// Sets the permission bits (`0o755`) of one file or folder.
    async fn set_permissions(&self, _path: &RemotePath, _mode: u32) -> Result<()> {
        bail!("ce type de connexion ne permet pas de changer les permissions")
    }

    /// Changes the owner and the group, as numeric ids; `None` keeps it.
    async fn set_owner(&self, _path: &RemotePath, _uid: Option<u32>, _gid: Option<u32>) -> Result<()> {
        bail!("ce type de connexion ne permet pas de changer le propriétaire")
    }

    /// The users and groups the server knows, to show names for ids.
    async fn accounts(&self) -> Result<ops::Accounts> {
        Ok(ops::Accounts::default())
    }

    /// What the server says about one entry beyond its listing.
    async fn properties(&self, _path: &RemotePath, _is_dir: bool) -> Result<ops::Properties> {
        Ok(ops::Properties::default())
    }

    /// Counts what the folder `path` holds, at any depth. `tally` grows as
    /// the walk goes and is checked for cancellation.
    async fn folder_size(&self, path: &RemotePath, tally: &ops::Tally) -> Result<()> {
        ops::walk_size(self, path, tally).await
    }
    /// The files of this computer itself: their paths are those of the
    /// disk, so they open and copy without any download.
    fn local(&self) -> Option<&local::LocalStorage> {
        None
    }

    // Transfers that go on where they stopped, and synchronisation (see
    // `crate::transfers`).

    /// Whether [`Storage::download_resumable`] works on this connection.
    fn can_resume_download(&self) -> bool {
        false
    }

    /// Fetches `path` into the local file `partial`. With `resume`, what
    /// `partial` holds is taken as the beginning of the file and only the
    /// rest is fetched; otherwise it starts from nothing. Unlike
    /// [`Storage::download`], nothing is renamed nor removed, even on
    /// failure: the caller decides what becomes of the partial copy.
    async fn download_resumable(
        &self,
        _path: &RemotePath,
        _partial: &Path,
        _resume: bool,
        _progress: &Progress,
    ) -> Result<()> {
        bail!("ce type de connexion ne permet pas de reprendre un téléchargement")
    }

    /// Whether [`Storage::upload_resumable`] works on this connection.
    fn can_resume_upload(&self) -> bool {
        false
    }

    /// Sends `src` as `dest`. With `resume`, what `dest` holds is taken as
    /// the beginning of `src` and only the rest is sent. Unlike
    /// [`Storage::upload`], what was sent stays in place on failure.
    async fn upload_resumable(&self, _src: &Path, _dest: &RemotePath, _resume: bool, _progress: &Progress) -> Result<()> {
        bail!("ce type de connexion ne permet pas de reprendre un envoi")
    }

    /// Gives up an upload made with [`Storage::upload_resumable`] and left
    /// halfway: what it sent goes. By default that is the file itself.
    async fn abandon_upload(&self, dest: &RemotePath) -> Result<()> {
        self.delete(dest, false).await.map(|_| ())
    }

    /// Every file below `path`, with its date: what synchronising compares.
    /// By default the folders are listed one by one.
    async fn list_deep_dated(&self, path: &RemotePath) -> Result<Vec<DeepFile>> {
        /// Deeper than this is a loop of links rather than real folders.
        const MAX_DEPTH: usize = 64;
        let mut files = Vec::new();
        let mut pending = vec![(path.clone(), String::new(), 0)];
        while let Some((dir, prefix, depth)) = pending.pop() {
            let mut cursor = None;
            loop {
                let page = self.list_page(&dir, cursor).await?;
                for entry in page.entries {
                    let relative = if prefix.is_empty() {
                        entry.name.clone()
                    } else {
                        format!("{prefix}/{}", entry.name)
                    };
                    if !entry.is_dir() {
                        files.push(DeepFile {
                            relative,
                            size: entry.size.unwrap_or(0),
                            modified: entry.modified,
                        });
                    } else if depth < MAX_DEPTH {
                        pending.push((dir.child(&entry.name), relative, depth + 1));
                    }
                }
                cursor = page.next;
                if cursor.is_none() {
                    break;
                }
            }
        }
        Ok(files)
    }

    /// Gives a remote file the date of its original, so that comparing them
    /// later finds them equal.
    async fn set_mtime(&self, _path: &RemotePath, _modified: i64) -> Result<()> {
        bail!("ce serveur ne permet pas de changer la date d'un fichier")
    }

    /// Whether [`Storage::set_mtime`] works: otherwise a file is dated by
    /// its upload, whatever the date of its original.
    fn keeps_dates(&self) -> bool {
        false
    }

    /// Seconds within which two dates given by this server are the same.
    fn date_precision(&self) -> i64 {
        2
    }
}

/// A file found by [`Storage::list_deep_dated`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeepFile {
    /// Path relative to the listed folder, `/`-separated.
    pub relative: String,
    pub size: u64,
    /// Unix seconds, when known.
    pub modified: Option<i64>,
}

/// Identifies a connection, to match change notifications with the tabs
/// browsing it.
pub fn storage_key(storage: &Arc<dyn Storage>) -> usize {
    Arc::as_ptr(storage) as *const () as usize
}

/// Buffer size for streamed transfers (SFTP, FTP).
const CHUNK: usize = 256 * 1024;

pub(crate) fn partial_path(dest: &Path) -> PathBuf {
    let mut name = dest.as_os_str().to_owned();
    name.push(".part");
    PathBuf::from(name)
}

/// Saves `reader` as the local file `dest`. The data goes to a `.part` file
/// first, so an interrupted download never looks complete.
pub(crate) async fn save_stream<R>(reader: &mut R, dest: &Path, progress: &Progress) -> Result<()>
where
    R: AsyncRead + Unpin + ?Sized,
{
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let partial = partial_path(dest);
    let copied = async {
        let mut file = tokio::fs::File::create(&partial).await?;
        let mut buffer = vec![0u8; CHUNK];
        loop {
            if progress.is_cancelled() {
                bail!("annulé");
            }
            let read = reader.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            progress.throttle(read as u64).await;
            file.write_all(&buffer[..read]).await?;
            progress.add(read as u64);
        }
        file.flush().await?;
        Ok(())
    }
    .await;

    match copied {
        Ok(()) => tokio::fs::rename(&partial, dest)
            .await
            .with_context(|| format!("impossible d'écrire {}", dest.display())),
        Err(error) => {
            let _ = tokio::fs::remove_file(&partial).await;
            Err(error)
        }
    }
}

/// Sends the local file `src` into `writer`, without closing it.
pub(crate) async fn send_file<W>(src: &Path, writer: &mut W, progress: &Progress) -> Result<()>
where
    W: AsyncWrite + Unpin + ?Sized,
{
    send_file_from(src, 0, writer, progress).await
}

/// Sends the local file `src` from byte `offset` on into `writer`, without
/// closing it: the rest of an interrupted upload.
pub(crate) async fn send_file_from<W>(src: &Path, offset: u64, writer: &mut W, progress: &Progress) -> Result<()>
where
    W: AsyncWrite + Unpin + ?Sized,
{
    let mut file = tokio::fs::File::open(src)
        .await
        .with_context(|| format!("impossible de lire {}", src.display()))?;
    if offset > 0 {
        file.seek(std::io::SeekFrom::Start(offset)).await?;
    }
    let mut buffer = vec![0u8; CHUNK];
    loop {
        if progress.is_cancelled() {
            bail!("annulé");
        }
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        progress.throttle(read as u64).await;
        writer.write_all(&buffer[..read]).await?;
        progress.add(read as u64);
    }
    writer.flush().await?;
    Ok(())
}

/// Writes what `reader` yields into the local file `dest`, after its first
/// `offset` bytes, which are kept (0 starts the file afresh). Unlike
/// [`save_stream`], nothing is renamed nor removed on failure, so that the
/// download can go on later from what arrived.
pub(crate) async fn append_stream<R>(reader: &mut R, dest: &Path, offset: u64, progress: &Progress) -> Result<()>
where
    R: AsyncRead + Unpin + ?Sized,
{
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(offset == 0)
        .open(dest)
        .await
        .with_context(|| format!("impossible d'écrire {}", dest.display()))?;
    if offset > 0 {
        // Whatever lies beyond the part being kept is not trusted.
        file.set_len(offset).await?;
        file.seek(std::io::SeekFrom::Start(offset)).await?;
    }
    progress.set_done(offset);
    let mut buffer = vec![0u8; CHUNK];
    loop {
        if progress.is_cancelled() {
            file.flush().await?;
            bail!("annulé");
        }
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        progress.throttle(read as u64).await;
        file.write_all(&buffer[..read]).await?;
        progress.add(read as u64);
    }
    file.flush().await?;
    Ok(())
}

/// The length of a local file, 0 when there is none.
pub(crate) async fn local_len(path: &Path) -> u64 {
    tokio::fs::metadata(path).await.map_or(0, |meta| meta.len())
}
