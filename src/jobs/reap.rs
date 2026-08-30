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
    let (cands, stamped) = nominees(core).await?;
    report.stamped = stamped;
    for c in &cands {
        report.judged += 1;
        match judge_one(core, c).await {
            Ok(crate::infer::prompt::Reap::Worthless { reason }) => {
                match reap_one(core, c, &reason).await {
                    Ok(()) => {
                        report.reaped += 1;
                        tracing::info!(artifact_id = %c.id, reason, "reaped a retired artifact");
                    }
                    Err(e) => {
                        tracing::warn!(artifact_id = %c.id, error = %e, "could not bury a worthless artifact")
                    }
                }
            }
            Ok(crate::infer::prompt::Reap::Valuable { reason }) => {
                tracing::info!(artifact_id = %c.id, reason, "a retired artifact still holds value");
            }
            // A reply that failed or cannot be read acts on nothing; the row
            // is simply a candidate again next interval. The sweep's cadence
            // is the retry — no bookkeeping.
            Err(e) => {
                tracing::warn!(artifact_id = %c.id, error = %e, "reap judgement failed; candidate waits")
            }
        }
    }
    Ok(report)
}

/// Act on `worthless`: copy to the graveyard and wipe the row, then delete
/// the vector point.
///
/// Row before point, marker before delete — the same protocol every lifecycle
/// write uses. A delete that fails leaves a marked row against a standing
/// point, which is exactly the drift the repair pass reads
/// `lifecycle_dirty` to find; nothing else in the system would notice. Under
/// `lifecycle_lock` so no restore lands between the copy and the wipe.
async fn reap_one(core: &Core, c: &crate::store::artifacts::Chunk, reason: &str) -> Result<()> {
    let meta = serde_json::json!({
        "reason": reason,
        "status": c.status.as_str(),
        "superseded_by": c.superseded_by,
        "provenance": c.provenance.as_str(),
        "tags": c.tags,
        "corpus_id": c.corpus_id,
        "corpus_span": c.corpus_span,
        "created_at": c.created_at,
        "retired_at": c.retired_at,
    })
    .to_string();
    let _guard = core.lifecycle_lock.lock().await;
    core.store.bury(&c.id, &meta).await?;
    core.store.mark_lifecycle_dirty(&c.id).await?;
    core.vectors
        .delete_artifacts(std::slice::from_ref(&c.id))
        .await?;
    core.store
        .clear_lifecycle_dirty(std::slice::from_ref(&c.id))
        .await?;
    Ok(())
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

/// One nominee, one call. The judge is `core.judge` — the same cheap
/// completer dedupe uses — and it compares the candidate against what a
/// searcher can actually reach: the successor row when one was named, and the
/// nearest *live* neighbours by the candidate's stored vector. A candidate
/// with no point simply gets no neighbours; the judge then sees only the
/// successor, or nothing live at all, and the prompt's own bias ("when
/// unsure, valuable") is what keeps that from destroying anything.
async fn judge_one(
    core: &Core,
    c: &crate::store::artifacts::Chunk,
) -> Result<crate::infer::prompt::Reap> {
    let judge = core
        .judge
        .as_ref()
        .ok_or_else(|| crate::error::Error::Validation("no judge model configured".into()))?;
    let successor = match &c.superseded_by {
        Some(id) => core.store.get_artifact(id).await.ok(),
        None => None,
    };
    let mut neighbours: Vec<(String, String)> = Vec::new();
    if let Ok(hits) = core.vectors.neighbours(&c.id, 6).await {
        for h in hits {
            if h.payload.artifact_id == c.id {
                continue;
            }
            // Live means what `in_results` means; the payload's own status
            // can lag the row, so the row answers.
            if let Ok(Some(true)) = core.store.artifact_in_results(&h.payload.artifact_id).await {
                neighbours.push((
                    h.payload.title.clone().unwrap_or_else(|| "untitled".into()),
                    h.payload.text.clone(),
                ));
            }
            if neighbours.len() >= 5 {
                break;
            }
        }
    }
    let case = crate::infer::prompt::ReapCase {
        title: c.title.as_deref().unwrap_or("untitled"),
        text: &c.text,
        successor: successor
            .as_ref()
            .map(|s| (s.title.as_deref().unwrap_or("untitled"), s.text.as_str())),
        neighbours: neighbours
            .iter()
            .map(|(t, x)| (t.as_str(), x.as_str()))
            .collect(),
    };
    let user = crate::infer::prompt::reap_prompt(&case);
    let permit = core.gate.background().await;
    let reply = judge.complete(crate::infer::prompt::REAP_SYSTEM, &user).await;
    permit.finished();
    crate::infer::prompt::parse_reap(&reply?)
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
    async fn the_judge_sees_the_successor_and_answers_from_the_script() {
        let mut core = test_core().await;
        let scripted = std::sync::Arc::new(crate::infer::fake::ScriptedCompleter::new(vec![
            r#"{"verdict":"worthless","reason":"covered"}"#.into(),
        ]));
        core.judge = Some(scripted.clone());
        let ids = seed(&core, &["the old wording", "the new wording"]).await;
        core.store
            .set_superseded_by(&ids[0], Some(&ids[1]))
            .await
            .unwrap();
        let c = core.store.get_artifact(&ids[0]).await.unwrap();
        let verdict = judge_one(&core, &c).await.unwrap();
        assert!(matches!(
            verdict,
            crate::infer::prompt::Reap::Worthless { .. }
        ));
        assert_eq!(scripted.calls(), 1);
        let prompts = scripted.prompts();
        assert!(
            prompts[0].contains("the old wording") && prompts[0].contains("the new wording"),
            "the judge must see both sides: {}",
            prompts[0]
        );
    }

    #[tokio::test]
    async fn a_worthless_verdict_reaches_the_graveyard_and_the_point_dies() {
        let mut core = test_core().await;
        core.judge = Some(std::sync::Arc::new(
            crate::infer::fake::ScriptedCompleter::new(vec![
                r#"{"verdict":"worthless","reason":"covered"}"#.into(),
            ]),
        ));
        let ids = seed(&core, &["stale duplicate fact"]).await;
        crate::jobs::embed::run(&core, &ids[0]).await.unwrap();
        deprecate_long_ago(&core, &ids[0]).await;

        let report = run(&core).await.unwrap();
        assert_eq!((report.judged, report.reaped, report.rescued), (1, 1, 0));

        let row = core.store.get_artifact(&ids[0]).await.unwrap();
        assert_eq!(row.text, "");
        assert!(row.reaped_at.is_some());
        let (text, meta, _) = core
            .store
            .graveyard_row(&ids[0])
            .await
            .unwrap()
            .expect("a grave");
        assert!(text.contains("stale duplicate fact"));
        assert!(meta.contains("covered"));
        assert!(
            core.vectors
                .payloads_of(std::slice::from_ref(&ids[0]))
                .await
                .unwrap()
                .is_empty(),
            "the point must be gone"
        );
        assert!(
            core.store
                .dirty_lifecycle_artifacts(10)
                .await
                .unwrap()
                .is_empty(),
            "the marker must be cleared once the delete is acknowledged"
        );
    }

    #[tokio::test]
    async fn a_failed_reply_leaves_the_candidate_untouched() {
        let mut core = test_core().await;
        // An empty script: the call itself errors, which must act on nothing.
        core.judge = Some(std::sync::Arc::new(
            crate::infer::fake::ScriptedCompleter::new(vec![]),
        ));
        let ids = seed(&core, &["still here"]).await;
        deprecate_long_ago(&core, &ids[0]).await;

        let report = run(&core).await.unwrap();
        assert_eq!((report.judged, report.reaped), (1, 0));
        assert_eq!(
            core.store.get_artifact(&ids[0]).await.unwrap().text,
            "still here"
        );
        assert!(core.store.graveyard_row(&ids[0]).await.unwrap().is_none());
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
