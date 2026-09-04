# The Quiet Workspace Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the workspace's idle state one quiet column, retire reminder notes that are done, let the due band populate itself while you watch, and teach the reminder and journal phrasings in the page.

**Architecture:** Four independent slices over the existing Askama + htmx + axum web layer. Retirement is one nullable column on `corpora` plus a demotion in the search fold. The idle page is a template restructure plus one app.js change from `remove()` to hidden. The due band gains a self-computed polling delay of the kind `_queue.html` already uses. Guidance rides out-of-band on the search response that every keystroke already makes.

**Tech Stack:** Rust 1.94 (edition 2024), axum 0.8, sqlx 0.9 over SQLite, Askama templates in `src/web/templates/`, htmx 2 plus hand-written `assets/app.js`, plain CSS in `assets/css/`.

**Spec:** `docs/superpowers/specs/2026-08-30-workspace-quiet-design.md`

## Global Constraints

- Build and test with plain `cargo test <args>`. `./build-lowmem.sh` is for the 2 GB deployment box: it links through the toolchain's bundled lld, which the distro rustc on this machine does not ship (`collect2: fatal error: cannot find 'ld'`). This machine has 31 GB and 12 cores and needs none of it.
- A dynamic `IN (…)` list must be wrapped in `sqlx::AssertSqlSafe`; sqlx 0.9 refuses a `&str` built at runtime. `src/store/moments.rs:190` is the idiom to copy.
- No model call, no embedding call, and no network on any path this plan touches. `cue()`, the date rules and `when_words` are pure functions and must stay that way.
- Nothing is ever deleted. Retirement is a nullable flag with an undo; there is no delete path anywhere in this plan.
- Every user-visible sentence is written in the voice of the existing templates: plain, lowercase-after-the-first-word, no exclamation marks, no "please". Copy strings in this plan are exact and are not to be improved on.
- Askama templates carry `{# … #}` comments explaining *why* a construct is the way it is, matching the density already in `workspace.html` and `_due.html`. A template edit with no comment where the surrounding file comments is incomplete.
- `cargo clippy` is not installed on this machine. Every task ends with `cargo check --all-targets` clean before its commit; run clippy wherever it is available before the branch is merged.

---

### Task 1: `corpora.retired_at`, and the store reads that honour it

**Files:**
- Modify: `src/store/schema.sql:15-39` (the `corpora` table)
- Modify: `src/store/mod.rs:150-179` (the `ADDITIVE` migration list)
- Modify: `src/store/corpora.rs:588-599` (`recent_captures`)
- Test: `src/store/corpora.rs` (its own `#[cfg(test)] mod tests`), `src/store/mod.rs` tests

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `Store::retire_corpus(&self, corpus_id: &str, at: i64) -> Result<()>`
  - `Store::unretire_corpus(&self, corpus_id: &str) -> Result<()>`
  - `Store::is_retired(&self, corpus_id: &str) -> Result<bool>`
  - `recent_captures` keeps its signature `(limit: i64) -> Result<Vec<(String, Option<String>, String, i64, String)>>` and silently excludes retired corpora.

- [ ] **Step 1: Write the failing test**

Add to the tests module in `src/store/corpora.rs`:

```rust
#[tokio::test]
async fn a_retired_corpus_leaves_the_recent_list_and_comes_back_on_undo() {
    let store = test_store().await;
    let id = insert_test_corpus(&store, "remind me friday to send the invoice").await;
    assert_eq!(store.recent_captures(5).await.unwrap().len(), 1);

    store.retire_corpus(&id, 1_700_000_000).await.unwrap();
    assert!(store.recent_captures(5).await.unwrap().is_empty(), "a retired note is not a recent capture");
    assert!(store.is_retired(&id).await.unwrap());

    store.unretire_corpus(&id).await.unwrap();
    assert_eq!(store.recent_captures(5).await.unwrap().len(), 1, "undo puts it back");
    assert!(!store.is_retired(&id).await.unwrap());
}
```

Use whatever `test_store()` / corpus-insert helper the surrounding tests in that file already use; do not invent a new one. Read the module's existing tests first and match them.

- [ ] **Step 2: Run it and watch it fail**

Run: `./build-lowmem.sh test --lib store::corpora::tests::a_retired_corpus_leaves_the_recent_list -- --nocapture`
Expected: FAIL, `no method named 'retire_corpus'`.

- [ ] **Step 3: Add the column to the schema**

In `src/store/schema.sql`, inside `CREATE TABLE IF NOT EXISTS corpora`, after `restored_at INTEGER,`:

```sql
  -- Set when the last reminder read out of this note was completed. The note
  -- stays: it is still searchable, still on its day page, still the corpus its
  -- artifacts belong to. What it stops being is a *recent capture* and a
  -- competitor in the ranked half of a result list. NULL is the ordinary
  -- state, and `undone` writes NULL back.
  retired_at      INTEGER,
```

- [ ] **Step 4: Make the migration additive**

`migrate()` refuses to start against a base missing any schema column unless the column is on the `ADDITIVE` list. `retired_at` qualifies for exactly the reason the list documents: it is nullable with no default, and NULL on an existing row says "no reminder on this note has been completed", which is true of every row written before the column existed.

In `src/store/mod.rs`, change the array's length from `4` to `5` and append:

```rust
            // Nullable, no default, and NULL is the truth about every row that
            // predates it: no reminder on that note had been completed,
            // because nothing could record that it had.
            (
                "corpora",
                "retired_at",
                "ALTER TABLE corpora ADD COLUMN retired_at INTEGER",
            ),
```

- [ ] **Step 5: Write the three store methods**

In `src/store/corpora.rs`, beside `recent_captures`:

```rust
    /// The last reminder read out of this note is done, so the note stops
    /// being *recent*. Not a delete and not a hide: see `schema.sql`.
    pub async fn retire_corpus(&self, corpus_id: &str, at: i64) -> Result<()> {
        sqlx::query("UPDATE corpora SET retired_at = ? WHERE id = ?")
            .bind(at)
            .bind(corpus_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The undo already on screen beside a completed reminder.
    pub async fn unretire_corpus(&self, corpus_id: &str) -> Result<()> {
        sqlx::query("UPDATE corpora SET retired_at = NULL WHERE id = ?")
            .bind(corpus_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn is_retired(&self, corpus_id: &str) -> Result<bool> {
        let at: Option<Option<i64>> =
            sqlx::query_scalar("SELECT retired_at FROM corpora WHERE id = ?")
                .bind(corpus_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(matches!(at, Some(Some(_))))
    }
```

- [ ] **Step 6: Exclude retired corpora from `recent_captures`**

Replace the SQL in `recent_captures` with:

```rust
            "SELECT id, title_hint, origin, created_at, substr(raw_text, 1, 400) \
             FROM corpora WHERE retired_at IS NULL \
             ORDER BY created_at DESC, id DESC LIMIT ?",
```

Add above the function:

```rust
    /// Newest first, retired notes excluded — a reminder that is done is not
    /// one of the last things you kept. The day page is where it stays
    /// visible, because a day is a record of what actually happened.
```

- [ ] **Step 7: Run the test and the migration tests**

Run: `./build-lowmem.sh test --lib store::`
Expected: PASS, including the existing migration tests that assert a dropped column is re-added rather than refused.

- [ ] **Step 8: Commit**

```bash
git add src/store/schema.sql src/store/mod.rs src/store/corpora.rs
git commit -m "feat(time): a corpus can be retired, and a retired one is not recent"
```

---

### Task 2: Completing the last read reminder retires its note

**Files:**
- Modify: `src/core/moments.rs:571-593` (`complete_moment`)
- Modify: `src/web/due.rs:120-124` (`undone`)
- Create: `Store::corpus_of_moment`, `Store::has_open_moment_for_corpus`, `Store::corpus_was_read_as_reminder` in `src/store/moments.rs`
- Test: `src/web/due.rs` tests, `src/core/moments.rs` tests

**Interfaces:**
- Consumes: `Store::retire_corpus`, `Store::unretire_corpus` from Task 1.
- Produces:
  - `Store::corpus_of_moment(&self, moment_id: &str) -> Result<Option<String>>`
  - `Store::has_open_moment_for_corpus(&self, corpus_id: &str) -> Result<bool>`
  - `Store::corpus_was_read_as_reminder(&self, corpus_id: &str) -> Result<bool>`

- [ ] **Step 1: Write the failing tests**

In `src/web/due.rs` tests, beside `done_strikes_the_row_and_undo_restores_it`. `artifact_with_due` in that module already inserts with `Source::Cue`, which is the read-out-of-the-note case:

```rust
    #[tokio::test]
    async fn done_retires_a_note_that_was_read_as_a_reminder() {
        let core = test_core().await;
        let id = artifact_with_due(&core, Some(crate::store::now() + 60)).await;
        let cid = core.store.corpus_of_moment(&id).await.unwrap().unwrap();
        let (app, cookie) = app_with_cookie(core.clone()).await;

        app.clone().oneshot(form(&format!("/ui/moments/{id}/done"), &cookie, "tz=Europe/Berlin")).await.unwrap();
        assert!(core.store.is_retired(&cid).await.unwrap(), "the last read reminder closed, so the note retires");

        app.oneshot(form(&format!("/ui/moments/{id}/undone"), &cookie, "tz=Europe/Berlin")).await.unwrap();
        assert!(!core.store.is_retired(&cid).await.unwrap(), "undo restores the row and the note together");
    }

    #[tokio::test]
    async fn a_recurring_done_retires_nothing_because_the_next_one_is_open() {
        let core = test_core().await;
        let out = core.ingest_capture(Capture::new("Pay rent", "ui")).await.unwrap();
        crate::jobs::test_support::drain(&core).await;
        let aid = core.store.artifacts_for_corpus(&out.id).await.unwrap()[0].id.clone();
        let at = chrono_tz::Tz::Europe__Berlin.with_ymd_and_hms(2026, 9, 1, 9, 0, 0).unwrap().timestamp();
        let id = core
            .store
            .insert_moment(&NewMoment {
                artifact_id: aid,
                kind: Kind::Due,
                at: Some(at),
                tz: "Europe/Berlin".into(),
                rule: Some("FREQ=MONTHLY;BYMONTHDAY=1".into()),
                source: Source::Cue,
                span: None,
            })
            .await
            .unwrap();
        let (app, cookie) = app_with_cookie(core.clone()).await;
        app.oneshot(form(&format!("/ui/moments/{id}/done"), &cookie, "tz=Europe/Berlin")).await.unwrap();
        assert!(!core.store.is_retired(&out.id).await.unwrap(), "an occurrence closed, the reminder did not");
    }

    #[tokio::test]
    async fn a_hand_set_date_on_an_ordinary_note_does_not_retire_it() {
        let core = test_core().await;
        let out = core.ingest_capture(Capture::new("An article about vector indexes", "ui")).await.unwrap();
        crate::jobs::test_support::drain(&core).await;
        let aid = core.store.artifacts_for_corpus(&out.id).await.unwrap()[0].id.clone();
        let id = core
            .store
            .insert_moment(&NewMoment {
                artifact_id: aid,
                kind: Kind::Due,
                at: Some(crate::store::now() + 60),
                tz: "Europe/Berlin".into(),
                rule: None,
                source: Source::Set,
                span: None,
            })
            .await
            .unwrap();
        let (app, cookie) = app_with_cookie(core.clone()).await;
        app.oneshot(form(&format!("/ui/moments/{id}/done"), &cookie, "tz=Europe/Berlin")).await.unwrap();
        assert!(!core.store.is_retired(&out.id).await.unwrap(), "a document with a date on it stays a document");
    }
```

- [ ] **Step 2: Run them and watch them fail**

Run: `./build-lowmem.sh test --lib web::due::tests`
Expected: FAIL, `no method named 'corpus_of_moment'`.

- [ ] **Step 3: Write the three store queries**

In `src/store/moments.rs`:

```rust
    /// A moment hangs off an artifact; the note is the artifact's corpus.
    pub async fn corpus_of_moment(&self, moment_id: &str) -> Result<Option<String>> {
        Ok(sqlx::query_scalar(
            "SELECT a.corpus_id FROM moments m JOIN artifacts a ON a.id = m.artifact_id \
             WHERE m.id = ?",
        )
        .bind(moment_id)
        .fetch_optional(&self.pool)
        .await?
        .flatten())
    }

    /// Anything still open on any artifact of this note. `complete_moment`
    /// arms the next occurrence of a recurring reminder before this is asked,
    /// so a recurring done answers `true` and retires nothing.
    pub async fn has_open_moment_for_corpus(&self, corpus_id: &str) -> Result<bool> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM moments m JOIN artifacts a ON a.id = m.artifact_id \
             WHERE a.corpus_id = ? AND m.done_at IS NULL",
        )
        .bind(corpus_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(n > 0)
    }

    /// Was this note ever *read* as a reminder, as opposed to given a date by
    /// a person? `source` already records exactly that: `cue` and `classified`
    /// are the two readings, `set` is a person, `extracted` is a date
    /// mentioned in passing prose. Every moment the note ever had is
    /// considered, because moving a cue reminder's date writes a fresh `set`
    /// row and the note does not stop having been a reminder.
    pub async fn corpus_was_read_as_reminder(&self, corpus_id: &str) -> Result<bool> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM moments m JOIN artifacts a ON a.id = m.artifact_id \
             WHERE a.corpus_id = ? AND m.source IN ('cue', 'classified')",
        )
        .bind(corpus_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(n > 0)
    }
```

If `moments.source` is stored through a `Source` enum with a different wire spelling than `'cue'`/`'classified'`, use `Source::Cue.as_str()` and `Source::Classified.as_str()` bound as parameters rather than literals. Check `src/store/moments.rs` for how `Source` serialises before writing this.

- [ ] **Step 4: Retire at the end of `complete_moment`**

In `src/core/moments.rs`, inside `complete_moment`, replace `self.store.rearm_remind().await?;` with:

```rust
            self.store.rearm_remind().await?;
            // Last, and only after the recurrence above has armed the next
            // occurrence: "no open moment remains" is a question about the
            // state this call leaves behind, not the one it found.
            if let Some(cid) = self.store.corpus_of_moment(id).await?
                && !self.store.has_open_moment_for_corpus(&cid).await?
                && self.store.corpus_was_read_as_reminder(&cid).await?
            {
                self.store.retire_corpus(&cid, now).await?;
            }
```

- [ ] **Step 5: Un-retire on undo**

In `src/web/due.rs`, `undone` becomes:

```rust
async fn undone(tenant: Tenant, Path(id): Path<String>, Form(f): Form<TzForm>) -> Result<Response> {
    tenant.core.store.undo_done(&id).await?;
    // The row comes back, so the note comes back with it. Unconditional: a
    // corpus that was never retired is already NULL here.
    if let Some(cid) = tenant.core.store.corpus_of_moment(&id).await? {
        tenant.core.store.unretire_corpus(&cid).await?;
    }
    tenant.core.store.rearm_remind().await?;
    render(&tenant, &f.tz, None).await
}
```

- [ ] **Step 6: Run the tests**

Run: `./build-lowmem.sh test --lib web::due::tests core::moments::`
Expected: PASS, all five due tests plus the three new ones.

- [ ] **Step 7: Commit**

```bash
git add src/store/moments.rs src/core/moments.rs src/web/due.rs
git commit -m "feat(time): the last reminder read out of a note retires it when done"
```

---

### Task 3: A retired note ranks below the fold, badged

**Files:**
- Modify: `src/core/search.rs:138-215` (`SearchResult`), `:1560-1580` (the due enrichment block), `:1714` (around `mark_past_cliff`)
- Modify: `src/web/ui.rs:25-75` (`RenderedResult`), `:1342-1388` (`render_hit`)
- Modify: `src/web/templates/_results.html:80-90` (the badge row)
- Test: `src/core/search.rs` tests

**Interfaces:**
- Consumes: `corpora.retired_at` from Task 1.
- Produces:
  - `SearchResult.retired: bool`
  - `RenderedResult.retired: bool`
  - `Store::retired_among(&self, corpus_ids: &[String]) -> Result<std::collections::HashSet<String>>`

- [ ] **Step 1: Write the failing test**

In `src/core/search.rs` tests, modelled on `a_hit_says_it_is_due_inside_the_horizon_and_not_outside` — copy that test's setup verbatim and change what it asserts:

```rust
    #[tokio::test]
    async fn a_retired_note_sinks_below_the_cliff_and_says_why() {
        let core = test_core().await;
        // Three ordinary notes so `cliff` has the three scores it needs, plus
        // the reminder that will be retired.
        for t in ["the invoice for august", "invoice terms and conditions", "invoice numbering scheme"] {
            core.ingest_capture(Capture::new(t, "ui")).await.unwrap();
        }
        let out = core.ingest_capture(Capture::new("remind me friday to send the invoice", "ui")).await.unwrap();
        crate::jobs::test_support::drain(&core).await;

        let before = core.search("invoice").await.unwrap();
        let pos = before.iter().position(|r| r.corpus_id == out.id).expect("it places while open");

        core.store.retire_corpus(&out.id, crate::store::now()).await.unwrap();
        let after = core.search("invoice").await.unwrap();
        let row = after.iter().find(|r| r.corpus_id == out.id).expect("still findable — nothing was deleted");
        assert!(row.retired, "the row says what it is");
        assert!(row.past_cliff, "and it is below the line");
        let last = after.len() - 1;
        assert_eq!(after[last].corpus_id, out.id, "demoted to the tail, so `ask` still truncates a tail");
        assert!(after.iter().position(|r| r.corpus_id == out.id).unwrap() > pos);
    }
```

`core.search(...)` is shorthand — use whatever entry point the neighbouring search tests in that file already call, with the same `Door` and origin arguments they pass.

- [ ] **Step 2: Run it and watch it fail**

Run: `./build-lowmem.sh test --lib core::search::tests::a_retired_note_sinks_below_the_cliff`
Expected: FAIL, `no field 'retired' on type 'SearchResult'`.

- [ ] **Step 3: Add the store lookup**

In `src/store/corpora.rs`, beside `is_retired`:

```rust
    /// Which of these notes are retired, in one read. The result list asks
    /// once for the whole page, the way `due_for` does.
    pub async fn retired_among(
        &self,
        corpus_ids: &[String],
    ) -> Result<std::collections::HashSet<String>> {
        if corpus_ids.is_empty() {
            return Ok(Default::default());
        }
        let marks = std::iter::repeat_n("?", corpus_ids.len()).collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT id FROM corpora WHERE retired_at IS NOT NULL AND id IN ({marks})"
        );
        let mut q = sqlx::query_scalar::<_, String>(&sql);
        for id in corpus_ids {
            q = q.bind(id);
        }
        Ok(q.fetch_all(&self.pool).await?.into_iter().collect())
    }
```

- [ ] **Step 4: Add the field to `SearchResult`**

In `src/core/search.rs`, in the `SearchResult` struct beside `past_cliff`:

```rust
    /// The note this passage came from is a reminder that is done. It keeps
    /// its place in the list and is moved to the tail rather than dropped:
    /// search for its own words and it is still there. What it stops doing is
    /// competing with the things that were written to be kept.
    #[serde(default)]
    pub retired: bool,
```

Add `retired: false` to every struct literal that constructs a `SearchResult` — the compiler will name each one; at the time of writing they are near lines 263, 1067, 3113, 4215, 4329 and 4389.

- [ ] **Step 5: Fill it, demote, and mark — in that order**

In `src/core/search.rs`, in the block that fills `due_at`/`due_in` around line 1575, append after that `for` loop:

```rust
        // Retirement, read for the whole page in one query beside the due map.
        {
            let cids: Vec<String> = results.iter().map(|r| r.corpus_id.clone()).collect();
            let retired = self.store.retired_among(&cids).await.unwrap_or_default();
            for r in &mut results {
                r.retired = retired.contains(&r.corpus_id);
            }
        }
```

Then, immediately **before** the `mark_past_cliff(&mut results, reranked);` call around line 1714:

```rust
        // Retired notes go to the tail before the cliff is read, never after.
        // `mark_past_cliff` guarantees that what it marks is a *suffix* of the
        // list, and `ask` truncates at the first marked hit — so marking a row
        // in the middle would silently cut the answer short. A stable partition
        // keeps every other row in the order the ranking produced.
        results.sort_by_key(|r| r.retired);
        mark_past_cliff(&mut results, reranked);
        for r in results.iter_mut().filter(|r| r.retired) {
            r.past_cliff = true;
            if let Some(e) = r.explanation.as_mut() {
                e.past_cliff = true;
            }
        }
```

`sort_by_key` on a `bool` is stable in Rust's standard library, so `false` rows keep their relative order and `true` rows land at the end in theirs.

- [ ] **Step 6: Carry it to the row and badge it**

In `src/web/ui.rs`, in `RenderedResult` beside `past_cliff`:

```rust
    /// A reminder that is done. Badged, because a row that has quietly sunk
    /// with no reason given reads as a ranking bug.
    pub retired: bool,
```

In `render_hit`, beside `past_cliff: h.past_cliff,`:

```rust
        retired: h.retired,
```

In `src/web/templates/_results.html`, in the badge run that currently reads `{% if r.weak %}<span class="badge badge-warning">loose</span>{% endif %}`, add directly after that `{% endif %}`:

```html
        {# Why this row is below the line, said on the row. Without it a
           demoted hit is indistinguishable from a ranking that went wrong. #}
        {% if r.retired %}
        <span class="badge">done reminder</span>
        {% endif %}
```

- [ ] **Step 7: Run the search tests**

Run: `./build-lowmem.sh test --lib core::search::`
Expected: PASS. Watch specifically that the existing cliff tests still pass — they assert the marked set is a suffix.

- [ ] **Step 8: Commit**

```bash
git add src/core/search.rs src/store/corpora.rs src/web/ui.rs src/web/templates/_results.html
git commit -m "feat(time): a done reminder ranks at the tail, below the fold, and says so"
```

---

### Task 4: The idle column

**Files:**
- Modify: `src/web/templates/workspace.html` (the region bar and the two regions)
- Rename: `src/web/templates/_rail_idle.html` → `src/web/templates/_idle_foot.html`
- Modify: `src/web/templates/_box_hint.html` (becomes the examples line — Task 8 fills it; this task only strips it back)
- Modify: `src/web/ui.rs:540-604` (`RailIdleTemplate` → `IdleFootTemplate`, `rail_idle` → `idle_foot`) and every call site the compiler names
- Test: `src/web/ui.rs` tests

**Interfaces:**
- Consumes: `recent_captures` from Task 1, which already excludes retired notes.
- Produces: `#idle` — the wrapper element Task 5's app.js hides and shows; `IdleFootTemplate { artifacts, corpora, recent, held }`.

- [ ] **Step 1: Write the failing test**

In `src/web/ui.rs` tests, beside the existing test that asserts `html.contains("Last captured")` at line 4581 — that assertion is what this task deletes, so replace that test rather than adding beside it:

```rust
    #[tokio::test]
    async fn an_idle_workspace_is_one_column_and_says_what_it_holds() {
        let (app, cookie) = app_with_session().await;
        app.clone().oneshot(form("/ui/capture", &cookie, "text=mounting+an+image")).await.unwrap();
        let html = body_of(app.oneshot(get("/ui", &cookie)).await.unwrap()).await;

        assert!(html.contains(r#"id="idle""#), "the column exists as one element");
        assert!(!html.contains("Last captured"), "the rail's second copy of the recent list is gone");
        assert!(!html.contains("kind-chips"), "chips qualify a search and there is no search");
        assert!(!html.contains("or drop one anywhere on the page"), "the attach prose is on the button's title");
        assert!(html.contains("idle-foot"), "one closing line instead");
    }

    #[tokio::test]
    async fn an_empty_base_still_says_what_the_program_is_for() {
        let (app, cookie) = app_with_session().await;
        let html = body_of(app.oneshot(get("/ui", &cookie)).await.unwrap()).await;
        assert!(html.contains("Nothing here yet"), "no counts to print, so it introduces itself");
    }
```

Use the `get(...)`, `form(...)`, `body_of(...)` and `app_with_session()` helpers already in that test module.

- [ ] **Step 2: Run them and watch them fail**

Run: `./build-lowmem.sh test --lib web::ui::tests::an_idle_workspace`
Expected: FAIL on the `id="idle"` assertion.

- [ ] **Step 3: Rename the fragment and its template struct**

`git mv src/web/templates/_rail_idle.html src/web/templates/_idle_foot.html`.

In `src/web/ui.rs`, rename `RailIdleTemplate` to `IdleFootTemplate`, `rail_idle` to `idle_foot`, and `#[template(path = "_rail_idle.html")]` to `_idle_foot.html`. Add a `pub(crate) held: bool` field, set from the same `held` the workspace already computes (`corpora > 0`). Fix every call site the compiler names.

- [ ] **Step 4: Rewrite `_idle_foot.html`**

Replace the whole file with:

```html
{# The last line of the idle column, and the only place the base says what it
   holds. It used to be the rail's own idle state — a block of counts above a
   list of the last five captures, standing beside a middle column that was
   rendering those same five rows in a different shape. One list, one line.

   Not `.rail-item` cards and not a list: these are not answers to anything.
   The title opens its source, `today` opens the day.

   `hx-swap-oob` when it arrives on its own, because emptying the box brings
   the idle column back and this comes with it. #}
<p id="idle-foot" class="idle-foot muted"{% if oob %} hx-swap-oob="true"{% endif %}>
{% if !held %}
  {# No counts to print and no last capture to name. The one thing worth
     saying to somebody who has put nothing in yet is what putting something
     in does — and for an operator who arrived through an identity provider
     and never saw the login card, this is the only place the program
     introduces itself at all. #}
  Nothing here yet. Paste anything worth keeping into the box above, or attach
  a file — it becomes searchable, in your own words.
{% else %}
  <span class="mono">{{ artifacts }}</span> artifact{% if artifacts != 1 %}s{% endif %}
  from <span class="mono">{{ corpora }}</span> source{% if corpora != 1 %}s{% endif %}
  {%- if let Some(r) = recent.first() %} · last kept
  <a href="/ui/corpora/{{ r.id }}">{{ r.label }}</a>
  <a href="/ui/day/{{ r.day }}" data-day-link>{{ r.when }}</a>
  {%- endif %}
{% endif %}
</p>
```

`idle_foot` in `ui.rs` still asks `recent_captures(5)`; only the first row is rendered, and the other four are the cheap headroom that keeps this one line correct when the newest capture is retired between two renders. Add `pub(crate) oob: bool` to the struct if it is not already there — `_box_hint.html` shows the pattern.

- [ ] **Step 5: Restructure `workspace.html`**

Three changes, no others:

Wrap the Kind chip row — the `{% if !facets.categories.is_empty() %}` block through its `{% endif %}` — so it renders only with a query, and comment why:

```html
    {# Chips qualify a search. On an idle page there is no search, and a chip
       row marooned at the far end of the verb row was the page's loudest piece
       of furniture. app.js reveals this on the first keystroke, on the same
       event that reveals the rest of the column. #}
    {% if !facets.categories.is_empty() %}
    <div id="kind-row" class="kind-row"{% if q.is_empty() %} hidden{% endif %}>
    …existing label and chips, unchanged…
    </div>
    {% endif %}
```

Delete the `<span class="muted hint attach-types">…</span>` line entirely. The accepted types are already on the `<label id="drop">` `title` and on the input's `aria-label`; the sentence was a third copy.

Replace the two blocks that render the offer and the due band with one wrapper holding four things in order — guidance, offer, due, closing line:

```html
{# The idle column: everything the page shows when nobody has typed. One
   element, because app.js hides and reveals it as a unit and because the four
   things in it are one statement — here is how to write to me, here is
   something you might want, here is what you asked for, here is what I hold.

   Hidden rather than removed on the first keystroke. Removing it is what made
   a reminder armed by a capture invisible until a reload: the capture empties
   the box, the idle state comes back, and there was no longer a `#due` in the
   document for anything to swap into. #}
<div id="idle"{% if !q.is_empty() %} hidden{% endif %}>
  {% include "_box_hint.html" %}
  {% if recommend %}
  <div id="context-offer" class="offer" hx-post="/ui/context" hx-trigger="load"
       hx-vals='js:{bundle: engramContext()}' hx-swap="outerHTML"></div>
  {% endif %}
  <div id="due" class="due" hx-post="/ui/due" hx-trigger="load"
       hx-vals='js:{tz: Intl.DateTimeFormat().resolvedOptions().timeZone || ""}' hx-swap="outerHTML"></div>
  {{ idle_foot|safe }}
</div>
```

Note three deliberate changes inside that block. The `q.is_empty()` guards move from the two children onto the wrapper, so the offer's "do not render beside real results" rule is preserved by the wrapper's own `hidden`. `intersect once` on the due band becomes `load`: the band is no longer below the fold of a three-column page, and `intersect` never fires for an element inside a `hidden` parent that is later revealed. And `_keyhint.html` stays where it is, outside the wrapper.

- [ ] **Step 6: Collapse the rail and the pane while idle**

In `workspace.html`, add `{% if q.is_empty() %} hidden{% endif %}` to the `#rail` and `#pane` region divs, and pass `idle_foot` where `idle` was passed before. In `src/web/ui.rs`, the workspace handler renders `idle_foot(&tenant)` into a `String` for `{{ idle_foot|safe }}` exactly as it renders `idle` today. `_pane_idle.html` is still included inside `#pane` and is now reached only when the pane is revealed — its `held = false` sentence is what the second new test asserts, so keep the include.

- [ ] **Step 7: Add the CSS**

Append to `assets/css/40-workspace.css`, beside the `.due` rules at line 791:

```css
/* The idle column. One measure wide, so four short things under a full-width
   box read as a column rather than as four bands across the page. */
#idle { max-width: 46rem; }
.idle-foot { margin: 0.75rem 0 0; font-size: var(--text-xs); }
.idle-foot a { color: inherit; }
.kind-row { display: contents; }
```

- [ ] **Step 8: Run the tests**

Run: `./build-lowmem.sh test --lib web::ui::`
Expected: PASS. Several existing tests assert on the old idle rail — read each failure and update the assertion to the new markup rather than restoring the old markup.

- [ ] **Step 9: Commit**

```bash
git add -A src/web/templates src/web/ui.rs assets/css/40-workspace.css
git commit -m "feat(ui): the idle workspace is one column, and says what it holds in one line"
```

---

### Task 5: The column is hidden, never removed

**Files:**
- Modify: `assets/app.js:864-871` (`dropOffer`), `:2068-2073` (the `afterSwap` guard), `:1365-1371` (`refreshRail`)
- Test: `tests/browser/` — follow the existing browser test setup in that directory; if it has no runner wired into `cargo test`, assert the behaviour in `src/web/ui.rs` instead by checking the rendered attributes, and verify the JavaScript by hand with `./run.sh` or the project's own launch path.

**Interfaces:**
- Consumes: `#idle` from Task 4.
- Produces: `showIdle()` / `hideIdle()` in app.js's module scope.

- [ ] **Step 1: Replace `dropOffer` with a hide**

```js
  // The idle column is hidden, not removed. Removing it was how a reminder
  // armed by a capture stayed invisible until a reload: Capture empties the
  // box, the idle state is correct again, and there was no `#due` left in the
  // document for the band to swap into.
  //
  // The offer is the one thing still genuinely removed. It is a measured
  // impression — see `confirmOffer` — and an offer computed for one situation
  // that reappears in another is a second impression nobody had.
  function hideIdle() {
    var idle = document.getElementById('idle');
    if (idle) idle.hidden = true;
    var area = document.getElementById('context-offer');
    if (area) area.remove();
  }

  // The box is empty again, so the column is right again. The due band is
  // re-fetched rather than restored from what it held: it may have been
  // standing there through a capture that armed something new, and what it
  // held is a minute old.
  function showIdle() {
    var idle = document.getElementById('idle');
    if (!idle) return;
    idle.hidden = false;
    var due = document.getElementById('due');
    if (due) htmx.trigger(due, 'refresh');
  }
```

Keep the name `dropOffer` as an alias if other call sites use it, or rename every call site — the compiler will not help here, so grep: `grep -n dropOffer assets/app.js`.

- [ ] **Step 2: Reveal the chip row with the column's disappearance**

Inside `hideIdle`, after hiding `#idle`:

```js
    var kinds = document.getElementById('kind-row');
    if (kinds) kinds.hidden = false;
```

and inside `showIdle`, the mirror:

```js
    var kinds = document.getElementById('kind-row');
    if (kinds) kinds.hidden = true;
```

- [ ] **Step 3: Show the column when the box empties**

`refreshRail()` at line 1365 already fires the form's `submit` after a capture, and the results endpoint answers an empty box with the idle rail. Add the column to that:

```js
    function refreshRail() {
      htmx.trigger(form, 'submit');
      // The capture emptied the box, so the idle column is correct again — and
      // this is the moment a reminder captured a second ago wants to appear.
      // The band's own polling (see `_due.html`) covers the gap between this
      // and the background job that reads the intent.
      showIdle();
    }
```

Find the keystroke handler that currently calls `dropOffer()` and make it call `hideIdle()`; find wherever the box being emptied by hand is already noticed (the same `input` handler, `box.value.trim() === ''`) and call `showIdle()` there.

- [ ] **Step 4: Fix the `afterSwap` guard**

At line 2068:

```js
      if (e.target.id === 'context-offer') {
        if (offerDismissed) hideIdle();
        else confirmOffer(e.target);
      }
```

Delete the `if (e.target.id === 'due' && offerDismissed) dropOffer();` line entirely. The band arriving inside a hidden column is now harmless, and removing the column because the band arrived is the bug this task exists to fix.

- [ ] **Step 5: Add the `refresh` trigger to the band**

In `src/web/templates/_due.html`, the outer `<div id="due">` gains:

```html
     hx-post="/ui/due" hx-swap="outerHTML" hx-trigger="refresh"
     hx-vals='{"tz": "{{ tz }}"}'
```

Task 6 extends that `hx-trigger` list; this task only makes the element re-fetchable at all.

- [ ] **Step 6: Verify by hand**

Run the app (`./run.sh`, or whatever `docs/` names as the launch path), capture `remind me in 2 minutes to check this`, and confirm the box empties, the column comes back, and the reminder appears in the band within a few seconds without a reload.

- [ ] **Step 7: Commit**

```bash
git add assets/app.js src/web/templates/_due.html
git commit -m "fix(ui): the idle column is hidden and re-fetched, not removed"
```

---

### Task 6: The band decides its own cadence

**Files:**
- Modify: `src/web/due.rs` (`DueTemplate`, `render`)
- Modify: `src/web/templates/_due.html` (the `hx-trigger`)
- Test: `src/web/due.rs` tests

**Interfaces:**
- Consumes: the `refresh` trigger from Task 5.
- Produces: `DueTemplate.refresh_in: Option<i64>` — seconds until the band should re-fetch itself, `None` when nothing is pending.

- [ ] **Step 1: Write the failing tests**

```rust
    #[tokio::test]
    async fn an_idle_band_with_nothing_pending_polls_not_at_all() {
        let core = test_core().await;
        let (app, cookie) = app_with_cookie(core).await;
        let html = body_of(app.oneshot(form("/ui/due", &cookie, "tz=Europe/Berlin")).await.unwrap()).await;
        assert!(!html.contains("every "), "an idle page in a background tab makes no requests");
    }

    #[tokio::test]
    async fn a_reminder_landing_inside_five_minutes_is_polled_for_at_its_second() {
        let core = test_core().await;
        artifact_with_due(&core, Some(crate::store::now() + 90)).await;
        let (app, cookie) = app_with_cookie(core).await;
        let html = body_of(app.oneshot(form("/ui/due", &cookie, "tz=Europe/Berlin")).await.unwrap()).await;
        assert!(html.contains("every 90s"), "polled when it lands, not on a fixed tick: {html}");
    }

    #[tokio::test]
    async fn a_reminder_further_out_is_polled_for_at_the_cap() {
        let core = test_core().await;
        artifact_with_due(&core, Some(crate::store::now() + 4 * 3_600)).await;
        let (app, cookie) = app_with_cookie(core).await;
        let html = body_of(app.oneshot(form("/ui/due", &cookie, "tz=Europe/Berlin")).await.unwrap()).await;
        assert!(html.contains("every 300s"), "five-minute cap: {html}");
    }
```

- [ ] **Step 2: Run them and watch them fail**

Run: `./build-lowmem.sh test --lib web::due::tests`
Expected: FAIL on `every 90s` — the fragment carries no trigger at all today.

- [ ] **Step 3: Compute the delay**

In `src/web/due.rs`, add to `DueTemplate`:

```rust
    /// Seconds until this fragment should ask again, or `None` for "never".
    /// The fragment carries its own trigger, so the swap that reports the last
    /// pending thing landing is also the swap that stops the polling — the
    /// contract `_queue.html` already keeps, and the reason an idle page open
    /// in a background tab costs nothing.
    pub refresh_in: Option<i64>,
```

and this free function beside `when_words`:

```rust
/// The cap. Further out than this and there is nothing to watch for yet: the
/// band re-reads on the five, and the reminder is still minutes from due.
const POLL_CAP: i64 = 300;
/// While a capture is still being read, its reminder does not exist yet. Two
/// seconds is the gap between "you pressed Capture" and "the band holds it".
const POLL_QUEUE: i64 = 2;

/// `queue_active` — anything still being segmented, embedded or classified.
/// `next_at` — the earliest moment that has not come due yet, if any.
pub(crate) fn refresh_in(queue_active: bool, next_at: Option<i64>, now: i64) -> Option<i64> {
    if queue_active {
        return Some(POLL_QUEUE);
    }
    let ahead = next_at?.saturating_sub(now);
    // Already due and still open: the row is on screen and nothing is coming,
    // so there is nothing to poll for.
    if ahead <= 0 {
        return None;
    }
    Some(ahead.min(POLL_CAP))
}
```

Add a unit test for it in the same module:

```rust
    #[test]
    fn the_cadence_is_the_soonest_thing_worth_asking_about() {
        assert_eq!(refresh_in(true, None, 1_000), Some(2), "a capture is still being read");
        assert_eq!(refresh_in(false, None, 1_000), None, "nothing pending, nothing asked");
        assert_eq!(refresh_in(false, Some(1_090), 1_000), Some(90), "polled at the second it lands");
        assert_eq!(refresh_in(false, Some(20_000), 1_000), Some(300), "and no later than the cap");
        assert_eq!(refresh_in(false, Some(900), 1_000), None, "already due and on screen");
    }
```

- [ ] **Step 4: Wire it into `render`**

In `render`, after `events` is built:

```rust
    // What the band is waiting for: a capture still being read, or the next
    // moment that has not come due yet — whichever is sooner.
    let queue_active = tenant.core.store.jobs_in_flight().await.unwrap_or(0) > 0;
    let next_at = tenant.core.store.next_due_after(now).await.unwrap_or(None);
    let refresh_in = refresh_in(queue_active, next_at, now);
```

`jobs_in_flight` and `next_due_after` may not exist under those names. Before writing this, grep for what `_queue.html`'s handler already calls to decide its `active` flag (`grep -n "active" src/web/ui.rs` around the `QueueTemplate`) and reuse it verbatim; add `next_due_after(now) -> Result<Option<i64>>` to `src/store/moments.rs` as `SELECT MIN(at) FROM moments WHERE kind = 'due' AND done_at IS NULL AND at > ?` only if nothing equivalent is there.

Pass `refresh_in` into every `DueTemplate { … }` literal.

- [ ] **Step 5: Put the trigger on the fragment**

In `_due.html`, the outer div's trigger becomes:

```html
     hx-trigger="refresh{% if let Some(s) = refresh_in %}, every {{ s }}s{% endif %}"
```

with the comment:

```html
{# Polls itself, and only while something is coming. The trigger is on the
   fragment rather than on the page, so the swap that reports the last thing
   landing is the swap that stops the polling. `refresh` is app.js's own
   event, fired when the box empties. Same contract as `_queue.html`. #}
```

- [ ] **Step 6: Run the tests**

Run: `./build-lowmem.sh test --lib web::due::`
Expected: PASS, all of them.

- [ ] **Step 7: Commit**

```bash
git add src/web/due.rs src/web/templates/_due.html src/store/moments.rs
git commit -m "feat(time): the due band polls for what is coming, and only for that"
```

---

### Task 7: The band, restyled

**Files:**
- Modify: `src/web/templates/_due.html` (the row)
- Modify: `assets/css/40-workspace.css:791-802` (the `.due` rules)
- Test: `src/web/due.rs` tests

**Interfaces:**
- Consumes: `DueView` from `src/web/due.rs`, unchanged.
- Produces: nothing new. Markup and CSS only.

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn a_row_shows_one_button_and_keeps_the_rest_behind_a_disclosure() {
        let core = test_core().await;
        artifact_with_due(&core, Some(crate::store::now() + 3_600)).await;
        let (app, cookie) = app_with_cookie(core).await;
        let html = body_of(app.oneshot(form("/ui/due", &cookie, "tz=Europe/Berlin")).await.unwrap()).await;
        assert!(html.contains("due-later"), "snooze and move are behind a disclosure");
        assert!(html.contains("<summary>later</summary>"));
        assert!(html.contains(">done<"), "done is the one visible verb");
    }

    #[tokio::test]
    async fn an_undated_row_opens_its_date_field_rather_than_hiding_it() {
        let core = test_core().await;
        artifact_with_due(&core, None).await;
        let (app, cookie) = app_with_cookie(core).await;
        let html = body_of(app.oneshot(form("/ui/due", &cookie, "tz=Europe/Berlin")).await.unwrap()).await;
        assert!(!html.contains("due-later"), "asking for the date is the whole point of the row");
        assert!(html.contains("set date"));
    }
```

- [ ] **Step 2: Run them and watch them fail**

Run: `./build-lowmem.sh test --lib web::due::tests::a_row_shows_one_button`
Expected: FAIL, no `due-later` in the markup.

- [ ] **Step 3: Rewrite the row**

Replace the `{% for r in rows %}` body in `_due.html` with:

```html
  {# `due-new` is on a row whose moment id was not in the previous render.
     Without it the band grows in silence while you are looking elsewhere, and
     "populates in real time" is a claim nothing on screen supports. The class
     drives one short animation and means nothing after it. #}
  <div class="due-row{% if r.overdue %} due-overdue{% endif %}{% if r.undated %} due-undated{% endif %}{% if r.fresh %} due-new{% endif %}">
    <a class="due-title" href="/ui/artifacts/{{ r.artifact_id }}">{{ r.title }}</a>
    <span class="due-when">{{ r.when }}{% if r.recurring %} <span title="repeats">↻</span>{% endif %}</span>
    {% if r.source != "set" %}<span class="muted due-source" title="read from the note, not set by you">read</span>{% endif %}
    <span class="due-acts">
      <button class="btn btn-ghost btn-sm" hx-post="/ui/moments/{{ r.id }}/done" hx-target="#due" hx-swap="outerHTML">done</button>
      {% if r.undated %}
      {# No disclosure: a row with no date is asking for one, and the field is
         the row's whole purpose. Hiding it behind "later" would hide the one
         thing this row exists to collect. #}
      <form class="due-date" hx-post="/ui/moments/{{ r.id }}/date" hx-target="#due" hx-swap="outerHTML">
        <input type="datetime-local" name="when" class="input" required>
        <button class="btn btn-ghost btn-sm" type="submit">set date</button>
      </form>
      {% else %}
      {# Seven controls on one line was the same clutter as the front page, one
         scale down. `done` is what a person presses; everything else is a
         second thought and opens on demand. #}
      <details class="due-later">
        <summary>later</summary>
        <span class="due-snooze">snooze
          <button class="btn btn-ghost btn-sm" hx-post="/ui/moments/{{ r.id }}/snooze" hx-vals='{"until":"hour"}' hx-target="#due" hx-swap="outerHTML">1h</button>
          <button class="btn btn-ghost btn-sm" hx-post="/ui/moments/{{ r.id }}/snooze" hx-vals='{"until":"tomorrow"}' hx-target="#due" hx-swap="outerHTML">tomorrow</button>
          <button class="btn btn-ghost btn-sm" hx-post="/ui/moments/{{ r.id }}/snooze" hx-vals='{"until":"monday"}' hx-target="#due" hx-swap="outerHTML">Monday</button>
        </span>
        <form class="due-date" hx-post="/ui/moments/{{ r.id }}/date" hx-target="#due" hx-swap="outerHTML">
          <input type="datetime-local" name="when" class="input" required>
          <button class="btn btn-ghost btn-sm" type="submit">move</button>
        </form>
      </details>
      {% endif %}
    </span>
  </div>
```

- [ ] **Step 4: Fill `fresh`**

Add `pub fresh: bool` to `DueView` in `src/web/due.rs`. A row is fresh when its moment was created since the previous render of this band. The band is stateless, so carry the boundary on the fragment: add `pub since: i64` to `DueTemplate`, render it as `data-since="{{ since }}"` on the outer div, and have `fragment`/`done`/`snooze` read an optional `since` field off `TzForm` (`#[serde(default)] pub since: i64`), which `_due.html` sends back via `hx-vals`. `fresh` is then `r.moment.created_at > since`, and `since` on the new render is `now`. On the very first render `since` is `0` and every row is fresh — which is right: they all just appeared.

- [ ] **Step 5: Replace the CSS**

Replace lines 791-802 of `assets/css/40-workspace.css`:

```css
/* A card, drawn around something or not at all. The left accent is what tells
   it apart from the context offer directly above: two bordered blocks with the
   same edge read as one list of two unrelated things. */
.due { margin: 0 0 0.75rem; }
.due-filled {
  border-left: 3px solid var(--color-due);
  border-radius: 0 4px 4px 0;
  background: color-mix(in oklab, var(--color-due) 6%, transparent);
  padding: 0.5rem 0.75rem;
}
.due-row { display: flex; flex-wrap: wrap; gap: 0.5rem 0.75rem; align-items: baseline; padding: 0.25rem 0; }
.due-title { font-weight: 500; }
/* Full strength overdue, faint upcoming. The colour is the whole signal, so a
   row that is merely coming must not shout in the same voice as one that is
   already late. */
.due-when { color: color-mix(in oklab, var(--color-due) 55%, var(--color-text)); }
.due-overdue .due-when { color: var(--color-due); font-weight: 600; }
.due-undated .due-when { font-style: italic; }
.due-done .due-title { text-decoration: line-through; }
.due-acts { display: inline-flex; gap: 0.5rem; align-items: baseline; margin-left: auto; flex-wrap: wrap; }
.due-later > summary { cursor: pointer; font-size: var(--text-xs); color: var(--color-muted); list-style: none; }
.due-later[open] { display: flex; flex-wrap: wrap; gap: 0.25rem 0.5rem; align-items: baseline; }
.due-snooze { display: inline-flex; gap: 0.25rem; align-items: baseline; font-size: var(--text-xs); }
.due-date { display: inline-flex; gap: 0.25rem; }
.due-coming { margin: 0.25rem 0 0; }

/* A row that was not here a moment ago. One pass, then it is an ordinary row —
   a persistent mark would become furniture inside a day. */
@keyframes due-arrive {
  from { background: color-mix(in oklab, var(--color-due) 28%, transparent); }
  to   { background: transparent; }
}
.due-new { animation: due-arrive 1.4s ease-out 1; }
@media (prefers-reduced-motion: reduce) {
  .due-new { animation: none; }
}
```

Check `assets/css/00-tokens.css` for the exact names of `--color-due`, `--color-text` and `--color-muted` before writing these; use whatever that file actually defines.

- [ ] **Step 6: Run the tests**

Run: `./build-lowmem.sh test --lib web::due::`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/web/templates/_due.html src/web/due.rs assets/css/40-workspace.css
git commit -m "feat(time): one verb on a due row, and a row that arrives says so"
```

---

### Task 8: Examples in the page, an echo while typing

**Files:**
- Modify: `src/web/templates/_box_hint.html` (becomes the guidance line)
- Modify: `src/web/ui.rs` (`BoxHintTemplate`, `search_results`)
- Create: `src/web/templates/_intent_echo.html`
- Modify: `src/core/moments.rs` (add `examples_for`)
- Modify: `assets/app.js` (chip click fills the box)
- Test: `src/core/moments.rs` tests, `src/web/ui.rs` tests

**Interfaces:**
- Consumes: `cue()`, `relative_date()`, `absolute_dates()` and `PROTOTYPES` from `src/core/moments.rs`; `when_words` from `src/web/due.rs`.
- Produces:
  - `moments::examples_for(accept_language: &str) -> (&'static str, &'static str)` — a remind example and a journal example, in the best available language.
  - `IntentEchoTemplate { kind: &'static str, when: String }`, rendered into `#intent-echo` out of band.

- [ ] **Step 1: Write the failing tests**

In `src/core/moments.rs` tests:

```rust
    #[test]
    fn examples_come_from_the_prototypes_in_the_readers_language() {
        let (remind, journal) = examples_for("de-DE,de;q=0.9,en;q=0.8");
        assert!(remind.starts_with("erinnere mich"), "{remind}");
        assert!(journal.starts_with("heute"), "{journal}");

        let (remind, journal) = examples_for("");
        assert!(remind.starts_with("remind me"), "English is the fallback: {remind}");
        assert!(journal.starts_with("today i"), "{journal}");

        let (remind, _) = examples_for("xx-YY");
        assert!(remind.starts_with("remind me"), "an unknown language falls back too");
    }
```

In `src/web/ui.rs` tests:

```rust
    #[tokio::test]
    async fn typing_a_reminder_echoes_what_it_will_become() {
        let (app, cookie) = app_with_session().await;
        let html = body_of(
            app.oneshot(get("/ui/search/results?q=remind+me+tomorrow+to+send+the+invoice", &cookie)).await.unwrap(),
        )
        .await;
        assert!(html.contains(r#"id="intent-echo""#));
        assert!(html.contains("reminder ·"), "it says what it is and when: {html}");
    }

    #[tokio::test]
    async fn ordinary_text_echoes_nothing_at_all() {
        let (app, cookie) = app_with_session().await;
        let html = body_of(app.oneshot(get("/ui/search/results?q=vector+index+rebuild", &cookie)).await.unwrap()).await;
        assert!(html.contains(r#"id="intent-echo""#), "the slot is always swapped, so a stale echo cannot survive");
        assert!(!html.contains("reminder ·"), "the echo claims only what the cue table proves");
    }
```

- [ ] **Step 2: Run them and watch them fail**

Run: `./build-lowmem.sh test --lib core::moments::tests::examples_come_from web::ui::tests::typing_a_reminder`
Expected: FAIL, `cannot find function 'examples_for'`.

- [ ] **Step 3: Write `examples_for`**

In `src/core/moments.rs`, after `PROTOTYPES`:

```rust
/// The language a prototype is written in, in the order `PROTOTYPES` lists
/// them. Kept beside that table rather than derived from it: a language tag is
/// not recoverable from a sentence, and the two lists are read together or not
/// at all.
const PROTOTYPE_LANGS: &[&str] = &[
    "en", "en", "de", "de", "fr", "es", "pt", "it", "nl", "pl", "tr", "ru",
    "en", "en", "de", "de", "fr", "es", "pt", "it", "nl", "pl", "tr", "ru",
];

/// One reminder and one journal example, in the reader's language where the
/// prototype table has it and in English where it does not.
///
/// Drawn from `PROTOTYPES` rather than written out again, because these are
/// examples of what the classifier reads and a second copy would drift from it
/// the first time a prototype is retuned.
///
/// `accept_language` is the raw header. Only the primary subtag of the first
/// entry is read: `de-DE,de;q=0.9,en;q=0.8` is a reader who wants German, and
/// weighing the rest to discover that would be arithmetic for nothing.
pub fn examples_for(accept_language: &str) -> (&'static str, &'static str) {
    debug_assert_eq!(PROTOTYPE_LANGS.len(), PROTOTYPES.len(), "one language per prototype");
    let want = accept_language
        .split(',')
        .next()
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .split('-')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let pick = |intent: Intent| -> &'static str {
        let of = |lang: &str| {
            PROTOTYPES
                .iter()
                .zip(PROTOTYPE_LANGS)
                .find(|((i, _), l)| *i == intent && **l == *lang)
                .map(|((_, p), _)| *p)
        };
        of(&want).or_else(|| of("en")).unwrap_or("")
    };
    (pick(Intent::Remind), pick(Intent::Journal))
}
```

- [ ] **Step 4: Rewrite `_box_hint.html` as the guidance line**

```html
{# The one line under the box, and the only guidance on an idle page. It
   replaces two: a sentence claiming that typing searches, and a second copy of
   the accepted file types that the Attach button's own `title` already
   carries. An example teaches the first better than the claim did, and the
   second was never guidance at all.

   The examples are the classifier's own prototypes, in the reader's language —
   see `moments::examples_for`. Nothing here is a second copy of anything: the
   sentences shown are the sentences engram measures against.

   Chips fill the box and do not submit. Pressing one puts the phrasing in
   front of you and fires the echo below it, which is the whole loop in one
   press. #}
<p id="box-hint" class="muted hint"{% if oob %} hx-swap-oob="true"{% endif %}>
  {%- if held -%}
  Try
  <button type="button" class="chip-example" data-example="{{ example_remind }}">&ldquo;{{ example_remind }}&rdquo;</button>
  or
  <button type="button" class="chip-example" data-example="{{ example_journal }}">&ldquo;{{ example_journal }}&rdquo;</button>
  — or paste a whole paragraph; a sentence finds more than keywords do.
  {%- else -%}
  engram keeps what you paste in your own words and finds it again by meaning,
  not by keywords. This base is yours: nobody else can search it.
  {%- endif -%}
</p>
{# The slot the echo swaps into. Always present and always swapped, so an echo
   for text that is no longer in the box cannot survive a keystroke. #}
<span id="intent-echo" class="intent-echo"></span>
```

Add `example_remind: &'static str` and `example_journal: &'static str` to the box-hint template struct in `src/web/ui.rs`, filled from `examples_for` with the request's `Accept-Language` header. Take the header with axum's `TypedHeader` or by reading `HeaderMap` — match whatever the surrounding handlers already do.

- [ ] **Step 5: Write the echo fragment**

`src/web/templates/_intent_echo.html`:

```html
{# What the box will become if it is captured, said before it is. Cue-only: it
   claims what the cue table proves and nothing else, so a note with no cue
   echoes silence rather than "not a reminder". The embedding classifier at
   capture can still fire where the table did not, and an echo that had
   promised otherwise would be lying — that surprise lands in the safe
   direction, and the arriving row in the band below is what makes it visible.

   Always swapped, even when empty: this is how a stale echo dies. #}
<span id="intent-echo" class="intent-echo" hx-swap-oob="true">
{%- if !kind.is_empty() %}<span class="intent-echo-kind">{{ kind }}</span> · {{ when }}{% endif -%}
</span>
```

- [ ] **Step 6: Render it from `search_results`**

In `src/web/ui.rs`, inside `search_results`, before the response is built:

```rust
    // The echo rides the search. Every keystroke already makes this request on
    // a 120ms debounce, and `cue` plus the date rules are pure string work with
    // no model and no store — so this is a fragment appended to a response, not
    // a second request per keystroke.
    let echo = {
        use crate::core::moments::{cue, relative_date, absolute_dates, Intent};
        let tz = crate::core::moments::zone(p.tz.as_deref());
        let now = tenant.core.clock.now();
        match cue(&p.q) {
            Some(Intent::Remind) => {
                let at = relative_date(&p.q, now, tz)
                    .or_else(|| absolute_dates(&p.q, now, tz, false).into_iter().next());
                IntentEchoTemplate {
                    kind: "reminder",
                    when: match at {
                        Some(f) => crate::web::due::when_words(f.at, now, tz),
                        None => "no date read — it will ask you for one".into(),
                    },
                }
            }
            Some(Intent::Journal) => IntentEchoTemplate { kind: "journal entry", when: "today".into() },
            None => IntentEchoTemplate { kind: "", when: String::new() },
        }
    };
```

`Found`'s date field may not be named `at`, and `absolute_dates`/`relative_date` take a `month_first` flag whose source is config — read their signatures at `src/core/moments.rs:268` and `:387` and pass what the moments *job* passes at `src/jobs/moments.rs`, so the echo and the job read a date the same way. `p.tz` may need adding to `UiSearchParams`; if so, add it and send it from the form's `hx-params` allowlist in `workspace.html`, which currently reads `q,category,rerank,explain,fold`.

Render `echo` into the response body alongside the results fragment, following whatever pattern `_results.html` already uses to ship its out-of-band `#fold-of` span — the echo is the same shape of thing.

- [ ] **Step 7: Make a chip fill the box**

In `assets/app.js`, in the delegated click handler:

```js
    // An example chip fills the box and stops there. It does not submit: the
    // point is to put the phrasing in front of you, let the echo answer it,
    // and leave the press to you.
    var chip = e.target.closest && e.target.closest('.chip-example');
    if (chip) {
      e.preventDefault();
      box.value = chip.getAttribute('data-example');
      box.focus();
      box.dispatchEvent(new Event('input', { bubbles: true }));
      return;
    }
```

- [ ] **Step 8: Style it**

Append to `assets/css/40-workspace.css`:

```css
/* An example reads as a quotation you can press, not as a button. Underlined
   on hover only: three buttons in a sentence would make the sentence a
   toolbar. */
.chip-example {
  background: none; border: 0; padding: 0; font: inherit; color: var(--color-text);
  cursor: pointer;
}
.chip-example:hover { text-decoration: underline; }
.intent-echo { display: block; font-size: var(--text-xs); color: var(--color-due); min-height: 1.2em; }
.intent-echo-kind { font-weight: 600; }
```

`min-height` so the line's arrival does not push the offer and the band down by a row on the first cue.

- [ ] **Step 9: Run the tests**

Run: `./build-lowmem.sh test --lib core::moments:: web::ui::`
Expected: PASS.

- [ ] **Step 10: Commit**

```bash
git add src/core/moments.rs src/web/ui.rs src/web/templates assets/app.js assets/css/40-workspace.css
git commit -m "feat(time): the box shows how to write a reminder, and says what it read"
```

---

### Task 9: The whole suite, and the README

**Files:**
- Modify: `README.md` (the **Time** bullet)
- Test: everything

- [ ] **Step 1: Run the full suite**

Run: `./build-lowmem.sh test`
Expected: PASS. Anything failing here is a test elsewhere that asserted on the old idle rail, the old due row, or the old hint prose — read each one and update the assertion to the new behaviour, never the behaviour to the old assertion.

- [ ] **Step 2: Run clippy**

Run: `./build-lowmem.sh clippy --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 3: Update the README's Time bullet**

In `README.md`, the **Time** bullet gains one sentence, after "…push to Gotify or UnifiedPush.":

```markdown
  The band fills itself while you watch, and a reminder that is done retires
  the note it came from: still searchable, no longer one of the last things
  you kept.
```

- [ ] **Step 4: Commit**

```bash
git add README.md
git commit -m "docs: the band fills itself, and a done reminder retires its note"
```

---

## Self-Review

**Spec coverage.**

| Spec section | Task |
| --- | --- |
| 1 — Kind chips hide while idle | 4 (markup), 5 (reveal) |
| 1 — one guidance line, two prose lines gone | 4 (delete `attach-types`), 8 (rewrite `_box_hint`) |
| 1 — offer then due, in that order | 4 |
| 1 — one closing line replacing counts and Last captured | 4 |
| 1 — rail and pane do not render while idle | 4 |
| 1 — `_pane_idle` survives for the empty base | 4, step 6 |
| 2 — retire on the last read reminder | 1, 2 |
| 2 — recurring, snooze and hand-set do not retire | 2 |
| 2 — leaves the recent list, keeps the day page | 1 |
| 2 — below the divider, badged, still findable | 3 |
| 2 — `retired_at`, undo clears it | 1, 2 |
| 3 — re-renders when the box empties | 5 |
| 3 — self-computed poll cadence, no trigger when idle | 6 |
| 3 — card with a left accent, one visible verb, `later` disclosure | 7 |
| 3 — arrival highlight | 7 |
| 4 — example chips from `PROTOTYPES`, by `Accept-Language` | 8 |
| 4 — echo out of band on the search response | 8 |
| 4 — silence when no cue matched | 8 |
| Error handling — echo is decoration | 8, step 5 (always-swapped empty slot) |
| Error handling — a failed poll leaves the band standing | 6 (htmx default: a failed swap changes nothing) |
| Error handling — unparseable `Accept-Language` falls back | 8, step 3 |

**Placeholder scan.** Four steps tell the implementer to read an existing signature before writing against it — Task 2 step 3 (`Source`'s wire spelling), Task 6 step 4 (the queue's `active` flag), Task 7 step 5 (the colour tokens), Task 8 step 6 (`Found`'s field names and `month_first`). These are not placeholders: the code to write is given in full, and what is deferred is a name this plan will not guess at rather than a decision left open.

**Type consistency.** `retire_corpus`/`unretire_corpus`/`is_retired`/`retired_among` are spelled the same in Tasks 1, 2 and 3. `SearchResult.retired` and `RenderedResult.retired` match. `hideIdle`/`showIdle` in Task 5 match their call sites in the same task. `refresh_in` is the field, the function and the template variable, deliberately, following `_queue.html`'s `active`. `#idle`, `#due`, `#intent-echo`, `#kind-row` and `#idle-foot` are each introduced once and referenced by the same id everywhere after.
