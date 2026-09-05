//! Sleep: what a quiet base does with what the day brought.
//!
//! Four phases, in `tune::run_if_quiet`'s order: integrate (here), rehearse
//! (here), reorganise (the corpus rules), wake (the journal). This module
//! holds the two that read the base and write evidence; nothing in it spends
//! a generation.

use crate::core::Core;
use crate::error::Result;
use crate::eval::sweep::OBSERVATION_LIMIT;
use crate::store::integrations::{Integration, Tag};
use crate::store::rehearsals::{Class, NewRehearsal};
use std::collections::BTreeSet;

#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct Integrated {
    pub integrated: usize,
    pub novel: usize,
    pub known: usize,
    pub conflicts: usize,
    /// Capture probes written.
    pub probes: usize,
    /// Somebody came back; the rest waits for the next quiet period.
    pub stopped: bool,
}

/// The tag, from the best surviving hit. Pure, so the rule is one function.
///
/// Below `review_min` nothing near: novel. At or above it: known — unless the
/// two are at or above `auto_supersede`, where they claim to be the same
/// statement, and carry different values while claiming it. That is the
/// narrow case, on purpose: below `auto_supersede` a value difference is two
/// notes about two things. Both sides need values to disagree.
pub fn tag_for(
    review_min: f32,
    auto_supersede: f32,
    mine: &BTreeSet<String>,
    nearest: Option<(f32, &BTreeSet<String>)>,
) -> (Tag, Option<String>) {
    let Some((score, theirs)) = nearest else {
        return (Tag::Novel, None);
    };
    if score < review_min {
        return (Tag::Novel, None);
    }
    if score >= auto_supersede && !mine.is_empty() && !theirs.is_empty() && mine != theirs {
        let list = |set: BTreeSet<&String>| {
            set.iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let detail = format!(
            "same statement, different values: this one says {}; the other says {}",
            list(mine.difference(theirs).collect()),
            list(theirs.difference(mine).collect())
        );
        return (Tag::Conflict, Some(detail));
    }
    (Tag::Known, None)
}

/// Every new artifact, run once against the rest of the base.
///
/// Same-corpus hits are structure, not knowledge — the reason `relate` skips
/// passages — applied as a filter so passages are integrated too. A conflict
/// files a pair for a person; a known artifact files nothing here, because
/// `relate` already does for model-written ones and verbatim text waits for
/// promotion. Every near hit gets a probe: this text is a question somebody
/// asked that the near artifact answers.
pub async fn integrate(core: &Core, started: i64) -> Result<Integrated> {
    let mut out = Integrated::default();
    let review_min = core.ranking.read().expect("ranking lock").review_min;
    let auto = core.consolidate.auto_supersede;
    let model = core.embedder.model().to_string();
    for a in core.store.artifacts_to_integrate(OBSERVATION_LIMIT).await? {
        if core.store.activity_since(started).await? {
            out.stopped = true;
            return Ok(out);
        }
        let hits = core
            .vectors
            .neighbours(&a.id, core.consolidate.per_point)
            .await?;
        let mut surviving: Vec<(f32, crate::store::artifacts::Chunk)> = Vec::new();
        for h in hits {
            if h.payload.artifact_id == a.id
                || a.corpus_id.as_deref() == Some(h.payload.corpus_id.as_str())
            {
                continue;
            }
            let Some(sim) = h.similarity else {
                continue;
            };
            let Ok(other) = core.store.get_artifact(&h.payload.artifact_id).await else {
                continue;
            };
            if !other.in_results() {
                continue;
            }
            surviving.push((sim, other));
        }
        surviving.sort_by(|x, y| y.0.partial_cmp(&x.0).unwrap_or(std::cmp::Ordering::Equal));

        let mine = crate::infer::facts::fact_tokens(&a.text);
        let nearest = surviving.first();
        let theirs = nearest.map(|(_, o)| crate::infer::facts::fact_tokens(&o.text));
        let (tag, detail) = tag_for(
            review_min,
            auto,
            &mine,
            nearest.map(|(s, _)| *s).zip(theirs.as_ref()),
        );
        let written = core
            .store
            .record_integration(&Integration {
                artifact_id: a.id.clone(),
                at: crate::store::now(),
                tag,
                nearest_id: nearest.map(|(_, o)| o.id.clone()),
                nearest_score: nearest.map(|(s, _)| *s),
                detail: detail.clone(),
            })
            .await?;
        if !written {
            continue;
        }
        out.integrated += 1;
        match tag {
            Tag::Novel => out.novel += 1,
            Tag::Known => out.known += 1,
            Tag::Conflict => {
                out.conflicts += 1;
                let (score, other) = nearest.expect("a conflict has a nearest");
                let detail = detail.as_deref().unwrap_or("");
                if core
                    .store
                    .record_pair_with_detail(&a.id, &other.id, *score, detail)
                    .await?
                    && let Some(pair) = core.store.pair_between(&a.id, &other.id).await?
                {
                    core.store
                        .set_pair_state(
                            pair.id,
                            crate::store::pairs::PairState::Contradiction,
                            Some(detail),
                            crate::store::pairs::DecidedBy::Evidence,
                        )
                        .await?;
                }
            }
        }
        // The vector is the artifact's own, read back from the index so
        // nothing is embedded.
        let Some(vec) = core.vectors.dense_of(&a.id).await? else {
            continue;
        };
        for (sim, other) in &surviving {
            if *sim < review_min {
                break;
            }
            if core
                .store
                .record_rehearsal(&NewRehearsal {
                    class: Class::Capture,
                    query: a.text.clone(),
                    query_vec: vec.clone(),
                    embed_model: model.clone(),
                    artifact_id: other.id.clone(),
                    source_id: Some(a.id.clone()),
                })
                .await?
                .is_some()
            {
                out.probes += 1;
            }
        }
    }
    Ok(out)
}

pub const REHEARSED_AFTER: &str = "sleep.rehearsed_after";

#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct Replayed {
    pub rehearsed: usize,
    pub found: usize,
    /// Probes retired on the way: another embedder, or an owner gone.
    pub retired: usize,
    pub stopped: bool,
}

/// One probe as a `Pair`, `satisfies` widened to what supersedes the owner.
pub(crate) async fn pair_of(
    core: &Core,
    r: &crate::store::rehearsals::Rehearsal,
) -> crate::eval::sweep::Pair {
    crate::eval::sweep::Pair {
        query: r.query.clone(),
        satisfies: crate::eval::satisfied_by(core, &r.artifact_id).await,
        query_vec: Some(r.query_vec.clone()),
        priming: None,
        served: None,
    }
}

/// Whether anything in the chain a probe is satisfied by is still in results.
async fn owner_stands(core: &Core, satisfies: &[String]) -> bool {
    for id in satisfies {
        if let Ok(c) = core.store.get_artifact(id).await
            && c.in_results()
        {
            return true;
        }
    }
    false
}

/// Fragile first, then the lap. Pure vector reads; nothing embedded.
///
/// The lap walks `(created_at, id)` from where the last pass stopped and
/// wraps at the end, the way `retract`'s cursor does: a probe has no end
/// state to reach, so the cursor has no end. Half the bound goes to the
/// probes whose last two results disagree — what wobbles is what needs
/// rehearsing, and this is the spacing effect measured rather than scheduled.
pub async fn rehearse(
    core: &Core,
    live: &crate::store::generations::Generation,
    started: i64,
) -> Result<Replayed> {
    let mut out = Replayed::default();
    let current = *core.ranking.read().expect("ranking lock");
    let model = core.embedder.model().to_string();
    let half = OBSERVATION_LIMIT / 2;

    let mut batch = core.store.fragile_rehearsals(&live.id, half).await?;
    let after = core
        .store
        .meta_get(REHEARSED_AFTER)
        .await?
        .map(|s| crate::store::Cursor::parse(&s))
        .unwrap_or_default();
    let want = OBSERVATION_LIMIT - batch.len();
    let mut lap = core.store.rehearsals_after(&after, want).await?;
    if lap.is_empty() && after != crate::store::Cursor::default() {
        lap = core
            .store
            .rehearsals_after(&crate::store::Cursor::default(), want)
            .await?;
    }
    let lap_start = batch.len();
    batch.extend(lap);

    let mut cursor = after;
    for (i, r) in batch.iter().enumerate() {
        if core.store.activity_since(started).await? {
            core.store
                .meta_set(REHEARSED_AFTER, &cursor.encode())
                .await?;
            out.stopped = true;
            return Ok(out);
        }
        if i >= lap_start {
            cursor = crate::store::Cursor {
                at: r.created_at,
                id: r.id.clone(),
            };
        }
        // Another era's vector is not comparable with the live index, the
        // way rule 2 skips give-ups from another embedder; retired, not
        // replayed. An owner nothing answers for any more likewise.
        let pair = pair_of(core, r).await;
        if r.embed_model != model || !owner_stands(core, &pair.satisfies).await {
            core.store
                .retire_rehearsal(&r.id, crate::store::now())
                .await?;
            out.retired += 1;
            continue;
        }
        let (rank, above) = crate::eval::sweep::rank_and_above(core, &pair, current).await?;
        out.rehearsed += 1;
        if rank.is_some() {
            out.found += 1;
        }
        core.store
            .record_rehearsal_result(&crate::store::rehearsals::NewResult {
                rehearsal_id: r.id.clone(),
                generation_id: live.id.clone(),
                rank: rank.map(|r| r as i64 + 1),
                outranked_by: above,
            })
            .await?;
    }
    core.store
        .meta_set(REHEARSED_AFTER, &cursor.encode())
        .await?;
    Ok(out)
}

/// Ids that stood above the owner in **every** retained result — at least
/// two — and are from another corpus. One result is not a pattern; a
/// same-corpus neighbour is structure.
pub fn interferers<F>(
    results: &[crate::store::rehearsals::RehearsalResult],
    own_corpus: Option<&str>,
    corpus_of: F,
) -> Vec<String>
where
    F: Fn(&str) -> Option<String>,
{
    if results.len() < 2 {
        return vec![];
    }
    let mut out = Vec::new();
    for x in &results[0].outranked_by {
        if results.iter().all(|r| r.outranked_by.contains(x))
            && !out.contains(x)
            && (own_corpus.is_none() || corpus_of(x).as_deref() != own_corpus)
        {
            out.push(x.clone());
        }
    }
    out
}

/// Rule 3: forgetting by displacement. A probe owner outranked by the same
/// artifact in every retained result has been answered for by it; the cosine
/// at embed time did not call them duplicates, behaviour did. File the pair
/// for the judge; the chain after this is dedupe's, unchanged. Returns
/// (pairs filed, stopped early).
pub async fn interference(
    core: &Core,
    live: &crate::store::generations::Generation,
    started: i64,
) -> Result<(usize, bool)> {
    use crate::store::actions::Kind;
    let mut filed = 0;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (probe, _) in core
        .store
        .latest_results_under(&live.id, OBSERVATION_LIMIT)
        .await?
    {
        if core.store.activity_since(started).await? {
            return Ok((filed, true));
        }
        if !seen.insert(probe.artifact_id.clone()) {
            continue;
        }
        let Ok(owner) = core.store.get_artifact(&probe.artifact_id).await else {
            continue;
        };
        if !owner.in_results() {
            continue;
        }
        // Every retained result of every live probe of this owner, together.
        let mut results = Vec::new();
        for p in core.store.rehearsals_of(&owner.id).await? {
            results.extend(core.store.results_of(&p.id, OBSERVATION_LIMIT).await?);
        }
        let mut corpora: std::collections::HashMap<String, Option<String>> =
            std::collections::HashMap::new();
        for r in &results {
            for x in &r.outranked_by {
                if !corpora.contains_key(x) {
                    let c = core
                        .store
                        .get_artifact(x)
                        .await
                        .ok()
                        .and_then(|a| a.corpus_id);
                    corpora.insert(x.clone(), c);
                }
            }
        }
        let found = interferers(&results, owner.corpus_id.as_deref(), |id| {
            corpora.get(id).cloned().flatten()
        });
        for x in found {
            // Filing is not acting, but it is what leads to one: it stops at
            // the budget with the rest of the corpus half.
            if !core.may_act().await? {
                return Ok((filed, false));
            }
            // A pair the base once acted on and took back is a person's now.
            if core.store.action_was_undone(&owner.id, Kind::Merge).await?
                || core
                    .store
                    .action_was_undone(&owner.id, Kind::Supersede)
                    .await?
                || core.store.action_was_undone(&x, Kind::Merge).await?
                || core.store.action_was_undone(&x, Kind::Supersede).await?
            {
                continue;
            }
            if core.store.pair_between(&owner.id, &x).await?.is_some() {
                continue;
            }
            let detail = format!(
                "interference: {x} stood above this in every one of {} rehearsals",
                results.len()
            );
            let score = core
                .vectors
                .neighbours(&owner.id, core.consolidate.per_point)
                .await?
                .into_iter()
                .find(|h| h.payload.artifact_id == x)
                .and_then(|h| h.similarity)
                .unwrap_or(0.0);
            if core
                .store
                .record_pair_with_detail(&owner.id, &x, score, &detail)
                .await?
            {
                filed += 1;
            }
        }
    }
    Ok((filed, false))
}

/// A stable owner dragging one competitor, with use behind it: arm a
/// condensation. Three things, each already measured: model-written and in
/// results; found in every retained result of at least two, with one id
/// standing above it in every one — the measurable half of "the same
/// competitor trails or leads it", since what trails is not in the results
/// table; and engagement at or above `promote.activation_above`. Returns
/// (units armed, stopped early).
pub async fn condense_candidates(
    core: &Core,
    live: &crate::store::generations::Generation,
    started: i64,
) -> Result<(usize, bool)> {
    use crate::store::actions::Kind;
    let mut armed = 0;
    let mut seen = std::collections::HashSet::new();
    let at = crate::store::now();
    for (probe, _) in core
        .store
        .latest_results_under(&live.id, OBSERVATION_LIMIT)
        .await?
    {
        if core.store.activity_since(started).await? {
            return Ok((armed, true));
        }
        if !seen.insert(probe.artifact_id.clone()) {
            continue;
        }
        let Ok(owner) = core.store.get_artifact(&probe.artifact_id).await else {
            continue;
        };
        if owner.provenance == crate::store::artifacts::Provenance::Passage || !owner.in_results() {
            continue;
        }
        if core
            .store
            .open_action_on(&owner.id, Kind::Condense)
            .await?
            .is_some()
            || core
                .store
                .action_was_undone(&owner.id, Kind::Condense)
                .await?
        {
            continue;
        }
        let mut results = Vec::new();
        for p in core.store.rehearsals_of(&owner.id).await? {
            results.extend(core.store.results_of(&p.id, OBSERVATION_LIMIT).await?);
        }
        if results.len() < 2 || results.iter().any(|r| r.rank.is_none()) {
            continue;
        }
        let same_company = results[0]
            .outranked_by
            .iter()
            .any(|x| results.iter().all(|r| r.outranked_by.contains(x)));
        if !same_company {
            continue;
        }
        let activation = core
            .store
            .activation_of(std::slice::from_ref(&owner.id))
            .await?;
        let Some((value, stamp, created_at)) = activation.get(&owner.id) else {
            continue;
        };
        let earned = crate::store::links::engagement_at(
            *value,
            *stamp,
            *created_at,
            at,
            core.activation.half_life_days,
        );
        if earned < core.promote.activation_above {
            continue;
        }
        if !core.may_act().await? {
            return Ok((armed, false));
        }
        crate::jobs::condense::arm(core, &owner.id).await?;
        armed += 1;
    }
    Ok((armed, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_tag_is_novel_below_review_known_above_and_conflict_only_at_the_same_statement_with_different_values()
     {
        let mine = toks(&["8080", "1.21.4"]);
        assert_eq!(tag_for(0.80, 0.94, &mine, None).0, Tag::Novel);
        assert_eq!(
            tag_for(0.80, 0.94, &mine, Some((0.79, &toks(&["9090"])))).0,
            Tag::Novel
        );
        assert_eq!(
            tag_for(0.80, 0.94, &mine, Some((0.85, &toks(&["9090"])))).0,
            Tag::Known,
            "below auto_supersede a different value is two notes about two things"
        );
        assert_eq!(
            tag_for(0.80, 0.94, &mine, Some((0.95, &mine))).0,
            Tag::Known
        );
        assert_eq!(
            tag_for(0.80, 0.94, &mine, Some((0.95, &toks(&[])))).0,
            Tag::Known,
            "a side with no values cannot disagree"
        );
        let (tag, detail) = tag_for(0.80, 0.94, &mine, Some((0.95, &toks(&["9090", "1.21.4"]))));
        assert_eq!(tag, Tag::Conflict);
        let d = detail.unwrap();
        assert!(
            d.contains("8080") && d.contains("9090") && !d.contains("1.21.4"),
            "{d}"
        );
    }

    pub(crate) async fn corpus_of(
        core: &crate::core::Core,
        raw: &str,
        texts: &[&str],
    ) -> Vec<String> {
        let src = core.store.insert_corpus(raw, "web", None).await.unwrap();
        let new: Vec<_> = texts
            .iter()
            .enumerate()
            .map(|(i, t)| crate::store::artifacts::NewArtifact {
                ordinal: i as i64,
                text: t.to_string(),
                corpus_span: None,
                title: None,
                category: None,
                tags: vec![],
                segment_idx: None,
                caveats: vec![],
            })
            .collect();
        let mut ids = Vec::new();
        for c in core.store.insert_artifacts(&src.id, &new).await.unwrap() {
            crate::jobs::embed::run(core, &c.id).await.unwrap();
            ids.push(c.id);
        }
        ids
    }

    /// Two corpora, the first integrated before the second exists — a day
    /// apart, as it were. The second's artifact says what the first's says,
    /// so it lands on it; a same-corpus twin is discarded as structure.
    pub(crate) async fn two_corpora() -> (crate::core::Core, String, String, String) {
        let core = crate::core::test_support::test_core().await;
        let first = corpus_of(
            &core,
            "first",
            &["the image will not mount", "the image will not mount"],
        )
        .await;
        integrate(&core, crate::store::now()).await.unwrap();
        let second = corpus_of(&core, "second", &["the image will not mount"]).await;
        (core, first[0].clone(), first[1].clone(), second[0].clone())
    }

    #[tokio::test]
    async fn integration_tags_each_new_artifact_once_and_writes_a_capture_probe_for_what_it_landed_on()
     {
        let (core, a1, a2, b) = two_corpora().await;
        let r = integrate(&core, crate::store::now()).await.unwrap();
        assert_eq!(r.integrated, 1, "the first corpus was filed the day before");
        assert!(!r.stopped);
        // The first corpus's two saw only each other, discarded as structure:
        // novel, and they stay novel. The second lands on both of the first.
        let tag = |id: &str| {
            let core = &core;
            let id = id.to_string();
            async move { core.store.integration_of(&id).await.unwrap().unwrap().tag }
        };
        assert_eq!(tag(&a1).await, Tag::Novel);
        assert_eq!(tag(&a2).await, Tag::Novel);
        assert_eq!(tag(&b).await, Tag::Known);
        // b's text is a probe for a1 and for a2; nothing probes b.
        assert_eq!(core.store.rehearsals_of(&a1).await.unwrap().len(), 1);
        assert_eq!(core.store.rehearsals_of(&a2).await.unwrap().len(), 1);
        assert!(core.store.rehearsals_of(&b).await.unwrap().is_empty());
        let p = &core.store.rehearsals_of(&a1).await.unwrap()[0];
        assert_eq!(p.source_id.as_deref(), Some(b.as_str()));
        assert_eq!(p.query, "the image will not mount");
        // A second pass finds nothing to do.
        assert_eq!(
            integrate(&core, crate::store::now())
                .await
                .unwrap()
                .integrated,
            0
        );
    }

    #[tokio::test]
    async fn a_conflict_files_a_contradiction_pair_for_a_person() {
        // The fake embedder hashes text; two texts that differ only in a value
        // do not land at 0.94, and may land below zero. Set the thresholds so
        // that any neighbour is "the same statement" and let the tokens decide.
        let mut core = crate::core::test_support::test_core().await;
        core.consolidate.auto_supersede = -1.0;
        core.ranking.write().unwrap().review_min = -1.0;
        let mut ids = Vec::new();
        // Versions, not bare ports: `fact_tokens` refuses a bare run of
        // digits on purpose (see `infer::facts::is_fact`).
        for (raw, text) in [
            ("first", "requires 1.21.4 or later"),
            ("second", "requires 1.22.0 or later"),
        ] {
            let src = core.store.insert_corpus(raw, "web", None).await.unwrap();
            let c = core
                .store
                .insert_artifacts(
                    &src.id,
                    &[crate::store::artifacts::NewArtifact {
                        ordinal: 0,
                        text: text.into(),
                        corpus_span: None,
                        title: None,
                        category: None,
                        tags: vec![],
                        segment_idx: None,
                        caveats: vec![],
                    }],
                )
                .await
                .unwrap()
                .remove(0);
            crate::jobs::embed::run(&core, &c.id).await.unwrap();
            ids.push(c.id);
        }
        let r = integrate(&core, crate::store::now()).await.unwrap();
        // Both arrived in one sleep, so each is tagged against the other;
        // the pair between them is filed once.
        assert_eq!(r.conflicts, 2, "{r:?}");
        let pair = core
            .store
            .pair_between(&ids[0], &ids[1])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pair.state, crate::store::pairs::PairState::Contradiction);
        assert!(pair.detail.unwrap_or_default().contains("1.22.0"));
    }

    pub(crate) async fn live_generation(
        core: &crate::core::Core,
    ) -> crate::store::generations::Generation {
        let params = *core.ranking.read().unwrap();
        let id = core
            .store
            .record_generation(&crate::store::generations::NewGeneration {
                params: params.into(),
                embed_recipe: "fake".into(),
                chat_model: "fake".into(),
                parent_id: None,
            })
            .await
            .unwrap();
        core.store.generation(&id).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn rehearsal_replays_each_probe_under_the_live_generation_and_the_lap_wraps() {
        let (core, a1, _a2, _b) = two_corpora().await;
        let live = live_generation(&core).await;
        integrate(&core, crate::store::now()).await.unwrap();
        let r = rehearse(&core, &live, crate::store::now()).await.unwrap();
        assert_eq!(r.rehearsed, 2, "{r:?}");
        assert_eq!(r.found, 2, "b's text finds both of the first corpus");
        let p = &core.store.rehearsals_of(&a1).await.unwrap()[0];
        let res = core.store.results_of(&p.id, 10).await.unwrap();
        assert_eq!(res.len(), 1);
        assert!(res[0].rank.is_some());
        // The lap wraps: a second pass replays the same two again.
        assert_eq!(
            rehearse(&core, &live, crate::store::now())
                .await
                .unwrap()
                .rehearsed,
            2
        );
    }

    #[test]
    fn an_interferer_stands_above_in_every_result_at_least_twice_and_from_another_corpus() {
        let res = |above: &[&str]| crate::store::rehearsals::RehearsalResult {
            id: String::new(),
            rehearsal_id: String::new(),
            generation_id: String::new(),
            at: 0,
            rank: Some(2),
            outranked_by: above.iter().map(|s| s.to_string()).collect(),
        };
        let corpus = |id: &str| {
            Some(if id == "twin" {
                "mine".to_string()
            } else {
                "theirs".to_string()
            })
        };
        assert!(
            interferers(&[res(&["x"])], Some("mine"), corpus).is_empty(),
            "one result is not a pattern"
        );
        assert_eq!(
            interferers(&[res(&["x", "y"]), res(&["x"])], Some("mine"), corpus),
            vec!["x".to_string()]
        );
        assert!(interferers(&[res(&["x"]), res(&["y"])], Some("mine"), corpus).is_empty());
        assert!(
            interferers(&[res(&["twin"]), res(&["twin"])], Some("mine"), corpus).is_empty(),
            "same corpus is structure"
        );
    }

    #[tokio::test]
    async fn interference_files_one_pending_pair_and_never_the_same_pair_twice() {
        let (mut core, a1, _a2, b) = two_corpora().await;
        core.evolve.autonomous = crate::config::Autonomy::Full;
        let live = live_generation(&core).await;
        // A probe for a1 that b — another corpus — has outranked twice.
        // Hand-written results keep the fake embedder's ordering out of it.
        let pid = core
            .store
            .record_rehearsal(&NewRehearsal {
                class: Class::Cue,
                query: "q".into(),
                query_vec: vec![0.0; crate::core::test_support::TEST_DIM],
                embed_model: core.embedder.model().to_string(),
                artifact_id: a1.clone(),
                source_id: None,
            })
            .await
            .unwrap()
            .unwrap();
        for _ in 0..2 {
            core.store
                .record_rehearsal_result(&crate::store::rehearsals::NewResult {
                    rehearsal_id: pid.clone(),
                    generation_id: live.id.clone(),
                    rank: Some(2),
                    outranked_by: vec![b.clone()],
                })
                .await
                .unwrap();
        }
        let (filed, _) = interference(&core, &live, crate::store::now())
            .await
            .unwrap();
        assert_eq!(filed, 1);
        let pair = core.store.pair_between(&a1, &b).await.unwrap().unwrap();
        assert_eq!(pair.state, crate::store::pairs::PairState::Pending);
        assert!(pair.detail.unwrap_or_default().contains("interference"));
        assert_eq!(
            interference(&core, &live, crate::store::now())
                .await
                .unwrap()
                .0,
            0
        );
    }

    #[tokio::test]
    async fn interference_stops_filing_when_the_week_is_spent() {
        let (mut core, a1, _a2, b) = two_corpora().await;
        core.evolve.autonomous = crate::config::Autonomy::Full;
        core.evolve.max_actions_per_week = 0;
        let live = live_generation(&core).await;
        let pid = core
            .store
            .record_rehearsal(&NewRehearsal {
                class: Class::Cue,
                query: "q".into(),
                query_vec: vec![0.0; crate::core::test_support::TEST_DIM],
                embed_model: core.embedder.model().to_string(),
                artifact_id: a1.clone(),
                source_id: None,
            })
            .await
            .unwrap()
            .unwrap();
        for _ in 0..2 {
            core.store
                .record_rehearsal_result(&crate::store::rehearsals::NewResult {
                    rehearsal_id: pid.clone(),
                    generation_id: live.id.clone(),
                    rank: Some(2),
                    outranked_by: vec![b.clone()],
                })
                .await
                .unwrap();
        }
        assert_eq!(
            interference(&core, &live, crate::store::now())
                .await
                .unwrap()
                .0,
            0
        );
        assert!(core.store.pair_between(&a1, &b).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_probe_under_another_embedder_is_retired_not_replayed() {
        let (core, a1, _, _) = two_corpora().await;
        let live = live_generation(&core).await;
        core.store
            .record_rehearsal(&NewRehearsal {
                class: Class::Cue,
                query: "from another era".into(),
                query_vec: vec![0.0; crate::core::test_support::TEST_DIM],
                embed_model: "older-model".into(),
                artifact_id: a1.clone(),
                source_id: None,
            })
            .await
            .unwrap();
        let r = rehearse(&core, &live, crate::store::now()).await.unwrap();
        assert_eq!(r.retired, 1);
        assert_eq!(r.rehearsed, 0);
        assert_eq!(core.store.live_rehearsal_count().await.unwrap(), 0);
    }
}
