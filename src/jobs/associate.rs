//! Learning what belongs together: a pure-SQLite sweep over the search log, and a queued model
//! judge per link.

use crate::core::Core;
use crate::error::Result;
use crate::store::links::normalize_query;

/// Last `search_events.created_at` folded into links.
pub const EVENTS_AFTER: &str = "associate.events_after";
/// Last `search_events.judged_at` folded into links.
pub const JUDGED_AFTER: &str = "associate.judged_after";
/// A ceiling, not a budget: a base offline for a month catches up over a few sweeps rather than
/// holding a worker for minutes.
const REPLAY_LIMIT: i64 = 2_000;

/// A bound on one tick's work; the next sweep prunes what the cap leaves.
const PRUNE_SCAN_LIMIT: i64 = 5_000;

/// Canonical, so the same pair never gets two units.
pub fn link_target(a: &str, b: &str) -> String {
    let (a, b) = crate::store::links::canonical(a, b);
    format!("{a}|{b}")
}

/// The exact complement of `record_search`'s fold predicate (`at - created <= coalesce_secs`), so
/// no event is both foldable and replayed.
fn settled_cutoff(at: i64, coalesce_secs: i64) -> i64 {
    at - coalesce_secs.max(0)
}

/// Watermarks are stamps, not rows, so a full batch leaves its last second for the next sweep
/// rather than strand rows sharing it. A full batch within one second is replayed whole and the
/// skip is logged at `warn`.
fn replayable(stamps: &[i64], limit: usize) -> usize {
    if stamps.len() < limit {
        return stamps.len();
    }
    let last = match stamps.last() {
        Some(s) => *s,
        None => return 0,
    };
    let end = stamps.partition_point(|s| *s < last);
    if end == 0 {
        tracing::warn!(
            stamp = last,
            read = stamps.len(),
            "more rows share one second than a single sweep reads; \
             the remainder of that second will not be replayed"
        );
        return stamps.len();
    }
    end
}

/// What one association sweep did, kept in the sweep account so a thinning sweep shows on Ops.
#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct Report {
    pub events: usize,
    pub verdicts: usize,
    pub forgotten: u64,
    /// Judged links reopened because one side was re-embedded.
    pub reopened: u64,
    /// Judge units armed; the calls happen later, one unit at a time.
    pub armed: i64,
}

pub async fn run(core: &Core) -> Result<Report> {
    if !core.associating() {
        return Ok(Report::default());
    }
    let at = crate::store::now();
    let bound = replay_events(core, at).await?;
    let confirmed = replay_verdicts(core, at).await?;

    let forgotten = core
        .store
        .prune_learning_links(
            core.associate.prune_below,
            core.associate.half_life_days,
            at,
            PRUNE_SCAN_LIMIT,
        )
        .await?;
    // Reopened before anything is armed, so a changed link is re-asked in this same sweep.
    let reopened = core
        .store
        .reopen_stale_judged_links(PRUNE_SCAN_LIMIT)
        .await?;

    // No judge, no units: nothing is queued that could never be claimed.
    let to_judge = if core.link_judge.is_some() {
        core.store
            .links_to_judge(
                core.associate.judge_min,
                core.associate.judge_min_queries,
                core.associate.half_life_days,
                at,
                core.associate.judge_per_sweep,
            )
            .await?
    } else {
        Vec::new()
    };
    let mut armed = 0;
    for l in to_judge {
        let target = link_target(&l.a_id, &l.b_id);
        // Re-arming a queued link is a no-op that costs another link its turn.
        if core
            .store
            .live_job(crate::store::jobs::Stage::LinkJudge, &target)
            .await?
        {
            continue;
        }
        core.store
            .rearm_idle_seq(crate::store::jobs::Stage::LinkJudge, "link", &target, armed)
            .await?;
        armed += 1;
    }

    tracing::info!(
        events = bound,
        verdicts = confirmed,
        forgotten,
        reopened,
        armed,
        "association sweep"
    );
    Ok(Report {
        events: bound,
        verdicts: confirmed,
        forgotten,
        reopened,
        armed,
    })
}

/// Only settled events: one inside `feedback.coalesce_secs` may still fold into a longer query.
async fn replay_events(core: &Core, at: i64) -> Result<usize> {
    let after: i64 = core
        .store
        .meta_get(EVENTS_AFTER)
        .await?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let settled = settled_cutoff(at, core.feedback.coalesce_secs);

    let events = sqlx::query!(
        r#"SELECT id AS "id!", query, created_at FROM search_events
          WHERE created_at > ? AND created_at < ?
          ORDER BY created_at ASC, id ASC LIMIT ?"#,
        after,
        settled,
        REPLAY_LIMIT
    )
    .fetch_all(&core.store.pool)
    .await?;

    let stamps: Vec<i64> = events.iter().map(|e| e.created_at).collect();
    let end = replayable(&stamps, REPLAY_LIMIT as usize);

    for (idx, e) in events[..end].iter().enumerate() {
        let id: String = e.id.clone();
        let cue = normalize_query(&e.query);
        let shown = shown_candidates(core, &id).await?;
        // One transaction per event: pairs are quadratic in what was shown, and each transaction is
        // exclusive.
        let pairs: Vec<(&str, &str)> = (0..shown.len())
            .flat_map(|i| ((i + 1)..shown.len()).map(move |j| (i, j)))
            .map(|(i, j)| (shown[i].as_str(), shown[j].as_str()))
            .collect();
        core.store
            .bump_links(&pairs, 1.0, Some(&cue), core.associate.half_life_days, at)
            .await?;
        // Committed per finished second: a failure propagates, and an unmoved watermark would count
        // the bumps twice.
        if last_of_its_second(&stamps, idx, end) {
            core.store
                .meta_set(EVENTS_AFTER, &stamps[idx].to_string())
                .await?;
        }
    }
    Ok(end)
}

/// The watermark may only advance to a second every row of which has been folded in.
fn last_of_its_second(stamps: &[i64], idx: usize, end: usize) -> bool {
    idx + 1 == end || stamps[idx + 1] > stamps[idx]
}

/// Its own cursor, because a verdict arrives days after its event. `gap` and `discard` say nothing
/// about a pair.
async fn replay_verdicts(core: &Core, at: i64) -> Result<usize> {
    let after: i64 = core
        .store
        .meta_get(JUDGED_AFTER)
        .await?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    // Bounded above at `at`: a verdict written later in the same second would otherwise fail `> at`
    // forever. Verdicts do not coalesce, so `settled_cutoff` is not needed.
    let events = sqlx::query!(
        r#"SELECT id AS "id!", expect_id AS "expect_id!", judged_at AS "judged_at!" FROM search_events
          WHERE judged_at > ? AND judged_at < ?
            AND verdict = 'hit' AND expect_id IS NOT NULL
          ORDER BY judged_at ASC, id ASC LIMIT ?"#,
        after,
        at,
        REPLAY_LIMIT
    )
    .fetch_all(&core.store.pool)
    .await?;

    let stamps: Vec<i64> = events.iter().map(|e| e.judged_at).collect();
    let end = replayable(&stamps, REPLAY_LIMIT as usize);

    for (idx, e) in events[..end].iter().enumerate() {
        let id: String = e.id.clone();
        let expect: String = e.expect_id.clone();
        let shown = shown_candidates(core, &id).await?;
        // Only a shown pair containing the answer binds harder: a find has no shown pair, and
        // binding it would invent a co-retrieval.
        if shown.contains(&expect) {
            // No cue: the event's words were already folded in when its co-appearance was replayed.
            let pairs: Vec<(&str, &str)> = shown
                .iter()
                .filter(|c| **c != expect)
                .map(|other| (expect.as_str(), other.as_str()))
                .collect();
            core.store
                .bump_links(&pairs, 2.0, None, core.associate.half_life_days, at)
                .await?;
        }
        // Raised even for a find: a confirmed artifact the ranking never returned is the most
        // valuable signal.
        core.store
            .bump_activation(
                std::slice::from_ref(&expect),
                core.activation.confirmed,
                core.activation.half_life_days,
                at,
            )
            .await?;
        crate::jobs::promote::maybe_promote(core, std::slice::from_ref(&expect), at).await?;
        // Same as `replay_events`: a verdict counted twice raises activation twice.
        if last_of_its_second(&stamps, idx, end) {
            core.store
                .meta_set(JUDGED_AFTER, &stamps[idx].to_string())
                .await?;
        }
    }
    Ok(end)
}

/// The ranked list only: reading the band's recalled artifacts would let a link strengthen itself
/// by having fired.
async fn shown_candidates(core: &Core, event_id: &str) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar!(
        "SELECT artifact_id FROM search_candidates
          WHERE event_id = ? AND shown = 1 AND band = 0 ORDER BY rank",
        event_id
    )
    .fetch_all(&core.store.pool)
    .await?)
}

use crate::error::Error;
use crate::store::links::LinkState;

/// Unreadable replies after which a link is shelved rather than asked forever.
pub const MAX_UNREADABLE_LINK_JUDGEMENTS: i64 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkVerdict {
    Related,
    Unrelated,
    Duplicate,
}

/// Unreadable is an error, not a verdict: defaulting either way misleads, and failing leaves the
/// link `learning` for the queue to retry.
pub fn parse_link(body: &str) -> Result<(LinkVerdict, String)> {
    #[derive(serde::Deserialize)]
    struct Raw {
        relation: String,
        #[serde(default)]
        reason: Option<String>,
    }
    let r: Raw = serde_json::from_value(crate::infer::prompt::unwrap_verdict(
        crate::infer::prompt::extract_json(body),
    )?)
    .map_err(|e| Error::MalformedLlmOutput(format!("link reply was not the expected JSON: {e}")))?;
    let verdict = match r.relation.as_str() {
        "related" => LinkVerdict::Related,
        "unrelated" => LinkVerdict::Unrelated,
        "duplicate" => LinkVerdict::Duplicate,
        other => {
            return Err(Error::MalformedLlmOutput(format!(
                "link reply named no relation this understands: {other}"
            )));
        }
    };
    Ok((verdict, r.reason.unwrap_or_default()))
}

/// `target` is `"<a_id>|<b_id>"`.
pub async fn judge(core: &Core, target: &str) -> Result<()> {
    // A unit queued before `[learn]` was switched off must not spend a model call after the
    // operator said stop.
    if !core.associating() {
        return Ok(());
    }
    let (a_id, b_id) = target.split_once('|').ok_or(Error::NotFound)?;
    let Some(link) = core.store.get_link(a_id, b_id).await? else {
        // Pruned, or one side deleted, while the unit waited.
        return Ok(());
    };
    if link.state != LinkState::Learning {
        // Answered by an operator's dismissal, or by a sibling unit.
        return Ok(());
    }
    let a = core.store.get_artifact(&link.a_id).await?;
    let b = core.store.get_artifact(&link.b_id).await?;
    // Re-checked here: a side can be superseded while this waits, and no one will be shown it.
    if !a.in_results() || !b.in_results() {
        return Ok(());
    }

    let Some(judge) = core.link_judge.clone() else {
        return Ok(());
    };
    let cues: Vec<String> = link.cues.iter().map(|c| c.q.clone()).collect();
    let permit = core.gate.background().await;
    let reply = judge
        .complete(
            crate::infer::prompt::LINK_SYSTEM,
            &crate::infer::prompt::link_prompt(
                (a.title.as_deref().unwrap_or("untitled"), &a.text),
                (b.title.as_deref().unwrap_or("untitled"), &b.text),
                &cues,
                link.judge_attempts,
            ),
        )
        .await;
    permit.finished();
    // An unanswered call says nothing about the link; the queue backs the unit off.
    let reply = reply?;

    let revs = Some((a.embed_rev, b.embed_rev));
    let (verdict, reason) = match parse_link(&reply) {
        Ok(v) => v,
        Err(e) => {
            // The only failure that says anything about the link itself.
            let attempts = core
                .store
                .record_link_judge_attempt(&link.a_id, &link.b_id)
                .await?;
            if attempts >= MAX_UNREADABLE_LINK_JUDGEMENTS {
                tracing::warn!(
                    target,
                    attempts,
                    "shelving a link the model will not answer for"
                );
                core.store
                    .set_link_state(
                        &link.a_id,
                        &link.b_id,
                        LinkState::Unrelated,
                        Some("unreadable"),
                        revs,
                    )
                    .await?;
                return Ok(());
            }
            tracing::warn!(
                target,
                attempts,
                reply_len = reply.len(),
                error = %e,
                "link reply unreadable"
            );
            return Err(e);
        }
    };

    // Never stored as `Some("")`: the pane falls back to the binding query only for a missing
    // reason.
    let reason = reason.trim();
    let reason = (!reason.is_empty()).then_some(reason);

    match verdict {
        LinkVerdict::Related => {
            core.store
                .set_link_state(&link.a_id, &link.b_id, LinkState::Related, reason, revs)
                .await?;
        }
        LinkVerdict::Unrelated => {
            core.store
                .set_link_state(&link.a_id, &link.b_id, LinkState::Unrelated, reason, revs)
                .await?;
        }
        LinkVerdict::Duplicate => {
            // Handed to consolidation, which owns every decision that hides an artifact; score zero
            // because no cosine was measured. `INSERT OR IGNORE` hands nothing over for a pair
            // already on file. Never over passages: they are not consolidation's material.
            let over_passages = a.provenance == crate::store::artifacts::Provenance::Passage
                || b.provenance == crate::store::artifacts::Provenance::Passage;
            // Rendered verbatim beside the link, so it must not claim a handover that did not
            // happen, nor call an artifact stored source text when only one side is.
            let reason = if over_passages {
                tracing::debug!(
                    a = %link.a_id, b = %link.b_id,
                    "duplicate verdict over a passage; not handing it to consolidation"
                );
                if a.provenance == crate::store::artifacts::Provenance::Passage
                    && b.provenance == crate::store::artifacts::Provenance::Passage
                {
                    "same content, and both are stored source text"
                } else {
                    "same content, and one of these is stored source text"
                }
            } else if core
                .store
                .record_pair_with_detail(&link.a_id, &link.b_id, 0.0, "link")
                .await?
            {
                "same content; handed to consolidation"
            } else {
                "same content; consolidation already has this pair"
            };
            core.store
                .set_link_state(
                    &link.a_id,
                    &link.b_id,
                    LinkState::Related,
                    Some(reason),
                    revs,
                )
                .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_support::test_core;
    use crate::store::artifacts::NewArtifact;
    use crate::store::feedback::{Door, NewCandidate, NewEvent, Verdict};
    use crate::store::links::LinkState;

    async fn seed(core: &Core, n: usize) -> Vec<String> {
        let mut ids = Vec::new();
        for i in 0..n {
            let src = core
                .store
                .insert_corpus(&format!("raw {i}"), "web", None)
                .await
                .unwrap();
            let made = core
                .store
                .insert_artifacts(
                    &src.id,
                    &[NewArtifact {
                        text: format!("artifact {i}"),
                        title: Some(format!("t{i}")),
                        ..Default::default()
                    }],
                )
                .await
                .unwrap();
            ids.push(made[0].id.clone());
        }
        ids
    }

    async fn record(core: &Core, query: &str, shown: &[&String], unshown: &[&String]) -> String {
        let candidates = shown
            .iter()
            .map(|id| NewCandidate {
                artifact_id: (*id).clone(),
                score: 1.0,
                similarity: Some(0.9),
                shown: true,
                ..Default::default()
            })
            .chain(unshown.iter().map(|id| NewCandidate {
                artifact_id: (*id).clone(),
                score: 0.1,
                similarity: Some(0.2),
                ..Default::default()
            }))
            .collect();
        core.store
            .record_search(
                NewEvent {
                    fold_onto: None,
                    query: query.into(),
                    door: Door::Ui,
                    scope: None,
                    filters: "{}".into(),
                    query_vec: vec![0.0],
                    embed_model: "fake".into(),
                    candidates,
                    answered: false,
                    context: None,
                },
                0,
            )
            .await
            .unwrap()
    }

    async fn settle(core: &Core) {
        sqlx::query!(
            "UPDATE search_events
                SET created_at = created_at - 3600,
                    judged_at = judged_at - 3600"
        )
        .execute(&core.store.pool)
        .await
        .unwrap();
    }

    async fn on(core: &mut Core) {
        core.learn.enabled = true;
    }

    #[tokio::test]
    async fn two_searches_showing_the_same_pair_bind_it_twice() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 2).await;
        record(&core, "fat32 cluster", &[&ids[0], &ids[1]], &[]).await;
        record(&core, "ntfs journal", &[&ids[0], &ids[1]], &[]).await;
        settle(&core).await;

        run(&core).await.unwrap();

        let l = core
            .store
            .get_link(&ids[0], &ids[1])
            .await
            .unwrap()
            .unwrap();
        assert!((l.weight - 2.0).abs() < 1e-6, "weight was {}", l.weight);
        assert_eq!(l.queries, 2, "two different questions bound this pair");
        assert_eq!(l.state, LinkState::Learning);
    }

    #[tokio::test]
    async fn the_same_question_asked_twice_is_one_binding_query() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 2).await;
        record(&core, "fat32", &[&ids[0], &ids[1]], &[]).await;
        // Coalescing is off in `record`, so this is a second event: a second use of one question.
        record(&core, "  FAT32  ", &[&ids[0], &ids[1]], &[]).await;
        settle(&core).await;

        run(&core).await.unwrap();

        let l = core
            .store
            .get_link(&ids[0], &ids[1])
            .await
            .unwrap()
            .unwrap();
        assert!((l.weight - 2.0).abs() < 1e-6);
        assert_eq!(l.queries, 1);
    }

    #[tokio::test]
    async fn only_what_the_searcher_saw_fires_together() {
        // The stored pool is wider than what was shown, for evaluation's sake.
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 3).await;
        record(&core, "q", &[&ids[0], &ids[1]], &[&ids[2]]).await;
        settle(&core).await;

        run(&core).await.unwrap();

        assert!(
            core.store
                .get_link(&ids[0], &ids[1])
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            core.store
                .get_link(&ids[0], &ids[2])
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn an_event_still_folding_is_left_for_the_next_sweep() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 2).await;
        record(&core, "fat", &[&ids[0], &ids[1]], &[]).await;

        run(&core).await.unwrap();
        assert!(
            core.store
                .get_link(&ids[0], &ids[1])
                .await
                .unwrap()
                .is_none()
        );

        settle(&core).await;
        run(&core).await.unwrap();
        assert!(
            core.store
                .get_link(&ids[0], &ids[1])
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn an_event_exactly_at_the_fold_boundary_waits_one_more_sweep() {
        // `run()` reads `now()` itself; computing `created_at` from a second read races it across a
        // second boundary, so this derives `at` from the stamped value.
        let mut core = test_core().await;
        on(&mut core).await;
        core.feedback.coalesce_secs = 15;
        let ids = seed(&core, 2).await;
        let ev = record(&core, "fat", &[&ids[0], &ids[1]], &[]).await;
        let created_at: i64 =
            sqlx::query_scalar!("SELECT created_at FROM search_events WHERE id = ?", &ev)
                .fetch_one(&core.store.pool)
                .await
                .unwrap();

        let at = created_at + core.feedback.coalesce_secs;
        replay_events(&core, at).await.unwrap();
        assert!(
            core.store
                .get_link(&ids[0], &ids[1])
                .await
                .unwrap()
                .is_none(),
            "an event still exactly at the fold boundary was replayed early"
        );

        replay_events(&core, at + 1).await.unwrap();
        assert!(
            core.store
                .get_link(&ids[0], &ids[1])
                .await
                .unwrap()
                .is_some(),
            "an event one second past the boundary was still withheld"
        );
    }

    #[test]
    fn settled_cutoff_is_the_exact_complement_of_the_fold_predicate() {
        let at = 1_000_000;
        let coalesce_secs = 15;
        let cutoff = settled_cutoff(at, coalesce_secs);

        let is_fresh = |created: i64| at - created <= coalesce_secs;
        let is_settled = |created: i64| created < cutoff;

        for created in (at - coalesce_secs - 2)..=(at - coalesce_secs + 2) {
            assert_eq!(
                is_settled(created),
                !is_fresh(created),
                "created_at {created} disagreed with the fold predicate"
            );
        }

        assert_eq!(settled_cutoff(at, 0), at);
        assert_eq!(settled_cutoff(at, -5), at);
    }

    #[tokio::test]
    async fn a_replayed_event_is_never_replayed_again() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 2).await;
        record(&core, "q", &[&ids[0], &ids[1]], &[]).await;
        settle(&core).await;

        run(&core).await.unwrap();
        run(&core).await.unwrap();

        let l = core
            .store
            .get_link(&ids[0], &ids[1])
            .await
            .unwrap()
            .unwrap();
        assert!(
            (l.weight - 1.0).abs() < 1e-6,
            "the event was replayed twice"
        );
    }

    #[tokio::test]
    async fn a_confirmed_answer_binds_harder_and_raises_its_activation() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 3).await;
        let ev = record(&core, "q", &[&ids[0], &ids[1], &ids[2]], &[]).await;
        core.store
            .judge_hit(&ev, &ids[0], crate::store::feedback::Labeller::Deck)
            .await
            .unwrap();
        settle(&core).await;

        run(&core).await.unwrap();

        // +1 for co-appearance, +2 more for the pairs containing the answer.
        let with = core
            .store
            .get_link(&ids[0], &ids[1])
            .await
            .unwrap()
            .unwrap();
        let without = core
            .store
            .get_link(&ids[1], &ids[2])
            .await
            .unwrap()
            .unwrap();
        assert!(
            (with.weight - 3.0).abs() < 1e-6,
            "weight was {}",
            with.weight
        );
        assert!((without.weight - 1.0).abs() < 1e-6);

        let act = core.store.activation_of(&ids).await.unwrap();
        assert!(
            act[&ids[0]].0 > act[&ids[1]].0,
            "the confirmed answer gained no activation"
        );
    }

    #[tokio::test]
    async fn an_answer_the_search_never_showed_raises_activation_but_binds_nothing() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 3).await;
        let ev = record(&core, "q", &[&ids[0], &ids[1]], &[]).await;
        core.store
            .judge_hit(&ev, &ids[2], crate::store::feedback::Labeller::Deck)
            .await
            .unwrap();
        settle(&core).await;

        run(&core).await.unwrap();

        assert!(
            core.store
                .get_link(&ids[2], &ids[0])
                .await
                .unwrap()
                .is_none(),
            "a find bound a pair that was never shown together"
        );
        assert!(
            core.store
                .get_link(&ids[2], &ids[1])
                .await
                .unwrap()
                .is_none(),
            "a find bound a pair that was never shown together"
        );
        assert!(
            core.store
                .get_link(&ids[0], &ids[1])
                .await
                .unwrap()
                .is_some()
        );

        let act = core.store.activation_of(&ids).await.unwrap();
        assert!(
            act[&ids[2]].0 > act[&ids[1]].0,
            "the find gained no activation"
        );
    }

    #[tokio::test]
    async fn a_gap_and_a_discard_teach_nothing_about_pairs() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 2).await;
        let g = record(&core, "nothing about this", &[&ids[0], &ids[1]], &[]).await;
        core.store
            .judge(&g, Verdict::Gap, crate::store::feedback::Labeller::Deck)
            .await
            .unwrap();
        settle(&core).await;

        run(&core).await.unwrap();

        let l = core
            .store
            .get_link(&ids[0], &ids[1])
            .await
            .unwrap()
            .unwrap();
        assert!((l.weight - 1.0).abs() < 1e-6, "weight was {}", l.weight);
    }

    #[tokio::test]
    async fn a_faded_link_is_forgotten_and_a_judged_one_is_not() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 4).await;
        core.store
            .bump_link(&ids[0], &ids[1], 1.0, Some("q"), 30.0, 0)
            .await
            .unwrap();
        core.store
            .bump_link(&ids[2], &ids[3], 1.0, Some("q"), 30.0, 0)
            .await
            .unwrap();
        core.store
            .set_link_state(
                &ids[2],
                &ids[3],
                LinkState::Related,
                Some("why"),
                Some((0, 0)),
            )
            .await
            .unwrap();

        run(&core).await.unwrap();

        assert!(
            core.store
                .get_link(&ids[0], &ids[1])
                .await
                .unwrap()
                .is_none(),
            "a link last used at the epoch has decayed to nothing"
        );
        assert!(
            core.store
                .get_link(&ids[2], &ids[3])
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn a_strong_cross_corpus_link_is_armed_for_the_judge_exactly_once() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 2).await;
        for q in ["one", "two", "three", "four"] {
            core.store
                .bump_link(&ids[0], &ids[1], 1.0, Some(q), 30.0, crate::store::now())
                .await
                .unwrap();
        }

        run(&core).await.unwrap();
        let target = link_target(&ids[0], &ids[1]);
        assert!(
            core.store
                .live_job(crate::store::jobs::Stage::LinkJudge, &target)
                .await
                .unwrap()
        );

        // The guard against re-arming is `rearm_idle_seq`'s `WHERE jobs.state = 'done'`, not
        // `live_job`; this only pins the attempts.
        run(&core).await.unwrap();
        let mut seen = 0;
        while let Some(j) = core.store.claim_job().await.unwrap() {
            if j.stage == crate::store::jobs::Stage::LinkJudge {
                seen += 1;
                assert_eq!(j.attempts, 1, "the unit was re-armed underneath itself");
            }
        }
        assert_eq!(seen, 1);
    }

    #[tokio::test]
    async fn a_judged_link_is_reopened_when_its_text_changes_under_it() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 2).await;
        core.store
            .bump_link(&ids[0], &ids[1], 9.0, Some("q"), 30.0, crate::store::now())
            .await
            .unwrap();
        core.store
            .set_link_state(
                &ids[0],
                &ids[1],
                LinkState::Unrelated,
                Some("coincidence"),
                Some((0, 0)),
            )
            .await
            .unwrap();
        core.store
            .update_artifact_text(&ids[0], "rewritten")
            .await
            .unwrap();

        run(&core).await.unwrap();

        let l = core
            .store
            .get_link(&ids[0], &ids[1])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            l.state,
            LinkState::Learning,
            "the judge read text that is gone"
        );
    }

    #[test]
    fn a_batch_cut_inside_one_second_leaves_that_second_for_the_next_sweep() {
        assert_eq!(
            replayable(&[1, 2, 3, 3], 4),
            2,
            "a second the batch may have cut in half was replayed anyway"
        );
        // More rows stamped 4 may sit just past the limit.
        assert_eq!(replayable(&[1, 2, 3, 4], 4), 3);
        assert_eq!(replayable(&[1, 2, 3, 3], 8), 4);
        assert_eq!(replayable(&[], 8), 0);
        assert_eq!(replayable(&[7, 7, 7], 3), 3);
    }

    #[tokio::test]
    async fn the_watermark_advances_only_over_seconds_that_were_replayed_whole() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 3).await;
        record(&core, "a", &[&ids[0], &ids[1]], &[]).await;
        record(&core, "b", &[&ids[1], &ids[2]], &[]).await;
        settle(&core).await;
        sqlx::query!("UPDATE search_events SET created_at = 1000")
            .execute(&core.store.pool)
            .await
            .unwrap();

        replay_events(&core, 100_000).await.unwrap();

        assert_eq!(
            core.store.meta_get(EVENTS_AFTER).await.unwrap().as_deref(),
            Some("1000")
        );
        assert!(
            core.store
                .get_link(&ids[1], &ids[2])
                .await
                .unwrap()
                .is_some(),
            "the second event of the second was skipped"
        );
    }

    #[test]
    fn the_watermarks_named_here_are_the_ones_migrate_seeds() {
        // `Store::migrate` seeds these keys spelled out, as `store` does not depend on `jobs`.
        assert_eq!(EVENTS_AFTER, "associate.events_after");
        assert_eq!(JUDGED_AFTER, "associate.judged_after");
    }

    #[tokio::test]
    async fn a_queued_unit_spends_no_call_once_search_recording_is_switched_off() {
        let mut core = test_core().await;
        on(&mut core).await;
        let scripted = std::sync::Arc::new(crate::infer::fake::ScriptedCompleter::new(vec![
            r#"{"relation":"related","reason":"should never be read"}"#.into(),
        ]));
        core.link_judge = Some(scripted.clone());
        let ids = seed(&core, 2).await;
        core.store
            .bump_link(&ids[0], &ids[1], 5.0, Some("q"), 30.0, crate::store::now())
            .await
            .unwrap();
        core.learn.enabled = false;

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        assert_eq!(scripted.calls(), 0);
        assert_eq!(
            core.store
                .get_link(&ids[0], &ids[1])
                .await
                .unwrap()
                .unwrap()
                .state,
            LinkState::Learning
        );
    }

    #[test]
    fn a_link_names_itself_the_same_way_round_however_it_is_armed() {
        assert_eq!(link_target("b", "a"), link_target("a", "b"));
        assert_eq!(link_target("a", "b"), "a|b");
    }

    #[test]
    fn a_verdict_is_read_out_of_the_reply_and_an_unreadable_one_is_an_error() {
        let (v, why) =
            parse_link(r#"{"relation":"related","reason":"both about mounting"}"#).unwrap();
        assert_eq!(v, LinkVerdict::Related);
        assert_eq!(why, "both about mounting");
        assert_eq!(
            parse_link(r#"{"relation":"duplicate","reason":"same thing"}"#)
                .unwrap()
                .0,
            LinkVerdict::Duplicate
        );
        assert!(parse_link("I think they are related!").is_err());
        assert!(parse_link(r#"{"relation":"maybe","reason":"x"}"#).is_err());
    }

    #[tokio::test]
    async fn a_related_verdict_names_the_relation_and_stops_the_decay() {
        let mut core = test_core().await;
        on(&mut core).await;
        core.link_judge = Some(std::sync::Arc::new(crate::infer::fake::FakeCompleter {
            reply: Some(r#"{"relation":"related","reason":"the config and its errors"}"#.into()),
        }));
        let ids = seed(&core, 2).await;
        core.store
            .bump_link(&ids[0], &ids[1], 5.0, Some("q"), 30.0, crate::store::now())
            .await
            .unwrap();

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        let l = core
            .store
            .get_link(&ids[0], &ids[1])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(l.state, LinkState::Related);
        assert_eq!(l.reason.as_deref(), Some("the config and its errors"));
        assert_eq!(l.judged_rev_a, Some(0));
    }

    #[tokio::test]
    async fn a_verdict_with_no_line_stores_no_reason_rather_than_an_empty_one() {
        let mut core = test_core().await;
        on(&mut core).await;
        core.link_judge = Some(std::sync::Arc::new(crate::infer::fake::FakeCompleter {
            reply: Some(r#"{"relation":"related"}"#.into()),
        }));
        let ids = seed(&core, 2).await;
        core.store
            .bump_link(&ids[0], &ids[1], 5.0, Some("q"), 30.0, crate::store::now())
            .await
            .unwrap();

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        let l = core
            .store
            .get_link(&ids[0], &ids[1])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(l.state, LinkState::Related);
        assert_eq!(l.reason, None, "an empty reason was stored as Some(\"\")");
    }

    #[tokio::test]
    async fn an_unrelated_verdict_is_stored_so_it_is_not_asked_again() {
        let mut core = test_core().await;
        on(&mut core).await;
        core.link_judge = Some(std::sync::Arc::new(crate::infer::fake::FakeCompleter {
            reply: Some(r#"{"relation":"unrelated","reason":"a coincidence of retrieval"}"#.into()),
        }));
        let ids = seed(&core, 2).await;
        core.store
            .bump_link(&ids[0], &ids[1], 5.0, Some("q"), 30.0, crate::store::now())
            .await
            .unwrap();

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        assert_eq!(
            core.store
                .get_link(&ids[0], &ids[1])
                .await
                .unwrap()
                .unwrap()
                .state,
            LinkState::Unrelated
        );
        run(&core).await.unwrap();
        assert!(
            !core
                .store
                .live_job(
                    crate::store::jobs::Stage::LinkJudge,
                    &link_target(&ids[0], &ids[1])
                )
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn a_disguised_duplicate_is_handed_to_consolidation_and_still_shown() {
        let mut core = test_core().await;
        on(&mut core).await;
        core.link_judge = Some(std::sync::Arc::new(crate::infer::fake::FakeCompleter {
            reply: Some(r#"{"relation":"duplicate","reason":"the same procedure twice"}"#.into()),
        }));
        let ids = seed(&core, 2).await;
        core.store
            .bump_link(&ids[0], &ids[1], 5.0, Some("q"), 30.0, crate::store::now())
            .await
            .unwrap();

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        let pair = core
            .store
            .pair_state_between(&ids[0], &ids[1])
            .await
            .unwrap()
            .expect("consolidation was never told");
        assert_eq!(pair, crate::store::pairs::PairState::Pending);
        let l = core
            .store
            .get_link(&ids[0], &ids[1])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(l.state, LinkState::Related);
        assert!(l.reason.as_deref().unwrap().contains("consolidation"));
    }

    async fn seed_passages(core: &Core, n: usize) -> Vec<String> {
        let mut ids = Vec::new();
        for i in 0..n {
            let src = core
                .store
                .insert_corpus(&format!("skript {i}"), "web", None)
                .await
                .unwrap();
            let made = core
                .store
                .insert_artifacts_with_provenance(
                    &src.id,
                    &[NewArtifact {
                        text: format!("Spuren sind materielle Veraenderungen {i}"),
                        title: Some(format!("t{i}")),
                        segment_idx: Some(0),
                        ..Default::default()
                    }],
                    crate::store::artifacts::Provenance::Passage,
                )
                .await
                .unwrap();
            ids.push(made[0].id.clone());
        }
        ids
    }

    #[tokio::test]
    async fn a_duplicate_verdict_over_passages_is_not_handed_to_consolidation() {
        let mut core = test_core().await;
        on(&mut core).await;
        core.link_judge = Some(std::sync::Arc::new(crate::infer::fake::FakeCompleter {
            reply: Some(r#"{"relation":"duplicate","reason":"the same definition twice"}"#.into()),
        }));
        let ids = seed_passages(&core, 2).await;
        core.store
            .bump_link(&ids[0], &ids[1], 5.0, Some("q"), 30.0, crate::store::now())
            .await
            .unwrap();

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        assert_eq!(
            core.store
                .pair_state_between(&ids[0], &ids[1])
                .await
                .unwrap(),
            None,
            "a passage is substrate on this path too"
        );
        let l = core
            .store
            .get_link(&ids[0], &ids[1])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            l.state,
            LinkState::Related,
            "the reader should still see the connection"
        );
        assert_eq!(
            l.reason.as_deref(),
            Some("same content, and both are stored source text"),
            "two passages are both source text, and the line may say so"
        );
    }

    #[tokio::test]
    async fn a_duplicate_over_one_passage_does_not_call_the_other_one_source_text() {
        let mut core = test_core().await;
        on(&mut core).await;
        core.link_judge = Some(std::sync::Arc::new(crate::infer::fake::FakeCompleter {
            reply: Some(r#"{"relation":"duplicate","reason":"the same definition twice"}"#.into()),
        }));
        let passage = seed_passages(&core, 1).await.remove(0);
        let captured = seed(&core, 1).await.remove(0);
        core.store
            .bump_link(
                &passage,
                &captured,
                5.0,
                Some("q"),
                30.0,
                crate::store::now(),
            )
            .await
            .unwrap();

        judge(&core, &link_target(&passage, &captured))
            .await
            .unwrap();

        assert_eq!(
            core.store
                .pair_state_between(&passage, &captured)
                .await
                .unwrap(),
            None,
            "the handover is refused whichever side the passage is on"
        );
        assert_eq!(
            core.store
                .get_link(&passage, &captured)
                .await
                .unwrap()
                .unwrap()
                .reason
                .as_deref(),
            Some("same content, and one of these is stored source text"),
            "the line claimed something untrue about the captured artifact"
        );
    }

    #[tokio::test]
    async fn three_unreadable_replies_shelve_the_link_rather_than_asking_forever() {
        let mut core = test_core().await;
        on(&mut core).await;
        core.link_judge = Some(std::sync::Arc::new(crate::infer::fake::FakeCompleter {
            reply: Some("no idea, sorry".into()),
        }));
        let ids = seed(&core, 2).await;
        core.store
            .bump_link(&ids[0], &ids[1], 5.0, Some("q"), 30.0, crate::store::now())
            .await
            .unwrap();
        let target = link_target(&ids[0], &ids[1]);

        for _ in 0..2 {
            assert!(
                judge(&core, &target).await.is_err(),
                "an unreadable reply is an error"
            );
            assert_eq!(
                core.store
                    .get_link(&ids[0], &ids[1])
                    .await
                    .unwrap()
                    .unwrap()
                    .state,
                LinkState::Learning,
                "the link stays visible while it is still being asked about"
            );
        }
        judge(&core, &target).await.unwrap();

        let l = core
            .store
            .get_link(&ids[0], &ids[1])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(l.state, LinkState::Unrelated);
        assert_eq!(l.reason.as_deref(), Some("unreadable"));
    }

    #[tokio::test]
    async fn a_link_that_has_already_been_answered_costs_no_call() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 2).await;
        core.store
            .bump_link(&ids[0], &ids[1], 5.0, Some("q"), 30.0, crate::store::now())
            .await
            .unwrap();
        core.store
            .set_link_state(&ids[0], &ids[1], LinkState::Dismissed, None, None)
            .await
            .unwrap();

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        assert_eq!(
            core.store
                .get_link(&ids[0], &ids[1])
                .await
                .unwrap()
                .unwrap()
                .state,
            LinkState::Dismissed
        );
    }

    #[tokio::test]
    async fn a_unit_already_queued_when_the_feature_is_switched_off_spends_no_call() {
        let mut core = test_core().await;
        on(&mut core).await;
        let scripted = std::sync::Arc::new(crate::infer::fake::ScriptedCompleter::new(vec![
            r#"{"relation":"related","reason":"should never be read"}"#.into(),
        ]));
        core.link_judge = Some(scripted.clone());
        let ids = seed(&core, 2).await;
        core.store
            .bump_link(&ids[0], &ids[1], 5.0, Some("q"), 30.0, crate::store::now())
            .await
            .unwrap();
        core.learn.enabled = false;

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        assert_eq!(
            scripted.calls(),
            0,
            "a call was spent after the feature was off"
        );
        assert_eq!(
            core.store
                .get_link(&ids[0], &ids[1])
                .await
                .unwrap()
                .unwrap()
                .state,
            LinkState::Learning,
            "an unjudged link stays visible as learning"
        );
    }

    #[tokio::test]
    async fn the_sweep_does_nothing_at_all_while_nothing_is_recorded() {
        let core = test_core().await; // feedback off, associate on
        let ids = seed(&core, 2).await;
        run(&core).await.unwrap();
        assert!(
            core.store
                .get_link(&ids[0], &ids[1])
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(core.store.meta_get(EVENTS_AFTER).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_verdict_recorded_in_the_second_the_sweep_ran_is_not_skipped() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 3).await;

        let first = record(&core, "one", &[&ids[0], &ids[1]], &[]).await;
        core.store
            .judge_hit(&first, &ids[0], crate::store::feedback::Labeller::Deck)
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE search_events SET judged_at = 1000 WHERE id = ?",
            &first
        )
        .execute(&core.store.pool)
        .await
        .unwrap();

        replay_verdicts(&core, 1000).await.unwrap();

        let second = record(&core, "two", &[&ids[0], &ids[2]], &[]).await;
        core.store
            .judge_hit(&second, &ids[2], crate::store::feedback::Labeller::Deck)
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE search_events SET judged_at = 1000 WHERE id = ?",
            &second
        )
        .execute(&core.store.pool)
        .await
        .unwrap();

        replay_verdicts(&core, 1001).await.unwrap();

        assert!(
            core.store
                .get_link(&ids[0], &ids[2])
                .await
                .unwrap()
                .is_some(),
            "the verdict recorded inside the sweep's own second was lost"
        );
        assert!(
            core.store
                .get_link(&ids[0], &ids[1])
                .await
                .unwrap()
                .is_some(),
            "the verdict the sweep did see was lost too"
        );
    }

    #[tokio::test]
    async fn a_duplicate_verdict_claims_no_handoff_when_the_pair_is_already_settled() {
        let mut core = test_core().await;
        on(&mut core).await;
        core.link_judge = Some(std::sync::Arc::new(crate::infer::fake::FakeCompleter {
            reply: Some(r#"{"relation":"duplicate","reason":"same thing"}"#.into()),
        }));
        let ids = seed(&core, 2).await;
        core.store
            .bump_link(&ids[0], &ids[1], 5.0, Some("q"), 30.0, crate::store::now())
            .await
            .unwrap();
        core.store
            .record_pair(&ids[0], &ids[1], 0.91)
            .await
            .unwrap();
        sqlx::query!("UPDATE artifact_pairs SET state = 'dismissed'")
            .execute(&core.store.pool)
            .await
            .unwrap();

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        let l = core
            .store
            .get_link(&ids[0], &ids[1])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(l.state, LinkState::Related);
        assert!(
            !l.reason
                .as_deref()
                .unwrap_or_default()
                .contains("handed to consolidation"),
            "the link claims a handoff that never happened: {:?}",
            l.reason
        );
    }

    #[tokio::test]
    async fn one_unbindable_pair_does_not_cost_an_event_its_other_pairs() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 3).await;
        record(&core, "three shown", &[&ids[0], &ids[1], &ids[2]], &[]).await;
        settle(&core).await;
        sqlx::query!("DELETE FROM artifacts WHERE id = ?", &ids[2])
            .execute(&core.store.pool)
            .await
            .unwrap();

        run(&core).await.unwrap();

        // The premise: the pairs naming the deleted side really did fail.
        for gone in [&ids[0], &ids[1]] {
            assert!(
                core.store.get_link(gone, &ids[2]).await.unwrap().is_none(),
                "a link was written against a deleted artifact"
            );
        }
        assert!(
            core.store
                .get_link(&ids[0], &ids[1])
                .await
                .unwrap()
                .is_some(),
            "a pair that could not be bound cost the event its other pairs"
        );
    }
}
