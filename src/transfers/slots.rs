//! How many transfers run at once. The others wait their turn in the order
//! they were queued; the limit can change while they wait or run.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;

use super::lock;

pub struct Slots {
    inner: Mutex<Inner>,
}

struct Inner {
    max: usize,
    running: usize,
    waiting: VecDeque<Waiter>,
}

struct Waiter {
    id: u64,
    wake: oneshot::Sender<Slot>,
}

/// The right to run, given back when dropped.
pub struct Slot {
    slots: Option<Arc<Slots>>,
}

/// A place in the queue, taken at once so that transfers keep the order
/// they were asked for in, whenever their tasks get to run.
pub enum Ticket {
    Ready(Slot),
    Waiting(oneshot::Receiver<Slot>),
}

impl Ticket {
    /// Waits for the turn. Giving up (by dropping the future) leaves the
    /// queue.
    pub async fn wait(self) -> Slot {
        match self {
            Self::Ready(slot) => slot,
            Self::Waiting(receiver) => match receiver.await {
                Ok(slot) => slot,
                // Only when the queue itself is gone, which outlives every task.
                Err(_) => std::future::pending().await,
            },
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        if let Some(slots) = self.slots.take() {
            lock(&slots.inner).running -= 1;
            slots.dispatch();
        }
    }
}

impl Slots {
    pub fn new(max: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                max: max.max(1),
                running: 0,
                waiting: VecDeque::new(),
            }),
        })
    }

    /// Changes how many may run. Lowering it stops nobody: the next ones
    /// simply wait longer.
    pub fn set_max(self: &Arc<Self>, max: usize) {
        lock(&self.inner).max = max.max(1);
        self.dispatch();
    }

    /// Takes a place in the queue. `urgent` goes before everything already
    /// waiting: the user is waiting for it, as when opening a file.
    pub fn ticket(self: &Arc<Self>, id: u64, urgent: bool) -> Ticket {
        let mut inner = lock(&self.inner);
        if inner.running < inner.max && inner.waiting.iter().all(|waiter| waiter.wake.is_closed()) {
            inner.running += 1;
            return Ticket::Ready(Slot {
                slots: Some(self.clone()),
            });
        }
        let (wake, receiver) = oneshot::channel();
        let waiter = Waiter { id, wake };
        if urgent {
            inner.waiting.push_front(waiter);
        } else {
            inner.waiting.push_back(waiter);
        }
        Ticket::Waiting(receiver)
    }

    /// Waits for a turn; see [`Slots::ticket`].
    #[cfg(test)]
    pub async fn acquire(self: &Arc<Self>, id: u64, urgent: bool) -> Slot {
        self.ticket(id, urgent).wait().await
    }

    /// Where a waiting transfer stands in the queue, from 1.
    pub fn position(&self, id: u64) -> Option<usize> {
        lock(&self.inner)
            .waiting
            .iter()
            .filter(|waiter| !waiter.wake.is_closed())
            .position(|waiter| waiter.id == id)
            .map(|index| index + 1)
    }

    /// Hands free slots to those waiting, in order.
    fn dispatch(self: &Arc<Self>) {
        loop {
            let waiter = {
                let mut inner = lock(&self.inner);
                if inner.running >= inner.max {
                    return;
                }
                let Some(waiter) = inner.waiting.pop_front() else {
                    return;
                };
                inner.running += 1;
                waiter
            };
            // Sent outside the lock. One that gave up hands the slot back
            // here rather than through its drop, which would recurse.
            let slot = Slot {
                slots: Some(self.clone()),
            };
            if let Err(mut unused) = waiter.wake.send(slot) {
                unused.slots = None;
                lock(&self.inner).running -= 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    async fn settle() {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    #[tokio::test]
    async fn turns_come_in_order_and_follow_the_limit() {
        let slots = Slots::new(2);
        let (first, second) = (slots.acquire(1, false).await, slots.acquire(2, false).await);
        let started = Arc::new(Mutex::new(Vec::new()));
        let mut tasks = Vec::new();
        for id in 3..=6 {
            let (slots, started) = (slots.clone(), started.clone());
            tasks.push(tokio::spawn(async move {
                let slot = slots.acquire(id, false).await;
                lock(&started).push(id);
                slot
            }));
            settle().await;
        }
        assert_eq!(slots.position(3), Some(1));
        assert_eq!(slots.position(6), Some(4));
        assert!(lock(&started).is_empty());

        drop(first);
        settle().await;
        assert_eq!(*lock(&started), [3]);
        assert_eq!(slots.position(4), Some(1));

        // Raising the limit lets more through at once, still in order.
        slots.set_max(4);
        settle().await;
        assert_eq!(*lock(&started), [3, 4, 5]);

        // Lowering it stops nobody, but the next one waits for two to end.
        slots.set_max(1);
        drop(second);
        settle().await;
        assert_eq!(*lock(&started), [3, 4, 5]);
        let mut held: Vec<Slot> = Vec::new();
        for task in tasks.drain(..3) {
            held.push(task.await.unwrap());
        }
        held.truncate(1);
        settle().await;
        assert_eq!(*lock(&started), [3, 4, 5]);
        held.clear();
        settle().await;
        assert_eq!(*lock(&started), [3, 4, 5, 6]);
    }

    #[tokio::test]
    async fn the_urgent_go_first_and_those_who_leave_lose_their_place() {
        let slots = Slots::new(1);
        let running = slots.acquire(1, false).await;
        let order = Arc::new(Mutex::new(Vec::new()));
        let wait = |id, urgent| {
            let (slots, order) = (slots.clone(), order.clone());
            tokio::spawn(async move {
                let _slot = slots.acquire(id, urgent).await;
                lock(&order).push(id);
                settle().await;
            })
        };
        let second = wait(2, false);
        settle().await;
        let leaving = wait(3, false);
        settle().await;
        let urgent = wait(4, true);
        settle().await;
        assert_eq!(slots.position(4), Some(1));

        leaving.abort();
        settle().await;
        assert_eq!(slots.position(2), Some(2), "the one that left no longer counts");

        drop(running);
        let _ = tokio::join!(second, urgent);
        assert_eq!(*lock(&order), [4, 2]);
        assert!(slots.acquire(5, false).await.slots.is_some(), "the slot was handed back");
    }
}
