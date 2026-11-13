//! Expire what is past keeping, then regroup what is left: grouping first would name clusters
//! around rows deleted a second later.
//! Runs with capture disabled too, so turning capture off still expires what it recorded.

use crate::core::Core;
use crate::error::Result;

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Report {
    pub expired: u64,
    pub trash_purged: u64,
    pub standing: Standing,
    pub named: usize,
    pub removed: usize,
    pub contexts: u64,
    pub interactions: u64,
    pub gave_up: usize,
    pub adopted: usize,
    pub reverted: usize,
    pub undone: usize,
    pub restored: usize,
    /// Artifacts the integrate phase filed against the rest of the base, and the probes it minted.
    pub integrated: usize,
    pub probes: usize,
    /// Probes retired: another embedder, or an owner gone.
    pub retired: usize,
    /// Replayed probes that landed somewhere new: what this pass learned
    /// (`sleep::Replayed::moved`).
    pub moved: usize,
    pub condensed: usize,
}

/// Standing counts, nested so `jobs::did_work` does not read them as work; still recorded in
/// `sweep_runs.detail`.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Standing {
    pub clusters: usize,
    /// Standing although this pass ran them: the lap wraps, so it is never zero, and repeating a
    /// measurement is not work.
    pub rehearsed: usize,
    /// Standing: `sleep::interference` files nothing.
    pub interference: usize,
}

pub async fn run(core: &Core) -> Result<Report> {
    let mut report = Report::default();
    // Held, not returned: neither half stops the other, but a pass where something failed must
    // reach `run_accounted` as failed. Carried as itself, since `retryable` reads the variant.
    let mut failure: Option<crate::error::Error> = None;

    match core.purge_trash(crate::core::TRASH_RETENTION_SECS).await {
        Ok(n) => report.trash_purged = n,
        Err(e) => {
            tracing::warn!(error = %e, "could not empty the trash");
            failure.get_or_insert(e);
        }
    }

    // Before expiry, which removes the rows a give-up is read from.
    match crate::jobs::observe::run(core).await {
        Ok(n) => report.gave_up = n,
        Err(e) => {
            tracing::warn!(error = %e, "could not record what was given up on");
            failure.get_or_insert(e);
        }
    }

    // After the give-ups are written, which the generation under watch is judged on, and before
    // expiry.
    match crate::jobs::tune::run_if_quiet(core).await {
        Ok(p) => {
            report.adopted = usize::from(p.adopted.is_some());
            report.reverted = usize::from(p.reverted.is_some());
            report.undone = p.undone;
            report.restored = p.restored;
            // Flat where the number is this pass acting, or a pass that integrated five hundred
            // artifacts backs off; nested where it repeats a measurement or counts a standing fact.
            report.integrated = p.integrated.integrated;
            report.probes = p.integrated.probes;
            report.retired = p.replayed.retired;
            report.moved = p.replayed.moved;
            report.condensed = p.condensed;
            report.standing.rehearsed = p.replayed.rehearsed;
            report.standing.interference = p.interference;
        }
        Err(e) => {
            tracing::warn!(error = %e, "the idle pass failed; the live generation is unchanged");
            failure.get_or_insert(e);
        }
    }

    if core.feedback.retain_days > 0 {
        match core
            .store
            .expire_rehearsal_results(core.feedback.retain_days)
            .await
        {
            Ok(n) if n > 0 => tracing::info!(dropped = n, "expired rehearsal results"),
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, "could not expire rehearsal results");
                failure.get_or_insert(e);
            }
        }
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

    // Its own window, behind no key: shortening the query log is not asking to forget weekly
    // patterns. Hence `periodic_units` also arms this for `recommend.enabled`.
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

    // Interactions ride the situations' window: the context sweep reads them as a pair.
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

    // The sweep history is trimmed by `background::repair_once`: this unit may not exist.

    match failure {
        Some(e) => Err(e),
        None => Ok(report),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn seed_old_event(core: &Core) {
        let id = core
            .store
            .record_search(
                crate::store::feedback::NewEvent {
                    fold_onto: None,
                    query: "old".into(),
                    door: crate::store::feedback::Door::Ui,
                    scope: None,
                    filters: "{}".into(),
                    query_vec: vec![0.0],
                    embed_model: "fake".into(),
                    candidates: vec![],
                    answered: false,
                    context: None,
                },
                0,
            )
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE search_events SET created_at = ? WHERE id = ?",
            crate::store::now() - 40 * 86_400,
            &id
        )
        .execute(&core.store.pool)
        .await
        .unwrap();
    }

    async fn captured(core: &Core) -> i64 {
        sqlx::query_scalar!("SELECT count(*) FROM search_events")
            .fetch_one(&core.store.pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn retention_runs_with_consolidation_switched_off() {
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
        // `run_accounted` reads only the return value, so warnings alone would record the pass as
        // `ok`.
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        core.feedback.retain_days = 30;
        sqlx::query!("DROP TABLE search_events")
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
        // One gap is not a group.
        for q in ["mount an E01", "mounting E01 images"] {
            let id = core
                .store
                .record_ask(crate::store::asks::NewAsk {
                    question: q.into(),
                    filters: "{}".into(),
                    query_vec: vec![1.0; 4],
                    embed_model: core.embedder.model().to_string(),
                    answer: "Not in the knowledge base.".into(),
                    abstained: true,
                    ..Default::default()
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
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        core.feedback.retain_days = 30;
        seed_old_event(&core).await;

        run(&core).await.unwrap();

        assert_eq!(captured(&core).await, 0);
    }
}
