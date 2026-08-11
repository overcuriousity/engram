use crate::core::Core;
use crate::error::Result;
use crate::store::jobs::Stage;
use crate::store::segments::SegmentState;

pub async fn run(core: &Core) -> Result<usize> {
    let mut armed = 0;
    let mut offset = 0;
    loop {
        let page = core.store.list_corpora(100, offset).await?;
        if page.is_empty() {
            break;
        }
        for c in &page {
            if c.near_dupe_of.is_some() {
                continue;
            }
            let segments = core.store.segments_for_corpus(&c.id).await?;
            if segments.iter().any(|w| w.state != SegmentState::Done) {
                core.store
                    .enqueue(Stage::Synthesize, "corpus", &c.id)
                    .await?;
                armed += 1;
                continue;
            }
            if !core
                .store
                .pending_artifacts_for_corpus(&c.id)
                .await?
                .is_empty()
            {
                core.store.enqueue(Stage::Embed, "corpus", &c.id).await?;
                armed += 1;
            }
        }
        offset += page.len() as i64;
    }
    if armed > 0 {
        tracing::info!(armed, "reconciliation queued unfinished work");
    }
    Ok(armed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_support::test_core;
    use crate::store::artifacts::NewArtifact;

    #[tokio::test]
    async fn a_corpus_with_an_unfinished_segment_and_no_job_gets_one() {
        let core = test_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        core.store
            .upsert_segments(&src.id, &[(1, 10), (11, 20)])
            .await
            .unwrap();
        core.store
            .set_segment_state(&src.id, 0, SegmentState::Done, None)
            .await
            .unwrap();

        assert_eq!(run(&core).await.unwrap(), 1);
        let job = core.store.claim_job().await.unwrap().expect("a job");
        assert_eq!(job.stage, Stage::Synthesize);
        assert_eq!(job.target_id, src.id);
    }

    #[tokio::test]
    async fn a_corpus_whose_artifacts_never_embedded_gets_an_embed_job() {
        let core = test_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        core.store
            .upsert_segments(&src.id, &[(1, 10)])
            .await
            .unwrap();
        core.store
            .set_segment_state(&src.id, 0, SegmentState::Done, None)
            .await
            .unwrap();
        core.store
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    ordinal: 0,
                    text: "a body".into(),
                    corpus_span: None,
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: Some(0),
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();

        assert_eq!(run(&core).await.unwrap(), 1);
        let job = core.store.claim_job().await.unwrap().expect("a job");
        assert_eq!(job.stage, Stage::Embed);
    }

    #[tokio::test]
    async fn a_finished_corpus_is_left_alone() {
        let core = test_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        core.store
            .upsert_segments(&src.id, &[(1, 10)])
            .await
            .unwrap();
        core.store
            .set_segment_state(&src.id, 0, SegmentState::Done, None)
            .await
            .unwrap();
        assert_eq!(run(&core).await.unwrap(), 0);
        assert!(core.store.claim_job().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_capture_parked_for_a_decision_is_not_dragged_into_the_pipeline() {
        let core = test_core().await;
        let a = core
            .store
            .insert_corpus("the same text", "web", None)
            .await
            .unwrap();
        let b = core
            .store
            .insert_corpus("the same text!", "web", None)
            .await
            .unwrap();
        core.store
            .set_near_dupe(&b.id, Some(&a.id), Some(0.99))
            .await
            .unwrap();

        run(&core).await.unwrap();
        let mut targets = Vec::new();
        while let Some(j) = core.store.claim_job().await.unwrap() {
            targets.push(j.target_id);
        }
        assert!(!targets.contains(&b.id), "the parked capture was queued");
    }

    #[tokio::test]
    async fn the_sweep_does_not_pile_up_jobs_across_runs() {
        let core = test_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        core.store
            .upsert_segments(&src.id, &[(1, 10)])
            .await
            .unwrap();
        run(&core).await.unwrap();
        run(&core).await.unwrap();

        core.store.claim_job().await.unwrap().expect("one job");
        assert!(
            core.store.claim_job().await.unwrap().is_none(),
            "the sweep queued the same work twice"
        );
    }
}
