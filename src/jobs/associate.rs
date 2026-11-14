//! Learning what belongs together: a pure-SQLite sweep over the search log, and a queued decision
//! per link: what kind of relation it is, rendered as a line, never written by a model.

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

    // A decision unit: armed only inside this tick's decision budget.
    let budget = core
        .associate
        .judge_per_sweep
        .min(i64::try_from(core.judging_budget().await?).unwrap_or(i64::MAX));
    let to_judge = core
        .store
        .links_to_judge(
            &link_era(core),
            core.associate.judge_min,
            core.associate.judge_min_queries,
            core.associate.half_life_days,
            at,
            budget,
        )
        .await?;
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

use crate::core::judging::band::Band;
use crate::core::judging::{self, Applier, Subjects, catalogue, state};
use crate::error::Error;
use crate::infer::decide::decide_swapped;
use crate::store::actions::Job;
use crate::store::artifacts::{Chunk, Provenance};
use crate::store::decision_log::Logged;
use crate::store::links::{Link, LinkKind, LinkState};
use crate::store::pairs::DecidedBy;
use async_trait::async_trait;

fn day(at: i64) -> Option<String> {
    chrono::DateTime::from_timestamp(at, 0).map(|d| d.format("%Y-%m-%d").to_string())
}

fn note(c: &Chunk) -> state::Note<'_> {
    state::Note {
        title: c.title.as_deref(),
        day: day(c.created_at),
        text: &c.text,
    }
}

pub(crate) fn link_era(core: &Core) -> String {
    crate::core::judging::band::era(core.decider.model(), catalogue::link_kind().version)
}

/// Asked in this era about both texts as they are now, and not put back since.
async fn asked_already(core: &Core, l: &Link, a: &Chunk, b: &Chunk) -> Result<bool> {
    let last = core
        .store
        .last_logged("link_kind", &l.a_id, Some(&l.b_id), &link_era(core))
        .await?;
    Ok(last.is_some_and(|d| {
        d.rev_a == Some(a.embed_rev)
            && d.rev_b == Some(b.embed_rev)
            && !matches!(d.reason.as_deref(), Some("asked_again" | "changed"))
    }))
}

/// `Stage::LinkJudge`, target `"<a_id>|<b_id>"`: ask `link_kind`, with `link_direction` in the same
/// request, and settle the answer.
pub async fn judge(core: &Core, target: &str) -> Result<()> {
    // A unit queued before `[learn]` was switched off must not spend a call after the operator
    // said stop.
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
    // A retried or second unit must not ask what is already answered.
    if asked_already(core, &link, &a, &b).await? {
        return Ok(());
    }

    let (kind, direction) = (catalogue::link_kind(), catalogue::link_direction());
    let cues: Vec<String> = link.cues.iter().map(|c| c.q.clone()).collect();
    let room = state::room(core, &[&kind, &direction]);
    let s = state::link(&note(&a), &note(&b), &cues, &core.counter, room);
    let subjects = Subjects {
        kind: "link",
        a: link.a_id.clone(),
        b: Some(link.b_id.clone()),
        rev_a: Some(a.embed_rev),
        rev_b: Some(b.embed_rev),
    };
    let d = judging::ask_with(
        core,
        &kind,
        &[("direction", &direction)],
        subjects,
        s.as_deref(),
    )
    .await?;
    let d = exchanged(core, d, &a, &b, &cues).await?;
    judging::settle(core, d).await?;
    Ok(())
}

/// A direction the pick could store is asked once more with the notes exchanged (B's text under
/// `A:`, A's under `B:`): option order cannot show a pull to the note labelled A, this can. The
/// exchanged pick is kept beside the direction's read; it is not a decision of its own.
async fn exchanged(
    core: &Core,
    d: Logged,
    a: &Chunk,
    b: &Chunk,
    cues: &[String],
) -> Result<Logged> {
    let directional = LinkKind::parse(&d.pick).is_some_and(LinkKind::directional);
    let mut answer: serde_json::Value = serde_json::from_str(&d.answer).unwrap_or_default();
    let agreed = answer["direction"]["agree"].as_bool() == Some(true);
    if d.band == Band::Leave || !directional || !agreed {
        return Ok(d);
    }
    let direction = catalogue::link_direction();
    let room = state::room(core, &[&direction]);
    let Some(s) = state::link(&note(b), &note(a), cues, &core.counter, room) else {
        return Ok(d);
    };
    let permit = core.gate.background_light().await;
    let got = decide_swapped(
        core.decider.as_ref(),
        &s,
        std::slice::from_ref(&direction.question),
    )
    .await;
    permit.finished();
    let got = got?;
    let key = &direction.question.key;
    let read = got
        .answers
        .answers
        .get(key)
        .ok_or_else(|| Error::InferenceRejected {
            role: "decide",
            detail: format!("the answer has none for \"{key}\""),
        })?;
    let (again, _) = crate::core::judging::band::pick_and_top(&direction.question, read);
    answer["direction"]["exchanged"] = again.into();
    core.store
        .set_decision_answer(&d.id, &answer.to_string())
        .await?;
    core.store.logged(&d.id).await?.ok_or(Error::NotFound)
}

/// Whether the decision's `subject_a` comes first: only when the direction's swapped read agreed
/// and, with the notes exchanged, the other label was named.
fn first_is_a(d: &Logged) -> Option<bool> {
    let v: serde_json::Value = serde_json::from_str(&d.answer).ok()?;
    let r = v.get("direction")?;
    if !r.get("agree")?.as_bool()? {
        return None;
    }
    match (r.get("pick")?.as_str()?, r.get("exchanged")?.as_str()?) {
        ("a_first", "b_first") => Some(true),
        ("b_first", "a_first") => Some(false),
        _ => None,
    }
}

/// What a kind does to a link. No journal row: a link hides nothing.
async fn carry_out(
    core: &Core,
    link: &Link,
    a: &Chunk,
    b: &Chunk,
    kind: LinkKind,
    first_is_a: Option<bool>,
    revs: Option<(i64, i64)>,
) -> Result<()> {
    let (x, y) = (link.a_id.as_str(), link.b_id.as_str());
    // Never over passages: they are not consolidation's material.
    let passages = [a, b]
        .iter()
        .filter(|c| c.provenance == Provenance::Passage)
        .count();
    match kind {
        LinkKind::Duplicate => {
            // Consolidation owns every decision that hides an artifact; score zero, as no cosine was
            // measured. The line is shown verbatim, so it claims no handover that did not happen
            // and calls only a passage stored source text.
            let reason = match passages {
                2 => "same content, and both are stored source text",
                1 => "same content, and one of these is stored source text",
                _ if core
                    .store
                    .record_pair_with_detail(x, y, 0.0, "link")
                    .await? =>
                {
                    "same content; handed to consolidation"
                }
                _ => "same content; consolidation already has this pair",
            };
            core.store
                .set_link_state(x, y, LinkState::Related, Some(reason), revs)
                .await
        }
        LinkKind::Contradicts => {
            if passages == 0 {
                core.store
                    .record_pair_with_detail(x, y, 0.0, "link")
                    .await?;
            }
            core.store
                .set_link_kind(x, y, LinkState::Related, Some(kind), None, revs)
                .await
        }
        LinkKind::Unrelated => {
            core.store
                .set_link_kind(x, y, LinkState::Unrelated, Some(kind), None, revs)
                .await
        }
        _ => {
            let first = first_is_a.filter(|_| kind.directional());
            core.store
                .set_link_kind(x, y, LinkState::Related, Some(kind), first, revs)
                .await
        }
    }
}

async fn artifact(core: &Core, id: &str) -> Result<Option<Chunk>> {
    match core.store.get_artifact(id).await {
        Ok(c) => Ok(Some(c)),
        Err(Error::NotFound) => Ok(None),
        Err(e) => Err(e),
    }
}

/// What `link_kind`'s answers do to a link.
pub struct LinkApplier;

#[async_trait]
impl Applier for LinkApplier {
    fn question_id(&self) -> &'static str {
        "link_kind"
    }

    fn label(&self) -> &'static str {
        "Related how?"
    }

    fn job(&self) -> Option<Job> {
        None
    }

    async fn buttons(&self, _: &Core, _: &Logged) -> Result<Vec<(String, String)>> {
        Ok([
            ("continues", "Next step"),
            ("example", "Example"),
            ("alternative", "Alternative"),
            ("background", "Background"),
            ("contradicts", "Contradicts"),
            ("duplicate", "Duplicate"),
            ("unrelated", "Unrelated"),
        ]
        .into_iter()
        .map(|(p, l)| (p.to_string(), l.to_string()))
        .collect())
    }

    async fn subjects(&self, core: &Core, d: &Logged) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        for id in [Some(&d.subject_a), d.subject_b.as_ref()]
            .into_iter()
            .flatten()
        {
            if let Some(c) = artifact(core, id).await? {
                out.push((c.id.clone(), crate::web::ui::row_label(&c).text));
            }
        }
        Ok(out)
    }

    async fn unchanged(&self, core: &Core, d: &Logged) -> Result<bool> {
        let Some(b_id) = d.subject_b.as_deref() else {
            return Ok(false);
        };
        if core.store.get_link(&d.subject_a, b_id).await?.is_none() {
            return Ok(false);
        }
        for (id, rev) in [(d.subject_a.as_str(), d.rev_a), (b_id, d.rev_b)] {
            let Some(c) = artifact(core, id).await? else {
                return Ok(false);
            };
            if !c.in_results() || Some(c.embed_rev) != rev {
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn apply(
        &self,
        core: &Core,
        d: &Logged,
        pick: &str,
        _: DecidedBy,
    ) -> Result<Vec<String>> {
        let kind = LinkKind::parse(pick)
            .ok_or_else(|| Error::Validation(format!("no link kind is called {pick:?}")))?;
        let Some(b_id) = d.subject_b.as_deref() else {
            return Ok(vec![]);
        };
        let Some(link) = core.store.get_link(&d.subject_a, b_id).await? else {
            return Ok(vec![]);
        };
        // Settled since it was asked, by a person's dismissal or a sibling: nothing is left to do.
        if link.state != LinkState::Learning {
            return Ok(vec![]);
        }
        let (Some(a), Some(b)) = (
            artifact(core, &link.a_id).await?,
            artifact(core, &link.b_id).await?,
        ) else {
            return Ok(vec![]);
        };
        // The revisions read: a text changed since reopens the link at the next sweep.
        let revs = d.rev_a.zip(d.rev_b);
        carry_out(core, &link, &a, &b, kind, first_is_a(d), revs).await?;
        Ok(vec![])
    }

    async fn reopen(&self, core: &Core, d: &Logged) -> Result<()> {
        let Some(b_id) = d.subject_b.as_deref() else {
            return Ok(());
        };
        // A person's dismissal since stays: asking again is not taking it back.
        match core.store.get_link(&d.subject_a, b_id).await? {
            Some(l) if l.state != LinkState::Dismissed => {
                core.store
                    .set_link_kind(&d.subject_a, b_id, LinkState::Learning, None, None, None)
                    .await
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::judging::band::Band;
    use crate::core::test_support::test_core;
    use crate::infer::decide::{Answer, Answers, Kind as QKind};
    use crate::infer::fake::RuleDecider;
    use crate::store::artifacts::NewArtifact;
    use crate::store::decision_log::State;
    use crate::store::feedback::{Door, NewCandidate, NewEvent, Verdict};
    use crate::store::links::LinkState;
    use std::sync::Arc;

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
        // Five, not four: four sit exactly on `judge_min`, and a second ticking before the sweep
        // decays them just under it.
        for q in ["one", "two", "three", "four", "five"] {
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

    /// How the decider reads a link: the kind and how sure, whether the kind's reversed read names
    /// another, which note comes first given the state, and whether the direction's reversed read
    /// names the other note.
    #[derive(Clone, Copy)]
    struct Says {
        kind: &'static str,
        top: f64,
        kind_twin: Option<&'static str>,
        first: fn(&str) -> &'static str,
        torn_direction: bool,
    }

    /// Always the note labelled A: the pull the exchanged read exists to catch.
    fn the_label(_: &str) -> &'static str {
        "a_first"
    }

    /// The note titled t0 comes first, whichever label it carries.
    fn the_text(state: &str) -> &'static str {
        if state.starts_with("A: t0") {
            "a_first"
        } else {
            "b_first"
        }
    }

    fn says(kind: &'static str) -> Says {
        Says {
            kind,
            top: 0.95,
            kind_twin: None,
            first: the_text,
            torn_direction: false,
        }
    }

    fn deciding(core: &mut Core, s: Says) -> Arc<RuleDecider> {
        let d = Arc::new(RuleDecider::new(move |state, qs| {
            let mut answers = std::collections::HashMap::new();
            for q in qs {
                let QKind::Choice { options } = &q.kind else {
                    return Err(Error::Validation("a choice only".into()));
                };
                let direction = options.iter().any(|(n, _)| n == "a_first");
                let (favoured, top) = if direction {
                    ((s.first)(state), 0.95)
                } else {
                    (s.kind, s.top)
                };
                let twin = q.key.ends_with('r');
                let said = match (direction, twin) {
                    (true, true) if s.torn_direction => {
                        if favoured == "a_first" {
                            "b_first"
                        } else {
                            "a_first"
                        }
                    }
                    (false, true) => s.kind_twin.unwrap_or(favoured),
                    _ => favoured,
                };
                let rest = (1.0 - top) / (options.len() - 1) as f64;
                answers.insert(
                    q.key.clone(),
                    Answer::Choice {
                        pick: said.into(),
                        probabilities: options
                            .iter()
                            .map(|(n, _)| if n == favoured { top } else { rest })
                            .collect(),
                        confidence: top,
                    },
                );
            }
            Ok(Answers {
                answers,
                model: "rule-decider".into(),
                input_tokens: 0,
            })
        }));
        core.decider = d.clone();
        d
    }

    /// Two artifacts from different documents, bound by one search.
    async fn bound(core: &Core) -> Vec<String> {
        let ids = seed(core, 2).await;
        core.store
            .bump_link(&ids[0], &ids[1], 5.0, Some("q"), 30.0, crate::store::now())
            .await
            .unwrap();
        ids
    }

    /// The line a reader of `from` sees beside the other note.
    async fn line_from(core: &Core, from: &str) -> Option<String> {
        let got = core
            .store
            .links_from(
                &[from.to_string()],
                &[LinkState::Related],
                30.0,
                crate::store::now(),
                0.0,
                10,
            )
            .await
            .unwrap();
        assert_eq!(got.len(), 1, "{got:?}");
        got[0].reason.clone()
    }

    async fn link_of(core: &Core, ids: &[String]) -> crate::store::links::Link {
        core.store
            .get_link(&ids[0], &ids[1])
            .await
            .unwrap()
            .unwrap()
    }

    fn link_era(core: &Core) -> String {
        crate::core::judging::band::era(
            core.decider.model(),
            crate::core::judging::catalogue::link_kind().version,
        )
    }

    async fn decision(core: &Core, ids: &[String]) -> crate::store::decision_log::Logged {
        let (a, b) = crate::store::links::canonical(&ids[0], &ids[1]);
        core.store
            .last_logged("link_kind", a, Some(b), &link_era(core))
            .await
            .unwrap()
            .expect("the link was asked")
    }

    async fn to_judge(core: &Core) -> Vec<crate::store::links::Link> {
        core.store
            .links_to_judge(&link_era(core), 4.0, 1, 30.0, crate::store::now(), 10)
            .await
            .unwrap()
    }

    #[test]
    fn each_kind_renders_a_line_for_each_side() {
        use crate::store::links::{LinkKind, link_line};
        // A is first; read from A (`from_a`), then from B.
        let both = |k| {
            (
                link_line(k, Some(true), true),
                link_line(k, Some(true), false),
            )
        };
        assert_eq!(
            both(LinkKind::Continues),
            (
                Some("The next step after this"),
                Some("The step before this")
            )
        );
        assert_eq!(
            both(LinkKind::Example),
            (
                Some("An example of this"),
                Some("What this is an example of")
            )
        );
        assert_eq!(
            both(LinkKind::Background),
            (
                Some("Uses what this explains"),
                Some("Explains what this uses")
            )
        );
        // B first: the lines trade sides.
        assert_eq!(
            link_line(LinkKind::Continues, Some(false), true),
            Some("The step before this")
        );
        for side in [true, false] {
            assert_eq!(
                link_line(LinkKind::Alternative, None, side),
                Some("Another way to do this")
            );
            assert_eq!(
                link_line(LinkKind::Contradicts, None, side),
                Some("States this differently")
            );
            assert_eq!(link_line(LinkKind::Unrelated, None, side), None);
            assert_eq!(link_line(LinkKind::Duplicate, None, side), None);
            for k in [LinkKind::Continues, LinkKind::Example, LinkKind::Background] {
                assert_eq!(link_line(k, None, side), Some("Related to this"));
            }
        }
        for k in [
            LinkKind::Continues,
            LinkKind::Example,
            LinkKind::Alternative,
            LinkKind::Background,
            LinkKind::Contradicts,
            LinkKind::Duplicate,
            LinkKind::Unrelated,
        ] {
            assert_eq!(LinkKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(LinkKind::parse("related"), None);
    }

    #[tokio::test]
    async fn a_sure_kind_is_stored_and_shown_as_its_line() {
        let mut core = test_core().await;
        on(&mut core).await;
        let decider = deciding(&mut core, says("example"));
        let ids = bound(&core).await;

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        let l = link_of(&core, &ids).await;
        assert_eq!(l.state, LinkState::Related);
        assert_eq!(l.kind, Some(crate::store::links::LinkKind::Example));
        let (a, _) = crate::store::links::canonical(&ids[0], &ids[1]);
        assert_eq!(l.first_is_a, Some(a == ids[0]), "t0 is the general one");
        assert_eq!(l.reason, None, "no text of the model's is stored");
        assert_eq!(l.judged_rev_a, Some(0));
        // t0 is the description, t1 the instance.
        assert_eq!(
            line_from(&core, &ids[0]).await.as_deref(),
            Some("An example of this")
        );
        assert_eq!(
            line_from(&core, &ids[1]).await.as_deref(),
            Some("What this is an example of")
        );
        assert_eq!(decider.calls(), 2, "the request, then the exchanged one");
        let d = decision(&core, &ids).await;
        assert_eq!((d.pick.as_str(), d.band), ("example", Band::Act));
        let answer: serde_json::Value = serde_json::from_str(&d.answer).unwrap();
        assert!(answer.get("direction").is_some(), "{answer}");
    }

    #[tokio::test]
    async fn a_directional_kind_whose_direction_is_torn_shows_the_neutral_line() {
        let mut core = test_core().await;
        on(&mut core).await;
        let decider = deciding(
            &mut core,
            Says {
                torn_direction: true,
                ..says("continues")
            },
        );
        let ids = bound(&core).await;

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        let l = link_of(&core, &ids).await;
        assert_eq!(l.state, LinkState::Related);
        assert_eq!(l.kind, Some(crate::store::links::LinkKind::Continues));
        assert_eq!(l.first_is_a, None);
        for id in &ids {
            assert_eq!(
                line_from(&core, id).await.as_deref(),
                Some("Related to this")
            );
        }
        assert_eq!(decider.calls(), 1, "a torn direction is not asked again");
    }

    #[tokio::test]
    async fn a_direction_that_follows_the_label_under_exchange_is_stored_undirected() {
        let mut core = test_core().await;
        on(&mut core).await;
        let decider = deciding(
            &mut core,
            Says {
                first: the_label,
                ..says("continues")
            },
        );
        let ids = bound(&core).await;

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        let l = link_of(&core, &ids).await;
        assert_eq!(l.state, LinkState::Related);
        assert_eq!(l.kind, Some(crate::store::links::LinkKind::Continues));
        assert_eq!(l.first_is_a, None, "A was named first both ways round");
        for id in &ids {
            assert_eq!(
                line_from(&core, id).await.as_deref(),
                Some("Related to this")
            );
        }
        assert_eq!(decider.calls(), 2);
        let exchanged = &decider.requests()[1];
        assert!(exchanged.0.starts_with("A: t1"), "{}", exchanged.0);
        assert!(
            exchanged.1.iter().all(|q| q.instructions
                == crate::core::judging::catalogue::link_direction()
                    .question
                    .instructions),
            "the exchanged request asks only the direction"
        );
    }

    #[tokio::test]
    async fn a_direction_that_follows_the_text_under_exchange_is_stored_with_first() {
        let mut core = test_core().await;
        on(&mut core).await;
        deciding(&mut core, says("continues"));
        let ids = bound(&core).await;

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        let l = link_of(&core, &ids).await;
        let (a, _) = crate::store::links::canonical(&ids[0], &ids[1]);
        assert_eq!(l.first_is_a, Some(a == ids[0]));
        assert_eq!(
            line_from(&core, &ids[0]).await.as_deref(),
            Some("The next step after this")
        );
        assert_eq!(
            line_from(&core, &ids[1]).await.as_deref(),
            Some("The step before this")
        );
    }

    #[tokio::test]
    async fn a_kind_with_no_direction_spends_no_exchanged_request() {
        let mut core = test_core().await;
        on(&mut core).await;
        let decider = deciding(&mut core, says("alternative"));
        let ids = bound(&core).await;

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        assert_eq!(decider.calls(), 1);
        assert_eq!(
            line_from(&core, &ids[1]).await.as_deref(),
            Some("Another way to do this")
        );
    }

    #[tokio::test]
    async fn a_queued_unit_spends_no_call_once_search_recording_is_switched_off() {
        let mut core = test_core().await;
        on(&mut core).await;
        let decider = deciding(&mut core, says("alternative"));
        let ids = bound(&core).await;
        core.learn.enabled = false;

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        assert_eq!(decider.calls(), 0);
        assert_eq!(link_of(&core, &ids).await.state, LinkState::Learning);
    }

    #[test]
    fn a_link_names_itself_the_same_way_round_however_it_is_armed() {
        assert_eq!(link_target("b", "a"), link_target("a", "b"));
        assert_eq!(link_target("a", "b"), "a|b");
    }

    #[tokio::test]
    async fn a_related_verdict_names_the_relation_and_stops_the_decay() {
        let mut core = test_core().await;
        on(&mut core).await;
        deciding(&mut core, says("alternative"));
        let ids = bound(&core).await;

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        let l = link_of(&core, &ids).await;
        assert_eq!(l.state, LinkState::Related);
        assert_eq!(
            line_from(&core, &ids[0]).await.as_deref(),
            Some("Another way to do this")
        );
        assert_eq!(l.judged_rev_a, Some(0));
    }

    #[tokio::test]
    async fn a_verdict_with_no_line_stores_no_reason_rather_than_an_empty_one() {
        let mut core = test_core().await;
        on(&mut core).await;
        deciding(&mut core, says("alternative"));
        let ids = bound(&core).await;

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        let l = link_of(&core, &ids).await;
        assert_eq!(l.state, LinkState::Related);
        assert_eq!(l.reason, None, "an empty reason was stored as Some(\"\")");
    }

    #[tokio::test]
    async fn an_unrelated_verdict_is_stored_so_it_is_not_asked_again() {
        let mut core = test_core().await;
        on(&mut core).await;
        deciding(&mut core, says("unrelated"));
        let ids = bound(&core).await;

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        let l = link_of(&core, &ids).await;
        assert_eq!(l.state, LinkState::Unrelated);
        assert_eq!(l.reason, None);
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
    async fn a_torn_link_is_an_unsure_row_and_stays_learning() {
        let mut core = test_core().await;
        on(&mut core).await;
        deciding(
            &mut core,
            Says {
                kind_twin: Some("unrelated"),
                ..says("alternative")
            },
        );
        let ids = bound(&core).await;

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        let d = decision(&core, &ids).await;
        assert_eq!(d.band, Band::Ask);
        assert_eq!(d.state, State::Open, "the row is seated");
        assert_eq!(d.subject_kind, "link");
        let l = link_of(&core, &ids).await;
        assert_eq!(l.state, LinkState::Learning);
        assert_eq!(l.kind, None);
    }

    #[tokio::test]
    async fn a_person_picking_a_kind_sets_it_and_can_ask_again() {
        let mut core = test_core().await;
        on(&mut core).await;
        deciding(
            &mut core,
            Says {
                kind_twin: Some("alternative"),
                ..says("unrelated")
            },
        );
        let ids = bound(&core).await;
        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();
        let d = decision(&core, &ids).await;
        assert_eq!(d.state, State::Open);
        let applier = core.appliers.get("link_kind").expect("registered");
        assert_eq!(applier.label(), "Related how?");
        assert!(applier.job().is_none());

        crate::core::judging::answer(&core, &d.id, Some("alternative"))
            .await
            .unwrap();

        let l = link_of(&core, &ids).await;
        assert_eq!(l.state, LinkState::Related);
        assert_eq!(l.kind, Some(crate::store::links::LinkKind::Alternative));
        assert_eq!(
            line_from(&core, &ids[0]).await.as_deref(),
            Some("Another way to do this")
        );
        assert!(to_judge(&core).await.is_empty());

        crate::core::judging::ask_again(&core, &d.id).await.unwrap();

        let l = link_of(&core, &ids).await;
        assert_eq!(l.state, LinkState::Learning);
        assert_eq!((l.kind, l.first_is_a), (None, None));
        assert_eq!(to_judge(&core).await.len(), 1, "asked again means asked");
    }

    #[tokio::test]
    async fn a_contradiction_between_linked_notes_is_handed_to_consolidation() {
        let mut core = test_core().await;
        on(&mut core).await;
        deciding(&mut core, says("contradicts"));
        let ids = bound(&core).await;

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        assert_eq!(
            core.store
                .pair_state_between(&ids[0], &ids[1])
                .await
                .unwrap(),
            Some(crate::store::pairs::PairState::Pending),
            "consolidation was never told"
        );
        let l = link_of(&core, &ids).await;
        assert_eq!(l.state, LinkState::Related);
        assert_eq!(l.kind, Some(crate::store::links::LinkKind::Contradicts));
        for id in &ids {
            assert_eq!(
                line_from(&core, id).await.as_deref(),
                Some("States this differently")
            );
        }
    }

    #[tokio::test]
    async fn a_disguised_duplicate_is_handed_to_consolidation_and_still_shown() {
        let mut core = test_core().await;
        on(&mut core).await;
        deciding(&mut core, says("duplicate"));
        let ids = bound(&core).await;

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        let pair = core
            .store
            .pair_state_between(&ids[0], &ids[1])
            .await
            .unwrap()
            .expect("consolidation was never told");
        assert_eq!(pair, crate::store::pairs::PairState::Pending);
        let l = link_of(&core, &ids).await;
        assert_eq!(l.state, LinkState::Related);
        assert!(l.reason.as_deref().unwrap().contains("consolidation"));
        assert_eq!(
            line_from(&core, &ids[0]).await.as_deref(),
            Some("same content; handed to consolidation")
        );
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
        deciding(&mut core, says("duplicate"));
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
        let l = link_of(&core, &ids).await;
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
    async fn a_contradiction_over_passages_is_shown_and_not_handed_over() {
        let mut core = test_core().await;
        on(&mut core).await;
        deciding(&mut core, says("contradicts"));
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
            None
        );
        assert_eq!(link_of(&core, &ids).await.state, LinkState::Related);
    }

    #[tokio::test]
    async fn a_duplicate_over_one_passage_does_not_call_the_other_one_source_text() {
        let mut core = test_core().await;
        on(&mut core).await;
        deciding(&mut core, says("duplicate"));
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
    async fn a_link_that_has_already_been_answered_costs_no_call() {
        let mut core = test_core().await;
        on(&mut core).await;
        let decider = deciding(&mut core, says("alternative"));
        let ids = bound(&core).await;
        core.store
            .set_link_state(&ids[0], &ids[1], LinkState::Dismissed, None, None)
            .await
            .unwrap();

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        assert_eq!(link_of(&core, &ids).await.state, LinkState::Dismissed);
        assert_eq!(decider.calls(), 0);
    }

    #[tokio::test]
    async fn a_unit_already_queued_when_the_feature_is_switched_off_spends_no_call() {
        let mut core = test_core().await;
        on(&mut core).await;
        let decider = deciding(&mut core, says("alternative"));
        let ids = bound(&core).await;
        core.learn.enabled = false;

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        assert_eq!(
            decider.calls(),
            0,
            "a call was spent after the feature was off"
        );
        assert_eq!(
            link_of(&core, &ids).await.state,
            LinkState::Learning,
            "an unjudged link stays visible as learning"
        );
    }

    #[tokio::test]
    async fn a_link_is_asked_once_per_era_and_again_when_a_text_changes() {
        let mut core = test_core().await;
        on(&mut core).await;
        let decider = deciding(
            &mut core,
            Says {
                kind_twin: Some("unrelated"),
                ..says("alternative")
            },
        );
        let ids = bound(&core).await;
        assert_eq!(to_judge(&core).await.len(), 1);

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();
        assert_eq!(link_of(&core, &ids).await.state, LinkState::Learning);
        assert!(to_judge(&core).await.is_empty(), "asked in this era");
        // A second unit for the same link asks nothing.
        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();
        assert_eq!(decider.calls(), 1);

        core.store
            .update_artifact_text(&ids[0], "rewritten")
            .await
            .unwrap();
        assert_eq!(to_judge(&core).await.len(), 1, "the text it read is gone");
        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();
        assert_eq!(decider.calls(), 2);
    }

    #[tokio::test]
    async fn a_link_judged_before_this_change_keeps_its_written_reason() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = bound(&core).await;
        core.store
            .set_link_state(
                &ids[0],
                &ids[1],
                LinkState::Related,
                Some("the tool and the error it prints"),
                Some((0, 0)),
            )
            .await
            .unwrap();

        let l = link_of(&core, &ids).await;
        assert_eq!(l.kind, None);
        for id in &ids {
            assert_eq!(
                line_from(&core, id).await.as_deref(),
                Some("the tool and the error it prints")
            );
        }
    }

    #[tokio::test]
    async fn the_searches_that_bound_them_are_in_the_state() {
        let mut core = test_core().await;
        on(&mut core).await;
        let decider = deciding(&mut core, says("alternative"));
        let ids = bound(&core).await;

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        let (state, questions) = &decider.requests()[0];
        assert!(
            state.contains("Found together by these searches: q"),
            "{state}"
        );
        // Both questions, each with its reversed twin, in the one request.
        assert_eq!(questions.len(), 4);
    }

    #[tokio::test]
    async fn links_are_armed_inside_the_judging_budget() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 2).await;
        for q in ["one", "two", "three", "four", "five"] {
            core.store
                .bump_link(&ids[0], &ids[1], 1.0, Some(q), 30.0, crate::store::now())
                .await
                .unwrap();
        }
        core.decide_cfg.max_per_tick = 0;

        let r = run(&core).await.unwrap();

        assert_eq!(r.armed, 0);
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
        deciding(&mut core, says("duplicate"));
        let ids = bound(&core).await;
        core.store
            .record_pair(&ids[0], &ids[1], 0.91)
            .await
            .unwrap();
        sqlx::query!("UPDATE artifact_pairs SET state = 'dismissed'")
            .execute(&core.store.pool)
            .await
            .unwrap();

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        let l = link_of(&core, &ids).await;
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
