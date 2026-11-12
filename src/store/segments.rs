use super::Store;
use crate::error::Result;

/// `Failed`: never synthesized, so its lines are a coverage hole. `Verbatim`: captured as
/// passages and owed nothing; promotion moves it to `pending`.
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
    /// Authoritative: the line range cannot reproduce it when the splitter cut inside a line.
    pub text: String,
    pub state: SegmentState,
    pub attempts: i64,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewSegment<'a> {
    pub start_line: i64,
    pub end_line: i64,
    pub text: &'a str,
}

#[derive(sqlx::FromRow)]
pub(crate) struct SegmentRow {
    pub corpus_id: String,
    pub idx: i64,
    pub start_line: i64,
    pub end_line: i64,
    pub text: String,
    #[allow(dead_code, reason = "filled by query_as!, unread here")]
    pub carry_lines: i64,
    pub state: String,
    #[allow(dead_code, reason = "filled by query_as!, unread here")]
    pub keep_artifacts: i64,
    #[allow(dead_code, reason = "filled by query_as!, unread here")]
    pub no_promote: i64,
    pub attempts: i64,
    pub last_error: Option<String>,
}

fn row_to_segment(r: SegmentRow) -> Segment {
    Segment {
        corpus_id: r.corpus_id,
        idx: r.idx,
        start_line: r.start_line,
        end_line: r.end_line,
        text: r.text,
        state: SegmentState::parse(r.state.as_str()),
        attempts: r.attempts,
        last_error: r.last_error,
    }
}

impl Store {
    /// The split is settled by the first window to write anything: re-splitting around a done
    /// window would duplicate or skip lines. Until then it is rewritten, surplus windows dropped.
    pub async fn upsert_segments(&self, corpus_id: &str, windows: &[NewSegment<'_>]) -> Result<()> {
        // Immediate, because this reads then writes: a deferred `BEGIN` gets `SQLITE_BUSY` at once
        // on the lock upgrade instead of waiting out `busy_timeout`.
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;

        let finished: i64 = sqlx::query_scalar!(
            "SELECT count(*) FROM segments WHERE corpus_id = ? AND state = 'done'",
            corpus_id
        )
        .fetch_one(&mut *tx)
        .await?;
        // Owning artifacts settles the split too: artifacts commit before the state turns `done`.
        // Notes own no window and are excluded.
        let owned: i64 = sqlx::query_scalar!(
            "SELECT count(*) FROM artifacts
              WHERE corpus_id = ? AND provenance != 'note' AND segment_idx IS NOT NULL",
            corpus_id
        )
        .fetch_one(&mut *tx)
        .await?;
        if finished > 0 || owned > 0 {
            return Ok(());
        }

        for (idx, w) in windows.iter().enumerate() {
            sqlx::query(
                // `carry_lines` stays in the schema at a constant 0.
                "INSERT INTO segments (corpus_id, idx, start_line, end_line, text, carry_lines)
                 VALUES (?, ?, ?, ?, ?, 0)
                 ON CONFLICT(corpus_id, idx) DO UPDATE SET
                   start_line = excluded.start_line,
                   end_line = excluded.end_line,
                   text = excluded.text,
                   carry_lines = 0,
                   no_promote = 0",
            )
            .bind(corpus_id)
            .bind(idx as i64)
            .bind(w.start_line)
            .bind(w.end_line)
            .bind(w.text)
            .execute(&mut *tx)
            .await?;
        }

        // None of these owns an artifact: the early return above guarantees it.
        sqlx::query!(
            "DELETE FROM segments WHERE corpus_id = ? AND idx >= ?",
            corpus_id,
            windows.len() as i64
        )
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    pub async fn segments_for_corpus(&self, corpus_id: &str) -> Result<Vec<Segment>> {
        let rows = sqlx::query_as!(
            crate::store::segments::SegmentRow,
            "SELECT * FROM segments WHERE corpus_id = ? ORDER BY idx",
            corpus_id
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_segment).collect())
    }

    /// `failed` is included: it records a transient error, not a verdict on the text.
    /// `verbatim` is owed nothing.
    pub async fn pending_segments(&self, corpus_id: &str) -> Result<Vec<Segment>> {
        let rows = sqlx::query_as!(
            crate::store::segments::SegmentRow,
            r#"SELECT corpus_id AS "corpus_id!", idx AS "idx!", start_line AS "start_line!", end_line AS "end_line!", text AS "text!", carry_lines AS "carry_lines!", state AS "state!", keep_artifacts AS "keep_artifacts!", no_promote AS "no_promote!", attempts AS "attempts!", last_error AS "last_error?" FROM segments
             WHERE corpus_id = ? AND state NOT IN ('done', 'verbatim') ORDER BY idx"#,
            corpus_id
        ).fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_segment).collect())
    }

    pub async fn set_segment_state(
        &self,
        corpus_id: &str,
        idx: i64,
        state: SegmentState,
        last_error: Option<&str>,
    ) -> Result<()> {
        // `keep_artifacts` is spent in the same statement as `done`: the steps after the write can
        // fail and retry, and a retry with the mark gone would delete the curated originals.
        sqlx::query!(
            "UPDATE segments SET state = ?, last_error = ?,
                    keep_artifacts = CASE WHEN ? = 'done' THEN 0 ELSE keep_artifacts END
             WHERE corpus_id = ? AND idx = ?",
            state.as_str(),
            last_error,
            state.as_str(),
            corpus_id,
            idx
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn segment_state(&self, corpus_id: &str, idx: i64) -> Result<Option<SegmentState>> {
        Ok(sqlx::query_scalar!(
            "SELECT state FROM segments WHERE corpus_id = ? AND idx = ?",
            corpus_id,
            idx
        )
        .fetch_optional(&self.pool)
        .await?
        .map(|s| SegmentState::parse(&s)))
    }

    /// An operator who undid its promotion said no; only a re-split resets it.
    pub async fn segment_no_promote(&self, corpus_id: &str, idx: i64) -> Result<bool> {
        Ok(sqlx::query_scalar!(
            "SELECT no_promote FROM segments WHERE corpus_id = ? AND idx = ?",
            corpus_id,
            idx
        )
        .fetch_optional(&self.pool)
        .await?
        .is_some_and(|v| v != 0))
    }

    /// Refuse this window to promotion, for good. Set by `undo_promotion`.
    pub async fn set_segment_no_promote(&self, corpus_id: &str, idx: i64) -> Result<()> {
        sqlx::query!(
            "UPDATE segments SET no_promote = 1 WHERE corpus_id = ? AND idx = ?",
            corpus_id,
            idx
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Only `pending` rows: a `done` or `failed` window has a history this must not erase.
    pub async fn mark_segments_verbatim(&self, corpus_id: &str) -> Result<()> {
        sqlx::query!(
            "UPDATE segments SET state = 'verbatim' WHERE corpus_id = ? AND state = 'pending'",
            corpus_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// `keep_artifacts`: a wrong read replaces its artifacts; a re-read for missed lines adds to
    /// them, keeping what an operator may have edited. See `window::write_segment_artifacts`.
    pub async fn reset_segment(
        &self,
        corpus_id: &str,
        idx: i64,
        keep_artifacts: bool,
    ) -> Result<()> {
        sqlx::query!(
            "UPDATE segments SET state = 'pending', attempts = 0, last_error = NULL,
                    keep_artifacts = ?
             WHERE corpus_id = ? AND idx = ?",
            keep_artifacts as i64,
            corpus_id,
            idx
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The re-read mark or a verbatim state; every writer must use this, or a verbatim window
    /// takes the replacing branch and deletes its passages.
    pub async fn segment_write_appends(&self, corpus_id: &str, idx: i64) -> Result<bool> {
        Ok(self.segment_keeps_artifacts(corpus_id, idx).await?
            || self.segment_state(corpus_id, idx).await? == Some(SegmentState::Verbatim))
    }

    pub async fn segment_keeps_artifacts(&self, corpus_id: &str, idx: i64) -> Result<bool> {
        Ok(sqlx::query_scalar!(
            "SELECT keep_artifacts FROM segments WHERE corpus_id = ? AND idx = ?",
            corpus_id,
            idx
        )
        .fetch_optional(&self.pool)
        .await?
        .unwrap_or(0)
            != 0)
    }

    /// Resolved counts both clean and given-up windows.
    pub async fn segment_progress(&self, corpus_id: &str) -> Result<(i64, i64)> {
        let row = sqlx::query!(
            "SELECT COUNT(*) AS total,
                    SUM(CASE WHEN state <> 'pending' THEN 1 ELSE 0 END) AS resolved
             FROM segments WHERE corpus_id = ?",
            corpus_id
        )
        .fetch_one(&self.pool)
        .await?;
        let total: i64 = row.total;
        let resolved: Option<i64> = row.resolved;
        Ok((resolved.unwrap_or(0), total))
    }

    /// Re-segmenting has to re-window: the text or the model's budget may have changed.
    pub async fn clear_segments(&self, corpus_id: &str) -> Result<()> {
        sqlx::query!("DELETE FROM segments WHERE corpus_id = ?", corpus_id)
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
        }
    }

    #[tokio::test]
    async fn the_split_waits_for_a_writer_instead_of_failing_busy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("segments.db");
        let s = Store::connect(
            &path.to_string_lossy(),
            {
                let c = crate::store::control::Control::memory().await.unwrap();
                c.provision(crate::store::TEST_SUBJECT, None).await.unwrap();
                c
            },
            crate::store::TEST_SUBJECT,
        )
        .await
        .unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let other = s.insert_corpus("other", "web", None).await.unwrap();

        let mut writer = s.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
        sqlx::query!(
            "UPDATE corpora SET updated_at = updated_at + 1 WHERE id = ?",
            &other.id
        )
        .execute(&mut *writer)
        .await
        .unwrap();

        let split = {
            let s = s.clone();
            let id = src.id.clone();
            tokio::spawn(async move { s.upsert_segments(&id, &[seg(1, 10, "window 0")]).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        writer.commit().await.unwrap();

        split.await.unwrap().unwrap();
        assert_eq!(s.segments_for_corpus(&src.id).await.unwrap().len(), 1);
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
        // Window 0 stays done at 1-10; a re-split starting window 1 at 16 would skip lines 11-15.
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
        // A failed window leaves no artifacts, so it does not settle the split.
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
        // A kill between artifact commit and `done` leaves a `pending` window that owns artifacts;
        // re-splitting under it would orphan them.
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        s.upsert_segments(&src.id, &[seg(1, 10, "window 0"), seg(11, 20, "window 1")])
            .await
            .unwrap();
        let a = crate::store::artifacts::NewArtifact {
            text: "what window 1 produced".into(),
            segment_idx: Some(1),
            ..Default::default()
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
                },
                NewSegment {
                    start_line: 6,
                    end_line: 9,
                    text: "b",
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
