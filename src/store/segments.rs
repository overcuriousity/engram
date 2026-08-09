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
    pub state: SegmentState,
    pub attempts: i64,
    pub last_error: Option<String>,
}

fn row_to_segment(r: &sqlx::sqlite::SqliteRow) -> Segment {
    Segment {
        corpus_id: r.get("corpus_id"),
        idx: r.get("idx"),
        start_line: r.get("start_line"),
        end_line: r.get("end_line"),
        state: SegmentState::parse(r.get::<String, _>("state").as_str()),
        attempts: r.get("attempts"),
        last_error: r.get("last_error"),
    }
}

impl Store {
    /// Record the windowing of a source. Idempotent by design: a retried job
    /// re-derives the same spans and must not undo the windows that finished.
    pub async fn upsert_segments(&self, corpus_id: &str, spans: &[(i64, i64)]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for (idx, (start, end)) in spans.iter().enumerate() {
            sqlx::query(
                "INSERT INTO segments (corpus_id, idx, start_line, end_line)
                 VALUES (?, ?, ?, ?)
                 ON CONFLICT(corpus_id, idx) DO NOTHING",
            )
            .bind(corpus_id)
            .bind(idx as i64)
            .bind(start)
            .bind(end)
            .execute(&mut *tx)
            .await?;
        }
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

    pub async fn pending_segments(&self, corpus_id: &str) -> Result<Vec<Segment>> {
        let rows = sqlx::query(
            "SELECT * FROM segments
             WHERE corpus_id = ? AND state = 'pending' ORDER BY idx",
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

    #[tokio::test]
    async fn windows_are_upserted_once_and_resume_reads_only_the_pending_ones() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();

        s.upsert_segments(&src.id, &[(1, 10), (11, 20), (21, 30)])
            .await
            .unwrap();
        assert_eq!(s.segments_for_corpus(&src.id).await.unwrap().len(), 3);

        s.set_segment_state(&src.id, 0, SegmentState::Done, None)
            .await
            .unwrap();

        // A second call must not reset the window that already finished.
        s.upsert_segments(&src.id, &[(1, 10), (11, 20), (21, 30)])
            .await
            .unwrap();

        let pending = s.pending_segments(&src.id).await.unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].idx, 1);
        assert_eq!(pending[0].start_line, 11);
    }

    #[tokio::test]
    async fn progress_counts_done_and_failed_as_resolved() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        s.upsert_segments(&src.id, &[(1, 5), (6, 10), (11, 15)])
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
        s.upsert_segments(&src.id, &[(1, 5)]).await.unwrap();

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
        s.upsert_segments(&src.id, &[(1, 5)]).await.unwrap();
        s.delete_corpus(&src.id).await.unwrap();
        assert!(s.segments_for_corpus(&src.id).await.unwrap().is_empty());
    }
}
