use super::Store;
use crate::error::Result;
use sqlx::Row;

/// Where one window of a source stands. `Fallback` means the chunker never
/// succeeded here and the lines were split structurally instead — worse than
/// an LLM split, and the reason the source ends up `partial`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WindowState {
    Pending,
    Done,
    Fallback,
}

impl WindowState {
    pub fn as_str(&self) -> &'static str {
        match self {
            WindowState::Pending => "pending",
            WindowState::Done => "done",
            WindowState::Fallback => "fallback",
        }
    }
    pub fn parse(s: &str) -> WindowState {
        match s {
            "done" => WindowState::Done,
            "fallback" => WindowState::Fallback,
            _ => WindowState::Pending,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SegmentWindow {
    pub source_id: String,
    pub idx: i64,
    pub start_line: i64,
    pub end_line: i64,
    pub state: WindowState,
    pub attempts: i64,
    pub last_error: Option<String>,
}

fn row_to_window(r: &sqlx::sqlite::SqliteRow) -> SegmentWindow {
    SegmentWindow {
        source_id: r.get("source_id"),
        idx: r.get("idx"),
        start_line: r.get("start_line"),
        end_line: r.get("end_line"),
        state: WindowState::parse(r.get::<String, _>("state").as_str()),
        attempts: r.get("attempts"),
        last_error: r.get("last_error"),
    }
}

impl Store {
    /// Record the windowing of a source. Idempotent by design: a retried job
    /// re-derives the same spans and must not undo the windows that finished.
    pub async fn upsert_windows(&self, source_id: &str, spans: &[(i64, i64)]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for (idx, (start, end)) in spans.iter().enumerate() {
            sqlx::query(
                "INSERT INTO segment_windows (source_id, idx, start_line, end_line)
                 VALUES (?, ?, ?, ?)
                 ON CONFLICT(source_id, idx) DO NOTHING",
            )
            .bind(source_id)
            .bind(idx as i64)
            .bind(start)
            .bind(end)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn windows_for_source(&self, source_id: &str) -> Result<Vec<SegmentWindow>> {
        let rows = sqlx::query("SELECT * FROM segment_windows WHERE source_id = ? ORDER BY idx")
            .bind(source_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(row_to_window).collect())
    }

    pub async fn pending_windows(&self, source_id: &str) -> Result<Vec<SegmentWindow>> {
        let rows = sqlx::query(
            "SELECT * FROM segment_windows
             WHERE source_id = ? AND state = 'pending' ORDER BY idx",
        )
        .bind(source_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_window).collect())
    }

    pub async fn set_window_state(
        &self,
        source_id: &str,
        idx: i64,
        state: WindowState,
        last_error: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE segment_windows SET state = ?, last_error = ?
             WHERE source_id = ? AND idx = ?",
        )
        .bind(state.as_str())
        .bind(last_error)
        .bind(source_id)
        .bind(idx)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn bump_window_attempts(&self, source_id: &str, idx: i64) -> Result<i64> {
        sqlx::query(
            "UPDATE segment_windows SET attempts = attempts + 1
             WHERE source_id = ? AND idx = ?",
        )
        .bind(source_id)
        .bind(idx)
        .execute(&self.pool)
        .await?;
        let row =
            sqlx::query("SELECT attempts FROM segment_windows WHERE source_id = ? AND idx = ?")
                .bind(source_id)
                .bind(idx)
                .fetch_one(&self.pool)
                .await?;
        Ok(row.get("attempts"))
    }

    /// Put a window back in the queue's path. The operator's "re-segment this
    /// window" button, and nothing else, calls this.
    pub async fn reset_window(&self, source_id: &str, idx: i64) -> Result<()> {
        sqlx::query(
            "UPDATE segment_windows SET state = 'pending', attempts = 0, last_error = NULL
             WHERE source_id = ? AND idx = ?",
        )
        .bind(source_id)
        .bind(idx)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// `(resolved, total)`, where resolved counts both a clean window and one
    /// that gave up and split structurally.
    pub async fn window_progress(&self, source_id: &str) -> Result<(i64, i64)> {
        let row = sqlx::query(
            "SELECT COUNT(*) AS total,
                    SUM(CASE WHEN state <> 'pending' THEN 1 ELSE 0 END) AS resolved
             FROM segment_windows WHERE source_id = ?",
        )
        .bind(source_id)
        .fetch_one(&self.pool)
        .await?;
        let total: i64 = row.get("total");
        let resolved: Option<i64> = row.get("resolved");
        Ok((resolved.unwrap_or(0), total))
    }

    /// Drop the windowing entirely. Re-segmenting a source from scratch has to
    /// re-window it, because the text or the model's budget may have changed.
    pub async fn clear_windows(&self, source_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM segment_windows WHERE source_id = ?")
            .bind(source_id)
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
        let src = s.insert_source("raw", "web", None).await.unwrap();

        s.upsert_windows(&src.id, &[(1, 10), (11, 20), (21, 30)])
            .await
            .unwrap();
        assert_eq!(s.windows_for_source(&src.id).await.unwrap().len(), 3);

        s.set_window_state(&src.id, 0, WindowState::Done, None)
            .await
            .unwrap();

        // A second call must not reset the window that already finished.
        s.upsert_windows(&src.id, &[(1, 10), (11, 20), (21, 30)])
            .await
            .unwrap();

        let pending = s.pending_windows(&src.id).await.unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].idx, 1);
        assert_eq!(pending[0].start_line, 11);
    }

    #[tokio::test]
    async fn progress_counts_done_and_fallback_as_resolved() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_source("raw", "web", None).await.unwrap();
        s.upsert_windows(&src.id, &[(1, 5), (6, 10), (11, 15)])
            .await
            .unwrap();

        s.set_window_state(&src.id, 0, WindowState::Done, None)
            .await
            .unwrap();
        s.set_window_state(&src.id, 1, WindowState::Fallback, Some("endpoint down"))
            .await
            .unwrap();

        assert_eq!(s.window_progress(&src.id).await.unwrap(), (2, 3));
        let w = s.windows_for_source(&src.id).await.unwrap();
        assert_eq!(w[1].last_error.as_deref(), Some("endpoint down"));
    }

    #[tokio::test]
    async fn attempts_accumulate_and_a_reset_clears_them() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_source("raw", "web", None).await.unwrap();
        s.upsert_windows(&src.id, &[(1, 5)]).await.unwrap();

        assert_eq!(s.bump_window_attempts(&src.id, 0).await.unwrap(), 1);
        assert_eq!(s.bump_window_attempts(&src.id, 0).await.unwrap(), 2);
        s.set_window_state(&src.id, 0, WindowState::Fallback, Some("boom"))
            .await
            .unwrap();

        s.reset_window(&src.id, 0).await.unwrap();
        let w = &s.windows_for_source(&src.id).await.unwrap()[0];
        assert_eq!(w.state, WindowState::Pending);
        assert_eq!(w.attempts, 0);
        assert_eq!(w.last_error, None);
    }

    #[tokio::test]
    async fn deleting_a_source_cascades_to_its_windows() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_source("raw", "web", None).await.unwrap();
        s.upsert_windows(&src.id, &[(1, 5)]).await.unwrap();
        s.delete_source(&src.id).await.unwrap();
        assert!(s.windows_for_source(&src.id).await.unwrap().is_empty());
    }
}
