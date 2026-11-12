//! Sleep's integrate and rehearse phases, plus the corpus rules `interference` (counts, files
//! nothing) and `condense_candidates`. Nothing here spends a generation.

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
    pub probes: usize,
    pub stopped: bool,
}

/// Novel below `review_min`, known at or above it; whether two artifacts disagree is not a question
/// token splitting can answer.
pub fn tag_for(review_min: f32, nearest: Option<f32>) -> Tag {
    match nearest {
        Some(score) if score >= review_min => Tag::Known,
        _ => Tag::Novel,
    }
}

/// Same-corpus hits are structure and filtered out. Files the integration row and one probe per
/// near hit, nothing for a person.
pub async fn integrate(core: &Core, started: i64) -> Result<Integrated> {
    let mut out = Integrated::default();
    let review_min = core
        .ranking
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .review_min;
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

        // Probes before the integration row: once the row commits the artifact is never revisited,
        // so probes minted after a failure are gone. A re-run re-mints nothing (unique over live
        // rows).
        let mut minted = 0;
        if let Some(vec) = core.vectors.dense_of(&a.id).await? {
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
                    minted += 1;
                }
            }
        }

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
        // `false` is another pass having got there first; its probes stand.
        if !written {
            continue;
        }
        out.probes += minted;
        out.integrated += 1;
        match tag {
            Tag::Novel => out.novel += 1,
            Tag::Known => out.known += 1,
        }
    }
    Ok(out)
}

pub const REHEARSED_AFTER: &str = "sleep.rehearsed_after";

#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct Replayed {
    pub rehearsed: usize,
    pub found: usize,
    /// A changed rank, or a first measurement under this generation. `jobs::did_work` reads this,
    /// not `rehearsed`, which the wrapping lap keeps non-zero forever.
    pub moved: usize,
    pub retired: usize,
    /// See `remint`.
    pub reminted: usize,
    pub stopped: bool,
}

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
        served_reranked: false,
        // The source answers itself at cosine one and would cap the owner's reciprocal rank at one
        // half.
        exclude: r.source_id.iter().cloned().collect(),
    }
}

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

/// Half the bound goes to probes whose last two results disagree; the lap cursor wraps like
/// `retract`'s.
pub async fn rehearse(
    core: &Core,
    live: &crate::store::generations::Generation,
    started: i64,
) -> Result<Replayed> {
    let mut out = Replayed::default();
    let current = *core
        .ranking
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
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
    // A fragile probe is also on the lap; measuring it twice writes two rows off one reading.
    let held: std::collections::HashSet<String> = batch.iter().map(|r| r.id.clone()).collect();
    // Taken before the overlap is dropped, or a lap that was all fragile never advances the cursor.
    let lap_end = lap.last().map(|r| crate::store::Cursor {
        at: r.created_at,
        id: r.id.clone(),
    });
    let lap: Vec<_> = lap.into_iter().filter(|r| !held.contains(&r.id)).collect();
    let lap_start = batch.len();
    batch.extend(lap);

    // Absent means never measured under this generation, not measured and missed.
    let ids: Vec<String> = batch.iter().map(|r| r.id.clone()).collect();
    let was = core.store.latest_ranks_under(&ids, &live.id).await?;

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
        // Another era's vector is not comparable with the live index.
        let pair = pair_of(core, r).await;
        let stale = r.embed_model != model;
        let stands = owner_stands(core, &pair.satisfies).await;
        if stale || !stands {
            // A gone owner stays retired; a changed embedder needs a replacement, minted from an
            // artifact that may not be re-embedded yet. `Waiting` keeps the probe live until it is.
            if stale && stands {
                // Decide here, write below: the live index is unique over `(artifact_id, class,
                // query)`, so the retirement must precede the insert, and nothing is retired
                // without a replacement coming.
                match remintable(core, r, &model).await? {
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
        // Stored 1-based, measured 0-based.
        let now_at = rank.map(|r| r as i64 + 1);
        match was.get(&r.id) {
            Some(before) => out.moved += usize::from(*before != now_at),
            None => out.moved += 1,
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
    if let Some(end) = lap_end {
        cursor = end;
    }
    core.store
        .meta_set(REHEARSED_AFTER, &cursor.encode())
        .await?;
    Ok(out)
}

/// Asked before the old probe is retired: `retired_at` is one-way. A capture probe's vector is read
/// back from its source, so it waits until the source carries the live model.
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
            // Not yet, rather than never: the embed queue drains.
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

/// Only `Gone` may retire the probe.
enum Remintable {
    From(Vec<f32>),
    Waiting,
    Gone,
}

/// `false` where `INSERT OR IGNORE` declined, which is a replacement either way.
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

/// Two rows are two observations only if two probes wrote them.
fn probes_behind(results: &[crate::store::rehearsals::RehearsalResult]) -> usize {
    results
        .iter()
        .map(|r| r.rehearsal_id.as_str())
        .collect::<std::collections::HashSet<_>>()
        .len()
}

/// Ids above the owner in every retained result from at least `MIN_AGREEING_PROBES` distinct
/// probes, in another corpus. Empty if any result missed the owner: its `outranked_by` is the whole
/// top ten.
pub fn interferers<F>(
    results: &[crate::store::rehearsals::RehearsalResult],
    own_corpus: Option<&str>,
    corpus_of: F,
) -> Vec<String>
where
    F: Fn(&str) -> Option<String>,
{
    if probes_behind(results) < crate::store::rehearsals::MIN_AGREEING_PROBES
        || results.iter().any(|r| r.rank.is_none())
    {
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

/// Rule 3: retrieval competition, counted for Ops. Files nothing, since a ranking observation is
/// not a disagreement, and is not budget-gated. Returns (interferers observed, stopped early).
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
        // Under the live generation only: a rank is a measurement at one set of parameters.
        let mut results = Vec::new();
        for p in core.store.rehearsals_of(&owner.id).await? {
            results.extend(
                core.store
                    .results_of_under(&p.id, &live.id, OBSERVATION_LIMIT)
                    .await?,
            );
        }
        let mut corpora: std::collections::HashMap<String, Option<String>> =
            std::collections::HashMap::new();
        // Only the ids every result carries; `outranked_by` can name an artifact since buried or
        // merged.
        let mut gone: std::collections::HashSet<String> = std::collections::HashSet::new();
        for x in results
            .first()
            .map(|r| &r.outranked_by)
            .into_iter()
            .flatten()
        {
            if corpora.contains_key(x) || !results.iter().all(|r| r.outranked_by.contains(x)) {
                continue;
            }
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
        let found = interferers(&results, owner.corpus_id.as_deref(), |id| {
            corpora.get(id).cloned().flatten()
        });
        for x in found {
            if gone.contains(&x) {
                tracing::debug!(owner = %owner.id, outranker = %x, "an outranker the base no longer has");
                continue;
            }
            // An action taken back is a person's decision, not re-observed at them.
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

/// Arm a condensation for a model-written owner one competitor stands above in every result of
/// `MIN_AGREEING_PROBES` probes, with engagement above `promote.activation_above`.
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
        // Under the live generation only: condensing sets `embed_state = 'pending'`, and its own
        // miss would otherwise disable this owner forever.
        let mut results = Vec::new();
        for p in core.store.rehearsals_of(&owner.id).await? {
            results.extend(
                core.store
                    .results_of_under(&p.id, &live.id, OBSERVATION_LIMIT)
                    .await?,
            );
        }
        // Distinct probes, not rows: the wrapping lap writes identical rows.
        if probes_behind(&results) < crate::store::rehearsals::MIN_AGREEING_PROBES
            || results.iter().any(|r| r.rank.is_none())
        {
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
        if !core.may_act(crate::store::actions::Job::Sleep).await? {
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
        // The first corpus's two saw only each other, discarded as structure.
        let tag = |id: &str| {
            let core = &core;
            let id = id.to_string();
            async move { core.store.integration_of(&id).await.unwrap().unwrap().tag }
        };
        assert_eq!(tag(&a1).await, Tag::Novel);
        assert_eq!(tag(&a2).await, Tag::Novel);
        assert_eq!(tag(&b).await, Tag::Known);
        assert_eq!(core.store.rehearsals_of(&a1).await.unwrap().len(), 1);
        assert_eq!(core.store.rehearsals_of(&a2).await.unwrap().len(), 1);
        assert!(core.store.rehearsals_of(&b).await.unwrap().is_empty());
        let p = &core.store.rehearsals_of(&a1).await.unwrap()[0];
        assert_eq!(p.source_id.as_deref(), Some(b.as_str()));
        assert_eq!(p.query, "the image will not mount");
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
        // The fake embedder hashes text, so any neighbour must count as the same statement.
        let mut core = crate::core::test_support::test_core().await;
        core.consolidate.auto_supersede = -1.0;
        core.ranking
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .review_min = -1.0;
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

    async fn two_probes_on(core: &crate::core::Core, owner: &str) -> Vec<String> {
        let mut out = Vec::new();
        for q in ["q", "and another"] {
            out.push(
                core.store
                    .record_rehearsal(&NewRehearsal {
                        class: Class::Cue,
                        query: q.into(),
                        query_vec: vec![0.0; crate::core::test_support::TEST_DIM],
                        embed_model: core.embedder.model().to_string(),
                        artifact_id: owner.to_string(),
                        source_id: None,
                    })
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        out
    }

    async fn outranked(
        core: &crate::core::Core,
        pids: &[String],
        generation: &str,
        above: &[String],
    ) {
        for pid in pids {
            core.store
                .record_rehearsal_result(&crate::store::rehearsals::NewResult {
                    rehearsal_id: pid.clone(),
                    generation_id: generation.to_string(),
                    rank: Some(2),
                    outranked_by: above.to_vec(),
                })
                .await
                .unwrap();
        }
    }

    pub(crate) async fn live_generation(
        core: &crate::core::Core,
    ) -> crate::store::generations::Generation {
        let params = *core
            .ranking
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        assert_eq!(
            rehearse(&core, &live, crate::store::now())
                .await
                .unwrap()
                .rehearsed,
            2
        );
    }

    #[test]
    fn an_interferer_stands_above_in_every_result_of_two_probes_and_is_from_another_corpus() {
        let res = |probe: &str, above: &[&str]| crate::store::rehearsals::RehearsalResult {
            id: String::new(),
            rehearsal_id: probe.to_string(),
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
            interferers(&[res("p", &["x"])], Some("mine"), corpus).is_empty(),
            "one result is not a pattern"
        );
        assert!(
            interferers(&[res("p", &["x"]), res("p", &["x"])], Some("mine"), corpus).is_empty(),
            "one probe agreeing with itself is one observation"
        );
        assert_eq!(
            interferers(
                &[res("p", &["x", "y"]), res("q", &["x"])],
                Some("mine"),
                corpus
            ),
            vec!["x".to_string()]
        );
        assert!(
            interferers(&[res("p", &["x"]), res("q", &["y"])], Some("mine"), corpus).is_empty()
        );
        assert!(
            interferers(
                &[res("p", &["twin"]), res("q", &["twin"])],
                Some("mine"),
                corpus
            )
            .is_empty(),
            "same corpus is structure"
        );

        let missed = |probe: &str, above: &[&str]| crate::store::rehearsals::RehearsalResult {
            rank: None,
            ..res(probe, above)
        };
        assert!(
            interferers(
                &[res("p", &["x"]), missed("q", &["x", "y"])],
                Some("mine"),
                corpus
            )
            .is_empty(),
            "a replay that did not find the owner is not evidence about what displaced it"
        );
    }

    #[tokio::test]
    async fn a_probe_the_fragile_half_took_is_not_measured_a_second_time_by_the_lap() {
        let (core, a1, _a2, _b) = two_corpora().await;
        let live = live_generation(&core).await;
        integrate(&core, crate::store::now()).await.unwrap();
        let pid = core.store.rehearsals_of(&a1).await.unwrap()[0].id.clone();
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

    #[tokio::test]
    async fn a_lap_the_fragile_half_emptied_still_moves_the_cursor_on() {
        let (core, _a1, _a2, _b) = two_corpora().await;
        let live = live_generation(&core).await;
        integrate(&core, crate::store::now()).await.unwrap();
        let probes = core
            .store
            .rehearsals_after(&crate::store::Cursor::default(), 10)
            .await
            .unwrap();
        assert_eq!(probes.len(), 2);
        let first = crate::store::Cursor {
            at: probes[0].created_at,
            id: probes[0].id.clone(),
        };
        core.store
            .meta_set(REHEARSED_AFTER, &first.encode())
            .await
            .unwrap();
        for rank in [Some(1), Some(3)] {
            core.store
                .record_rehearsal_result(&crate::store::rehearsals::NewResult {
                    rehearsal_id: probes[1].id.clone(),
                    generation_id: live.id.clone(),
                    rank,
                    outranked_by: vec![],
                })
                .await
                .unwrap();
        }

        rehearse(&core, &live, crate::store::now()).await.unwrap();
        let at = core
            .store
            .meta_get(REHEARSED_AFTER)
            .await
            .unwrap()
            .map(|s| crate::store::Cursor::parse(&s))
            .expect("a cursor");
        assert_eq!(
            at.id, probes[1].id,
            "the cursor stood still behind a lap the fragile half had emptied"
        );
    }

    #[tokio::test]
    async fn a_capture_probe_waits_for_its_artifact_to_be_re_embedded_rather_than_being_lost() {
        let (core, a1, _a2, b) = two_corpora().await;
        let live = live_generation(&core).await;
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

    #[tokio::test]
    async fn an_outranker_the_base_no_longer_has_is_skipped_rather_than_failing_the_sweep() {
        let (mut core, a1, _a2, b) = two_corpora().await;
        core.evolve.autonomous = crate::config::Autonomy::Full;
        let live = live_generation(&core).await;
        let pids = two_probes_on(&core, &a1).await;
        outranked(
            &core,
            &pids,
            &live.id,
            &["a-buried-artifact".to_string(), b.clone()],
        )
        .await;
        let (seen, _) = interference(&core, &live, crate::store::now())
            .await
            .expect("one missing id does not fail the sweep");
        assert_eq!(
            seen, 1,
            "the outranker that is still there is still counted"
        );
        assert!(core.store.pair_between(&a1, &b).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn interference_counts_what_it_sees_and_files_no_pair() {
        let (mut core, a1, _a2, b) = two_corpora().await;
        core.evolve.autonomous = crate::config::Autonomy::Full;
        let live = live_generation(&core).await;
        let pids = two_probes_on(&core, &a1).await;
        outranked(&core, &pids, &live.id, std::slice::from_ref(&b)).await;
        let (seen, _) = interference(&core, &live, crate::store::now())
            .await
            .unwrap();
        assert_eq!(seen, 1, "the interferer is still counted");
        assert!(
            core.store.pair_between(&a1, &b).await.unwrap().is_none(),
            "retrieval competition is not a dedupe finding and files no pair"
        );
        assert_eq!(
            interference(&core, &live, crate::store::now())
                .await
                .unwrap()
                .0,
            1
        );
    }

    #[tokio::test]
    async fn interference_writes_no_pair_carrying_a_zero_score() {
        let (mut core, a1, _a2, b) = two_corpora().await;
        core.evolve.autonomous = crate::config::Autonomy::Full;
        let live = live_generation(&core).await;
        let pids = two_probes_on(&core, &a1).await;
        outranked(&core, &pids, &live.id, std::slice::from_ref(&b)).await;
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

    #[tokio::test]
    async fn interference_keeps_counting_when_the_day_is_spent() {
        let (mut core, a1, _a2, b) = two_corpora().await;
        core.evolve.autonomous = crate::config::Autonomy::Full;
        core.evolve.max_actions_per_day = 0;
        let live = live_generation(&core).await;
        let pids = two_probes_on(&core, &a1).await;
        outranked(&core, &pids, &live.id, std::slice::from_ref(&b)).await;
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

    #[tokio::test]
    async fn a_miss_under_an_older_generation_does_not_disable_condense_for_ever() {
        let (mut core, a1, _a2, b) = two_corpora().await;
        core.evolve.autonomous = crate::config::Autonomy::Full;
        core.promote.activation_above = 0.0;
        let old = live_generation(&core).await;
        let pids = two_probes_on(&core, &a1).await;
        core.store
            .record_rehearsal_result(&crate::store::rehearsals::NewResult {
                rehearsal_id: pids[0].clone(),
                generation_id: old.id.clone(),
                rank: None,
                outranked_by: vec![],
            })
            .await
            .unwrap();

        let live = live_generation(&core).await;
        assert_ne!(live.id, old.id);
        outranked(&core, &pids, &live.id, std::slice::from_ref(&b)).await;

        let (armed, _) = condense_candidates(&core, &live, crate::store::now())
            .await
            .unwrap();
        assert_eq!(
            armed, 1,
            "a miss under settings the base no longer runs held the rule shut"
        );
        assert_eq!(
            interference(&core, &live, crate::store::now())
                .await
                .unwrap()
                .0,
            1,
            "and the same history was read by rule 3"
        );
    }

    #[tokio::test]
    async fn only_a_probe_that_landed_somewhere_new_counts_as_work() {
        let (core, _a1, _a2, _b) = two_corpora().await;
        let live = live_generation(&core).await;
        integrate(&core, crate::store::now()).await.unwrap();

        let first = rehearse(&core, &live, crate::store::now()).await.unwrap();
        assert!(first.rehearsed > 0);
        assert_eq!(
            first.moved, first.rehearsed,
            "a first measurement is all signal"
        );

        let second = rehearse(&core, &live, crate::store::now()).await.unwrap();
        assert_eq!(second.rehearsed, first.rehearsed, "the lap wrapped");
        assert_eq!(
            second.moved, 0,
            "an unchanged base reported work and the backoff never engaged"
        );
    }

    #[tokio::test]
    async fn an_integrated_artifact_always_has_the_probes_its_integration_implies() {
        let (core, a1, a2, b) = two_corpora().await;
        let out = integrate(&core, crate::store::now()).await.unwrap();
        assert!(out.integrated > 0);
        assert!(out.probes > 0, "the near hits minted nothing");
        let mut found = 0;
        for id in [&a1, &a2, &b] {
            found += core.store.rehearsals_of(id).await.unwrap().len();
        }
        assert_eq!(
            found, out.probes,
            "the count and the rows disagree, so one of them was written without the other"
        );
    }

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
        let r = rehearse(&core, &live, crate::store::now()).await.unwrap();
        assert_eq!(r.rehearsed, 1);
        assert_eq!(r.retired, 0);
    }

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
