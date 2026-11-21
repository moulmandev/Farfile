//! Looking into remote files without downloading them: a quick preview of
//! the selected file, a search through a folder and all its subfolders,
//! and followers that show what is appended to a log as it happens.
//!
//! All of it belongs to the tab it was opened from: the preview covers the
//! files of that tab, the search and the followers share a panel below
//! them. Their jobs stop as soon as what they feed is closed.

pub mod decode;
pub mod follow;
#[cfg(test)]
mod live_tests;
#[cfg(test)]
mod memory;
pub mod preview;
pub mod search;
pub mod task;

use std::sync::Arc;

use crate::launch::Launch;
use crate::storage::{Entry, RemotePath, Storage};
use crate::worker::Worker;
use follow::Follow;
use preview::{Origin, Preview};
use search::Search;
use task::Spawner;

/// What the panel below the files shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pane {
    Search,
    Follow(u64),
}

/// A file to select once its folder is listed, and what to do with it then.
pub struct Reveal {
    pub folder: RemotePath,
    pub name: String,
    pub then: Option<Launch>,
}

/// The viewers of one tab.
pub struct Viewers {
    pub spawner: Spawner,
    pub preview: Option<Preview>,
    pub search: Option<Search>,
    pub follows: Vec<Follow>,
    pub pane: Pane,
    pub reveal: Option<Reveal>,
    /// The search results were clicked last, rather than the files: the
    /// space bar previews the selected result.
    pub results_focused: bool,
    /// The file last clicked in the listing, to notice the next click.
    pub listing_anchor: Option<String>,
    /// The panel is folded down to its row of tabs.
    pub folded: bool,
    next_id: u64,
}

impl Viewers {
    pub fn new(worker: &Worker) -> Self {
        Self {
            spawner: Spawner::new(worker),
            preview: None,
            search: None,
            follows: Vec::new(),
            pane: Pane::Search,
            reveal: None,
            results_focused: false,
            listing_anchor: None,
            folded: false,
            next_id: 1,
        }
    }

    /// Whether the panel below the files has anything to show.
    pub fn has_panel(&self) -> bool {
        self.search.is_some() || !self.follows.is_empty()
    }

    pub fn show_preview(&mut self, storage: Arc<dyn Storage>, origin: Origin, path: RemotePath, entry: Entry) {
        self.preview = Some(Preview::open(&self.spawner, storage, origin, path, entry));
    }

    /// Follows `path`, or brings its follower forward if there is one.
    pub fn follow(&mut self, storage: Arc<dyn Storage>, path: RemotePath, hint: u64) {
        let id = match self.follows.iter().find(|follow| follow.path == path) {
            Some(follow) => follow.id,
            None => {
                let id = self.next_id;
                self.next_id += 1;
                self.follows.push(Follow::start(&self.spawner, storage, path, hint, id));
                id
            }
        };
        self.pane = Pane::Follow(id);
        self.folded = false;
    }

    /// Stops following; the panel turns to a neighbour.
    pub fn close_follow(&mut self, id: u64) {
        let Some(at) = self.follows.iter().position(|follow| follow.id == id) else {
            return;
        };
        self.follows.remove(at);
        if self.pane == Pane::Follow(id) {
            self.pane = self
                .follows
                .get(at)
                .or_else(|| self.follows.last())
                .map_or(Pane::Search, |follow| Pane::Follow(follow.id));
        }
    }

    pub fn close_search(&mut self) {
        self.search = None;
        self.results_focused = false;
        if self.pane == Pane::Search
            && let Some(follow) = self.follows.first()
        {
            self.pane = Pane::Follow(follow.id);
        }
    }

    /// Opens the search on `root`, with the filter of the files as a
    /// start. A search that runs keeps its folder.
    pub fn open_search(&mut self, root: RemotePath, pattern: &str) {
        match &mut self.search {
            Some(search) => {
                if !search.running() {
                    search.root = root;
                }
                if !pattern.trim().is_empty() {
                    search.pattern = pattern.trim().to_owned();
                }
                search.focus = true;
            }
            None => self.search = Some(Search::new(root, pattern)),
        }
        self.pane = Pane::Search;
        self.folded = false;
    }

    /// The pane shown, made valid.
    pub fn pane(&mut self) -> Pane {
        let valid = match self.pane {
            Pane::Search => self.search.is_some(),
            Pane::Follow(id) => self.follows.iter().any(|follow| follow.id == id),
        };
        if !valid {
            self.pane = match (&self.search, self.follows.first()) {
                (Some(_), _) | (None, None) => Pane::Search,
                (None, Some(follow)) => Pane::Follow(follow.id),
            };
        }
        self.pane
    }
}

/// How many folders a search lists at once: FTP servers allow few
/// connections, and the files must stay browsable meanwhile.
pub fn parallel_listings(protocol: crate::profiles::Protocol) -> usize {
    if protocol == crate::profiles::Protocol::Ftp { 1 } else { 8 }
}
