//! The heartbeat: pick up anything that was left unfinished.
//!
//! Every stage already retries its own job, so this is not the retry mechanism.
//! It is for the case no retry covers — a job completed while its work was not,
//! a process killed between two writes, a corpus queued by a build that had a
//! bug in it. Without it, "the system repairs itself" holds only for the
//! failures the system happened to be watching at the time.
//!
//! Cheap and idempotent: the queue is keyed by (stage, target), and this arms
//! only units nothing is going to run — a base with nothing wrong costs one
//! query per hundred corpora and changes not a row. Deliberately *not*
//! `enqueue`: that resets attempts, and a sweep that keeps winding a failing
//! unit's attempts back to zero is a sweep that stops its document ever
//! settling.

use crate::core::Core;
use crate::error::Result;
use crate::store::jobs::Stage;
use crate::store::segments::SegmentState;

pub async fn run(core: &Core) -> Result<usize> {
    let mut armed = 0;
    // A cursor, not an offset: captures land while this runs, and an offset
    // over a newest-first list would step over one corpus per insertion.
    let mut cursor: Option<(i64, String)> = None;
    loop {
        let page = core.store.list_corpora_after(cursor.as_ref(), 100).await?;
        let Some(last) = page.last() else { break };
        cursor = Some((last.created_at, last.id.clone()));
        for c in &page {
            // A corpus parked as a near-duplicate is waiting on a person by
            // design, and segmenting it is the decision they have not made.
            if c.near_dupe_of.is_some() {
                continue;
            }
            // A corpus with no window rows at all is deliberately left alone:
            // its artifacts pre-date windows, and planning it would re-segment
            // a document that is already fine. Capture arms the planning job;
            // this sweep is for work that started and stopped.
            let segments = core.store.segments_for_corpus(&c.id).await?;
            let unresolved: Vec<_> = segments
                .iter()
                .filter(|w| w.state != SegmentState::Done)
                .collect();
            if !unresolved.is_empty() {
                // Windows exist but their units may not: a database written
                // before units existed has none at all, and a process killed
                // between two writes can be missing one. This is what makes a
                // materialised queue safe — the units stay derivable from the
                // rows that describe the work, so drift heals on a sweep rather
                // than needing to be noticed.
                for w in unresolved {
                    core.store
                        .rearm_idle_seq(
                            Stage::SegmentWindow,
                            "segment",
                            &crate::jobs::window::unit_target(&c.id, w.idx),
                            w.idx,
                        )
                        .await?;
                    armed += 1;
                }
                continue;
            }
            // Every window resolved and the document was still never finished.
            // `finish` measures coverage on every path that produced artifacts,
            // so a corpus with artifacts and none of it is one whose process
            // died between the last window's `Done` and the `settle` that
            // follows it. Nothing else would notice: the embed branch below
            // still fires and `settle_corpus` gives it a status, while the
            // renumbering, the coverage measure and the title unit never ran.
            //
            // `settle` rather than a job, because there is no inference here —
            // it is the same local work `finish` does, and re-running it on a
            // document that is fine is what the coverage test rules out.
            if c.coverage.is_none() && !core.store.artifacts_for_corpus(&c.id).await?.is_empty() {
                crate::jobs::window::settle(core, &c.id).await?;
                armed += 1;
                continue;
            }
            if !core
                .store
                .pending_artifacts_for_corpus(&c.id)
                .await?
                .is_empty()
            {
                core.store
                    .rearm_idle_seq(Stage::Embed, "corpus", &c.id, 0)
                    .await?;
                armed += 1;
            }
        }
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
    use crate::store::segments::NewSegment;

    #[tokio::test]
    async fn a_sweep_does_not_wind_back_a_failing_units_attempts() {
        // A window the model will not read has to reach the ceiling before its
        // document can settle around it. A sweep that re-armed every unresolved
        // window from zero kept such a unit forever young, so `settle` never
        // counted it as spent and the corpus never left `segmenting` — the sweep
        // meant to heal the base was the thing holding it open.
        let core = test_core().await;
        let out = core
            .ingest("alpha para\n\nbeta para", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::plan(&core, &out.id).await.unwrap();
        sqlx::query("UPDATE jobs SET attempts = 4 WHERE stage = 'segment_window'")
            .execute(&core.store.pool)
            .await
            .unwrap();

        run(&core).await.unwrap();

        let attempts: Vec<i64> =
            sqlx::query_scalar("SELECT attempts FROM jobs WHERE stage = 'segment_window'")
                .fetch_all(&core.store.pool)
                .await
                .unwrap();
        assert!(
            attempts.iter().all(|&a| a == 4),
            "the sweep reset a unit that was already queued: {attempts:?}"
        );
    }

    #[tokio::test]
    async fn an_old_corpus_level_job_becomes_per_window_units() {
        // The upgrade path. A database written before units existed holds one
        // Synthesize row per unfinished corpus and no window units at all, so
        // without this the windows would sit unsegmented until someone noticed.
        let core = test_core().await;
        let body = (0..400)
            .map(|i| format!("paragraph {i} with filler text"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let out = core.ingest(&body, "web", None).await.unwrap();
        crate::jobs::synthesize::plan(&core, &out.id).await.unwrap();

        // Wind the clock back to the old shape: windows, no units.
        sqlx::query("DELETE FROM jobs WHERE stage = 'segment_window'")
            .execute(&core.store.pool)
            .await
            .unwrap();
        core.store
            .enqueue(Stage::Synthesize, "corpus", &out.id)
            .await
            .unwrap();

        run(&core).await.unwrap();

        let windows = core.store.segments_for_corpus(&out.id).await.unwrap().len();
        assert!(windows > 2, "the fixture must span several windows");
        let armed: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM jobs WHERE stage = 'segment_window' AND state = 'pending'",
        )
        .fetch_one(&core.store.pool)
        .await
        .unwrap();
        assert_eq!(armed as usize, windows, "the old job did not become units");
    }

    fn seg(start_line: i64, end_line: i64, text: &str) -> NewSegment<'_> {
        NewSegment {
            start_line,
            end_line,
            text,
            carry_lines: 0,
        }
    }

    #[tokio::test]
    async fn a_corpus_with_an_unfinished_segment_and_no_job_gets_one() {
        let core = test_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        core.store
            .upsert_segments(
                &src.id,
                &[seg(1, 10, "first window"), seg(11, 20, "second window")],
            )
            .await
            .unwrap();
        core.store
            .set_segment_state(&src.id, 0, SegmentState::Done, None)
            .await
            .unwrap();

        // Segment 1 never ran and nothing is queued: the crack this closes.
        // One unit, addressed to that window — not a job for the whole corpus,
        // which would re-plan a document whose other window is already done.
        assert_eq!(run(&core).await.unwrap(), 1);
        let job = core.store.claim_job().await.unwrap().expect("a job");
        assert_eq!(job.stage, Stage::SegmentWindow);
        assert_eq!(job.target_id, crate::jobs::window::unit_target(&src.id, 1));
    }

    #[tokio::test]
    async fn a_corpus_whose_artifacts_never_embedded_gets_an_embed_job() {
        let core = test_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        core.store
            .upsert_segments(&src.id, &[seg(1, 10, "the window")])
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

        // A corpus that got as far as embedding has been through `finish`, and
        // `finish` measures it. Without this the fixture is indistinguishable
        // from a document whose `finish` never ran, which is a different repair.
        core.store.set_corpus_coverage(&src.id, 0.9).await.unwrap();

        assert_eq!(run(&core).await.unwrap(), 1);
        let job = core.store.claim_job().await.unwrap().expect("a job");
        assert_eq!(job.stage, Stage::Embed);
    }

    #[tokio::test]
    async fn a_corpus_that_resolved_every_window_but_never_finished_is_healed() {
        // Killed between the last window's `Done` and the `settle` that
        // follows it. Every window has resolved, so the branch above arms
        // nothing, and the embed branch below gives the corpus a status anyway
        // — which is what makes this invisible. The document was never
        // renumbered, never measured and never named, and nothing else in the
        // system would ever do it.
        let core = test_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        core.store
            .upsert_segments(&src.id, &[seg(1, 10, "the window")])
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
                    text: "the window".into(),
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
        assert!(
            core.store
                .get_corpus(&src.id)
                .await
                .unwrap()
                .coverage
                .is_none(),
            "the fixture must start unmeasured"
        );

        run(&core).await.unwrap();

        assert!(
            core.store
                .get_corpus(&src.id)
                .await
                .unwrap()
                .coverage
                .is_some(),
            "the document was never measured, and nothing was left that would"
        );
        assert!(
            core.store.has_job(Stage::Title, &src.id).await.unwrap(),
            "the document was never handed to the namer"
        );
    }

    #[tokio::test]
    async fn a_finished_corpus_is_left_alone() {
        let core = test_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        core.store
            .upsert_segments(&src.id, &[seg(1, 10, "the window")])
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
        // Parking withholds the model call until someone chooses. Re-arming it
        // here would spend exactly what parking exists to save.
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
            .upsert_segments(&src.id, &[seg(1, 10, "the window")])
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
