use super::control::Control;
use super::{Store, now};
use crate::error::Result;
use sqlx::Row;

/// Where a job's behaviour changes, not where it is abandoned.
///
/// Past this many attempts a stage may switch tactics — splitting a batch
/// embed into one job per artifact, recording which segments the synthesizer
/// refused — but the work stays queued either way, at the backoff's ceiling.
pub const MAX_ATTEMPTS: i64 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Splits a corpus into windows and arms one `SegmentWindow` per window.
    /// Makes no inference call itself.
    Synthesize,
    Enrich,
    /// One window, one call. The unit the job model is built around.
    SegmentWindow,
    /// Naming one document. One call.
    Title,
    Embed,
    /// The periodic consolidation sweep. Its target is the collection rather
    /// than any one corpus, so there is exactly one of these in the queue at a
    /// time. Local work: it arms `Judge` units rather than calling the model.
    Consolidate,
    /// One component of near-duplicate artifacts, one call.
    Dedupe,
    /// One artifact, one neighbour query. No inference: `neighbours` looks the
    /// vector up by point id, so this costs a round trip and nothing else.
    ///
    /// What makes duplicate detection complete rather than sampled. The sweep
    /// draws `sample` points and computes pairs only within that draw, so both
    /// members of a pair have to land in the same one — probability (s/N)^2,
    /// which decays quadratically and leaves a given pair waiting years on a
    /// large base.
    Relate,
    /// One image, one vision call: reads a captured image into the markdown
    /// that becomes its `raw_text`, then hands off to `Synthesize`.
    Describe,
    /// One captured PDF, read into the markdown that becomes its `raw_text`,
    /// then handed off to `Synthesize`. Local work: no inference call, so no
    /// role gates it and no budget is spent on it.
    Extract,
    /// The periodic association sweep. Its target is the collection rather than
    /// any one artifact, so the `UNIQUE(stage, target_id)` on `jobs` guarantees
    /// at most one queued sweep however often the ticker fires. Local work: it
    /// replays the search log and arms `LinkJudge` units, and calls no model.
    Associate,
    /// One strong cross-corpus link, one call. Target is `"<a_id>|<b_id>"`.
    LinkJudge,
    /// The pursuit sweep: groups quiet searches, scores what was engaged,
    /// decides. Local work; arms `Generate`.
    Pursuit,
    /// One pursuit, one call: write the artifact it earned.
    Generate,
    /// The periodic retention sweep: expire what `feedback.retain_days` says is
    /// past keeping, then regroup the knowledge gaps. In that order, which is
    /// the order the ticker it replaces used — grouping reads the rows expiring
    /// removes. Local work, no call.
    Retention,
    /// The periodic dedupe arming: walks the pairs `Relate` found and arms
    /// `Dedupe` units for the ones worth a call. Named for what it does — it
    /// arms judgements, it is not one — and local, since the call belongs to
    /// the units it arms.
    ArmDedupe,
    /// The periodic context sweep: reads the interaction log, agglomerates the
    /// situations each artifact was opened in, and writes the surviving
    /// centroids to the vector store. Local work and no call — the whole point
    /// of this faculty is that the learning is a sweep and the read is one
    /// vector query.
    Context,
}

impl Stage {
    /// Every stage there is. Written out rather than derived, and the compiler
    /// is no help here — a stage left out of this list is not an error, it is a
    /// stage the class backfill silently never sees.
    pub const ALL: [Stage; 17] = [
        Stage::Synthesize,
        Stage::Enrich,
        Stage::SegmentWindow,
        Stage::Title,
        Stage::Embed,
        Stage::Consolidate,
        Stage::Dedupe,
        Stage::Relate,
        Stage::Describe,
        Stage::Extract,
        Stage::Associate,
        Stage::LinkJudge,
        Stage::Pursuit,
        Stage::Generate,
        Stage::Retention,
        Stage::ArmDedupe,
        Stage::Context,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Stage::Synthesize => "synthesize",
            Stage::Enrich => "enrich",
            Stage::SegmentWindow => "segment_window",
            Stage::Title => "title",
            Stage::Embed => "embed",
            Stage::Consolidate => "consolidate",
            Stage::Dedupe => "dedupe",
            Stage::Relate => "relate",
            Stage::Describe => "describe",
            Stage::Extract => "extract",
            Stage::Associate => "associate",
            Stage::LinkJudge => "link_judge",
            Stage::Pursuit => "pursuit",
            Stage::Generate => "generate",
            Stage::Retention => "retention",
            Stage::ArmDedupe => "arm_dedupe",
            Stage::Context => "context",
        }
    }
    /// Is someone waiting on this? `0` foreground, `1` background.
    ///
    /// Foreground is the capture pipeline: the operator pasted something and is
    /// watching it move `raw → embedding → ready`. Background is every sweep
    /// whose result nobody is standing in front of. One distinction and not a
    /// scale — two classes cannot say that embedding matters more than titling,
    /// and deliberately so: the distinction that pays is *someone is waiting*,
    /// and a second one can be added later without moving the column.
    ///
    /// Exhaustive on purpose. A stage added later has to answer this question
    /// rather than inherit an answer from a wildcard arm.
    pub fn class(self) -> i64 {
        match self {
            // `Enrich` shares `synthesize::plan` with `Synthesize` and is
            // foreground for the same reason: it is a capture in flight.
            Stage::Synthesize
            | Stage::Enrich
            | Stage::SegmentWindow
            | Stage::Title
            | Stage::Embed
            | Stage::Describe
            | Stage::Extract => 0,
            Stage::Consolidate
            | Stage::Dedupe
            | Stage::Relate
            | Stage::Associate
            | Stage::LinkJudge
            | Stage::Pursuit
            | Stage::Generate
            | Stage::Retention
            | Stage::ArmDedupe
            | Stage::Context => 1,
        }
    }

    pub fn parse(s: &str) -> Option<Stage> {
        match s {
            "synthesize" => Some(Stage::Synthesize),
            "enrich" => Some(Stage::Enrich),
            "segment_window" => Some(Stage::SegmentWindow),
            "title" => Some(Stage::Title),
            "embed" => Some(Stage::Embed),
            "consolidate" => Some(Stage::Consolidate),
            "dedupe" => Some(Stage::Dedupe),
            "relate" => Some(Stage::Relate),
            "describe" => Some(Stage::Describe),
            "extract" => Some(Stage::Extract),
            "associate" => Some(Stage::Associate),
            "link_judge" => Some(Stage::LinkJudge),
            "pursuit" => Some(Stage::Pursuit),
            "generate" => Some(Stage::Generate),
            "retention" => Some(Stage::Retention),
            "arm_dedupe" => Some(Stage::ArmDedupe),
            "context" => Some(Stage::Context),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: i64,
    pub stage: Stage,
    pub target_kind: String,
    pub target_id: String,
    pub attempts: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct FailedJob {
    pub id: i64,
    pub stage: String,
    pub target_id: String,
    pub attempts: i64,
    pub last_error: Option<String>,
}

/// Work waiting out a backoff. What replaced the failed list: there is no
/// terminal state to report, only a next attempt to name.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RetryingJob {
    pub stage: String,
    pub target_id: String,
    pub attempts: i64,
    pub next_attempt_secs: i64,
    pub last_error: Option<String>,
}

/// 2s, 4s, 8s, 16s, 32s ... doubling to a six-hour ceiling, and never stopping.
///
/// The ceiling used to be five minutes, which suited a caller that gave up
/// after five attempts — one minute of patience in total. An inference endpoint
/// that loads a model on demand takes ten, so the whole budget was spent before
/// the endpoint had finished starting, and the work was lost until a person
/// noticed and pressed a button. Six hours is short enough that a base heals
/// the same day and long enough that text the model will never accept costs
/// four calls a day rather than a thousand.
pub fn backoff_secs(attempts: i64) -> i64 {
    let exp = attempts.clamp(1, 16) as u32;
    2i64.saturating_pow(exp).min(21_600)
}

/// Which existing rows an arming upsert may disturb.
///
/// Each carries its whole statement rather than a fragment to paste in, so
/// there is no construction step for anything to be smuggled through.
#[derive(Clone, Copy)]
enum Guard {
    /// Anything, running included. An operator's reprocess.
    Any,
    /// Only a row nothing is going to run: closed, or given up on.
    ///
    /// `'failed'` belongs here with `'done'`. Nothing writes that state any
    /// more — a job out of attempts is delayed rather than abandoned — but bases
    /// upgraded across that change still hold rows in it, and a row that is
    /// neither live nor armable is a unit that never runs again. For a periodic
    /// sweep that is silent and permanent: `arm_missing_periodic` sees a unit
    /// nothing is going to run, and the arming it answers with is refused.
    Closed,
}

/// `enqueue`, on the control pool.
///
/// It used to take whatever executor the caller was inside — a capture's
/// transaction, so that the corpus row and the unit that processes it landed
/// together or not at all. The queue lives in a second database now, and
/// SQLite makes no atomicity promise across two of them in WAL mode, so that
/// guarantee is gone: a crash between the commit and this leaves a corpus with
/// no queued job. `jobs/reconcile.rs` is what finds it — "a process killed
/// between two writes" is the case its module doc opens with.
///
/// Which fixes the order every caller has to use: **commit first, arm after**.
/// The other way round the unit is claimable before its target is visible, and
/// a claimed unit whose target is not there is not a race to a worker — it is
/// a deletion, closed with `complete_job` so it never runs again. One order
/// leaves work a sweep can find; the other loses it silently.
pub(crate) async fn enqueue_with(
    control: &crate::store::control::Control,
    subject: &str,
    stage: Stage,
    target_kind: &str,
    target_id: &str,
) -> Result<()> {
    upsert_job_with(
        control,
        subject,
        stage,
        target_kind,
        target_id,
        0,
        Guard::Any,
    )
    .await
}

async fn upsert_job_with(
    control: &crate::store::control::Control,
    subject: &str,
    stage: Stage,
    target_kind: &str,
    target_id: &str,
    seq: i64,
    guard: Guard,
) -> Result<()> {
    sqlx::query(guard.statement())
        .bind(subject)
        .bind(stage.as_str())
        .bind(target_kind)
        .bind(target_id)
        .bind(now())
        .bind(seq)
        .bind(stage.class())
        .execute(&control.pool)
        .await?;
    Ok(())
}

/// The upsert the three guards share, spelled out once per guard because a
/// statement assembled at runtime is a statement nobody can read in the source.
macro_rules! arm_job {
    ($guard:literal) => {
        concat!(
            "INSERT INTO jobs (subject, stage, target_kind, target_id, state, attempts, run_after, created_at, seq, class)
             VALUES (?, ?, ?, ?, 'pending', 0, 0, ?, ?, ?)
             ON CONFLICT(subject, stage, target_id) DO UPDATE SET
               state = 'pending', attempts = 0, run_after = 0, last_error = NULL,
               claimed_at = NULL, created_at = excluded.created_at, seq = excluded.seq,
               -- Re-armed rows take the stage's class back, ageing included: an
               -- arming resets `attempts` and `created_at` too, so what it
               -- leaves behind is a fresh unit, and a fresh background unit has
               -- not waited for anything yet.
               class = excluded.class ",
            $guard
        )
    };
}

impl Guard {
    fn statement(self) -> &'static str {
        match self {
            Guard::Any => arm_job!(""),
            Guard::Closed => arm_job!("WHERE jobs.state IN ('done', 'failed')"),
        }
    }
}

impl Store {
    pub async fn enqueue(&self, stage: Stage, target_kind: &str, target_id: &str) -> Result<()> {
        self.enqueue_seq(stage, target_kind, target_id, 0).await
    }

    /// Arm a unit at a given position within its batch.
    ///
    /// `enqueue` is this with `seq = 0`, which is right for a singleton and
    /// wrong for the thirty-four windows of one document: left at zero they
    /// would sort among themselves by row id, and a document captured later
    /// would wait behind all of them.
    ///
    /// Idempotent per (stage, target). A conflicting row is re-armed whatever
    /// state it is in, running included — which is what an operator's reprocess
    /// needs and what nothing automatic should do. Automatic arming wants
    /// `arm_seq` or `rearm_idle_seq` below.
    pub async fn enqueue_seq(
        &self,
        stage: Stage,
        target_kind: &str,
        target_id: &str,
        seq: i64,
    ) -> Result<()> {
        self.upsert_job(stage, target_kind, target_id, seq, Guard::Any)
            .await
    }

    /// Arm a unit only if nothing is going to run it.
    ///
    /// What every automatic arming wants, and `enqueue_seq` is not it: a unit
    /// already queued is already going to run, and winding its `attempts` back
    /// to zero is how a window the model will not read never reaches the ceiling
    /// that lets its document settle — forever young, and its corpus forever
    /// `segmenting`. Only a row closed while its work was not is resurrected.
    ///
    /// There was once a middle tier that armed anything a worker was not inside,
    /// on the reasoning that a merely *queued* unit is safe to disturb. It is
    /// not, for the reason above, and every caller it had has moved here.
    pub async fn rearm_idle_seq(
        &self,
        stage: Stage,
        target_kind: &str,
        target_id: &str,
        seq: i64,
    ) -> Result<()> {
        self.upsert_job(stage, target_kind, target_id, seq, Guard::Closed)
            .await
    }

    /// Arm a periodic unit to run at `run_after`.
    ///
    /// What a self-rescheduling sweep does when it finishes: `run_after` is the
    /// cursor recording when it last ran, and it is already indexed, so nothing
    /// has to hold a clock and no meta key has to remember a period.
    ///
    /// Guarded on the row being closed, like every automatic arming: a sweep
    /// already queued is already going to run, and pushing its `run_after` an
    /// interval further out on every pass is how a sweep would recede forever.
    /// `'failed'` counts as closed here for the reason `Guard::Closed` gives —
    /// a row nothing will run is not a row that is going to recede.
    pub async fn arm_periodic(
        &self,
        stage: Stage,
        target_kind: &str,
        target_id: &str,
        run_after: i64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO jobs (subject, stage, target_kind, target_id, state, attempts, run_after, created_at, seq, class)
             VALUES (?, ?, ?, ?, 'pending', 0, ?, ?, 0, ?)
             ON CONFLICT(subject, stage, target_id) DO UPDATE SET
               state = 'pending', attempts = 0, run_after = excluded.run_after,
               last_error = NULL, claimed_at = NULL,
               created_at = excluded.created_at, class = excluded.class
             WHERE jobs.state IN ('done', 'failed')",
        )
        .bind(&self.subject)
        .bind(stage.as_str())
        .bind(target_kind)
        .bind(target_id)
        .bind(run_after)
        .bind(now())
        .bind(stage.class())
        .execute(&self.control.pool)
        .await?;
        Ok(())
    }

    /// `arm_periodic`, carrying the count of consecutive runs that found
    /// nothing.
    ///
    /// A separate method rather than a fifth argument on `arm_periodic`,
    /// because the two say different things: `arm_periodic` is the repair
    /// pass putting a missing sweep back, and knows nothing about how the last
    /// run went, while this is a sweep that has just finished saying so.
    pub async fn arm_periodic_with_backoff(
        &self,
        stage: Stage,
        target_kind: &str,
        target_id: &str,
        run_after: i64,
        empty_runs: i64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO jobs (subject, stage, target_kind, target_id, state, attempts, run_after, created_at, seq, class, empty_runs)
             VALUES (?, ?, ?, ?, 'pending', 0, ?, ?, 0, ?, ?)
             ON CONFLICT(subject, stage, target_id) DO UPDATE SET
               state = 'pending', attempts = 0, run_after = excluded.run_after,
               last_error = NULL, claimed_at = NULL,
               created_at = excluded.created_at, class = excluded.class,
               empty_runs = excluded.empty_runs
             WHERE jobs.state IN ('done', 'failed')",
        )
        .bind(&self.subject)
        .bind(stage.as_str())
        .bind(target_kind)
        .bind(target_id)
        .bind(run_after)
        .bind(now())
        .bind(stage.class())
        .bind(empty_runs)
        .execute(&self.control.pool)
        .await?;
        Ok(())
    }

    /// How many consecutive runs of this unit have found nothing. Zero when
    /// there is no row, which is the same answer a first run gets.
    pub async fn empty_runs(&self, stage: Stage, target_id: &str) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "SELECT empty_runs FROM jobs WHERE subject = ? AND stage = ? AND target_id = ?",
        )
        .bind(&self.subject)
        .bind(stage.as_str())
        .bind(target_id)
        .fetch_optional(&self.control.pool)
        .await?
        .unwrap_or(0))
    }

    /// This unit has just become worth running: let it go now.
    ///
    /// Two statements, because there are two cases and neither is the other's.
    /// A unit sleeping on its own period has its `run_after` pulled forward —
    /// and only that, so a sweep that keeps failing does not have its attempt
    /// count wound back and reclaim the front of the queue every time something
    /// arms it. A unit that is closed is armed outright.
    ///
    /// A unit a worker is already inside is left alone by both: it is running.
    ///
    /// The pulled-forward row has its class restored along with its `run_after`,
    /// the way the closed-row path does. Ageing is a statement about how long
    /// something has been waiting, and a unit that is being armed is not the
    /// unit that waited: leaving the aged class on it would hand a sweep
    /// foreground priority for the rest of its life, one arming at a time.
    ///
    /// `created_at` moves with them, for the same reason: pulling `run_after`
    /// down to zero on a row stamped a period ago would leave a unit that has
    /// been ready for one instant looking, to `age_background`, like one that
    /// has been waiting all period.
    pub async fn arm_now(&self, stage: Stage, target_kind: &str, target_id: &str) -> Result<()> {
        // The backoff count first, and on its own statement, because it is the
        // one thing here that is true of a row in *any* state. The two
        // statements below cover a sleeping row and a closed one; a row a
        // worker is inside matches neither, and that is not a rare case for a
        // periodic unit — it is a capture landing while the sweep it concerns
        // is running. Left to them, `rearm_periodic_with` read the count this
        // arming was supposed to have cleared and re-armed the sweep at the
        // doubled wait, with the new data already in the base.
        sqlx::query(
            "UPDATE jobs SET empty_runs = 0
              WHERE subject = ? AND stage = ? AND target_id = ?",
        )
        .bind(&self.subject)
        .bind(stage.as_str())
        .bind(target_id)
        .execute(&self.control.pool)
        .await?;
        sqlx::query(
            "UPDATE jobs SET run_after = 0, class = ?, created_at = ?
              WHERE subject = ? AND stage = ? AND target_id = ?
                AND state = 'pending' AND run_after > 0",
        )
        .bind(stage.class())
        .bind(now())
        .bind(&self.subject)
        .bind(stage.as_str())
        .bind(target_id)
        .execute(&self.control.pool)
        .await?;
        self.rearm_idle_seq(stage, target_kind, target_id, 0).await
    }

    /// The one upsert the three above differ only in the guard on.
    async fn upsert_job(
        &self,
        stage: Stage,
        target_kind: &str,
        target_id: &str,
        seq: i64,
        guard: Guard,
    ) -> Result<()> {
        upsert_job_with(
            &self.control,
            &self.subject,
            stage,
            target_kind,
            target_id,
            seq,
            guard,
        )
        .await
    }

    /// The `seq` a job currently carries, so a unit that re-arms itself can
    /// climb rather than re-entering at the front of its batch.
    pub async fn job_seq(&self, stage: Stage, target_id: &str) -> Result<Option<i64>> {
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT seq FROM jobs WHERE subject = ? AND stage = ? AND target_id = ?",
        )
        .bind(&self.subject)
        .bind(stage.as_str())
        .bind(target_id)
        .fetch_optional(&self.control.pool)
        .await?)
    }

    /// Is anything going to run this unit? `Some` for a row that is queued or
    /// that a worker is inside, `None` for one that is closed or was never
    /// armed.
    ///
    /// What a sweep needs before spending budget on a unit: a pair whose
    /// judgement is already queued is already going to be judged, and arming it
    /// again is a no-op that costs another pair its turn.
    pub async fn live_job(&self, stage: Stage, target_id: &str) -> Result<bool> {
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT 1 FROM jobs
              WHERE subject = ? AND stage = ? AND target_id = ?
                AND state IN ('pending', 'running')",
        )
        .bind(&self.subject)
        .bind(stage.as_str())
        .bind(target_id)
        .fetch_optional(&self.control.pool)
        .await?
        .is_some())
    }

    /// Forget that this unit was ever armed.
    ///
    /// Only an operator asking for the work again wants this. A row surviving
    /// its completion is what lets a stage give up for good, so removing one
    /// undoes exactly that — which is the point when the person who owns the
    /// collection has asked for another try.
    pub async fn delete_job(&self, stage: Stage, target_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM jobs WHERE subject = ? AND stage = ? AND target_id = ?")
            .bind(&self.subject)
            .bind(stage.as_str())
            .bind(target_id)
            .execute(&self.control.pool)
            .await?;
        Ok(())
    }

    /// Forget every window unit of one document.
    ///
    /// The units are keyed `corpus#idx` and outlive the window rows they name,
    /// so `clear_segments` on its own leaves a rerun sharing the attempt counts
    /// of the run it replaces — and since planning arms idle-only, it would keep
    /// them. Only an operator asking for the work again wants this, for the same
    /// reason `delete_job` exists: a rerun a person asked for is a clean slate
    /// or it is not one.
    ///
    /// Matched on the `corpus#` prefix rather than with `LIKE`, so an id
    /// carrying a wildcard character cannot widen the delete.
    pub async fn delete_window_jobs(&self, corpus_id: &str) -> Result<()> {
        sqlx::query(
            "DELETE FROM jobs
              WHERE subject = ? AND stage = 'segment_window'
                AND substr(target_id, 1, length(?) + 1) = ? || '#'",
        )
        .bind(&self.subject)
        .bind(corpus_id)
        .bind(corpus_id)
        .execute(&self.control.pool)
        .await?;
        Ok(())
    }

    /// Has this unit ever been armed? A row survives being completed, so this
    /// distinguishes "never asked" from "asked, and done asking" — which is the
    /// only thing separating a first settle from the fiftieth for a stage that
    /// is allowed to give up.
    pub async fn has_job(&self, stage: Stage, target_id: &str) -> Result<bool> {
        Ok(self.job_seq(stage, target_id).await?.is_some())
    }

    pub async fn job_counts(&self) -> Result<Vec<(String, i64)>> {
        let rows =
            sqlx::query("SELECT state, COUNT(*) AS n FROM jobs WHERE subject = ? GROUP BY state")
                .bind(&self.subject)
                .fetch_all(&self.control.pool)
                .await?;
        Ok(rows.iter().map(|r| (r.get("state"), r.get("n"))).collect())
    }

    /// Jobs waiting on a backoff, soonest first.
    ///
    /// `attempts > 0` is what separates work that has hit something from work
    /// that is merely queued: a fresh job has `run_after` in the past and does
    /// not belong on a page about trouble.
    pub async fn retrying_jobs(&self, limit: i64) -> Result<Vec<RetryingJob>> {
        let rows = sqlx::query(
            "SELECT stage, target_id, attempts, last_error, run_after FROM jobs
              WHERE subject = ? AND state = 'pending' AND attempts > 0 AND run_after > ?
              ORDER BY run_after LIMIT ?",
        )
        .bind(&self.subject)
        .bind(now())
        .bind(limit)
        .fetch_all(&self.control.pool)
        .await?;
        let at = now();
        Ok(rows
            .iter()
            .map(|r| RetryingJob {
                stage: r.get("stage"),
                target_id: r.get("target_id"),
                attempts: r.get("attempts"),
                next_attempt_secs: (r.get::<i64, _>("run_after") - at).max(0),
                last_error: r.get("last_error"),
            })
            .collect())
    }

    pub async fn failed_jobs(&self, limit: i64) -> Result<Vec<FailedJob>> {
        let rows = sqlx::query(
            "SELECT id, stage, target_id, attempts, last_error FROM jobs
              WHERE subject = ? AND state = 'failed' ORDER BY id DESC LIMIT ?",
        )
        .bind(&self.subject)
        .bind(limit)
        .fetch_all(&self.control.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| FailedJob {
                id: r.get("id"),
                stage: r.get("stage"),
                target_id: r.get("target_id"),
                attempts: r.get("attempts"),
                last_error: r.get("last_error"),
            })
            .collect())
    }

    /// How long the longest-waiting pending job has been queued, in seconds.
    ///
    /// Measured from `created_at`, not `run_after`: a job that was never
    /// delayed has `run_after = 0`, which would report seconds-since-epoch.
    pub async fn oldest_pending_age(&self) -> Result<Option<i64>> {
        let row = sqlx::query(
            "SELECT MIN(created_at) AS oldest FROM jobs WHERE subject = ? AND state = 'pending'",
        )
        .bind(&self.subject)
        .fetch_one(&self.control.pool)
        .await?;
        let oldest: Option<i64> = row.get("oldest");
        Ok(oldest.map(|t| (now() - t).max(0)))
    }
}

/// The instance-wide half of the queue.
///
/// Claiming, closing and recovering are about the machine rather than about
/// any one tenant: one pool of workers serves everybody, so these run without
/// a subject and `claim_job` reports the one it found.
impl Control {
    /// Atomic claim. The UPDATE ... WHERE id = (SELECT ...) RETURNING form runs
    /// as one statement under SQLite's write lock, so two workers can never
    /// take the same row.
    ///
    /// Least-tried first, then earliest position in its batch, then oldest.
    ///
    /// Ordering by id alone made the queue strictly sequential in the one case
    /// where that hurts: `fail_job` re-arms the row in place, so a job that
    /// cannot get through keeps its original id and reclaims the front of the
    /// queue every time its backoff expires, ahead of everything captured since.
    /// One document the model would not parse therefore held up every document
    /// behind it. Sorting by `attempts` puts work that has already had its turn
    /// behind work that has not — a reordering rather than a demotion, since the
    /// sore thumb still runs as soon as nothing fresher is ready.
    ///
    /// `seq` then interleaves whole documents. Without it, a corpus armed as
    /// thirty-four units takes thirty-four consecutive ids and reproduces the
    /// same head-of-line blocking one level down.
    ///
    /// `class` sorts ahead of all of that and answers one question: is somebody
    /// waiting on this? Without it a capture the operator is watching queues
    /// behind a consolidation sweep armed a minute earlier and waits for it,
    /// with nothing anywhere able to say that one of the two has a person in
    /// front of it. It sorts *before* `seq` and never instead of it: within one
    /// class the fairness above is untouched.
    /// Instance-wide, and it says whose job it is.
    ///
    /// `subject` is deliberately absent from the ordering. The claim order is
    /// the single-user one, unchanged, and `seq` already interleaves batches —
    /// so across tenants it interleaves those too, and one user's ingest cannot
    /// drain ahead of another's without a scheduler being written to say so.
    pub async fn claim_job(&self) -> Result<Option<(String, Job)>> {
        let row = sqlx::query(
            "UPDATE jobs
                SET state = 'running', claimed_at = ?, attempts = attempts + 1
              WHERE id = (
                SELECT id FROM jobs
                 WHERE state = 'pending' AND run_after <= ?
                 ORDER BY class, attempts, seq, id LIMIT 1
              )
              RETURNING id, subject, stage, target_kind, target_id, attempts",
        )
        .bind(now())
        .bind(now())
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| {
            (
                r.get::<String, _>("subject"),
                Job {
                    id: r.get("id"),
                    stage: Stage::parse(r.get::<String, _>("stage").as_str())
                        .unwrap_or(Stage::Synthesize),
                    target_kind: r.get("target_kind"),
                    target_id: r.get("target_id"),
                    attempts: r.get("attempts"),
                },
            )
        }))
    }

    pub async fn complete_job(&self, id: i64) -> Result<()> {
        sqlx::query(
            "UPDATE jobs SET state = 'done', last_error = NULL, claimed_at = NULL WHERE id = ?",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Put a job back in the queue with a delay.
    ///
    /// There is no terminal state. `attempts` past `MAX_ATTEMPTS` only means
    /// the delay has reached its ceiling: a base that cannot reach its
    /// endpoint should cost nothing and heal when the endpoint returns, and
    /// the previous behaviour — mark it failed, close it, wait for a human —
    /// turned a ten-minute outage into permanently missing knowledge.
    pub async fn fail_job(&self, id: i64, attempts: i64, err: &str) -> Result<()> {
        sqlx::query(
            "UPDATE jobs SET state = 'pending', run_after = ?, last_error = ?, claimed_at = NULL WHERE id = ?",
        )
        .bind(now() + backoff_secs(attempts))
        .bind(err)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Rows left 'running' by a crashed process. Called once at startup.
    pub async fn reclaim_stuck(&self, older_than_secs: i64) -> Result<u64> {
        let res = sqlx::query(
            "UPDATE jobs SET state = 'pending', run_after = 0, claimed_at = NULL
              WHERE state = 'running' AND claimed_at IS NOT NULL AND claimed_at < ?",
        )
        .bind(now() - older_than_secs)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// A background unit that has waited long enough becomes foreground.
    ///
    /// Written, not computed. Ageing at claim time would put `created_at` — an
    /// inequality — into the ordering, and an inequality ends an index's usable
    /// ordering: every poll would find the ready rows and then sort them in a
    /// temp B-tree, which is the cost `idx_jobs_claim3`'s column order exists to
    /// avoid. Ageing a few rows on the repair tick is one indexed update and
    /// leaves the hot path exactly as fast as it was.
    ///
    /// A unit that has aged stays aged: it has already waited, and demoting it
    /// again would be starting its wait over.
    ///
    /// The wait is measured from the moment the unit became *ready*, which is
    /// the later of the two stamps it carries. A unit asleep on its own period
    /// carries the `created_at` of the moment it was rescheduled, so waiting and
    /// sleeping look identical from `created_at` alone — and every sweep whose
    /// period is longer than `age_after_mins` (consolidation's day, retention's
    /// six hours) would arrive at its `run_after` with a `created_at` already a
    /// whole period old, and be promoted on the very next repair tick without
    /// having waited in the queue at all. That is the promotion the class exists
    /// to stop, arriving a period late instead of never. `run_after` is what
    /// tells resting from waiting, and taking the later of the two stamps says
    /// both things at once: a unit that is not due yet cannot age, and a unit
    /// that has just come due starts its wait then.
    ///
    /// A job that was never delayed has `run_after = 0`, so for everything that
    /// is not periodic the later stamp is `created_at` and this reads exactly as
    /// it did.
    /// At most `limit` rows move per call, oldest first. Background units
    /// accumulate on their own — every `arm_dedupe` tick arms a batch of judge
    /// units — and a slow judge endpoint leaves hundreds of them waiting past
    /// the threshold. Promoting the whole backlog at once puts every one of
    /// them ahead of a capture the operator has just pasted, since within a
    /// class the order is `attempts, seq, id` and they were all armed first.
    /// An aged unit going ahead of a fresh capture is the promise (§4.4) and
    /// stays; the whole queue doing it at once is not.
    ///
    /// Per tenant, and `limit` is that tenant's whole budget.
    ///
    /// Instance-wide it was one budget shared by everybody, which is the same
    /// starvation this promotes units to end, moved up a level: ten users past
    /// the threshold at once got two promotions an hour each, a hundred got a
    /// fifth of one, and the guarantee the class column exists for thinned out
    /// in proportion to how many people used the instance. Oldest-first over
    /// the whole table made it worse than the average suggests — one tenant
    /// with a deep backlog is *always* the oldest, so it took the entire budget
    /// and everyone else waited on it.
    pub async fn age_background(&self, subject: &str, older_than: i64, limit: i64) -> Result<u64> {
        let res = sqlx::query(
            "UPDATE jobs SET class = 0
              WHERE id IN (
                SELECT id FROM jobs
                 WHERE subject = ? AND state = 'pending' AND class = 1
                   AND max(created_at, run_after) < ?
                 ORDER BY max(created_at, run_after)
                 LIMIT ?)",
        )
        .bind(subject)
        .bind(older_than)
        .bind(limit)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }
}

/// Closing a unit, as its owner. Both take a row id, which is already unique
/// instance-wide, so these only exist so that the callers deep inside the job
/// runner keep talking to the store they are already holding.
impl Store {
    pub async fn complete_job(&self, id: i64) -> Result<()> {
        self.control.complete_job(id).await
    }

    pub async fn fail_job(&self, id: i64, attempts: i64, err: &str) -> Result<()> {
        self.control.fail_job(id, attempts, err).await
    }

    /// Promote this tenant's background units that have waited long enough.
    ///
    /// Scoped to this store's own subject: unlike the claim, ageing is about
    /// one person's queue, and `limit` is what that person gets. Kept here
    /// because the tests drive it through a single base.
    pub async fn age_background(&self, older_than: i64, limit: i64) -> Result<u64> {
        self.control
            .age_background(&self.subject, older_than, limit)
            .await
    }

    /// Claim one of *this* tenant's units.
    ///
    /// The workers do not use this — they claim instance-wide and dispatch on
    /// the subject that comes back, which is what keeps one pool in front of
    /// one set of inference endpoints. This is for asking a single base to take
    /// its next step, which is what every test of a stage is doing.
    pub async fn claim_job(&self) -> Result<Option<Job>> {
        let row = sqlx::query(
            "UPDATE jobs
                SET state = 'running', claimed_at = ?, attempts = attempts + 1
              WHERE id = (
                SELECT id FROM jobs
                 WHERE subject = ? AND state = 'pending' AND run_after <= ?
                 ORDER BY class, attempts, seq, id LIMIT 1
              )
              RETURNING id, stage, target_kind, target_id, attempts",
        )
        .bind(now())
        .bind(&self.subject)
        .bind(now())
        .fetch_optional(&self.control.pool)
        .await?;

        Ok(row.map(|r| Job {
            id: r.get("id"),
            stage: Stage::parse(r.get::<String, _>("stage").as_str()).unwrap_or(Stage::Synthesize),
            target_kind: r.get("target_kind"),
            target_id: r.get("target_id"),
            attempts: r.get("attempts"),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    #[test]
    fn backoff_doubles_then_caps() {
        assert_eq!(backoff_secs(2), 4);
        assert_eq!(backoff_secs(3), 8);
        assert_eq!(backoff_secs(4), 16);
        assert_eq!(backoff_secs(100), 21_600, "must cap, not grow unbounded");
    }

    #[tokio::test]
    async fn enqueue_is_idempotent_per_stage_and_target() {
        let s = Store::memory().await.unwrap();
        s.enqueue(Stage::Synthesize, "corpus", "src-1")
            .await
            .unwrap();
        s.enqueue(Stage::Synthesize, "corpus", "src-1")
            .await
            .unwrap();
        assert!(s.claim_job().await.unwrap().is_some());
        assert!(
            s.claim_job().await.unwrap().is_none(),
            "duplicate enqueue created a second job"
        );
    }

    #[tokio::test]
    async fn a_capture_does_not_wait_behind_a_sweep() {
        // The whole point of the class column: a sweep armed first is not what
        // the operator is standing in front of.
        let s = Store::memory().await.unwrap();
        s.enqueue(Stage::Associate, "collection", "collection")
            .await
            .unwrap();
        s.enqueue(Stage::Synthesize, "corpus", "src-1")
            .await
            .unwrap();

        let first = s.claim_job().await.unwrap().unwrap();
        assert_eq!(
            first.stage,
            Stage::Synthesize,
            "the capture queued behind the sweep that was armed a moment earlier"
        );
        let second = s.claim_job().await.unwrap().unwrap();
        assert_eq!(second.stage, Stage::Associate, "the sweep must still run");
    }

    #[tokio::test]
    async fn within_one_class_the_order_is_still_attempts_then_seq() {
        // `seq`'s anti-starvation property is the easiest thing to lose to a
        // priority column, so it is asserted directly rather than inferred from
        // the claim ordering being "unchanged".
        let s = Store::memory().await.unwrap();
        // Two documents' worth of windows, interleaved by seq: the second
        // document's first window must beat the first document's second.
        s.enqueue_seq(Stage::SegmentWindow, "corpus", "a#0", 0)
            .await
            .unwrap();
        s.enqueue_seq(Stage::SegmentWindow, "corpus", "a#1", 1)
            .await
            .unwrap();
        s.enqueue_seq(Stage::SegmentWindow, "corpus", "b#0", 0)
            .await
            .unwrap();

        let mut order = Vec::new();
        while let Some(j) = s.claim_job().await.unwrap() {
            order.push(j.target_id);
        }
        assert_eq!(
            order,
            vec!["a#0", "b#0", "a#1"],
            "seq no longer interleaves whole documents"
        );
    }

    #[tokio::test]
    async fn the_claim_still_walks_one_index_and_never_sorts() {
        // The shape of the class column, and of ageing being a write rather
        // than a read, exists to keep this true. An inequality or a computed
        // age in the ordering turns every poll into a temp B-tree sort, and
        // nothing about the queue's behaviour would say so.
        let s = Store::memory().await.unwrap();
        let rows = sqlx::query(
            "EXPLAIN QUERY PLAN
             SELECT id FROM jobs
              WHERE state = 'pending' AND run_after <= ?
              ORDER BY class, attempts, seq, id LIMIT 1",
        )
        .bind(now())
        .fetch_all(&s.control.pool)
        .await
        .unwrap();
        let plan = rows
            .iter()
            .map(|r| r.get::<String, _>("detail"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            plan.contains("idx_jobs_claim3"),
            "the claim stopped using its covering index: {plan}"
        );
        assert!(
            !plan.to_uppercase().contains("TEMP B-TREE"),
            "the claim now sorts: {plan}"
        );
    }

    #[tokio::test]
    async fn a_background_unit_that_has_waited_long_enough_goes_first() {
        // Priority without ageing is starvation. The claim is unchanged: what
        // moves is the row.
        let s = Store::memory().await.unwrap();
        s.enqueue(Stage::Associate, "collection", "collection")
            .await
            .unwrap();
        sqlx::query("UPDATE jobs SET created_at = ? WHERE stage = 'associate'")
            .bind(now() - 7200)
            .execute(&s.control.pool)
            .await
            .unwrap();
        s.enqueue(Stage::Synthesize, "corpus", "src-1")
            .await
            .unwrap();

        let aged = s.age_background(now() - 3600, 100).await.unwrap();
        assert_eq!(aged, 1);

        let first = s.claim_job().await.unwrap().unwrap();
        assert_eq!(
            first.stage,
            Stage::Associate,
            "a sweep that has waited an hour is still behind a fresh capture"
        );
        let class: i64 = sqlx::query_scalar("SELECT class FROM jobs WHERE stage = 'associate'")
            .fetch_one(&s.control.pool)
            .await
            .unwrap();
        assert_eq!(class, 0, "having aged must be durable, not recomputed");
    }

    #[tokio::test]
    async fn only_so_many_units_may_age_on_one_pass() {
        // Ageing lets a unit that has waited go ahead of a fresh capture, which
        // is the promise. The whole backlog doing it at once is not: background
        // units arm themselves — a judge endpoint that is slow for an hour
        // leaves hundreds of them past the threshold — and within one class the
        // order is `attempts, seq, id`, so every one of them was armed before
        // the capture the operator just pasted and every one of them goes
        // first. That is the head-of-line wait the class exists to end,
        // arriving an hour late.
        let s = Store::memory().await.unwrap();
        for i in 0..5 {
            s.enqueue(Stage::Relate, "artifact", &format!("a-{i}"))
                .await
                .unwrap();
        }
        sqlx::query("UPDATE jobs SET created_at = ? WHERE stage = 'relate'")
            .bind(now() - 7200)
            .execute(&s.control.pool)
            .await
            .unwrap();

        assert_eq!(s.age_background(now() - 3600, 2).await.unwrap(), 2);

        let still: i64 =
            sqlx::query_scalar("SELECT count(*) FROM jobs WHERE class = 1 AND stage = 'relate'")
                .fetch_one(&s.control.pool)
                .await
                .unwrap();
        assert_eq!(still, 3, "the rest wait for the next pass");

        // And they do get their turn.
        assert_eq!(s.age_background(now() - 3600, 100).await.unwrap(), 3);
    }

    #[tokio::test]
    async fn the_oldest_units_age_first() {
        // The cap is a cap on how many jump the queue, not a reason for the
        // one that has waited longest to keep waiting.
        let s = Store::memory().await.unwrap();
        for i in 0..3 {
            s.enqueue(Stage::Relate, "artifact", &format!("a-{i}"))
                .await
                .unwrap();
        }
        sqlx::query("UPDATE jobs SET created_at = ? WHERE target_id = 'a-2'")
            .bind(now() - 86_400)
            .execute(&s.control.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE jobs SET created_at = ? WHERE target_id <> 'a-2'")
            .bind(now() - 7200)
            .execute(&s.control.pool)
            .await
            .unwrap();

        assert_eq!(s.age_background(now() - 3600, 1).await.unwrap(), 1);

        let aged: String =
            sqlx::query_scalar("SELECT target_id FROM jobs WHERE class = 0 AND stage = 'relate'")
                .fetch_one(&s.control.pool)
                .await
                .unwrap();
        assert_eq!(aged, "a-2", "the longest wait goes first");
    }

    #[tokio::test]
    async fn a_sweep_asleep_on_its_own_period_is_not_waiting() {
        // `arm_periodic` stamps `created_at` when it reschedules, so a sweep
        // resting on a day-long period looks, from `created_at` alone, exactly
        // like one that has been queued a day. Ageing it would hand the front
        // of the queue to work that is not even due — ahead of the captures
        // somebody is watching, which is the one thing the class exists to
        // stop.
        let s = Store::memory().await.unwrap();
        s.arm_periodic(Stage::Retention, "collection", "collection", now() + 21_600)
            .await
            .unwrap();
        sqlx::query("UPDATE jobs SET created_at = ? WHERE stage = 'retention'")
            .bind(now() - 7200)
            .execute(&s.control.pool)
            .await
            .unwrap();

        assert_eq!(
            s.age_background(now() - 3600, 100).await.unwrap(),
            0,
            "a unit that is not due yet has not been waiting"
        );

        // Due, and now it ages.
        sqlx::query("UPDATE jobs SET run_after = 0 WHERE stage = 'retention'")
            .execute(&s.control.pool)
            .await
            .unwrap();
        assert_eq!(s.age_background(now() - 3600, 100).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn a_sweep_that_has_just_come_due_has_not_been_waiting() {
        // The other half of the test above, and the one the `run_after` guard
        // alone did not answer: a sweep re-armed for six hours' time carries a
        // `created_at` six hours old by the moment it comes due, so the very
        // next repair tick would promote a unit that had been ready for
        // seconds. The wait has to be measured from when it became ready.
        let s = Store::memory().await.unwrap();
        s.arm_periodic(Stage::Retention, "collection", "collection", now() + 21_600)
            .await
            .unwrap();
        // Six hours later: due a minute ago, stamped when it was rescheduled.
        sqlx::query("UPDATE jobs SET created_at = ?, run_after = ? WHERE stage = 'retention'")
            .bind(now() - 21_600)
            .bind(now() - 60)
            .execute(&s.control.pool)
            .await
            .unwrap();

        assert_eq!(
            s.age_background(now() - 3600, 100).await.unwrap(),
            0,
            "a sweep one minute past due has not waited an hour for a worker"
        );

        // An hour of actually being ready, and now it ages.
        sqlx::query("UPDATE jobs SET run_after = ? WHERE stage = 'retention'")
            .bind(now() - 7200)
            .execute(&s.control.pool)
            .await
            .unwrap();
        assert_eq!(s.age_background(now() - 3600, 100).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn arming_a_sleeping_unit_starts_its_wait_over() {
        // `arm_now` pulls `run_after` to zero, which leaves `created_at` as the
        // only stamp — and on a sweep armed an interval ago that stamp is an
        // interval old, so the unit would be ready and immediately ageable
        // without having waited at all.
        let s = Store::memory().await.unwrap();
        s.arm_periodic(Stage::Pursuit, "collection", "collection", now() + 600)
            .await
            .unwrap();
        sqlx::query("UPDATE jobs SET created_at = ? WHERE stage = 'pursuit'")
            .bind(now() - 7200)
            .execute(&s.control.pool)
            .await
            .unwrap();

        s.arm_now(Stage::Pursuit, "collection", "collection")
            .await
            .unwrap();

        assert_eq!(
            s.age_background(now() - 3600, 100).await.unwrap(),
            0,
            "a unit that was just armed is not a unit that waited two hours"
        );
    }

    #[tokio::test]
    async fn a_sweep_left_failed_by_an_older_build_can_still_be_armed() {
        // Nothing writes `failed` any more — a job out of attempts is delayed
        // rather than abandoned — but a base upgraded across that change still
        // holds rows in it. Such a row is not live, so the repair pass asks for
        // it; if the arming refuses, the sweep never runs again, silently, for
        // the life of the install.
        let s = Store::memory().await.unwrap();
        s.arm_periodic(Stage::Consolidate, "collection", "collection", 0)
            .await
            .unwrap();
        sqlx::query("UPDATE jobs SET state = 'failed' WHERE stage = 'consolidate'")
            .execute(&s.control.pool)
            .await
            .unwrap();
        assert!(!s.live_job(Stage::Consolidate, "collection").await.unwrap());

        s.arm_periodic(Stage::Consolidate, "collection", "collection", 0)
            .await
            .unwrap();

        assert!(
            s.live_job(Stage::Consolidate, "collection").await.unwrap(),
            "a sweep nothing was going to run stayed that way"
        );
    }

    #[tokio::test]
    async fn arming_a_sleeping_unit_gives_back_the_class_it_had() {
        // Ageing says how long something has waited. Pulling a unit forward
        // makes it a unit that has not waited, and leaving the aged class on it
        // would let a sweep hold foreground priority for the rest of its life,
        // one arming at a time.
        let s = Store::memory().await.unwrap();
        s.arm_periodic(Stage::Pursuit, "collection", "collection", now() + 600)
            .await
            .unwrap();
        sqlx::query("UPDATE jobs SET class = 0 WHERE stage = 'pursuit'")
            .execute(&s.control.pool)
            .await
            .unwrap();

        s.arm_now(Stage::Pursuit, "collection", "collection")
            .await
            .unwrap();

        let (run_after, class): (i64, i64) =
            sqlx::query_as("SELECT run_after, class FROM jobs WHERE stage = 'pursuit'")
                .fetch_one(&s.control.pool)
                .await
                .unwrap();
        assert_eq!(run_after, 0, "the unit must have been pulled forward");
        assert_eq!(class, 1, "and it must be background again");
    }

    #[tokio::test]
    async fn a_job_is_claimed_exactly_once() {
        let s = Store::memory().await.unwrap();
        s.enqueue(Stage::Embed, "artifact", "c-1").await.unwrap();

        let a = s.claim_job().await.unwrap();
        let b = s.claim_job().await.unwrap();
        assert!(a.is_some());
        assert!(b.is_none(), "two workers claimed the same job");
        assert_eq!(a.unwrap().attempts, 1, "claiming must count the attempt");
    }

    /// The sequential test above cannot prove atomicity: `Store::memory()` is
    /// pinned to a single connection. This one uses a file-backed pool so real
    /// connections contend, which is the only way to catch a claim that is not
    /// actually atomic.
    #[tokio::test]
    async fn concurrent_workers_never_claim_the_same_job_twice() {
        use std::collections::HashSet;
        use std::sync::{Arc, Mutex};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.db");
        let store = Store::connect(
            &crate::config::StoreConfig {
                path: path.to_string_lossy().to_string(),
                ..Default::default()
            },
            {
                let c = crate::store::control::Control::memory().await.unwrap();
                c.provision(crate::store::TEST_SUBJECT, None).await.unwrap();
                c
            },
            crate::store::TEST_SUBJECT,
        )
        .await
        .unwrap();

        const JOBS: usize = 200;
        const WORKERS: usize = 8;
        for i in 0..JOBS {
            store
                .enqueue(Stage::Embed, "artifact", &format!("c-{i}"))
                .await
                .unwrap();
        }

        let claimed: Arc<Mutex<Vec<i64>>> = Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::new();
        for _ in 0..WORKERS {
            let store = store.clone();
            let claimed = Arc::clone(&claimed);
            handles.push(tokio::spawn(async move {
                while let Some(job) = store.claim_job().await.unwrap() {
                    claimed.lock().unwrap().push(job.id);
                    store.complete_job(job.id).await.unwrap();
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        let ids = claimed.lock().unwrap().clone();
        let unique: HashSet<i64> = ids.iter().copied().collect();
        assert_eq!(
            ids.len(),
            unique.len(),
            "a job was claimed more than once: {} claims for {} distinct jobs",
            ids.len(),
            unique.len()
        );
        assert_eq!(ids.len(), JOBS, "some jobs were never claimed");
    }

    #[tokio::test]
    async fn failure_reschedules_with_backoff_and_keeps_the_work() {
        let s = Store::memory().await.unwrap();
        s.enqueue(Stage::Embed, "artifact", "c-1").await.unwrap();

        let j = s.claim_job().await.unwrap().unwrap();
        s.fail_job(j.id, j.attempts, "endpoint down").await.unwrap();
        // Backed off: not immediately claimable.
        assert!(s.claim_job().await.unwrap().is_none());

        // Well past the old give-up point.
        for _ in 0..MAX_ATTEMPTS + 3 {
            sqlx::query("UPDATE jobs SET run_after = 0")
                .execute(&s.control.pool)
                .await
                .unwrap();
            let j = s
                .claim_job()
                .await
                .unwrap()
                .expect("the job must still be there to try again");
            s.fail_job(j.id, j.attempts, "still down").await.unwrap();
        }

        sqlx::query("UPDATE jobs SET run_after = 0")
            .execute(&s.control.pool)
            .await
            .unwrap();
        let again = s.claim_job().await.unwrap();
        assert!(
            again.is_some(),
            "the work was abandoned; an endpoint that comes back would never be noticed"
        );
        assert!(s.failed_jobs(10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn work_that_keeps_failing_stops_holding_the_head_of_the_queue() {
        // `fail_job` re-arms the row in place, so a job that fails over and
        // over keeps its original id. Ordering by id alone therefore handed the
        // queue's front to the one target that could not make progress, every
        // time its backoff expired, ahead of everything captured since.
        let s = Store::memory().await.unwrap();
        s.enqueue(Stage::Synthesize, "corpus", "sore-thumb")
            .await
            .unwrap();
        for _ in 0..4 {
            let j = s.claim_job().await.unwrap().unwrap();
            s.fail_job(j.id, 0, "malformed llm output").await.unwrap();
            // Past the backoff it just set; the delay is not what this test is
            // about.
            sqlx::query("UPDATE jobs SET run_after = 0")
                .execute(&s.control.pool)
                .await
                .unwrap();
        }

        // Captured long after, and with a lower row id nowhere in sight.
        s.enqueue(Stage::Synthesize, "corpus", "fresh")
            .await
            .unwrap();
        sqlx::query("UPDATE jobs SET run_after = 0")
            .execute(&s.control.pool)
            .await
            .unwrap();

        let next = s.claim_job().await.unwrap().unwrap();
        assert_eq!(
            next.target_id, "fresh",
            "the failing job kept the front of the queue"
        );
    }

    #[tokio::test]
    async fn a_job_that_yielded_is_not_starved_once_it_is_the_only_work_left() {
        // Yielding must be a reordering, not a demotion: the sore thumb still
        // runs when nothing fresher is ready.
        let s = Store::memory().await.unwrap();
        s.enqueue(Stage::Synthesize, "corpus", "sore-thumb")
            .await
            .unwrap();
        for _ in 0..4 {
            let j = s.claim_job().await.unwrap().unwrap();
            s.fail_job(j.id, 0, "malformed llm output").await.unwrap();
            // Past the backoff it just set; the delay is not what this test is
            // about.
            sqlx::query("UPDATE jobs SET run_after = 0")
                .execute(&s.control.pool)
                .await
                .unwrap();
        }
        sqlx::query("UPDATE jobs SET run_after = 0")
            .execute(&s.control.pool)
            .await
            .unwrap();

        let next = s.claim_job().await.unwrap();
        assert_eq!(
            next.expect("the failing job was abandoned").target_id,
            "sore-thumb"
        );
    }

    #[tokio::test]
    async fn units_of_two_documents_interleave_rather_than_queueing_behind_each_other() {
        // A thirty-four window document takes thirty-four consecutive row ids.
        // Under id ordering a capture made during that ingest waits for every
        // one of them before producing a single artifact.
        let s = Store::memory().await.unwrap();
        for i in 0..3 {
            s.enqueue_seq(Stage::SegmentWindow, "segment", &format!("doc-a#{i}"), i)
                .await
                .unwrap();
        }
        for i in 0..3 {
            s.enqueue_seq(Stage::SegmentWindow, "segment", &format!("doc-b#{i}"), i)
                .await
                .unwrap();
        }

        let mut order = Vec::new();
        while let Some(j) = s.claim_job().await.unwrap() {
            order.push(j.target_id);
            s.complete_job(j.id).await.unwrap();
        }
        assert_eq!(
            order,
            vec![
                "doc-a#0", "doc-b#0", "doc-a#1", "doc-b#1", "doc-a#2", "doc-b#2"
            ],
            "the second document waited for the whole of the first"
        );
    }

    #[tokio::test]
    async fn attempts_still_outrank_seq() {
        // The fairness fix must survive the interleaving one: a unit that keeps
        // failing sinks below fresher work whatever its position in a document.
        let s = Store::memory().await.unwrap();
        s.enqueue_seq(Stage::SegmentWindow, "segment", "sore#0", 0)
            .await
            .unwrap();
        let j = s.claim_job().await.unwrap().unwrap();
        s.fail_job(j.id, 0, "malformed llm output").await.unwrap();

        s.enqueue_seq(Stage::SegmentWindow, "segment", "fresh#9", 9)
            .await
            .unwrap();
        sqlx::query("UPDATE jobs SET run_after = 0")
            .execute(&s.control.pool)
            .await
            .unwrap();

        let next = s.claim_job().await.unwrap().unwrap();
        assert_eq!(next.target_id, "fresh#9", "a failing unit kept the front");
    }

    #[tokio::test]
    async fn arming_a_unit_a_worker_is_already_inside_leaves_it_alone() {
        // One row per unit, so putting a running one back in the queue hands the
        // same window to a second worker while the first is still inside the
        // model call — and `write_segment_artifacts` has each of them deleting
        // the artifacts the other just wrote.
        let s = Store::memory().await.unwrap();
        s.enqueue(Stage::SegmentWindow, "segment", "src-1#0")
            .await
            .unwrap();
        s.claim_job().await.unwrap().unwrap();

        s.rearm_idle_seq(Stage::SegmentWindow, "segment", "src-1#0", 0)
            .await
            .unwrap();
        assert!(
            s.claim_job().await.unwrap().is_none(),
            "a second worker could claim a window already being segmented"
        );

        // An operator's reprocess still gets through: they are asking for the
        // work to be redone, having presumably changed what it would produce.
        s.enqueue(Stage::SegmentWindow, "segment", "src-1#0")
            .await
            .unwrap();
        assert!(s.claim_job().await.unwrap().is_some());
    }

    #[tokio::test]
    async fn the_sweeps_arming_only_resurrects_a_closed_unit() {
        let s = Store::memory().await.unwrap();
        s.enqueue(Stage::SegmentWindow, "segment", "src-1#0")
            .await
            .unwrap();
        let j = s.claim_job().await.unwrap().unwrap();
        s.fail_job(j.id, 4, "unreadable reply").await.unwrap();

        // Queued and waiting out a backoff: already going to run, so the sweep
        // has nothing to add and must not wind it back.
        s.rearm_idle_seq(Stage::SegmentWindow, "segment", "src-1#0", 0)
            .await
            .unwrap();
        let (attempts, run_after): (i64, i64) =
            sqlx::query_as("SELECT attempts, run_after FROM jobs WHERE target_id = 'src-1#0'")
                .fetch_one(&s.control.pool)
                .await
                .unwrap();
        assert_eq!(attempts, 1, "the sweep reset a unit's attempt count");
        assert!(run_after > now(), "the sweep cleared a unit's backoff");

        // Closed while its work was not: exactly what the sweep exists for.
        s.complete_job(j.id).await.unwrap();
        s.rearm_idle_seq(Stage::SegmentWindow, "segment", "src-1#0", 0)
            .await
            .unwrap();
        assert!(
            s.claim_job().await.unwrap().is_some(),
            "the sweep left a closed unit's unfinished work unarmed"
        );
    }

    #[tokio::test]
    async fn stuck_running_jobs_are_reclaimed_after_a_crash() {
        let s = Store::memory().await.unwrap();
        s.enqueue(Stage::Synthesize, "corpus", "src-1")
            .await
            .unwrap();
        let j = s.claim_job().await.unwrap().unwrap();
        // Simulate the process dying mid-job: row left 'running'.
        sqlx::query("UPDATE jobs SET claimed_at = ? WHERE id = ?")
            .bind(crate::store::now() - 3600)
            .bind(j.id)
            .execute(&s.control.pool)
            .await
            .unwrap();

        assert_eq!(s.control.reclaim_stuck(600).await.unwrap(), 1);
        assert!(
            s.claim_job().await.unwrap().is_some(),
            "reclaimed job must be runnable again"
        );
    }

    #[tokio::test]
    async fn oldest_pending_age_is_a_waiting_time_not_a_timestamp() {
        let s = Store::memory().await.unwrap();
        assert_eq!(s.oldest_pending_age().await.unwrap(), None);

        s.enqueue(Stage::Synthesize, "corpus", "src-1")
            .await
            .unwrap();
        let age = s.oldest_pending_age().await.unwrap().unwrap();
        assert!(age < 5, "a just-enqueued job reported an age of {age}s");

        // A job enqueued an hour ago should read as roughly an hour.
        sqlx::query("UPDATE jobs SET created_at = ?")
            .bind(crate::store::now() - 3600)
            .execute(&s.control.pool)
            .await
            .unwrap();
        let age = s.oldest_pending_age().await.unwrap().unwrap();
        assert!((3595..=3605).contains(&age), "got {age}");
    }

    #[test]
    fn backoff_climbs_to_hours_and_stops_there() {
        // An endpoint that is down stays down for minutes; one loading a model
        // on demand takes ten. The old ceiling of five minutes went with a
        // caller that gave up after five attempts — one minute of patience in
        // total, spent before the endpoint had finished starting.
        assert_eq!(backoff_secs(1), 2);
        assert_eq!(backoff_secs(5), 32);
        assert_eq!(backoff_secs(20), 21_600);
        assert_eq!(backoff_secs(1_000), 21_600);
    }

    #[tokio::test]
    async fn a_job_out_of_attempts_waits_rather_than_failing() {
        let s = Store::memory().await.unwrap();
        s.enqueue(Stage::Embed, "artifact", "a1").await.unwrap();
        let job = s.claim_job().await.unwrap().unwrap();
        s.fail_job(job.id, MAX_ATTEMPTS + 10, "endpoint down")
            .await
            .unwrap();
        assert!(
            s.failed_jobs(10).await.unwrap().is_empty(),
            "a job was abandoned; nothing would ever pick it up again"
        );
        let state: String = sqlx::query_scalar("SELECT state FROM jobs WHERE id = ?")
            .bind(job.id)
            .fetch_one(&s.control.pool)
            .await
            .unwrap();
        assert_eq!(state, "pending");
    }

    #[tokio::test]
    async fn requeue_revives_a_failed_job() {
        let s = Store::memory().await.unwrap();
        s.enqueue(Stage::Embed, "artifact", "c-1").await.unwrap();
        let j = s.claim_job().await.unwrap().unwrap();
        sqlx::query("UPDATE jobs SET state='failed' WHERE id = ?")
            .bind(j.id)
            .execute(&s.control.pool)
            .await
            .unwrap();

        s.enqueue(Stage::Embed, "artifact", "c-1").await.unwrap();
        let again = s.claim_job().await.unwrap().unwrap();
        assert_eq!(again.attempts, 1, "requeue must reset the attempt counter");
    }

    #[test]
    fn describe_is_a_stage_that_round_trips_its_name() {
        assert_eq!(Stage::Describe.as_str(), "describe");
        assert_eq!(Stage::parse("describe"), Some(Stage::Describe));
    }

    #[tokio::test]
    async fn two_tenants_do_not_see_each_others_jobs() {
        let control = crate::store::control::Control::memory().await.unwrap();
        control.provision("sub-a", None).await.unwrap();
        control.provision("sub-b", None).await.unwrap();
        let a = crate::store::Store::memory_with(control.clone())
            .await
            .unwrap()
            .for_subject("sub-a");
        let b = crate::store::Store::memory_with(control.clone())
            .await
            .unwrap()
            .for_subject("sub-b");

        a.enqueue(Stage::Embed, "corpus", "shared-id")
            .await
            .unwrap();
        assert!(a.live_job(Stage::Embed, "shared-id").await.unwrap());
        assert!(!b.live_job(Stage::Embed, "shared-id").await.unwrap());
    }

    #[tokio::test]
    async fn the_same_target_id_in_two_tenants_is_two_jobs() {
        let control = crate::store::control::Control::memory().await.unwrap();
        control.provision("sub-a", None).await.unwrap();
        control.provision("sub-b", None).await.unwrap();
        let a = crate::store::Store::memory_with(control.clone())
            .await
            .unwrap()
            .for_subject("sub-a");
        let b = crate::store::Store::memory_with(control.clone())
            .await
            .unwrap()
            .for_subject("sub-b");

        a.enqueue(Stage::Embed, "corpus", "same").await.unwrap();
        b.enqueue(Stage::Embed, "corpus", "same").await.unwrap();

        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs")
            .fetch_one(&control.pool)
            .await
            .unwrap();
        assert_eq!(n, 2, "UNIQUE is per subject, not instance-wide");
    }

    #[tokio::test]
    async fn claiming_says_whose_job_it_is() {
        let control = crate::store::control::Control::memory().await.unwrap();
        control.provision("sub-a", None).await.unwrap();
        let a = crate::store::Store::memory_with(control.clone())
            .await
            .unwrap()
            .for_subject("sub-a");
        a.enqueue(Stage::Embed, "corpus", "c1").await.unwrap();

        let (subject, job) = control.claim_job().await.unwrap().expect("a job");
        assert_eq!(subject, "sub-a");
        assert_eq!(job.target_id, "c1");
    }

    #[tokio::test]
    async fn deleting_a_user_takes_their_queue_with_them() {
        let control = crate::store::control::Control::memory().await.unwrap();
        control.provision("sub-a", None).await.unwrap();
        let a = crate::store::Store::memory_with(control.clone())
            .await
            .unwrap()
            .for_subject("sub-a");
        a.enqueue(Stage::Embed, "corpus", "c1").await.unwrap();

        control.delete_user("sub-a").await.unwrap();
        assert!(control.claim_job().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn one_tenants_sweep_does_not_close_anothers() {
        let control = crate::store::control::Control::memory().await.unwrap();
        control.provision("sub-a", None).await.unwrap();
        control.provision("sub-b", None).await.unwrap();
        let a = crate::store::Store::memory_with(control.clone())
            .await
            .unwrap()
            .for_subject("sub-a");
        let b = crate::store::Store::memory_with(control.clone())
            .await
            .unwrap()
            .for_subject("sub-b");

        a.enqueue(Stage::Consolidate, "collection", "collection")
            .await
            .unwrap();
        b.enqueue(Stage::Consolidate, "collection", "collection")
            .await
            .unwrap();
        a.delete_job(Stage::Consolidate, "collection")
            .await
            .unwrap();

        assert!(!a.live_job(Stage::Consolidate, "collection").await.unwrap());
        assert!(b.live_job(Stage::Consolidate, "collection").await.unwrap());
    }
}
