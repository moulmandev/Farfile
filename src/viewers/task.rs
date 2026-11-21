//! Background jobs of the viewers. They run on the worker runtime and hand
//! their results over through shared slots the UI looks at on each frame,
//! so that nothing here needs an event of its own.

use std::future::Future;
use std::sync::{Arc, Mutex};

use tokio::task::JoinHandle;

use crate::worker::Worker;

/// Starts jobs on the worker runtime and wakes the UI when they report.
#[derive(Clone)]
pub struct Spawner {
    runtime: tokio::runtime::Handle,
    ctx: egui::Context,
}

impl Spawner {
    pub fn new(worker: &Worker) -> Self {
        Self {
            runtime: worker.within(tokio::runtime::Handle::current),
            ctx: worker.ctx(),
        }
    }

    /// Jobs on the runtime of a test, which a worker could not be dropped in.
    #[cfg(test)]
    pub fn on(runtime: tokio::runtime::Handle) -> Self {
        Self {
            runtime,
            ctx: egui::Context::default(),
        }
    }

    pub fn ctx(&self) -> &egui::Context {
        &self.ctx
    }

    pub fn spawn(&self, job: impl Future<Output = ()> + Send + 'static) -> Job {
        Job(self.runtime.spawn(job))
    }

    /// Runs `job` and keeps what it returns for [`Task::take`].
    pub fn task<T: Send + 'static>(&self, job: impl Future<Output = T> + Send + 'static) -> Task<T> {
        let slot = Arc::new(Mutex::new(None));
        let (filled, ctx) = (slot.clone(), self.ctx.clone());
        let job = self.spawn(async move {
            let outcome = job.await;
            if let Ok(mut slot) = filled.lock() {
                *slot = Some(outcome);
            }
            ctx.request_repaint();
        });
        Task { slot, _job: job }
    }
}

/// A running job. Dropping it stops the job: once nobody looks at what it
/// would bring, a read, a search or a follower has no reason to go on.
pub struct Job(JoinHandle<()>);

impl Drop for Job {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A job that ends with a result.
pub struct Task<T> {
    slot: Arc<Mutex<Option<T>>>,
    _job: Job,
}

impl<T> Task<T> {
    /// The result, once, as soon as the job is over.
    pub fn take(&self) -> Option<T> {
        self.slot.lock().ok()?.take()
    }
}
