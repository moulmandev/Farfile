//! State of one file browser: where we are, what is cached, what is
//! selected and how the listing is sorted and filtered.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use crate::format::natural_cmp;
use crate::storage::{Entry, EntryKind, RemotePath, Storage, storage_key};

#[derive(Default)]
pub struct Listing {
    pub entries: Vec<Entry>,
    /// While a known folder is being refreshed, new pages accumulate here
    /// and replace `entries` in one go, so the view never flickers.
    pub refreshed: Option<Vec<Entry>>,
    pub loading: bool,
    pub error: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Size,
    Modified,
}

pub struct Session {
    pub title: String,
    pub storage: Arc<dyn Storage>,
    pub path: RemotePath,
    /// The listing request this browser is waiting for; anything answering
    /// an older one is ignored.
    pub generation: Arc<AtomicU64>,
    /// False until the first listing was requested: connections are only
    /// opened once their files are actually shown.
    pub started: bool,
    /// The start folder is not known yet: ask the server for the home one.
    pub needs_home: bool,
    back: Vec<RemotePath>,
    forward: Vec<RemotePath>,
    cache: HashMap<RemotePath, Listing>,

    pub filter: String,
    pub sort_key: SortKey,
    pub ascending: bool,
    /// Names of the selected entries in the current folder.
    pub selection: HashSet<String>,
    /// Where a shift-click range starts.
    pub anchor: Option<String>,
    /// Indices into the current listing, filtered and sorted.
    view: Vec<usize>,
    view_stale: bool,
    /// Row to bring into view on the next frame.
    pub scroll_to: Option<usize>,
    /// When set, refresh the current folder once this time has passed.
    pub refresh_at: Option<f64>,
    /// Give the keyboard to the filter field on the next frame.
    pub focus_filter: bool,
    /// The files deleted from this folder, on S3 (see `crate::s3`).
    pub s3: crate::s3::Browse,
    /// Rename in place, bookmarks, clipboard and the like (see `file_ops`).
    pub file_ops: crate::file_ops::SessionOps,
    /// What the other view shows, while this browser is on screen next to
    /// one: where "Copier vers l'autre vue" copies. Set on every frame.
    pub other_view: Option<crate::copy::OtherView>,
}

impl Session {
    /// `start` is where to land; `None` means the account's home folder.
    pub fn new(title: String, storage: Arc<dyn Storage>, start: Option<RemotePath>) -> Self {
        Self {
            title,
            storage,
            needs_home: start.is_none(),
            path: start.unwrap_or_default(),
            generation: Arc::new(AtomicU64::new(0)),
            started: false,
            back: Vec::new(),
            forward: Vec::new(),
            cache: HashMap::new(),
            filter: String::new(),
            sort_key: SortKey::Name,
            ascending: true,
            selection: HashSet::new(),
            anchor: None,
            view: Vec::new(),
            view_stale: true,
            scroll_to: None,
            refresh_at: None,
            focus_filter: false,
            s3: crate::s3::Browse::default(),
            file_ops: Default::default(),
            other_view: None,
        }
    }

    /// Starts in this folder rather than where the connection would land.
    /// For a browser that has not listed anything yet.
    pub fn start_at(&mut self, path: RemotePath) {
        self.needs_home = false;
        self.path = path;
        self.view_stale = true;
    }

    /// The folder being shown, once it is known: before the first listing
    /// of a connection without start folder, it is not.
    pub fn known_path(&self) -> Option<RemotePath> {
        (!self.needs_home).then(|| self.path.clone())
    }

    pub fn storage_key(&self) -> usize {
        storage_key(&self.storage)
    }

    /// Whether files can live in the current folder. The root of an S3
    /// connection only lists buckets.
    pub fn writable(&self) -> bool {
        !(self.path.is_root() && self.storage.has_buckets())
    }

    /// Lands in the home folder reported by the server, without leaving a
    /// trace in the history.
    pub fn set_home(&mut self, path: RemotePath) {
        self.needs_home = false;
        if path != self.path {
            self.cache.remove(&self.path);
            self.path = path;
            self.view_stale = true;
        }
    }

    pub fn listing(&self) -> Option<&Listing> {
        self.cache.get(&self.path)
    }

    /// The listing of the current folder, for changes made in place (folder
    /// sizes). Call [`Session::mark_view_stale`] after changing it.
    pub fn listing_mut(&mut self) -> Option<&mut Listing> {
        self.cache.get_mut(&self.path)
    }

    pub fn can_go_back(&self) -> bool {
        !self.back.is_empty()
    }

    pub fn can_go_forward(&self) -> bool {
        !self.forward.is_empty()
    }

    /// Moves to `path`, recording the current one in the history.
    pub fn go_to(&mut self, path: RemotePath) {
        if path == self.path {
            return;
        }
        let previous = std::mem::replace(&mut self.path, path);
        self.back.push(previous);
        self.forward.clear();
        self.entered();
    }

    pub fn go_back(&mut self) -> bool {
        let Some(path) = self.back.pop() else {
            return false;
        };
        let previous = std::mem::replace(&mut self.path, path);
        self.forward.push(previous);
        self.entered();
        true
    }

    pub fn go_forward(&mut self) -> bool {
        let Some(path) = self.forward.pop() else {
            return false;
        };
        let previous = std::mem::replace(&mut self.path, path);
        self.back.push(previous);
        self.entered();
        true
    }

    fn entered(&mut self) {
        self.filter.clear();
        self.selection.clear();
        self.anchor = None;
        self.scroll_to = Some(0);
        self.refresh_at = None;
        self.view_stale = true;
    }

    /// Call before starting a listing of the current folder. Any listing
    /// still in flight for another folder is abandoned: a half-loaded one is
    /// forgotten, a refresh simply keeps the entries it already had.
    pub fn begin_listing(&mut self) {
        self.cache.retain(|_, listing| {
            if !listing.loading {
                return true;
            }
            listing.loading = false;
            listing.refreshed.take().is_some()
        });

        let listing = self.cache.entry(self.path.clone()).or_default();
        listing.loading = true;
        if listing.error.take().is_none() && !listing.entries.is_empty() {
            listing.refreshed = Some(Vec::new());
        } else {
            listing.entries.clear();
        }
        self.view_stale = true;
    }

    pub fn listed(&mut self, entries: Vec<Entry>, done: bool) {
        let Some(listing) = self.cache.get_mut(&self.path) else {
            return;
        };
        match &mut listing.refreshed {
            Some(refreshed) => {
                refreshed.extend(entries);
                if done {
                    listing.entries = listing.refreshed.take().unwrap_or_default();
                }
            }
            None => listing.entries.extend(entries),
        }
        if done {
            listing.loading = false;
        }
        self.view_stale = true;
    }

    /// Returns the error back when it should be shown as a notice instead,
    /// because the folder still displays its previous content.
    pub fn list_failed(&mut self, error: String) -> Option<String> {
        let listing = self.cache.get_mut(&self.path)?;
        listing.loading = false;
        if listing.refreshed.take().is_some() {
            return Some(error);
        }
        listing.entries.clear();
        listing.error = Some(error);
        self.view_stale = true;
        None
    }

    /// Forgets cached listings below `path`; the folder itself is kept so it
    /// can be refreshed in place.
    pub fn invalidate_below(&mut self, path: &RemotePath) {
        let depth = path.segments().len();
        let current = self.path.clone();
        self.cache.retain(|cached, _| {
            *cached == current
                || cached == path
                || cached.segments().len() <= depth
                || cached.ancestor(depth) != *path
        });
        if *path != current {
            self.cache.remove(path);
        }
    }

    pub fn mark_view_stale(&mut self) {
        self.view_stale = true;
    }

    /// Recomputes the filtered, sorted view if anything changed.
    pub fn refresh_view(&mut self) {
        if !self.view_stale {
            return;
        }
        self.view_stale = false;
        self.view.clear();
        let Some(listing) = self.cache.get(&self.path) else {
            self.selection.clear();
            return;
        };

        let entries = &listing.entries;
        let needle = self.filter.trim().to_lowercase();
        self.view.extend(
            (0..entries.len())
                .filter(|&i| needle.is_empty() || entries[i].name.to_lowercase().contains(&needle)),
        );

        let (key, ascending) = (self.sort_key, self.ascending);
        self.view.sort_by(|&a, &b| {
            let (a, b) = (&entries[a], &entries[b]);
            // Folders always come first, whatever the sort; on this
            // computer, the usual folders come before the drives.
            let drive = |entry: &Entry| entry.kind == EntryKind::Bucket;
            b.is_dir().cmp(&a.is_dir()).then(drive(a).cmp(&drive(b))).then_with(|| {
                let order = match key {
                    SortKey::Name => Ordering::Equal,
                    SortKey::Size => a.size.cmp(&b.size),
                    SortKey::Modified => a.modified.cmp(&b.modified),
                }
                .then_with(|| natural_cmp(&a.name, &b.name));
                if ascending { order } else { order.reverse() }
            })
        });

        if !self.selection.is_empty() {
            let visible: HashSet<&str> = self
                .view
                .iter()
                .map(|&i| entries[i].name.as_str())
                .collect();
            self.selection.retain(|name| visible.contains(name.as_str()));
        }
    }

    pub fn row_count(&self) -> usize {
        self.view.len()
    }

    /// The entry shown on the given row of the view.
    pub fn row(&self, row: usize) -> Option<&Entry> {
        let listing = self.cache.get(&self.path)?;
        listing.entries.get(*self.view.get(row)?)
    }

    pub fn rows(&self) -> impl Iterator<Item = &Entry> {
        let entries = self.cache.get(&self.path).map(|listing| &listing.entries);
        self.view
            .iter()
            .filter_map(move |&i| entries.and_then(|entries| entries.get(i)))
    }

    pub fn row_of(&self, name: &str) -> Option<usize> {
        self.rows().position(|entry| entry.name == name)
    }

    pub fn selected(&self) -> impl Iterator<Item = &Entry> {
        self.rows()
            .filter(|entry| self.selection.contains(&entry.name))
    }

    pub fn select_only(&mut self, name: &str) {
        self.selection.clear();
        self.selection.insert(name.to_owned());
        self.anchor = Some(name.to_owned());
    }

    pub fn toggle(&mut self, name: &str) {
        if !self.selection.remove(name) {
            self.selection.insert(name.to_owned());
        }
        self.anchor = Some(name.to_owned());
    }

    /// Selects everything between the anchor and `row`.
    pub fn select_range_to(&mut self, row: usize) {
        let start = self
            .anchor
            .as_deref()
            .and_then(|name| self.row_of(name))
            .unwrap_or(row);
        let (low, high) = (start.min(row), start.max(row));
        let names: HashSet<String> = (low..=high)
            .filter_map(|i| self.row(i))
            .map(|entry| entry.name.clone())
            .collect();
        self.selection = names;
    }

    pub fn select_all(&mut self) {
        self.selection = self.rows().map(|entry| entry.name.clone()).collect();
    }

    /// Moves a single selection up or down by `delta` rows.
    pub fn move_selection(&mut self, delta: isize) {
        let count = self.row_count();
        if count == 0 {
            return;
        }
        let current = self.anchor.as_deref().and_then(|name| self.row_of(name));
        let target = match current {
            Some(row) => row.saturating_add_signed(delta).min(count - 1),
            None if delta > 0 => 0,
            None => count - 1,
        };
        if let Some(name) = self.row(target).map(|entry| entry.name.clone()) {
            self.select_only(&name);
            self.scroll_to = Some(target);
        }
    }
}
