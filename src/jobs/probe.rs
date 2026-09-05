//! Cue probes: the questions a model-written artifact was written for,
//! embedded once so the base can ask them of itself.
//!
//! One unit per artifact, armed at `embed::mark_indexed` and nowhere else.
//! Idle-only, like `relate::arm`, so a re-embed that reaches this while an
//! earlier unit is queued does not wind its attempts back. One embedding per
//! cue, through the query side of the embedder — a cue is a question — and a
//! cue list is a handful.

use crate::core::Core;
use crate::error::Result;
use crate::store::jobs::Stage;
use crate::store::rehearsals::{Class, NewRehearsal};

pub async fn arm(core: &Core, artifact_id: &str) -> Result<()> {
    core.store
        .rearm_idle_seq(Stage::Probe, "artifact", artifact_id, 0)
        .await
}

pub async fn run(core: &Core, artifact_id: &str) -> Result<()> {
    let c = core.store.get_artifact(artifact_id).await?;
    // Passages have no cues and never get a probe minted from their heading:
    // that would be a query written while looking at the answer. Everything
    // else — captured, synthesized, merged — was written by a model with the
    // questions it answers beside it.
    if c.provenance == crate::store::artifacts::Provenance::Passage
        || c.cues.is_empty()
        || !c.in_results()
    {
        return Ok(());
    }
    let model = core.embedder.model().to_string();
    for cue in &c.cues {
        let cue = cue.trim();
        if cue.is_empty() {
            continue;
        }
        let permit = core.gate.background_light().await;
        let vec = core.embedder.embed_query(cue).await;
        permit.finished();
        let query_vec = vec?;
        core.store
            .record_rehearsal(&NewRehearsal {
                class: Class::Cue,
                query: cue.to_string(),
                query_vec,
                embed_model: model.clone(),
                artifact_id: c.id.clone(),
                source_id: None,
            })
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::artifacts::NewSynthesized;

    #[tokio::test]
    async fn a_cue_probe_is_minted_once_per_cue_and_never_for_a_passage() {
        let core = crate::core::test_support::test_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let passage = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: "verbatim".into(),
                    corpus_span: None,
                    title: Some("A heading".into()),
                    category: None,
                    tags: vec![],
                    segment_idx: None,
                    caveats: vec![],
                }],
            )
            .await
            .unwrap()[0]
            .id
            .clone();
        let synth = core
            .store
            .insert_synthesized_artifact(
                &NewSynthesized {
                    text: "mount with -o loop".into(),
                    title: Some("Mounting images".into()),
                    category: None,
                    tags: vec![],
                    caveats: vec![],
                    cues: vec!["how do I mount an image".into(), "loop mount".into()],
                },
                std::slice::from_ref(&passage),
            )
            .await
            .unwrap()
            .id;

        run(&core, &passage).await.unwrap();
        assert_eq!(
            core.store.live_rehearsal_count().await.unwrap(),
            0,
            "a passage has no cues"
        );

        run(&core, &synth).await.unwrap();
        run(&core, &synth).await.unwrap();
        let probes = core.store.rehearsals_of(&synth).await.unwrap();
        assert_eq!(probes.len(), 2, "once per cue, however often the unit runs");
        assert!(
            probes
                .iter()
                .all(|p| p.class == Class::Cue && p.source_id.is_none())
        );
        assert_eq!(probes[0].embed_model, core.embedder.model());
    }
}
