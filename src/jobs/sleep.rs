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
}
