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

/// One sweep over everything learned since the last one.
pub async fn run(core: &Core) -> Result<()> {
    if !core.associate.enabled || !core.feedback.enabled {
        return Ok(());
    }
    let at = crate::store::now();
    let bound = replay_events(core, at).await?;
    let confirmed = replay_verdicts(core, at).await?;
    tracing::info!(events = bound, verdicts = confirmed, "association sweep");
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
    let settled = at - core.feedback.coalesce_secs.max(0);

    let events = sqlx::query(
        "SELECT id, query, created_at FROM search_events
          WHERE created_at > ? AND created_at <= ?
          ORDER BY created_at ASC, id ASC LIMIT ?",
    )
    .bind(after)
    .bind(settled)
    .bind(REPLAY_LIMIT)
    .fetch_all(&core.store.pool)
    .await?;

    let mut high = after;
    for e in &events {
        let id: String = e.get("id");
        let cue = normalize_query(&e.get::<String, _>("query"));
        let shown = shown_candidates(core, &id).await?;
        for i in 0..shown.len() {
            for j in (i + 1)..shown.len() {
                if let Err(err) = core
                    .store
                    .bump_link(
                        &shown[i],
                        &shown[j],
                        1.0,
                        Some(&cue),
                        core.associate.half_life_days,
                        at,
                    )
                    .await
                {
                    tracing::debug!(error = %err, "could not bind a pair; one side is gone");
                }
            }
        }
        high = high.max(e.get::<i64, _>("created_at"));
    }

    if high > after {
        core.store.meta_set(EVENTS_AFTER, &high.to_string()).await?;
    }
    Ok(events.len())
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

    let events = sqlx::query(
        "SELECT id, expect_id, judged_at FROM search_events
          WHERE judged_at > ? AND verdict = 'hit' AND expect_id IS NOT NULL
          ORDER BY judged_at ASC, id ASC LIMIT ?",
    )
    .bind(after)
    .bind(REPLAY_LIMIT)
    .fetch_all(&core.store.pool)
    .await?;

    let mut high = after;
    for e in &events {
        let id: String = e.get("id");
        let expect: String = e.get("expect_id");
        let shown = shown_candidates(core, &id).await?;
        for other in shown.iter().filter(|c| **c != expect) {
            // No cue: this event's words were already folded in as a binding
            // query when its co-appearance was replayed, and counting them
            // twice would say two questions bound this pair.
            if let Err(err) = core
                .store
                .bump_link(&expect, other, 2.0, None, core.associate.half_life_days, at)
                .await
            {
                tracing::debug!(error = %err, "could not bind a pair; one side is gone");
            }
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
        high = high.max(e.get::<i64, _>("judged_at"));
    }

    if high > after {
        core.store.meta_set(JUDGED_AFTER, &high.to_string()).await?;
    }
    Ok(events.len())
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

/// One link, one call. `target` is `"<a_id>|<b_id>"`.
pub async fn judge(core: &Core, target: &str) -> Result<()> {
    let _ = (core, target);
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
    async fn settle(core: &Core) {
        sqlx::query("UPDATE search_events SET created_at = created_at - 3600")
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
}
