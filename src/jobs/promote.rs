//! Promotion: synthesis armed by evidence instead of by capture.

use crate::core::Core;
use crate::error::Result;
use crate::store::artifacts::Provenance;
use crate::store::jobs::Stage;
use crate::store::segments::SegmentState;

/// Promote the windows of any of these passages that have earned it.
///
/// Called after an engagement bump — opened, or confirmed — and never after a
/// retrieval bump: a passage that merely keeps appearing in result lists has
/// helped nobody, and the condition "opened or confirmed at least once" is
/// *where* this is called, not a stored flag. Checked at the bump, not on a
/// sweep: a sweep reads decayed activation and the threshold would then mean
/// something different depending on when it ran.
///
/// Arms a job; calls no model. The job queue and `[pacing]` bound the load.
pub async fn maybe_promote(core: &Core, ids: &[String], at: i64) -> Result<usize> {
    if core.synthesis != crate::config::SynthesisMode::Earned || !core.synthesizes() {
        return Ok(0);
    }
    let activation = core.store.activation_of(ids).await?;
    let mut armed = 0;
    for id in ids {
        let Some((value, stamp)) = activation.get(id) else {
            continue;
        };
        let now_value =
            crate::store::links::decayed(*value, *stamp, at, core.activation.half_life_days);
        if now_value < core.promote.activation_above {
            continue;
        }
        let Ok(c) = core.store.get_artifact(id).await else {
            continue;
        };
        if c.provenance != Provenance::Passage || !c.in_results() {
            continue;
        }
        let (Some(corpus_id), Some(idx)) = (c.corpus_id.as_deref(), c.segment_idx) else {
            continue;
        };
        // The guard against re-promotion is the segment state: a window that
        // is `done` — or already on its way — never promotes again, however
        // many of its surviving passages cross the line afterwards.
        if core.store.segment_state(corpus_id, idx).await? != Some(SegmentState::Verbatim) {
            continue;
        }
        // `keep_artifacts`: the window job appends rather than replaces, so the
        // passages survive to be superseded by what covers them.
        core.store.reset_segment(corpus_id, idx, true).await?;
        core.store
            .rearm_idle_seq(
                Stage::SegmentWindow,
                "segment",
                &crate::jobs::window::unit_target(corpus_id, idx),
                idx,
            )
            .await?;
        tracing::info!(
            artifact_id = %id,
            corpus_id,
            window = idx,
            activation = now_value,
            "promoting a window"
        );
        armed += 1;
    }
    Ok(armed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SynthesisMode;
    use crate::core::test_support::test_core;

    /// A core at `earned`, recording, with one verbatim corpus of one passage.
    async fn earned_with_one_passage() -> (crate::core::Core, String, String) {
        let mut core = test_core().await;
        core.synthesis = SynthesisMode::Earned;
        core.feedback.enabled = true;
        let out = core
            .ingest("a single verbatim passage", "web", None)
            .await
            .unwrap();
        crate::jobs::passages::capture_verbatim(&core, &out.id)
            .await
            .unwrap();
        let p = core.store.artifacts_for_corpus(&out.id).await.unwrap()[0]
            .id
            .clone();
        (core, out.id, p)
    }

    fn unit(corpus: &str) -> String {
        crate::jobs::window::unit_target(corpus, 0)
    }

    #[tokio::test]
    async fn a_passage_over_the_line_arms_its_window_once() {
        let (core, corpus, p) = earned_with_one_passage().await;
        // Baseline 1.0 plus one confirmed bump puts it at 4.0 exactly.
        core.store
            .bump_activation(std::slice::from_ref(&p), 3.0, 14.0, 1_000)
            .await
            .unwrap();
        let armed = maybe_promote(&core, std::slice::from_ref(&p), 1_000)
            .await
            .unwrap();
        assert_eq!(armed, 1);
        assert_eq!(
            core.store.segment_state(&corpus, 0).await.unwrap(),
            Some(SegmentState::Pending)
        );
        assert!(
            core.store
                .segment_keeps_artifacts(&corpus, 0)
                .await
                .unwrap()
        );
        assert!(
            core.store
                .live_job(Stage::SegmentWindow, &unit(&corpus))
                .await
                .unwrap()
        );
        // A second trigger on a window that is no longer verbatim does nothing.
        core.store
            .set_segment_state(&corpus, 0, SegmentState::Done, None)
            .await
            .unwrap();
        let again = maybe_promote(&core, std::slice::from_ref(&p), 1_000)
            .await
            .unwrap();
        assert_eq!(again, 0);
    }

    #[tokio::test]
    async fn under_the_line_nothing_is_armed() {
        let (core, corpus, p) = earned_with_one_passage().await;
        core.store
            .bump_activation(std::slice::from_ref(&p), 1.0, 14.0, 1_000)
            .await
            .unwrap();
        assert_eq!(
            maybe_promote(&core, std::slice::from_ref(&p), 1_000)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            core.store.segment_state(&corpus, 0).await.unwrap(),
            Some(SegmentState::Verbatim)
        );
    }

    #[tokio::test]
    async fn only_earned_with_a_synthesizer_promotes() {
        let (mut core, _corpus, p) = earned_with_one_passage().await;
        core.store
            .bump_activation(std::slice::from_ref(&p), 5.0, 14.0, 1_000)
            .await
            .unwrap();
        core.synthesis = SynthesisMode::Off;
        assert_eq!(
            maybe_promote(&core, std::slice::from_ref(&p), 1_000)
                .await
                .unwrap(),
            0
        );
        core.synthesis = SynthesisMode::Earned;
        core.synthesizer = None;
        assert_eq!(
            maybe_promote(&core, std::slice::from_ref(&p), 1_000)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn retrieval_alone_never_promotes_but_one_open_afterwards_does() {
        // The threshold is checked at the opened bump, not the retrieved one:
        // ten retrievals leave the window verbatim; the first open promotes.
        let (core, corpus, p) = earned_with_one_passage().await;
        let ids = vec![p.clone()];
        // Stamped now: `mark_artifact_seen` reads the clock, and a bump from
        // 1970 would have decayed to nothing by the time it looks.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        for _ in 0..10 {
            core.store
                .bump_activation(&ids, core.activation.retrieved, 14.0, now)
                .await
                .unwrap();
        }
        assert_eq!(
            core.store.segment_state(&corpus, 0).await.unwrap(),
            Some(SegmentState::Verbatim)
        );
        core.mark_artifact_seen(&p);
        core.background.wait_idle().await;
        assert_eq!(
            core.store.segment_state(&corpus, 0).await.unwrap(),
            Some(SegmentState::Pending)
        );
    }
}
