//! The heartbeat: re-arm work no retry covers, such as a process killed between two writes.
//! Idempotent, and arms only units nothing will run. Not `enqueue`, which resets attempts and stops
//! a failing document ever settling.

use crate::core::Core;
use crate::error::Result;
use crate::store::corpora::CorpusStatus;
use crate::store::jobs::Stage;
use crate::store::segments::SegmentState;

/// These statuses are written beside an enqueue and cleared by the stage it arms; holding one with
/// nothing queued means the unit was never armed.
fn awaiting(status: &CorpusStatus) -> Option<Stage> {
    match status {
        CorpusStatus::Raw => Some(Stage::Synthesize),
        CorpusStatus::Describing => Some(Stage::Describe),
        CorpusStatus::Extracting => Some(Stage::Extract),
        _ => None,
    }
}

/// Each re-read is a model call per lost window, and the first sweep after an upgrade finds the
/// whole backlog.
const AUTO_REREADS_PER_PASS: usize = 5;

pub async fn run(core: &Core) -> Result<usize> {
    // On the repair ticker whatever `[consolidate]` says, so a base with dedupe off still has
    // nothing left waiting.
    let drained = core.store.drain_waiting_pairs().await?;
    if drained.closed + drained.rejudged > 0 {
        tracing::info!(
            ?drained,
            "moved what was waiting on a person into the sweep"
        );
    }
    let mut armed = 0;
    let mut rereads = 0;
    // A cursor, not an offset: captures land while this runs.
    let mut cursor: Option<(i64, String)> = None;
    loop {
        let page = core.store.list_corpora_after(cursor.as_ref(), 100).await?;
        let Some(last) = page.last() else { break };
        cursor = Some((last.created_at, last.id.clone()));
        for c in &page {
            // Nothing writes `needs_review` any more: release it with its flag kept, and pair
            // judging settles it.
            if c.status == CorpusStatus::NeedsReview {
                core.release_parked(&c.id).await?;
                armed += 1;
                continue;
            }
            // Only for a capture the queue calls low-coverage: nearly every document has a line
            // nothing was written from. Once, stamped first so a failed read cannot loop.
            if rereads < AUTO_REREADS_PER_PASS
                && c.auto_reread_at.is_none()
                && crate::core::ingest::coverage_final(&c.status)
                && crate::core::coverage::low_coverage(c.coverage)
            {
                rereads += 1;
                core.store.mark_auto_reread(&c.id).await?;
                if core.reread_uncovered(&c.id, 1, i64::MAX).await? {
                    armed += 1;
                }
            }
            // Windowless corpora are left alone: `heal_dangling_supersessions` writes such
            // placeholders.
            let segments = core.store.segments_for_corpus(&c.id).await?;
            // The corpus row is written before its unit, in another database, so a failure between
            // leaves a capture with no job. Only with no job row at all and no artifacts: a
            // placeholder is windowless and `raw` too.
            if segments.is_empty()
                && let Some(stage) = awaiting(&c.status)
                && core.store.artifacts_for_corpus(&c.id).await?.is_empty()
                && !core.store.has_job(stage, &c.id).await?
            {
                core.store.rearm_idle_seq(stage, "corpus", &c.id, 0).await?;
                armed += 1;
                continue;
            }
            // `verbatim` is the decided state of a window at `off`; arming it would synthesize
            // through the back door.
            let unresolved: Vec<_> = segments
                .iter()
                .filter(|w| w.state != SegmentState::Done && w.state != SegmentState::Verbatim)
                .collect();
            if !unresolved.is_empty() {
                // A killed process can leave a window without its unit; units stay derivable from
                // the rows.
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
            // Every window resolved but coverage was never measured: the process died before
            // `settle`. Windowless corpora are excluded again, or a placeholder gets settled and a
            // `Title` call to name an empty document.
            if !segments.is_empty()
                && c.coverage.is_none()
                && !core.store.artifacts_for_corpus(&c.id).await?.is_empty()
            {
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
        // Re-arming from zero would keep a window forever young, so `settle` never counts it spent.
        let core = test_core().await;
        let out = core
            .ingest("alpha para\n\nbeta para", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::plan(&core, &out.id).await.unwrap();
        sqlx::query!("UPDATE jobs SET attempts = 4 WHERE stage = 'segment_window'")
            .execute(&core.store.control.pool)
            .await
            .unwrap();

        run(&core).await.unwrap();

        let attempts: Vec<i64> =
            sqlx::query_scalar!("SELECT attempts FROM jobs WHERE stage = 'segment_window'")
                .fetch_all(&core.store.control.pool)
                .await
                .unwrap();
        assert!(
            attempts.iter().all(|&a| a == 4),
            "the sweep reset a unit that was already queued: {attempts:?}"
        );
    }

    #[tokio::test]
    async fn a_corpus_whose_window_units_are_gone_gets_them_back() {
        let core = test_core().await;
        let body = (0..400)
            .map(|i| format!("paragraph {i} with filler text"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let out = core.ingest(&body, "web", None).await.unwrap();
        crate::jobs::synthesize::plan(&core, &out.id).await.unwrap();

        for w in core.store.segments_for_corpus(&out.id).await.unwrap() {
            core.store
                .reset_segment(&out.id, w.idx, true)
                .await
                .unwrap();
        }
        sqlx::query!("DELETE FROM jobs WHERE stage = 'segment_window'")
            .execute(&core.store.control.pool)
            .await
            .unwrap();
        core.store
            .enqueue(Stage::Synthesize, "corpus", &out.id)
            .await
            .unwrap();

        run(&core).await.unwrap();

        let windows = core.store.segments_for_corpus(&out.id).await.unwrap().len();
        assert!(windows > 2, "the fixture must span several windows");
        let armed: i64 = sqlx::query_scalar!(
            "SELECT count(*) FROM jobs WHERE stage = 'segment_window' AND state = 'pending'"
        )
        .fetch_one(&core.store.control.pool)
        .await
        .unwrap();
        assert_eq!(armed as usize, windows, "the old job did not become units");
    }

    fn seg(start_line: i64, end_line: i64, text: &str) -> NewSegment<'_> {
        NewSegment {
            start_line,
            end_line,
            text,
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

        // One unit for that window, not a re-plan of a document whose other window is done.
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
                    text: "a body".into(),
                    segment_idx: Some(0),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();

        // `finish` measures every corpus that reached embedding; without this the fixture is a
        // different repair.
        core.store
            .set_corpus_coverage(&src.id, Some(0.9))
            .await
            .unwrap();

        assert_eq!(run(&core).await.unwrap(), 1);
        let job = core.store.claim_job().await.unwrap().expect("a job");
        assert_eq!(job.stage, Stage::Embed);
    }

    #[tokio::test]
    async fn a_corpus_that_resolved_every_window_but_never_finished_is_healed() {
        // The embed branch gives the corpus a status anyway, which is what makes this invisible.
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
                    text: "the window".into(),
                    segment_idx: Some(0),
                    ..Default::default()
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
    async fn a_corpus_with_no_windows_at_all_is_not_settled_or_named() {
        // Coverage alone cannot tell a placeholder from a document whose `finish` never ran.
        let core = test_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        core.store
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    text: "an artifact older than windows".into(),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();

        run(&core).await.unwrap();

        assert!(
            core.store
                .get_corpus(&src.id)
                .await
                .unwrap()
                .coverage
                .is_none(),
            "a document with no windows was measured against a source it never had"
        );
        assert!(
            !core.store.has_job(Stage::Title, &src.id).await.unwrap(),
            "a model call was spent naming a document with nothing to name it from"
        );
    }

    #[tokio::test]
    async fn a_capture_whose_unit_was_never_armed_gets_one() {
        let core = test_core().await;
        let src = core
            .store
            .insert_corpus("a capture nothing was armed for", "web", None)
            .await
            .unwrap();

        assert_eq!(run(&core).await.unwrap(), 1);
        let job = core.store.claim_job().await.unwrap().expect("a job");
        assert_eq!(job.stage, Stage::Synthesize);
        assert_eq!(job.target_id, src.id);
    }

    #[tokio::test]
    async fn a_capture_that_already_has_its_unit_is_left_alone() {
        // Re-arming would wind a queued unit's attempts back to zero.
        let core = test_core().await;
        let src = core
            .store
            .insert_corpus("a queued capture", "web", None)
            .await
            .unwrap();
        core.store
            .enqueue(Stage::Synthesize, "corpus", &src.id)
            .await
            .unwrap();

        assert_eq!(run(&core).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn an_image_still_waiting_to_be_read_gets_its_unit_back() {
        let core = test_core().await;
        let src = core.store.insert_corpus("", "upload", None).await.unwrap();
        core.store
            .set_corpus_status(&src.id, crate::store::corpora::CorpusStatus::Describing)
            .await
            .unwrap();

        assert_eq!(run(&core).await.unwrap(), 1);
        assert_eq!(
            core.store.claim_job().await.unwrap().expect("a job").stage,
            Stage::Describe
        );
    }

    #[tokio::test]
    async fn a_placeholder_corpus_is_not_dragged_into_synthesis() {
        let core = test_core().await;
        let src = core.store.insert_corpus("", "web", None).await.unwrap();
        core.store
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    text: "an artifact that outlived its corpus".into(),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();

        run(&core).await.unwrap();
        assert!(
            !core
                .store
                .has_job(Stage::Synthesize, &src.id)
                .await
                .unwrap(),
            "a placeholder was handed to synthesis"
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
    async fn a_flagged_capture_left_without_its_unit_is_armed_like_any_other() {
        // A flag is not a gate: a capture nobody chose on must still become searchable.
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
        assert!(
            targets.contains(&b.id),
            "the flagged capture was left unread"
        );
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

    #[tokio::test]
    async fn the_sweep_arms_nothing_for_a_verbatim_corpus() {
        let core = test_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        core.store
            .upsert_segments(&src.id, &[seg(1, 10, "the window")])
            .await
            .unwrap();
        core.store.mark_segments_verbatim(&src.id).await.unwrap();
        let armed = run(&core).await.unwrap();
        assert_eq!(armed, 0);
        assert!(
            !core
                .store
                .live_job(
                    Stage::SegmentWindow,
                    &crate::jobs::window::unit_target(&src.id, 0)
                )
                .await
                .unwrap()
        );
    }

    async fn a_capture_that_lost_its_second_half(
        core: &Core,
        status: CorpusStatus,
    ) -> crate::store::corpora::Corpus {
        a_capture_that_lost_its_second_half_named(core, status, "eight").await
    }

    async fn a_capture_that_lost_its_second_half_named(
        core: &Core,
        status: CorpusStatus,
        last: &str,
    ) -> crate::store::corpora::Corpus {
        use crate::store::artifacts::{CorpusSpan, SpanSource};
        let raw = format!("one\ntwo\nthree\nfour\nfive\nsix\nseven\n{last}");
        let src = core.store.insert_corpus(&raw, "web", None).await.unwrap();
        core.store
            .upsert_segments(
                &src.id,
                &[
                    seg(1, 4, "one\ntwo\nthree\nfour"),
                    seg(5, 8, &format!("five\nsix\nseven\n{last}")),
                ],
            )
            .await
            .unwrap();
        for idx in [0, 1] {
            core.store
                .set_segment_state(&src.id, idx, SegmentState::Done, None)
                .await
                .unwrap();
        }
        core.store
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    text: "one to four".into(),
                    segment_idx: Some(0),
                    corpus_span: Some(CorpusSpan {
                        start_line: 1,
                        end_line: 4,
                        source: SpanSource::Located,
                    }),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();
        core.store
            .set_corpus_coverage(&src.id, Some(0.5))
            .await
            .unwrap();
        core.store.set_corpus_status(&src.id, status).await.unwrap();
        sqlx::query!(
            "UPDATE artifacts SET status = 'ready' WHERE corpus_id = ?",
            &src.id
        )
        .execute(&core.store.pool)
        .await
        .unwrap();
        core.store.get_corpus(&src.id).await.unwrap()
    }

    async fn window_jobs(core: &Core) -> i64 {
        sqlx::query_scalar!("SELECT COUNT(*) FROM jobs WHERE stage = 'segment_window'")
            .fetch_one(&core.store.control.pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_finished_capture_with_lost_lines_is_reread_once() {
        let core = test_core().await;
        let c = a_capture_that_lost_its_second_half(&core, CorpusStatus::Partial).await;
        assert!(c.auto_reread_at.is_none());

        run(&core).await.unwrap();

        assert!(
            core.store
                .live_job(
                    Stage::SegmentWindow,
                    &crate::jobs::window::unit_target(&c.id, 1)
                )
                .await
                .unwrap(),
            "the window holding the lost lines was not queued"
        );
        assert!(
            !core
                .store
                .live_job(
                    Stage::SegmentWindow,
                    &crate::jobs::window::unit_target(&c.id, 0)
                )
                .await
                .unwrap(),
            "a window that was fully covered was read again"
        );
        assert!(
            core.store
                .get_corpus(&c.id)
                .await
                .unwrap()
                .auto_reread_at
                .is_some()
        );

        core.store
            .set_segment_state(&c.id, 1, SegmentState::Done, None)
            .await
            .unwrap();
        sqlx::query!("DELETE FROM jobs WHERE stage = 'segment_window'")
            .execute(&core.store.control.pool)
            .await
            .unwrap();
        run(&core).await.unwrap();
        assert_eq!(
            window_jobs(&core).await,
            0,
            "the capture was read a third time"
        );
    }

    #[tokio::test]
    async fn a_capture_still_being_read_is_left_alone() {
        let core = test_core().await;
        let c = a_capture_that_lost_its_second_half(&core, CorpusStatus::Segmenting).await;

        run(&core).await.unwrap();

        assert_eq!(
            window_jobs(&core).await,
            0,
            "unread lines were queued as lost"
        );
        assert!(
            core.store
                .get_corpus(&c.id)
                .await
                .unwrap()
                .auto_reread_at
                .is_none(),
            "a capture not yet final was stamped as reread"
        );
    }

    #[tokio::test]
    async fn a_well_read_capture_with_an_uncovered_line_is_not_reread() {
        let core = test_core().await;
        // Lines nothing claims on a capture whose coverage the queue calls fine.
        let c = a_capture_that_lost_its_second_half(&core, CorpusStatus::Ready).await;
        core.store
            .set_corpus_coverage(&c.id, Some(0.95))
            .await
            .unwrap();

        run(&core).await.unwrap();

        assert_eq!(
            window_jobs(&core).await,
            0,
            "a well-read capture was reread"
        );
        assert!(
            core.store
                .get_corpus(&c.id)
                .await
                .unwrap()
                .auto_reread_at
                .is_none(),
            "a capture the gate passed over was stamped"
        );
    }

    #[tokio::test]
    async fn one_sweep_rereads_at_most_its_bound_and_the_next_takes_the_rest() {
        let core = test_core().await;
        let mut ids = Vec::new();
        for i in 0..=AUTO_REREADS_PER_PASS {
            ids.push(
                a_capture_that_lost_its_second_half_named(
                    &core,
                    CorpusStatus::Partial,
                    &format!("eight {i}"),
                )
                .await
                .id,
            );
        }
        let stamped = |core: &Core, ids: Vec<String>| {
            let store = core.store.clone();
            async move {
                let mut n = 0;
                for id in ids {
                    if store
                        .get_corpus(&id)
                        .await
                        .unwrap()
                        .auto_reread_at
                        .is_some()
                    {
                        n += 1;
                    }
                }
                n
            }
        };

        run(&core).await.unwrap();
        assert_eq!(window_jobs(&core).await, AUTO_REREADS_PER_PASS as i64);
        assert_eq!(stamped(&core, ids.clone()).await, AUTO_REREADS_PER_PASS);

        run(&core).await.unwrap();
        assert_eq!(
            window_jobs(&core).await,
            AUTO_REREADS_PER_PASS as i64 + 1,
            "the capture past the bound was not taken up by the next sweep"
        );
        assert_eq!(stamped(&core, ids).await, AUTO_REREADS_PER_PASS + 1);
    }
}
