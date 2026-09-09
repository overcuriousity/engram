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
/// Below `review_min` nothing near: novel. At or above it: known. There is no
/// third answer here. There used to be — two artifacts at or above
/// `auto_supersede` whose value-shaped tokens differed were tagged a conflict
/// and filed as a contradiction, without a model ever reading either one. In a
/// base of lecture slides the tokens that differed were the footer dates and
/// the section numbers, so 130 pairs of near-identical passages settled into a
/// terminal state no sweep reopens and no person could clear. Whether two
/// artifacts disagree is a question about what they say, and nothing that
/// splits on whitespace can be asked it.
pub fn tag_for(review_min: f32, nearest: Option<f32>) -> Tag {
    match nearest {
        Some(score) if score >= review_min => Tag::Known,
        _ => Tag::Novel,
    }
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

        let nearest = surviving.first();
        let tag = tag_for(review_min, nearest.map(|(s, _)| *s));
        let written = core
            .store
            .record_integration(&Integration {
                artifact_id: a.id.clone(),
                at: crate::store::now(),
                tag,
                nearest_id: nearest.map(|(_, o)| o.id.clone()),
                nearest_score: nearest.map(|(s, _)| *s),
                detail: None,
            })
            .await?;
        if !written {
            continue;
        }
        out.integrated += 1;
        match tag {
            Tag::Novel => out.novel += 1,
            Tag::Known => out.known += 1,
            // Nothing here writes this any more, and `Integrated::conflicts`
            // stays at zero for the same reason. The variant and the column
            // survive so the integrations and sleep runs recorded before the
            // evidence heuristic was retired still read back as what they were.
            Tag::Conflict => {}
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
    /// Of those, the ones asked again at the live embedder. See `remint`.
    pub reminted: usize,
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
        served_rank: None,
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
    // The two halves overlap, and the overlap has to go. A fragile probe is a
    // live probe like any other, so the lap walks straight over the ones
    // already in hand — and on a base whose probes all fit in one lap, that is
    // every one of them. `rehearsal_results` has no uniqueness, so a probe
    // measured twice in one pass writes two rows off a single reading, and
    // every test spelled "at least two retained results" then passes on one
    // observation agreeing with itself: interference files a pair, condense
    // arms a rewrite, and neither has the second measurement it says it has.
    let held: std::collections::HashSet<String> = batch.iter().map(|r| r.id.clone()).collect();
    let lap: Vec<_> = lap.into_iter().filter(|r| !held.contains(&r.id)).collect();
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
        let stale = r.embed_model != model;
        let stands = owner_stands(core, &pair.satisfies).await;
        if stale || !stands {
            // The two retirements are not the same event. An owner that left
            // results is a question with nothing left to answer it, and it
            // stays retired. A changed embedder is not: the question is as
            // good as it ever was, only the vector beside it is from another
            // era. Nothing else would ever ask it again — `integrate` files
            // an artifact once and never revisits it, and a cue is minted at
            // `embed::mark_indexed` — so a base that changed embedder lost
            // every probe it had, and with them the anchor that refuses a
            // candidate and reverts a generation.
            //
            // Which is why the stale probe is retired only once its
            // replacement exists, or once it is certain none ever will.
            // `retired_at` is a one-way door and every reader filters on it,
            // and a capture probe's replacement is minted from the artifact
            // its query *is* — an artifact that, on the pass right after the
            // embedder changed, has very often not been re-embedded yet.
            // Retired first and re-minted after, that probe was thrown away
            // for the reason it was being kept: the sweep ran while the
            // re-embed was still queued. `Waiting` leaves it live for the
            // pass that finds the artifact ready.
            if stale && stands {
                // The vector first, and the retirement only after it is in
                // hand. `idx_rehearsals_live` is unique over
                // `(artifact_id, class, query)` where `retired_at IS NULL`, so
                // the replacement cannot be written beside the old row — the
                // retirement has to come first, and that is exactly what made
                // the loss possible. Deciding here and writing below keeps
                // both: nothing is retired that has no replacement coming, and
                // nothing is inserted against a row still holding the index.
                match remintable(core, r, &model).await? {
                    // Left live for the pass that finds the artifact embedded.
                    Remintable::Waiting => continue,
                    Remintable::Gone => {}
                    Remintable::From(query_vec) => {
                        core.store
                            .retire_rehearsal(&r.id, crate::store::now())
                            .await?;
                        out.retired += 1;
                        out.reminted += usize::from(remint(core, r, &model, query_vec).await?);
                        continue;
                    }
                }
            }
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

/// The same question again, at the live embedder.
///
/// Both classes are re-embedded the way they were minted, which is what keeps
/// a re-minted probe comparable with the ones around it. A cue is a question
/// and cost a query embedding; a capture probe's query is another artifact's
/// text and cost nothing at all — its vector is that artifact's own, read back
/// out of the index — so it is read back the same way, and only once the
/// source itself carries the live model. Reading it sooner would write the
/// very vector this probe is being retired for.
///
/// `false` where the source is gone, not yet re-embedded, or already has a
/// live probe for this question. None of those is a failure: the next lap
/// comes round.
/// The query vector a re-mint would carry, or why there is none.
///
/// Asked before the old probe is retired, which is the whole point of it
/// being a separate step. `retired_at` is a one-way door and every reader
/// filters on it, so a probe retired ahead of a replacement that never came
/// is a probe the base has lost — and a capture probe's replacement is minted
/// from the artifact its query *is*, an artifact that, on the pass right
/// after the embedder changed, has very often not been re-embedded yet. That
/// pass used to retire every probe it had and re-mint almost none of them.
async fn remintable(
    core: &Core,
    r: &crate::store::rehearsals::Rehearsal,
    model: &str,
) -> Result<Remintable> {
    Ok(match r.class {
        Class::Cue => {
            let permit = core.gate.background_light().await;
            let v = core.embedder.embed_query(&r.query).await;
            permit.finished();
            Remintable::From(v?)
        }
        Class::Capture => {
            let Some(source) = r.source_id.as_deref() else {
                return Ok(Remintable::Gone);
            };
            let Ok(a) = core.store.get_artifact(source).await else {
                return Ok(Remintable::Gone);
            };
            // Not yet, rather than never: the embed queue is what makes both
            // of these true, and it drains.
            if a.embed_model.as_deref() != Some(model) {
                return Ok(Remintable::Waiting);
            }
            match core.vectors.dense_of(source).await? {
                Some(v) => Remintable::From(v),
                None => Remintable::Waiting,
            }
        }
    })
}

/// Three answers and not two, because "no replacement" splits: a source that
/// has not been re-embedded *yet* is a wait the embed queue ends, and a source
/// that is gone is not. Only the second may retire the probe.
enum Remintable {
    /// The query vector for the replacement.
    From(Vec<f32>),
    /// Nothing to mint from yet. The old probe stays live.
    Waiting,
    /// Nothing to mint from, ever.
    Gone,
}

/// Write the replacement, once the old row is out of the live index.
///
/// `false` where `INSERT OR IGNORE` declined — this question already stands on
/// this artifact at the live embedder, which is a replacement either way.
async fn remint(
    core: &Core,
    r: &crate::store::rehearsals::Rehearsal,
    model: &str,
    query_vec: Vec<f32>,
) -> Result<bool> {
    Ok(core
        .store
        .record_rehearsal(&NewRehearsal {
            class: r.class,
            query: r.query.clone(),
            query_vec,
            embed_model: model.to_string(),
            artifact_id: r.artifact_id.clone(),
            source_id: r.source_id.clone(),
        })
        .await?
        .is_some())
}

/// Ids that stood above the owner in **every** retained result — at least
/// two — and are from another corpus. One result is not a pattern; a
/// same-corpus neighbour is structure.
///
/// Nothing at all where any retained result missed the owner, which is the
/// same guard `condense_candidates` carries and for a sharper reason here.
/// `rank_and_above` returns the *entire* top ten as `outranked_by` when the
/// owner is not in the results, so a probe whose owner is simply not
/// retrievable — one condensed and awaiting a re-embed, which still passes
/// `in_results()` — contributed a full slate of ids to the "stood above it in
/// every one" test and made interference out of consistency. That mattered
/// more when the rule filed pairs and dedupe could merge or supersede away the
/// very artifact that was only briefly unfindable; it files nothing now, so
/// the cost of the same mistake is a wrong number on Ops rather than a lost
/// artifact. The guard stays: a wrong number is still wrong.
pub fn interferers<F>(
    results: &[crate::store::rehearsals::RehearsalResult],
    own_corpus: Option<&str>,
    corpus_of: F,
) -> Vec<String>
where
    F: Fn(&str) -> Option<String>,
{
    if results.len() < 2 || results.iter().any(|r| r.rank.is_none()) {
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

/// Rule 3: forgetting by displacement. Which artifacts stand above a probe
/// owner in every one of its retained results — retrieval competition,
/// counted for Ops. Returns (interferers observed, stopped early).
///
/// It files nothing, and that is the point. It used to record an
/// `artifact_pairs` row per interferer, which put a ranking observation onto a
/// queue whose cards make claims about meaning: `_decide.html` renders a pair
/// as "these two disagree", and two documents sharing a template outrank each
/// other constantly while agreeing about everything. A base's whole queue was
/// this rule's output — cover pages against cover pages, one town's shop
/// listings against another's — and not one of them was a disagreement.
///
/// It also carried neither passage guard its sibling producers have
/// (`relate::arm`, `associate::judge` both refuse a passage), so it was the
/// only reason passage pairs reached consolidation at all — where `dedupe`
/// cannot merge stored source text and settles them `Contradiction`, which is
/// the state that renders as the claim. And it invented a `0.0` score out of a
/// missed neighbour lookup, colliding with the marker `web::ops` reads as
/// "found by co-retrieval, never measured".
///
/// The budget does not gate it any more either. That check was here because
/// filing is what leads to a merge; with nothing written there is nothing to
/// withhold, and a count that fell to zero when the week ran out would report
/// that the competition had stopped rather than that the spending had.
pub async fn interference(
    core: &Core,
    live: &crate::store::generations::Generation,
    started: i64,
) -> Result<(usize, bool)> {
    use crate::store::actions::Kind;
    let mut observed = 0;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (probe, _) in core
        .store
        .latest_results_under(&live.id, OBSERVATION_LIMIT)
        .await?
    {
        if core.store.activity_since(started).await? {
            return Ok((observed, true));
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
        // Which of these ids the base still has. `outranked_by` is free-form
        // JSON written when the rehearsal ran, and an id in it can name an
        // artifact a burial or a merge has since taken away — a missing row
        // and a row with no corpus are two different answers, and reading
        // both as `None` conflated them.
        let mut gone: std::collections::HashSet<String> = std::collections::HashSet::new();
        for r in &results {
            for x in &r.outranked_by {
                if !corpora.contains_key(x) {
                    match core.store.get_artifact(x).await {
                        Ok(a) => {
                            corpora.insert(x.clone(), a.corpus_id);
                        }
                        Err(_) => {
                            gone.insert(x.clone());
                            corpora.insert(x.clone(), None);
                        }
                    }
                }
            }
        }
        let found = interferers(&results, owner.corpus_id.as_deref(), |id| {
            corpora.get(id).cloned().flatten()
        });
        for x in found {
            // An id the base no longer has is not an observation about
            // anything still in results. `outranked_by` is free-form JSON
            // written when the rehearsal ran, and a burial or a merge can take
            // the artifact it names away afterwards.
            if gone.contains(&x) {
                tracing::debug!(owner = %owner.id, outranker = %x, "an outranker the base no longer has");
                continue;
            }
            // A pair the base once acted on and took back is a person's now,
            // and their decision is not re-observed at them.
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
            observed += 1;
        }
    }
    Ok((observed, false))
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

    #[test]
    fn the_tag_is_novel_below_review_and_known_at_or_above_it() {
        assert_eq!(tag_for(0.80, None), Tag::Novel);
        assert_eq!(tag_for(0.80, Some(0.79)), Tag::Novel);
        assert_eq!(tag_for(0.80, Some(0.80)), Tag::Known);
        assert_eq!(tag_for(0.80, Some(0.99)), Tag::Known);
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
                ..Default::default()
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
    async fn two_artifacts_differing_only_in_a_value_are_known_and_file_nothing() {
        // The fake embedder hashes text; two texts that differ only in a value
        // do not land at 0.94, and may land below zero. Set the threshold so
        // that any neighbour counts as the same statement.
        let mut core = crate::core::test_support::test_core().await;
        core.consolidate.auto_supersede = -1.0;
        core.ranking.write().unwrap().review_min = -1.0;
        let mut ids = Vec::new();
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
                        text: text.into(),
                        ..Default::default()
                    }],
                )
                .await
                .unwrap()
                .remove(0);
            crate::jobs::embed::run(&core, &c.id).await.unwrap();
            ids.push(c.id);
        }
        let r = integrate(&core, crate::store::now()).await.unwrap();
        // Two artifacts stating one thing with different values is the cleanest
        // thing there is to merge, and which of them is current is the judge's
        // question. Reading the answer off the digits in the text is what filed
        // a lecture archive's slide dates as contradictions nobody could clear.
        assert_eq!(r.conflicts, 0, "{r:?}");
        assert_eq!(r.known, 2, "{r:?}");
        assert!(
            core.store
                .pair_between(&ids[0], &ids[1])
                .await
                .unwrap()
                .is_none(),
            "no pair is filed without a model having looked at one"
        );
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
                ..Default::default()
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

        // A result that missed the owner carries the whole top ten as
        // `outranked_by`, so it agrees with anything. One is enough to make
        // the set say nothing.
        let missed = |above: &[&str]| crate::store::rehearsals::RehearsalResult {
            rank: None,
            ..res(above)
        };
        assert!(
            interferers(&[res(&["x"]), missed(&["x", "y"])], Some("mine"), corpus).is_empty(),
            "a replay that did not find the owner is not evidence about what displaced it"
        );
    }

    /// A probe the fragile half already took is not walked over again by the
    /// lap.
    ///
    /// A fragile probe is a live probe like any other, so `rehearsals_after`
    /// returns it too — and on any base whose probes fit in one lap, that is
    /// every one of them. `rehearsal_results` has no uniqueness, so measured
    /// twice in one pass a probe wrote two rows off a single reading, and
    /// "at least two retained results" then read as agreement between two
    /// observations where there was only ever one. Interference files a pair
    /// on that and condense arms a rewrite.
    #[tokio::test]
    async fn a_probe_the_fragile_half_took_is_not_measured_a_second_time_by_the_lap() {
        let (core, a1, _a2, _b) = two_corpora().await;
        let live = live_generation(&core).await;
        integrate(&core, crate::store::now()).await.unwrap();
        let pid = core.store.rehearsals_of(&a1).await.unwrap()[0].id.clone();
        // Two results that disagree is what `fragile_rehearsals` selects on.
        for rank in [Some(1), Some(3)] {
            core.store
                .record_rehearsal_result(&crate::store::rehearsals::NewResult {
                    rehearsal_id: pid.clone(),
                    generation_id: live.id.clone(),
                    rank,
                    outranked_by: vec![],
                })
                .await
                .unwrap();
        }
        assert_eq!(
            core.store
                .fragile_rehearsals(&live.id, 10)
                .await
                .unwrap()
                .len(),
            1,
            "the probe is in the fragile half"
        );
        let before = core.store.results_of(&pid, 100).await.unwrap().len();
        rehearse(&core, &live, crate::store::now()).await.unwrap();
        let after = core.store.results_of(&pid, 100).await.unwrap().len();
        assert_eq!(after - before, 1, "one pass is one measurement");
    }

    /// A capture probe whose artifact has not been re-embedded yet is left
    /// alone, not retired.
    ///
    /// The pass right after an embedder changes is exactly when the re-embeds
    /// are still queued, and `remint` cannot mint a capture probe until the
    /// artifact its query *is* carries the live model. Retired first and
    /// re-minted after, that probe was thrown away for the reason it was being
    /// kept — every reader filters `retired_at IS NULL`, and nothing else ever
    /// mints one again.
    #[tokio::test]
    async fn a_capture_probe_waits_for_its_artifact_to_be_re_embedded_rather_than_being_lost() {
        let (core, a1, _a2, b) = two_corpora().await;
        let live = live_generation(&core).await;
        // The probe and its source are both from the old era, which is the
        // state a changed embedder leaves behind.
        let source = core.store.get_artifact(&b).await.unwrap();
        core.store
            .mark_embedded(&b, "older-model", source.embed_rev)
            .await
            .unwrap();
        let pid = core
            .store
            .record_rehearsal(&NewRehearsal {
                class: Class::Capture,
                query: "the image will not mount".into(),
                query_vec: vec![0.0; crate::core::test_support::TEST_DIM],
                embed_model: "older-model".into(),
                artifact_id: a1.clone(),
                source_id: Some(b.clone()),
            })
            .await
            .unwrap()
            .unwrap();

        let r = rehearse(&core, &live, crate::store::now()).await.unwrap();
        assert_eq!(
            r.retired, 0,
            "nothing is thrown away while a re-embed is owed"
        );
        assert_eq!(r.reminted, 0);
        assert_eq!(r.rehearsed, 0, "and the old vector is still not replayed");
        assert!(
            core.store
                .rehearsal(&pid)
                .await
                .unwrap()
                .is_some_and(|p| p.retired_at.is_none()),
            "the probe is still live"
        );

        // The embed queue drains, and the next pass mints the replacement.
        core.store
            .mark_embedded(&b, core.embedder.model(), source.embed_rev)
            .await
            .unwrap();
        let r = rehearse(&core, &live, crate::store::now()).await.unwrap();
        assert_eq!(r.retired, 1);
        assert_eq!(r.reminted, 1);
        let live_probes = core.store.rehearsals_of(&a1).await.unwrap();
        assert_eq!(live_probes.len(), 1, "the same question, once");
        assert_eq!(live_probes[0].embed_model, core.embedder.model());
    }

    /// An outranker the base no longer has costs one skipped id, not the
    /// sweep.
    ///
    /// `outranked_by` is free-form JSON written when the rehearsal ran, and a
    /// burial or a merge can take the artifact it names away afterwards.
    /// `artifact_pairs` carries foreign keys on both members and
    /// `INSERT OR IGNORE` does not suppress a foreign-key violation, so filing
    /// against a since-deleted id failed the whole retention sweep — every
    /// rule after this one included.
    #[tokio::test]
    async fn an_outranker_the_base_no_longer_has_is_skipped_rather_than_failing_the_sweep() {
        let (mut core, a1, _a2, b) = two_corpora().await;
        core.evolve.autonomous = crate::config::Autonomy::Full;
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
        // One id the base has, one it never had.
        for _ in 0..2 {
            core.store
                .record_rehearsal_result(&crate::store::rehearsals::NewResult {
                    rehearsal_id: pid.clone(),
                    generation_id: live.id.clone(),
                    rank: Some(3),
                    outranked_by: vec!["a-buried-artifact".into(), b.clone()],
                })
                .await
                .unwrap();
        }
        let (seen, _) = interference(&core, &live, crate::store::now())
            .await
            .expect("one missing id does not fail the sweep");
        assert_eq!(
            seen, 1,
            "the outranker that is still there is still counted"
        );
        assert!(core.store.pair_between(&a1, &b).await.unwrap().is_none());
    }

    /// Retrieval competition is a fact about ranking, not about meaning. It is
    /// counted for Ops and it files nothing: the decide card renders a pair as
    /// "these two disagree", and two documents sharing a template outrank each
    /// other constantly while agreeing about everything.
    #[tokio::test]
    async fn interference_counts_what_it_sees_and_files_no_pair() {
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
        let (seen, _) = interference(&core, &live, crate::store::now())
            .await
            .unwrap();
        assert_eq!(seen, 1, "the interferer is still counted");
        assert!(
            core.store.pair_between(&a1, &b).await.unwrap().is_none(),
            "retrieval competition is not a dedupe finding and files no pair"
        );
        // Observing is idempotent in a way filing never was: the same standing
        // is still the same standing on the next pass, and reporting it twice
        // is the truth twice rather than a duplicate row.
        assert_eq!(
            interference(&core, &live, crate::store::now())
                .await
                .unwrap()
                .0,
            1
        );
    }

    /// `web::ops` reads an exact zero score as "found by co-retrieval, never
    /// measured". Interference manufactured one out of a missed neighbour
    /// lookup — `unwrap_or(0.0)` over a `find` that returned nothing — which
    /// put two meanings on one value. It writes no score at all now.
    #[tokio::test]
    async fn interference_writes_no_pair_carrying_a_zero_score() {
        let (mut core, a1, _a2, b) = two_corpora().await;
        core.evolve.autonomous = crate::config::Autonomy::Full;
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
        interference(&core, &live, crate::store::now())
            .await
            .unwrap();
        for p in core
            .store
            .pairs_by_state(crate::store::pairs::PairState::Pending, 100)
            .await
            .unwrap()
        {
            assert!(
                p.score != 0.0 || p.detail.as_deref() == Some("link"),
                "an exact zero means the link judge filed it, and nothing else"
            );
        }
    }

    /// The week's budget is about acting, and observing is not acting. It used
    /// to gate this rule because the rule filed pairs, and filing is what leads
    /// to a merge; with nothing written there is nothing to withhold, and a
    /// count that went to zero when the budget ran out would have Ops report
    /// that the competition stopped rather than that the spending did.
    #[tokio::test]
    async fn interference_keeps_counting_when_the_week_is_spent() {
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
            1,
            "the standing is reported whatever is left of the budget"
        );
        assert!(core.store.pair_between(&a1, &b).await.unwrap().is_none());
    }

    /// Not replayed — the vector is from another era and is not comparable
    /// with the live index — and not lost either. The question is as good as
    /// it ever was, so it is asked again at the live embedder. Nothing else
    /// would: `integrate` files an artifact once and never revisits it, and a
    /// cue is minted at `embed::mark_indexed`, so a base that changed
    /// embedder used to lose every probe it had and with them the anchor that
    /// refuses a candidate and reverts a generation.
    #[tokio::test]
    async fn a_probe_under_another_embedder_is_retired_and_asked_again() {
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
        assert_eq!(r.reminted, 1);
        assert_eq!(r.rehearsed, 0, "the old row is not replayed");

        let live_probes = core.store.rehearsals_of(&a1).await.unwrap();
        assert_eq!(live_probes.len(), 1, "the same question, once");
        assert_eq!(live_probes[0].query, "from another era");
        assert_eq!(live_probes[0].embed_model, core.embedder.model());
        // And the next lap replays it like any other.
        let r = rehearse(&core, &live, crate::store::now()).await.unwrap();
        assert_eq!(r.rehearsed, 1);
        assert_eq!(r.retired, 0);
    }

    /// The other retirement, and it is not the same event: an owner that left
    /// results is a question with nothing left to answer it, and it stays
    /// retired.
    #[tokio::test]
    async fn a_probe_whose_owner_left_results_is_retired_for_good() {
        let (core, a1, _, _) = two_corpora().await;
        let live = live_generation(&core).await;
        core.store
            .record_rehearsal(&NewRehearsal {
                class: Class::Cue,
                query: "nothing answers this any more".into(),
                query_vec: vec![0.0; crate::core::test_support::TEST_DIM],
                embed_model: core.embedder.model().to_string(),
                artifact_id: a1.clone(),
                source_id: None,
            })
            .await
            .unwrap();
        core.store
            .set_artifact_status(&a1, crate::store::artifacts::ArtifactStatus::Deprecated)
            .await
            .unwrap();

        let r = rehearse(&core, &live, crate::store::now()).await.unwrap();
        assert_eq!(r.retired, 1);
        assert_eq!(r.reminted, 0);
        assert_eq!(core.store.live_rehearsal_count().await.unwrap(), 0);
    }
}
