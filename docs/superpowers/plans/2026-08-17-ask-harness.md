# Ask Harness, Ask Feedback and Knowledge Gaps — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Record every question asked on `/ui/ask`, let the operator judge it on the page (right / wrong / nothing here, plus which citations carried it), export judged questions beside judged searches, measure citation recall, abstention and faithfulness in a second ignored harness, and group unanswered questions and gap searches into named knowledge gaps on the capture page.

**Architecture:** A new `store/asks.rs` holds ask events and their citations; `Core::ask` gains an origin and records UI asks synchronously; the answer page grows a verdict bar and carrier toggles served by two HTMX routes. `eval/` gains a third frozen file and pure metrics; `tests/eval.rs` gains `evaluate_ask`. Gaps are read from both event tables, clustered by a pure single-linkage function over stored vectors, named once per cluster by an efficient-tier completer on the existing retention ticker, and rendered on the capture page.

**Tech Stack:** Rust 2024, axum 0.8, askama 0.16, sqlx 0.9 (SQLite), tokio, serde_json, sha2/hex. Tests: `cargo test` (unit + in-process oneshot routes), `cargo test --test eval -- --ignored` (real endpoints).

**Spec:** `docs/superpowers/specs/2026-08-17-ask-harness-design.md`

## Global Constraints

- Branch: `feat/ask-harness` (already exists, contains the spec). Commit after every task; `cargo fmt`, `cargo clippy --all-targets` and `cargo test` must be clean before each commit.
- Only `Door::Ui` asks are recorded, and only when `feedback.enabled` (spec §5).
- No new configuration keys. Constants: `ABSTAIN_PREFIX = "Not in the knowledge base"`, `GAP_LINK_AT = 0.55` (spec §6, §10.2).
- No inference on any request path. The gap namer runs in the background under `core.gate.background()`; the claim check runs only in the harness with `ENGRAM_EVAL_CLAIMS=1` (spec §3, §9).
- Nothing rewrites an artifact or an answer; verdicts are kept through expiry, unjudged asks age out with `feedback.retain_days` (spec §7.4).
- House style: doc comments say *why*, test names are sentences (`a_ui_ask_is_recorded_with_its_citations`), errors are `crate::error::Error`, ids come from `crate::store::new_id()`, timestamps from `crate::store::now()`.
- Existing helpers to reuse, not reinvent: `crate::store::feedback::{Door, Origin, blob_to_vec}`, `crate::infer::verify::missing_literals`, `crate::infer::prompt::{extract_json, unwrap_verdict}`, `crate::web::test_support::{app_with_cookie, body_of}`, and in `ui.rs` tests `app_session_and_core_with_feedback`, `form`, `get_body`.

---

## File map

| File | Responsibility |
|---|---|
| `src/store/schema.sql` | `ask_events`, `ask_citations`, `gap_clusters` tables |
| `src/store/mod.rs` | `pub mod asks; pub mod gaps;`, `("search_events","dismissed_at","INTEGER")` in `ADDED_COLUMNS` |
| `src/store/asks.rs` (new) | `NewAsk`, `AskVerdict`, `AskEvent`, `AskCitation`, `AskStats`; record / judge / unjudge / toggle_carried / ask_event / ask_stats / expire_asks / purge_asks |
| `src/store/feedback.rs` | `expire_feedback` and `purge_feedback` also call the ask hooks |
| `src/store/gaps.rs` (new) | `Gap`, `GapKind`, `open_gaps`, `dismiss_gap`, `GapCluster`, `replace_clusters`, `clusters`, `unclustered_gaps` |
| `src/infer/prompt.rs` | `ABSTAIN_PREFIX`, `abstained()`, `ASK_SYSTEM` wording, `CLAIMS_SYSTEM`/`claims_prompt`/`claims_schema`, `GAP_LABEL_SYSTEM`/`gap_label_prompt`/`gap_label_schema` |
| `src/infer/openai.rs` | `HttpCompleter::for_claim_checking`, `HttpCompleter::for_gap_naming` |
| `src/core/mod.rs` | `gap_namer: Arc<dyn Completer>` on `Core` |
| `src/core/ask.rs` | origin parameter, recording, `abstained`, `event_id` |
| `src/core/search.rs` | `Core::cached_query_vector` |
| `src/core/gaps.rs` (new) | `cluster()`, `cosine()`, `terms_label()`, `cluster_key()` |
| `src/jobs/gaps.rs` (new) | `sweep(core)`: cluster → diff → name new clusters |
| `src/core/background.rs` | retention ticker also runs the gap sweep |
| `src/eval/mod.rs` | `EvalQuestion`, `questions_path`, `load_questions`, `save_questions` |
| `src/eval/export.rs` | writes `questions.json`; returns three counts |
| `src/eval/metrics.rs` | `fraction_cited`, `Abstention`, `fully_supported` |
| `src/eval/claims.rs` (new) | `ClaimCheck`, `parse_claims` |
| `src/main.rs` | prints the third count |
| `src/web/ui.rs` | `AnswerTemplate` fields, verdict/carried/dismiss routes, capture-page gaps, `?q=` prefill |
| `src/web/templates/_answer.html`, `_ask_verdict.html` (new), `_ask_carried.html` (new), `_gaps.html` (new), `capture.html`, `ask.html`, `judge.html` | markup |
| `src/web/judge.rs` | ask stats line on the judge page |
| `src/web/api.rs`, `src/mcp/mod.rs` | pass `Door::Api` / `Door::Mcp` |
| `tests/eval.rs` | `evaluate_ask`, `gap_namer` in the Core literal |
| `README.md`, `ROADMAP.md` | docs |

---

### Task 1: Ask events in the store

**Files:**
- Modify: `src/store/schema.sql` (append after the `search_candidates` table)
- Modify: `src/store/mod.rs:1-11` (module list)
- Create: `src/store/asks.rs`
- Modify: `src/store/feedback.rs:641-665` (`expire_feedback`, `purge_feedback`)

**Interfaces:**
- Produces:
  ```rust
  pub struct NewAsk { pub question: String, pub scope: Option<String>, pub filters: String,
      pub query_vec: Vec<f32>, pub embed_model: String, pub answer: String, pub abstained: bool,
      pub dropped: usize, pub truncated: bool, pub citations: Vec<NewAskCitation> }
  pub struct NewAskCitation { pub artifact_id: String, pub score: f32 }
  pub enum AskVerdict { Right, Wrong, NothingHere }   // as_str: "right" | "wrong" | "nothing_here"; parse(&str) -> Option<Self>
  pub struct AskCitation { pub n: i64, pub artifact_id: String, pub score: f32, pub carried: bool }
  pub struct AskEvent { pub id: String, pub question: String, pub answer: String, pub abstained: bool,
      pub verdict: Option<AskVerdict>, pub judged_at: Option<i64>, pub citations: Vec<AskCitation> }
  pub struct AskStats { pub asked: i64, pub judged: i64, pub right: i64, pub wrong: i64, pub nothing_here: i64 }
  impl Store {
      pub async fn record_ask(&self, ask: NewAsk) -> Result<String>;
      pub async fn ask_event(&self, id: &str) -> Result<Option<AskEvent>>;
      pub async fn judge_ask(&self, id: &str, verdict: AskVerdict) -> Result<()>;   // NotFound if no row
      pub async fn unjudge_ask(&self, id: &str) -> Result<()>;                      // clears verdict AND carriers
      pub async fn toggle_carried(&self, id: &str, n: i64) -> Result<bool>;         // returns new state; sets verdict Right if unjudged
      pub async fn ask_stats(&self) -> Result<AskStats>;
      pub async fn expire_asks(&self, retain_days: i64) -> Result<u64>;
      pub async fn purge_asks(&self) -> Result<u64>;
  }
  ```

- [ ] **Step 1: Add the tables to `schema.sql`**

Append after the `search_candidates` table (before `-- ── Association`):

```sql
-- ── Ask feedback ─────────────────────────────────────────────────────────────
-- A question asked on the page, the answer it got and the excerpts the model
-- was shown — so a verdict given later can be scored against exactly what
-- happened. Only the UI door records; see `Core::ask`.
CREATE TABLE IF NOT EXISTS ask_events (
  id           TEXT PRIMARY KEY,
  question     TEXT NOT NULL,
  scope        TEXT,
  filters      TEXT NOT NULL DEFAULT '{}',
  -- Stored so a "nothing here" can be clustered with other gaps later without
  -- paying for the embedding again.
  query_vec    BLOB NOT NULL,
  vec_dim      INTEGER NOT NULL,
  embed_model  TEXT NOT NULL,
  answer       TEXT NOT NULL,
  abstained    INTEGER NOT NULL,
  dropped      INTEGER NOT NULL,
  truncated    INTEGER NOT NULL,
  created_at   INTEGER NOT NULL,
  judged_at    INTEGER,
  verdict      TEXT,
  -- Set when the operator says a "nothing here" gap has since been covered.
  dismissed_at INTEGER
);
CREATE INDEX IF NOT EXISTS idx_asks_verdict ON ask_events(verdict, dismissed_at);
CREATE INDEX IF NOT EXISTS idx_asks_created ON ask_events(created_at);

CREATE TABLE IF NOT EXISTS ask_citations (
  event_id    TEXT NOT NULL REFERENCES ask_events(id) ON DELETE CASCADE,
  -- The [n] the model was shown, 1-based, in the order it was shown.
  n           INTEGER NOT NULL,
  artifact_id TEXT NOT NULL,
  score       REAL NOT NULL,
  -- The operator said this excerpt carried the answer.
  carried     INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (event_id, n)
);
```

- [ ] **Step 2: Register the module**

In `src/store/mod.rs` add `pub mod asks;` between `pub mod artifacts;` and `pub mod attachments;`.

- [ ] **Step 3: Write the failing store tests**

Create `src/store/asks.rs` with only the module doc, the imports and the tests module (implementation comes in step 5):

```rust
//! What a question looked like, so it can be judged later.
//!
//! The search side keeps the query and the verdict apart in time; a question
//! is judged where it is answered, because judging an answer means reading it
//! in context. What still has to be recorded in the moment is the answer and
//! the excerpts the model saw — the verdict is about *those*, and neither can
//! be reconstructed afterwards.

use super::{Store, new_id, now};
use crate::error::{Error, Result};
use sqlx::Row;

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(question: &str, citations: usize) -> NewAsk {
        NewAsk {
            question: question.into(),
            scope: Some("me".into()),
            filters: "{}".into(),
            query_vec: vec![0.1, 0.2, 0.3],
            embed_model: "fake".into(),
            answer: "an answer".into(),
            abstained: false,
            dropped: 0,
            truncated: false,
            citations: (0..citations)
                .map(|i| NewAskCitation { artifact_id: format!("art-{i}"), score: 1.0 - i as f32 * 0.1 })
                .collect(),
        }
    }

    #[tokio::test]
    async fn a_recorded_ask_comes_back_with_its_citations_in_shown_order() {
        let store = Store::memory().await.unwrap();
        let id = store.record_ask(ask("how", 3)).await.unwrap();
        let ev = store.ask_event(&id).await.unwrap().expect("recorded");
        assert_eq!(ev.question, "how");
        assert_eq!(ev.answer, "an answer");
        assert!(ev.verdict.is_none());
        assert_eq!(
            ev.citations.iter().map(|c| (c.n, c.artifact_id.as_str(), c.carried)).collect::<Vec<_>>(),
            vec![(1, "art-0", false), (2, "art-1", false), (3, "art-2", false)]
        );
    }

    #[tokio::test]
    async fn an_unknown_ask_is_none_not_an_error() {
        let store = Store::memory().await.unwrap();
        assert!(store.ask_event("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn judging_records_the_verdict_and_unjudging_takes_it_back_with_the_carriers() {
        let store = Store::memory().await.unwrap();
        let id = store.record_ask(ask("how", 2)).await.unwrap();
        store.judge_ask(&id, AskVerdict::Wrong).await.unwrap();
        let ev = store.ask_event(&id).await.unwrap().unwrap();
        assert_eq!(ev.verdict, Some(AskVerdict::Wrong));
        assert!(ev.judged_at.is_some());

        assert!(store.toggle_carried(&id, 1).await.unwrap());
        store.unjudge_ask(&id).await.unwrap();
        let ev = store.ask_event(&id).await.unwrap().unwrap();
        assert!(ev.verdict.is_none() && ev.judged_at.is_none());
        assert!(
            ev.citations.iter().all(|c| !c.carried),
            "a carrier left behind would count towards recall for a verdict nobody stands behind"
        );
    }

    #[tokio::test]
    async fn judging_an_unknown_ask_is_not_found() {
        let store = Store::memory().await.unwrap();
        assert!(matches!(store.judge_ask("nope", AskVerdict::Right).await, Err(Error::NotFound)));
        assert!(matches!(store.toggle_carried("nope", 1).await, Err(Error::NotFound)));
    }

    #[tokio::test]
    async fn marking_a_carrier_on_an_unjudged_ask_makes_it_right() {
        let store = Store::memory().await.unwrap();
        let id = store.record_ask(ask("how", 2)).await.unwrap();
        assert!(store.toggle_carried(&id, 2).await.unwrap());
        let ev = store.ask_event(&id).await.unwrap().unwrap();
        assert_eq!(ev.verdict, Some(AskVerdict::Right));
        assert_eq!(ev.citations.iter().filter(|c| c.carried).map(|c| c.n).collect::<Vec<_>>(), vec![2]);

        // Toggling again turns it off and leaves the verdict alone.
        assert!(!store.toggle_carried(&id, 2).await.unwrap());
        let ev = store.ask_event(&id).await.unwrap().unwrap();
        assert_eq!(ev.verdict, Some(AskVerdict::Right));
        assert!(ev.citations.iter().all(|c| !c.carried));
    }

    #[tokio::test]
    async fn a_carrier_on_a_wrong_answer_does_not_flip_the_verdict() {
        let store = Store::memory().await.unwrap();
        let id = store.record_ask(ask("how", 1)).await.unwrap();
        store.judge_ask(&id, AskVerdict::Wrong).await.unwrap();
        store.toggle_carried(&id, 1).await.unwrap();
        assert_eq!(store.ask_event(&id).await.unwrap().unwrap().verdict, Some(AskVerdict::Wrong));
    }

    #[tokio::test]
    async fn stats_count_what_was_asked_and_how_it_was_judged() {
        let store = Store::memory().await.unwrap();
        let a = store.record_ask(ask("a", 1)).await.unwrap();
        let b = store.record_ask(ask("b", 1)).await.unwrap();
        store.record_ask(ask("c", 1)).await.unwrap();
        store.judge_ask(&a, AskVerdict::Right).await.unwrap();
        store.judge_ask(&b, AskVerdict::NothingHere).await.unwrap();
        let s = store.ask_stats().await.unwrap();
        assert_eq!((s.asked, s.judged, s.right, s.wrong, s.nothing_here), (3, 2, 1, 0, 1));
    }

    #[tokio::test]
    async fn expiry_takes_unjudged_asks_past_the_window_and_keeps_judged_ones() {
        let store = Store::memory().await.unwrap();
        let old_unjudged = store.record_ask(ask("a", 1)).await.unwrap();
        let old_judged = store.record_ask(ask("b", 1)).await.unwrap();
        let fresh = store.record_ask(ask("c", 1)).await.unwrap();
        store.judge_ask(&old_judged, AskVerdict::Right).await.unwrap();
        // Age two of them past a 30-day window.
        for id in [&old_unjudged, &old_judged] {
            sqlx::query("UPDATE ask_events SET created_at = ? WHERE id = ?")
                .bind(now() - 31 * 86_400)
                .bind(id)
                .execute(&store.pool)
                .await
                .unwrap();
        }
        assert_eq!(store.expire_asks(30).await.unwrap(), 1);
        assert!(store.ask_event(&old_unjudged).await.unwrap().is_none());
        assert!(store.ask_event(&old_judged).await.unwrap().is_some());
        assert!(store.ask_event(&fresh).await.unwrap().is_some());
        // Zero means keep forever.
        assert_eq!(store.expire_asks(0).await.unwrap(), 0);
        // Citations go with the event.
        let orphans: i64 = sqlx::query_scalar("SELECT count(*) FROM ask_citations WHERE event_id = ?")
            .bind(&old_unjudged)
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(orphans, 0);
    }

    #[tokio::test]
    async fn purge_takes_everything_judged_or_not() {
        let store = Store::memory().await.unwrap();
        let a = store.record_ask(ask("a", 1)).await.unwrap();
        store.record_ask(ask("b", 1)).await.unwrap();
        store.judge_ask(&a, AskVerdict::Right).await.unwrap();
        assert_eq!(store.purge_asks().await.unwrap(), 2);
        assert_eq!(store.ask_stats().await.unwrap().asked, 0);
    }
}
```

- [ ] **Step 4: Run to verify it fails**

Run: `cargo test --lib store::asks 2>&1 | tail -5`
Expected: compile errors — `NewAsk`, `record_ask` etc. not found.

- [ ] **Step 5: Implement the store module**

Insert between the imports and `#[cfg(test)]` in `src/store/asks.rs`:

```rust
#[derive(Debug, Clone)]
pub struct NewAskCitation {
    pub artifact_id: String,
    pub score: f32,
}

#[derive(Debug, Clone)]
pub struct NewAsk {
    pub question: String,
    /// The authenticated subject. Recorded, never used for coalescing: a
    /// question is one deliberate act, not a typing burst.
    pub scope: Option<String>,
    /// JSON, as `search_events.filters` is.
    pub filters: String,
    /// The vector ask retrieved with. May be empty when the query cache had
    /// already evicted it; such an event is never clustered as a gap.
    pub query_vec: Vec<f32>,
    pub embed_model: String,
    pub answer: String,
    pub abstained: bool,
    pub dropped: usize,
    pub truncated: bool,
    /// In the order the model saw them; `n` is assigned 1-based from it.
    pub citations: Vec<NewAskCitation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskVerdict {
    /// The answer is correct as stated.
    Right,
    /// The base holds the answer and this is not it.
    Wrong,
    /// The base does not hold the answer, whatever the model said.
    NothingHere,
}

impl AskVerdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            AskVerdict::Right => "right",
            AskVerdict::Wrong => "wrong",
            AskVerdict::NothingHere => "nothing_here",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "right" => Some(AskVerdict::Right),
            "wrong" => Some(AskVerdict::Wrong),
            "nothing_here" => Some(AskVerdict::NothingHere),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AskCitation {
    pub n: i64,
    pub artifact_id: String,
    pub score: f32,
    pub carried: bool,
}

#[derive(Debug, Clone)]
pub struct AskEvent {
    pub id: String,
    pub question: String,
    pub answer: String,
    pub abstained: bool,
    pub verdict: Option<AskVerdict>,
    pub judged_at: Option<i64>,
    pub citations: Vec<AskCitation>,
}

#[derive(Debug, Clone, Default)]
pub struct AskStats {
    pub asked: i64,
    pub judged: i64,
    pub right: i64,
    pub wrong: i64,
    pub nothing_here: i64,
}

fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn one_row(res: sqlx::sqlite::SqliteQueryResult) -> Result<()> {
    if res.rows_affected() == 0 {
        return Err(Error::NotFound);
    }
    Ok(())
}

impl Store {
    pub async fn record_ask(&self, ask: NewAsk) -> Result<String> {
        let mut tx = self.pool.begin().await?;
        let id = new_id();
        sqlx::query(
            "INSERT INTO ask_events
               (id, question, scope, filters, query_vec, vec_dim, embed_model, answer,
                abstained, dropped, truncated, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&ask.question)
        .bind(&ask.scope)
        .bind(&ask.filters)
        .bind(vec_to_blob(&ask.query_vec))
        .bind(ask.query_vec.len() as i64)
        .bind(&ask.embed_model)
        .bind(&ask.answer)
        .bind(ask.abstained as i64)
        .bind(ask.dropped as i64)
        .bind(ask.truncated as i64)
        .bind(now())
        .execute(&mut *tx)
        .await?;
        for (i, c) in ask.citations.iter().enumerate() {
            sqlx::query(
                "INSERT INTO ask_citations (event_id, n, artifact_id, score) VALUES (?, ?, ?, ?)",
            )
            .bind(&id)
            .bind(i as i64 + 1)
            .bind(&c.artifact_id)
            .bind(c.score)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(id)
    }

    pub async fn ask_event(&self, id: &str) -> Result<Option<AskEvent>> {
        let Some(row) = sqlx::query(
            "SELECT id, question, answer, abstained, verdict, judged_at FROM ask_events WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(None);
        };
        let citations = sqlx::query(
            "SELECT n, artifact_id, score, carried FROM ask_citations WHERE event_id = ? ORDER BY n",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|r| AskCitation {
            n: r.get("n"),
            artifact_id: r.get("artifact_id"),
            score: r.get::<f64, _>("score") as f32,
            carried: r.get::<i64, _>("carried") != 0,
        })
        .collect();
        Ok(Some(AskEvent {
            id: row.get("id"),
            question: row.get("question"),
            answer: row.get("answer"),
            abstained: row.get::<i64, _>("abstained") != 0,
            verdict: row
                .get::<Option<String>, _>("verdict")
                .as_deref()
                .and_then(AskVerdict::parse),
            judged_at: row.get("judged_at"),
            citations,
        }))
    }

    pub async fn judge_ask(&self, id: &str, verdict: AskVerdict) -> Result<()> {
        one_row(
            sqlx::query("UPDATE ask_events SET judged_at = ?, verdict = ? WHERE id = ?")
                .bind(now())
                .bind(verdict.as_str())
                .bind(id)
                .execute(&self.pool)
                .await?,
        )
    }

    /// Take a verdict back, carriers included. A carrier left behind would
    /// count towards citation recall for a judgement nobody stands behind.
    pub async fn unjudge_ask(&self, id: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let res = sqlx::query(
            "UPDATE ask_events SET judged_at = NULL, verdict = NULL WHERE id = ?",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        one_row(res)?;
        sqlx::query("UPDATE ask_citations SET carried = 0 WHERE event_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Flip whether citation `n` carried the answer; returns the new state.
    /// Saying an excerpt carried the answer is saying the answer was right, so
    /// an unjudged event becomes `right`. A verdict already given is left as it
    /// is: the toggle refines a verdict, it does not overrule one.
    pub async fn toggle_carried(&self, id: &str, n: i64) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let res = sqlx::query(
            "UPDATE ask_citations SET carried = 1 - carried WHERE event_id = ? AND n = ?",
        )
        .bind(id)
        .bind(n)
        .execute(&mut *tx)
        .await?;
        one_row(res)?;
        let carried: i64 =
            sqlx::query_scalar("SELECT carried FROM ask_citations WHERE event_id = ? AND n = ?")
                .bind(id)
                .bind(n)
                .fetch_one(&mut *tx)
                .await?;
        if carried != 0 {
            sqlx::query(
                "UPDATE ask_events SET judged_at = ?, verdict = 'right'
                 WHERE id = ? AND verdict IS NULL",
            )
            .bind(now())
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(carried != 0)
    }

    pub async fn ask_stats(&self) -> Result<AskStats> {
        let mut s = AskStats {
            asked: sqlx::query_scalar("SELECT count(*) FROM ask_events")
                .fetch_one(&self.pool)
                .await?,
            ..Default::default()
        };
        for (field, verdict) in [
            (&mut s.right, "right"),
            (&mut s.wrong, "wrong"),
            (&mut s.nothing_here, "nothing_here"),
        ] {
            *field = sqlx::query_scalar("SELECT count(*) FROM ask_events WHERE verdict = ?")
                .bind(verdict)
                .fetch_one(&self.pool)
                .await?;
        }
        s.judged = s.right + s.wrong + s.nothing_here;
        Ok(s)
    }

    /// Unjudged questions older than the window. Judged ones are exempt for
    /// the reason judged searches are: they are the operator's own work.
    pub async fn expire_asks(&self, retain_days: i64) -> Result<u64> {
        if retain_days <= 0 {
            return Ok(0);
        }
        Ok(sqlx::query("DELETE FROM ask_events WHERE created_at < ? AND verdict IS NULL")
            .bind(now() - retain_days * 86_400)
            .execute(&self.pool)
            .await?
            .rows_affected())
    }

    pub async fn purge_asks(&self) -> Result<u64> {
        Ok(sqlx::query("DELETE FROM ask_events")
            .execute(&self.pool)
            .await?
            .rows_affected())
    }
}
```

- [ ] **Step 6: Hook expiry and purge into the feedback promise**

In `src/store/feedback.rs`, `expire_feedback`: replace the body's single query with a sum, keeping the `retain_days <= 0` early return:

```rust
        let searches = sqlx::query(
            "DELETE FROM search_events
                 WHERE created_at < ? AND (verdict IS NULL OR verdict = 'discard')",
        )
        .bind(now() - retain_days * 86_400)
        .execute(&self.pool)
        .await?
        .rows_affected();
        // One promise, both tables. A question is the same class of personal
        // data as a query and ages under the same window.
        Ok(searches + self.expire_asks(retain_days).await?)
```

And `purge_feedback`:

```rust
        let searches = sqlx::query("DELETE FROM search_events")
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(searches + self.purge_asks().await?)
```

- [ ] **Step 7: Run the tests**

Run: `cargo test --lib store:: 2>&1 | tail -5`
Expected: all pass, including the existing feedback tests.

- [ ] **Step 8: Commit**

```bash
cargo fmt && cargo clippy --all-targets 2>&1 | grep -E "^(warning|error)" ; git add -A && git commit -m "feat(store): record questions, their citations and their verdicts

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: The abstention sentinel

**Files:**
- Modify: `src/infer/prompt.rs:298-302` (`ASK_SYSTEM`) and add beside it

**Interfaces:**
- Produces: `pub const ABSTAIN_PREFIX: &str = "Not in the knowledge base";` and `pub fn abstained(answer: &str) -> bool`.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module at the bottom of `src/infer/prompt.rs`:

```rust
    #[test]
    fn an_answer_that_opens_with_the_sentinel_is_an_abstention() {
        assert!(abstained("Not in the knowledge base. Nothing covers mounting E01 images."));
        assert!(abstained("  not in the knowledge base — the excerpts are about FAT."));
        // Models wrap the opening in emphasis or a heading; that is still the opening.
        assert!(abstained("**Not in the knowledge base.** The excerpts describe…"));
        assert!(abstained("# Not in the knowledge base\n\nThe excerpts…"));
    }

    #[test]
    fn an_answer_that_merely_mentions_the_phrase_is_not_an_abstention() {
        assert!(!abstained("Mount it with `ewfmount`. (Details on E02 are not in the knowledge base.)"));
        assert!(!abstained(""));
        assert!(!abstained("Not in the manual, but in the excerpts: use -o ro."));
    }

    #[test]
    fn the_system_prompt_tells_the_model_the_exact_sentinel_the_code_reads() {
        assert!(ASK_SYSTEM.contains(ABSTAIN_PREFIX), "{ASK_SYSTEM}");
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib infer::prompt::tests::an_answer_that_opens 2>&1 | tail -3`
Expected: compile error, `abstained` not found.

- [ ] **Step 3: Implement**

Replace `ASK_SYSTEM` and add the constant and function:

```rust
/// The words an abstaining answer opens with. One definition for the string
/// the model is told and the string `abstained` looks for, for the reason
/// `Caveat:` is: splitting the two apart is how the agreement quietly breaks.
pub const ABSTAIN_PREFIX: &str = "Not in the knowledge base";

pub const ASK_SYSTEM: &str = "You answer questions using only the provided knowledge-base excerpts. \
Quote commands, paths and code exactly as they appear. If the excerpts do not contain the answer, \
begin your reply with the exact words `Not in the knowledge base.` and say what is missing rather \
than guessing. Cite excerpts by their number. \
An excerpt may carry lines beginning `Caveat:` — the conditions under which it does not apply. \
Repeat any caveat that bears on your answer rather than dropping it.";

/// Whether an answer opened with `ABSTAIN_PREFIX`. Leading whitespace and
/// markdown emphasis or heading marks are skipped, because models wrap an
/// opening sentence in them no matter what they were told; the comparison is
/// case-insensitive for the same reason. Mentioning the phrase later in a real
/// answer is not an abstention.
pub fn abstained(answer: &str) -> bool {
    let opening = answer.trim_start_matches(|c: char| c.is_whitespace() || matches!(c, '*' | '_' | '#' | '>' | '`'));
    opening
        .get(..ABSTAIN_PREFIX.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(ABSTAIN_PREFIX))
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib infer::prompt 2>&1 | tail -3`
Expected: pass. (If `get(..len)` panics on a non-char boundary in some test string, use `opening.char_indices().nth(ABSTAIN_PREFIX.chars().count()).map(|(i,_)| &opening[..i]).unwrap_or(opening)` — but ASCII prefix on ASCII opening is the case here.)

- [ ] **Step 5: Commit**

```bash
cargo fmt && git add -A && git commit -m "feat(ask): a fixed opening sentence for 'not in the base', and a reader for it

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: `Core::ask` takes an origin and records UI questions

**Files:**
- Modify: `src/core/ask.rs`
- Modify: `src/core/search.rs` (add `cached_query_vector`)
- Modify: `src/web/api.rs:691-697`, `src/web/ui.rs:1567-1595` (`ask_submit`), `src/mcp/mod.rs:147-160`

**Interfaces:**
- Produces:
  ```rust
  impl Core {
      pub async fn ask(&self, req: &AskRequest, origin: impl Into<Origin>) -> Result<AskResponse>;
      pub fn cached_query_vector(&self, q: &str) -> Option<Vec<f32>>;   // in search.rs
  }
  pub struct AskResponse { …existing…, pub abstained: bool, pub event_id: Option<String> }
  ```

- [ ] **Step 1: Add `cached_query_vector` to `src/core/search.rs`**

Inside `impl Core` (just above `pub async fn search_with`):

```rust
    /// The embedding of `q`, if a search just made it. `search_with` caches the
    /// query vector under the whitespace-normalised query; a caller that ran a
    /// search a moment ago and wants to store the vector it used reads it here
    /// rather than paying for the embedding twice. `None` only if the cache
    /// evicted it in between, which a caller must tolerate.
    pub fn cached_query_vector(&self, q: &str) -> Option<Vec<f32>> {
        let key = q.split_whitespace().collect::<Vec<_>>().join(" ");
        self.query_cache.lock().ok().and_then(|c| c.get(&key))
    }
```

- [ ] **Step 2: Write the failing tests in `src/core/ask.rs`**

Update the existing `req` helper's callers: every `core.ask(&req(...))` / `core.ask(&AskRequest{..})` in the test module gets a second argument `Door::Api`. Add `use crate::store::feedback::Door;` at the top of the tests module. Then add:

```rust
    #[tokio::test]
    async fn a_ui_ask_is_recorded_with_its_citations_when_feedback_is_on() {
        let mut core = test_core().await;
        core.feedback.enabled = true;
        seed(&core, 3, 4).await;
        let out = core.ask(&req("chunk"), Door::Ui.by("me")).await.unwrap();
        let id = out.event_id.expect("a UI ask is recorded");
        let ev = core.store.ask_event(&id).await.unwrap().expect("stored");
        assert_eq!(ev.question, "chunk");
        assert_eq!(ev.answer, out.answer);
        assert_eq!(
            ev.citations.iter().map(|c| c.artifact_id.as_str()).collect::<Vec<_>>(),
            out.citations.iter().map(|c| c.artifact_id.as_str()).collect::<Vec<_>>(),
            "the stored citations must be exactly the excerpts the model saw, in order"
        );
        assert!(!out.abstained);
        // The vector it retrieved with travels with it, so a gap can be
        // clustered later without re-embedding.
        let dim: i64 = sqlx::query_scalar("SELECT vec_dim FROM ask_events WHERE id = ?")
            .bind(&id)
            .fetch_one(&core.store.pool)
            .await
            .unwrap();
        assert!(dim > 0);
    }

    #[tokio::test]
    async fn an_api_or_mcp_ask_is_never_recorded() {
        let mut core = test_core().await;
        core.feedback.enabled = true;
        seed(&core, 3, 4).await;
        for door in [Door::Api, Door::Mcp] {
            let out = core.ask(&req("chunk"), door).await.unwrap();
            assert!(out.event_id.is_none(), "{door:?} recorded a question");
        }
        assert_eq!(core.store.ask_stats().await.unwrap().asked, 0);
    }

    #[tokio::test]
    async fn a_ui_ask_is_not_recorded_when_feedback_is_off() {
        let core = test_core().await;
        seed(&core, 3, 4).await;
        let out = core.ask(&req("chunk"), Door::Ui.by("me")).await.unwrap();
        assert!(out.event_id.is_none());
    }

    #[tokio::test]
    async fn an_ask_with_no_hits_is_recorded_as_an_abstention_without_a_model_call() {
        let mut core = test_core().await;
        core.feedback.enabled = true;
        let out = core.ask(&req("nothing is stored"), Door::Ui.by("me")).await.unwrap();
        assert!(out.abstained);
        assert!(crate::infer::prompt::abstained(&out.answer), "{}", out.answer);
        let ev = core.store.ask_event(out.event_id.as_deref().unwrap()).await.unwrap().unwrap();
        assert!(ev.abstained && ev.citations.is_empty());
    }

    #[tokio::test]
    async fn an_answer_that_opens_with_the_sentinel_is_flagged_abstained() {
        let mut core = test_core().await;
        core.completer = std::sync::Arc::new(crate::infer::fake::FakeCompleter {
            reply: Some("Not in the knowledge base. The excerpts cover chunks only.".into()),
        });
        seed(&core, 3, 4).await;
        let out = core.ask(&req("chunk"), Door::Api).await.unwrap();
        assert!(out.abstained);
        assert!(!out.citations.is_empty(), "abstaining does not hide what was shown");
    }
```

- [ ] **Step 3: Run to verify failure**

Run: `cargo test --lib core::ask 2>&1 | grep -E "^error" | head -3`
Expected: `ask` takes 1 argument, no field `event_id`.

- [ ] **Step 4: Implement**

In `src/core/ask.rs`:

1. Imports: add `use crate::store::asks::{NewAsk, NewAskCitation};` and `use crate::store::feedback::{Door, Origin};`.
2. `AskResponse` gains, after `truncated`:

```rust
    /// The answer opened with `prompt::ABSTAIN_PREFIX`, or there was nothing
    /// to show the model. What the harness counts as "said nothing here".
    pub abstained: bool,
    /// The recorded question, when this door records — the UI, with feedback
    /// on. The page shows a verdict bar only when this is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
```

3. Signature: `pub async fn ask(&self, req: &AskRequest, origin: impl Into<Origin>) -> Result<AskResponse>`. First line of the body: `let origin = origin.into();`.

4. Replace the three `return Ok(AskResponse { … })` sites and the final `Ok(AskResponse {…})` so that every response is built into a local `response`, then passed through a recorder. Concretely, restructure the tail of the function:

```rust
        let response = if hits.is_empty() {
            // No retrieval, no completion: spending a model call to say
            // "nothing found" is pure latency. Opens with the sentinel so the
            // page and the harness read it as the abstention it is.
            AskResponse {
                answer: format!("{}. Nothing matches that question.", crate::infer::prompt::ABSTAIN_PREFIX),
                citations: vec![],
                dropped: 0,
                truncated: false,
                abstained: true,
                event_id: None,
            }
        } else {
            … existing block-building, packing …
            if kept == 0 {
                AskResponse {
                    answer: "The best matching excerpt is too large for the configured context window.".into(),
                    citations: vec![],
                    dropped,
                    truncated: false,
                    // A configuration failure, not a statement about the base.
                    abstained: false,
                    event_id: None,
                }
            } else {
                … existing prompt + completion …
                AskResponse {
                    abstained: crate::infer::prompt::abstained(&answer.text),
                    answer: answer.text,
                    citations: hits.into_iter().take(kept).collect(),
                    dropped,
                    truncated: answer.truncated,
                    event_id: None,
                }
            }
        };
        self.record_ask(req, &origin, response).await
```

(Keep every existing comment where its code moved; the early `return`s become branch values.)

5. Add the recorder as a private method in the same `impl Core`:

```rust
    /// Record the question when this door records. Only the UI, and only with
    /// feedback on: a question is personal data of the same kind as a query,
    /// and API and MCP callers asked for the smallest footprint. Recorded
    /// synchronously — the id goes back to the page — and after the answer,
    /// which has already taken seconds; one insert costs nothing beside it.
    /// A failure to record must not cost the answer: it is logged and the
    /// response goes out without an id.
    async fn record_ask(
        &self,
        req: &AskRequest,
        origin: &Origin,
        mut response: AskResponse,
    ) -> Result<AskResponse> {
        if !(self.feedback.enabled && origin.door == Door::Ui) {
            return Ok(response);
        }
        let ask = NewAsk {
            question: req.q.trim().to_string(),
            scope: origin.scope.clone(),
            filters: serde_json::json!({
                "tags": req.tags,
                "category": req.category,
                "limit": req.limit.unwrap_or(8),
            })
            .to_string(),
            query_vec: self.cached_query_vector(&req.q).unwrap_or_default(),
            embed_model: self.embedder.model().to_string(),
            answer: response.answer.clone(),
            abstained: response.abstained,
            dropped: response.dropped,
            truncated: response.truncated,
            citations: response
                .citations
                .iter()
                .map(|c| NewAskCitation {
                    artifact_id: c.artifact_id.clone(),
                    score: c.score,
                })
                .collect(),
        };
        match self.store.record_ask(ask).await {
            Ok(id) => response.event_id = Some(id),
            Err(e) => tracing::warn!(error = %e, "could not record the question"),
        }
        Ok(response)
    }
```

Note: with no hits, `search_with` still embedded the query, so the vector is in the cache. Test `an_ask_with_no_hits…` does not assert the dim, so an empty vector there is tolerated.

6. Callers:
   - `src/web/api.rs` `ask`: `st.core.ask(&req, crate::store::feedback::Door::Api).await?`
   - `src/mcp/mod.rs` `ask`: `.ask(&…, crate::store::feedback::Door::Mcp)`
   - `src/web/ui.rs` `ask_submit`: change `_id: Identity` to `id: Identity` and call `.ask(&…, crate::store::feedback::Door::Ui.by(id.subject))`. The `AnswerTemplate` literal is completed in Task 4; for now add `abstained: out.abstained, event_id: out.event_id` fields to `AnswerTemplate` (declare them: `abstained: bool`, `event_id: Option<String>`) so it compiles — the template does not read them yet.

- [ ] **Step 5: Run the tests**

Run: `cargo test --lib core::ask 2>&1 | tail -3` then `cargo test 2>&1 | grep -E "^test result|FAILED"`
Expected: all pass. The existing `ask_with_no_matches_says_so_without_calling_the_model` asserts the answer contains "nothing" — the new wording still does.

- [ ] **Step 6: Commit**

```bash
cargo fmt && cargo clippy --all-targets 2>&1 | grep -E "^(warning|error)"; git add -A && git commit -m "feat(ask): record UI questions with what the model was shown, and say when it abstained

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: Judging on the page

**Files:**
- Modify: `src/web/templates/_answer.html`
- Create: `src/web/templates/_ask_verdict.html`, `src/web/templates/_ask_carried.html`
- Modify: `src/web/ui.rs` (`AnswerTemplate`, new templates, two routes, router registration at the `.route("/ui/ask", …)` line)
- Modify: `src/web/judge.rs` (`JudgeTemplate` gets `asks: AskStats`), `src/web/templates/judge.html`

**Interfaces:**
- Consumes: `Store::{ask_event, judge_ask, unjudge_ask, toggle_carried, ask_stats}`, `AskVerdict`.
- Produces: routes `POST /ui/ask/{id}/verdict` (form `verdict=right|wrong|nothing_here|none`) → `_ask_verdict.html`; `POST /ui/ask/{id}/carried` (form `n=<i64>`) → `_ask_carried.html` + out-of-band verdict bar.

- [ ] **Step 1: Write the failing route tests in `src/web/ui.rs` tests**

```rust
    /// Capture a small base, ask on a feedback-enabled session, and return the
    /// answer html and the recorded event id. Capture through the page, exactly
    /// as `ask_renders_an_answer_with_citations` does: the fake synthesizer and
    /// embedder in `test_core` complete inside the capture handler.
    async fn ask_recorded(app: &axum::Router, cookie: &str, core: &crate::core::Core) -> (String, String) {
        app.clone()
            .oneshot(form("/ui/capture", cookie, "text=alpha+para%0A%0Abeta+para"))
            .await
            .unwrap();
        let res = app
            .clone()
            .oneshot(form("/ui/ask", cookie, "q=what+is+alpha"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let html = body_of(res).await;
        assert_eq!(core.store.ask_stats().await.unwrap().asked, 1, "the UI ask was not recorded");
        let id: String = sqlx::query_scalar("SELECT id FROM ask_events LIMIT 1")
            .fetch_one(&core.store.pool)
            .await
            .unwrap();
        (html, id)
    }
```

Then the tests:

```rust
    #[tokio::test]
    async fn the_answer_page_offers_a_verdict_when_the_question_was_recorded() {
        let (app, cookie, core) = app_session_and_core_with_feedback().await;
        let (html, id) = ask_recorded(&app, &cookie, &core).await;
        assert!(html.contains(&format!("/ui/ask/{id}/verdict")), "{html}");
        assert!(html.contains("Nothing here"), "{html}");
        assert!(html.contains(&format!("/ui/ask/{id}/carried")), "{html}");
    }

    #[tokio::test]
    async fn the_answer_page_offers_no_verdict_when_feedback_is_off() {
        let (app, cookie) = app_with_session().await;
        app.clone()
            .oneshot(form("/ui/capture", &cookie, "text=alpha+para%0A%0Abeta+para"))
            .await
            .unwrap();
        let html = body_of(app.oneshot(form("/ui/ask", &cookie, "q=what+is+alpha")).await.unwrap()).await;
        assert!(!html.contains("/verdict"), "{html}");
    }

    #[tokio::test]
    async fn a_verdict_is_recorded_and_can_be_undone() {
        let (app, cookie, core) = app_session_and_core_with_feedback().await;
        let (_, id) = ask_recorded(&app, &cookie, &core).await;
        let res = app
            .clone()
            .oneshot(form(&format!("/ui/ask/{id}/verdict"), &cookie, "verdict=wrong"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bar = body_of(res).await;
        assert!(bar.contains("wrong") && bar.contains("undo"), "{bar}");
        assert_eq!(
            core.store.ask_event(&id).await.unwrap().unwrap().verdict,
            Some(crate::store::asks::AskVerdict::Wrong)
        );

        let bar = body_of(
            app.clone()
                .oneshot(form(&format!("/ui/ask/{id}/verdict"), &cookie, "verdict=none"))
                .await
                .unwrap(),
        )
        .await;
        assert!(bar.contains("Nothing here"), "the buttons are back: {bar}");
        assert!(core.store.ask_event(&id).await.unwrap().unwrap().verdict.is_none());
    }

    #[tokio::test]
    async fn marking_a_carrier_marks_the_answer_right_and_updates_the_bar_out_of_band() {
        let (app, cookie, core) = app_session_and_core_with_feedback().await;
        let (_, id) = ask_recorded(&app, &cookie, &core).await;
        let res = app
            .clone()
            .oneshot(form(&format!("/ui/ask/{id}/carried"), &cookie, "n=1"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let html = body_of(res).await;
        assert!(html.contains("hx-swap-oob"), "the verdict bar must follow the toggle: {html}");
        assert!(html.contains("right"), "{html}");
        let ev = core.store.ask_event(&id).await.unwrap().unwrap();
        assert_eq!(ev.verdict, Some(crate::store::asks::AskVerdict::Right));
        assert!(ev.citations[0].carried);
    }

    #[tokio::test]
    async fn judging_an_unknown_question_is_not_found() {
        let (app, cookie) = app_with_session().await;
        let res = app
            .oneshot(form("/ui/ask/nope/verdict", &cookie, "verdict=right"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib web::ui::tests::a_verdict_is_recorded 2>&1 | tail -3`
Expected: 404 / assertion failures (routes missing).

- [ ] **Step 3: Templates**

`src/web/templates/_ask_verdict.html`:

```html
{# The bar under an answer. Its id is what the carrier toggle swaps
   out-of-band, so a click on a card and a click on the bar land in one place. #}
<div id="ask-verdict" class="row ask-verdict">
  {% match verdict %}
  {% when Some with (v) %}
    <span class="muted">judged <b>{{ v }}</b></span>
    <button class="btn btn-ghost btn-sm" hx-post="/ui/ask/{{ event_id }}/verdict"
            hx-vals='{"verdict":"none"}' hx-target="#ask-verdict" hx-swap="outerHTML">undo</button>
  {% when None %}
    <span class="muted">Was this right?</span>
    <button class="btn btn-ghost btn-sm" hx-post="/ui/ask/{{ event_id }}/verdict"
            hx-vals='{"verdict":"right"}' hx-target="#ask-verdict" hx-swap="outerHTML">Right</button>
    <button class="btn btn-ghost btn-sm" hx-post="/ui/ask/{{ event_id }}/verdict"
            hx-vals='{"verdict":"wrong"}' hx-target="#ask-verdict" hx-swap="outerHTML">Wrong</button>
    <button class="btn btn-ghost btn-sm" hx-post="/ui/ask/{{ event_id }}/verdict"
            hx-vals='{"verdict":"nothing_here"}' hx-target="#ask-verdict" hx-swap="outerHTML">Nothing here</button>
  {% endmatch %}
</div>
```

`src/web/templates/_ask_carried.html`:

```html
{# One toggle per citation card. Swaps itself; the bar follows out-of-band
   because saying an excerpt carried the answer is saying the answer was right. #}
<button class="btn btn-ghost btn-sm{% if carried %} is-on{% endif %}"
        hx-post="/ui/ask/{{ event_id }}/carried" hx-vals='{"n":{{ n }}}'
        hx-swap="outerHTML" title="This excerpt carried the answer">
  {% if carried %}✓ carried the answer{% else %}carried the answer{% endif %}
</button>
{% if let Some(bar) = bar %}
<div id="ask-verdict" hx-swap-oob="outerHTML:#ask-verdict">{{ bar|safe }}</div>
{% endif %}
```

`_answer.html`: after the answer card's `<div class="md">…</div>` line add

```html
  {% if abstained %}
    <span class="card-meta"><span class="badge">nothing here</span></span>
  {% endif %}
  {% if let Some(id) = event_id %}
    {{ verdict_bar|safe }}
  {% endif %}
```

(Simpler: `AnswerTemplate` carries `verdict_bar: String` pre-rendered, empty when there is no event.) And inside the citations loop's `.chips` div, before the `source` link:

```html
    {% if let Some(id) = event_id %}
    <button class="btn btn-ghost btn-sm" hx-post="/ui/ask/{{ id }}/carried"
            hx-vals='{"n":{{ loop.index }}}' hx-swap="outerHTML"
            title="This excerpt carried the answer">carried the answer</button>
    {% endif %}
```

- [ ] **Step 4: Rust side in `src/web/ui.rs`**

Templates:

```rust
#[derive(Template)]
#[template(path = "_ask_verdict.html")]
struct AskVerdictTemplate {
    event_id: String,
    /// `"right" | "wrong" | "nothing here"` for display; `None` shows the buttons.
    verdict: Option<String>,
}

#[derive(Template)]
#[template(path = "_ask_carried.html")]
struct AskCarriedTemplate {
    event_id: String,
    n: i64,
    carried: bool,
    /// The bar, rendered, to swap out-of-band. Always `Some` from the route.
    bar: Option<String>,
}
```

`AnswerTemplate` gains `abstained: bool`, `event_id: Option<String>`, `verdict_bar: String`. In `ask_submit`, build:

```rust
    let verdict_bar = match &out.event_id {
        Some(id) => AskVerdictTemplate { event_id: id.clone(), verdict: None }.render()?,
        None => String::new(),
    };
```

(`render()` returns `askama::Result`; map with `.map_err(|e| crate::error::Error::Internal(e.to_string()))` or whatever the file already uses for template errors — grep `askama::Error` in `ui.rs` and copy.)

Handlers:

```rust
#[derive(serde::Deserialize)]
struct VerdictForm {
    verdict: String,
}

fn verdict_label(v: crate::store::asks::AskVerdict) -> String {
    use crate::store::asks::AskVerdict::*;
    match v {
        Right => "right",
        Wrong => "wrong",
        NothingHere => "nothing here",
    }
    .into()
}

async fn ask_verdict_bar(st: &AppState, id: &str) -> Result<String> {
    let ev = st.core.store.ask_event(id).await?.ok_or(crate::error::Error::NotFound)?;
    Ok(AskVerdictTemplate {
        event_id: ev.id,
        verdict: ev.verdict.map(verdict_label),
    }
    .render()
    .map_err(|e| crate::error::Error::Internal(e.to_string()))?)
}

async fn ask_verdict(
    State(st): State<AppState>,
    _id: Identity,
    Path(id): Path<String>,
    Form(f): Form<VerdictForm>,
) -> Result<Response> {
    match f.verdict.as_str() {
        "none" => st.core.store.unjudge_ask(&id).await?,
        v => {
            let verdict = crate::store::asks::AskVerdict::parse(v)
                .ok_or_else(|| crate::error::Error::Validation(format!("unknown verdict {v}")))?;
            st.core.store.judge_ask(&id, verdict).await?;
        }
    }
    Ok(Html(ask_verdict_bar(&st, &id).await?).into_response())
}

#[derive(serde::Deserialize)]
struct CarriedForm {
    n: i64,
}

async fn ask_carried(
    State(st): State<AppState>,
    _id: Identity,
    Path(id): Path<String>,
    Form(f): Form<CarriedForm>,
) -> Result<Response> {
    let carried = st.core.store.toggle_carried(&id, f.n).await?;
    let bar = ask_verdict_bar(&st, &id).await?;
    Ok(HtmlTemplate(AskCarriedTemplate {
        event_id: id,
        n: f.n,
        carried,
        bar: Some(bar),
    })
    .into_response())
}
```

Check what the file imports for `Html` (axum::response::Html) and for `Error::Internal` — use the variant that exists in `src/error.rs` for "template failed" (grep `askama` in `error.rs`; if there is a `From<askama::Error>`, use `?` directly).

Router: after `.route("/ui/ask", get(ask_page).post(ask_submit))` add

```rust
        .route("/ui/ask/{id}/verdict", post(ask_verdict))
        .route("/ui/ask/{id}/carried", post(ask_carried))
```

CSS (`assets/app.css`, near `.flag`): `.ask-verdict { margin-top: .5rem; gap: .5rem; } .is-on { color: var(--color-accent); }` — check the variable name for the accent colour in the file and use that.

- [ ] **Step 5: Judge page line**

In `src/web/judge.rs`: `JudgeTemplate` gains `asks: crate::store::asks::AskStats`; `page` sets `asks: st.core.store.ask_stats().await?`. In `judge.html`, after the `hits · finds · gaps · discarded` paragraph:

```html
  {% if asks.asked > 0 %}
  <p class="muted mono">
    {{ asks.judged }} of {{ asks.asked }} questions judged ·
    {{ asks.right }} right · {{ asks.wrong }} wrong · {{ asks.nothing_here }} nothing here
  </p>
  {% endif %}
```

Add a test in `judge.rs` tests: record two asks through the store, judge one right, GET `/ui/judge`, assert the body contains `1 of 2 questions judged`. Follow the existing judge page tests for how the app and cookie are built there.

- [ ] **Step 6: Run the tests**

Run: `cargo test --lib web:: 2>&1 | grep -E "^test result|FAILED|panicked"`
Expected: pass.

- [ ] **Step 7: Commit**

```bash
cargo fmt && cargo clippy --all-targets 2>&1 | grep -E "^(warning|error)"; git add -A && git commit -m "feat(ui): judge an answer where it is read — right, wrong, nothing here, and what carried it

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 5: Export judged questions

**Files:**
- Modify: `src/eval/mod.rs`, `src/eval/export.rs`, `src/main.rs:211-225`

**Interfaces:**
- Produces:
  ```rust
  pub struct EvalQuestion { pub question: String, pub verdict: String, pub expect: Vec<String>, pub note: Option<String> }
  pub fn questions_path(dir) -> PathBuf; pub fn load_questions(dir) -> Result<Vec<EvalQuestion>>; pub fn save_questions(dir, &[EvalQuestion]) -> Result<()>;
  pub async fn export(store, dir) -> Result<(usize, usize, usize)>;  // artifacts, pairs, questions
  ```

- [ ] **Step 1: Failing tests in `src/eval/export.rs`**

```rust
    async fn record_ask(store: &Store, q: &str, cited: &[&str]) -> String {
        store
            .record_ask(crate::store::asks::NewAsk {
                question: q.into(),
                scope: None,
                filters: "{}".into(),
                query_vec: vec![0.0; 4],
                embed_model: "fake".into(),
                answer: "a".into(),
                abstained: false,
                dropped: 0,
                truncated: false,
                citations: cited
                    .iter()
                    .map(|id| crate::store::asks::NewAskCitation { artifact_id: id.to_string(), score: 1.0 })
                    .collect(),
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_judged_question_becomes_an_eval_question_with_its_carriers() {
        let store = Store::memory().await.unwrap();
        let art = seed_one_artifact(&store).await;
        let id = record_ask(&store, "how do I", &[&art]).await;
        store.toggle_carried(&id, 1).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (_, _, questions) = export(&store, dir.path()).await.unwrap();
        assert_eq!(questions, 1);
        let qs = crate::eval::load_questions(dir.path()).unwrap();
        assert_eq!(qs[0].question, "how do I");
        assert_eq!(qs[0].verdict, "right");
        assert_eq!(qs[0].expect, vec![art]);
    }

    #[tokio::test]
    async fn unjudged_questions_are_not_exported_and_gone_carriers_are_dropped() {
        let store = Store::memory().await.unwrap();
        let art = seed_one_artifact(&store).await;
        record_ask(&store, "unjudged", &[&art]).await;
        let id = record_ask(&store, "carrier gone", &[&art, "deleted-artifact"]).await;
        store.toggle_carried(&id, 1).await.unwrap();
        store.toggle_carried(&id, 2).await.unwrap();
        let nothing = record_ask(&store, "not here", &[]).await;
        store.judge_ask(&nothing, crate::store::asks::AskVerdict::NothingHere).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (_, _, n) = export(&store, dir.path()).await.unwrap();
        assert_eq!(n, 2);
        let qs = crate::eval::load_questions(dir.path()).unwrap();
        let gone = qs.iter().find(|q| q.question == "carrier gone").unwrap();
        assert_eq!(gone.expect, vec![art]);
        let none = qs.iter().find(|q| q.question == "not here").unwrap();
        assert_eq!(none.verdict, "nothing_here");
        assert!(none.expect.is_empty());
    }
```

Check how the existing export tests make a temp dir (grep `tempdir\|tempfile` in the file) and use the same.

- [ ] **Step 2: Run to verify failure** — `cargo test --lib eval::export 2>&1 | tail -3` → compile errors.

- [ ] **Step 3: Implement**

`src/eval/mod.rs`, after `EvalPair`:

```rust
/// A question, its verdict, and the artifacts the operator said carried the
/// answer. `expect` is empty for `wrong` and `nothing_here`, and for a `right`
/// answer that was a synthesis with no single carrier — those still measure
/// abstention, not citation recall.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EvalQuestion {
    pub question: String,
    /// `right` | `wrong` | `nothing_here`.
    pub verdict: String,
    #[serde(default)]
    pub expect: Vec<String>,
    #[serde(default)]
    pub note: Option<String>,
}

pub fn questions_path(dir: &Path) -> PathBuf {
    dir.join("questions.json")
}

pub fn save_questions(dir: &Path, questions: &[EvalQuestion]) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = questions_path(dir);
    let json = serde_json::to_string_pretty(questions)?;
    std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))
}

pub fn load_questions(dir: &Path) -> Result<Vec<EvalQuestion>> {
    let path = questions_path(dir);
    let raw = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}
```

`src/eval/export.rs`: change the return type to `Result<(usize, usize, usize)>`, and before `save_artifacts` add:

```rust
    let asks = sqlx::query(
        "SELECT id, question, verdict, judged_at FROM ask_events
         WHERE verdict IS NOT NULL ORDER BY judged_at",
    )
    .fetch_all(&store.pool)
    .await?;
    let mut questions = Vec::with_capacity(asks.len());
    let mut lost_carriers = 0usize;
    for r in &asks {
        let id: String = r.get("id");
        let carriers: Vec<String> = sqlx::query_scalar(
            "SELECT artifact_id FROM ask_citations WHERE event_id = ? AND carried = 1 ORDER BY n",
        )
        .bind(&id)
        .fetch_all(&store.pool)
        .await?;
        let (kept, lost): (Vec<String>, Vec<String>) =
            carriers.into_iter().partition(|c| known.contains(c));
        lost_carriers += lost.len();
        questions.push(EvalQuestion {
            question: r.get("question"),
            verdict: r.get("verdict"),
            expect: kept,
            note: Some(format!("judged {}", r.get::<i64, _>("judged_at"))),
        });
    }
    if lost_carriers > 0 {
        tracing::warn!(lost_carriers, "carriers skipped: their artifact no longer exists");
    }
    …
    save_questions(dir, &questions)?;
    Ok((frozen.len(), pairs.len(), questions.len()))
```

Update the import line to include `EvalQuestion, save_questions`. Fix the two existing tests that destructure `(a, p)` to `(a, p, _)`.

`src/main.rs`: `let (artifacts, pairs, questions) = …; println!("wrote {artifacts} artifacts, {pairs} pairs and {questions} questions to {}", dir.display());`

- [ ] **Step 4: Run** — `cargo test --lib eval:: 2>&1 | tail -3` → pass. `cargo build` clean.

- [ ] **Step 5: Commit**

```bash
cargo fmt && git add -A && git commit -m "feat(eval): export judged questions beside judged searches

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 6: Metrics and the claim check

**Files:**
- Modify: `src/eval/metrics.rs`
- Create: `src/eval/claims.rs`; register `pub mod claims;` in `src/eval/mod.rs`
- Modify: `src/infer/prompt.rs` (claims prompt + schema), `src/infer/openai.rs` (`for_claim_checking`)

**Interfaces:**
- Produces:
  ```rust
  // metrics.rs
  pub fn fraction_cited(carriers: &[Vec<String>], cited: &[String]) -> f64;   // one question: carriers are alternatives (self + supersedes) per carrier; 0..1; 1.0 when no carriers
  #[derive(Default)] pub struct Abstention { pub should_and_did: usize, pub should_not_did: usize, pub should_and_did_not: usize, pub should_not_did_not: usize }
  impl Abstention { pub fn tally(pairs: &[(bool, bool)]) -> Self; }  // (expected, observed)
  pub fn fully_supported(unsupported_counts: &[usize]) -> (usize, usize);   // (answers with 0 unsupported, total)
  // claims.rs
  pub struct Claim { pub claim: String, pub supported_by: Vec<usize> }
  pub fn parse_claims(reply: &str, shown: usize) -> Result<Vec<Claim>>;   // a supported_by naming n > shown or 0 is dropped from that claim
  pub fn supported(claims: &[Claim]) -> (usize, usize);   // (supported, total)
  // prompt.rs
  pub const CLAIMS_SYSTEM: &str; pub fn claims_prompt(answer: &str, excerpts: &[String]) -> String; pub fn claims_schema() -> serde_json::Value;
  // openai.rs
  impl HttpCompleter { pub fn for_claim_checking(cfg: &SynthesizeRole) -> Self }
  ```

- [ ] **Step 1: Failing tests in `src/eval/metrics.rs`**

```rust
    #[test]
    fn fraction_cited_counts_each_carrier_once_and_accepts_a_successor() {
        let carriers = vec![vec!["a".to_string()], vec!["b".to_string(), "b2".to_string()]];
        assert_eq!(fraction_cited(&carriers, &["a".into(), "x".into()]), 0.5);
        assert_eq!(fraction_cited(&carriers, &["a".into(), "b2".into()]), 1.0);
        assert_eq!(fraction_cited(&[], &["a".into()]), 1.0, "no carriers is nothing to miss");
    }

    #[test]
    fn abstention_tallies_the_four_corners() {
        let t = Abstention::tally(&[(true, true), (true, false), (false, true), (false, false), (false, false)]);
        assert_eq!((t.should_and_did, t.should_and_did_not, t.should_not_did, t.should_not_did_not), (1, 1, 1, 2));
    }

    #[test]
    fn fully_supported_counts_answers_with_nothing_unsupported() {
        assert_eq!(fully_supported(&[0, 2, 0]), (2, 3));
        assert_eq!(fully_supported(&[]), (0, 0));
    }
```

- [ ] **Step 2: Implement metrics**

```rust
/// One question's citation recall: the fraction of its carriers that were
/// cited. Each carrier is a list of ids that satisfy it — itself and whatever
/// superseded it. No carriers is nothing to miss, and scores 1.
pub fn fraction_cited(carriers: &[Vec<String>], cited: &[String]) -> f64 {
    if carriers.is_empty() {
        return 1.0;
    }
    let hit = carriers
        .iter()
        .filter(|alts| alts.iter().any(|a| cited.contains(a)))
        .count();
    hit as f64 / carriers.len() as f64
}

/// The four corners of "did it say nothing here when it should have".
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Abstention {
    pub should_and_did: usize,
    pub should_and_did_not: usize,
    pub should_not_did: usize,
    pub should_not_did_not: usize,
}

impl Abstention {
    /// `(expected, observed)` per question.
    pub fn tally(pairs: &[(bool, bool)]) -> Self {
        let mut t = Self::default();
        for &(expected, observed) in pairs {
            match (expected, observed) {
                (true, true) => t.should_and_did += 1,
                (true, false) => t.should_and_did_not += 1,
                (false, true) => t.should_not_did += 1,
                (false, false) => t.should_not_did_not += 1,
            }
        }
        t
    }
}

/// `(answers with no unsupported item, answers)`.
pub fn fully_supported(unsupported_counts: &[usize]) -> (usize, usize) {
    (
        unsupported_counts.iter().filter(|n| **n == 0).count(),
        unsupported_counts.len(),
    )
}
```

- [ ] **Step 3: Failing tests for claims** — create `src/eval/claims.rs`:

```rust
//! The claim check: what the efficient model said about which excerpt
//! supports each sentence of an answer. Parsing only — the call is made by
//! the harness, and only when asked for.

use crate::infer::prompt::extract_json;
use anyhow::{Context, Result};

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct Claim {
    pub claim: String,
    #[serde(default)]
    pub supported_by: Vec<usize>,
}

#[derive(serde::Deserialize)]
struct Reply {
    claims: Vec<Claim>,
}

/// `shown` is how many excerpts the model was given; a number outside `1..=shown`
/// names nothing and is dropped from that claim rather than counted as support.
pub fn parse_claims(reply: &str, shown: usize) -> Result<Vec<Claim>> {
    let r: Reply = serde_json::from_str(extract_json(reply)).context("claim check reply was not the expected JSON")?;
    Ok(r
        .claims
        .into_iter()
        .map(|mut c| {
            c.supported_by.retain(|n| (1..=shown).contains(n));
            c
        })
        .collect())
}

/// `(claims with at least one supporting excerpt, claims)`.
pub fn supported(claims: &[Claim]) -> (usize, usize) {
    (claims.iter().filter(|c| !c.supported_by.is_empty()).count(), claims.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_well_formed_reply_is_read_and_counted() {
        let claims = parse_claims(
            r#"{"claims":[{"claim":"use -o ro","supported_by":[1]},{"claim":"it is fast","supported_by":[]}]}"#,
            2,
        )
        .unwrap();
        assert_eq!(supported(&claims), (1, 2));
    }

    #[test]
    fn a_claim_naming_an_excerpt_that_was_not_shown_is_unsupported() {
        let claims = parse_claims(r#"{"claims":[{"claim":"x","supported_by":[3, 0]}]}"#, 2).unwrap();
        assert_eq!(supported(&claims), (0, 1));
    }

    #[test]
    fn prose_around_the_json_is_tolerated_and_garbage_is_an_error() {
        assert!(parse_claims("Here you go:\n```json\n{\"claims\":[]}\n```", 1).unwrap().is_empty());
        assert!(parse_claims("no json here", 1).is_err());
    }
}
```

`extract_json` is `pub(crate)` in prompt.rs — fine inside the crate.

- [ ] **Step 4: Prompt and schema in `src/infer/prompt.rs`** (after `ask_prompt`):

```rust
/// The claim check behind the ask harness. It never runs on a request path.
pub const CLAIMS_SYSTEM: &str = r#"You check an answer against the excerpts it was written from. Split the answer into its atomic factual claims — one statement each, in the answer's own words. For every claim, list the numbers of the excerpts that state or directly entail it. A claim no excerpt supports gets an empty list. Do not judge whether a claim is true, only whether the excerpts say it. Reply with JSON only: {"claims":[{"claim":"…","supported_by":[1,3]}]}"#;

pub fn claims_prompt(answer: &str, excerpts: &[String]) -> String {
    format!(
        "Answer:\n{answer}\n\nExcerpts:\n\n{}",
        excerpts.join("\n\n---\n\n")
    )
}

/// The shape `eval::claims::parse_claims` reads. Rooted in an object and closed,
/// like every judge schema.
pub fn claims_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "claims": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "claim": {"type": "string"},
                        "supported_by": {"type": "array", "items": {"type": "integer"}}
                    },
                    "required": ["claim", "supported_by"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["claims"],
        "additionalProperties": false
    })
}
```

Add `("claims", claims_schema())` to the list in the existing test `every_judge_schema_object_is_closed_and_rooted_in_an_object` (prompt.rs ~line 1081).

`src/infer/openai.rs`, after `for_link_judging`:

```rust
    /// The claim check behind the ask harness: same endpoint and settings as the
    /// judges, its own response shape.
    pub fn for_claim_checking(cfg: &SynthesizeRole) -> Self {
        Self::judging(cfg, ("claims", prompt::claims_schema()))
    }
```

- [ ] **Step 5: Run** — `cargo test --lib eval:: infer::prompt 2>&1 | grep -E "^test result|FAILED"` → pass.

- [ ] **Step 6: Commit**

```bash
cargo fmt && cargo clippy --all-targets 2>&1 | grep -E "^(warning|error)"; git add -A && git commit -m "feat(eval): the arithmetic behind citation recall, abstention and faithfulness

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 7: The ask harness

**Files:**
- Modify: `tests/eval.rs`

**Interfaces:**
- Consumes: `load_questions`, `index`, `resolve_expected`, `Core::ask(_, Door::Judge)`, `missing_literals`, `HttpCompleter::for_claim_checking`, `CLAIMS_SYSTEM`/`claims_prompt`/`parse_claims`/`supported`, metrics.

- [ ] **Step 1: Header doc**

Extend the file's `//!` header:

```rust
//! The ask harness runs the same way and needs the ask endpoint too:
//!   ENGRAM_EVAL_DIR=~/engram-eval cargo test --test eval evaluate_ask -- --ignored --nocapture
//! It measures citation recall, abstention accuracy and faithfulness by
//! literals. With ENGRAM_EVAL_CLAIMS=1 it also asks the synthesize endpoint to
//! trace every claim to an excerpt — one call per answered question.
```

- [ ] **Step 2: The test**

Add after `evaluate_retrieval`:

```rust
#[tokio::test]
#[ignore]
async fn evaluate_ask() {
    use engram::eval::claims::{parse_claims, supported};
    use engram::eval::metrics::{Abstention, fraction_cited, fully_supported};
    use engram::infer::Completer;
    use engram::infer::prompt::{CLAIMS_SYSTEM, ask_excerpt, claims_prompt};
    use engram::infer::verify::missing_literals;

    let dir = eval_dir();
    let (artifacts, questions) = match (load_artifacts(&dir), engram::eval::load_questions(&dir)) {
        (Ok(a), Ok(q)) => (a, q),
        (a, q) => {
            let why = a.err().map(|e| e.to_string()).unwrap_or_default()
                + &q.err().map(|e| format!(" {e}")).unwrap_or_default();
            eprintln!(
                "no judged questions at {} ({}). Ask on /ui/ask with feedback.enabled, judge the \
                 answers, run `engram --export-eval <dir>` and set ENGRAM_EVAL_DIR to it.",
                dir.display(),
                why.trim()
            );
            return;
        }
    };
    assert!(!questions.is_empty(), "questions.json is empty");

    let mut cfg = Config::load(None).expect("config.toml");
    cfg.vector.collection = COLLECTION.to_string();
    let vectors = Arc::new(QdrantVectors::connect(&cfg.vector).await.unwrap());
    vectors.drop_collection().await.unwrap();
    vectors.ensure_collection(cfg.infer.embed.dim).await.unwrap();
    let store = Store::memory().await.unwrap();
    let core = Core::from_config(&cfg, vectors.clone(), store);
    let translated = index(&core, &artifacts).await;

    let check_claims = std::env::var("ENGRAM_EVAL_CLAIMS").is_ok_and(|v| v == "1");
    let claim_checker = engram::infer::openai::HttpCompleter::for_claim_checking(&cfg.infer.synthesize);

    let mut recall: Vec<f64> = Vec::new();
    let mut all_cited = (0usize, 0usize);
    let mut abstention: Vec<(bool, bool)> = Vec::new();
    let mut wrong_abstain: Vec<String> = Vec::new();
    let mut wrong_answer: Vec<String> = Vec::new();
    let mut unsupported_literals: Vec<usize> = Vec::new();
    let mut literal_misses: Vec<(String, Vec<String>)> = Vec::new();
    let mut claims_total = (0usize, 0usize);
    let mut answers_fully = Vec::new();

    for q in &questions {
        let out = core
            .ask(
                &engram::core::ask::AskRequest { q: q.question.clone(), limit: None, tags: vec![], category: None },
                engram::store::feedback::Door::Judge,
            )
            .await
            .expect("ask failed");
        let short: String = q.question.chars().take(48).collect();

        // Abstention.
        let expected = q.verdict == "nothing_here";
        abstention.push((expected, out.abstained));
        if expected && !out.abstained {
            wrong_answer.push(short.clone());
        }
        if !expected && out.abstained {
            wrong_abstain.push(short.clone());
        }

        // Citation recall, over right answers with carriers.
        if q.verdict == "right" && !q.expect.is_empty() {
            let mut carriers = Vec::new();
            for e in &q.expect {
                let stored = translated.get(e).expect("questions.json names an artifact not in artifacts.json");
                carriers.push(resolve_expected(&core, stored).await);
            }
            let cited: Vec<String> = out.citations.iter().map(|c| c.artifact_id.clone()).collect();
            let f = fraction_cited(&carriers, &cited);
            recall.push(f);
            all_cited.1 += 1;
            if f >= 1.0 {
                all_cited.0 += 1;
            }
        }

        // Faithfulness, over answered questions.
        if !out.abstained && !out.citations.is_empty() {
            let excerpts: Vec<String> = out
                .citations
                .iter()
                .enumerate()
                .map(|(i, c)| ask_excerpt(i + 1, c.title.as_deref().unwrap_or_default(), &c.text, &[]))
                .collect();
            let missing = missing_literals(&out.answer, &[], &excerpts.join("\n"));
            unsupported_literals.push(missing.len());
            if !missing.is_empty() {
                literal_misses.push((short.clone(), missing));
            }
            if check_claims {
                let reply = claim_checker
                    .complete(CLAIMS_SYSTEM, &claims_prompt(&out.answer, &excerpts))
                    .await
                    .expect("claim check failed");
                match parse_claims(&reply, excerpts.len()) {
                    Ok(claims) => {
                        let (s, t) = supported(&claims);
                        claims_total.0 += s;
                        claims_total.1 += t;
                        answers_fully.push(t - s);
                    }
                    Err(e) => eprintln!("  claim check unreadable for {short:?}: {e}"),
                }
            }
        }
    }

    println!(
        "\n{} questions over {} artifacts   (ask {}, embed {}, claims {})",
        questions.len(),
        artifacts.len(),
        cfg.infer.ask.model,
        cfg.infer.embed.model,
        if check_claims { "on" } else { "off" }
    );
    if !recall.is_empty() {
        println!(
            "citation recall   {:.2}   (all carriers cited {}/{})",
            recall.iter().sum::<f64>() / recall.len() as f64,
            all_cited.0,
            all_cited.1
        );
    }
    let t = Abstention::tally(&abstention);
    println!(
        "abstained when it should   {}/{}\nanswered when it should    {}/{}",
        t.should_and_did,
        t.should_and_did + t.should_and_did_not,
        t.should_not_did_not,
        t.should_not_did_not + t.should_not_did
    );
    let (clean, answered) = fully_supported(&unsupported_literals);
    println!("answers with no unsupported literal   {clean}/{answered}");
    if check_claims {
        let (fc, fa) = fully_supported(&answers_fully);
        println!(
            "claims supported   {}/{}   (answers fully supported {fc}/{fa})",
            claims_total.0, claims_total.1
        );
    }
    for (label, list) in [("answered when it should have abstained", &wrong_answer), ("abstained when it should have answered", &wrong_abstain)] {
        if !list.is_empty() {
            println!("\n{label}:");
            for q in list {
                println!("  {q}");
            }
        }
    }
    if !literal_misses.is_empty() {
        println!("\nunsupported literals:");
        for (q, lits) in &literal_misses {
            println!("  {q:<50} {}", lits.join(" · "));
        }
    }
    println!();
    vectors.drop_collection().await.unwrap();
}
```

Confirm `HttpCompleter` is `pub` at `engram::infer::openai::HttpCompleter` (grep `pub struct HttpCompleter`); and that `Config` fields `infer.ask.model` exist (`AskRole` has `model`).

- [ ] **Step 3: Compile the harness** — `cargo test --test eval --no-run 2>&1 | tail -3` → builds. `cargo test --test eval` (the wiring test still passes; the ignored ones are skipped).

- [ ] **Step 4: Commit**

```bash
cargo fmt && git add -A && git commit -m "feat(eval): the ask harness — citation recall, abstention, faithfulness

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 8: Gaps in the store, and the clustering function

**Files:**
- Modify: `src/store/schema.sql` (append `gap_clusters`), `src/store/mod.rs` (`pub mod gaps;` and `ADDED_COLUMNS` entry)
- Create: `src/store/gaps.rs`, `src/core/gaps.rs`; register `pub mod gaps;` in `src/core/mod.rs`

**Interfaces:**
- Produces:
  ```rust
  // store/gaps.rs
  #[derive(Clone, Copy, PartialEq, Eq, Debug)] pub enum GapKind { Ask, Search }   // as_str "ask"|"search", parse
  pub struct Gap { pub kind: GapKind, pub id: String, pub text: String, pub vec: Vec<f32> }
  pub struct GapCluster { pub key: String, pub label: String, pub labelled_by: String, pub members: Vec<(GapKind, String)> }
  pub struct GapRow { pub label: String, pub labelled_by: String, pub members: Vec<Gap> }  // for the page (vec left empty)
  impl Store {
      pub async fn open_gaps(&self, embed_model: &str) -> Result<Vec<Gap>>;
      pub async fn dismiss_gap(&self, kind: GapKind, id: &str) -> Result<()>;   // NotFound if no row
      pub async fn cluster_keys(&self) -> Result<Vec<(String, String)>>;         // (key, labelled_by)
      pub async fn delete_clusters(&self, keys: &[String]) -> Result<()>;
      pub async fn put_cluster(&self, c: &GapCluster) -> Result<()>;            // upsert
      pub async fn gap_rows(&self, embed_model: &str) -> Result<(Vec<GapRow>, Vec<Gap>)>; // (clustered rows, gaps not yet in any cluster)
  }
  // core/gaps.rs
  pub const GAP_LINK_AT: f32 = 0.55;
  pub fn cosine(a: &[f32], b: &[f32]) -> f32;
  pub fn cluster(vecs: &[Vec<f32>], link_at: f32) -> Vec<Vec<usize>>;   // index groups, each sorted, groups ordered by first member
  pub fn cluster_key(members: &[(GapKind, String)]) -> String;            // sha256 hex of sorted "kind:id\n"
  pub fn terms_label(texts: &[&str]) -> String;                           // 3 most frequent content words, or the first text truncated
  ```

- [ ] **Step 1: Schema and column**

Append to `schema.sql` after `ask_citations`:

```sql
-- ── Knowledge gaps ───────────────────────────────────────────────────────────
-- Unanswered questions and gap searches, grouped by their stored vectors and
-- named once. Membership is identity: a group whose members change is a new
-- row with a new name, so the same members are never named twice.
CREATE TABLE IF NOT EXISTS gap_clusters (
  key         TEXT PRIMARY KEY,
  label       TEXT NOT NULL,
  labelled_by TEXT NOT NULL,
  members     TEXT NOT NULL,
  created_at  INTEGER NOT NULL
);
```

In `ADDED_COLUMNS` add, with a comment: `// Arrived with knowledge gaps. NULL on every existing row: nothing predating it was covered.` → `("search_events", "dismissed_at", "INTEGER"),`. Also add `dismissed_at INTEGER` to the `search_events` CREATE TABLE in `schema.sql` (after `skips`), so fresh databases match.

- [ ] **Step 2: Failing tests for `core/gaps.rs`**

Create `src/core/gaps.rs`:

```rust
//! Grouping the holes. A gap is a question the base could not answer or a
//! search judged to have no answer; two gaps about the same thing are one hole
//! and should be shown as one. Pure functions over stored vectors, so grouping
//! costs no inference and can be tested without any.

use crate::store::gaps::GapKind;

/// Cosine at or above which two gaps are the same hole. A constant with its
/// reasoning here rather than a setting: nothing has measured it yet, and the
/// roadmap's rule is that a default moves after the harness has run. 0.55 is
/// well above what unrelated questions score under the embedders engram is
/// run with, and below what two phrasings of one situation score.
pub const GAP_LINK_AT: f32 = 0.55;

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 { 0.0 } else { dot / (na * nb) }
}

/// Single-linkage over cosine: two vectors join at `link_at`, and joining is
/// transitive. Returns groups of indices, each sorted, ordered by their first
/// member. N is tens, so the quadratic pass is fine.
pub fn cluster(vecs: &[Vec<f32>], link_at: f32) -> Vec<Vec<usize>> {
    let n = vecs.len();
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(p: &mut [usize], i: usize) -> usize {
        let mut r = i;
        while p[r] != r { r = p[r]; }
        let mut c = i;
        while p[c] != r { let next = p[c]; p[c] = r; c = next; }
        r
    }
    for i in 0..n {
        for j in i + 1..n {
            if cosine(&vecs[i], &vecs[j]) >= link_at {
                let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                if a != b { parent[a.max(b)] = a.min(b); }
            }
        }
    }
    let mut groups: std::collections::BTreeMap<usize, Vec<usize>> = Default::default();
    for i in 0..n {
        let r = find(&mut parent, i);
        groups.entry(r).or_default().push(i);
    }
    groups.into_values().collect()
}

/// Identity of a cluster: its members, and nothing else.
pub fn cluster_key(members: &[(GapKind, String)]) -> String {
    use sha2::{Digest, Sha256};
    let mut keys: Vec<String> = members.iter().map(|(k, id)| format!("{}:{id}\n", k.as_str())).collect();
    keys.sort();
    hex::encode(Sha256::digest(keys.concat().as_bytes()))
}

const STOP: &[&str] = &[
    "a", "an", "the", "and", "or", "of", "to", "in", "on", "for", "with", "how", "do", "i", "is",
    "it", "what", "can", "my", "me", "does", "why", "when", "are", "be", "this", "that", "from",
    "at", "by", "into", "was", "we", "you", "not", "no",
];

/// The three most frequent content words across the texts, or the first text
/// cut short. What a cluster is called before — or without — a model naming it.
pub fn terms_label(texts: &[&str]) -> String {
    let mut counts: std::collections::HashMap<String, usize> = Default::default();
    for t in texts {
        for w in t.split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() > 2) {
            let w = w.to_lowercase();
            if !STOP.contains(&w.as_str()) {
                *counts.entry(w).or_default() += 1;
            }
        }
    }
    let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let words: Vec<String> = ranked.into_iter().take(3).map(|(w, _)| w).collect();
    if words.is_empty() {
        texts.first().map(|t| t.chars().take(40).collect()).unwrap_or_default()
    } else {
        words.join(" · ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn near_vectors_group_and_far_ones_stand_alone() {
        let v = vec![
            vec![1.0, 0.0, 0.0],
            vec![0.9, 0.1, 0.0],   // near 0
            vec![0.0, 1.0, 0.0],
            vec![0.0, 0.95, 0.05], // near 2
            vec![0.0, 0.0, 1.0],   // alone
        ];
        assert_eq!(cluster(&v, 0.55), vec![vec![0, 1], vec![2, 3], vec![4]]);
        assert!(cluster(&[], 0.55).is_empty());
    }

    #[test]
    fn linkage_is_transitive() {
        // 0~1 and 1~2 but 0 and 2 are below the line: one cluster.
        let v = vec![vec![1.0, 0.0], vec![0.7, 0.7], vec![0.0, 1.0]];
        assert_eq!(cluster(&v, 0.6), vec![vec![0, 1, 2]]);
    }

    #[test]
    fn a_key_depends_on_membership_and_not_on_order() {
        let a = vec![(GapKind::Ask, "1".to_string()), (GapKind::Search, "2".to_string())];
        let b = vec![(GapKind::Search, "2".to_string()), (GapKind::Ask, "1".to_string())];
        assert_eq!(cluster_key(&a), cluster_key(&b));
        assert_ne!(cluster_key(&a), cluster_key(&a[..1]));
    }

    #[test]
    fn a_terms_label_is_the_shared_content_words() {
        let l = terms_label(&["how do I mount an E01 image", "mounting E01 images read only", "E01 mount fails"]);
        assert!(l.contains("e01") && l.contains("mount"), "{l}");
        assert_eq!(terms_label(&["a of the"]), "a of the");
    }
}
```

(Tests are written with the implementation here because the functions are small and pure; run them and adjust the transitive test's numbers if cosine(0,1)=0.707 ≥ 0.6 and cosine(0,2)=0 hold — they do.)

- [ ] **Step 3: Failing store tests, then implementation** — create `src/store/gaps.rs`:

```rust
//! The holes, as rows: unanswered questions and gap searches, and the groups
//! the sweep made of them.

use super::{Store, now};
use crate::error::{Error, Result};
use crate::store::feedback::blob_to_vec;
use sqlx::Row;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapKind {
    Ask,
    Search,
}

impl GapKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            GapKind::Ask => "ask",
            GapKind::Search => "search",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ask" => Some(GapKind::Ask),
            "search" => Some(GapKind::Search),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Gap {
    pub kind: GapKind,
    pub id: String,
    pub text: String,
    pub vec: Vec<f32>,
}

#[derive(Debug, Clone)]
pub struct GapCluster {
    pub key: String,
    pub label: String,
    /// `model` or `terms`.
    pub labelled_by: String,
    pub members: Vec<(GapKind, String)>,
}

/// A cluster as the capture page shows it.
#[derive(Debug, Clone)]
pub struct GapRow {
    pub label: String,
    pub labelled_by: String,
    pub members: Vec<Gap>,
}

impl Store {
    /// Every open gap with a vector under `embed_model`. A vector under another
    /// model is not comparable and is left out; an empty one (the cache had
    /// evicted it) likewise.
    pub async fn open_gaps(&self, embed_model: &str) -> Result<Vec<Gap>> {
        let mut out = Vec::new();
        for r in sqlx::query(
            "SELECT id, question AS text, query_vec FROM ask_events
             WHERE verdict = 'nothing_here' AND dismissed_at IS NULL
               AND embed_model = ? AND vec_dim > 0
             ORDER BY judged_at DESC",
        )
        .bind(embed_model)
        .fetch_all(&self.pool)
        .await?
        {
            out.push(Gap {
                kind: GapKind::Ask,
                id: r.get("id"),
                text: r.get("text"),
                vec: blob_to_vec(&r.get::<Vec<u8>, _>("query_vec")),
            });
        }
        for r in sqlx::query(
            "SELECT id, query AS text, query_vec FROM search_events
             WHERE verdict = 'gap' AND dismissed_at IS NULL AND embed_model = ?
             ORDER BY judged_at DESC",
        )
        .bind(embed_model)
        .fetch_all(&self.pool)
        .await?
        {
            out.push(Gap {
                kind: GapKind::Search,
                id: r.get("id"),
                text: r.get("text"),
                vec: blob_to_vec(&r.get::<Vec<u8>, _>("query_vec")),
            });
        }
        Ok(out)
    }

    pub async fn dismiss_gap(&self, kind: GapKind, id: &str) -> Result<()> {
        let table = match kind {
            GapKind::Ask => "ask_events",
            GapKind::Search => "search_events",
        };
        // The table name is one of two literals above; nothing from a request
        // reaches the statement text.
        let res = sqlx::query(&format!("UPDATE {table} SET dismissed_at = ? WHERE id = ?"))
            .bind(now())
            .bind(id)
            .execute(&self.pool)
            .await?;
        if res.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    pub async fn cluster_keys(&self) -> Result<Vec<(String, String)>> {
        Ok(sqlx::query("SELECT key, labelled_by FROM gap_clusters")
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(|r| (r.get("key"), r.get("labelled_by")))
            .collect())
    }

    pub async fn delete_clusters(&self, keys: &[String]) -> Result<()> {
        for k in keys {
            sqlx::query("DELETE FROM gap_clusters WHERE key = ?")
                .bind(k)
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    pub async fn put_cluster(&self, c: &GapCluster) -> Result<()> {
        let members = serde_json::to_string(
            &c.members
                .iter()
                .map(|(k, id)| serde_json::json!({"kind": k.as_str(), "id": id}))
                .collect::<Vec<_>>(),
        )?;
        sqlx::query(
            "INSERT INTO gap_clusters (key, label, labelled_by, members, created_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(key) DO UPDATE SET label = excluded.label, labelled_by = excluded.labelled_by",
        )
        .bind(&c.key)
        .bind(&c.label)
        .bind(&c.labelled_by)
        .bind(members)
        .bind(now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The clusters with their open members resolved, and the open gaps no
    /// cluster names yet (judged since the last sweep). A member that has been
    /// dismissed since the sweep is simply absent from its row; a row left with
    /// no members is not returned.
    pub async fn gap_rows(&self, embed_model: &str) -> Result<(Vec<GapRow>, Vec<Gap>)> {
        let open = self.open_gaps(embed_model).await?;
        let mut unclustered: Vec<Gap> = open.clone();
        let mut rows = Vec::new();
        for r in sqlx::query("SELECT label, labelled_by, members FROM gap_clusters ORDER BY created_at DESC")
            .fetch_all(&self.pool)
            .await?
        {
            let members: Vec<serde_json::Value> = serde_json::from_str(&r.get::<String, _>("members"))?;
            let mut resolved = Vec::new();
            for m in members {
                let kind = m["kind"].as_str().and_then(GapKind::parse);
                let id = m["id"].as_str();
                if let (Some(kind), Some(id)) = (kind, id)
                    && let Some(g) = open.iter().find(|g| g.kind == kind && g.id == id)
                {
                    resolved.push(g.clone());
                    unclustered.retain(|u| !(u.kind == kind && u.id == id));
                }
            }
            if !resolved.is_empty() {
                rows.push(GapRow { label: r.get("label"), labelled_by: r.get("labelled_by"), members: resolved });
            }
        }
        Ok((rows, unclustered))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::asks::{AskVerdict, NewAsk};
    use crate::store::feedback::{Door, NewEvent, Verdict};

    async fn nothing_here(store: &Store, q: &str, vec: Vec<f32>) -> String {
        let id = store
            .record_ask(NewAsk {
                question: q.into(), scope: None, filters: "{}".into(), query_vec: vec,
                embed_model: "fake".into(), answer: "Not in the knowledge base.".into(),
                abstained: true, dropped: 0, truncated: false, citations: vec![],
            })
            .await
            .unwrap();
        store.judge_ask(&id, AskVerdict::NothingHere).await.unwrap();
        id
    }

    async fn gap_search(store: &Store, q: &str, vec: Vec<f32>) -> String {
        let id = store
            .record_search(
                NewEvent { query: q.into(), door: Door::Api, scope: None, filters: "{}".into(),
                           query_vec: vec, embed_model: "fake".into(), candidates: vec![] },
                0,
            )
            .await
            .unwrap();
        store.judge(&id, Verdict::Gap).await.unwrap();
        id
    }

    #[tokio::test]
    async fn open_gaps_are_the_unanswered_questions_and_the_gap_searches_under_this_model() {
        let store = Store::memory().await.unwrap();
        nothing_here(&store, "q1", vec![1.0, 0.0]).await;
        gap_search(&store, "s1", vec![0.0, 1.0]).await;
        // Not gaps: a right answer, an unjudged search, an empty vector.
        let right = store.record_ask(NewAsk {
            question: "ok".into(), scope: None, filters: "{}".into(), query_vec: vec![1.0, 1.0],
            embed_model: "fake".into(), answer: "yes".into(), abstained: false, dropped: 0,
            truncated: false, citations: vec![] }).await.unwrap();
        store.judge_ask(&right, AskVerdict::Right).await.unwrap();
        nothing_here(&store, "no vector", vec![]).await;
        let gaps = store.open_gaps("fake").await.unwrap();
        assert_eq!(gaps.iter().map(|g| g.text.as_str()).collect::<Vec<_>>(), vec!["q1", "s1"]);
        assert!(store.open_gaps("other-model").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_dismissed_gap_is_no_longer_open() {
        let store = Store::memory().await.unwrap();
        let a = nothing_here(&store, "q1", vec![1.0]).await;
        let s = gap_search(&store, "s1", vec![1.0]).await;
        store.dismiss_gap(GapKind::Ask, &a).await.unwrap();
        assert_eq!(store.open_gaps("fake").await.unwrap().len(), 1);
        store.dismiss_gap(GapKind::Search, &s).await.unwrap();
        assert!(store.open_gaps("fake").await.unwrap().is_empty());
        assert!(matches!(store.dismiss_gap(GapKind::Ask, "nope").await, Err(Error::NotFound)));
    }

    #[tokio::test]
    async fn rows_resolve_members_and_report_what_no_cluster_names_yet() {
        let store = Store::memory().await.unwrap();
        let a = nothing_here(&store, "q1", vec![1.0]).await;
        let b = nothing_here(&store, "q2", vec![1.0]).await;
        let later = nothing_here(&store, "q3", vec![1.0]).await;
        store.put_cluster(&GapCluster {
            key: "k".into(), label: "Mounting".into(), labelled_by: "model".into(),
            members: vec![(GapKind::Ask, a.clone()), (GapKind::Ask, b.clone())],
        }).await.unwrap();
        let (rows, loose) = store.gap_rows("fake").await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "Mounting");
        assert_eq!(rows[0].members.len(), 2);
        assert_eq!(loose.iter().map(|g| g.id.as_str()).collect::<Vec<_>>(), vec![later.as_str()]);

        // Dismissing a member thins the row; dismissing both removes it.
        store.dismiss_gap(GapKind::Ask, &a).await.unwrap();
        assert_eq!(store.gap_rows("fake").await.unwrap().0[0].members.len(), 1);
        store.dismiss_gap(GapKind::Ask, &b).await.unwrap();
        assert!(store.gap_rows("fake").await.unwrap().0.is_empty());
    }

    #[tokio::test]
    async fn clusters_can_be_listed_replaced_and_deleted() {
        let store = Store::memory().await.unwrap();
        let c = GapCluster { key: "k".into(), label: "x".into(), labelled_by: "terms".into(), members: vec![] };
        store.put_cluster(&c).await.unwrap();
        store.put_cluster(&GapCluster { label: "y".into(), labelled_by: "model".into(), ..c.clone() }).await.unwrap();
        assert_eq!(store.cluster_keys().await.unwrap(), vec![("k".to_string(), "model".to_string())]);
        store.delete_clusters(&["k".into()]).await.unwrap();
        assert!(store.cluster_keys().await.unwrap().is_empty());
    }
}
```

Register `pub mod gaps;` in `src/store/mod.rs` and `pub mod gaps;` in `src/core/mod.rs`. Check `NewEvent`'s fields against `feedback.rs:112` and match them exactly.

- [ ] **Step 4: Run** — `cargo test --lib gaps 2>&1 | grep -E "^test result|FAILED|panicked"` → pass.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --all-targets 2>&1 | grep -E "^(warning|error)"; git add -A && git commit -m "feat(gaps): open gaps from both doors, grouped by their stored vectors

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 9: The gap sweep and its namer

**Files:**
- Modify: `src/infer/prompt.rs` (label prompt + schema), `src/infer/openai.rs` (`for_gap_naming`)
- Modify: `src/core/mod.rs` (`gap_namer` field; `from_config`; `test_support::build`), `tests/eval.rs:320` (Core literal)
- Create: `src/jobs/gaps.rs`; register `pub mod gaps;` in `src/jobs/mod.rs`
- Modify: `src/core/background.rs:254-287` (retention ticker)

**Interfaces:**
- Produces: `pub async fn sweep(core: &Core) -> Result<SweepReport>` with `pub struct SweepReport { pub clusters: usize, pub named: usize, pub removed: usize }`; `Core.gap_namer: Arc<dyn Completer>`; `GAP_LABEL_SYSTEM`, `gap_label_prompt(&[&str]) -> String`, `gap_label_schema()`, `parse_gap_label(&str) -> Result<String>`.

- [ ] **Step 1: Prompt, schema, parser in `src/infer/prompt.rs`**

```rust
/// Names a knowledge gap from the questions in it. Sees questions only, never
/// answers: it names the hole, not the guess.
pub const GAP_LABEL_SYSTEM: &str = r#"You name topics. Given several questions a knowledge base could not answer, reply with the name of the subject they share — three to six words, a noun phrase, no quotes, no trailing punctuation. Reply with JSON only: {"label":"…"}"#;

pub fn gap_label_prompt(questions: &[&str]) -> String {
    let mut s = String::from("Questions:\n");
    for q in questions {
        s.push_str("- ");
        s.push_str(q);
        s.push('\n');
    }
    s
}

pub fn gap_label_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": { "label": {"type": "string"} },
        "required": ["label"],
        "additionalProperties": false
    })
}

/// The label out of the reply, trimmed of quotes and trailing punctuation;
/// an empty label is an error, because a cluster must be called something.
pub fn parse_gap_label(reply: &str) -> Result<String> {
    let v: serde_json::Value = serde_json::from_str(extract_json(reply))
        .map_err(|e| Error::MalformedLlmOutput(format!("gap label was not JSON: {e}")))?;
    let label = v["label"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .trim_matches(|c: char| c == '"' || c == '\'' || c == '.')
        .trim()
        .to_string();
    if label.is_empty() {
        return Err(Error::MalformedLlmOutput("gap label was empty".into()));
    }
    Ok(label)
}
```

Tests in the prompt tests module:

```rust
    #[test]
    fn a_gap_label_is_read_out_of_the_envelope_and_tidied() {
        assert_eq!(parse_gap_label(r#"{"label": "\"Forensic image mounting.\""}"#).unwrap(), "Forensic image mounting");
        assert!(parse_gap_label(r#"{"label": ""}"#).is_err());
        assert!(parse_gap_label("nope").is_err());
    }
```

Add `("gap_label", gap_label_schema())` to `every_judge_schema_object_is_closed_and_rooted_in_an_object`. In `openai.rs`: `pub fn for_gap_naming(cfg: &SynthesizeRole) -> Self { Self::judging(cfg, ("gap_label", prompt::gap_label_schema())) }`.

- [ ] **Step 2: `gap_namer` on `Core`**

`src/core/mod.rs`: field after `link_judge`:

```rust
    /// The model that names a knowledge gap from the questions in it. Same
    /// endpoint as the judges, its own response shape, background only.
    pub gap_namer: Arc<dyn Completer>,
```

`from_config`: `gap_namer: Arc::new(HttpCompleter::for_gap_naming(&cfg.infer.synthesize)),`. `test_support::build`: `gap_namer: Arc::new(FakeCompleter { reply: Some(r#"{"label":"Fake topic"}"#.into()) }),`. `tests/eval.rs` Core literal: `gap_namer: Arc::new(engram::infer::fake::FakeCompleter::default()),`.

- [ ] **Step 3: Failing tests, then the sweep** — create `src/jobs/gaps.rs`:

```rust
//! Group the open gaps and name the new groups.
//!
//! Runs on the retention ticker. Clustering is free; naming costs one
//! efficient-tier call per cluster that did not exist before — membership is
//! identity, so the same members are never named twice — and a cluster named
//! by terms because the model was unavailable is offered to the model again
//! next pass.

use crate::core::Core;
use crate::core::gaps::{GAP_LINK_AT, cluster, cluster_key, terms_label};
use crate::error::Result;
use crate::store::gaps::GapCluster;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub clusters: usize,
    pub named: usize,
    pub removed: usize,
}

pub async fn sweep(core: &Core) -> Result<SweepReport> {
    let gaps = core.store.open_gaps(core.embedder.model()).await?;
    let vecs: Vec<Vec<f32>> = gaps.iter().map(|g| g.vec.clone()).collect();
    let groups = cluster(&vecs, GAP_LINK_AT);

    let existing = core.store.cluster_keys().await?;
    let mut report = SweepReport { clusters: groups.len(), ..Default::default() };
    let mut live_keys = Vec::with_capacity(groups.len());

    for group in &groups {
        let members: Vec<_> = group.iter().map(|&i| (gaps[i].kind, gaps[i].id.clone())).collect();
        let key = cluster_key(&members);
        live_keys.push(key.clone());
        let known = existing.iter().find(|(k, _)| *k == key).map(|(_, by)| by.as_str());
        if known == Some("model") {
            continue;
        }
        let texts: Vec<&str> = group.iter().map(|&i| gaps[i].text.as_str()).collect();
        let (label, labelled_by) = match name(core, &texts).await {
            Some(l) => (l, "model"),
            None => (terms_label(&texts), "terms"),
        };
        if labelled_by == "model" {
            report.named += 1;
        }
        core.store
            .put_cluster(&GapCluster { key, label, labelled_by: labelled_by.into(), members })
            .await?;
    }

    let stale: Vec<String> = existing
        .into_iter()
        .map(|(k, _)| k)
        .filter(|k| !live_keys.contains(k))
        .collect();
    report.removed = stale.len();
    core.store.delete_clusters(&stale).await?;
    Ok(report)
}

/// One bounded call under the background lane. Any failure — endpoint down,
/// unreadable reply — is `None`, and the caller falls back to terms.
async fn name(core: &Core, questions: &[&str]) -> Option<String> {
    let permit = core.gate.background().await;
    let reply = core
        .gap_namer
        .complete(
            crate::infer::prompt::GAP_LABEL_SYSTEM,
            &crate::infer::prompt::gap_label_prompt(questions),
        )
        .await;
    permit.finished();
    match reply.and_then(|r| crate::infer::prompt::parse_gap_label(&r)) {
        Ok(label) => Some(label),
        Err(e) => {
            tracing::warn!(error = %e, "could not name a knowledge gap; using its terms");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_support::test_core;
    use crate::store::asks::{AskVerdict, NewAsk};
    use crate::store::gaps::GapKind;

    async fn nothing_here(core: &Core, q: &str, vec: Vec<f32>) -> String {
        let id = core.store.record_ask(NewAsk {
            question: q.into(), scope: None, filters: "{}".into(), query_vec: vec,
            embed_model: core.embedder.model().to_string(), answer: "Not in the knowledge base.".into(),
            abstained: true, dropped: 0, truncated: false, citations: vec![],
        }).await.unwrap();
        core.store.judge_ask(&id, AskVerdict::NothingHere).await.unwrap();
        id
    }

    #[tokio::test]
    async fn a_new_cluster_is_named_once_and_a_vanished_one_is_removed() {
        let core = test_core().await;
        let a = nothing_here(&core, "mount an E01", vec![1.0, 0.0]).await;
        nothing_here(&core, "mounting E01 images", vec![0.95, 0.05]).await;
        nothing_here(&core, "FAT entries", vec![0.0, 1.0]).await;

        let r = sweep(&core).await.unwrap();
        assert_eq!(r, SweepReport { clusters: 2, named: 2, removed: 0 });
        let (rows, loose) = core.store.gap_rows(core.embedder.model()).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert!(loose.is_empty());
        assert!(rows.iter().all(|r| r.label == "Fake topic" && r.labelled_by == "model"));

        // Same members, no new call.
        assert_eq!(sweep(&core).await.unwrap().named, 0);

        // Dismissing one member changes the cluster: the old key goes, the
        // new one is named.
        core.store.dismiss_gap(GapKind::Ask, &a).await.unwrap();
        let r = sweep(&core).await.unwrap();
        assert_eq!((r.clusters, r.named, r.removed), (2, 1, 1));
    }

    #[tokio::test]
    async fn without_a_readable_model_a_cluster_is_named_by_its_terms_and_retried_later() {
        let mut core = test_core().await;
        core.gap_namer = std::sync::Arc::new(crate::infer::fake::FakeCompleter { reply: Some("not json".into()) });
        nothing_here(&core, "mount an E01 image", vec![1.0]).await;
        nothing_here(&core, "E01 mount read only", vec![1.0]).await;
        let r = sweep(&core).await.unwrap();
        assert_eq!((r.clusters, r.named), (1, 0));
        let (rows, _) = core.store.gap_rows(core.embedder.model()).await.unwrap();
        assert_eq!(rows[0].labelled_by, "terms");
        assert!(rows[0].label.contains("e01"), "{}", rows[0].label);

        core.gap_namer = std::sync::Arc::new(crate::infer::fake::FakeCompleter { reply: Some(r#"{"label":"Image mounting"}"#.into()) });
        assert_eq!(sweep(&core).await.unwrap().named, 1);
        assert_eq!(core.store.gap_rows(core.embedder.model()).await.unwrap().0[0].label, "Image mounting");
    }

    #[tokio::test]
    async fn no_gaps_means_no_clusters_and_no_calls() {
        let core = test_core().await;
        assert_eq!(sweep(&core).await.unwrap(), SweepReport::default());
    }
}
```

Check `permit.finished()` exists on what `gate.background()` returns (it is used in `associate.rs:391-401`); copy that usage exactly.

- [ ] **Step 4: The ticker**

Rewrite `spawn_retention_ticker` in `background.rs` so it always runs (drop the `retain_days <= 0` early return; log `"captured searches kept indefinitely"` once instead), and in the tick arm run both:

```rust
                _ = tick.tick() => {
                    if core.feedback.retain_days > 0 {
                        match core.store.expire_feedback(core.feedback.retain_days).await {
                            Ok(n) if n > 0 => tracing::info!(dropped = n, "expired captured searches and questions"),
                            Err(e) => tracing::warn!(error = %e, "could not expire captured searches"),
                            _ => {}
                        }
                    }
                    // Grouping the holes rides the same rhythm: it reads the
                    // same tables, and hours is the right cadence for
                    // something a person looks at when they next capture.
                    if core.feedback.enabled {
                        match crate::jobs::gaps::sweep(&core).await {
                            Ok(r) if r.named > 0 || r.removed > 0 => tracing::info!(clusters = r.clusters, named = r.named, removed = r.removed, "knowledge gaps regrouped"),
                            Err(e) => tracing::warn!(error = %e, "could not group knowledge gaps"),
                            _ => {}
                        }
                    }
                }
```

Update the doc comment above it to say it also groups gaps. Grep `spawn_retention_ticker` for any test asserting the early return and adjust.

- [ ] **Step 5: Run** — `cargo test 2>&1 | grep -E "^test result|FAILED|panicked"` → all pass; `cargo test --test eval --no-run` builds.

- [ ] **Step 6: Commit**

```bash
cargo fmt && cargo clippy --all-targets 2>&1 | grep -E "^(warning|error)"; git add -A && git commit -m "feat(gaps): group the open gaps on the retention ticker and name each new group once

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 10: Knowledge gaps on the capture page

**Files:**
- Create: `src/web/templates/_gaps.html`
- Modify: `src/web/templates/capture.html`, `src/web/templates/ask.html`, `src/web/ui.rs` (`CaptureTemplate`, `capture_page`, `ask_page`, dismiss route), `assets/app.css`

**Interfaces:**
- Consumes: `Store::gap_rows`, `Store::dismiss_gap`, `GapKind::parse`.
- Produces: `POST /ui/gaps/{kind}/{id}/dismiss` → empty 200 (the row removes itself); `GET /ui/ask?q=…` prefills the box.

- [ ] **Step 1: Failing tests in `ui.rs`**

```rust
    #[tokio::test]
    async fn the_capture_page_lists_knowledge_gaps_by_group_and_lets_one_be_covered() {
        let (app, cookie, core) = app_session_and_core_with_feedback().await;
        let id = core.store.record_ask(crate::store::asks::NewAsk {
            question: "how do I mount an E01".into(), scope: None, filters: "{}".into(),
            query_vec: vec![1.0; 8], embed_model: core.embedder.model().to_string(),
            answer: "Not in the knowledge base.".into(), abstained: true, dropped: 0,
            truncated: false, citations: vec![],
        }).await.unwrap();
        core.store.judge_ask(&id, crate::store::asks::AskVerdict::NothingHere).await.unwrap();

        // Before the sweep: listed, not yet grouped.
        let page = get_body(&app, &cookie, "/ui/capture").await;
        assert!(page.contains("Knowledge gaps"), "{page}");
        assert!(page.contains("not yet grouped"), "{page}");
        assert!(page.contains("mount an E01"), "{page}");

        crate::jobs::gaps::sweep(&core).await.unwrap();
        let page = get_body(&app, &cookie, "/ui/capture").await;
        assert!(page.contains("Fake topic"), "{page}");
        assert!(page.contains(&format!("/ui/gaps/ask/{id}/dismiss")), "{page}");
        assert!(page.contains("/ui/ask?q=how+do+I+mount+an+E01") || page.contains("/ui/ask?q=how%20do%20I%20mount%20an%20E01"), "{page}");

        let res = app.clone().oneshot(form(&format!("/ui/gaps/ask/{id}/dismiss"), &cookie, "")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let page = get_body(&app, &cookie, "/ui/capture").await;
        assert!(!page.contains("Knowledge gaps"), "a covered gap must leave the page: {page}");
    }

    #[tokio::test]
    async fn the_capture_page_shows_no_gaps_block_when_feedback_is_off() {
        let (app, cookie) = app_with_session().await;
        let page = get_body(&app, &cookie, "/ui/capture").await;
        assert!(!page.contains("Knowledge gaps"), "{page}");
    }

    #[tokio::test]
    async fn the_ask_page_prefills_a_question_from_the_query_string() {
        let (app, cookie) = app_with_session().await;
        let page = get_body(&app, &cookie, "/ui/ask?q=mount+an+E01").await;
        assert!(page.contains(r#"value="mount an E01""#), "{page}");
    }
```

- [ ] **Step 2: Template `_gaps.html`**

```html
{# The holes, where capturing happens. A group is a name over the questions
   in it; a question not yet grouped is shown under itself until the sweep
   runs. "covered" is the operator's word, never the system's. #}
{% if !gaps.is_empty() || !loose.is_empty() %}
<h3>Knowledge gaps</h3>
<div class="gaps">
  {% for g in gaps %}
  <details class="gap">
    <summary>{{ g.label }} <span class="muted">({{ g.members.len() }})</span></summary>
    <ul>
      {% for m in g.members %}
      <li id="gap-{{ m.kind }}-{{ m.id }}" class="row">
        <span>{{ m.text }}</span>
        <a class="btn btn-ghost btn-sm" href="/ui/ask?q={{ m.text|urlencode }}">ask again</a>
        <button class="btn btn-ghost btn-sm" hx-post="/ui/gaps/{{ m.kind }}/{{ m.id }}/dismiss"
                hx-target="closest li" hx-swap="outerHTML">covered</button>
      </li>
      {% endfor %}
    </ul>
  </details>
  {% endfor %}
  {% if !loose.is_empty() %}
  <details class="gap">
    <summary>not yet grouped <span class="muted">({{ loose.len() }})</span></summary>
    <ul>
      {% for m in loose %}
      <li id="gap-{{ m.kind }}-{{ m.id }}" class="row">
        <span>{{ m.text }}</span>
        <a class="btn btn-ghost btn-sm" href="/ui/ask?q={{ m.text|urlencode }}">ask again</a>
        <button class="btn btn-ghost btn-sm" hx-post="/ui/gaps/{{ m.kind }}/{{ m.id }}/dismiss"
                hx-target="closest li" hx-swap="outerHTML">covered</button>
      </li>
      {% endfor %}
    </ul>
  </details>
  {% endif %}
</div>
{% endif %}
```

Askama needs plain fields: define view models in `ui.rs`:

```rust
pub struct GapMember { pub kind: String, pub id: String, pub text: String }
pub struct GapGroup { pub label: String, pub members: Vec<GapMember> }
```

and convert from `GapRow`/`Gap` (`kind: g.kind.as_str().into()`). `CaptureTemplate` gains `gaps: Vec<GapGroup>, loose: Vec<GapMember>`; `capture_page` fills them only when `st.core.feedback.enabled` (else both empty) via `st.core.store.gap_rows(st.core.embedder.model()).await?`. In `capture.html`, include `{% include "_gaps.html" %}` between the "Needs you" block and `<h3>Recent</h3>`.

CSS: `.gaps details { margin: .25rem 0 } .gaps summary { cursor: pointer } .gap li { gap: .5rem; align-items: baseline; margin: .25rem 0 }`.

- [ ] **Step 3: Routes**

```rust
async fn gap_dismiss(
    State(st): State<AppState>,
    _id: Identity,
    Path((kind, id)): Path<(String, String)>,
) -> Result<Response> {
    let kind = crate::store::gaps::GapKind::parse(&kind)
        .ok_or_else(|| crate::error::Error::Validation(format!("unknown gap kind {kind}")))?;
    st.core.store.dismiss_gap(kind, &id).await?;
    Ok(StatusCode::OK.into_response())
}
```

Register `.route("/ui/gaps/{kind}/{id}/dismiss", post(gap_dismiss))`.

`ask_page`: add `Query(p): Query<AskPrefill>` with `#[derive(serde::Deserialize)] struct AskPrefill { #[serde(default)] q: String }`; `AskTemplate` gains `q: String`; in `ask.html` the input becomes `<input class="input" name="q" value="{{ q }}" placeholder="Ask a question…" autofocus>`.

- [ ] **Step 4: Run** — `cargo test --lib web:: 2>&1 | grep -E "^test result|FAILED|panicked"` → pass. If the `ask again` href assertion fails on encoding, print the page and match Askama's `urlencode` output (`+` vs `%20`) — either is acceptable; fix the assertion, not the filter.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --all-targets 2>&1 | grep -E "^(warning|error)"; git add -A && git commit -m "feat(ui): knowledge gaps where capturing happens, grouped and named, each coverable

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 11: Documentation

**Files:**
- Modify: `README.md` (section "Learning what the search got wrong", and the Ask mention if any), `ROADMAP.md`

- [ ] **Step 1: README**

Under "Learning what the search got wrong", add a paragraph:

```markdown
Questions get the same treatment. With `feedback.enabled`, every question asked
on the ask page is recorded with the excerpts the model saw, and the answer
carries a verdict bar — right, wrong, nothing here — plus a "carried the
answer" toggle on each excerpt. An answer that opens with *Not in the knowledge
base* is an abstention and is marked as one. `--export-eval` writes the judged
questions to `questions.json`, and

    ENGRAM_EVAL_DIR=~/engram-eval cargo test --test eval evaluate_ask -- --ignored --nocapture

measures citation recall, abstention accuracy and faithfulness by literals;
`ENGRAM_EVAL_CLAIMS=1` adds a claim-by-claim check by the synthesize model.
Questions judged "nothing here" and searches judged `gap` are the base's holes:
they are grouped by meaning, named once by the synthesize model, and listed as
**Knowledge gaps** on the capture page until you mark them covered.
```

- [ ] **Step 2: ROADMAP**

- In "What is built", after the ask endpoint clause: `; judged questions with a second harness — citation recall, abstention, faithfulness by literals and by claim check; knowledge gaps grouped and named on the capture page`.
- Remove item 1 from the [Ask] list and renumber 2–4 to 1–3; add under the [Ask] intro: `Item 1 of the original list — the ask harness and ask feedback — is built (spec 2026-08-17-ask-harness-design.md); the numbers below now exist.`
- The tiers paragraph: note `HttpCompleter::for_claim_checking` / `for_gap_naming` already run on the synthesize (efficient) endpoint, so the tier split is now a rename waiting for the first item that needs the deep tier elsewhere.

- [ ] **Step 3: Full verification and commit**

```bash
cargo fmt && cargo clippy --all-targets 2>&1 | grep -E "^(warning|error)"; cargo test 2>&1 | grep -E "^test result|FAILED"; git add -A && git commit -m "docs: the ask harness, ask feedback and knowledge gaps

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## Self-review

- **Spec coverage:** §4 → T1, T8; §5 → T3; §6 → T2, T3; §7 → T1, T4; §8 → T5; §9 → T6, T7; §10 → T8, T9, T10; §11 (no config) honoured; §12 tests are spread per task; §13 order matches; README/ROADMAP → T11.
- **Types:** `NewAsk`/`NewAskCitation`/`AskVerdict`/`AskEvent`/`AskStats` (T1) are used by name in T3, T4, T5, T8, T9, T10. `GapKind`/`Gap`/`GapCluster`/`GapRow` (T8) used in T9, T10. `SweepReport` (T9) used in T10's test. `EvalQuestion`/`load_questions` (T5) used in T7. `fraction_cited`/`Abstention`/`fully_supported` (T6) used in T7. `Origin`/`Door` are existing.
- **Known implementer judgement calls (not placeholders):** template error mapping in T4 (copy what `ui.rs` already does for askama errors); the `permit.finished()` call shape in T9 (copy `associate.rs`); the URL-encoding assertion in T10.
