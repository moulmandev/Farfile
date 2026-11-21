//! Reading a file into the editor and writing it back where it came from:
//! straight on the disk for this computer, through a private copy and the
//! connection's upload otherwise. What the server says of the file (size,
//! date) is kept, to notice when someone else changed it meanwhile.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};

use super::encoding::{self, Decoded, Indent};
use crate::viewers::decode::Encoding;
use super::syntax;
use super::text::Document;
use crate::dev;
use crate::storage::{Progress, RemotePath, Storage, storage_key};
use crate::worker::{Emitter, Event, Worker, safe_component};
use syntect::parsing::SyntaxReference;

/// Bigger files are refused by the editor: a preview or another program
/// suits them better.
pub const MAX_SIZE: u64 = 32 * 1024 * 1024;

/// What tells two versions of a file apart, as the server says.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stamp {
    pub size: Option<u64>,
    /// Milliseconds since 1970: seconds for most servers.
    pub modified: Option<i64>,
}

impl Stamp {
    /// Whether the file changed between `self` and `now`, as far as both
    /// tell.
    pub fn differs(&self, now: &Stamp) -> bool {
        let size = matches!((self.size, now.size), (Some(a), Some(b)) if a != b);
        let date = matches!((self.modified, now.modified), (Some(a), Some(b)) if a != b);
        size || date
    }
}

/// The size and date of `path` now. An error when the server cannot say.
pub async fn stat(storage: &Arc<dyn Storage>, path: &RemotePath) -> Result<Stamp> {
    if let Some(local) = storage.local() {
        let real = local.resolve(path)?;
        let meta = tokio::fs::metadata(&real).await.with_context(|| format!("{} introuvable", real.display()))?;
        let modified = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|elapsed| elapsed.as_millis() as i64);
        return Ok(Stamp { size: Some(meta.len()), modified });
    }
    if let Ok(found) = storage.properties(path, false).await
        && (found.size.is_some() || found.modified.is_some())
    {
        return Ok(Stamp { size: found.size, modified: found.modified.map(|seconds| seconds * 1000) });
    }
    // FTP, and servers that tell nothing of one file: its folder does.
    let folder = path.parent().unwrap_or_default();
    let mut cursor = None;
    loop {
        let page = storage.list_page(&folder, cursor).await?;
        if let Some(entry) = page.entries.into_iter().find(|entry| entry.name == path.name() && !entry.is_dir()) {
            return Ok(Stamp { size: entry.size, modified: entry.modified.map(|seconds| seconds * 1000) });
        }
        cursor = page.next;
        if cursor.is_none() {
            bail!("« {} » n'existe plus", path.name());
        }
    }
}

/// Where remote files are copied on their way in and out. Scripted runs
/// keep away from the real app's.
fn copies() -> PathBuf {
    let base = if dev::active() {
        std::env::temp_dir().join("s3viewer-dev")
    } else {
        directories::ProjectDirs::from("", "", crate::APP_ID)
            .map(|dirs| dirs.cache_dir().to_path_buf())
            .unwrap_or_else(|| std::env::temp_dir().join(crate::APP_ID))
    };
    let unique = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos())
    );
    base.join("editor").join(unique)
}

/// A private copy, removed with its folder when dropped.
struct TempCopy(PathBuf);

impl TempCopy {
    fn of(name: &str) -> Self {
        Self(copies().join(safe_component(name)))
    }
}

impl Drop for TempCopy {
    fn drop(&mut self) {
        if let Some(folder) = self.0.parent() {
            let _ = std::fs::remove_dir_all(folder);
        }
    }
}

/// A file read for the editor, worked out off the screen's thread.
pub struct Opened {
    /// `None` when the bytes are not text.
    pub decoded: Option<(Decoded, Document)>,
    pub size: u64,
    pub stamp: Option<Stamp>,
    pub language: &'static SyntaxReference,
    pub indent: Option<Indent>,
}

/// Reads the whole file. `limit` refuses bigger ones before anything is
/// fetched, when the size is known. `forced` reads it in that encoding,
/// whatever its bytes look like.
pub async fn open(storage: Arc<dyn Storage>, path: RemotePath, limit: u64, forced: Option<Encoding>) -> Result<Opened> {
    let stamp = stat(&storage, &path).await.ok();
    if let Some(size) = stamp.and_then(|stamp| stamp.size)
        && size > limit
    {
        bail!("le fichier est trop gros pour l'éditeur ({})", crate::format::size(size));
    }
    let bytes = read(&storage, &path).await?;
    let name = path.name().to_owned();
    let full = path.to_string();
    tokio::task::spawn_blocking(move || {
        let size = bytes.len() as u64;
        let decoded = match forced {
            Some(chosen) => {
                let (text, ending, mixed) = encoding::split_lines(&encoding::decode_as(&bytes, chosen));
                Some(Decoded { text, encoding: chosen, ending, mixed })
            }
            None => encoding::decode(&bytes),
        };
        let decoded = decoded.map(|decoded| {
            let document = Document::from_text(&decoded.text);
            (decoded, document)
        });
        let first_line = decoded.as_ref().map_or("", |(_, doc)| doc.line(0));
        let language = syntax::detect(&name, &full, first_line);
        let indent = decoded.as_ref().and_then(|(decoded, _)| encoding::detect_indent(&decoded.text));
        Opened { decoded, size, stamp, language, indent }
    })
    .await
    .context("lecture interrompue")
}

async fn read(storage: &Arc<dyn Storage>, path: &RemotePath) -> Result<Vec<u8>> {
    if let Some(local) = storage.local() {
        let real = local.resolve(path)?;
        return tokio::fs::read(&real).await.with_context(|| format!("impossible de lire {}", real.display()));
    }
    let copy = TempCopy::of(path.name());
    storage.download(path, &copy.0, &Progress::default()).await?;
    let bytes = tokio::fs::read(&copy.0).await.context("copie locale illisible")?;
    Ok(bytes)
}

/// How a save went.
#[derive(Debug)]
pub enum Saved {
    /// Written; what the server says of the file now.
    Done(Option<Stamp>),
    /// Not written: the file changed on the server since it was read.
    Changed,
}

/// Writes `bytes` as the file. Unless `force`, first checks that the file
/// is still what `expected` says, and leaves it alone otherwise.
pub async fn save(
    storage: Arc<dyn Storage>,
    path: RemotePath,
    bytes: Vec<u8>,
    expected: Option<Stamp>,
    force: bool,
    emitter: Option<Emitter>,
) -> Result<Saved> {
    if !force
        && let Some(expected) = expected
        && let Ok(now) = stat(&storage, &path).await
        && expected.differs(&now)
    {
        return Ok(Saved::Changed);
    }
    write(&storage, &path, &bytes).await?;
    if let (Some(emitter), Some(folder)) = (emitter, path.parent()) {
        emitter.emit(Event::Changed { storage: storage_key(&storage), path: folder });
    }
    Ok(Saved::Done(stat(&storage, &path).await.ok()))
}

async fn write(storage: &Arc<dyn Storage>, path: &RemotePath, bytes: &[u8]) -> Result<()> {
    if let Some(local) = storage.local() {
        let real = local.resolve(path)?;
        return write_in_place(&real, bytes).await;
    }
    let copy = TempCopy::of(path.name());
    if let Some(folder) = copy.0.parent() {
        tokio::fs::create_dir_all(folder).await.context("dossier temporaire impossible à créer")?;
    }
    tokio::fs::write(&copy.0, bytes).await.context("copie locale impossible à écrire")?;
    // Never `upload`: on failure it removes what it half wrote, which here
    // would be the user's file.
    storage.replace(&copy.0, path).await
}

/// Writes over the file itself, so that it keeps its rights, links and
/// place in the folder, as Notepad does.
async fn write_in_place(real: &Path, bytes: &[u8]) -> Result<()> {
    tokio::fs::write(real, bytes)
        .await
        .with_context(|| format!("impossible d'écrire {}", real.display()))
}

/// A job whose result is picked up by the screen on a later frame. Unlike
/// the viewers' tasks it is not stopped when nobody waits for it any more:
/// a save goes to its end even once its tab is closed.
pub struct Pending<T> {
    slot: Arc<Mutex<Option<T>>>,
}

impl<T: Send + 'static> Pending<T> {
    pub fn start(worker: &Worker, job: impl Future<Output = T> + Send + 'static) -> Self {
        let slot = Arc::new(Mutex::new(None));
        let (filled, ctx) = (slot.clone(), worker.ctx());
        worker.spawn(async move {
            let outcome = job.await;
            *filled.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(outcome);
            ctx.request_repaint();
        });
        Self { slot }
    }

    /// The result, once, as soon as the job is over.
    pub fn take(&self) -> Option<T> {
        self.slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::local::tests::{Scratch, storage_in};

    #[test]
    fn stamps_tell_versions_apart_as_far_as_they_can() {
        let old = Stamp { size: Some(10), modified: Some(1_000) };
        assert!(!old.differs(&old));
        assert!(old.differs(&Stamp { size: Some(11), modified: Some(1_000) }));
        assert!(old.differs(&Stamp { size: Some(10), modified: Some(2_000) }));
        // What a server does not say cannot tell them apart.
        assert!(!old.differs(&Stamp { size: None, modified: None }));
        assert!(!Stamp::default().differs(&old));
    }

    /// A file of this computer: read, saved over, and the check against a
    /// change made by someone else.
    #[tokio::test]
    async fn local_files_are_read_and_written_in_place() {
        let scratch = Scratch::new("editor-file");
        let storage: Arc<dyn Storage> = Arc::new(storage_in(&scratch.0));
        let path = RemotePath::parse("Essai/notes.txt");
        std::fs::write(scratch.0.join("notes.txt"), b"caf\xe9\r\nligne 2\r\n").unwrap();

        let opened = open(storage.clone(), path.clone(), MAX_SIZE, None).await.unwrap();
        let (decoded, doc) = opened.decoded.unwrap();
        assert_eq!(doc.text(), "café\nligne 2\n");
        assert_eq!(opened.language.name, "Plain Text");
        let stamp = opened.stamp.unwrap();
        assert_eq!(stamp.size, Some(15));

        let bytes = encoding::encode("thé\nligne 2\n", decoded.encoding, decoded.ending).unwrap();
        let saved = save(storage.clone(), path.clone(), bytes, Some(stamp), false, None).await.unwrap();
        let Saved::Done(Some(after)) = saved else {
            panic!("not saved: {saved:?}");
        };
        assert_eq!(std::fs::read(scratch.0.join("notes.txt")).unwrap(), b"th\xe9\r\nligne 2\r\n");

        // Changed by another program since: refused, unless forced.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(scratch.0.join("notes.txt"), b"someone else").unwrap();
        let refused = save(storage.clone(), path.clone(), b"mine".to_vec(), Some(after), false, None).await.unwrap();
        assert!(matches!(refused, Saved::Changed));
        assert_eq!(std::fs::read(scratch.0.join("notes.txt")).unwrap(), b"someone else");
        save(storage.clone(), path.clone(), b"mine".to_vec(), Some(after), true, None).await.unwrap();
        assert_eq!(std::fs::read(scratch.0.join("notes.txt")).unwrap(), b"mine");

        // Too big, and not text.
        assert!(open(storage.clone(), path.clone(), 2, None).await.is_err());
        std::fs::write(scratch.0.join("image.png"), b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR").unwrap();
        let binary = open(storage, RemotePath::parse("Essai/image.png"), MAX_SIZE, None).await.unwrap();
        assert!(binary.decoded.is_none());
    }
}
