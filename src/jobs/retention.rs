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
    pub clusters: usize,
    pub named: usize,
    pub removed: usize,
}

pub async fn run(core: &Core) -> Result<Report> {
    let mut report = Report::default();

    // Each half is reported on its own and neither stops the other: they are
    // independent, both are retried on the next run, and a base whose grouping
    // is failing still wants its log trimmed.
    if core.feedback.retain_days > 0 {
        match core.store.expire_feedback(core.feedback.retain_days).await {
            Ok(n) => {
                if n > 0 {
                    tracing::info!(dropped = n, "expired captured searches and questions");
                }
                report.expired = n;
            }
            Err(e) => tracing::warn!(error = %e, "could not expire captured searches"),
        }
    }

    if core.feedback.enabled {
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
                report.clusters = r.clusters;
                report.named = r.named;
                report.removed = r.removed;
            }
            Err(e) => tracing::warn!(error = %e, "could not group knowledge gaps"),
        }
    }

    Ok(report)
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
        core.feedback.enabled = true;
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
        core.feedback.enabled = true;
        core.feedback.retain_days = 30;
        seed_old_event(&core).await;

        run(&core).await.unwrap();

        assert_eq!(captured(&core).await, 0);
    }
}
