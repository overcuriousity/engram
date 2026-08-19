//! Pursuits: a coherent run of searches, what was engaged with, and — when the
//! base did not answer or the answer was assembled by hand — the one artifact
//! that earns.
//!
//! Local decides, the model only writes. The sweep (`run`) groups quiet
//! searches by their stored vectors, scores engagement, and arms `Generate`
//! for a pursuit that earned it; `generate` makes the one call.

use crate::core::Core;
use crate::error::Result;
use crate::infer::prompt;
use crate::store::artifacts::NewSynthesized;
use crate::store::jobs::Stage;

/// Write the artifact a pursuit earned. One call; supersedes nothing.
///
/// Idempotent: a pursuit that is no longer `open`, or already names an
/// artifact, is left alone — a retry after a crash between the insert and the
/// pursuit update must not write twice.
pub async fn generate(core: &Core, pursuit_id: &str) -> Result<()> {
    let p = core.store.get_pursuit(pursuit_id).await?;
    if p.state != "open" || p.artifact_id.is_some() {
        return Ok(());
    }
    let Some(generator) = core.generator.clone() else {
        return Ok(());
    };
    let now = crate::store::now();

    // The engaged artifacts, whatever their provenance — a generated artifact
    // the operator pivoted through contributes its own text, unresolved. In
    // engagement order, the way the sweep stored them.
    let rows = core.store.artifacts_by_ids(&p.sources).await?;
    let mut sources: Vec<crate::store::artifacts::Chunk> = p
        .sources
        .iter()
        .filter_map(|id| rows.iter().find(|c| &c.id == id).cloned())
        .filter(|c| c.in_results())
        .collect();
    if sources.len() < core.pursuit.min_sources {
        core.store
            .close_pursuit(
                pursuit_id,
                "unsatisfied",
                "sources gone before generation",
                now,
            )
            .await?;
        return Ok(());
    }

    // Packed to the window the way the dedupe judge packs: the questions and
    // the system prompt always go out; sources are dropped from the tail when
    // they would not fit.
    let window = generator.context_tokens();
    let ceiling = generator.max_output_tokens();
    let system = core.counter.count(prompt::GENERATE_SYSTEM);
    let user = loop {
        let excerpts: Vec<(String, String)> = sources
            .iter()
            .map(|c| (c.title.clone().unwrap_or_default(), c.text.clone()))
            .collect();
        let user = prompt::generate_prompt(&p.queries, &excerpts);
        let spent = system + core.counter.count(&user);
        if spent + ceiling.min(window / 2) <= window || sources.len() <= core.pursuit.min_sources {
            break user;
        }
        sources.pop();
    };
    let source_text: String = sources
        .iter()
        .map(|c| c.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");

    let permit = core.gate.background().await;
    let reply = generator.complete(prompt::GENERATE_SYSTEM, &user).await;
    permit.finished();
    let g = prompt::parse_generation(&reply?)?;

    let ids: Vec<String> = sources.iter().map(|c| c.id.clone()).collect();
    let made = core
        .store
        .insert_synthesized_artifact(
            &NewSynthesized {
                text: g.text,
                title: Some(g.title),
                category: g.category,
                tags: g.tags,
                caveats: g.caveats,
                cues: p.queries.clone(),
            },
            &ids,
        )
        .await?;
    // Drift is caught rather than prevented: a literal in the generated text
    // that no source carries is flagged for whoever reads it.
    let missing = crate::infer::verify::missing_literals(&made.text, &made.caveats, &source_text);
    if let Some(first) = missing.first() {
        core.store
            .set_artifact_flags(
                &made.id,
                &[crate::infer::verify::FLAG_LITERALS.to_string()],
                Some(&format!("missing literal: {first}")),
            )
            .await?;
    }
    core.store
        .enqueue(Stage::Embed, "artifact", &made.id)
        .await?;
    core.store
        .set_pursuit_artifact(pursuit_id, &made.id, now)
        .await?;
    tracing::info!(pursuit = pursuit_id, artifact_id = %made.id, sources = ids.len(), "generated an artifact from a pursuit");
    Ok(())
}

/// Placeholder until the sweep lands in the next task.
pub async fn run(_core: &Core) -> Result<usize> {
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_support::test_core;
    use crate::store::artifacts::{NewArtifact, Provenance};

    #[test]
    fn a_generation_reply_parses_and_an_empty_one_does_not() {
        let g = prompt::parse_generation(
            r#"{"artifact":{"title":"T","text":"run `mount -o ro`","category":"procedure","tags":["x"],"caveats":["read-only"]}}"#,
        )
        .unwrap();
        assert_eq!(g.title, "T");
        assert_eq!(g.caveats, vec!["read-only".to_string()]);
        assert!(prompt::parse_generation(r#"{"artifact":{"title":"T","text":"  "}}"#).is_err());
        assert!(prompt::parse_generation("not json").is_err());
    }

    async fn two_sources(core: &crate::core::Core) -> Vec<String> {
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let na = |o: i64, t: &str| NewArtifact {
            ordinal: o,
            text: t.into(),
            corpus_span: None,
            title: Some(format!("S{o}")),
            category: None,
            tags: vec![],
            segment_idx: None,
            caveats: vec![],
        };
        core.store
            .insert_artifacts(
                &src.id,
                &[
                    na(0, "mount the image with `mount -o ro,loop`"),
                    na(1, "then read the journal at /var/log/journal"),
                ],
            )
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect()
    }

    #[tokio::test]
    async fn a_pursuit_is_written_up_once_with_cues_lineage_and_an_embed() {
        let mut core = test_core().await;
        core.generator = Some(std::sync::Arc::new(
            crate::infer::fake::ScriptedCompleter::new(vec![
                r#"{"artifact":{"title":"Reading a journal","text":"Mount with `mount -o ro,loop`, then read /var/log/journal.","category":"procedure","tags":[],"caveats":[]}}"#.into(),
            ]),
        ));
        let ids = two_sources(&core).await;
        let pid = core
            .store
            .insert_pursuit(100, &["how do I read the journal".into()], &ids)
            .await
            .unwrap();

        generate(&core, &pid).await.unwrap();
        generate(&core, &pid).await.unwrap();

        let made = core.store.synthesized_artifacts(10).await.unwrap();
        assert_eq!(made.len(), 1, "generated twice");
        let g = &made[0];
        assert_eq!(g.provenance, Provenance::Synthesized);
        assert_eq!(g.cues, vec!["how do I read the journal".to_string()]);
        assert!(g.flags.is_empty(), "{:?}", g.flags);
        let roots = core
            .store
            .roots_of(std::slice::from_ref(&g.id))
            .await
            .unwrap();
        let mut got = roots[&g.id].clone();
        got.sort();
        let mut want = ids.clone();
        want.sort();
        assert_eq!(got, want);
        assert!(core.store.live_job(Stage::Embed, &g.id).await.unwrap());
        let p = core.store.get_pursuit(&pid).await.unwrap();
        assert_eq!(p.state, "generated");
        assert_eq!(p.artifact_id.as_deref(), Some(g.id.as_str()));
        // Its sources stay active: nothing was superseded.
        for id in &ids {
            assert!(core.store.get_artifact(id).await.unwrap().in_results());
        }
    }

    #[tokio::test]
    async fn a_literal_no_source_carries_is_flagged() {
        let mut core = test_core().await;
        core.generator = Some(std::sync::Arc::new(
            crate::infer::fake::ScriptedCompleter::new(vec![
                r#"{"artifact":{"title":"T","text":"Run `wipefs --all /dev/sdX` first.","category":"procedure","tags":[],"caveats":[]}}"#.into(),
            ]),
        ));
        let ids = two_sources(&core).await;
        let pid = core
            .store
            .insert_pursuit(100, &["q".into()], &ids)
            .await
            .unwrap();
        generate(&core, &pid).await.unwrap();
        let g = &core.store.synthesized_artifacts(10).await.unwrap()[0];
        assert!(
            g.flags
                .iter()
                .any(|f| f == crate::infer::verify::FLAG_LITERALS),
            "{:?}",
            g.flags
        );
    }
}
