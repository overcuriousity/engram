//! Learning what belongs together, and saying so.
//!
//! Two things happen here and they are deliberately not the same job. The sweep
//! is pure SQLite: it replays the search log, strengthens the pairs that were
//! reached together, fades and prunes the ones that were not, and decides which
//! links are worth asking about. The judge is one model call on one link, armed
//! by the sweep and paced by the queue like every other call in the system.

use crate::core::Core;
use crate::error::Result;
use crate::store::links::normalize_query;
use sqlx::Row;

/// Last `search_events.created_at` folded into links.
pub const EVENTS_AFTER: &str = "associate.events_after";
/// Last `search_events.judged_at` folded into links.
pub const JUDGED_AFTER: &str = "associate.judged_after";
/// Events read per sweep. A ceiling rather than a budget: at 30-minute ticks
/// nothing real reaches it, and a base that has been offline for a month
/// catches up over a few sweeps instead of holding one worker for minutes.
const REPLAY_LIMIT: i64 = 2_000;

/// Learning links read per prune pass. Whatever the cap leaves is pruned by the
/// next sweep — a bound on one tick's work, not on what is eventually forgotten.
const PRUNE_SCAN_LIMIT: i64 = 5_000;

/// The queue name of one link. Canonical, so the same pair never gets two units.
pub fn link_target(a: &str, b: &str) -> String {
    let (a, b) = crate::store::links::canonical(a, b);
    format!("{a}|{b}")
}

/// The newest `created_at` a settled event may still have. An event is
/// settled once `created_at < settled_cutoff(at, coalesce_secs)` — the exact
/// complement of `record_search`'s fold predicate at
/// `src/store/feedback.rs:206` (`at - created <= coalesce_secs`), so there is
/// no instant where both are true and an event still a keystroke away from
/// folding gets replayed early.
fn settled_cutoff(at: i64, coalesce_secs: i64) -> i64 {
    at - coalesce_secs.max(0)
}

/// How many rows of a batch it is safe to replay, given the stamps they are
/// ordered by.
///
/// Both reads are `ORDER BY <stamp> ASC, id ASC LIMIT REPLAY_LIMIT` and the
/// watermark each leaves behind is a stamp, not a row. So a batch that filled
/// the limit may have cut a group of rows sharing one second in half, and
/// advancing the watermark past that second would strand the remainder unread
/// forever. The last second of a full batch is therefore left for the next
/// sweep, which will read it whole.
///
/// The one shape with no smaller step available is a full batch whose every row
/// shares a single second. Then the remainder of that second is genuinely
/// skipped — and said so at `warn`, because a gap in what was learned from is
/// otherwise indistinguishable from there having been nothing to learn.
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

/// One sweep over everything learned since the last one.
pub async fn run(core: &Core) -> Result<()> {
    if !core.associating() {
        return Ok(());
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
    // A re-embed of either side reopens the verdict before anything is armed,
    // so a link whose text changed is re-asked in this same sweep rather than
    // waiting out another interval.
    let reopened = core
        .store
        .reopen_stale_judged_links(PRUNE_SCAN_LIMIT)
        .await?;

    // No judge, no units: a link stays `learning` and visible, and nothing is
    // queued that could never be claimed.
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
        // A link whose judgement is already queued is already going to be
        // judged; arming it again is a no-op that costs another link its turn.
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
    Ok(())
}

/// Every pair of shown candidates in every settled event past the watermark.
///
/// "Settled" is the whole of the read condition beyond the watermark: an event
/// inside `feedback.coalesce_secs` of now is still moving — a typing burst folds
/// into one row — and binding the pairs of a half-typed query would then be
/// followed by binding the finished one.
async fn replay_events(core: &Core, at: i64) -> Result<usize> {
    let after: i64 = core
        .store
        .meta_get(EVENTS_AFTER)
        .await?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let settled = settled_cutoff(at, core.feedback.coalesce_secs);

    let events = sqlx::query(
        "SELECT id, query, created_at FROM search_events
          WHERE created_at > ? AND created_at < ?
          ORDER BY created_at ASC, id ASC LIMIT ?",
    )
    .bind(after)
    .bind(settled)
    .bind(REPLAY_LIMIT)
    .fetch_all(&core.store.pool)
    .await?;

    let stamps: Vec<i64> = events
        .iter()
        .map(|e| e.get::<i64, _>("created_at"))
        .collect();
    let end = replayable(&stamps, REPLAY_LIMIT as usize);

    for (idx, e) in events[..end].iter().enumerate() {
        let id: String = e.get("id");
        let cue = normalize_query(&e.get::<String, _>("query"));
        let shown = shown_candidates(core, &id).await?;
        // One transaction for the whole event rather than one per pair: the
        // pairs are quadratic in what was shown, and each transaction is an
        // exclusive one. `bump_links` warns about and steps over any single
        // pair it cannot write.
        let pairs: Vec<(&str, &str)> = (0..shown.len())
            .flat_map(|i| ((i + 1)..shown.len()).map(move |j| (i, j)))
            .map(|(i, j)| (shown[i].as_str(), shown[j].as_str()))
            .collect();
        core.store
            .bump_links(&pairs, 1.0, Some(&cue), core.associate.half_life_days, at)
            .await?;
        // Committed as each second finishes rather than once at the end. A
        // failure below propagates out of the sweep, and a watermark that never
        // moved would replay every bump already written on the next tick.
        // Weight is what pruning, showing and judging all decide on, so
        // counting a co-appearance twice is not cosmetic.
        if last_of_its_second(&stamps, idx, end) {
            core.store
                .meta_set(EVENTS_AFTER, &stamps[idx].to_string())
                .await?;
        }
    }
    Ok(end)
}

/// Whether `idx` is the last row of the run of equal stamps it belongs to,
/// within the prefix being replayed. The watermark may only advance to a
/// second every row of which has been folded in.
fn last_of_its_second(stamps: &[i64], idx: usize, end: usize) -> bool {
    idx + 1 == end || stamps[idx + 1] > stamps[idx]
}

/// Every hit verdict past the second watermark: the pairs containing the
/// confirmed answer bind harder, and the answer itself becomes more accessible.
///
/// Its own cursor, because a verdict arrives days after the event it is about —
/// one cursor would either replay the event's pairs again or skip the verdict.
/// `gap` and `discard` are not read here: neither says anything about a pair.
async fn replay_verdicts(core: &Core, at: i64) -> Result<usize> {
    let after: i64 = core
        .store
        .meta_get(JUDGED_AFTER)
        .await?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    // Bounded above as well as below, for the reason `replay_events` is: the
    // watermark left behind is a second, not a row, and `last_of_its_second`
    // only closes that gap *within* a batch. A sweep running during second `at`
    // that read the verdicts stamped `at` would move the watermark to `at`, and
    // a verdict recorded moments later in that same second would then fail the
    // `> at` test on every sweep after — its link bumps and its activation bump
    // lost with no trace. `at` rather than `settled_cutoff`: nothing coalesces
    // verdicts the way a typing burst coalesces events, so there is no reason to
    // hold a confirmation back beyond the second it was written in.
    let events = sqlx::query(
        "SELECT id, expect_id, judged_at FROM search_events
          WHERE judged_at > ? AND judged_at < ?
            AND verdict = 'hit' AND expect_id IS NOT NULL
          ORDER BY judged_at ASC, id ASC LIMIT ?",
    )
    .bind(after)
    .bind(at)
    .bind(REPLAY_LIMIT)
    .fetch_all(&core.store.pool)
    .await?;

    let stamps: Vec<i64> = events
        .iter()
        .map(|e| e.get::<i64, _>("judged_at"))
        .collect();
    let end = replayable(&stamps, REPLAY_LIMIT as usize);

    for (idx, e) in events[..end].iter().enumerate() {
        let id: String = e.get("id");
        let expect: String = e.get("expect_id");
        let shown = shown_candidates(core, &id).await?;
        // Only a *shown* pair containing the answer is bound harder — a find,
        // where the operator confirmed an artifact the search never returned,
        // has no shown pair to strengthen. Binding it against the pool anyway
        // would invent a co-retrieval that never happened, which is exactly
        // the rule ("only what the searcher actually saw fires together")
        // this whole sweep exists to hold. The activation bump below is
        // unconditional and still fires for a find — that is the one signal a
        // find is allowed to give.
        if shown.contains(&expect) {
            // No cue: this event's words were already folded in as a binding
            // query when its co-appearance was replayed, and counting them
            // twice would say two questions bound this pair.
            let pairs: Vec<(&str, &str)> = shown
                .iter()
                .filter(|c| **c != expect)
                .map(|other| (expect.as_str(), other.as_str()))
                .collect();
            core.store
                .bump_links(&pairs, 2.0, None, core.associate.half_life_days, at)
                .await?;
        }
        // Raised whether or not the answer was in the pool at all — an artifact
        // the ranking never returned and a person confirmed anyway is the most
        // valuable confirmation there is.
        core.store
            .bump_activation(
                std::slice::from_ref(&expect),
                core.activation.confirmed,
                core.activation.half_life_days,
                at,
            )
            .await?;
        // Same reason as in `replay_events`: the bump above propagates, and a
        // verdict counted twice raises an artifact's activation twice.
        if last_of_its_second(&stamps, idx, end) {
            core.store
                .meta_set(JUDGED_AFTER, &stamps[idx].to_string())
                .await?;
        }
    }
    Ok(end)
}

/// What one event actually put in front of the searcher, in rank order.
async fn shown_candidates(core: &Core, event_id: &str) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT artifact_id FROM search_candidates
          WHERE event_id = ? AND shown = 1 ORDER BY rank",
    )
    .bind(event_id)
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
    /// The two say the same thing and the embedding failed to notice.
    Duplicate,
}

/// A reply that cannot be read is an error, not a verdict.
///
/// Defaulting to `unrelated` would quietly close real relations; defaulting to
/// `related` would show the reader a line the model never wrote. Failing leaves
/// the link `learning` — still visible, still binding — and the unit retries
/// under the queue's backoff with a prompt that differs by its attempt number.
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

/// One link, one call. `target` is `"<a_id>|<b_id>"`.
pub async fn judge(core: &Core, target: &str) -> Result<()> {
    // A unit already queued when the operator disables the feature should not
    // spend the one scarce thing in the system after they said stop. The
    // sweep will not arm more (`run` returns early), but a unit armed before
    // the flag flipped is still sitting in the queue when this runs.
    //
    // `associating()` and not `associate.enabled`: switching search recording
    // off switches this layer off too — that is what the predicate means, and
    // every other surface (priming, association, the detail pane, Ops) already
    // reads it. A verdict written after that switch would name a relation on a
    // page that no longer shows one.
    if !core.associating() {
        return Ok(());
    }
    let (a_id, b_id) = target.split_once('|').ok_or(Error::NotFound)?;
    let Some(link) = core.store.get_link(a_id, b_id).await? else {
        // Pruned, or one side deleted, while the unit waited out a backoff.
        return Ok(());
    };
    if link.state != LinkState::Learning {
        // Answered by an operator's dismissal, or by a sweep that reopened and
        // a sibling unit that then settled it.
        return Ok(());
    }
    let a = core.store.get_artifact(&link.a_id).await?;
    let b = core.store.get_artifact(&link.b_id).await?;
    // Re-checked here and not only when the unit was armed: a side can be
    // superseded or deprecated while this waits, and spending the scarcest
    // thing in the system on an artifact nobody will be shown buys nothing.
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
    // A call the endpoint never answered says nothing about the link: it stays
    // `learning`, stays visible, and the queue backs the unit off.
    let reply = reply?;

    let revs = Some((a.embed_rev, b.embed_rev));
    let (verdict, reason) = match parse_link(&reply) {
        Ok(v) => v,
        Err(e) => {
            // Counted only here, because this is the only failure that says
            // anything about the link itself.
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

    // A verdict with no line is still a verdict, so this does not fail the parse.
    // But it must not be stored as an empty string either: the pane falls back to
    // the binding query when a link has no reason, and `Some("")` would defeat that
    // fallback and render a blank explanation instead.
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
            // Handed over rather than acted on: consolidation owns every
            // decision that hides an artifact, with its own guards and its own
            // undo. The score is zero because no cosine was ever measured —
            // that is what the zero itself says on the review page.
            //
            // `INSERT OR IGNORE`, so this hands nothing over when a row for the
            // pair is already on file: an operator dismissed it, or the dedupe
            // judge answered it and consolidation is done with it. The verdict
            // is the same either way — these two say the same thing — but the
            // reason is rendered verbatim in the "Seen together" pane and in
            // the associated-results rail, so it must not assert a handover
            // that did not happen.
            let handed = core
                .store
                .record_pair_with_detail(&link.a_id, &link.b_id, 0.0, "link")
                .await?;
            core.store
                .set_link_state(
                    &link.a_id,
                    &link.b_id,
                    LinkState::Related,
                    Some(if handed {
                        "same content; handed to consolidation"
                    } else {
                        "same content; consolidation already has this pair"
                    }),
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

    /// `n` artifacts, each in its own corpus, so every link between them is a
    /// cross-corpus one.
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
                        ordinal: 0,
                        text: format!("artifact {i}"),
                        corpus_span: None,
                        title: Some(format!("t{i}")),
                        category: None,
                        tags: vec![],
                        segment_idx: None,
                        caveats: vec![],
                    }],
                )
                .await
                .unwrap();
            ids.push(made[0].id.clone());
        }
        ids
    }

    /// One recorded search, with `shown` deciding what the searcher saw.
    async fn record(core: &Core, query: &str, shown: &[&String], unshown: &[&String]) -> String {
        let candidates = shown
            .iter()
            .map(|id| NewCandidate {
                artifact_id: (*id).clone(),
                score: 1.0,
                similarity: Some(0.9),
                shown: true,
            })
            .chain(unshown.iter().map(|id| NewCandidate {
                artifact_id: (*id).clone(),
                score: 0.1,
                similarity: Some(0.2),
                shown: false,
            }))
            .collect();
        core.store
            .record_search(
                NewEvent {
                    query: query.into(),
                    door: Door::Ui,
                    scope: None,
                    filters: "{}".into(),
                    query_vec: vec![0.0],
                    embed_model: "fake".into(),
                    candidates,
                },
                0,
            )
            .await
            .unwrap()
    }

    /// Age every recorded event past the coalescing window, so the sweep will
    /// look at it: a folding event is still moving.
    /// Age everything the sweep reads far enough back that both of its upper
    /// bounds are cleared — `created_at` below `settled_cutoff`, and `judged_at`
    /// below the sweep's own clock. A verdict recorded in the second the sweep
    /// runs in is deliberately left for the next one (`replay_verdicts`), so a
    /// test that judges and sweeps in the same breath has to say which second
    /// it means. `judged_at` is NULL on an unjudged event and stays NULL here.
    async fn settle(core: &Core) {
        sqlx::query(
            "UPDATE search_events
                SET created_at = created_at - 3600,
                    judged_at = judged_at - 3600",
        )
        .execute(&core.store.pool)
        .await
        .unwrap();
    }

    async fn on(core: &mut Core) {
        core.feedback.enabled = true;
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
        // Coalescing is off in `record`, so this is a second event with the
        // same words — which is a second use, and one question.
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
        // The stored pool is wider than the answer for evaluation's sake. An
        // artifact nobody was shown was not reached, and did not fire.
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
        // A typing burst is one event, and it is not finished until the
        // coalescing window has passed. Replaying it early would bind the pairs
        // of a half-typed query and then bind the finished one again.
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
        // `record_search` treats an event as still foldable while
        // `(at - created_at) <= coalesce_secs` (src/store/feedback.rs:206).
        // The sweep's read must be the complement of that with nothing
        // shared, or there is an instant where both are true and the sweep
        // binds an event a further keystroke could still fold into — the
        // exact double-binding this whole two-watermark design exists to
        // prevent.
        //
        // `run()` reads `now()` itself, and reading it a second time here to
        // compute the aged `created_at` would race it: if the two calls
        // straddle a wall-clock second, the event ends up one second older
        // than intended and the assertion flips (roughly 1-in-200). So this
        // reads back the `created_at` `record_search` actually stamped and
        // derives everything from that one value, and drives `replay_events`
        // directly with an `at` computed from it — the same arithmetic
        // `run()` would use, with no live clock involved at all.
        let mut core = test_core().await;
        on(&mut core).await;
        core.feedback.coalesce_secs = 15;
        let ids = seed(&core, 2).await;
        let ev = record(&core, "fat", &[&ids[0], &ids[1]], &[]).await;
        let created_at: i64 =
            sqlx::query_scalar("SELECT created_at FROM search_events WHERE id = ?")
                .bind(&ev)
                .fetch_one(&core.store.pool)
                .await
                .unwrap();

        // `at` placed exactly `coalesce_secs` after the event: still
        // foldable, so the sweep must not replay it yet.
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

        // One second further: no longer foldable, so the next sweep replays it.
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
        // `replay_events` folds an event's watermark against `settled_cutoff`
        // with `<`. `record_search` (src/store/feedback.rs:206) treats an
        // event as still foldable while `at - created <= coalesce_secs`. For
        // the two to share no instant, `created < settled_cutoff(at, cs)`
        // must be false exactly where `at - created <= cs` is true, and true
        // everywhere else — checked here arithmetically, with no clock
        // involved.
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

        // A window of zero turns folding off entirely, so nothing is ever
        // "still folding" and the cutoff is `at` itself.
        assert_eq!(settled_cutoff(at, 0), at);
        // A clock or config that hands in a negative window must not make
        // the cutoff run past `at`.
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
        // Confirmation is the strong signal; co-appearance is the weak one.
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 3).await;
        let ev = record(&core, "q", &[&ids[0], &ids[1], &ids[2]], &[]).await;
        core.store.judge_hit(&ev, &ids[0]).await.unwrap();
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
        // A find: the operator confirmed an artifact the search never
        // returned at all. Spec §5.1 step 2 binds harder only the shown pairs
        // containing the answer — with no shown pair to strengthen, nothing
        // is bound. Step 3's activation bump has no such condition, and a
        // find is the most valuable confirmation there is.
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 3).await;
        // ids[2] is never offered as a candidate at all.
        let ev = record(&core, "q", &[&ids[0], &ids[1]], &[]).await;
        core.store.judge_hit(&ev, &ids[2]).await.unwrap();
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
        // The shown pair not involving the answer still binds on co-appearance.
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
        core.store.judge(&g, Verdict::Gap).await.unwrap();
        settle(&core).await;

        run(&core).await.unwrap();

        // Co-appearance still counts — the searcher did see both — but the
        // verdict adds nothing on top of it.
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

        // A second sweep must not wind the queued unit's attempts back. (The
        // guard that actually prevents re-arming here is `rearm_idle_seq`'s own
        // `WHERE jobs.state = 'done'` SQL condition, src/store/jobs.rs:189 —
        // not the `live_job` check above, which this assertion does not
        // isolate. What this pins is real regardless: a second sweep must not
        // wind a queued unit's attempts back.)
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
        // The watermark is a stamp, not a row. If a full batch ends in the
        // middle of a group of events sharing one second, advancing past that
        // second strands its remainder unread forever — so the group is left
        // whole for the next sweep, which reads it with room to spare.
        assert_eq!(
            replayable(&[1, 2, 3, 3], 4),
            2,
            "a second the batch may have cut in half was replayed anyway"
        );
        // A full batch always holds back its last second, whether or not the
        // rows read share it: what the limit cut off is invisible from here,
        // and more rows stamped 4 may be sitting just past it.
        assert_eq!(replayable(&[1, 2, 3, 4], 4), 3);
        // Not full: nothing was cut, so everything is replayable including the
        // last second.
        assert_eq!(replayable(&[1, 2, 3, 3], 8), 4);
        assert_eq!(replayable(&[], 8), 0);
        // A whole full batch inside one second has no smaller step to take.
        // Replaying it and moving on is the only way forward; `replayable`
        // says so at `warn` rather than silently.
        assert_eq!(replayable(&[7, 7, 7], 3), 3);
    }

    #[tokio::test]
    async fn the_watermark_advances_only_over_seconds_that_were_replayed_whole() {
        // Two events in one second and one in the next: the watermark must
        // never sit between the two, or the second of them is never read.
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 3).await;
        record(&core, "a", &[&ids[0], &ids[1]], &[]).await;
        record(&core, "b", &[&ids[1], &ids[2]], &[]).await;
        settle(&core).await;
        sqlx::query("UPDATE search_events SET created_at = 1000")
            .execute(&core.store.pool)
            .await
            .unwrap();

        replay_events(&core, 100_000).await.unwrap();

        assert_eq!(
            core.store.meta_get(EVENTS_AFTER).await.unwrap().as_deref(),
            Some("1000")
        );
        // Both were read, not just the first.
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
        // `Store::migrate` seeds these two keys when it adopts an existing
        // database into activation, spelling them out because nothing in
        // `store` reaches up into `jobs`. Renaming one there and not here
        // would make the seeding silently stop working.
        assert_eq!(EVENTS_AFTER, "associate.events_after");
        assert_eq!(JUDGED_AFTER, "associate.judged_after");
    }

    #[tokio::test]
    async fn a_queued_unit_spends_no_call_once_search_recording_is_switched_off() {
        // `associating()`, not `associate.enabled`: turning capture off turns
        // this whole layer off — priming, association and the pane all read
        // that predicate — and a verdict written afterwards would name a
        // relation on a page that no longer shows one.
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
        core.feedback.enabled = false;

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
        // A `related` reply that omits `reason` is still a usable verdict, and
        // `parse_link` does not fail it. But `Some("")` would defeat the pane's
        // fallback to the top binding query, so the empty string must never
        // reach the row.
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
        // ...and it is never armed again, however strong it becomes.
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
        // The embedding failed to notice; the reader should still see the
        // connection while dedupe decides what to do about it.
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
        // The sweep will not arm more once `associate.enabled` is false — see
        // `the_sweep_does_nothing_at_all_while_nothing_is_recorded` — but a unit
        // already in the queue when the operator disables the feature must not
        // spend the one scarce thing in the system after they said stop.
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
        core.associate.enabled = false;

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
        // `replay_events` reads below a settled cutoff, so it never consumes
        // the second it is running in. Verdicts had no upper bound at all: a
        // sweep at second T read every verdict stamped T and moved the
        // watermark to T, and a verdict recorded moments later in that same
        // second was then past the watermark's own `> T` test forever — its
        // link bumps and its activation bump silently lost.
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 3).await;

        let first = record(&core, "one", &[&ids[0], &ids[1]], &[]).await;
        core.store.judge_hit(&first, &ids[0]).await.unwrap();
        sqlx::query("UPDATE search_events SET judged_at = 1000 WHERE id = ?")
            .bind(&first)
            .execute(&core.store.pool)
            .await
            .unwrap();

        // The sweep that ran during second 1000.
        replay_verdicts(&core, 1000).await.unwrap();

        // A second verdict written moments later, still inside second 1000.
        let second = record(&core, "two", &[&ids[0], &ids[2]], &[]).await;
        core.store.judge_hit(&second, &ids[2]).await.unwrap();
        sqlx::query("UPDATE search_events SET judged_at = 1000 WHERE id = ?")
            .bind(&second)
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
        // `record_pair_with_detail` is `INSERT OR IGNORE`. A pair an operator
        // already dismissed, or one the dedupe judge already closed, is left
        // exactly as it was — nothing reaches consolidation. The link's reason
        // is rendered verbatim in the "Seen together" pane, so it must not
        // assert a handover that did not happen.
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
        // The pair is already on file and already answered.
        core.store
            .record_pair(&ids[0], &ids[1], 0.91)
            .await
            .unwrap();
        sqlx::query("UPDATE artifact_pairs SET state = 'dismissed'")
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
        // Every pair of one event's shown candidates is now bound in a single
        // transaction rather than one apiece. A pair that cannot be written —
        // one side deleted between the search and the sweep — must still be
        // warned about and stepped over, not take the rest of the event's
        // learning down with it.
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 3).await;
        record(&core, "three shown", &[&ids[0], &ids[1], &ids[2]], &[]).await;
        settle(&core).await;
        // Gone by the time the sweep reads the event it was shown in.
        sqlx::query("DELETE FROM artifacts WHERE id = ?")
            .bind(&ids[2])
            .execute(&core.store.pool)
            .await
            .unwrap();

        run(&core).await.unwrap();

        // The premise first: the pairs naming the deleted side really did fail,
        // rather than this test quietly binding three live artifacts.
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
