use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;

#[derive(Default)]
pub struct Background {
    inflight: AtomicUsize,
    idle: Notify,
}

struct Slot(Arc<Background>);

impl Drop for Slot {
    fn drop(&mut self) {
        if self.0.inflight.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.idle.notify_waiters();
        }
    }
}

impl Background {
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

    pub async fn wait_idle(&self) {
        loop {
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

pub const CONSOLIDATE_TARGET: &str = "collection";

pub fn spawn_consolidation_ticker(
    core: crate::core::Core,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if !core.consolidate.enabled {
            tracing::info!("consolidation sweep disabled");
            return;
        }
        let period = std::time::Duration::from_secs(core.consolidate.interval_hours.max(1) * 3600);
        let mut tick = tokio::time::interval(period);
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { break; }
                }
                _ = tick.tick() => {
                    if let Err(e) = core
                        .store
                        .enqueue(
                            crate::store::jobs::Stage::Consolidate,
                            "collection",
                            CONSOLIDATE_TARGET,
                        )
                        .await
                    {
                        tracing::warn!(error = %e, "could not queue the consolidation sweep");
                    }
                }
            }
        }
        tracing::info!("consolidation ticker stopped");
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    #[tokio::test]
    async fn the_ticker_queues_exactly_one_sweep() {
        let core = crate::core::test_support::test_core().await;
        for _ in 0..3 {
            core.store
                .enqueue(
                    crate::store::jobs::Stage::Consolidate,
                    "collection",
                    CONSOLIDATE_TARGET,
                )
                .await
                .unwrap();
        }
        let mut seen = 0;
        while let Some(j) = core.store.claim_job().await.unwrap() {
            assert_eq!(j.stage, crate::store::jobs::Stage::Consolidate);
            seen += 1;
        }
        assert_eq!(seen, 1, "the sweep stacked up in the queue");
    }

    #[tokio::test]
    async fn a_disabled_sweep_is_never_queued() {
        let mut core = crate::core::test_support::test_core().await;
        core.consolidate.enabled = false;
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let h = spawn_consolidation_ticker(core.clone(), rx);
        let _ = h.await;
        assert!(
            core.store.claim_job().await.unwrap().is_none(),
            "a disabled sweep must not be queued"
        );
    }

    #[tokio::test]
    async fn the_ticker_queues_a_sweep_as_soon_as_it_starts() {
        let core = crate::core::test_support::test_core().await;
        let (tx, rx) = tokio::sync::watch::channel(false);
        let h = spawn_consolidation_ticker(core.clone(), rx);

        for _ in 0..50 {
            if core
                .store
                .job_counts()
                .await
                .unwrap()
                .iter()
                .any(|(_, n)| *n > 0)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        let _ = tx.send(true);
        let _ = h.await;

        let j = core
            .store
            .claim_job()
            .await
            .unwrap()
            .expect("the ticker queued nothing");
        assert_eq!(j.stage, crate::store::jobs::Stage::Consolidate);
        assert_eq!(j.target_id, CONSOLIDATE_TARGET);
    }

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
                tokio::time::sleep(Duration::from_millis(i % 7)).await;
                c.fetch_add(1, Ordering::SeqCst);
            });
        }
        bg.wait_idle().await;
        assert_eq!(count.load(Ordering::SeqCst), 50);
    }

    #[tokio::test]
    async fn a_panicking_task_does_not_wedge_shutdown() {
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
