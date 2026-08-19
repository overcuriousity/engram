use super::Store;
use crate::error::Result;
use sqlx::Row;

/// Where one window of a source stands. `Failed` means the synthesizer never
/// succeeded here and the lines are represented by no chunk at all — the model
/// is a hard dependency, so an unsegmentable window leaves a hole that the
/// source's coverage measures, and the reason it ends up `partial`. `Verbatim`
/// means the window was captured as passages and never sent to the synthesizer
/// — not work owed, and nothing re-arms it; promotion moves it to `pending`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SegmentState {
    Pending,
    Done,
    Failed,
    Verbatim,
}

impl SegmentState {
    pub fn as_str(&self) -> &'static str {
        match self {
            SegmentState::Pending => "pending",
            SegmentState::Done => "done",
            SegmentState::Failed => "failed",
            SegmentState::Verbatim => "verbatim",
        }
    }
    pub fn parse(s: &str) -> SegmentState {
        match s {
            "done" => SegmentState::Done,
            "failed" => SegmentState::Failed,
            "verbatim" => SegmentState::Verbatim,
            _ => SegmentState::Pending,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Segment {
    pub corpus_id: String,
    pub idx: i64,
    pub start_line: i64,
    pub end_line: i64,
    /// The window's text, as the splitter produced it. Authoritative: the line
    /// range describes where it came from, but cannot reproduce it when the
    /// splitter had to cut inside a line.
    pub text: String,
    /// Leading lines of `text` that lie outside `start_line..=end_line`.
    pub carry_lines: i64,
    pub state: SegmentState,
    pub attempts: i64,
    pub last_error: Option<String>,
}

/// One window as the splitter produced it, on its way into the table.
///
/// A struct rather than a tuple because the fourth number is the one that means
/// nothing on its own: `(1, 40, text, 1)` cannot be read at a call site.
#[derive(Debug, Clone)]
pub struct NewSegment<'a> {
    pub start_line: i64,
    pub end_line: i64,
    pub text: &'a str,
    pub carry_lines: i64,
}

fn row_to_segment(r: &sqlx::sqlite::SqliteRow) -> Segment {
    Segment {
        corpus_id: r.get("corpus_id"),
        idx: r.get("idx"),
        start_line: r.get("start_line"),
        end_line: r.get("end_line"),
        text: r.get("text"),
        carry_lines: r.get("carry_lines"),
        state: SegmentState::parse(r.get::<String, _>("state").as_str()),
        attempts: r.get("attempts"),
        last_error: r.get("last_error"),
    }
}

impl Store {
    /// Record the windowing of a source.
    ///
    /// A corpus that has finished any window keeps the split it started with,
    /// whatever the current token budget would produce. The two cannot be
    /// mixed: a window that finished holds the text its artifacts were written
    /// from and cannot be re-derived without orphaning them, so a re-split
    /// around it moves only the boundaries it does not own. Window 0 stays
    /// `done` at lines 1-100 while window 1 is rewritten as 91-180, and the
    /// overlap is synthesised twice — or, shifted the other way, the gap
    /// between them is never synthesised at all. Neither is visible afterwards.
    ///
    /// So the split is settled by the first window to write anything. Until
    /// then it is
    /// rewritten freely, and windows the new split no longer reaches are
    /// dropped — otherwise a corpus that never started carries stale text into
    /// the model and queues surplus windows forever.
    pub async fn upsert_segments(&self, corpus_id: &str, windows: &[NewSegment<'_>]) -> Result<()> {
        let mut tx = self.pool.begin().await?;

        let finished: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM segments WHERE corpus_id = ? AND state = 'done'",
        )
        .bind(corpus_id)
        .fetch_one(&mut *tx)
        .await?;
        // Owning artifacts settles the split too, not just being marked `done`.
        // `write_segment_artifacts` commits the artifacts and only then sets the
        // state, so a process killed between the two leaves a window that owns
        // artifacts while still reading `pending` — and `done` alone would let
        // the split move out from under it. A window that produced no chunks is
        // why the state is still consulted: it owns nothing and has still
        // finished.
        let owned: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM artifacts WHERE corpus_id = ? AND segment_idx IS NOT NULL",
        )
        .bind(corpus_id)
        .fetch_one(&mut *tx)
        .await?;
        if finished > 0 || owned > 0 {
            return Ok(());
        }

        for (idx, w) in windows.iter().enumerate() {
            sqlx::query(
                "INSERT INTO segments (corpus_id, idx, start_line, end_line, text, carry_lines)
                 VALUES (?, ?, ?, ?, ?, ?)
                 ON CONFLICT(corpus_id, idx) DO UPDATE SET
                   start_line = excluded.start_line,
                   end_line = excluded.end_line,
                   text = excluded.text,
                   carry_lines = excluded.carry_lines,
                   no_promote = 0",
            )
            .bind(corpus_id)
            .bind(idx as i64)
            .bind(w.start_line)
            .bind(w.end_line)
            .bind(w.text)
            .bind(w.carry_lines)
            .execute(&mut *tx)
            .await?;
        }

        // Windows past the end of the new split. None of them owns an artifact:
        // the early return above leaves only corpora where no window has written
        // one, whatever state the rows are in.
        sqlx::query("DELETE FROM segments WHERE corpus_id = ? AND idx >= ?")
            .bind(corpus_id)
            .bind(windows.len() as i64)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(())
    }

    pub async fn segments_for_corpus(&self, corpus_id: &str) -> Result<Vec<Segment>> {
        let rows = sqlx::query("SELECT * FROM segments WHERE corpus_id = ? ORDER BY idx")
            .bind(corpus_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(row_to_segment).collect())
    }

    /// Segments still owed a model call: never tried, or tried and refused.
    ///
    /// `failed` is included on purpose. It records what went wrong last time,
    /// not a verdict — an endpoint that was loading a model, or a machine that
    /// was asleep, says nothing about the text. Excluding it made the next run
    /// see a finished corpus and close the job, which is how a quarter of a
    /// document stayed missing while the endpoint sat there answering.
    ///
    /// `verbatim` is excluded: that window was captured as passages on purpose
    /// and is owed nothing.
    pub async fn pending_segments(&self, corpus_id: &str) -> Result<Vec<Segment>> {
        let rows = sqlx::query(
            "SELECT * FROM segments
             WHERE corpus_id = ? AND state NOT IN ('done', 'verbatim') ORDER BY idx",
        )
        .bind(corpus_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_segment).collect())
    }

    pub async fn set_segment_state(
        &self,
        corpus_id: &str,
        idx: i64,
        state: SegmentState,
        last_error: Option<&str>,
    ) -> Result<()> {
        // Reaching `done` is also what spends `keep_artifacts`, in the same
        // statement so there is no window between the two. The write that
        // honoured the mark is not the end of the run — `flag_unverified`, this
        // state change and `settle` all follow it, and `SQLITE_BUSY` is routine
        // now that two workers can be in one corpus — so clearing at the write
        // meant any of those failing retried the window with the mark already
        // gone, and the retry deleted the curated originals the mark exists to
        // protect. Spending it here costs at worst one duplicated set of
        // artifacts, which the dedupe sweep folds.
        sqlx::query(
            "UPDATE segments SET state = ?, last_error = ?,
                    keep_artifacts = CASE WHEN ? = 'done' THEN 0 ELSE keep_artifacts END
             WHERE corpus_id = ? AND idx = ?",
        )
        .bind(state.as_str())
        .bind(last_error)
        .bind(state.as_str())
        .bind(corpus_id)
        .bind(idx)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// One window's state, or `None` for a window that does not exist.
    pub async fn segment_state(&self, corpus_id: &str, idx: i64) -> Result<Option<SegmentState>> {
        Ok(sqlx::query_scalar::<_, String>(
            "SELECT state FROM segments WHERE corpus_id = ? AND idx = ?",
        )
        .bind(corpus_id)
        .bind(idx)
        .fetch_optional(&self.pool)
        .await?
        .map(|s| SegmentState::parse(&s)))
    }

    /// Whether this window may be promoted at all: an operator who undid its
    /// promotion said no, and nothing but a re-split says otherwise.
    pub async fn segment_no_promote(&self, corpus_id: &str, idx: i64) -> Result<bool> {
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT no_promote FROM segments WHERE corpus_id = ? AND idx = ?",
        )
        .bind(corpus_id)
        .bind(idx)
        .fetch_optional(&self.pool)
        .await?
        .is_some_and(|v| v != 0))
    }

    /// Refuse this window to promotion, for good. Set by `undo_promotion`.
    pub async fn set_segment_no_promote(&self, corpus_id: &str, idx: i64) -> Result<()> {
        sqlx::query("UPDATE segments SET no_promote = 1 WHERE corpus_id = ? AND idx = ?")
            .bind(corpus_id)
            .bind(idx)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Mark every window of a corpus that was never synthesized — and is not
    /// going to be, at this mode — as captured verbatim. Only `pending` rows:
    /// a window that is `done` or `failed` has a history this must not erase.
    pub async fn mark_segments_verbatim(&self, corpus_id: &str) -> Result<()> {
        sqlx::query(
            "UPDATE segments SET state = 'verbatim' WHERE corpus_id = ? AND state = 'pending'",
        )
        .bind(corpus_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Put a window back in the queue's path, and say whether what it has
    /// already produced survives the second read.
    ///
    /// `keep_artifacts` is the whole difference between the two reasons to run a
    /// window twice. A window whose read was *wrong* is being replaced, and the
    /// artifacts it wrote go with it. A window that was read correctly but
    /// missed lines — the uncovered-lines button — is being *added to*, and
    /// deleting what it already produced would throw away artifacts an operator
    /// may have edited, tagged or verified since, for lines that were never the
    /// problem. See `window::write_segment_artifacts`.
    pub async fn reset_segment(
        &self,
        corpus_id: &str,
        idx: i64,
        keep_artifacts: bool,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE segments SET state = 'pending', attempts = 0, last_error = NULL,
                    keep_artifacts = ?
             WHERE corpus_id = ? AND idx = ?",
        )
        .bind(keep_artifacts as i64)
        .bind(corpus_id)
        .bind(idx)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Is this window mid-re-read, so that a write appends rather than replaces?
    pub async fn segment_keeps_artifacts(&self, corpus_id: &str, idx: i64) -> Result<bool> {
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT keep_artifacts FROM segments WHERE corpus_id = ? AND idx = ?",
        )
        .bind(corpus_id)
        .bind(idx)
        .fetch_optional(&self.pool)
        .await?
        .unwrap_or(0)
            != 0)
    }

    /// `(resolved, total)`, where resolved counts both a clean window and one
    /// the model gave up on. Both are settled; neither is still owed work.
    pub async fn segment_progress(&self, corpus_id: &str) -> Result<(i64, i64)> {
        let row = sqlx::query(
            "SELECT COUNT(*) AS total,
                    SUM(CASE WHEN state <> 'pending' THEN 1 ELSE 0 END) AS resolved
             FROM segments WHERE corpus_id = ?",
        )
        .bind(corpus_id)
        .fetch_one(&self.pool)
        .await?;
        let total: i64 = row.get("total");
        let resolved: Option<i64> = row.get("resolved");
        Ok((resolved.unwrap_or(0), total))
    }

    /// Drop the windowing entirely. Re-segmenting a source from scratch has to
    /// re-window it, because the text or the model's budget may have changed.
    pub async fn clear_segments(&self, corpus_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM segments WHERE corpus_id = ?")
            .bind(corpus_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn seg(start_line: i64, end_line: i64, text: &str) -> NewSegment<'_> {
        NewSegment {
            start_line,
            end_line,
            text,
            carry_lines: 0,
        }
    }

    #[tokio::test]
    async fn windows_are_upserted_once_and_resume_reads_only_the_pending_ones() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();

        s.upsert_segments(
            &src.id,
            &[
                seg(1, 10, "window 0"),
                seg(11, 20, "window 1"),
                seg(21, 30, "window 2"),
            ],
        )
        .await
        .unwrap();
        assert_eq!(s.segments_for_corpus(&src.id).await.unwrap().len(), 3);

        s.set_segment_state(&src.id, 0, SegmentState::Done, None)
            .await
            .unwrap();

        // A second call must not reset the window that already finished.
        s.upsert_segments(
            &src.id,
            &[
                seg(1, 10, "window 0"),
                seg(11, 20, "window 1"),
                seg(21, 30, "window 2"),
            ],
        )
        .await
        .unwrap();

        let pending = s.pending_segments(&src.id).await.unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].idx, 1);
        assert_eq!(pending[0].start_line, 11);
    }

    #[tokio::test]
    async fn a_corpus_that_never_started_is_re_split_and_the_surplus_dropped() {
        // The token budget can change under a corpus — the context blocks now
        // subtract from it — and the old windowing then survived as text no
        // splitter would produce, sent to the model as though it were current,
        // with the windows past the new end queued forever.
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        s.upsert_segments(
            &src.id,
            &[
                seg(1, 10, "window 0"),
                seg(11, 20, "window 1"),
                seg(21, 30, "window 2"),
            ],
        )
        .await
        .unwrap();

        s.upsert_segments(
            &src.id,
            &[seg(1, 15, "wider window 0"), seg(16, 30, "wider window 1")],
        )
        .await
        .unwrap();

        let w = s.segments_for_corpus(&src.id).await.unwrap();
        assert_eq!(w.len(), 2, "the surplus window was left queued");
        assert_eq!(w[0].text, "wider window 0");
        assert_eq!(w[1].start_line, 16);
    }

    #[tokio::test]
    async fn a_split_is_settled_by_the_first_window_to_finish() {
        // Re-splitting around a done window moves only the boundaries it does
        // not own. Here the budget shrank: window 0 stays done at 1-10, and a
        // re-split would leave window 1 starting at 16 — so source lines 11-15
        // would be synthesised by nothing, silently and unrecoverably. The
        // opposite drift duplicates instead. Neither is visible afterwards, so
        // the split stops moving once any of it has been acted on.
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        s.upsert_segments(
            &src.id,
            &[
                seg(1, 10, "window 0"),
                seg(11, 20, "window 1"),
                seg(21, 30, "window 2"),
            ],
        )
        .await
        .unwrap();
        s.set_segment_state(&src.id, 0, SegmentState::Done, None)
            .await
            .unwrap();

        s.upsert_segments(
            &src.id,
            &[seg(1, 15, "wider window 0"), seg(16, 30, "wider window 1")],
        )
        .await
        .unwrap();

        let w = s.segments_for_corpus(&src.id).await.unwrap();
        assert_eq!(w.len(), 3, "the old split must survive intact");
        assert_eq!(w[0].text, "window 0");
        assert_eq!(
            (w[1].start_line, w[1].end_line),
            (11, 20),
            "the window after a finished one must still start where that one ended"
        );
        assert_eq!(w[2].text, "window 2");
    }

    #[tokio::test]
    async fn a_failed_window_does_not_settle_the_split() {
        // Failure leaves no artifacts behind, so there is nothing for a new
        // split to orphan. Only a window that produced something settles it.
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        s.upsert_segments(&src.id, &[seg(1, 10, "window 0"), seg(11, 20, "window 1")])
            .await
            .unwrap();
        s.set_segment_state(&src.id, 0, SegmentState::Failed, Some("endpoint down"))
            .await
            .unwrap();

        s.upsert_segments(&src.id, &[seg(1, 20, "one wide window")])
            .await
            .unwrap();

        let w = s.segments_for_corpus(&src.id).await.unwrap();
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].text, "one wide window");
    }

    #[tokio::test]
    async fn progress_counts_done_and_failed_as_resolved() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        s.upsert_segments(
            &src.id,
            &[
                seg(1, 5, "window 0"),
                seg(6, 10, "window 1"),
                seg(11, 15, "window 2"),
            ],
        )
        .await
        .unwrap();

        s.set_segment_state(&src.id, 0, SegmentState::Done, None)
            .await
            .unwrap();
        s.set_segment_state(&src.id, 1, SegmentState::Failed, Some("endpoint down"))
            .await
            .unwrap();

        assert_eq!(s.segment_progress(&src.id).await.unwrap(), (2, 3));
        let w = s.segments_for_corpus(&src.id).await.unwrap();
        assert_eq!(w[1].last_error.as_deref(), Some("endpoint down"));
    }

    #[tokio::test]
    async fn a_reset_clears_a_window_back_to_pending() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        s.upsert_segments(&src.id, &[seg(1, 5, "window 0")])
            .await
            .unwrap();

        s.set_segment_state(&src.id, 0, SegmentState::Failed, Some("boom"))
            .await
            .unwrap();

        s.reset_segment(&src.id, 0, false).await.unwrap();
        let w = &s.segments_for_corpus(&src.id).await.unwrap()[0];
        assert_eq!(w.state, SegmentState::Pending);
        assert_eq!(w.attempts, 0);
        assert_eq!(w.last_error, None);
        assert!(
            !s.segment_keeps_artifacts(&src.id, 0).await.unwrap(),
            "a plain reset replaces what the window produced"
        );

        // The uncovered-lines re-read asks for the opposite, and the mark is
        // spent by the window reaching `done` so a later retry replaces again.
        s.reset_segment(&src.id, 0, true).await.unwrap();
        assert!(s.segment_keeps_artifacts(&src.id, 0).await.unwrap());
        s.set_segment_state(&src.id, 0, SegmentState::Failed, Some("boom"))
            .await
            .unwrap();
        assert!(
            s.segment_keeps_artifacts(&src.id, 0).await.unwrap(),
            "a failed attempt must leave the mark for the retry to honour"
        );
        s.set_segment_state(&src.id, 0, SegmentState::Done, None)
            .await
            .unwrap();
        assert!(!s.segment_keeps_artifacts(&src.id, 0).await.unwrap());
    }

    #[tokio::test]
    async fn a_window_that_wrote_artifacts_settles_the_split_before_it_is_done() {
        // `write_segment_artifacts` commits the artifacts and only then marks
        // the window `done`, so a process killed between the two leaves a
        // window that owns artifacts while still reading `pending`. Judged on
        // state alone, the resume re-split that follows would rewrite the
        // boundaries under it — and, if the new split were shorter, delete the
        // row while leaving its artifacts in the base with no window that will
        // ever replace them.
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        s.upsert_segments(&src.id, &[seg(1, 10, "window 0"), seg(11, 20, "window 1")])
            .await
            .unwrap();
        let a = crate::store::artifacts::NewArtifact {
            ordinal: 0,
            text: "what window 1 produced".into(),
            corpus_span: None,
            title: None,
            category: None,
            tags: vec![],
            segment_idx: Some(1),
            caveats: vec![],
        };
        s.insert_artifacts(&src.id, &[a]).await.unwrap();

        s.upsert_segments(&src.id, &[seg(1, 20, "one wide window")])
            .await
            .unwrap();

        let w = s.segments_for_corpus(&src.id).await.unwrap();
        assert_eq!(w.len(), 2, "the split moved out from under an artifact");
        assert_eq!(w[1].text, "window 1");
        assert_eq!(
            s.artifact_ids_for_segment(&src.id, 1).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn deleting_a_source_cascades_to_its_windows() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        s.upsert_segments(&src.id, &[seg(1, 5, "window 0")])
            .await
            .unwrap();
        s.delete_corpus(&src.id).await.unwrap();
        assert!(s.segments_for_corpus(&src.id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn verbatim_segments_are_not_pending_work() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        s.upsert_segments(
            &src.id,
            &[
                NewSegment {
                    start_line: 1,
                    end_line: 5,
                    text: "a",
                    carry_lines: 0,
                },
                NewSegment {
                    start_line: 6,
                    end_line: 9,
                    text: "b",
                    carry_lines: 0,
                },
            ],
        )
        .await
        .unwrap();
        assert_eq!(s.pending_segments(&src.id).await.unwrap().len(), 2);
        s.mark_segments_verbatim(&src.id).await.unwrap();
        assert!(s.pending_segments(&src.id).await.unwrap().is_empty());
        let rows = s.segments_for_corpus(&src.id).await.unwrap();
        assert!(rows.iter().all(|w| w.state == SegmentState::Verbatim));
        assert_eq!(SegmentState::parse("verbatim"), SegmentState::Verbatim);
        assert_eq!(SegmentState::Verbatim.as_str(), "verbatim");
        // Resolved, for the progress count: neither is still owed a call.
        assert_eq!(s.segment_progress(&src.id).await.unwrap(), (2, 2));
    }

    #[tokio::test]
    async fn segment_state_reads_one_window() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        s.upsert_segments(
            &src.id,
            &[NewSegment {
                start_line: 1,
                end_line: 2,
                text: "t",
                carry_lines: 0,
            }],
        )
        .await
        .unwrap();
        assert_eq!(
            s.segment_state(&src.id, 0).await.unwrap(),
            Some(SegmentState::Pending)
        );
        s.mark_segments_verbatim(&src.id).await.unwrap();
        assert_eq!(
            s.segment_state(&src.id, 0).await.unwrap(),
            Some(SegmentState::Verbatim)
        );
        assert_eq!(s.segment_state(&src.id, 9).await.unwrap(), None);
    }
}
