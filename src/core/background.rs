//! Detached writes that still have to finish: counted, so shutdown and tests can wait for them.
//! `tokio::spawn` alone drops outstanding tasks when `main` returns.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;

#[derive(Default)]
pub struct Background {
    inflight: AtomicUsize,
    idle: Notify,
}

/// Decrements on drop, so a task that panics or is cancelled still releases its slot.
struct Slot(Arc<Background>);

impl Drop for Slot {
    fn drop(&mut self) {
        if self.0.inflight.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.idle.notify_waiters();
        }
    }
}

impl Background {
    /// Counted here rather than inside the future, so a racing `wait_idle` never sees zero for
    /// handed-over work.
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

    /// Work spawned after this returns is not covered: callers stop accepting requests first.
    pub async fn wait_idle(&self) {
        loop {
            // Registered before the count is read: `notify_waiters` wakes only what is already
            // waiting.
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

/// `UNIQUE(stage, target_id)` on `jobs` then keeps at most one queued sweep.
pub const CONSOLIDATE_TARGET: &str = "collection";

/// Every periodic sweep that is switched on; each re-arms itself `run_after` one period out.
/// Each gate is the sweep's own, so switching one feature off never stops another's pass.
pub fn periodic_units(core: &crate::core::Core) -> Vec<(crate::store::jobs::Stage, &'static str)> {
    use crate::store::jobs::Stage;
    let mut out = Vec::new();
    if core.consolidate.enabled {
        out.push((Stage::Consolidate, CONSOLIDATE_TARGET));
        // Arming the judging without a judge is a treadmill: `dedupe::run` returns `Ok(())` and the
        // pair stays `Pending`.
        if core.consolidate.max_dedupe_per_tick > 0 && core.judge.is_some() {
            out.push((Stage::ArmDedupe, CONSOLIDATE_TARGET));
        }
    }
    // Not behind consolidation, and `recommend.enabled` rather than `recommends()`:
    // switching learning off is when the recorded situations most need to keep ageing out.
    if core.feedback.retain_days > 0 || core.learn.enabled || core.recommend.enabled {
        out.push((Stage::Retention, CONSOLIDATE_TARGET));
    }
    if core.recommends() {
        out.push((Stage::Context, CONSOLIDATE_TARGET));
    }
    // `acts_on_corpus()`, not `may_act()` (always true below `full`): burial destroys text,
    // and `jobs::retract`, which undoes it, is itself gated on `acts_on_corpus()`.
    if core.reap.enabled && core.judge.is_some() && core.evolve.autonomous.acts_on_corpus() {
        out.push((Stage::Reap, CONSOLIDATE_TARGET));
    }
    if core.associating() {
        out.push((Stage::Associate, ASSOCIATE_TARGET));
        // Behind the generator: a pursuit reaching `Generate` with no `[infer.generate]` would stay
        // `open` for ever.
        if core.generator.is_some() {
            out.push((Stage::Pursuit, ASSOCIATE_TARGET));
        }
    }
    out
}

/// `None` when switched off. Saturating: the interval is operator-typed and a wrap would shorten
/// it.
pub fn periodic_period(
    core: &crate::core::Core,
    stage: crate::store::jobs::Stage,
) -> Option<std::time::Duration> {
    use crate::store::jobs::Stage;
    if !periodic_units(core).iter().any(|(s, _)| *s == stage) {
        return None;
    }
    let secs = match stage {
        Stage::Consolidate => core.consolidate.interval_hours.max(1).saturating_mul(3600),
        Stage::ArmDedupe => core
            .consolidate
            .dedupe_interval_mins
            .max(1)
            .saturating_mul(60),
        Stage::Retention => core.feedback.sweep_hours.max(1).saturating_mul(3600),
        Stage::Context => crate::jobs::context::INTERVAL_HOURS.saturating_mul(3600),
        Stage::Associate => core.associate.interval_mins.max(1).saturating_mul(60),
        Stage::Reap => core.reap.interval_mins.max(1).saturating_mul(60),
        // Shorter than the idle window, so a run of searches is grouped soon after it goes quiet.
        Stage::Pursuit => (core.pursuit.idle_secs / 2).max(60),
        _ => return None,
    };
    Some(std::time::Duration::from_secs(secs))
}

/// Arm every periodic unit that is not live, to run now; this also arms them at boot.
/// A unit that died between claim and re-arm would otherwise never run again.
pub(crate) async fn arm_missing_periodic(core: &crate::core::Core) {
    use crate::jobs::remind::REMIND_TARGET;
    use crate::store::jobs::Stage;
    for (stage, target) in periodic_units(core) {
        match core.store.live_job(stage, target).await {
            Ok(true) => {}
            Ok(false) => {
                if let Err(e) = core
                    .store
                    .arm_periodic(stage, "collection", target, 0)
                    .await
                {
                    tracing::warn!(stage = stage.as_str(), error = %e, "could not arm a sweep");
                }
            }
            Err(e) => {
                tracing::warn!(stage = stage.as_str(), error = %e, "could not look for a sweep")
            }
        }
    }
    // `Stage::Remind` is not periodic, so the loop above cannot reach it; arm it only when missing,
    // because `arm_at` resets `attempts` and would undo a failing channel's backoff.
    match core.store.live_job(Stage::Remind, REMIND_TARGET).await {
        Ok(true) => {}
        Ok(false) => {
            if let Err(e) = core.store.rearm_remind().await {
                tracing::warn!(error = %e, "could not re-arm the reminder unit");
            }
        }
        Err(e) => tracing::warn!(error = %e, "could not look for the reminder unit"),
    }
}

/// A constant: it keeps a crashed capture from staying half-finished, which is not a preference.
const REPAIR_INTERVAL_HOURS: u64 = 1;

/// Not the repair cadence: this scrolls the whole vector collection and scans `artifacts` twice.
const STORE_DRIFT_INTERVAL_HOURS: u64 = 24;

/// Background units promoted per tenant per repair tick, so ageing never puts a whole backlog
/// in front of something a person just pasted.
const AGE_PER_TICK: i64 = 20;

/// Behind no setting: these repairs must run with every sweep switched off. Iterates registered
/// `users`, since an untouched tenant is the one whose interrupted work nothing else finds.
pub fn spawn_repair_ticker(
    tenants: std::sync::Arc<crate::tenants::Tenants>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let period = std::time::Duration::from_secs(REPAIR_INTERVAL_HOURS * 3600);
        let mut tick = tokio::time::interval(period);
        // Not from now: a tenant already reconciles its stores when first opened, so firing at once
        // would scroll twice.
        let drift_period = std::time::Duration::from_secs(STORE_DRIFT_INTERVAL_HOURS * 3600);
        let mut drift_tick =
            tokio::time::interval_at(tokio::time::Instant::now() + drift_period, drift_period);
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { break; }
                }
                _ = tick.tick() => repair_every_tenant(&tenants).await,
                _ = drift_tick.tick() => {
                    for subject in every_subject(&tenants).await {
                        if let Some(core) = open_for_pass(&tenants, &subject).await {
                            reconcile_stores_once(&core).await;
                        }
                    }
                }
            }
        }
        tracing::info!("repair ticker stopped");
    })
}

/// One base is open at a time, so the walk's cost does not grow with the number of users.
pub(crate) async fn repair_every_tenant(tenants: &crate::tenants::Tenants) {
    repair_control_once(tenants.control()).await;
    let older_than = age_threshold(tenants.config());
    for subject in every_subject(tenants).await {
        // Before the open: ageing concerns that subject's rows in the control database.
        match tenants
            .control()
            .age_background(&subject, older_than, AGE_PER_TICK)
            .await
        {
            Ok(n) if n > 0 => {
                tracing::info!(subject = %subject, aged = n, "background units have waited long enough")
            }
            Err(e) => {
                tracing::warn!(subject = %subject, error = %e, "could not age the units that have been waiting")
            }
            _ => {}
        }
        if let Some(core) = open_for_pass(tenants, &subject).await {
            repair_once(&core).await;
        }
    }
}

/// Registered users, not the open registry: an untouched base is the one with interrupted work
/// nobody finds.
async fn every_subject(tenants: &crate::tenants::Tenants) -> Vec<String> {
    match tenants.control().users().await {
        Ok(u) => u.into_iter().map(|u| u.subject).collect(),
        Err(e) => {
            tracing::warn!(error = %e, "could not list the users to repair; retrying on the next pass");
            Vec::new()
        }
    }
}

/// `get_transient`, not `get`, so the walk does not evict the registry's active tenants.
/// An open that fails is logged and skipped; the next tick tries again.
async fn open_for_pass(
    tenants: &crate::tenants::Tenants,
    subject: &str,
) -> Option<crate::core::Core> {
    match tenants.get_transient(subject).await {
        Ok(t) => Some(t.core),
        Err(e) => {
            tracing::warn!(subject = %subject, error = %e, "could not open a base to repair it");
            None
        }
    }
}

/// Saturating: `age_after_mins` is operator-typed. `0` does not switch ageing off; it ages
/// everything.
fn age_threshold(cfg: &crate::config::Config) -> i64 {
    crate::store::now().saturating_sub(cfg.schedule.age_after_mins.max(0).saturating_mul(60))
}

/// Repairs over the whole control database, run once per tick rather than per tenant.
pub(crate) async fn repair_control_once(control: &crate::store::control::Control) {
    match control.reclaim_stuck(crate::jobs::STUCK_AFTER_SECS).await {
        Ok(n) if n > 0 => tracing::info!(
            reclaimed = n,
            "requeued jobs left running by a dead process"
        ),
        Err(e) => tracing::warn!(error = %e, "could not requeue jobs left running"),
        _ => {}
    }
    use tower_sessions::ExpiredDeletion;
    if let Err(e) = tower_sessions_sqlx_store::SqliteStore::new(control.pool.clone())
        .delete_expired()
        .await
    {
        tracing::warn!(error = %e, "could not remove expired sessions");
    }
}

/// Each step warns and the next still runs; each is retried on the next tick.
pub(crate) async fn repair_once(core: &crate::core::Core) {
    if let Err(e) = crate::jobs::reconcile::run(core).await {
        tracing::warn!(error = %e, "could not finish interrupted captures; retrying on the next pass");
    }
    // A hidden artifact pointing at nothing is invisible to search and to every page that could
    // restore it.
    if let Err(e) = core.heal_dangling_supersessions().await {
        tracing::warn!(
            error = %e,
            "could not restore every artifact whose winner was deleted; retrying on the next pass"
        );
    }
    if let Err(e) = crate::jobs::consolidate::repair_lifecycle_drift(core).await {
        tracing::warn!(
            error = %e,
            "could not finish interrupted lifecycle writes; retrying on the next pass"
        );
    }
    // Backstops `Relate` arming that failed after the embed committed; outside the sweep, which is
    // behind a setting.
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
    // The one failure mode a ticker does not have, and why this pass stays outside the schedule.
    arm_missing_periodic(core).await;
    // Measured before the passes that read it; a constant cannot know which embedder is configured.
    match core.calibrate().await {
        Ok(line) => tracing::debug!(line, "measured where unrelated stops"),
        Err(e) => tracing::warn!(error = %e, "could not measure the weak line; the floors stand"),
    }
    // `gap_id` names one of three tables, so it is not a foreign key and nothing cascades onto it.
    match core
        .store
        .trim_gap_coverage(crate::jobs::gaps::cover_line(core))
        .await
    {
        Ok(n) if n > 0 => tracing::info!(
            dropped = n,
            "dropped coverage of gaps that are gone or under the line"
        ),
        Err(e) => tracing::warn!(error = %e, "could not drop coverage of gaps that are gone"),
        _ => {}
    }
    match core.store.trim_sweep_runs().await {
        Ok(n) if n > 0 => tracing::info!(dropped = n, "trimmed the sweep history"),
        Err(e) => tracing::warn!(error = %e, "could not trim the sweep history"),
        _ => {}
    }
}

/// Given a much longer period: its cost is a scroll of the whole collection, even with nothing to
/// fix.
pub(crate) async fn reconcile_stores_once(core: &crate::core::Core) {
    if let Err(e) = core.heal_store_drift().await {
        tracing::warn!(
            error = %e,
            "could not reconcile which artifacts the two stores hold; retrying on the next pass"
        );
    }
}

/// `UNIQUE(stage, target_id)` then bounds the queue to one sweep.
pub const ASSOCIATE_TARGET: &str = "collection";

#[cfg(test)]
mod tests {

    #[tokio::test]
    async fn dedupe_judging_is_not_armed_on_a_base_with_no_judge() {
        use crate::store::jobs::Stage;
        let mut core = crate::core::test_support::test_core().await;
        core.consolidate.enabled = true;
        core.consolidate.max_dedupe_per_tick = 5;
        let armed = |core: &crate::core::Core| {
            super::periodic_units(core)
                .into_iter()
                .map(|(s, _)| s)
                .collect::<Vec<_>>()
        };
        assert!(
            armed(&core).contains(&Stage::Consolidate),
            "the sweep that finds pairs runs either way"
        );
        assert!(
            armed(&core).contains(&Stage::ArmDedupe),
            "and with a judge, so does the judging"
        );
        assert!(
            super::periodic_period(&core, Stage::ArmDedupe).is_some(),
            "with a period to go with it"
        );

        core.judge = None;
        assert!(armed(&core).contains(&Stage::Consolidate));
        assert!(!armed(&core).contains(&Stage::ArmDedupe));
        assert!(super::periodic_period(&core, Stage::ArmDedupe).is_none());
    }

    #[tokio::test]
    async fn a_repair_tick_requeues_stuck_work_for_every_tenant() {
        let (tenants, a, b, _dir) = crate::tenants::test_support::two_tenants().await;
        for t in [&a, &b] {
            t.core
                .store
                .enqueue(crate::store::jobs::Stage::Embed, "corpus", "c1")
                .await
                .unwrap();
        }
        // Both claimed by a process that then died: `running`, claimed long
        // enough ago that nothing is coming back for them.
        sqlx::query!(
            "UPDATE jobs SET state = 'running', claimed_at = ?",
            crate::store::now() - crate::jobs::STUCK_AFTER_SECS - 60
        )
        .execute(&tenants.control().pool)
        .await
        .unwrap();

        repair_control_once(tenants.control()).await;

        let pending: i64 = sqlx::query_scalar!("SELECT count(*) FROM jobs WHERE state = 'pending'")
            .fetch_one(&tenants.control().pool)
            .await
            .unwrap();
        assert_eq!(pending, 2, "one tenant's stuck work was left running");
    }

    #[tokio::test]
    async fn every_tenant_gets_its_own_ageing_budget() {
        let (tenants, a, b, _dir) = crate::tenants::test_support::two_tenants().await;
        // `a`'s units are all older than `b`'s, which under one shared budget let `a` spend all of
        // it.
        for (t, base) in [(&a, 10_000i64), (&b, 5_000)] {
            for i in 0..(AGE_PER_TICK + 5) {
                t.core
                    .store
                    .enqueue(
                        crate::store::jobs::Stage::Dedupe,
                        "artifact",
                        &format!("x{i}"),
                    )
                    .await
                    .unwrap();
            }
            sqlx::query!(
                "UPDATE jobs SET created_at = ? WHERE subject = ?",
                crate::store::now() - base,
                &t.user.subject
            )
            .execute(&tenants.control().pool)
            .await
            .unwrap();
        }

        repair_every_tenant(&tenants).await;

        for t in [&a, &b] {
            let promoted: i64 = sqlx::query_scalar!(
                "SELECT count(*) FROM jobs WHERE subject = ? AND class = 0",
                &t.user.subject
            )
            .fetch_one(&tenants.control().pool)
            .await
            .unwrap();
            assert_eq!(
                promoted, AGE_PER_TICK,
                "{} got {promoted} of its own budget of {AGE_PER_TICK}",
                t.user.subject
            );
        }
    }

    #[tokio::test]
    async fn a_repair_pass_does_not_evict_the_tenant_somebody_is_using() {
        let (tenants, _dir) = crate::tenants::test_support::test_tenants(1).await;
        for s in ["sub-a", "sub-b", "sub-c"] {
            tenants.get_or_provision(s, None).await.unwrap();
        }
        // `sub-a` is the first user the walk reaches, so a pass that went through `get` would evict
        // it.
        tenants.get("sub-a").await.unwrap();
        assert!(tenants.is_open("sub-a"));

        repair_every_tenant(&tenants).await;

        assert_eq!(
            tenants.open_count(),
            1,
            "the pass left more bases open than the cap allows"
        );
        assert!(
            tenants.is_open("sub-a"),
            "the pass evicted the tenant in use in favour of one it walked past"
        );
    }
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    #[tokio::test]
    async fn the_reap_sweep_is_armed_only_when_enabled_a_judge_exists_and_it_may_act() {
        use crate::store::jobs::Stage;
        let mut core = crate::core::test_support::test_core().await;
        core.evolve.autonomous = crate::config::Autonomy::Full;
        assert!(
            periodic_units(&core).iter().any(|(s, _)| *s == Stage::Reap),
            "on when a judge is configured and the base may act on the corpus"
        );
        assert_eq!(
            periodic_period(&core, Stage::Reap),
            Some(Duration::from_secs(1440 * 60))
        );

        let mut off = crate::core::test_support::test_core().await;
        off.evolve.autonomous = crate::config::Autonomy::Full;
        off.reap.enabled = false;
        assert!(
            !periodic_units(&off).iter().any(|(s, _)| *s == Stage::Reap),
            "the switch arms nothing"
        );

        let mut judgeless = crate::core::test_support::test_core().await;
        judgeless.evolve.autonomous = crate::config::Autonomy::Full;
        judgeless.judge = None;
        assert!(
            !periodic_units(&judgeless)
                .iter()
                .any(|(s, _)| *s == Stage::Reap),
            "no judge, no sweep — the rules alone may never tombstone"
        );

        for level in [
            crate::config::Autonomy::Off,
            crate::config::Autonomy::Ranking,
        ] {
            let mut below = crate::core::test_support::test_core().await;
            below.evolve.autonomous = level;
            assert!(
                !periodic_units(&below)
                    .iter()
                    .any(|(s, _)| *s == Stage::Reap),
                "the one sweep that destroys text needs the same permission as \
                 the one that undoes it: {level:?}"
            );
        }
    }

    #[tokio::test]
    async fn the_context_sweep_is_armed_only_when_the_offer_is_on() {
        use crate::store::jobs::Stage;
        let mut core = crate::core::test_support::test_core().await;
        assert!(
            !periodic_units(&core)
                .iter()
                .any(|(s, _)| *s == Stage::Context),
            "off by default"
        );
        assert_eq!(periodic_period(&core, Stage::Context), None);

        core.recommend.enabled = true;

        core.learn.enabled = true;
        assert!(
            periodic_units(&core)
                .iter()
                .any(|(s, _)| *s == Stage::Context)
        );
        assert_eq!(
            periodic_period(&core, Stage::Context),
            Some(Duration::from_secs(
                crate::jobs::context::INTERVAL_HOURS * 3600
            ))
        );
        assert!(
            periodic_units(&core)
                .iter()
                .any(|(s, _)| *s == Stage::Retention),
            "nothing else would expire a recorded situation"
        );
    }

    #[tokio::test]
    async fn the_consolidation_sweep_never_stacks_up_in_the_queue() {
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
        arm_missing_periodic(&core).await;
        while let Some(j) = core.store.claim_job().await.unwrap() {
            assert_ne!(
                j.stage,
                crate::store::jobs::Stage::Consolidate,
                "a disabled sweep must not be armed"
            );
        }
    }

    #[tokio::test]
    async fn a_crashed_capture_is_repaired_with_the_sweep_switched_off() {
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
                    },
                    crate::store::segments::NewSegment {
                        start_line: 11,
                        end_line: 20,
                        text: "second window",
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

    #[tokio::test]
    async fn the_sweep_history_is_trimmed_with_capture_switched_off() {
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = false;
        core.feedback.retain_days = 0;
        core.recommend.enabled = false;
        assert!(
            !periodic_units(&core)
                .iter()
                .any(|(s, _)| *s == crate::store::jobs::Stage::Retention),
            "this base still has the unit that used to do the trimming"
        );
        let over = crate::store::sweeps::MAX_RUNS + 5;
        for i in 0..over {
            core.store
                .record_sweep_run("consolidate", crate::store::now() - (over - i), "ok", "{}")
                .await
                .unwrap();
        }

        repair_once(&core).await;

        assert_eq!(
            core.store
                .sweep_history(crate::store::sweeps::MAX_RUNS + 10)
                .await
                .unwrap()
                .len() as i64,
            crate::store::sweeps::MAX_RUNS,
        );
    }

    #[tokio::test]
    async fn the_sweeps_are_armed_to_run_now_when_the_process_starts() {
        let core = crate::core::test_support::test_core().await;

        arm_missing_periodic(&core).await;

        let j = core
            .store
            .claim_job()
            .await
            .unwrap()
            .expect("nothing was armed");
        assert_eq!(j.stage, crate::store::jobs::Stage::Consolidate);
        assert_eq!(j.target_id, CONSOLIDATE_TARGET);
    }

    #[tokio::test]
    async fn arming_what_is_missing_never_stacks_a_sweep_up() {
        let core = crate::core::test_support::test_core().await;
        for _ in 0..3 {
            arm_missing_periodic(&core).await;
        }
        let mut consolidations = 0;
        while let Some(j) = core.store.claim_job().await.unwrap() {
            if j.stage == crate::store::jobs::Stage::Consolidate {
                consolidations += 1;
            }
        }
        assert_eq!(consolidations, 1, "the sweep stacked up in the queue");
    }

    #[tokio::test]
    async fn a_sweep_that_died_between_claim_and_rearm_is_armed_again() {
        let core = crate::core::test_support::test_core().await;
        arm_missing_periodic(&core).await;
        let j = core.store.claim_job().await.unwrap().unwrap();
        core.store.complete_job(j.id).await.unwrap();

        arm_missing_periodic(&core).await;

        let mut stages = Vec::new();
        while let Some(j) = core.store.claim_job().await.unwrap() {
            stages.push(j.stage);
        }
        assert!(
            stages.contains(&crate::store::jobs::Stage::Consolidate),
            "a sweep closed without re-arming was never put back"
        );
    }

    #[tokio::test]
    async fn the_association_sweep_never_stacks_up_in_the_queue() {
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
    async fn the_association_sweep_is_armed_when_the_process_starts() {
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;

        arm_missing_periodic(&core).await;

        let mut stages = Vec::new();
        while let Some(j) = core.store.claim_job().await.unwrap() {
            stages.push((j.stage, j.target_id));
        }
        assert!(
            stages.contains(&(
                crate::store::jobs::Stage::Associate,
                ASSOCIATE_TARGET.to_string()
            )),
            "the association sweep was never armed: {stages:?}"
        );
    }

    #[tokio::test]
    async fn no_recorded_searches_means_no_association_sweep_at_all() {
        let core = crate::core::test_support::test_core().await; // `[learn]` off
        assert!(
            !periodic_units(&core)
                .iter()
                .any(|(s, _)| *s == crate::store::jobs::Stage::Associate)
        );
    }

    #[tokio::test]
    async fn nothing_to_expire_and_nothing_to_group_is_no_retention_unit() {
        let mut core = crate::core::test_support::test_core().await;
        core.feedback.retain_days = 0;
        core.learn.enabled = false;
        core.recommend.enabled = false;
        assert!(
            !periodic_units(&core)
                .iter()
                .any(|(s, _)| *s == crate::store::jobs::Stage::Retention)
        );
    }

    #[tokio::test]
    async fn switching_learning_off_does_not_freeze_the_situations_already_written() {
        let mut core = crate::core::test_support::test_core().await;
        core.feedback.retain_days = 0;
        core.learn.enabled = false;
        core.recommend.enabled = true;
        assert!(!core.recommends(), "learning is off");
        assert!(
            periodic_units(&core)
                .iter()
                .any(|(s, _)| *s == crate::store::jobs::Stage::Retention),
            "nothing is left to age out what the offer wrote while it was on"
        );
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
