//! Detached writes that still have to finish.
//!
//! Some work must not sit on the request path but must not be lost either.
//! Recording which chunks a search showed is the case that exists today: a
//! search must not get slower, or fail, because a bookkeeping write did — and
//! yet a stamp dropped at shutdown is a chunk that looks forgotten when it is
//! not.
//!
//! `tokio::spawn` alone gives the first half and not the second: the runtime
//! drops outstanding tasks when `main` returns. This counts them, so shutdown
//! can wait, and so tests can await the effect instead of sleeping and hoping.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;

#[derive(Default)]
pub struct Background {
    inflight: AtomicUsize,
    idle: Notify,
}

/// Decrements on drop rather than after the await, so a task that panics or is
/// cancelled releases its slot. Decrementing at the end of the future body
/// would wedge every later shutdown behind a task that is already gone.
struct Slot(Arc<Background>);

impl Drop for Slot {
    fn drop(&mut self) {
        if self.0.inflight.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.idle.notify_waiters();
        }
    }
}

impl Background {
    /// Run `task` detached, counted until it finishes.
    ///
    /// The count is incremented here rather than inside the spawned future, so
    /// a `wait_idle` racing a `spawn` cannot observe zero for work that has
    /// already been handed over.
    pub fn spawn<F>(self: &Arc<Self>, task: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.inflight.fetch_add(1, Ordering::SeqCst);
        let slot = Slot(self.clone());
        tokio::spawn(async move {
            let _slot = slot;
            task.await;
        });
    }

    pub fn inflight(&self) -> usize {
        self.inflight.load(Ordering::SeqCst)
    }

    /// Resolve once nothing is outstanding.
    ///
    /// Waiting for work spawned *after* this returns is not the contract:
    /// callers stop accepting requests first, then drain.
    pub async fn wait_idle(&self) {
        loop {
            // Registered before the count is read, because `notify_waiters`
            // wakes only what is already waiting. Checking first would lose a
            // task that finished in between and wait for a wake-up that has
            // already happened.
            let notified = self.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if self.inflight() == 0 {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    #[tokio::test]
    async fn waiting_resolves_after_the_task_has_actually_run() {
        let bg = Arc::new(Background::default());
        let done = Arc::new(AtomicBool::new(false));

        let flag = done.clone();
        bg.spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            flag.store(true, Ordering::SeqCst);
        });

        bg.wait_idle().await;
        assert!(
            done.load(Ordering::SeqCst),
            "shutdown returned while the write was still in flight"
        );
    }

    #[tokio::test]
    async fn waiting_on_nothing_returns_immediately() {
        let bg = Arc::new(Background::default());
        bg.wait_idle().await;
        assert_eq!(bg.inflight(), 0);
    }

    #[tokio::test]
    async fn every_spawned_task_is_waited_for() {
        let bg = Arc::new(Background::default());
        let count = Arc::new(AtomicUsize::new(0));
        for i in 0..50 {
            let c = count.clone();
            bg.spawn(async move {
                // Staggered, so a wait that resolves early is caught rather
                // than passing because everything happened to finish at once.
                tokio::time::sleep(Duration::from_millis(i % 7)).await;
                c.fetch_add(1, Ordering::SeqCst);
            });
        }
        bg.wait_idle().await;
        assert_eq!(count.load(Ordering::SeqCst), 50);
    }

    #[tokio::test]
    async fn a_panicking_task_does_not_wedge_shutdown() {
        // A background write that panics must still release its slot, or every
        // later shutdown hangs waiting for a task that is already gone.
        let bg = Arc::new(Background::default());
        bg.spawn(async { panic!("the write failed") });

        tokio::time::timeout(Duration::from_secs(5), bg.wait_idle())
            .await
            .expect("wait_idle hung after a task panicked");
        assert_eq!(bg.inflight(), 0);
    }

    #[tokio::test]
    async fn waiting_twice_is_allowed() {
        let bg = Arc::new(Background::default());
        bg.spawn(async {});
        bg.wait_idle().await;
        bg.spawn(async {});
        bg.wait_idle().await;
        assert_eq!(bg.inflight(), 0);
    }
}
