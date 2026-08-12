use super::Store;
use crate::error::Result;
use sqlx::Row;

/// Where one window of a source stands. `Failed` means the synthesizer never
/// succeeded here and the lines are represented by no chunk at all — the model
/// is a hard dependency, so an unsegmentable window leaves a hole that the
/// source's coverage measures, and the reason it ends up `partial`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SegmentState {
    Pending,
    Done,
    Failed,
}

impl SegmentState {
    pub fn as_str(&self) -> &'static str {
        match self {
            SegmentState::Pending => "pending",
            SegmentState::Done => "done",
            SegmentState::Failed => "failed",
        }
    }
    pub fn parse(s: &str) -> SegmentState {
        match s {
            "done" => SegmentState::Done,
            "failed" => SegmentState::Failed,
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
    /// So the split is settled by the first window to finish. Until then it is
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
        if finished > 0 {
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
                   carry_lines = excluded.carry_lines",
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

        // Windows past the end of the new split. Nothing here is `done` — the
        // early return above saw to that — so none of them owns an artifact.
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
    pub async fn pending_segments(&self, corpus_id: &str) -> Result<Vec<Segment>> {
        let rows = sqlx::query(
            "SELECT * FROM segments
             WHERE corpus_id = ? AND state != 'done' ORDER BY idx",
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
        sqlx::query(
            "UPDATE segments SET state = ?, last_error = ?
             WHERE corpus_id = ? AND idx = ?",
        )
        .bind(state.as_str())
        .bind(last_error)
        .bind(corpus_id)
        .bind(idx)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn bump_segment_attempts(&self, corpus_id: &str, idx: i64) -> Result<i64> {
        sqlx::query(
            "UPDATE segments SET attempts = attempts + 1
             WHERE corpus_id = ? AND idx = ?",
        )
        .bind(corpus_id)
        .bind(idx)
        .execute(&self.pool)
        .await?;
        let row = sqlx::query("SELECT attempts FROM segments WHERE corpus_id = ? AND idx = ?")
            .bind(corpus_id)
            .bind(idx)
            .fetch_one(&self.pool)
            .await?;
        Ok(row.get("attempts"))
    }

    /// Put a window back in the queue's path. The operator's "re-segment this
    /// window" button, and nothing else, calls this.
    pub async fn reset_segment(&self, corpus_id: &str, idx: i64) -> Result<()> {
        sqlx::query(
            "UPDATE segments SET state = 'pending', attempts = 0, last_error = NULL
             WHERE corpus_id = ? AND idx = ?",
        )
        .bind(corpus_id)
        .bind(idx)
        .execute(&self.pool)
        .await?;
        Ok(())
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
    async fn attempts_accumulate_and_a_reset_clears_them() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        s.upsert_segments(&src.id, &[seg(1, 5, "window 0")])
            .await
            .unwrap();

        assert_eq!(s.bump_segment_attempts(&src.id, 0).await.unwrap(), 1);
        assert_eq!(s.bump_segment_attempts(&src.id, 0).await.unwrap(), 2);
        s.set_segment_state(&src.id, 0, SegmentState::Failed, Some("boom"))
            .await
            .unwrap();

        s.reset_segment(&src.id, 0).await.unwrap();
        let w = &s.segments_for_corpus(&src.id).await.unwrap()[0];
        assert_eq!(w.state, SegmentState::Pending);
        assert_eq!(w.attempts, 0);
        assert_eq!(w.last_error, None);
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
}
