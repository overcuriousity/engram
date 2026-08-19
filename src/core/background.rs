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

/// The sweep's job target. A constant rather than a corpus id: consolidation
/// looks at the whole collection, and the `UNIQUE(stage, target_id)` on `jobs`
/// then guarantees at most one queued sweep however often the ticker fires.
pub const CONSOLIDATE_TARGET: &str = "collection";

/// Queue a consolidation sweep now and every `interval_hours` after.
///
/// A timer rather than a trigger on write: a sweep after every capture would
/// re-examine the whole collection for one new artifact, and the pairs it finds
/// do not become interesting the instant they are written. The first tick fires
/// immediately, so a restart picks the work up rather than waiting a day.
pub fn spawn_consolidation_ticker(
    core: crate::core::Core,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if !core.consolidate.enabled {
            tracing::info!("consolidation sweep disabled");
            return;
        }
        if !core.synthesizes() {
            tracing::info!("no synthesizer; consolidation judging disabled");
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

/// How often the repair pass runs. A constant rather than a setting: this is
/// what keeps a crashed capture from staying half-finished, which is not a
/// preference, and the one knob it could plausibly borrow —
/// `consolidate.interval_hours` — has no meaning in the configuration this pass
/// exists to survive.
const REPAIR_INTERVAL_HOURS: u64 = 1;

/// How often the two stores are compared. Deliberately not the repair cadence:
/// every other pass is either marker-driven or a walk over SQLite, while this
/// one scrolls the entire vector collection over the network and scans
/// `artifacts` whole, twice. Nothing produces store drift on its own — it takes
/// a crash between two writes, or a restore of one side from a backup — so
/// looking hourly costs a full pass over both stores twenty-four times a day to
/// find, on almost every base, nothing.
const STORE_DRIFT_INTERVAL_HOURS: u64 = 24;

/// Finish what a crash left half-done, now and every `REPAIR_INTERVAL_HOURS`,
/// and compare the two stores every `STORE_DRIFT_INTERVAL_HOURS`.
///
/// Its own ticker, and behind no setting at all. These four passes used to ride
/// on the consolidation sweep, where `consolidate.enabled = false` stopped the
/// ticker before its loop and so stopped them too: a corpus left `segmenting`
/// by a crash stayed stuck, an artifact whose winner was deleted stayed hidden
/// from search and from every page that could put it back, and a lifecycle
/// write that reached one store and not the other stayed torn — with nothing
/// but the sweep to notice, and the sweep switched off. None of that is
/// duplicate hygiene, and none of it is something an operator asks to keep by
/// turning duplicate hygiene off.
pub fn spawn_repair_ticker(
    core: crate::core::Core,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let period = std::time::Duration::from_secs(REPAIR_INTERVAL_HOURS * 3600);
        let mut tick = tokio::time::interval(period);
        // Not from now: start already reconciles the two stores. An interval
        // that fired immediately would scroll the whole collection a second
        // time for an answer the process just computed.
        let drift_period = std::time::Duration::from_secs(STORE_DRIFT_INTERVAL_HOURS * 3600);
        let mut drift_tick =
            tokio::time::interval_at(tokio::time::Instant::now() + drift_period, drift_period);
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { break; }
                }
                _ = tick.tick() => repair_once(&core).await,
                _ = drift_tick.tick() => reconcile_stores_once(&core).await,
            }
        }
        tracing::info!("repair ticker stopped");
    })
}

/// One pass. Each step warns and the next still runs: they are independent, each
/// is retried on the next tick, and each is most likely to fail on exactly the
/// base that needs it most.
pub(crate) async fn repair_once(core: &crate::core::Core) {
    if let Err(e) = crate::jobs::reconcile::run(core).await {
        tracing::warn!(error = %e, "could not finish interrupted captures; retrying on the next pass");
    }
    // A hidden artifact pointing at nothing is invisible to search and to every
    // page that could put it back, and nothing else would notice.
    if let Err(e) = core.heal_dangling_supersessions().await {
        tracing::warn!(
            error = %e,
            "could not restore every artifact whose winner was deleted; retrying on the next pass"
        );
    }
    // The marker pass is cheap, complete for every lifecycle write this system
    // makes, and does not grow with the base.
    if let Err(e) = crate::jobs::consolidate::repair_lifecycle_drift(core).await {
        tracing::warn!(
            error = %e,
            "could not finish interrupted lifecycle writes; retrying on the next pass"
        );
    }
    // Duplicate detection is the per-artifact `Relate` unit, armed when an
    // artifact is indexed. This backstops that arming: an artifact whose unit
    // never got a row — the arm failed after the embed committed — is asked for
    // once here, and the row that leaves is what stops it being asked again.
    //
    // Here rather than on the sweep, where it was, for the reason everything
    // else in this pass is here: arming fails on its own, without the sweep's
    // involvement, and the sweep is behind a setting. An operator who turns
    // duplicate hygiene off is asking for no *judgements*; the artifact that
    // lost its unit to a crash would never be asked about again even after they
    // turned it back on, because a `Relate` row is the only record that it was
    // ever asked.
    match core.store.list_unrelated_artifact_ids(500).await {
        Ok(ids) => {
            for id in ids {
                if let Err(e) = crate::jobs::relate::arm(core, &id, 0).await {
                    tracing::warn!(artifact_id = %id, error = %e, "could not arm a relate unit");
                }
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not look for artifacts that were never related")
        }
    }
}

/// Compare what SQLite says exists against what the vector store holds.
///
/// Split out of `repair_once` and given a much longer period because it is the
/// one repair whose cost is a function of the whole base rather than of what
/// went wrong: a full scroll of the collection and two full scans of
/// `artifacts`, every time, on a base with nothing to fix.
pub(crate) async fn reconcile_stores_once(core: &crate::core::Core) {
    if let Err(e) = core.heal_store_drift().await {
        tracing::warn!(
            error = %e,
            "could not reconcile which artifacts the two stores hold; retrying on the next pass"
        );
    }
}

/// Enforce `feedback.retain_days` now and every `feedback.sweep_hours` after,
/// and regroup the knowledge gaps on the same beat.
///
/// Its own ticker rather than a passenger on the consolidation sweep, which is
/// where it used to live: an operator who switches duplicate hygiene off is not
/// asking to keep their query log forever, and that is what the coupling
/// quietly did. Runs even with capture disabled, so turning capture off also
/// expires what it recorded while it was on. Grouping the gaps rides the same
/// rhythm because it reads the same tables, and hours is the right cadence for
/// something a person looks at when they next capture.
pub fn spawn_retention_ticker(
    core: crate::core::Core,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if core.feedback.retain_days <= 0 {
            tracing::info!("captured searches and questions kept indefinitely");
            // Nothing to expire, and with capture off nothing to group either:
            // the ticker would wake every `sweep_hours` for the rest of the
            // process to do nothing at all. The line above is the whole of what
            // this task has to say, so it says it and stops.
            if !core.feedback.enabled {
                return;
            }
        }
        let period = std::time::Duration::from_secs(core.feedback.sweep_hours.max(1) * 3600);
        let mut tick = tokio::time::interval(period);
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { break; }
                }
                _ = tick.tick() => {
                    if core.feedback.retain_days > 0 {
                        match core.store.expire_feedback(core.feedback.retain_days).await {
                            Ok(n) if n > 0 => {
                                tracing::info!(dropped = n, "expired captured searches and questions")
                            }
                            // A failed sweep is retried on the next tick; there is
                            // nothing here worth taking the process down for.
                            Err(e) => {
                                tracing::warn!(error = %e, "could not expire captured searches")
                            }
                            _ => {}
                        }
                    }
                    if core.feedback.enabled {
                        match crate::jobs::gaps::sweep(&core).await {
                            Ok(r) if r.named > 0 || r.removed > 0 => tracing::info!(
                                clusters = r.clusters, named = r.named, removed = r.removed,
                                "knowledge gaps regrouped"
                            ),
                            Err(e) => tracing::warn!(error = %e, "could not group knowledge gaps"),
                            _ => {}
                        }
                    }
                }
            }
        }
        tracing::info!("retention ticker stopped");
    })
}

/// Arm dedupe units at a steady rate, independent of the consolidation sweep.
///
/// Its own ticker rather than a passenger on that sweep, for the same reason
/// retention got one: the pacing of model calls has nothing to do with the
/// rhythm of duplicate *discovery*. The sweep runs daily because re-examining
/// the whole collection more often buys nothing; the calls want a rate the
/// hardware can actually sustain.
///
/// What it does not do is cap units in flight. A unit the queue cannot get
/// through — a dead endpoint — would then block every other
/// pair permanently, which is the head-of-line blocking the per-pair units were
/// introduced to remove. `live_job` skips a pair whose unit is already queued,
/// and the ordering in `pairs_to_judge` keeps a pair that keeps failing from
/// starving the rest.
pub fn spawn_dedupe_ticker(
    core: crate::core::Core,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if !core.consolidate.enabled || core.consolidate.max_dedupe_per_tick == 0 {
            tracing::info!("dedupe pass disabled");
            return;
        }
        if !core.synthesizes() {
            tracing::info!("no synthesizer; dedupe pass disabled");
            return;
        }
        let period =
            std::time::Duration::from_secs(core.consolidate.dedupe_interval_mins.max(1) * 60);
        let mut tick = tokio::time::interval(period);
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { break; }
                }
                _ = tick.tick() => {
                    match crate::jobs::consolidate::arm_dedupe(&core).await {
                        Ok(n) if n > 0 => tracing::info!(armed = n, "armed dedupe units"),
                        // A failed tick is retried on the next one; there is
                        // nothing here worth taking the process down for.
                        Err(e) => tracing::warn!(error = %e, "could not arm dedupe units"),
                        _ => {}
                    }
                }
            }
        }
        tracing::info!("dedupe ticker stopped");
    })
}

/// The association sweep's job target. A constant rather than an artifact id:
/// the sweep replays the whole log, and `UNIQUE(stage, target_id)` then bounds
/// the queue to one of them however often the ticker fires.
pub const ASSOCIATE_TARGET: &str = "collection";

/// Queue an association sweep now and every `associate.interval_mins` after.
///
/// Its own ticker, like retention and dedupe: the rhythm of replaying a search
/// log has nothing to do with the rhythm of duplicate discovery, and coupling
/// the two is how switching one feature off silently switches another one off.
///
/// Returns before its loop when there is nothing to learn from — either the
/// feature is off, or searches are not being recorded, which is the same thing.
/// Queue the pursuit sweep on a period shorter than the idle window, so a run
/// of searches is grouped soon after it goes quiet.
pub fn spawn_pursuit_ticker(
    core: crate::core::Core,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if !core.pursuit.enabled || !core.associating() {
            tracing::info!("pursuit sweep disabled");
            return;
        }
        let period = std::time::Duration::from_secs((core.pursuit.idle_secs / 2).max(60));
        let mut tick = tokio::time::interval(period);
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { break; }
                }
                _ = tick.tick() => {
                    if let Err(e) = core
                        .store
                        .enqueue(crate::store::jobs::Stage::Pursuit, "collection", ASSOCIATE_TARGET)
                        .await
                    {
                        tracing::warn!(error = %e, "could not queue the pursuit sweep");
                    }
                }
            }
        }
        tracing::info!("pursuit ticker stopped");
    })
}

pub fn spawn_associate_ticker(
    core: crate::core::Core,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if !core.associating() {
            tracing::info!("association sweep disabled");
            return;
        }
        // Saturating: the operand is operator-typed, and a wrap here would turn
        // a very long configured interval into a very short one — a sweep
        // hammering the queue is the opposite of what was asked for.
        let period =
            std::time::Duration::from_secs(core.associate.interval_mins.max(1).saturating_mul(60));
        let mut tick = tokio::time::interval(period);
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { break; }
                }
                _ = tick.tick() => {
                    if let Err(e) = core
                        .store
                        .enqueue(crate::store::jobs::Stage::Associate, "collection", ASSOCIATE_TARGET)
                        .await
                    {
                        tracing::warn!(error = %e, "could not queue the association sweep");
                    }
                }
            }
        }
        tracing::info!("association ticker stopped");
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    #[tokio::test]
    async fn the_ticker_queues_exactly_one_sweep() {
        // `jobs` is unique on (stage, target), so a ticker that fires while a
        // sweep is still queued must collapse onto the same row rather than
        // stacking sweeps behind a slow one.
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
    async fn a_crashed_capture_is_repaired_with_the_sweep_switched_off() {
        // The coupling this ticker exists to break. These passes used to run
        // inside the consolidation sweep, and the sweep's ticker returns before
        // its loop when `enabled` is false — so no sweep was ever queued, the
        // passes never ran, and a corpus left half-segmented by a crash stayed
        // that way for as long as duplicate hygiene was switched off.
        let mut core = crate::core::test_support::test_core().await;
        core.consolidate.enabled = false;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        core.store
            .upsert_segments(
                &src.id,
                &[
                    crate::store::segments::NewSegment {
                        start_line: 1,
                        end_line: 10,
                        text: "first window",
                        carry_lines: 0,
                    },
                    crate::store::segments::NewSegment {
                        start_line: 11,
                        end_line: 20,
                        text: "second window",
                        carry_lines: 0,
                    },
                ],
            )
            .await
            .unwrap();
        core.store
            .set_segment_state(&src.id, 0, crate::store::segments::SegmentState::Done, None)
            .await
            .unwrap();

        repair_once(&core).await;

        let job = core
            .store
            .claim_job()
            .await
            .unwrap()
            .expect("the unfinished window should have been re-armed");
        assert_eq!(job.stage, crate::store::jobs::Stage::SegmentWindow);
        assert_eq!(
            job.target_id,
            crate::jobs::window::unit_target(&src.id, 1),
            "the window that never ran"
        );
    }

    #[tokio::test]
    async fn an_indexed_artifact_that_was_never_related_is_armed_with_the_sweep_off() {
        // The backstop used to sit inside the consolidation sweep, behind its
        // `enabled` check, while the arming it backstops is allowed to fail
        // silently. On a base with duplicate hygiene switched off, an artifact
        // whose `relate::arm` failed after its embed committed was therefore
        // never related — and never would be, because a `Relate` row is the
        // only record that it was ever asked.
        let mut core = crate::core::test_support::test_core().await;
        core.consolidate.enabled = false;
        let ids = crate::jobs::consolidate::tests::seed(&core, &[("alpha", [1.0, 0.0])]).await;
        core.store.mark_embedded(&ids[0], "fake", 0).await.unwrap();

        repair_once(&core).await;

        assert!(
            core.store
                .live_job(crate::store::jobs::Stage::Relate, &ids[0])
                .await
                .unwrap(),
            "the backstop did not arm a relate unit"
        );
        // A second pass does not arm it again: the row now exists.
        let first = core.store.claim_job().await.unwrap().expect("the unit");
        assert_eq!(first.stage, crate::store::jobs::Stage::Relate);
        core.store.complete_job(first.id).await.unwrap();
        repair_once(&core).await;
        assert!(
            !core
                .store
                .live_job(crate::store::jobs::Stage::Relate, &ids[0])
                .await
                .unwrap()
        );
    }

    /// One captured search, aged past any plausible window.
    async fn seed_old_event(core: &crate::core::Core) {
        let id = core
            .store
            .record_search(
                crate::store::feedback::NewEvent {
                    query: "old".into(),
                    door: crate::store::feedback::Door::Ui,
                    scope: None,
                    filters: "{}".into(),
                    query_vec: vec![0.0],
                    embed_model: "fake".into(),
                    candidates: vec![],
                    answered: false,
                },
                0,
            )
            .await
            .unwrap();
        sqlx::query("UPDATE search_events SET created_at = ? WHERE id = ?")
            .bind(crate::store::now() - 40 * 86_400)
            .bind(&id)
            .execute(&core.store.pool)
            .await
            .unwrap();
    }

    async fn captured(core: &crate::core::Core) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM search_events")
            .fetch_one(&core.store.pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn retention_runs_with_consolidation_switched_off() {
        // Retention used to ride on the consolidation sweep, so switching
        // duplicate hygiene off silently kept the query log forever.
        let mut core = crate::core::test_support::test_core().await;
        core.consolidate.enabled = false;
        core.feedback.enabled = true;
        core.feedback.retain_days = 30;
        seed_old_event(&core).await;

        let (tx, rx) = tokio::sync::watch::channel(false);
        let h = spawn_retention_ticker(core.clone(), rx);
        for _ in 0..50 {
            if captured(&core).await == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let _ = tx.send(true);
        let _ = h.await;

        assert_eq!(
            captured(&core).await,
            0,
            "an event past the window outlived the retention ticker"
        );
    }

    #[tokio::test]
    async fn keeping_forever_expires_nothing() {
        let core = crate::core::test_support::test_core().await; // retain_days defaults to 0
        seed_old_event(&core).await;
        let (tx, rx) = tokio::sync::watch::channel(false);
        let h = spawn_retention_ticker(core.clone(), rx);
        // Let the first tick land, then stop it. The ticker keeps running for
        // the gap sweep, so it is stopped rather than awaited to its end.
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        let _ = tx.send(true);
        let _ = h.await;
        assert_eq!(captured(&core).await, 1, "`0` must keep them forever");
    }

    /// Nothing to expire and, with capture off, nothing to group either. The
    /// task used to return here; it lost the `return` when the gap sweep moved
    /// in, and went on waking every `sweep_hours` for the life of the process to
    /// do nothing at all.
    #[tokio::test]
    async fn a_ticker_with_nothing_to_do_stops_instead_of_idling() {
        let mut core = crate::core::test_support::test_core().await;
        core.feedback.retain_days = 0;
        core.feedback.enabled = false;

        let (_tx, rx) = tokio::sync::watch::channel(false);
        let h = spawn_retention_ticker(core.clone(), rx);
        // No shutdown is sent: it has to end on its own account.
        tokio::time::timeout(Duration::from_secs(2), h)
            .await
            .expect("the ticker sat waiting for a tick it has no work for")
            .unwrap();
    }

    #[tokio::test]
    async fn the_ticker_groups_the_gaps_on_its_first_tick() {
        let mut core = crate::core::test_support::test_core().await;
        core.feedback.enabled = true;
        // Two of them: one gap is not a group, and the sweep no longer spends a
        // naming call restating a single question.
        for q in ["mount an E01", "mounting E01 images"] {
            let id = core
                .store
                .record_ask(crate::store::asks::NewAsk {
                    question: q.into(),
                    scope: None,
                    filters: "{}".into(),
                    query_vec: vec![1.0; 4],
                    embed_model: core.embedder.model().to_string(),
                    answer: "Not in the knowledge base.".into(),
                    abstained: true,
                    dropped: 0,
                    truncated: false,
                    citations: vec![],
                })
                .await
                .unwrap();
            core.store
                .judge_ask(&id, crate::store::asks::AskVerdict::NothingHere)
                .await
                .unwrap();
        }

        let (tx, rx) = tokio::sync::watch::channel(false);
        let h = spawn_retention_ticker(core.clone(), rx);
        for _ in 0..50 {
            if !core.store.cluster_keys().await.unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let _ = tx.send(true);
        let _ = h.await;
        assert_eq!(
            core.store.cluster_keys().await.unwrap().len(),
            1,
            "the gap was never grouped"
        );
    }

    #[tokio::test]
    async fn the_ticker_queues_a_sweep_as_soon_as_it_starts() {
        // `tokio::time::interval` fires immediately on its first tick, which is
        // what makes a restart pick consolidation up rather than waiting a day.
        let core = crate::core::test_support::test_core().await;
        let (tx, rx) = tokio::sync::watch::channel(false);
        let h = spawn_consolidation_ticker(core.clone(), rx);

        // Give the first tick a chance to land, then stop the ticker.
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
    async fn the_association_sweep_never_stacks_up_in_the_queue() {
        // `jobs` is unique on (stage, target), so a ticker firing while a
        // sweep is still queued must collapse onto the same row rather than
        // stacking sweeps behind a slow one.
        let core = crate::core::test_support::test_core().await;
        for _ in 0..3 {
            core.store
                .enqueue(
                    crate::store::jobs::Stage::Associate,
                    "collection",
                    ASSOCIATE_TARGET,
                )
                .await
                .unwrap();
        }
        let mut seen = 0;
        while let Some(j) = core.store.claim_job().await.unwrap() {
            assert_eq!(j.stage, crate::store::jobs::Stage::Associate);
            seen += 1;
        }
        assert_eq!(seen, 1, "the sweep stacked up in the queue");
    }

    #[tokio::test]
    async fn the_association_ticker_queues_a_sweep_as_soon_as_it_starts() {
        // `tokio::time::interval` fires immediately on its first tick, which
        // is what makes a restart pick the association sweep up rather than
        // waiting out a full `interval_mins`.
        let mut core = crate::core::test_support::test_core().await;
        core.feedback.enabled = true;
        let (tx, rx) = tokio::sync::watch::channel(false);
        let h = spawn_associate_ticker(core.clone(), rx);
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
        assert_eq!(j.stage, crate::store::jobs::Stage::Associate);
        assert_eq!(j.target_id, ASSOCIATE_TARGET);
        assert!(core.store.claim_job().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn no_recorded_searches_means_no_association_ticker_at_all() {
        // `associate.enabled` without `feedback.enabled` is a warning at startup
        // and nothing else: there is nothing to learn from.
        let core = crate::core::test_support::test_core().await; // feedback off
        let (_tx, rx) = tokio::sync::watch::channel(false);
        // Returns rather than looping, so awaiting it cannot hang.
        let _ = spawn_associate_ticker(core.clone(), rx).await;
        assert!(core.store.claim_job().await.unwrap().is_none());
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
