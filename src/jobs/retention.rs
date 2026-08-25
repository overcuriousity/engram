//! Expire what is past keeping, then regroup what is left.
//!
//! One unit, two passes, in this order: grouping reads the rows expiring
//! removes, and grouping first would name a cluster around a search deleted a
//! second later. It was one ticker before it was one unit, and the ordering
//! inside it is the half that was already written down.
//!
//! Runs even with capture disabled, so turning capture off also expires what it
//! recorded while it was on.

use crate::core::Core;
use crate::error::Result;

/// What one pass did. The counts the two halves already returned; nothing here
/// is counted for the account's sake.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Report {
    /// Searches and questions dropped for being past `retain_days`.
    pub expired: u64,
    pub standing: Standing,
    pub named: usize,
    pub removed: usize,
    /// Situations dropped for being past `store::context::RETAIN_DAYS`.
    pub contexts: u64,
    /// Interactions dropped for being past the same window.
    pub interactions: u64,
}

/// What the base holds, as opposed to what this pass did to it.
///
/// Nested, and the nesting is the point: `jobs::did_work` reads the flat
/// numbers of a report and calls any non-zero one work. `clusters` is a
/// standing count — `gaps::sweep` returns how many gap clusters exist, which is
/// why the log line beside it prints only when `named` or `removed` says
/// something happened — so at the top level it made every run over a base with
/// any gaps at all report work, and the empty-run backoff never engaged for
/// this sweep. Recorded in `sweep_runs.detail` all the same, for a person
/// reading the history.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Standing {
    /// Gap clusters the base holds after this pass, named or not.
    pub clusters: usize,
}

pub async fn run(core: &Core) -> Result<Report> {
    let mut report = Report::default();
    // The first thing that went wrong, held rather than returned. Neither half
    // stops the other — they are independent, and a base whose grouping is
    // failing still wants its log trimmed — but a pass where nothing worked
    // must not report itself as a pass. `run_accounted` reads the return value
    // and nothing else, so swallowing these into warnings is what would make
    // the `failed` flag on `sweep_runs` unreachable for this sweep, and the
    // history's whole reason for existing is that a sweep going wrong is
    // visible there rather than only in the log.
    //
    // The error is carried as itself, not flattened to a string: `retryable`
    // classifies the variant, and that decides whether the worker spends
    // another attempt.
    let mut failure: Option<crate::error::Error> = None;

    if core.feedback.retain_days > 0 {
        match core.store.expire_feedback(core.feedback.retain_days).await {
            Ok(n) => {
                if n > 0 {
                    tracing::info!(dropped = n, "expired captured searches and questions");
                }
                report.expired = n;
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not expire captured searches");
                failure.get_or_insert(e);
            }
        }
    }

    if core.learn.enabled {
        match crate::jobs::gaps::sweep(core).await {
            Ok(r) => {
                if r.named > 0 || r.removed > 0 {
                    tracing::info!(
                        clusters = r.clusters,
                        named = r.named,
                        removed = r.removed,
                        "knowledge gaps regrouped"
                    );
                }
                report.standing.clusters = r.clusters;
                report.named = r.named;
                report.removed = r.removed;
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not group knowledge gaps");
                failure.get_or_insert(e);
            }
        }
    }

    // Its own window, and behind no key. A weekly pattern needs weeks, and
    // `feedback.retain_days` defaults to keeping for ever but is an operator
    // switch — an operator who shortens their query log is not asking the base
    // to forget what Friday afternoon looks like. Runs whenever this unit runs,
    // which is why `periodic_units` also arms it for `recommend.enabled`.
    match core
        .store
        .expire_context_events(crate::store::context::RETAIN_DAYS)
        .await
    {
        Ok(n) => {
            if n > 0 {
                tracing::info!(dropped = n, "expired recorded situations");
            }
            report.contexts = n;
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not expire recorded situations");
            failure.get_or_insert(e);
        }
    }

    // Interactions ride the situations' window, not the query log's: the two
    // are read as a pair by the context sweep, and one kept past the other
    // profiles nothing. Behind no key for the same reason the line above is —
    // an operator who turned the offer off still wants the rows it wrote while
    // it was on to leave.
    match core
        .store
        .expire_interactions(crate::store::context::RETAIN_DAYS)
        .await
    {
        Ok(n) => {
            if n > 0 {
                tracing::info!(dropped = n, "expired recorded interactions");
            }
            report.interactions = n;
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not expire recorded interactions");
            failure.get_or_insert(e);
        }
    }

    // The sweep history is trimmed by the repair pass, not here. This unit is
    // behind `feedback`, and an operator with capture off keeps no retention
    // unit at all — while the sweeps that do run keep writing a row apiece.
    // See `background::repair_once`.

    match failure {
        Some(e) => Err(e),
        None => Ok(report),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One search, old enough to be past any window.
    async fn seed_old_event(core: &Core) {
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

    async fn captured(core: &Core) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM search_events")
            .fetch_one(&core.store.pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn retention_runs_with_consolidation_switched_off() {
        // Retention used to ride on the consolidation sweep, so switching
        // duplicate hygiene off silently kept the query log forever. It is its
        // own unit now, and it stays its own unit.
        let mut core = crate::core::test_support::test_core().await;
        core.consolidate.enabled = false;
        core.learn.enabled = true;
        core.feedback.retain_days = 30;
        seed_old_event(&core).await;

        let report = run(&core).await.unwrap();

        assert_eq!(report.expired, 1);
        assert_eq!(
            captured(&core).await,
            0,
            "an event past the window outlived the retention unit"
        );
    }

    #[tokio::test]
    async fn a_sweep_where_nothing_worked_does_not_report_a_clean_run() {
        // `run_accounted` reads the return value and nothing else, so a pass
        // that warned about every half and then returned `Ok` would be written
        // into `sweep_runs` as `ok` with no counts — and the `failed` flag, the
        // stated reason that history exists, could never fire for this sweep.
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        core.feedback.retain_days = 30;
        // The expiry's table, taken out from under it.
        sqlx::query("DROP TABLE search_events")
            .execute(&core.store.pool)
            .await
            .unwrap();

        let err = run(&core).await.unwrap_err();

        assert!(
            err.to_string().contains("search_events"),
            "the failure has to reach the account, and say what failed: {err}"
        );
    }

    #[tokio::test]
    async fn keeping_forever_expires_nothing() {
        let core = crate::core::test_support::test_core().await; // retain_days defaults to 0
        seed_old_event(&core).await;

        let report = run(&core).await.unwrap();

        assert_eq!(report.expired, 0);
        assert_eq!(captured(&core).await, 1, "`0` must keep them forever");
    }

    #[tokio::test]
    async fn the_unit_groups_the_gaps() {
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
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

        run(&core).await.unwrap();

        assert_eq!(
            core.store.cluster_keys().await.unwrap().len(),
            1,
            "the gap was never grouped"
        );
    }

    #[tokio::test]
    async fn expiring_comes_before_grouping() {
        // The ordering is the whole reason these two share a unit: grouping
        // reads the rows expiring removes, and naming a cluster around a search
        // deleted a second later is a call spent on nothing.
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        core.feedback.retain_days = 30;
        seed_old_event(&core).await;

        run(&core).await.unwrap();

        assert_eq!(captured(&core).await, 0);
    }
}
