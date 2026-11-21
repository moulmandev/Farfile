//! What the panel asks of a transfer that is queued, running, paused or
//! failed, and what the transfer tells the panel back.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

use super::lock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    Pause,
    /// Go on after a pause, or try again after a failure.
    Resume,
    Cancel,
    /// Out of the list: the transfer is forgotten.
    Dismiss,
}

#[derive(Default)]
pub struct Control {
    request: Mutex<Option<Request>>,
    notify: Notify,
    /// What it stops at is kept, so that it continues where it was.
    resumable: AtomicBool,
    /// Waiting for the user to say what to do with an existing destination.
    asking: AtomicBool,
    /// Shown in place of the destination: a new name, a resumed copy…
    note: Mutex<Option<String>>,
}

impl Control {
    pub fn send(&self, request: Request) {
        *lock(&self.request) = Some(request);
        // Stored when nobody waits yet, so it is never missed.
        self.notify.notify_one();
    }

    /// Takes the request made since the last call, if any.
    pub fn take(&self) -> Option<Request> {
        lock(&self.request).take()
    }

    /// Waits for the next request.
    pub async fn next(&self) -> Request {
        loop {
            if let Some(request) = self.take() {
                return request;
            }
            self.notify.notified().await;
        }
    }

    pub fn resumable(&self) -> bool {
        self.resumable.load(Ordering::Relaxed)
    }

    pub fn set_resumable(&self, resumable: bool) {
        self.resumable.store(resumable, Ordering::Relaxed);
    }

    pub fn asking(&self) -> bool {
        self.asking.load(Ordering::Relaxed)
    }

    pub fn set_asking(&self, asking: bool) {
        self.asking.store(asking, Ordering::Relaxed);
    }

    pub fn note(&self) -> Option<String> {
        lock(&self.note).clone()
    }

    pub fn set_note(&self, note: impl Into<String>) {
        *lock(&self.note) = Some(note.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_request_made_before_anyone_waits_is_not_lost() {
        let control = Control::default();
        control.send(Request::Pause);
        assert_eq!(control.next().await, Request::Pause);
        // The latest request wins: pausing then cancelling cancels.
        control.send(Request::Pause);
        control.send(Request::Cancel);
        assert_eq!(control.next().await, Request::Cancel);
        assert_eq!(control.take(), None);
    }
}
