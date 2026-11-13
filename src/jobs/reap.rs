//! Revisit the long-retired and act without asking: bury the worthless, rewrite the valuable live.
//! A wrong burial answers to its journal row, the Reaped listing's Restore, and `retract`'s
//! exhumation.

use crate::core::Core;
use crate::error::Result;

/// Flat numbers: `jobs::did_work` reads any non-zero flat count as work, and each count here is
/// this pass acting.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Report {
    pub judged: u64,
    pub reaped: u64,
    pub rescued: u64,
    /// Retired rows given a `retired_at` because they predate the column.
    pub stamped: u64,
}

pub async fn run(core: &Core) -> Result<Report> {
    // Re-checked here: `periodic_units` gates arming, and nothing disarms a row already pending.
    if !core.reap.enabled {
        return Ok(Report::default());
    }
    let mut report = Report::default();
    let (cands, stamped) = nominees(core).await?;
    report.stamped = stamped;
    for c in &cands {
        // Reap's own daily cap, before the judge call: burial is the action here that destroys
        // text.
        if !core.may_act(crate::store::actions::Job::Reap).await? {
            tracing::info!("budget spent; the rest of the nominees wait for the window to move");
            break;
        }
        // A burial taken back is not bought again; checked before the expensive judge call.
        if core
            .store
            .action_was_undone(&c.id, crate::store::actions::Kind::Reap)
            .await?
        {
            tracing::debug!(artifact_id = %c.id, "a burial taken back before; left alone");
            // Stood down, not skipped: `reap_candidates` orders by `retired_at ASC`, so an
            // unstamped row would hold a nominee slot forever.
            step_aside(core, &c.id).await;
            continue;
        }
        let verdict = judge_one(core, c).await;
        // Counted only when a judgement came back, or a sweep with a dead endpoint looks busy and
        // never backs off.
        if verdict.is_ok() {
            report.judged += 1;
        }
        match verdict {
            Ok(crate::infer::prompt::Reap::Worthless { reason }) => {
                match reap_one(core, c, &reason, min_age_secs(core)).await {
                    Ok(()) => {
                        report.reaped += 1;
                        tracing::info!(artifact_id = %c.id, reason, "reaped a retired artifact");
                    }
                    Err(e) => {
                        tracing::warn!(artifact_id = %c.id, error = %e, "could not bury a worthless artifact");
                        step_aside(core, &c.id).await;
                    }
                }
            }
            Ok(crate::infer::prompt::Reap::Valuable { reason }) => {
                // Bounded per run so one bad judging batch cannot flood the live base with
                // model-written text.
                if report.rescued >= core.reap.max_rescues_per_run {
                    tracing::info!(artifact_id = %c.id, reason, "valuable, but over this run's rescue cap; it waits");
                    continue;
                }
                // Checked before the row is touched: a missing generator is a wait, not this
                // candidate's failure.
                if core.generator.is_none() {
                    tracing::info!(artifact_id = %c.id, reason, "valuable, but no generator model to rewrite it with; it waits");
                    continue;
                }
                match rescue_one(core, c, &reason).await {
                    Ok(new_id) => {
                        report.rescued += 1;
                        tracing::info!(artifact_id = %c.id, new_id, reason, "rescued a retired artifact into a rewrite");
                    }
                    Err(e) => {
                        tracing::warn!(artifact_id = %c.id, error = %e, "could not rescue a valuable artifact; it waits");
                        // Restamped only for a failure about this row; an unanswered or busy
                        // endpoint says nothing about it, and the sweep's cadence is the retry.
                        if !matches!(
                            e,
                            crate::error::Error::Inference { .. }
                                | crate::error::Error::InferenceBusy { .. }
                        ) {
                            step_aside(core, &c.id).await;
                        }
                    }
                }
            }
            // A failed or unreadable reply acts on nothing. A rejection (`InferenceRejected`) is
            // about this row and stands it down; "no reap model" is a wait and must not.
            Err(e @ crate::error::Error::InferenceRejected { .. }) => {
                tracing::warn!(artifact_id = %c.id, error = %e, "the judge will not rule on this candidate; standing it down");
                step_aside(core, &c.id).await;
            }
            Err(e) => {
                tracing::warn!(artifact_id = %c.id, error = %e, "reap judgement failed; candidate waits")
            }
        }
    }
    Ok(report)
}

/// Moves a candidate the sweep could not act on to the back of the queue with a fresh `retired_at`,
/// so one that fails the same way every time cannot starve the rest.
async fn step_aside(core: &Core, id: &str) {
    if let Err(e) = core.store.restamp_retired(id).await {
        tracing::warn!(artifact_id = %id, error = %e, "could not restamp a candidate the sweep could not act on");
    }
}

/// The nearest live artifacts, `(title, text)`. Errors propagate: a failed search read as "nothing
/// near" would let the judge bury text with no evidence in front of it.
async fn live_neighbours(core: &Core, id: &str) -> Result<Vec<(String, String)>> {
    let mut out: Vec<(String, String)> = Vec::new();
    for h in core.vectors.neighbours(id, 6).await? {
        if h.payload.artifact_id == id {
            continue;
        }
        if core
            .store
            .artifact_in_results(&h.payload.artifact_id)
            .await?
            == Some(true)
        {
            out.push((
                h.payload.title.clone().unwrap_or_else(|| "untitled".into()),
                h.payload.text.clone(),
            ));
        }
        if out.len() >= 5 {
            break;
        }
    }
    Ok(out)
}

/// Source text rewrites from itself; a model-written candidate rewrites from its roots' text, never
/// from a paraphrase, and is skipped if they are gone.
async fn rescue_one(
    core: &Core,
    c: &crate::store::artifacts::Chunk,
    reason: &str,
) -> Result<String> {
    // `core.generator`, not `core.judge`: `RESCUE_SYSTEM` needs `for_generating`'s response format.
    let writer = core
        .generator
        .as_ref()
        .ok_or_else(|| crate::error::Error::Validation("no generator model configured".into()))?;
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
    let neighbours = live_neighbours(core, &c.id).await?;
    if !neighbours.is_empty() {
        user.push_str("----- CLOSEST LIVE ARTIFACTS -----\n");
        for (title, text) in &neighbours {
            user.push_str(&format!("Title: {title}\n\n{text}\n\n"));
        }
    }
    let permit = core.gate.background().await;
    let reply = writer
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
    // Through the store, not `core.supersede()`, whose both-active guard refuses a retired loser.
    // Re-checked like `bury`: the row may have been reactivated during the model calls.
    if c.superseded_by.is_none() {
        let _guard = core.lifecycle_lock.lock().await;
        match core
            .store
            .supersede_if_reapable(&c.id, &made.id, min_age_secs(core))
            .await
        {
            Ok(()) => {
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
            Err(crate::error::Error::NotFound) => {
                tracing::info!(
                    artifact_id = %c.id,
                    new_id = %made.id,
                    "the candidate stopped being one while it was being rewritten; \
                     the rewrite stands and the row is left alone"
                );
                return Ok(made.id);
            }
            Err(e) => return Err(e),
        }
    }
    // Restamped on both branches, so the second visit is an age later, against the rewrite.
    core.store.restamp_retired(&c.id).await?;
    Ok(made.id)
}

/// Row before point, marker before delete: a failed delete leaves `lifecycle_dirty` for the repair
/// pass. Under `lifecycle_lock` so no restore lands between copy and wipe.
async fn reap_one(
    core: &Core,
    c: &crate::store::artifacts::Chunk,
    reason: &str,
    min_age_secs: i64,
) -> Result<()> {
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
    // Read before the point is deleted. A store that cannot answer buries without it, and says so.
    let dense = match core.vectors.dense_of(&c.id).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(artifact_id = %c.id, error = %e, "buried without its vector");
            None
        }
    };
    let journal = crate::store::actions::NewAction {
        job: crate::store::actions::Job::Reap,
        kind: crate::store::actions::Kind::Reap,
        subject_id: c.id.clone(),
        survivor_id: None,
        detail: Some(reason.to_string()),
        evidence: serde_json::json!({
            "status": c.status.as_str(),
            "retired_at": c.retired_at,
            "created_at": c.created_at,
        }),
        pair_score: None,
    };
    let _guard = core.lifecycle_lock.lock().await;
    // `bury` sets `lifecycle_dirty` and writes the journal row in its own transaction.
    core.store
        .bury(
            &c.id,
            &meta,
            min_age_secs,
            dense.as_deref(),
            c.embed_model.as_deref(),
            &journal,
        )
        .await?;
    core.vectors
        .delete_artifacts(std::slice::from_ref(&c.id))
        .await?;
    core.store
        .clear_lifecycle_dirty(std::slice::from_ref(&c.id))
        .await?;
    Ok(())
}

/// Rows read per row to judge: the seen-since-retired filter runs on vector payloads after the page
/// is fetched.
const OVERFETCH: i64 = 4;

/// How long out of `active` before the sweep may look; `nominees` and `reap_one` must re-check
/// against the same number.
fn min_age_secs(core: &Core) -> i64 {
    (core.reap.min_age_days as i64).saturating_mul(86_400)
}

/// Who goes before the judge: `reap_candidates` holds every SQL rule; the seen-since-retired rule
/// reads the vector point.
async fn nominees(core: &Core) -> Result<(Vec<crate::store::artifacts::Chunk>, u64)> {
    let stamped = core.store.stamp_unaged_retired().await?;
    if stamped > 0 {
        tracing::info!(stamped, "gave pre-column retirements a clock");
    }
    let min_age_secs = min_age_secs(core);
    let want = core.reap.max_judged_per_run as i64;
    let mut cands = core
        .store
        .reap_candidates(min_age_secs, want.saturating_mul(OVERFETCH))
        .await?;
    if cands.is_empty() {
        return Ok((cands, stamped));
    }
    let ids: Vec<String> = cands.iter().map(|c| c.id.clone()).collect();
    let payloads = core.vectors.payloads_of(&ids).await?;
    cands.retain(|c| {
        goes_before_the_judge(
            c.retired_at,
            payloads.get(&c.id).and_then(|p| p.last_seen_at),
        )
    });
    // The cap is the sweep's: what is over it comes back oldest-first on the next run.
    cands.truncate(want.max(0) as usize);
    Ok((cands, stamped))
}

/// Only if nothing has reached it since retirement. A missing `retired_at` is treated as the
/// beginning of time, so any `last_seen_at` holds the row back.
fn goes_before_the_judge(retired_at: Option<i64>, last_seen_at: Option<i64>) -> bool {
    last_seen_at.is_none_or(|t| t < retired_at.unwrap_or(0))
}

/// `core.reaper` compares the candidate with its successor and its nearest live neighbours; the
/// prompt's "when unsure, valuable" protects a candidate with none.
async fn judge_one(
    core: &Core,
    c: &crate::store::artifacts::Chunk,
) -> Result<crate::infer::prompt::Reap> {
    let judge = core
        .reaper
        .as_ref()
        .ok_or_else(|| crate::error::Error::Validation("no reap model configured".into()))?;
    let successor = match &c.superseded_by {
        Some(id) => core.store.get_artifact(id).await.ok(),
        None => None,
    };
    let neighbours = live_neighbours(core, &c.id).await?;
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
    let reply = judge
        .complete(crate::infer::prompt::REAP_SYSTEM, &user)
        .await;
    permit.finished();
    crate::infer::prompt::parse_reap(&reply?)
}

#[cfg(test)]
pub(crate) mod test_support {
    pub(crate) fn row(id: &str) -> crate::store::actions::NewAction {
        crate::store::actions::NewAction {
            job: crate::store::actions::Job::Reap,
            kind: crate::store::actions::Kind::Reap,
            subject_id: id.to_string(),
            survivor_id: None,
            detail: None,
            evidence: serde_json::json!({}),
            pair_score: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_support::test_core;

    #[test]
    fn a_missing_retirement_stamp_defends_the_row() {
        assert!(goes_before_the_judge(Some(100), None));
        assert!(goes_before_the_judge(None, None));
        assert!(!goes_before_the_judge(Some(100), Some(100)));
        assert!(!goes_before_the_judge(Some(100), Some(200)));
        assert!(goes_before_the_judge(Some(100), Some(99)));
        assert!(!goes_before_the_judge(None, Some(1)));
        assert!(!goes_before_the_judge(None, Some(i64::MAX)));
    }

    use crate::store::artifacts::{ArtifactStatus, NewArtifact, NewMerged};

    async fn seed(core: &Core, texts: &[&str]) -> Vec<String> {
        let src = core
            .store
            .insert_corpus(&texts.join(" | "), "web", None)
            .await
            .unwrap();
        let rows: Vec<NewArtifact> = texts
            .iter()
            .enumerate()
            .map(|(i, t)| NewArtifact {
                ordinal: i as i64,
                text: (*t).into(),
                title: Some(format!("S{i}")),
                ..Default::default()
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
        sqlx::query!(
            "UPDATE artifacts SET retired_at = ? WHERE id = ?",
            crate::store::now() - secs_ago,
            id
        )
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
    async fn a_burial_taken_back_stands_down_instead_of_holding_its_slot() {
        use crate::store::actions::{Job, Kind, NewAction, UndoneBy};
        let mut core = test_core().await;
        core.reap.enabled = true;
        // No judge: every nominee fails at the call, below the branch under test.
        let ids = seed(&core, &["taken back before", "behind it"]).await;
        deprecate_long_ago(&core, &ids[0]).await;
        deprecate_long_ago(&core, &ids[1]).await;
        core.store
            .record_action(&NewAction {
                job: Job::Reap,
                kind: Kind::Reap,
                subject_id: ids[0].clone(),
                survivor_id: None,
                detail: None,
                evidence: serde_json::json!({}),
                pair_score: None,
            })
            .await
            .unwrap();
        core.store
            .undo_action_on(&ids[0], Kind::Reap, UndoneBy::Operator, "restored")
            .await
            .unwrap();
        let before = core.store.get_artifact(&ids[0]).await.unwrap().retired_at;

        run(&core).await.unwrap();

        let after = core.store.get_artifact(&ids[0]).await.unwrap().retired_at;
        assert!(
            after > before,
            "the row kept its place at the head of the queue: {before:?} -> {after:?}"
        );
        assert_eq!(
            core.store.get_artifact(&ids[1]).await.unwrap().retired_at,
            Some(crate::store::now() - 100 * 86_400),
            "a candidate the endpoint never answered for was stood down too"
        );
    }

    #[tokio::test]
    async fn an_active_young_seen_or_reminded_artifact_is_never_nominated() {
        let core = test_core().await;
        let ids = seed(
            &core,
            &["active", "young", "seen", "unreachable-reminder", "clean"],
        )
        .await;
        core.store
            .set_artifact_status(&ids[1], ArtifactStatus::Deprecated)
            .await
            .unwrap(); // fresh stamp — too young
        deprecate_long_ago(&core, &ids[2]).await;
        crate::jobs::embed::run(&core, &ids[2]).await.unwrap();
        core.vectors
            .touch(&[crate::vector::Touch::shown(&ids[2])], crate::store::now())
            .await
            .unwrap(); // seen after retirement
        // A retired artifact is on no band, so its own reminder holds nothing back.
        deprecate_long_ago(&core, &ids[3]).await;
        open_reminder(&core, "m1", &ids[3]).await;
        deprecate_long_ago(&core, &ids[4]).await;

        let pair = seed(&core, &["live", "retired-beside-a-live-reminder"]).await;
        open_reminder(&core, "m2", &pair[0]).await;
        deprecate_long_ago(&core, &pair[1]).await;

        let (cands, _) = nominees(&core).await.unwrap();
        let mut got = cands.iter().map(|c| c.id.as_str()).collect::<Vec<_>>();
        got.sort_unstable();
        let mut want = vec![ids[3].as_str(), ids[4].as_str()];
        want.sort_unstable();
        assert_eq!(
            got, want,
            "the old unseen retirements, the live reminder's note aside"
        );
    }

    async fn open_reminder(core: &Core, moment_id: &str, artifact_id: &str) {
        sqlx::query!(
            "INSERT INTO moments (id, artifact_id, kind, at, tz, source, created_at)
             VALUES (?, ?, 'due', ?, 'UTC', 'set', ?)",
            moment_id,
            artifact_id,
            crate::store::now() + 3_600,
            crate::store::now()
        )
        .execute(&core.store.pool)
        .await
        .unwrap();
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
                    ..Default::default()
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

        sqlx::query!(
            "UPDATE artifacts SET created_at = ? WHERE id = ?",
            crate::store::now() - 100 * 86_400,
            &merge.id
        )
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
        core.reaper = Some(scripted.clone());
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
    async fn a_disabled_sweep_does_not_run_a_unit_that_was_already_pending() {
        let mut core = test_core().await;
        core.reaper = Some(std::sync::Arc::new(
            crate::infer::fake::ScriptedCompleter::new(vec![
                r#"{"verdict":"worthless","reason":"covered"}"#.into(),
            ]),
        ));
        let ids = seed(&core, &["stale duplicate fact"]).await;
        crate::jobs::embed::run(&core, &ids[0]).await.unwrap();
        deprecate_long_ago(&core, &ids[0]).await;
        core.reap.enabled = false;

        let report = run(&core).await.unwrap();
        assert_eq!((report.judged, report.reaped, report.rescued), (0, 0, 0));
        assert_eq!(
            core.store.get_artifact(&ids[0]).await.unwrap().text,
            "stale duplicate fact",
            "the one stage that destroys text must re-check its own gate"
        );
        assert!(core.store.graveyard_row(&ids[0]).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_spent_budget_judges_nothing_and_buries_nothing() {
        let mut core = test_core().await;
        let scripted = std::sync::Arc::new(crate::infer::fake::ScriptedCompleter::new(vec![
            r#"{"verdict":"worthless","reason":"covered"}"#.into(),
        ]));
        core.reaper = Some(scripted.clone());
        core.evolve.autonomous = crate::config::Autonomy::Full;
        core.evolve.max_actions_per_day = 0;
        let ids = seed(&core, &["stale duplicate fact"]).await;
        crate::jobs::embed::run(&core, &ids[0]).await.unwrap();
        deprecate_long_ago(&core, &ids[0]).await;

        let report = run(&core).await.unwrap();
        assert_eq!((report.judged, report.reaped), (0, 0));
        assert_eq!(scripted.calls(), 0);
        assert!(core.store.graveyard_row(&ids[0]).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_burial_taken_back_before_is_not_bought_again() {
        use crate::store::actions::{Kind, UndoneBy};
        let mut core = test_core().await;
        let scripted = std::sync::Arc::new(crate::infer::fake::ScriptedCompleter::new(vec![
            r#"{"verdict":"worthless","reason":"covered"}"#.into(),
        ]));
        core.reaper = Some(scripted.clone());
        let ids = seed(&core, &["stale duplicate fact"]).await;
        crate::jobs::embed::run(&core, &ids[0]).await.unwrap();
        deprecate_long_ago(&core, &ids[0]).await;
        core.store
            .record_action(&test_support::row(&ids[0]))
            .await
            .unwrap();
        core.store
            .undo_action_on(&ids[0], Kind::Reap, UndoneBy::Operator, "restored")
            .await
            .unwrap();

        let report = run(&core).await.unwrap();
        assert_eq!((report.judged, report.reaped), (0, 0));
        assert_eq!(
            scripted.calls(),
            0,
            "the judge was asked about a burial taken back"
        );
        assert!(
            core.store
                .get_artifact(&ids[0])
                .await
                .unwrap()
                .reaped_at
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_worthless_verdict_reaches_the_graveyard_and_the_point_dies() {
        let mut core = test_core().await;
        core.reaper = Some(std::sync::Arc::new(
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
        let rows = core
            .store
            .open_actions(&[crate::store::actions::Kind::Reap], 10)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].subject_id, ids[0]);
        assert_eq!(rows[0].detail.as_deref(), Some("covered"));
        let kept: i64 = sqlx::query_scalar!(
            "SELECT vec IS NOT NULL FROM graveyard WHERE id = ?",
            &ids[0]
        )
        .fetch_one(&core.store.pool)
        .await
        .unwrap();
        assert_eq!(kept, 1, "the vector was not kept in the grave");
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
    async fn restoring_a_buried_superseded_artifact_brings_its_text_back() {
        let mut core = test_core().await;
        core.reaper = Some(std::sync::Arc::new(
            crate::infer::fake::ScriptedCompleter::new(vec![
                r#"{"verdict":"worthless","reason":"covered"}"#.into(),
            ]),
        ));
        let ids = seed(&core, &["the loser's own words", "the keeper"]).await;
        crate::jobs::embed::run(&core, &ids[0]).await.unwrap();
        core.supersede(&ids[0], &ids[1]).await.unwrap();
        backdate_retired_at(&core, &ids[0], 400 * 86_400).await;
        let report = run(&core).await.unwrap();
        assert_eq!(report.reaped, 1);
        assert!(
            core.store
                .get_artifact(&ids[0])
                .await
                .unwrap()
                .text
                .is_empty()
        );

        core.unsupersede(&ids[0]).await.unwrap();

        let row = core.store.get_artifact(&ids[0]).await.unwrap();
        assert_eq!(row.text, "the loser's own words", "not an empty row");
        assert!(row.reaped_at.is_none());
        assert!(row.superseded_by.is_none());
        assert!(core.store.graveyard_row(&ids[0]).await.unwrap().is_none());
        assert_eq!(
            row.embed_state,
            crate::store::artifacts::EmbedState::Pending,
            "and a fresh point is queued for it"
        );
    }

    #[tokio::test]
    async fn a_failed_reply_leaves_the_candidate_untouched() {
        let mut core = test_core().await;
        core.reaper = Some(std::sync::Arc::new(
            crate::infer::fake::ScriptedCompleter::new(vec![]),
        ));
        let ids = seed(&core, &["still here"]).await;
        deprecate_long_ago(&core, &ids[0]).await;

        let report = run(&core).await.unwrap();
        // `did_work` reads any non-zero count as work; failed calls must leave the backoff free to
        // engage.
        assert_eq!((report.judged, report.reaped), (0, 0));
        assert_eq!(
            core.store.get_artifact(&ids[0]).await.unwrap().text,
            "still here"
        );
        assert!(core.store.graveyard_row(&ids[0]).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_candidate_the_judge_refuses_stands_down_and_lets_the_queue_move() {
        let mut core = test_core().await;
        core.reaper = Some(std::sync::Arc::new(
            crate::infer::fake::ScriptedCompleter::rejecting("HTTP 400: input too long"),
        ));
        let ids = seed(&core, &["the one it will not read"]).await;
        deprecate_long_ago(&core, &ids[0]).await;
        let before = core
            .store
            .get_artifact(&ids[0])
            .await
            .unwrap()
            .retired_at
            .unwrap();

        let report = run(&core).await.unwrap();
        assert_eq!((report.judged, report.reaped), (0, 0));
        let after = core
            .store
            .get_artifact(&ids[0])
            .await
            .unwrap()
            .retired_at
            .unwrap();
        assert!(
            after > before,
            "a refusal is a verdict about this row: it goes to the back, {before} → {after}"
        );
    }

    #[tokio::test]
    async fn a_sweep_with_no_judge_leaves_every_candidate_where_it_was() {
        let mut core = test_core().await;
        core.reaper = None;
        let ids = seed(&core, &["waiting for a judge"]).await;
        deprecate_long_ago(&core, &ids[0]).await;
        let before = core
            .store
            .get_artifact(&ids[0])
            .await
            .unwrap()
            .retired_at
            .unwrap();

        run(&core).await.unwrap();
        assert_eq!(
            core.store
                .get_artifact(&ids[0])
                .await
                .unwrap()
                .retired_at
                .unwrap(),
            before
        );
    }

    #[tokio::test]
    async fn a_buried_stub_is_out_of_reach_by_consequence_not_by_filter() {
        let mut core = test_core().await;
        core.reaper = Some(std::sync::Arc::new(
            crate::infer::fake::ScriptedCompleter::new(vec![
                r#"{"verdict":"worthless","reason":"covered"}"#.into(),
            ]),
        ));
        let ids = seed(&core, &["a fact about zeppelins", "its living successor"]).await;
        crate::jobs::embed::run(&core, &ids[0]).await.unwrap();
        core.store
            .set_superseded_by(&ids[0], Some(&ids[1]))
            .await
            .unwrap();
        backdate_retired_at(&core, &ids[0], 100 * 86_400).await;
        run(&core).await.unwrap();

        let q = crate::core::search::SearchQuery {
            q: "zeppelins".into(),
            limit: 10,
            tags: vec![],
            category: None,
            mark: false,
            rerank: false,
            explain: false,
            include_deprecated: true,
            include_superseded: true,
        };
        let hits = core
            .search(&q, crate::store::feedback::Door::Ui)
            .await
            .unwrap();
        assert!(
            hits.iter().all(|h| h.artifact_id != ids[0]),
            "a reaped stub surfaced in search"
        );
        let stub = core.store.get_artifact(&ids[0]).await.unwrap();
        assert_eq!(stub.superseded_by.as_deref(), Some(ids[1].as_str()));
    }

    const RESCUE_REPLY: &str = r#"{"artifact":{"title":"Kept fact","text":"the port is 8443","category":null,"tags":[],"caveats":[]}}"#;

    #[tokio::test]
    async fn a_valuable_deprecated_artifact_is_rewritten_and_superseded_by_the_rewrite() {
        let mut core = test_core().await;
        core.reaper = Some(std::sync::Arc::new(
            crate::infer::fake::ScriptedCompleter::new(vec![
                r#"{"verdict":"valuable","reason":"names the port"}"#.into(),
            ]),
        ));
        core.generator = Some(std::sync::Arc::new(
            crate::infer::fake::ScriptedCompleter::new(vec![RESCUE_REPLY.into()]),
        ));
        let ids = seed(&core, &["the admin port is 8443"]).await;
        deprecate_long_ago(&core, &ids[0]).await;

        let report = run(&core).await.unwrap();
        assert_eq!((report.judged, report.reaped, report.rescued), (1, 0, 1));

        let old = core.store.get_artifact(&ids[0]).await.unwrap();
        let new_id = old.superseded_by.expect("superseded by the rewrite");
        let new = core.store.get_artifact(&new_id).await.unwrap();
        assert_eq!(
            new.provenance,
            crate::store::artifacts::Provenance::Synthesized
        );
        assert_eq!(new.text, "the port is 8443");
        assert!(new.in_results(), "the rewrite is live");
        assert!(!old.text.is_empty(), "a rescue wipes nothing");
    }

    #[tokio::test]
    async fn a_model_written_candidate_is_rewritten_from_its_roots() {
        let mut core = test_core().await;
        core.reaper = Some(std::sync::Arc::new(
            crate::infer::fake::ScriptedCompleter::new(vec![
                r#"{"verdict":"valuable","reason":"names the mount flags"}"#.into(),
            ]),
        ));
        let scripted = std::sync::Arc::new(crate::infer::fake::ScriptedCompleter::new(vec![
            RESCUE_REPLY.into(),
        ]));
        core.generator = Some(scripted.clone());
        let ids = seed(&core, &["mount with ro,loop", "journal at /var/log"]).await;
        let merge = core
            .store
            .insert_merged_artifact(
                &NewMerged {
                    text: "a paraphrase of both".into(),
                    title: Some("M".into()),
                    ..Default::default()
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
        let rescue_prompt = &scripted.prompts()[0];
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
        assert_eq!(
            roots, want,
            "the rewrite's lineage names the captured roots"
        );
    }

    #[tokio::test]
    async fn the_rescue_cap_holds() {
        let mut core = test_core().await;
        core.reap.max_rescues_per_run = 1;
        core.reaper = Some(std::sync::Arc::new(
            crate::infer::fake::ScriptedCompleter::new(vec![
                r#"{"verdict":"valuable","reason":"one"}"#.into(),
                r#"{"verdict":"valuable","reason":"two"}"#.into(),
            ]),
        ));
        core.generator = Some(std::sync::Arc::new(
            crate::infer::fake::ScriptedCompleter::new(vec![RESCUE_REPLY.into()]),
        ));
        let ids = seed(&core, &["first fact", "second fact"]).await;
        for id in &ids {
            deprecate_long_ago(&core, id).await;
        }
        let report = run(&core).await.unwrap();
        assert_eq!((report.judged, report.rescued), (2, 1));
    }

    #[tokio::test]
    async fn a_rescue_is_reaped_against_its_own_rewrite_on_the_second_pass() {
        // A rescue must restamp, or the candidate is back before the judge next interval, and must
        // wipe nothing.
        let mut core = test_core().await;
        core.reaper = Some(std::sync::Arc::new(
            crate::infer::fake::ScriptedCompleter::new(vec![
                r#"{"verdict":"valuable","reason":"names the port"}"#.into(),
                r#"{"verdict":"worthless","reason":"the rewrite carries it"}"#.into(),
            ]),
        ));
        core.generator = Some(std::sync::Arc::new(
            crate::infer::fake::ScriptedCompleter::new(vec![RESCUE_REPLY.into()]),
        ));
        let ids = seed(&core, &["the admin port is 8443"]).await;
        crate::jobs::embed::run(&core, &ids[0]).await.unwrap();
        deprecate_long_ago(&core, &ids[0]).await;

        let first = run(&core).await.unwrap();
        assert_eq!((first.reaped, first.rescued), (0, 1));
        let new_id = core
            .store
            .get_artifact(&ids[0])
            .await
            .unwrap()
            .superseded_by
            .expect("superseded by the rewrite");
        crate::jobs::embed::run(&core, &new_id).await.unwrap();

        let (none_yet, _) = nominees(&core).await.unwrap();
        assert!(
            none_yet.is_empty(),
            "a rescued candidate waits out a fresh age"
        );

        backdate_retired_at(&core, &ids[0], 100 * 86_400).await;

        let second = run(&core).await.unwrap();
        assert_eq!((second.judged, second.reaped, second.rescued), (1, 1, 0));
        assert!(core.store.graveyard_row(&ids[0]).await.unwrap().is_some());
        let rewrite = core.store.get_artifact(&new_id).await.unwrap();
        assert!(rewrite.in_results());
        assert_eq!(rewrite.text, "the port is 8443");
    }

    #[tokio::test]
    async fn the_first_pass_stamps_the_unaged_and_nominates_none_of_them() {
        let core = test_core().await;
        let ids = seed(&core, &["pre-column retirement"]).await;
        core.store
            .set_artifact_status(&ids[0], ArtifactStatus::Deprecated)
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE artifacts SET retired_at = NULL WHERE id = ?",
            &ids[0]
        )
        .execute(&core.store.pool)
        .await
        .unwrap();
        let (cands, stamped) = nominees(&core).await.unwrap();
        assert_eq!(stamped, 1);
        assert!(cands.is_empty(), "a fresh clock has not run");
    }
}
