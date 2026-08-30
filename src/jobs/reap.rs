//! Revisit the retired, and act without asking.
//!
//! Retirement hides an artifact and then keeps everything about it forever.
//! This sweep is the second look nobody was going to take by hand: free rules
//! nominate the long-retired, one model call per nominee asks whether it still
//! states anything the live base does not, and the verdict is acted on — the
//! worthless are buried (text into `graveyard`, point deleted, stub kept), the
//! valuable rewritten as live synthesized artifacts. No operator queue; the
//! graveyard is the insurance a wrong verdict answers to.

use crate::core::Core;
use crate::error::Result;

/// What one pass did. Flat numbers on purpose: `jobs::did_work` reads any
/// non-zero flat count as work, which is what drives the empty-run backoff,
/// and every count here really is this pass acting.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Report {
    /// Nominees put in front of the judge, verdicts and failures alike.
    pub judged: u64,
    /// Buried: text in the graveyard, vector point deleted, stub kept.
    pub reaped: u64,
    /// Rewritten as a live synthesized artifact.
    pub rescued: u64,
    /// Retired rows given a fresh `retired_at` because they predate the
    /// column — the migration-free backfill, counted as the work it is.
    pub stamped: u64,
}

pub async fn run(core: &Core) -> Result<Report> {
    let mut report = Report::default();
    let (_cands, stamped) = nominees(core).await?;
    report.stamped = stamped;
    Ok(report)
}

/// The free pass: who goes in front of the judge, and at what cost — three
/// SQL statements and one payload read, no model call.
///
/// The SQL half (age, status, no open reminder) lives in `reap_candidates`.
/// The two rules here need what SQLite does not hold: the usage stamps live
/// only on the vector point, and a candidate seen since it was retired is
/// still earning its keep; and a source of a *fresh* active merge keeps its
/// text because the merge's undo needs it — an old merge has forfeited that
/// window (nobody unmerges after `min_age_days`), which is the one price the
/// design accepts.
async fn nominees(core: &Core) -> Result<(Vec<crate::store::artifacts::Chunk>, u64)> {
    let stamped = core.store.stamp_unaged_retired().await?;
    if stamped > 0 {
        tracing::info!(stamped, "gave pre-column retirements a clock");
    }
    let min_age_secs = (core.reap.min_age_days as i64).saturating_mul(86_400);
    let mut cands = core
        .store
        .reap_candidates(min_age_secs, core.reap.max_judged_per_run as i64)
        .await?;
    if cands.is_empty() {
        return Ok((cands, stamped));
    }
    let ids: Vec<String> = cands.iter().map(|c| c.id.clone()).collect();
    let payloads = core.vectors.payloads_of(&ids).await?;
    let protected: std::collections::HashSet<String> = core
        .store
        .sources_of_fresh_merges(min_age_secs)
        .await?
        .into_iter()
        .collect();
    cands.retain(|c| {
        // A candidate always has a stamp — the SQL requires it — so a missing
        // one can only defend the row, never expose it.
        let retired_at = c.retired_at.unwrap_or(i64::MAX);
        let seen = payloads
            .get(&c.id)
            .and_then(|p| p.last_seen_at)
            .is_some_and(|t| t >= retired_at);
        !seen && !protected.contains(&c.id)
    });
    Ok((cands, stamped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_support::test_core;
    use crate::store::artifacts::{ArtifactStatus, NewArtifact, NewMerged};

    async fn seed(core: &Core, texts: &[&str]) -> Vec<String> {
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let rows: Vec<NewArtifact> = texts
            .iter()
            .enumerate()
            .map(|(i, t)| NewArtifact {
                ordinal: i as i64,
                text: (*t).into(),
                corpus_span: None,
                title: Some(format!("S{i}")),
                category: None,
                tags: vec![],
                segment_idx: None,
                caveats: vec![],
            })
            .collect();
        core.store
            .insert_artifacts(&src.id, &rows)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect()
    }

    async fn backdate_retired_at(core: &Core, id: &str, secs_ago: i64) {
        sqlx::query("UPDATE artifacts SET retired_at = ? WHERE id = ?")
            .bind(crate::store::now() - secs_ago)
            .bind(id)
            .execute(&core.store.pool)
            .await
            .unwrap();
    }

    async fn deprecate_long_ago(core: &Core, id: &str) {
        core.store
            .set_artifact_status(id, ArtifactStatus::Deprecated)
            .await
            .unwrap();
        backdate_retired_at(core, id, 100 * 86_400).await;
    }

    #[tokio::test]
    async fn an_active_young_seen_or_reminded_artifact_is_never_nominated() {
        let core = test_core().await;
        let ids = seed(&core, &["active", "young", "seen", "reminded", "clean"]).await;
        // ids[0] stays active.
        core.store
            .set_artifact_status(&ids[1], ArtifactStatus::Deprecated)
            .await
            .unwrap(); // fresh stamp — too young
        deprecate_long_ago(&core, &ids[2]).await;
        crate::jobs::embed::run(&core, &ids[2]).await.unwrap();
        core.vectors
            .touch(
                &[crate::vector::Touch::shown(&ids[2])],
                crate::store::now(),
            )
            .await
            .unwrap(); // seen after retirement
        deprecate_long_ago(&core, &ids[3]).await;
        sqlx::query(
            "INSERT INTO moments (id, artifact_id, kind, at, tz, source, created_at)
             VALUES ('m1', ?, 'due', ?, 'UTC', 'set', ?)",
        )
        .bind(&ids[3])
        .bind(crate::store::now() + 3_600)
        .bind(crate::store::now())
        .execute(&core.store.pool)
        .await
        .unwrap();
        deprecate_long_ago(&core, &ids[4]).await;

        let (cands, _) = nominees(&core).await.unwrap();
        assert_eq!(
            cands.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec![ids[4].as_str()],
            "only the old, unseen, unreminded retirement is nominated"
        );
    }

    #[tokio::test]
    async fn a_fresh_merge_keeps_its_sources_out_of_the_reap() {
        let core = test_core().await;
        let ids = seed(&core, &["side one of the fact", "side two of the fact"]).await;
        let merge = core
            .store
            .insert_merged_artifact(
                &NewMerged {
                    text: "the fact, once".into(),
                    title: Some("M".into()),
                    category: None,
                    tags: vec![],
                    caveats: vec![],
                },
                &ids,
            )
            .await
            .unwrap();
        for id in &ids {
            core.store
                .set_superseded_by(id, Some(&merge.id))
                .await
                .unwrap();
            backdate_retired_at(&core, id, 100 * 86_400).await;
        }
        let (cands, _) = nominees(&core).await.unwrap();
        assert!(cands.is_empty(), "a fresh merge's sources are protected");

        sqlx::query("UPDATE artifacts SET created_at = ? WHERE id = ?")
            .bind(crate::store::now() - 100 * 86_400)
            .bind(&merge.id)
            .execute(&core.store.pool)
            .await
            .unwrap();
        let (cands, _) = nominees(&core).await.unwrap();
        assert_eq!(cands.len(), 2, "an old merge has forfeited its undo window");
    }

    #[tokio::test]
    async fn the_first_pass_stamps_the_unaged_and_nominates_none_of_them() {
        let core = test_core().await;
        let ids = seed(&core, &["pre-column retirement"]).await;
        core.store
            .set_artifact_status(&ids[0], ArtifactStatus::Deprecated)
            .await
            .unwrap();
        sqlx::query("UPDATE artifacts SET retired_at = NULL WHERE id = ?")
            .bind(&ids[0])
            .execute(&core.store.pool)
            .await
            .unwrap();
        let (cands, stamped) = nominees(&core).await.unwrap();
        assert_eq!(stamped, 1);
        assert!(cands.is_empty(), "a fresh clock has not run");
    }
}
