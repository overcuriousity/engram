use super::control::Control;
use super::{Store, now};
use crate::error::Result;
use sqlx::Row;

/// Where a job's behaviour changes, not where it is abandoned: work stays queued at the
/// backoff's ceiling.
pub const MAX_ATTEMPTS: i64 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Makes no inference call itself.
    Synthesize,
    SegmentWindow,
    Title,
    Embed,
    /// Collection-targeted, so one at a time. Local: it arms `Judge` units.
    Consolidate,
    Dedupe,
    /// No inference: `neighbours` looks the vector up by point id. Makes duplicate detection
    /// complete; sampled pairs land together with probability (s/N)^2.
    Relate,
    Describe,
    /// Local work: no inference call, so no role gates it.
    Extract,
    /// Collection-targeted; `UNIQUE(stage, target_id)` keeps one queued sweep. Calls no model.
    Associate,
    /// Target is `"<a_id>|<b_id>"`.
    LinkJudge,
    /// Local work; arms `Generate`.
    Pursuit,
    Generate,
    /// Expires first, then regroups the gaps: grouping reads the rows expiring removes.
    Retention,
    /// Local: the call belongs to the `Dedupe` units it arms.
    ArmDedupe,
    /// Local, no call: learning is a sweep, the read is one vector query.
    Context,
    /// Armed at the next due moment by every write that can move it, never on a period.
    Remind,
    /// Collection-targeted; model calls bounded by `reap.max_judged_per_run`.
    Reap,
    /// Its own failure domain, like `Relate`: a failed cue embed must not fail the artifact's.
    Probe,
    /// A draft that would lose a value or a literal is refused without writing.
    Condense,
    /// Local, background: a page that stays down is retried at the ceiling, then left.
    Fetch,
}

impl Stage {
    /// Written out: a stage missing here is not a compile error, just invisible to the backfill.
    pub const ALL: [Stage; 21] = [
        Stage::Synthesize,
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
        Stage::Remind,
        Stage::Reap,
        Stage::Probe,
        Stage::Condense,
        Stage::Fetch,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Stage::Synthesize => "synthesize",
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
            Stage::Remind => "remind",
            Stage::Reap => "reap",
            Stage::Probe => "probe",
            Stage::Condense => "condense",
            Stage::Fetch => "fetch",
        }
    }
    /// In `ALL`'s order.
    pub const GENERATORS: [Stage; 7] = [
        Stage::SegmentWindow,
        Stage::Title,
        Stage::Dedupe,
        Stage::LinkJudge,
        Stage::Generate,
        Stage::Reap,
        Stage::Condense,
    ];

    /// A base with nothing to generate with passes these over at the claim, unpenalised.
    /// `Describe` has a role of its own. Exhaustive for the reason `class` is.
    pub fn calls_a_generator(self) -> bool {
        match self {
            Stage::SegmentWindow
            | Stage::Title
            | Stage::Dedupe
            | Stage::LinkJudge
            | Stage::Generate
            | Stage::Reap
            | Stage::Condense => true,
            Stage::Synthesize
            | Stage::Embed
            | Stage::Consolidate
            | Stage::Relate
            | Stage::Describe
            | Stage::Extract
            | Stage::Associate
            | Stage::Pursuit
            | Stage::Retention
            | Stage::ArmDedupe
            | Stage::Context
            | Stage::Remind
            | Stage::Probe
            | Stage::Fetch => false,
        }
    }

    /// `0` foreground (someone is watching a capture), `1` background. Exhaustive on purpose:
    /// a new stage must answer this rather than inherit a wildcard arm.
    pub fn class(self) -> i64 {
        match self {
            Stage::Synthesize
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
            | Stage::Context
            | Stage::Remind
            | Stage::Reap
            | Stage::Probe
            | Stage::Condense
            | Stage::Fetch => 1,
        }
    }

    /// Not the same as class 0: `age_background` writes `class = 0` onto aged sweeps, while the
    /// stage is never rewritten.
    pub fn reads_a_capture(self) -> bool {
        match self {
            Stage::Synthesize
            | Stage::SegmentWindow
            | Stage::Title
            | Stage::Embed
            | Stage::Describe
            | Stage::Extract => true,
            Stage::Consolidate
            | Stage::Dedupe
            | Stage::Relate
            | Stage::Associate
            | Stage::LinkJudge
            | Stage::Pursuit
            | Stage::Generate
            | Stage::Retention
            | Stage::ArmDedupe
            | Stage::Context
            | Stage::Remind
            | Stage::Reap
            | Stage::Probe
            | Stage::Condense
            | Stage::Fetch => false,
        }
    }

    pub fn parse(s: &str) -> Option<Stage> {
        match s {
            "synthesize" => Some(Stage::Synthesize),
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
            "remind" => Some(Stage::Remind),
            "reap" => Some(Stage::Reap),
            "probe" => Some(Stage::Probe),
            "condense" => Some(Stage::Condense),
            "fetch" => Some(Stage::Fetch),
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

#[derive(Debug, Clone, serde::Serialize)]
pub struct RetryingJob {
    pub stage: String,
    pub target_id: String,
    pub attempts: i64,
    pub next_attempt_secs: i64,
    pub last_error: Option<String>,
}

/// An on-demand model load takes ten minutes; six hours heals the same day while capping
/// text the model never accepts at four calls a day.
pub fn backoff_secs(attempts: i64) -> i64 {
    let exp = attempts.clamp(1, 16) as u32;
    2i64.saturating_pow(exp).min(21_600)
}

/// Units failing together would re-arm together against a shared limiter. Additive, so a
/// refusal is never cheaper to retry; half a gap, so 1.5 × 2ⁿ stays below 2ⁿ⁺¹.
fn spread_backoff(attempts: i64) -> i64 {
    use std::hash::{BuildHasher, Hasher};
    let gap = backoff_secs(attempts);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_i64(now());
    gap + (h.finish() % (gap.max(1) as u64 / 2 + 1)) as i64
}

/// Each variant carries its whole statement, so nothing is assembled at runtime.
#[derive(Clone, Copy)]
enum Guard {
    /// Anything, running included. An operator's reprocess.
    Any,
    /// `'failed'` is closed too: older bases still hold such rows, and a periodic sweep in that
    /// state would never be armed again.
    Closed,
}

/// The queue is a second database, so there is no shared transaction: callers must
/// **commit first, arm after**, or a worker closes the unit as deleted.
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
    // runtime: the conflict clause depends on the guard.
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

/// Spelled out once per guard so every statement is readable in the source.
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

    /// Batches need `seq` or a later document waits behind all windows of an earlier one.
    /// Re-arms whatever the state, running included: an operator's reprocess only.
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

    /// Re-arming a queued unit resets `attempts`, and a window the model refuses then never
    /// reaches the ceiling that lets its document settle.
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

    /// Guarded on the row being closed: pushing a queued sweep further out each pass would
    /// recede it forever. `'failed'` counts as closed (see `Guard::Closed`).
    pub async fn arm_periodic(
        &self,
        stage: Stage,
        target_kind: &str,
        target_id: &str,
        run_after: i64,
    ) -> Result<()> {
        sqlx::query!(
            "INSERT INTO jobs (subject, stage, target_kind, target_id, state, attempts, run_after, created_at, seq, class)
             VALUES (?, ?, ?, ?, 'pending', 0, ?, ?, 0, ?)
             ON CONFLICT(subject, stage, target_id) DO UPDATE SET
               state = 'pending', attempts = 0, run_after = excluded.run_after,
               last_error = NULL, claimed_at = NULL,
               created_at = excluded.created_at, class = excluded.class
             WHERE jobs.state IN ('done', 'failed')",
            &self.subject,
            stage.as_str(),
            target_kind,
            target_id,
            run_after,
            now(),
            stage.class()
        )
            .execute(&self.control.pool)
        .await?;
        Ok(())
    }

    /// A running row is left alone: the run re-arms at its end. Attempts reset: new information.
    pub async fn arm_at(
        &self,
        stage: Stage,
        target_kind: &str,
        target_id: &str,
        run_after: i64,
    ) -> Result<()> {
        sqlx::query!(
            "INSERT INTO jobs (subject, stage, target_kind, target_id, state, attempts, run_after, created_at, seq, class, empty_runs)
             VALUES (?, ?, ?, ?, 'pending', 0, ?, ?, 0, ?, 0)
             ON CONFLICT(subject, stage, target_id) DO UPDATE SET
               state = 'pending', run_after = excluded.run_after, attempts = 0, empty_runs = 0,
               last_error = NULL, claimed_at = NULL, class = excluded.class
             WHERE jobs.state IN ('pending', 'done', 'failed')",
            &self.subject,
            stage.as_str(),
            target_kind,
            target_id,
            run_after,
            now(),
            stage.class()
        )
            .execute(&self.control.pool)
        .await?;
        Ok(())
    }

    /// Marked done, not deleted, so `arm_at` can wake it again.
    pub async fn disarm(&self, stage: Stage, target_id: &str) -> Result<()> {
        sqlx::query!(
            "UPDATE jobs SET state = 'done' WHERE subject = ? AND stage = ? AND target_id = ? AND state = 'pending'",
            &self.subject,
            stage.as_str(),
            target_id
        )
            .execute(&self.control.pool)
            .await?;
        Ok(())
    }

    pub async fn arm_periodic_with_backoff(
        &self,
        stage: Stage,
        target_kind: &str,
        target_id: &str,
        run_after: i64,
        empty_runs: i64,
    ) -> Result<()> {
        sqlx::query!(
            "INSERT INTO jobs (subject, stage, target_kind, target_id, state, attempts, run_after, created_at, seq, class, empty_runs)
             VALUES (?, ?, ?, ?, 'pending', 0, ?, ?, 0, ?, ?)
             ON CONFLICT(subject, stage, target_id) DO UPDATE SET
               state = 'pending', attempts = 0, run_after = excluded.run_after,
               last_error = NULL, claimed_at = NULL,
               created_at = excluded.created_at, class = excluded.class,
               empty_runs = excluded.empty_runs
             WHERE jobs.state IN ('done', 'failed')",
            &self.subject,
            stage.as_str(),
            target_kind,
            target_id,
            run_after,
            now(),
            stage.class(),
            empty_runs
        )
            .execute(&self.control.pool)
        .await?;
        Ok(())
    }

    pub async fn empty_runs(&self, stage: Stage, target_id: &str) -> Result<i64> {
        Ok(sqlx::query_scalar!(
            "SELECT empty_runs FROM jobs WHERE subject = ? AND stage = ? AND target_id = ?",
            &self.subject,
            stage.as_str(),
            target_id
        )
        .fetch_optional(&self.control.pool)
        .await?
        .unwrap_or(0))
    }

    /// A sleeping unit has `run_after` pulled forward without resetting attempts; a closed one
    /// is armed outright; a running one is untouched. Class and `created_at` reset, so ageing
    /// does not count time spent asleep.
    pub async fn arm_now(&self, stage: Stage, target_kind: &str, target_id: &str) -> Result<()> {
        // First and on its own: it is true of a row in any state, including running.
        sqlx::query!(
            "UPDATE jobs SET empty_runs = 0
              WHERE subject = ? AND stage = ? AND target_id = ?",
            &self.subject,
            stage.as_str(),
            target_id
        )
        .execute(&self.control.pool)
        .await?;
        sqlx::query!(
            "UPDATE jobs SET run_after = 0, class = ?, created_at = ?
              WHERE subject = ? AND stage = ? AND target_id = ?
                AND state = 'pending' AND run_after > 0",
            stage.class(),
            now(),
            &self.subject,
            stage.as_str(),
            target_id
        )
        .execute(&self.control.pool)
        .await?;
        self.rearm_idle_seq(stage, target_kind, target_id, 0).await
    }

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

    /// So a unit that re-arms itself climbs rather than re-entering at the front of its batch.
    pub async fn job_seq(&self, stage: Stage, target_id: &str) -> Result<Option<i64>> {
        Ok(sqlx::query_scalar!(
            "SELECT seq FROM jobs WHERE subject = ? AND stage = ? AND target_id = ?",
            &self.subject,
            stage.as_str(),
            target_id
        )
        .fetch_optional(&self.control.pool)
        .await?)
    }

    /// `Some` for queued or running; a sweep checks this before spending budget on a unit.
    pub async fn live_job(&self, stage: Stage, target_id: &str) -> Result<bool> {
        Ok(sqlx::query_scalar!(
            "SELECT 1 FROM jobs
              WHERE subject = ? AND stage = ? AND target_id = ?
                AND state IN ('pending', 'running')",
            &self.subject,
            stage.as_str(),
            target_id
        )
        .fetch_optional(&self.control.pool)
        .await?
        .is_some())
    }

    /// Only for an operator asking for the work again: the surviving row is what lets a
    /// stage give up for good.
    pub async fn delete_job(&self, stage: Stage, target_id: &str) -> Result<()> {
        sqlx::query!(
            "DELETE FROM jobs WHERE subject = ? AND stage = ? AND target_id = ?",
            &self.subject,
            stage.as_str(),
            target_id
        )
        .execute(&self.control.pool)
        .await?;
        Ok(())
    }

    /// Units are keyed `corpus#idx` and outlive their windows. Matched on the prefix, not
    /// `LIKE`, so a wildcard in an id cannot widen the delete.
    pub async fn delete_window_jobs(&self, corpus_id: &str) -> Result<()> {
        sqlx::query!(
            "DELETE FROM jobs
              WHERE subject = ? AND stage = 'segment_window'
                AND substr(target_id, 1, length(?) + 1) = ? || '#'",
            &self.subject,
            corpus_id,
            corpus_id
        )
        .execute(&self.control.pool)
        .await?;
        Ok(())
    }

    /// A row survives completion, so this separates "never asked" from "done asking".
    pub async fn has_job(&self, stage: Stage, target_id: &str) -> Result<bool> {
        Ok(self.job_seq(stage, target_id).await?.is_some())
    }

    pub async fn job_counts(&self) -> Result<Vec<(String, i64)>> {
        let rows = sqlx::query!(
            "SELECT state, COUNT(*) AS n FROM jobs WHERE subject = ? GROUP BY state",
            &self.subject
        )
        .fetch_all(&self.control.pool)
        .await?;
        Ok(rows.into_iter().map(|r| (r.state, r.n)).collect())
    }

    /// `attempts > 0` separates trouble from merely queued.
    pub async fn retrying_jobs(&self, limit: i64) -> Result<Vec<RetryingJob>> {
        let rows = sqlx::query!(
            "SELECT stage, target_id, attempts, last_error, run_after FROM jobs
              WHERE subject = ? AND state = 'pending' AND attempts > 0 AND run_after > ?
              ORDER BY run_after LIMIT ?",
            &self.subject,
            now(),
            limit
        )
        .fetch_all(&self.control.pool)
        .await?;
        let at = now();
        Ok(rows
            .into_iter()
            .map(|r| RetryingJob {
                stage: r.stage,
                target_id: r.target_id,
                attempts: r.attempts,
                next_attempt_secs: (r.run_after - at).max(0),
                last_error: r.last_error,
            })
            .collect())
    }

    pub async fn failed_jobs(&self, limit: i64) -> Result<Vec<FailedJob>> {
        let rows = sqlx::query!(
            "SELECT id, stage, target_id, attempts, last_error FROM jobs
              WHERE subject = ? AND state = 'failed' ORDER BY id DESC LIMIT ?",
            &self.subject,
            limit
        )
        .fetch_all(&self.control.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| FailedJob {
                id: r.id,
                stage: r.stage,
                target_id: r.target_id,
                attempts: r.attempts,
                last_error: r.last_error,
            })
            .collect())
    }

    /// From `created_at`: a never-delayed job has `run_after = 0`.
    pub async fn oldest_pending_age(&self) -> Result<Option<i64>> {
        let row = sqlx::query!(
            "SELECT MIN(created_at) AS oldest FROM jobs WHERE subject = ? AND state = 'pending'",
            &self.subject
        )
        .fetch_one(&self.control.pool)
        .await?;
        let oldest: Option<i64> = row.oldest;
        Ok(oldest.map(|t| (now() - t).max(0)))
    }

    /// Not any pending row (periodic sweeps park far out) and not `class = 0` (aged sweeps).
    /// Counts reading stages that are running and not stuck, pending and due, or retrying.
    pub async fn capture_being_read(&self) -> Result<bool> {
        // Built from the stage list, so a new stage answers `reads_a_capture`.
        let stages: Vec<&str> = Stage::ALL
            .iter()
            .filter(|s| s.reads_a_capture())
            .map(|s| s.as_str())
            .collect();
        // Only `Stage::as_str`, a `&'static str` from a match arm, is interpolated.
        let sql = sqlx::AssertSqlSafe(format!(
            "SELECT 1 FROM jobs
              WHERE subject = ? AND stage IN ('{}')
                AND (   (state = 'running' AND claimed_at > ?)
                     OR (state = 'pending' AND (run_after <= ? OR attempts > 0)))
              LIMIT 1",
            stages.join("','")
        ));
        // runtime: one placeholder per item in the IN list.
        let r: Option<i64> = sqlx::query_scalar(sql)
            .bind(&self.subject)
            .bind(now() - crate::jobs::STUCK_AFTER_SECS)
            .bind(now())
            .fetch_optional(&self.control.pool)
            .await?;
        Ok(r.is_some())
    }
}

/// Instance-wide: one worker pool serves every tenant.
impl Control {
    /// One statement under SQLite's write lock, so two workers never take the same row. Order:
    /// class, attempts (a failing job yields), seq (interleaves batches and tenants), id.
    pub async fn claim_job(&self) -> Result<Option<(String, Job)>> {
        self.claim_job_holding(&[]).await
    }

    /// A held unit is untouched: not claimed, not failed, its place kept.
    pub async fn claim_job_holding(&self, held: &[Stage]) -> Result<Option<(String, Job)>> {
        let except = if held.is_empty() {
            String::new()
        } else {
            format!("AND stage NOT IN ({})", vec!["?"; held.len()].join(", "))
        };
        let sql = format!(
            "UPDATE jobs
                SET state = 'running', claimed_at = ?, attempts = attempts + 1
              WHERE id = (
                SELECT id FROM jobs
                 WHERE state = 'pending' AND run_after <= ? {except}
                 ORDER BY class, attempts, seq, id LIMIT 1
              )
              RETURNING id, subject, stage, target_kind, target_id, attempts"
        );
        // runtime: the claim leaves out a variable list of held stages.
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(now())
            .bind(now());
        for stage in held {
            q = q.bind(stage.as_str());
        }
        let row = q.fetch_optional(&self.pool).await?;

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

    pub async fn waiting_on(&self, subject: &str, stages: &[Stage]) -> Result<i64> {
        if stages.is_empty() {
            return Ok(0);
        }
        let sql = format!(
            "SELECT COUNT(*) FROM jobs WHERE subject = ? AND state = 'pending' AND stage IN ({})",
            vec!["?"; stages.len()].join(", ")
        );
        // runtime: one placeholder per item in the IN list.
        let mut q = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql)).bind(subject);
        for stage in stages {
            q = q.bind(stage.as_str());
        }
        Ok(q.fetch_one(&self.pool).await?)
    }

    pub async fn complete_job(&self, id: i64) -> Result<()> {
        sqlx::query!(
            "UPDATE jobs SET state = 'done', last_error = NULL, claimed_at = NULL WHERE id = ?",
            id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// No terminal state: past `MAX_ATTEMPTS` the delay is at its ceiling, so an outage heals.
    pub async fn fail_job(&self, id: i64, attempts: i64, err: &str) -> Result<()> {
        sqlx::query!(
            "UPDATE jobs SET state = 'pending', run_after = ?, last_error = ?, claimed_at = NULL WHERE id = ?",
            now() + spread_backoff(attempts),
            err,
            id
        )
            .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Rows left 'running' by a crashed process. Called once at startup.
    pub async fn reclaim_stuck(&self, older_than_secs: i64) -> Result<u64> {
        let res = sqlx::query!(
            "UPDATE jobs SET state = 'pending', run_after = 0, claimed_at = NULL
              WHERE state = 'running' AND claimed_at IS NOT NULL AND claimed_at < ?",
            now() - older_than_secs
        )
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// Written, not computed at claim: an inequality in the ordering defeats `idx_jobs_claim3`.
    /// The wait runs from the later of `created_at` and `run_after`, so sleeping is not waiting.
    /// At most `limit` rows per tenant per call, oldest first: a backlog must not jump a capture.
    pub async fn age_background(&self, subject: &str, older_than: i64, limit: i64) -> Result<u64> {
        let res = sqlx::query!(
            "UPDATE jobs SET class = 0
              WHERE id IN (
                SELECT id FROM jobs
                 WHERE subject = ? AND state = 'pending' AND class = 1
                   AND max(created_at, run_after) < ?
                 ORDER BY max(created_at, run_after)
                 LIMIT ?)",
            subject,
            older_than,
            limit
        )
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }
}

/// Callers deep inside the job runner keep talking to the store they hold.
impl Store {
    pub async fn complete_job(&self, id: i64) -> Result<()> {
        self.control.complete_job(id).await
    }

    pub async fn fail_job(&self, id: i64, attempts: i64, err: &str) -> Result<()> {
        self.control.fail_job(id, attempts, err).await
    }

    /// Scoped to this store's subject; the tests drive it through a single base.
    pub async fn age_background(&self, older_than: i64, limit: i64) -> Result<u64> {
        self.control
            .age_background(&self.subject, older_than, limit)
            .await
    }

    /// The workers claim instance-wide; this is for tests stepping a single base.
    pub async fn claim_job(&self) -> Result<Option<Job>> {
        let row = sqlx::query!(
            "UPDATE jobs
                SET state = 'running', claimed_at = ?, attempts = attempts + 1
              WHERE id = (
                SELECT id FROM jobs
                 WHERE subject = ? AND state = 'pending' AND run_after <= ?
                 ORDER BY class, attempts, seq, id LIMIT 1
              )
              RETURNING id, stage, target_kind, target_id, attempts",
            now(),
            &self.subject,
            now()
        )
        .fetch_optional(&self.control.pool)
        .await?;

        Ok(row.map(|r| Job {
            id: r.id,
            stage: Stage::parse(r.stage.as_str()).unwrap_or(Stage::Synthesize),
            target_kind: r.target_kind,
            target_id: r.target_id,
            attempts: r.attempts,
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

    #[test]
    fn a_spread_gap_never_falls_below_its_rung_or_reaches_the_next() {
        for attempts in 1..=MAX_ATTEMPTS {
            let gap = backoff_secs(attempts);
            for _ in 0..64 {
                let spread = spread_backoff(attempts);
                assert!(spread >= gap, "{spread} fell below rung {attempts}");
                assert!(
                    spread < backoff_secs(attempts + 1),
                    "{spread} overtook the rung above {attempts}"
                );
            }
        }
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
        let s = Store::memory().await.unwrap();
        // The second document's first window must beat the first document's second.
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
        // An inequality or computed age in the ordering turns every poll into a temp B-tree sort.
        let s = Store::memory().await.unwrap();
        let rows = sqlx::query!(
            "EXPLAIN QUERY PLAN
             SELECT id FROM jobs
              WHERE state = 'pending' AND run_after <= ?
              ORDER BY class, attempts, seq, id LIMIT 1",
            now()
        )
        .fetch_all(&s.control.pool)
        .await
        .unwrap();
        let plan = rows
            .into_iter()
            .map(|r| r.detail.unwrap_or_default())
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
        let s = Store::memory().await.unwrap();
        s.enqueue(Stage::Associate, "collection", "collection")
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE jobs SET created_at = ? WHERE stage = 'associate'",
            now() - 7200
        )
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
        let class: i64 = sqlx::query_scalar!("SELECT class FROM jobs WHERE stage = 'associate'")
            .fetch_one(&s.control.pool)
            .await
            .unwrap();
        assert_eq!(class, 0, "having aged must be durable, not recomputed");
    }

    #[tokio::test]
    async fn only_so_many_units_may_age_on_one_pass() {
        // Within one class the order is `attempts, seq, id`, so an aged backlog would all go first.
        let s = Store::memory().await.unwrap();
        for i in 0..5 {
            s.enqueue(Stage::Relate, "artifact", &format!("a-{i}"))
                .await
                .unwrap();
        }
        sqlx::query!(
            "UPDATE jobs SET created_at = ? WHERE stage = 'relate'",
            now() - 7200
        )
        .execute(&s.control.pool)
        .await
        .unwrap();

        assert_eq!(s.age_background(now() - 3600, 2).await.unwrap(), 2);

        let still: i64 =
            sqlx::query_scalar!("SELECT count(*) FROM jobs WHERE class = 1 AND stage = 'relate'")
                .fetch_one(&s.control.pool)
                .await
                .unwrap();
        assert_eq!(still, 3, "the rest wait for the next pass");

        assert_eq!(s.age_background(now() - 3600, 100).await.unwrap(), 3);
    }

    #[tokio::test]
    async fn the_oldest_units_age_first() {
        let s = Store::memory().await.unwrap();
        for i in 0..3 {
            s.enqueue(Stage::Relate, "artifact", &format!("a-{i}"))
                .await
                .unwrap();
        }
        sqlx::query!(
            "UPDATE jobs SET created_at = ? WHERE target_id = 'a-2'",
            now() - 86_400
        )
        .execute(&s.control.pool)
        .await
        .unwrap();
        sqlx::query!(
            "UPDATE jobs SET created_at = ? WHERE target_id <> 'a-2'",
            now() - 7200
        )
        .execute(&s.control.pool)
        .await
        .unwrap();

        assert_eq!(s.age_background(now() - 3600, 1).await.unwrap(), 1);

        let aged: String =
            sqlx::query_scalar!("SELECT target_id FROM jobs WHERE class = 0 AND stage = 'relate'")
                .fetch_one(&s.control.pool)
                .await
                .unwrap();
        assert_eq!(aged, "a-2", "the longest wait goes first");
    }

    #[tokio::test]
    async fn a_sweep_asleep_on_its_own_period_is_not_waiting() {
        // `arm_periodic` stamps `created_at` on reschedule, so a resting sweep looks queued.
        let s = Store::memory().await.unwrap();
        s.arm_periodic(Stage::Retention, "collection", "collection", now() + 21_600)
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE jobs SET created_at = ? WHERE stage = 'retention'",
            now() - 7200
        )
        .execute(&s.control.pool)
        .await
        .unwrap();

        assert_eq!(
            s.age_background(now() - 3600, 100).await.unwrap(),
            0,
            "a unit that is not due yet has not been waiting"
        );

        sqlx::query!("UPDATE jobs SET run_after = 0 WHERE stage = 'retention'")
            .execute(&s.control.pool)
            .await
            .unwrap();
        assert_eq!(s.age_background(now() - 3600, 100).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn a_sweep_that_has_just_come_due_has_not_been_waiting() {
        // The wait is measured from when the unit became ready.
        let s = Store::memory().await.unwrap();
        s.arm_periodic(Stage::Retention, "collection", "collection", now() + 21_600)
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE jobs SET created_at = ?, run_after = ? WHERE stage = 'retention'",
            now() - 21_600,
            now() - 60
        )
        .execute(&s.control.pool)
        .await
        .unwrap();

        assert_eq!(
            s.age_background(now() - 3600, 100).await.unwrap(),
            0,
            "a sweep one minute past due has not waited an hour for a worker"
        );

        sqlx::query!(
            "UPDATE jobs SET run_after = ? WHERE stage = 'retention'",
            now() - 7200
        )
        .execute(&s.control.pool)
        .await
        .unwrap();
        assert_eq!(s.age_background(now() - 3600, 100).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn arming_a_sleeping_unit_starts_its_wait_over() {
        // `arm_now` zeroes `run_after`, leaving an interval-old `created_at` as the only stamp.
        let s = Store::memory().await.unwrap();
        s.arm_periodic(Stage::Pursuit, "collection", "collection", now() + 600)
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE jobs SET created_at = ? WHERE stage = 'pursuit'",
            now() - 7200
        )
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
        // Older bases hold `failed` rows; refusing to arm them stops the sweep for good.
        let s = Store::memory().await.unwrap();
        s.arm_periodic(Stage::Consolidate, "collection", "collection", 0)
            .await
            .unwrap();
        sqlx::query!("UPDATE jobs SET state = 'failed' WHERE stage = 'consolidate'")
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
        // Pulling a unit forward makes it one that has not waited.
        let s = Store::memory().await.unwrap();
        s.arm_periodic(Stage::Pursuit, "collection", "collection", now() + 600)
            .await
            .unwrap();
        sqlx::query!("UPDATE jobs SET class = 0 WHERE stage = 'pursuit'")
            .execute(&s.control.pool)
            .await
            .unwrap();

        s.arm_now(Stage::Pursuit, "collection", "collection")
            .await
            .unwrap();

        let (run_after, class): (i64, i64) =
            sqlx::query!("SELECT run_after, class FROM jobs WHERE stage = 'pursuit'")
                .fetch_one(&s.control.pool)
                .await
                .map(|r| (r.run_after, r.class))
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

    #[tokio::test]
    async fn concurrent_workers_never_claim_the_same_job_twice() {
        use std::collections::HashSet;
        use std::sync::{Arc, Mutex};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.db");
        let store = Store::connect(
            &path.to_string_lossy(),
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
        assert!(s.claim_job().await.unwrap().is_none());

        for _ in 0..MAX_ATTEMPTS + 3 {
            sqlx::query!("UPDATE jobs SET run_after = 0")
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

        sqlx::query!("UPDATE jobs SET run_after = 0")
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
        // `fail_job` re-arms in place, so a failing job keeps its original id.
        let s = Store::memory().await.unwrap();
        s.enqueue(Stage::Synthesize, "corpus", "sore-thumb")
            .await
            .unwrap();
        for _ in 0..4 {
            let j = s.claim_job().await.unwrap().unwrap();
            s.fail_job(j.id, 0, "malformed llm output").await.unwrap();
            sqlx::query!("UPDATE jobs SET run_after = 0")
                .execute(&s.control.pool)
                .await
                .unwrap();
        }

        s.enqueue(Stage::Synthesize, "corpus", "fresh")
            .await
            .unwrap();
        sqlx::query!("UPDATE jobs SET run_after = 0")
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
        // A reordering, not a demotion: the failing job still runs when nothing fresher is ready.
        let s = Store::memory().await.unwrap();
        s.enqueue(Stage::Synthesize, "corpus", "sore-thumb")
            .await
            .unwrap();
        for _ in 0..4 {
            let j = s.claim_job().await.unwrap().unwrap();
            s.fail_job(j.id, 0, "malformed llm output").await.unwrap();
            sqlx::query!("UPDATE jobs SET run_after = 0")
                .execute(&s.control.pool)
                .await
                .unwrap();
        }
        sqlx::query!("UPDATE jobs SET run_after = 0")
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
        // A 34-window document takes 34 consecutive row ids.
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
        let s = Store::memory().await.unwrap();
        s.enqueue_seq(Stage::SegmentWindow, "segment", "sore#0", 0)
            .await
            .unwrap();
        let j = s.claim_job().await.unwrap().unwrap();
        s.fail_job(j.id, 0, "malformed llm output").await.unwrap();

        s.enqueue_seq(Stage::SegmentWindow, "segment", "fresh#9", 9)
            .await
            .unwrap();
        sqlx::query!("UPDATE jobs SET run_after = 0")
            .execute(&s.control.pool)
            .await
            .unwrap();

        let next = s.claim_job().await.unwrap().unwrap();
        assert_eq!(next.target_id, "fresh#9", "a failing unit kept the front");
    }

    #[tokio::test]
    async fn arming_a_unit_a_worker_is_already_inside_leaves_it_alone() {
        // Re-queueing a running unit hands it to a second worker mid-call; both delete each
        // other's artifacts.
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

        // Already going to run, so the sweep must not wind it back.
        s.rearm_idle_seq(Stage::SegmentWindow, "segment", "src-1#0", 0)
            .await
            .unwrap();
        let (attempts, run_after): (i64, i64) =
            sqlx::query!("SELECT attempts, run_after FROM jobs WHERE target_id = 'src-1#0'")
                .fetch_one(&s.control.pool)
                .await
                .map(|r| (r.attempts, r.run_after))
                .unwrap();
        assert_eq!(attempts, 1, "the sweep reset a unit's attempt count");
        assert!(run_after > now(), "the sweep cleared a unit's backoff");

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
    async fn a_parked_sweep_is_not_a_capture_in_flight() {
        let s = Store::memory().await.unwrap();
        assert!(!s.capture_being_read().await.unwrap(), "an idle base");

        // Periodic units sit `pending` months out on every live install.
        s.arm_at(
            Stage::Consolidate,
            "sweep",
            "consolidate",
            crate::store::now() + 86_400,
        )
        .await
        .unwrap();
        assert!(
            s.oldest_pending_age().await.unwrap().is_some(),
            "the old question says yes"
        );
        assert!(
            !s.capture_being_read().await.unwrap(),
            "and it is nobody's capture"
        );

        s.enqueue(Stage::Synthesize, "corpus", "src-1")
            .await
            .unwrap();
        assert!(s.capture_being_read().await.unwrap());
        let j = s.claim_job().await.unwrap().unwrap();
        assert!(s.capture_being_read().await.unwrap(), "running counts");
        s.control.complete_job(j.id).await.unwrap();
        assert!(
            !s.capture_being_read().await.unwrap(),
            "and it is over when it is done"
        );
    }

    #[tokio::test]
    async fn reading_is_the_capture_pipeline_and_nothing_parked() {
        let s = Store::memory().await.unwrap();
        while let Some(j) = s.claim_job().await.unwrap() {
            s.control.complete_job(j.id).await.unwrap();
        }
        assert!(!s.capture_being_read().await.unwrap());
        s.enqueue(Stage::Synthesize, "corpus", "c-1").await.unwrap();
        assert!(
            s.capture_being_read().await.unwrap(),
            "the operator is waiting for this one"
        );
        let j = s.claim_job().await.unwrap().unwrap();
        s.control.complete_job(j.id).await.unwrap();
        assert!(!s.capture_being_read().await.unwrap());

        // `remind` is a periodic class-1 notifier parked months out.
        s.arm_at(
            Stage::Remind,
            "sweep",
            "remind",
            crate::store::now() + 86_400,
        )
        .await
        .unwrap();
        assert!(
            !s.capture_being_read().await.unwrap(),
            "a parked sweep is nobody's capture"
        );
    }

    #[tokio::test]
    async fn a_unit_backing_off_after_a_failure_is_still_being_read() {
        // A retry during its backoff is still a capture being read.
        let s = Store::memory().await.unwrap();
        s.enqueue(Stage::Synthesize, "corpus", "c-1").await.unwrap();
        let j = s.claim_job().await.unwrap().unwrap();
        s.fail_job(j.id, j.attempts, "the model was unreachable")
            .await
            .unwrap();

        let run_after: i64 = sqlx::query_scalar!("SELECT run_after FROM jobs WHERE id = ?", j.id)
            .fetch_one(&s.control.pool)
            .await
            .unwrap();
        assert!(run_after > now(), "the retry is parked, not due");
        assert!(
            s.capture_being_read().await.unwrap(),
            "a capture waiting out its backoff is still being read"
        );
    }

    #[tokio::test]
    async fn a_promoted_sweep_is_not_a_capture_being_read() {
        // `age_background` writes `class = 0` onto a sweep, so class alone cannot answer this.
        let s = Store::memory().await.unwrap();
        s.enqueue(Stage::Relate, "artifact", "a-1").await.unwrap();
        s.age_background(now() + 60, 10).await.unwrap();
        let class: i64 = sqlx::query_scalar!("SELECT class FROM jobs WHERE stage = 'relate'")
            .fetch_one(&s.control.pool)
            .await
            .unwrap();
        assert_eq!(class, 0, "the backlog unit was promoted");
        assert!(!s.capture_being_read().await.unwrap());
    }

    #[tokio::test]
    async fn a_claim_older_than_the_stuck_threshold_is_nobody_reading() {
        // `reclaim_stuck` runs once at startup, so a stale `running` row needs the age check.
        let s = Store::memory().await.unwrap();
        s.enqueue(Stage::Synthesize, "corpus", "c-1").await.unwrap();
        let j = s.claim_job().await.unwrap().unwrap();
        assert!(s.capture_being_read().await.unwrap(), "a fresh claim");
        sqlx::query!(
            "UPDATE jobs SET claimed_at = ? WHERE id = ?",
            now() - crate::jobs::STUCK_AFTER_SECS - 1,
            j.id
        )
        .execute(&s.control.pool)
        .await
        .unwrap();
        assert!(
            !s.capture_being_read().await.unwrap(),
            "nothing is reading this one; the process that claimed it is gone"
        );
    }

    #[tokio::test]
    async fn stuck_running_jobs_are_reclaimed_after_a_crash() {
        let s = Store::memory().await.unwrap();
        s.enqueue(Stage::Synthesize, "corpus", "src-1")
            .await
            .unwrap();
        let j = s.claim_job().await.unwrap().unwrap();
        sqlx::query!(
            "UPDATE jobs SET claimed_at = ? WHERE id = ?",
            crate::store::now() - 3600,
            j.id
        )
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

        sqlx::query!("UPDATE jobs SET created_at = ?", crate::store::now() - 3600)
            .execute(&s.control.pool)
            .await
            .unwrap();
        let age = s.oldest_pending_age().await.unwrap().unwrap();
        assert!((3595..=3605).contains(&age), "got {age}");
    }

    #[test]
    fn backoff_climbs_to_hours_and_stops_there() {
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
        let state: String = sqlx::query_scalar!("SELECT state FROM jobs WHERE id = ?", job.id)
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
        sqlx::query!("UPDATE jobs SET state='failed' WHERE id = ?", j.id)
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

        let n: i64 = sqlx::query_scalar!("SELECT count(*) FROM jobs")
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

    #[test]
    fn the_two_time_stages_round_trip_and_are_background() {
        assert_eq!(Stage::parse("remind"), Some(Stage::Remind));
        assert_eq!(Stage::Remind.class(), 1);
    }

    #[tokio::test]
    async fn a_held_stage_is_passed_over_and_not_penalised() {
        let s = Store::memory().await.unwrap();
        s.enqueue(Stage::SegmentWindow, "segment", "c#0")
            .await
            .unwrap();
        s.enqueue(Stage::Embed, "corpus", "c").await.unwrap();
        let (_, first) = s
            .control
            .claim_job_holding(&Stage::GENERATORS)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.stage, Stage::Embed);
        assert!(
            s.control
                .claim_job_holding(&Stage::GENERATORS)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            s.control
                .waiting_on(&s.subject, &Stage::GENERATORS)
                .await
                .unwrap(),
            1
        );
        let (_, held) = s.control.claim_job().await.unwrap().unwrap();
        assert_eq!((held.stage, held.attempts), (Stage::SegmentWindow, 1));
    }

    #[test]
    fn the_stages_that_call_a_generator_are_the_seven() {
        let named: Vec<_> = Stage::ALL
            .into_iter()
            .filter(|s| s.calls_a_generator())
            .collect();
        assert_eq!(named, Stage::GENERATORS);
    }
}
