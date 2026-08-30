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
                // Bounded per run so one bad judging batch cannot flood the
                // live base with model-written text. What is over the cap is
                // logged and stays a candidate.
                if report.rescued >= core.reap.max_rescues_per_run {
                    tracing::info!(artifact_id = %c.id, reason, "valuable, but over this run's rescue cap; it waits");
                    continue;
                }
                match rescue_one(core, c, &reason).await {
                    Ok(new_id) => {
                        report.rescued += 1;
                        tracing::info!(artifact_id = %c.id, new_id, reason, "rescued a retired artifact into a rewrite");
                    }
                    Err(e) => {
                        tracing::warn!(artifact_id = %c.id, error = %e, "could not rescue a valuable artifact; it waits")
                    }
                }
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

/// The nearest artifacts a searcher can actually reach, `(title, text)`. Live
/// means what `in_results` means; the payload's own status can lag the row,
/// so the row answers. A candidate with no point gets none, which only ever
/// defends it.
async fn live_neighbours(core: &Core, id: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    if let Ok(hits) = core.vectors.neighbours(id, 6).await {
        for h in hits {
            if h.payload.artifact_id == id {
                continue;
            }
            if let Ok(Some(true)) = core.store.artifact_in_results(&h.payload.artifact_id).await {
                out.push((
                    h.payload.title.clone().unwrap_or_else(|| "untitled".into()),
                    h.payload.text.clone(),
                ));
            }
            if out.len() >= 5 {
                break;
            }
        }
    }
    out
}

/// Act on `valuable`: rewrite what remains into a live synthesized artifact
/// and point the candidate at it.
///
/// What the rewrite is written *from* is the one rule that matters here. A
/// source-text candidate (captured, passage, note) is its own material. A
/// model-written one (merged, synthesized) is a paraphrase already, and a
/// rewrite of it would be a paraphrase of a paraphrase posing as an original —
/// so its material is the text of its roots through `artifact_sources`, and a
/// candidate whose roots have lost their text (reaped themselves, or gone) is
/// skipped rather than rewritten from the copy.
///
/// `insert_synthesized_artifact` is handed the candidate as the source list
/// and resolves roots itself, so the lineage the new artifact records is
/// root-true at any depth regardless of which branch fed the prompt.
async fn rescue_one(
    core: &Core,
    c: &crate::store::artifacts::Chunk,
    reason: &str,
) -> Result<String> {
    let judge = core
        .judge
        .as_ref()
        .ok_or_else(|| crate::error::Error::Validation("no judge model configured".into()))?;
    let mut excerpts: Vec<(String, String)> = Vec::new();
    if c.provenance.is_source_text() {
        excerpts.push((
            c.title.clone().unwrap_or_else(|| "untitled".into()),
            c.text.clone(),
        ));
    } else {
        let roots = core
            .store
            .roots_of(std::slice::from_ref(&c.id))
            .await?
            .remove(&c.id)
            .unwrap_or_default();
        for root in &roots {
            if let Ok(r) = core.store.get_artifact(root).await
                && !r.text.trim().is_empty()
            {
                excerpts.push((
                    r.title.clone().unwrap_or_else(|| "untitled".into()),
                    r.text.clone(),
                ));
            }
        }
        if excerpts.is_empty() {
            return Err(crate::error::Error::Validation(
                "a model-written candidate with no surviving source text cannot be rescued".into(),
            ));
        }
    }
    let mut user = String::new();
    user.push_str(&format!("What the live base lacks: {reason}\n\n"));
    user.push_str("----- SOURCE EXCERPTS -----\n");
    for (title, text) in &excerpts {
        user.push_str(&format!("Title: {title}\n\n{text}\n\n"));
    }
    let neighbours = live_neighbours(core, &c.id).await;
    if !neighbours.is_empty() {
        user.push_str("----- CLOSEST LIVE ARTIFACTS -----\n");
        for (title, text) in &neighbours {
            user.push_str(&format!("Title: {title}\n\n{text}\n\n"));
        }
    }
    let permit = core.gate.background().await;
    let reply = judge
        .complete(crate::infer::prompt::RESCUE_SYSTEM, &user)
        .await;
    permit.finished();
    let g = crate::infer::prompt::parse_generation(&reply?)?;
    let made = core
        .store
        .insert_synthesized_artifact(
            &crate::store::artifacts::NewSynthesized {
                text: g.text,
                title: Some(g.title),
                category: g.category,
                tags: g.tags,
                caveats: g.caveats,
                cues: vec![format!("reap rescue: {reason}")],
            },
            std::slice::from_ref(&c.id),
        )
        .await?;
    core.store
        .enqueue(crate::store::jobs::Stage::Embed, "artifact", &made.id)
        .await?;
    // A deprecated candidate now has the winner a supersession names, so it
    // gets one — through the store directly, not `core.supersede()`, whose
    // both-active guard rightly refuses a retired loser. An already-superseded
    // candidate keeps its pointer: its old winner retired it, and the rewrite
    // will simply outrank it as a live neighbour on the second visit.
    if c.superseded_by.is_none() {
        let _guard = core.lifecycle_lock.lock().await;
        core.store.set_superseded_by(&c.id, Some(&made.id)).await?;
        core.vectors
            .set_lifecycle(
                &c.id,
                crate::store::artifacts::ArtifactStatus::Superseded,
                Some(&made.id),
            )
            .await?;
        core.store
            .clear_lifecycle_dirty(std::slice::from_ref(&c.id))
            .await?;
    }
    Ok(made.id)
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
    let neighbours = live_neighbours(core, &c.id).await;
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

    const RESCUE_REPLY: &str =
        r#"{"artifact":{"title":"Kept fact","text":"the port is 8443","category":null,"tags":[],"caveats":[]}}"#;

    #[tokio::test]
    async fn a_valuable_deprecated_artifact_is_rewritten_and_superseded_by_the_rewrite() {
        let mut core = test_core().await;
        core.judge = Some(std::sync::Arc::new(
            crate::infer::fake::ScriptedCompleter::new(vec![
                r#"{"verdict":"valuable","reason":"names the port"}"#.into(),
                RESCUE_REPLY.into(),
            ]),
        ));
        let ids = seed(&core, &["the admin port is 8443"]).await;
        deprecate_long_ago(&core, &ids[0]).await;

        let report = run(&core).await.unwrap();
        assert_eq!((report.judged, report.reaped, report.rescued), (1, 0, 1));

        let old = core.store.get_artifact(&ids[0]).await.unwrap();
        let new_id = old.superseded_by.expect("superseded by the rewrite");
        let new = core.store.get_artifact(&new_id).await.unwrap();
        assert_eq!(new.provenance, crate::store::artifacts::Provenance::Synthesized);
        assert_eq!(new.text, "the port is 8443");
        assert!(new.in_results(), "the rewrite is live");
        assert!(!old.text.is_empty(), "a rescue wipes nothing");
    }

    #[tokio::test]
    async fn a_model_written_candidate_is_rewritten_from_its_roots() {
        let mut core = test_core().await;
        let scripted = std::sync::Arc::new(crate::infer::fake::ScriptedCompleter::new(vec![
            r#"{"verdict":"valuable","reason":"names the mount flags"}"#.into(),
            RESCUE_REPLY.into(),
        ]));
        core.judge = Some(scripted.clone());
        let ids = seed(&core, &["mount with ro,loop", "journal at /var/log"]).await;
        let merge = core
            .store
            .insert_merged_artifact(
                &NewMerged {
                    text: "a paraphrase of both".into(),
                    title: Some("M".into()),
                    category: None,
                    tags: vec![],
                    caveats: vec![],
                },
                &ids,
            )
            .await
            .unwrap();
        core.store
            .set_artifact_status(&merge.id, ArtifactStatus::Deprecated)
            .await
            .unwrap();
        backdate_retired_at(&core, &merge.id, 100 * 86_400).await;

        let report = run(&core).await.unwrap();
        assert_eq!(report.rescued, 1);
        let rescue_prompt = &scripted.prompts()[1];
        assert!(
            rescue_prompt.contains("mount with ro,loop")
                && rescue_prompt.contains("journal at /var/log"),
            "the rewrite must be fed source text: {rescue_prompt}"
        );
        assert!(
            !rescue_prompt.contains("a paraphrase of both"),
            "never the model-written candidate's own text"
        );
        let new_id = core
            .store
            .get_artifact(&merge.id)
            .await
            .unwrap()
            .superseded_by
            .unwrap();
        let mut roots = core
            .store
            .roots_of(std::slice::from_ref(&new_id))
            .await
            .unwrap()
            .remove(&new_id)
            .unwrap();
        roots.sort();
        let mut want = ids.clone();
        want.sort();
        assert_eq!(roots, want, "the rewrite's lineage names the captured roots");
    }

    #[tokio::test]
    async fn the_rescue_cap_holds() {
        let mut core = test_core().await;
        core.reap.max_rescues_per_run = 1;
        core.judge = Some(std::sync::Arc::new(
            crate::infer::fake::ScriptedCompleter::new(vec![
                r#"{"verdict":"valuable","reason":"one"}"#.into(),
                RESCUE_REPLY.into(),
                r#"{"verdict":"valuable","reason":"two"}"#.into(),
            ]),
        ));
        let ids = seed(&core, &["first fact", "second fact"]).await;
        for id in &ids {
            deprecate_long_ago(&core, id).await;
        }
        let report = run(&core).await.unwrap();
        assert_eq!((report.judged, report.rescued), (2, 1));
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
