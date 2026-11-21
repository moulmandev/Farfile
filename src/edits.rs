//! Remote files opened in a local program.
//!
//! Opening a file fetches it into a private folder and hands that copy to an
//! editor. The copy is then watched: once the editor saves it, the app offers
//! to send it back where it came from. An edit outlives the tab it was opened
//! from, since it keeps its own handle on the connection.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use crate::dev;
use crate::launch::Launch;
use crate::storage::{RemotePath, Storage, storage_key};

/// Copies left behind by a run that could not clean up are removed after
/// this long. Until then, changes that were never sent can still be found.
const KEEP_LEFTOVERS: Duration = Duration::from_secs(7 * 24 * 3600);

/// What tells two versions of a local file apart: its date and its size.
pub type Stamp = (SystemTime, u64);

pub fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// Shared with the background task that watches the local copy.
#[derive(Default)]
pub struct Watch {
    /// The version last reported to the app. Nothing is reported while this
    /// is empty, i.e. while the copy is being fetched.
    seen: Mutex<Option<Stamp>>,
    stop: AtomicBool,
}

impl Watch {
    pub fn seen(&self) -> Option<Stamp> {
        *self.seen.lock().unwrap()
    }

    pub fn set_seen(&self, stamp: Option<Stamp>) {
        *self.seen.lock().unwrap() = stamp;
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum State {
    /// Being downloaded; the program starts once it is there.
    Fetching,
    /// The local copy is what the server has.
    Synced,
    /// Saved locally and not sent.
    Modified,
    Uploading,
    /// The last upload failed; the changes are still only local.
    Failed(String),
}

pub struct Edit {
    pub id: u64,
    /// Identifies the remote file across tabs: its address when it has one.
    key: String,
    pub name: String,
    /// The connection it came from, for display.
    pub origin: String,
    pub remote: RemotePath,
    pub storage: Arc<dyn Storage>,
    pub local: PathBuf,
    pub state: State,
    /// Send every save without asking.
    pub auto_upload: bool,
    /// How to open the copy once it has been fetched.
    pub launch: Launch,
    /// Saved once more while an upload was running.
    pub saved_again: bool,
    pub watch: Arc<Watch>,
}

impl Edit {
    /// True while the server lacks changes made here.
    pub fn unsent(&self) -> bool {
        matches!(self.state, State::Modified | State::Uploading | State::Failed(_))
    }

    /// Where the file goes back to, for display.
    pub fn target(&self) -> String {
        format!("{} · {}", self.origin, self.remote)
    }
}

pub fn key(storage: &Arc<dyn Storage>, remote: &RemotePath) -> String {
    storage
        .plain_url(remote)
        .unwrap_or_else(|| format!("{}:{remote}", storage_key(storage)))
}

/// The files being edited, and the folder holding their local copies.
pub struct Edits {
    root: PathBuf,
    next_id: u64,
    list: Vec<Edit>,
}

impl Edits {
    pub fn new() -> Self {
        let run = format!(
            "{}-{}",
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_secs()),
            std::process::id()
        );
        // Scripted runs keep away from the copies of the real app.
        let base = if dev::active() {
            std::env::temp_dir().join("s3viewer-dev").join("edit")
        } else {
            let base = directories::ProjectDirs::from("", "", crate::APP_ID)
                .map(|dirs| dirs.cache_dir().join("edit"))
                .unwrap_or_else(|| std::env::temp_dir().join(crate::APP_ID).join("edit"));
            purge_leftovers(base.clone());
            base
        };
        Self {
            root: base.join(run),
            next_id: 1,
            list: Vec::new(),
        }
    }

    pub fn list(&self) -> &[Edit] {
        &self.list
    }

    pub fn get(&self, id: u64) -> Option<&Edit> {
        self.list.iter().find(|edit| edit.id == id)
    }

    pub fn get_mut(&mut self, id: u64) -> Option<&mut Edit> {
        self.list.iter_mut().find(|edit| edit.id == id)
    }

    pub fn find(&self, key: &str) -> Option<u64> {
        self.list.iter().find(|edit| edit.key == key).map(|edit| edit.id)
    }

    /// How many files hold changes the server does not have.
    pub fn unsent(&self) -> usize {
        self.list.iter().filter(|edit| edit.unsent()).count()
    }

    /// Registers a file about to be fetched. Each gets its own folder, so
    /// the copy keeps the name of the original.
    pub fn add(
        &mut self,
        storage: Arc<dyn Storage>,
        remote: RemotePath,
        origin: String,
        local_name: &str,
        launch: Launch,
    ) -> &Edit {
        let id = self.next_id;
        self.next_id += 1;
        self.list.push(Edit {
            id,
            key: key(&storage, &remote),
            name: remote.name().to_owned(),
            origin,
            local: self.root.join(id.to_string()).join(local_name),
            remote,
            storage,
            state: State::Fetching,
            auto_upload: false,
            launch,
            saved_again: false,
            watch: Arc::new(Watch::default()),
        });
        self.list.last().expect("just pushed")
    }

    /// Stops following a file and deletes its local copy.
    pub fn forget(&mut self, id: u64) {
        if let Some(index) = self.list.iter().position(|edit| edit.id == id) {
            let edit = self.list.remove(index);
            edit.watch.stop();
            remove_copy(&edit.local);
        }
    }
}

impl Drop for Edits {
    /// On the way out, copies in sync with the server go away. Those with
    /// changes that were never sent stay on disk for [`KEEP_LEFTOVERS`].
    fn drop(&mut self) {
        for edit in &self.list {
            edit.watch.stop();
            if !edit.unsent() {
                remove_copy(&edit.local);
            }
        }
        // Only succeeds once the folder is empty.
        let _ = std::fs::remove_dir(&self.root);
    }
}

fn remove_copy(local: &Path) {
    if let Some(folder) = local.parent() {
        let _ = std::fs::remove_dir_all(folder);
    }
}

/// Deletes what earlier runs left in `base`, once old enough.
fn purge_leftovers(base: PathBuf) {
    std::thread::spawn(move || {
        let Ok(runs) = std::fs::read_dir(&base) else {
            return;
        };
        for run in runs.flatten() {
            let old = newest_change(&run.path())
                .and_then(|changed| changed.elapsed().ok())
                .is_some_and(|age| age > KEEP_LEFTOVERS);
            if old {
                let _ = std::fs::remove_dir_all(run.path());
            }
        }
    });
}

/// The most recent modification of a folder or of anything inside it.
fn newest_change(path: &Path) -> Option<SystemTime> {
    let mut newest = std::fs::metadata(path).ok()?.modified().ok()?;
    if let Ok(children) = std::fs::read_dir(path) {
        for child in children.flatten() {
            if let Some(changed) = newest_change(&child.path()) {
                newest = newest.max(changed);
            }
        }
    }
    Some(newest)
}

#[cfg(test)]
mod tests {
    use anyhow::{Result, bail};
    use async_trait::async_trait;

    use super::*;
    use crate::storage::{FoundSink, ListPage, Progress};

    /// A connection that is never used: the tests only handle local copies.
    struct Nowhere;

    #[async_trait]
    impl Storage for Nowhere {
        async fn list_page(&self, _: &RemotePath, _: Option<String>) -> Result<ListPage> {
            bail!("unused")
        }
        async fn walk(&self, _: &RemotePath, _: FoundSink<'_>) -> Result<()> {
            bail!("unused")
        }
        async fn download(&self, _: &RemotePath, _: &Path, _: &Progress) -> Result<()> {
            bail!("unused")
        }
        async fn upload(&self, _: &Path, _: &RemotePath, _: &Progress) -> Result<()> {
            bail!("unused")
        }
        async fn delete(&self, _: &RemotePath, _: bool) -> Result<u64> {
            bail!("unused")
        }
        async fn create_folder(&self, _: &RemotePath) -> Result<()> {
            bail!("unused")
        }
        fn plain_url(&self, path: &RemotePath) -> Option<String> {
            Some(format!("test://host{path}"))
        }
    }

    fn workspace(name: &str) -> Edits {
        let root = std::env::temp_dir().join(format!("s3viewer-edits-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        Edits {
            root,
            next_id: 1,
            list: Vec::new(),
        }
    }

    fn open(edits: &mut Edits, path: &str, state: State) -> (u64, PathBuf) {
        let storage: Arc<dyn Storage> = Arc::new(Nowhere);
        let remote = RemotePath::parse(path);
        let name = remote.name().to_owned();
        let (id, local) = {
            let edit = edits.add(storage, remote, "Test".to_owned(), &name, Launch::Default);
            (edit.id, edit.local.clone())
        };
        std::fs::create_dir_all(local.parent().unwrap()).unwrap();
        std::fs::write(&local, "contenu").unwrap();
        edits.get_mut(id).unwrap().state = state;
        (id, local)
    }

    #[test]
    fn the_same_remote_file_is_one_edit() {
        let mut edits = workspace("same");
        let (id, local) = open(&mut edits, "/etc/app/config.yml", State::Synced);
        let again: Arc<dyn Storage> = Arc::new(Nowhere);
        assert_eq!(edits.find(&key(&again, &RemotePath::parse("/etc/app/config.yml"))), Some(id));
        assert_eq!(edits.find(&key(&again, &RemotePath::parse("/etc/app/other.yml"))), None);
        // The copy keeps the name of the original, in a folder of its own.
        assert_eq!(local.file_name().unwrap(), "config.yml");
        assert_eq!(edits.get(id).unwrap().target(), "Test · /etc/app/config.yml");
    }

    #[test]
    fn forgetting_an_edit_deletes_its_copy() {
        let mut edits = workspace("forget");
        let (id, local) = open(&mut edits, "/a/notes.txt", State::Modified);
        assert_eq!(edits.unsent(), 1);
        edits.forget(id);
        assert!(!local.exists());
        assert!(edits.list().is_empty());
    }

    #[test]
    fn leaving_keeps_only_the_changes_that_were_never_sent() {
        let mut edits = workspace("leave");
        let (_, synced) = open(&mut edits, "/a/synced.txt", State::Synced);
        let (_, modified) = open(&mut edits, "/a/modified.txt", State::Modified);
        let (_, failed) = open(&mut edits, "/a/failed.txt", State::Failed("refusé".to_owned()));
        let root = edits.root.clone();
        let watch = edits.list()[0].watch.clone();
        drop(edits);

        assert!(watch.stopped());
        assert!(!synced.exists());
        assert!(modified.exists() && failed.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_age_of_a_folder_is_that_of_its_newest_file() {
        let root = std::env::temp_dir().join(format!("s3viewer-edits-test-age-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("1")).unwrap();
        std::fs::write(root.join("1").join("a.txt"), "a").unwrap();
        let newest = newest_change(&root).unwrap();
        assert!(newest >= std::fs::metadata(root.join("1").join("a.txt")).unwrap().modified().unwrap());
        assert!(newest.elapsed().unwrap() < KEEP_LEFTOVERS);
        let _ = std::fs::remove_dir_all(root);
    }
}
