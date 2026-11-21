//! The rules transfers follow: what to do when the destination exists,
//! speed limits, how many run at once, pausing, folder synchronisation.
//! The worker applies them (see `worker::transfer`); the transfers panel
//! changes them.

pub mod conflict;
pub mod control;
pub mod limit;
#[cfg(test)]
mod live_tests;
pub mod slots;
pub mod sync;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use crate::storage::RemotePath;
use conflict::ConflictPolicy;
use limit::Limiter;
use slots::Slots;

/// How many transfers run side by side unless the user says otherwise.
pub const DEFAULT_PARALLEL: usize = 3;
/// The choice offered in the panel: more than this hurts more than it helps.
pub const MAX_PARALLEL: usize = 10;
/// Below this size, a file of a folder counts as small: it costs round trips
/// rather than bandwidth.
pub const SMALL_FILE: u64 = 1024 * 1024;
/// How many small files of folders run at once, on top of the transfers
/// counted by the user's limit: a folder of a thousand little files would
/// otherwise take a thousand round trips one after the other.
pub const SMALL_PARALLEL: usize = 16;

/// What the user chose about transfers, saved with the other preferences.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TransferSettings {
    /// Bytes per second; 0 for no limit.
    pub download_limit: u64,
    pub upload_limit: u64,
    /// How many transfers run at once; the others wait their turn.
    pub parallel: usize,
    /// What happens when the destination of a transfer exists already.
    pub on_conflict: ConflictPolicy,
    /// The local folder last synchronised with each remote folder, by
    /// [`sync_key`].
    pub sync_folders: BTreeMap<String, String>,
}

impl Default for TransferSettings {
    fn default() -> Self {
        Self {
            download_limit: 0,
            upload_limit: 0,
            parallel: DEFAULT_PARALLEL,
            on_conflict: ConflictPolicy::Ask,
            sync_folders: BTreeMap::new(),
        }
    }
}

/// Identifies a remote folder of a saved connection, to remember which
/// local folder goes with it.
pub fn sync_key(profile_id: &str, remote: &RemotePath) -> String {
    format!("{profile_id}:{remote}")
}

/// Shared by the worker, which applies it, and the UI, which changes it.
pub struct Shared {
    pub slots: Arc<Slots>,
    /// The turns of the small files of folders (see [`SMALL_PARALLEL`]).
    pub small: Arc<Slots>,
    pub download: Arc<Limiter>,
    pub upload: Arc<Limiter>,
    /// The rule for existing destinations, read when a batch is planned.
    policy: Mutex<ConflictPolicy>,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            slots: Slots::new(DEFAULT_PARALLEL),
            small: Slots::new(SMALL_PARALLEL),
            download: Arc::new(Limiter::default()),
            upload: Arc::new(Limiter::default()),
            policy: Mutex::new(ConflictPolicy::Ask),
        }
    }
}

impl Shared {
    /// Applies the saved choices; running transfers follow at once.
    pub fn apply(&self, settings: &TransferSettings) {
        self.download.set_rate(settings.download_limit);
        self.upload.set_rate(settings.upload_limit);
        self.slots.set_max(settings.parallel.clamp(1, MAX_PARALLEL));
        *lock(&self.policy) = settings.on_conflict;
    }

    pub fn policy(&self) -> ConflictPolicy {
        *lock(&self.policy)
    }
}

/// Locks a mutex whose holder may have panicked: the data it guards here
/// stays consistent at every step, so it is still worth using.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
