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
/// then guarantees at most one queued sweep however often it is armed.
pub const CONSOLIDATE_TARGET: &str = "collection";

/// Every sweep that runs on a period, what switches it off, and how long it
/// waits between runs.
///
/// One list, where five ticker preambles used to be. Each of those started
/// separately, was gated separately, and knew about none of the others; what
/// ran first on a given night was whichever interval happened to elapse first.
/// A periodic unit re-arms itself `run_after` an interval out when it finishes,
/// so `run_after` *is* the cursor recording when it last ran — already indexed,
/// with no clock to hold and no meta key to keep.
///
/// This is also where every gate that used to be an early `return` now lives,
/// and each one keeps its original condition exactly. Getting that wrong
/// re-creates the bug `spawn_repair_ticker` exists to record: a pass that rides
/// on another feature's switch stops when that feature is switched off, and
/// nobody asked for that.
///
/// `Pursuit` is here despite being armed by the association sweep on completion
/// — replay before pursue. That arming is what *orders* the two; the period it
/// keeps here is a floor under it, and the row is what the repair pass needs to
/// find in order to recover a pursuit that died mid-run. See `periodic_period`.
pub fn periodic_units(core: &crate::core::Core) -> Vec<(crate::store::jobs::Stage, &'static str)> {
    use crate::store::jobs::Stage;
    let mut out = Vec::new();
    // Duplicate hygiene, and the judging that needs a model to do it with.
    if core.consolidate.enabled && core.synthesizes() {
        out.push((Stage::Consolidate, CONSOLIDATE_TARGET));
        // Zero units per tick is the off switch for the calls, not for the
        // sweep that finds the pairs.
        if core.consolidate.max_dedupe_per_tick > 0 {
            out.push((Stage::ArmDedupe, CONSOLIDATE_TARGET));
        }
    }
    // Expiring and grouping. Behind neither consolidation nor association: an
    // operator who switches duplicate hygiene off is not asking to keep their
    // query log forever. With nothing to expire and nothing to group there is
    // no unit at all, which is what the ticker's `return` used to say.
    // `recommend.enabled` and not `recommends()`: the second is `&& learn`,
    // and switching learning off is precisely when the situations already
    // written down most need to keep ageing out. Gating the ageing on the same
    // key as the collecting means an operator who turns the feature off freezes
    // a 400-day window at rest for ever, which is the opposite of what they
    // asked for. The unit is armed by the key that says the feature exists on
    // this base; whether it is currently learning does not enter into it.
    if core.feedback.retain_days > 0 || core.learn.enabled || core.recommend.enabled {
        out.push((Stage::Retention, CONSOLIDATE_TARGET));
    }
    // Learning which situations recur for which artifact. Behind its own gate
    // and nothing else's: it reads the interaction log, which is not something
    // an operator switches off by switching off duplicate hygiene.
    if core.recommends() {
        out.push((Stage::Context, CONSOLIDATE_TARGET));
    }
    if core.associating() {
        out.push((Stage::Associate, ASSOCIATE_TARGET));
        // Its own period as a floor. The association sweep arming it is what
        // orders the two; this is what keeps pursuits running at the cadence
        // they ran at before, rather than at the association sweep's. No second
        // condition of its own any more — a pursuit runs behind `[learn]`, and
        // `associating()` is `[learn]`.
        out.push((Stage::Pursuit, ASSOCIATE_TARGET));
    }
    out
}

/// How long this sweep waits between runs, or `None` if it is switched off.
///
/// The existing interval keys keep their names and their meanings and become
/// each unit's own `run_after` step, so an operator who tuned
/// `associate.interval_mins` finds it still doing what it did. The `max(1)` and
/// the saturating multiply come with them: the operand is operator-typed, and a
/// wrap here would turn a very long configured interval into a very short one.
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
        // Shorter than the idle window, so a run of searches is grouped soon
        // after it goes quiet.
        Stage::Pursuit => (core.pursuit.idle_secs / 2).max(60),
        _ => return None,
    };
    Some(std::time::Duration::from_secs(secs))
}

/// Arm every periodic unit that nothing is going to run.
///
/// The guard against the one failure mode a ticker does not have: a unit that
/// dies between being claimed and re-arming itself would otherwise never run
/// again. "Missing" means *not live* rather than *no row*, because a sweep
/// closed without re-arming is just as gone as one that was deleted — and the
/// two are indistinguishable from here, which is the point.
///
/// Armed to run now, not an interval out. This is also what arms them at boot,
/// where the repair pass's first tick fires immediately, and a restart picking
/// the work straight up is the behaviour the tickers had.
pub(crate) async fn arm_missing_periodic(core: &crate::core::Core) {
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

/// How many of *one tenant's* background units may be promoted on one repair
/// tick.
///
/// A constant rather than a setting, and small on purpose. Ageing exists so
/// that a unit which has waited goes ahead of a fresh capture (§4.4), and that
/// stays true — but the queue fills on its own, and a slow judge endpoint can
/// leave hundreds of units past the threshold at the same moment. Promoting
/// all of them puts the whole backlog in front of something the operator just
/// pasted, which is the head-of-line wait the class column exists to end.
/// Twenty at a time keeps that wait bounded; the rest are still worked as
/// background, and the next tick takes the next twenty.
///
/// Per tenant, because the wait it bounds is per tenant: what a person may have
/// pasted is in front of their own promotions and nobody else's. Shared across
/// the instance it was the same number however many people used it, so the
/// guarantee thinned out as users were added — and, taken oldest-first over the
/// whole table, the tenant with the deepest backlog spent all of it.
const AGE_PER_TICK: i64 = 20;

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
/// One ticker for the instance, and a pass per registered user.
///
/// It iterates `users` rather than the open registry, because a tenant nobody
/// has touched since boot is exactly the one whose interrupted work nothing
/// else will find. That opens each base in turn, which is the cost of the
/// guarantee; the passes themselves are SQLite-local and cheap.
///
/// The instance-wide half — reclaiming units a dead process left `running`,
/// ageing background work, expiring sessions — runs once per tick and not once
/// per user: those queries are over the whole control database, and repeating
/// them per tenant would do the same work N times for the same answer.
pub fn spawn_repair_ticker(
    tenants: std::sync::Arc<crate::tenants::Tenants>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let period = std::time::Duration::from_secs(REPAIR_INTERVAL_HOURS * 3600);
        let mut tick = tokio::time::interval(period);
        // Not from now: a tenant reconciles its two stores the first time
        // somebody asks for it, so an interval that fired immediately would
        // scroll the whole collection a second time for an answer the process
        // just computed. Opening a tenant from *here* deliberately does not
        // reconcile it — see `Tenants::on_first_open` — which is what keeps
        // this pass from turning the first tick into one scroll per user.
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

/// The instance-wide half of a repair tick, then the per-tenant half.
///
/// One base is open at a time. The pass used to open every registered user up
/// front and hold the whole set for its duration, which on an instance with any
/// number of users is a SQLite pool and a vector client each, all live at once,
/// every hour and again at boot — with `store.max_open_tenants` bounding the
/// registry and nothing at all bounding this. Opening inside the loop makes the
/// walk's cost its own rather than the instance's.
pub(crate) async fn repair_every_tenant(tenants: &crate::tenants::Tenants) {
    repair_control_once(tenants.control()).await;
    let older_than = age_threshold(tenants.config());
    for subject in every_subject(tenants).await {
        // Before the open, because it does not need the base: ageing is a
        // statement about that subject's rows in the control database.
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

/// Every registered subject.
///
/// Registered users and not the open registry, because a base nobody has
/// touched since boot is exactly the one whose interrupted work nothing else
/// will find.
async fn every_subject(tenants: &crate::tenants::Tenants) -> Vec<String> {
    match tenants.control().users().await {
        Ok(u) => u.into_iter().map(|u| u.subject).collect(),
        Err(e) => {
            tracing::warn!(error = %e, "could not list the users to repair; retrying on the next pass");
            Vec::new()
        }
    }
}

/// One base, opened for the length of one pass over it and dropped after.
///
/// `get_transient` and not `get`: a walk over the whole user table must not
/// decide what the fixed-size registry holds, or the hourly tick ends with
/// every actually-active tenant evicted in favour of whichever users the walk
/// happened to reach last. A tenant that cannot be opened is logged and left
/// out rather than stopping the pass for everybody else; the next tick tries
/// again.
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

/// How old a waiting background unit has to be to be promoted.
///
/// Saturating for the same reason `periodic_period` is: `age_after_mins` comes
/// out of `config.toml`, and a large enough value panics in debug and wraps in
/// release, leaving an `older_than` that ages rows which have not waited at
/// all. `0` is not a way to switch ageing off — it means every waiting unit is
/// old enough, so the classes stop dividing anything, which is the behaviour
/// from before the column existed.
fn age_threshold(cfg: &crate::config::Config) -> i64 {
    crate::store::now().saturating_sub(cfg.schedule.age_after_mins.max(0).saturating_mul(60))
}

/// The repairs that are about the queue rather than about anyone's knowledge.
///
/// One control database, so one pass — not one per tenant. `reclaim_stuck` and
/// `purge_expired_sessions` are questions about the whole table, and running
/// them per user would ask the same thing as many times as there are users and
/// act on the first answer. Ageing is not one of those and no longer lives
/// here: it is a per-tenant budget, so it runs in the per-tenant loop.
pub(crate) async fn repair_control_once(control: &crate::store::control::Control) {
    match control.reclaim_stuck(crate::jobs::STUCK_AFTER_SECS).await {
        Ok(n) if n > 0 => tracing::info!(reclaimed = n, "requeued jobs left running by a dead process"),
        Err(e) => tracing::warn!(error = %e, "could not requeue jobs left running"),
        _ => {}
    }
    match control.purge_expired_sessions().await {
        Ok(n) if n > 0 => tracing::info!(purged = n, "removed expired sessions"),
        Err(e) => tracing::warn!(error = %e, "could not remove expired sessions"),
        _ => {}
    }
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
    // A sweep that died between being claimed and re-arming itself would
    // otherwise never run again — the one failure mode a ticker does not have,
    // and the reason this pass stays outside the schedule.
    arm_missing_periodic(core).await;
    // Priority without ageing is starvation, but the ageing is instance-wide
    // and lives in `repair_control_once` beside the rest of the queue's own
    // repairs — one control database, one pass.
    // Housekeeping about housekeeping. It rode on the retention unit, which is
    // behind `feedback`: an operator with capture off and `retain_days` at its
    // default has no retention unit at all, while the sweeps that do run — the
    // dedupe arming every fifteen minutes, consolidation every day — keep
    // writing a row apiece into a table nothing was left to trim. The same
    // mistake this whole pass exists to record, one table further in.
    // A coverage row outlives what it covers. `gap_id` names one of three
    // tables, so it is deliberately not a foreign key and nothing cascades onto
    // it — while retention deletes searches and questions on a promise, and a
    // purge takes every one of them. The rows left behind were kept for the
    // life of the base and read back as nothing at all, `gaps_covered_by_each`
    // skipping each one because the join found no text to show.
    match core.store.trim_gap_coverage().await {
        Ok(n) if n > 0 => tracing::info!(dropped = n, "dropped coverage of gaps that are gone"),
        Err(e) => tracing::warn!(error = %e, "could not drop coverage of gaps that are gone"),
        _ => {}
    }
    match core.store.trim_sweep_runs().await {
        Ok(n) if n > 0 => tracing::info!(dropped = n, "trimmed the sweep history"),
        Err(e) => tracing::warn!(error = %e, "could not trim the sweep history"),
        _ => {}
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

/// The association sweep's job target. A constant rather than an artifact id:
/// the sweep replays the whole log, and `UNIQUE(stage, target_id)` then bounds
/// the queue to one of them however often the ticker fires.
pub const ASSOCIATE_TARGET: &str = "collection";

#[cfg(test)]
mod tests {

    /// The instance-wide half of a repair tick: one control database, one pass,
    /// covering every tenant's stuck work rather than the caller's alone.
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
        sqlx::query("UPDATE jobs SET state = 'running', claimed_at = ?")
            .bind(crate::store::now() - crate::jobs::STUCK_AFTER_SECS - 60)
            .execute(&tenants.control().pool)
            .await
            .unwrap();

        repair_control_once(tenants.control()).await;

        let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE state = 'pending'")
            .fetch_one(&tenants.control().pool)
            .await
            .unwrap();
        assert_eq!(pending, 2, "one tenant's stuck work was left running");
    }

    /// Ageing is a per-tenant budget, not one budget the instance shares out.
    ///
    /// Shared, `AGE_PER_TICK` was the same twenty however many people used the
    /// instance — ten users past the threshold got two promotions an hour each
    /// — and oldest-first over the whole table meant the tenant with the
    /// deepest backlog took all of it and the others got none.
    #[tokio::test]
    async fn every_tenant_gets_its_own_ageing_budget() {
        let (tenants, a, b, _dir) = crate::tenants::test_support::two_tenants().await;
        // More waiting units each than one tick may promote, and `a`'s are all
        // older than `b`'s — which under one shared budget is exactly what let
        // `a` spend the lot.
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
            sqlx::query("UPDATE jobs SET created_at = ? WHERE subject = ?")
                .bind(crate::store::now() - base)
                .bind(&t.user.subject)
                .execute(&tenants.control().pool)
                .await
                .unwrap();
        }

        repair_every_tenant(&tenants).await;

        for t in [&a, &b] {
            let promoted: i64 =
                sqlx::query_scalar("SELECT count(*) FROM jobs WHERE subject = ? AND class = 0")
                    .bind(&t.user.subject)
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

    /// A walk over every registered user must not decide what the registry
    /// holds. Opening each tenant through `get` did: the pass ended with the
    /// last `max_open_tenants` users it happened to reach resident and whoever
    /// was actually being served evicted behind them.
    #[tokio::test]
    async fn a_repair_pass_does_not_evict_the_tenant_somebody_is_using() {
        let (tenants, _dir) = crate::tenants::test_support::test_tenants(1).await;
        for s in ["sub-a", "sub-b", "sub-c"] {
            tenants.get_or_provision(s, None).await.unwrap();
        }
        // `sub-a` is the one in use, and so the one the registry holds. It is
        // also the *first* the walk reaches, which is what makes this a test:
        // through `get`, every later user displaced it in turn and the pass
        // ended holding whoever it happened to visit last.
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
        // And it drags the retention unit along with it: `context_events` has
        // its own window, and the trim rides that unit.
        assert!(
            periodic_units(&core)
                .iter()
                .any(|(s, _)| *s == Stage::Retention),
            "nothing else would expire a recorded situation"
        );
    }

    #[tokio::test]
    async fn the_consolidation_sweep_never_stacks_up_in_the_queue() {
        // `jobs` is unique on (stage, target), so an arming that lands while a
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

    #[tokio::test]
    async fn the_sweep_history_is_trimmed_with_capture_switched_off() {
        // The trim rode on the retention unit, and that unit does not exist
        // when `feedback` is off and nothing is being retained — while the
        // dedupe arming and consolidation keep running, and keep writing a row
        // apiece into a table nothing was left to trim.
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = false;
        core.feedback.retain_days = 0;
        // The offer too: it has a retention window of its own, and a base
        // where it exists is a base with something left to expire.
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
        // The repair pass's first tick fires immediately, and this is what it
        // does with it. Armed at `run_after = 0` and not an interval out: a
        // restart picking the work straight up is the behaviour the tickers
        // had, and waiting a day after every deploy is not.
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
        // `jobs` is unique on (stage, target), so a repair pass running while a
        // sweep is still queued collapses onto the same row rather than
        // stacking sweeps behind a slow one.
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
        // The one failure mode a ticker does not have: a unit that is claimed
        // and never re-arms itself would otherwise never run again. "Missing"
        // is *not live* rather than *no row*, because a sweep closed without
        // re-arming is just as gone as one that was deleted.
        let core = crate::core::test_support::test_core().await;
        arm_missing_periodic(&core).await;
        let j = core.store.claim_job().await.unwrap().unwrap();
        // Closed without re-arming: the sweep died between the claim and the
        // one write that would have put it back.
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
        // With `[learn]` off nothing is recorded, so there is nothing to learn
        // from and there is no unit — which is the list's job now that there is
        // no ticker to return from.
        let core = crate::core::test_support::test_core().await; // `[learn]` off
        assert!(
            !periodic_units(&core)
                .iter()
                .any(|(s, _)| *s == crate::store::jobs::Stage::Associate)
        );
    }

    #[tokio::test]
    async fn nothing_to_expire_and_nothing_to_group_is_no_retention_unit() {
        // The ticker used to `return` here, and once lost the `return` and went
        // on waking every `sweep_hours` for the life of the process to do
        // nothing at all. A unit that is never armed cannot do that.
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
        // The ageing used to ride `recommends()`, which is `recommend && learn`
        // — so an operator who switched learning off got no retention unit at
        // all, and the 400-day window over `context_events` and
        // `interaction_events` stopped moving with everything collected so far
        // still in it. Turning a thing off is when its data most needs to leave.
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
