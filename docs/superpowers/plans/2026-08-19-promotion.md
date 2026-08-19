# Promotion, Consolidation and Origins Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `earned` becomes real — a passage that is opened or confirmed past a threshold gets its window synthesized, the new artifacts supersede the passages they cover by majority and inherit their access; consolidation judges rather than hides and never pairs rows from one window; a merged or synthesized artifact belongs to every corpus it drew from.

**Architecture:** Promotion is today's `Stage::SegmentWindow` armed by evidence: the two engagement bump sites (`mark_artifact_seen`, the association sweep's confirmed bump) call `promote::maybe_promote`, which moves a `verbatim` segment to `pending` with `keep_artifacts = 1` and arms the unit; the window job, seeing `keep_artifacts`, supersedes covered passages by the majority rule and carries activation and links forward. Consolidation's `classify_pair` gains one exclusion and loses its free hide. `Store::origins_of` projects lineage onto corpora; four readers use it, and the vector payload mirrors it.

**Tech Stack:** Rust 2024, sqlx/SQLite, askama, the existing job queue. No new crates.

**Spec:** `docs/superpowers/specs/2026-08-19-tiered-synthesis-design.md` — §3 (Promotion, incl. "Activation must actually move", "Carrying access forward", "Undo", "Ordinals after a promotion" already done), §6 (Consolidation), §7 (Origins). Pursuits (§4) are the last plan.

## Global Constraints

- Config, verbatim from the spec: `[promote] activation_above = 4.0`, `resynthesize_after_unconfirmed = 0` ("0 disables, and it ships disabled"). `feedback.enabled` becomes `true` by default; `earned` with `feedback.enabled = false` is a startup warning, not an error.
- The threshold is tested `>=`, after the bump, decay folded in — and only at the `opened` and `confirmed` bump sites, never at `retrieved`.
- Promotion arms a job; it never calls a model itself. No `max_per_sweep`.
- `keep_artifacts = 1` → append; the guard against re-promotion is the segment state (`done` never promotes again); a re-run under `keep_artifacts` over a window that already holds non-passage rows writes nothing.
- Majority rule: a passage is superseded only when some **one** artifact's span covers more than half its lines; best overlap wins, ties to the lowest ordinal; a passage nobody substantially claims stays active.
- Activation carries as the **max** decayed activation of the superseded passages, stamped now. Links are **copied**, not moved; collisions: decayed max weight, `bumped_at = now`, `queries` max, cues merged top-3 by count, `dismissed` wins, `judged_rev_*` cleared, state `learning` unless dismissed. `hit_count`, `last_seen_at`, `search_candidates`, judged pairs do **not** carry.
- Undo: passages back to active, the window's artifacts deprecated, segment back to `verbatim`; copied links and raised activation stay.
- Consolidation: two rows from the same `(corpus_id, segment_idx)` are never pair candidates; `auto_supersede` stops hiding and becomes a fast lane to the judge (the key stays); `losses()` untouched.
- Origins: derived, not stored; `origins_of` is non-empty for every active artifact; five call sites; `VectorPayload.origin_corpora`.
- `cargo fmt` before every commit, `cargo clippy --all-targets` clean per task, commit messages as in the earlier plans.

---

## File structure

| file | responsibility after this plan |
|---|---|
| `src/config.rs` | `PromoteConfig` + `[promote]`; `FeedbackConfig.enabled` default `true`; the earned-without-feedback warning |
| `src/core/mod.rs` | `Core.promote: PromoteConfig`; `Core::undo_promotion` |
| `src/store/segments.rs` | `segment_state(corpus, idx)` |
| `src/store/artifacts.rs` | `artifacts_for_segment`, `set_activation`, `artifact_confirmed` |
| `src/store/links.rs` | `links_touching`, `carry_link` (copy with collision merge) |
| `src/store/lineage.rs` | `origins_of`, `Origin` |
| `src/jobs/promote.rs` (new) | `maybe_promote`, `maybe_resynthesize`, `supersede_covered` (majority + carry), `covered_by` (pure) |
| `src/jobs/window.rs` | keep-path idempotency; calls `supersede_covered` |
| `src/core/search.rs` | `mark_artifact_seen` → `maybe_promote`; `mark_seen` → `maybe_resynthesize`; `cap_per_corpus` over origins |
| `src/jobs/associate.rs` | confirmed bump → `maybe_promote` |
| `src/jobs/relate.rs`, `src/jobs/consolidate.rs` | same-segment exclusion; fast lane instead of hide; header comment |
| `src/store/links.rs` (`links_from`, `links_to_judge`) | cross-corpus via origins |
| `src/vector/mod.rs`, `src/jobs/embed.rs` | `origin_corpora` payload field, written for model-written rows |
| `src/web/ui.rs`, `src/web/templates/corpus.html` | promoted windows list + undo; "written from this corpus" section |
| `config.example.toml`, `README.md` | `[promote]`, the feedback default |

---

### Task 1: Config — `[promote]`, and activation that actually moves

**Files:**
- Modify: `src/config.rs` (`FeedbackConfig` default `:107-117`; new `PromoteConfig`; `Config` struct field; `warn_on_inert_settings` `:1092+`)
- Modify: `src/core/mod.rs` (`Core.promote`, `from_config`, `test_support::build`), `tests/eval.rs` (Core literal), `src/main.rs` (Config literal if it names `feedback`/a new field — check with `grep -n "feedback:" src/main.rs`)
- Test: `src/config.rs`, `src/core/mod.rs` `mod tests`

**Interfaces:**
- Produces:
  ```rust
  #[derive(Debug, Deserialize, Clone)] #[serde(default)]
  pub struct PromoteConfig { pub activation_above: f64, pub resynthesize_after_unconfirmed: i64 }  // Default 4.0, 0
  pub struct Config { /* … */ pub promote: PromoteConfig }
  pub struct Core { /* … */ pub promote: crate::config::PromoteConfig }
  ```

- [ ] **Step 1: Write the failing tests**

`src/config.rs` `mod tests`:

```rust
    #[test]
    fn promote_defaults_and_feedback_ships_on() {
        let _guard = env_guard();
        let cfg = load_infer(&format!(
            "{BARE_PREAMBLE}
            [infer.synthesize]
            tier = \"efficient\"
            output_ratio = 8.0
            [infer.ask]
            tier = \"efficient\"
            "
        ))
        .unwrap();
        assert_eq!(cfg.promote.activation_above, 4.0);
        assert_eq!(cfg.promote.resynthesize_after_unconfirmed, 0);
        // Opt-out now: promotion reads activation, and activation only moves
        // while searches are recorded.
        assert!(cfg.feedback.enabled);
        let cfg = load_infer(&format!(
            "{BARE_PREAMBLE}
            [infer.synthesize]
            tier = \"efficient\"
            output_ratio = 8.0
            [infer.ask]
            tier = \"efficient\"
            [promote]
            activation_above = 2.5
            resynthesize_after_unconfirmed = 12
            [feedback]
            enabled = false
            "
        ))
        .unwrap();
        assert_eq!(cfg.promote.activation_above, 2.5);
        assert_eq!(cfg.promote.resynthesize_after_unconfirmed, 12);
        assert!(!cfg.feedback.enabled);
    }
```

`src/core/mod.rs` `mod tests`: the existing `associating_requires_both_flags_the_shipped_default_has_only_one` asserts `feedback.enabled == false` on `test_core()`. `test_core` builds `FeedbackConfig::default()`, which flips to `true` — but many tests assert "nothing moves unless feedback is on" by *setting* `core.feedback.enabled = true` and relying on the default being off elsewhere. Keep `test_support::build` at `feedback: FeedbackConfig { enabled: false, ..Default::default() }` with a comment ("Off in tests, whatever ships: the capture tests switch it on and the rest assert nothing is recorded"), and rewrite the test:

```rust
    #[tokio::test]
    async fn associating_requires_both_flags_and_the_shipped_default_has_both() {
        // Shipped defaults: `associate.enabled = true`, `feedback.enabled =
        // true` — promotion reads activation, and activation only moves while
        // searches are recorded, so recording is opt-out. The test core keeps
        // feedback off so every other test starts from nothing recorded.
        assert!(crate::config::FeedbackConfig::default().enabled);
        assert!(crate::config::AssociateConfig::default().enabled);
        let mut core = test_support::test_core().await;
        assert!(core.associate.enabled && !core.feedback.enabled);
        assert!(!core.associating(), "on with only associate.enabled set");
        core.feedback.enabled = true;
        assert!(core.associating());
        core.associate.enabled = false;
        assert!(!core.associating(), "on with only feedback.enabled set");
    }
```

(Read the old test body first and keep any assertion it makes that is still true.)

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib config::tests::promote_defaults 2>&1 | grep -E "^error" | head -3`
Expected: no field `promote`.

- [ ] **Step 3: Implement**

In `src/config.rs`, after `ActivationConfig`:

```rust
/// When a passage has earned its window a synthesis call, and when an eager
/// artifact has earned a second one.
///
/// `activation_above` is read against `[activation]`: baseline `1.0`,
/// `retrieved = 1.0`, `opened = 0.5`, `confirmed = 3.0`, half-life 14 days.
/// Checked with `>=` after the bump, decay folded in, and only at the two
/// engagement bumps — opened and confirmed — never at retrieved, so a passage
/// that merely keeps appearing in result lists is never promoted.
///
/// `resynthesize_after_unconfirmed` is the `eager` counterpart: an artifact
/// shown this many times with no confirmation recorded against it is
/// re-synthesised from its segment. `0` disables it, and it ships disabled —
/// re-synthesising changes what an existing base contains without anyone
/// asking, so it is a default the harness moves.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct PromoteConfig {
    pub activation_above: f64,
    pub resynthesize_after_unconfirmed: i64,
}

impl Default for PromoteConfig {
    fn default() -> Self {
        Self {
            activation_above: 4.0,
            resynthesize_after_unconfirmed: 0,
        }
    }
}
```

Add `pub promote: PromoteConfig` to `Config` with `#[serde(default)]` (look at how `activation` is declared on `Config` and copy its attributes). `FeedbackConfig::default().enabled = true`; update its doc comment: "On by default: promotion at `synthesis = \"earned\"` reads activation, and activation moves only while searches are recorded. Recording is the thing an operator turns *off*." In `warn_on_inert_settings` add:

```rust
        if self.infer.synthesis == SynthesisMode::Earned && !self.feedback.enabled {
            tracing::warn!(
                "infer.synthesis = \"earned\" with feedback.enabled = false: activation never \
                 moves, so nothing is ever promoted — this is `off` under another name."
            );
        }
```

`Core`: add `pub promote: crate::config::PromoteConfig` after `activation`; `from_config` sets `promote: cfg.promote.clone()`; `test_support::build` sets `promote: crate::config::PromoteConfig::default()` and `feedback: crate::config::FeedbackConfig { enabled: false, ..Default::default() }`; `tests/eval.rs` gets both fields the same way. `grep -rn "FeedbackConfig::default()" src tests` — every test that relied on the default being off and then asserted nothing was recorded must be read: if it built its core through `test_support`, it is fine; if it built `FeedbackConfig::default()` by hand expecting `enabled == false`, set `enabled: false` explicitly.

- [ ] **Step 4: Run everything**

Run: `cargo test 2>&1 | grep -E "^test result|FAILED|panicked" | head -5`
Expected: PASS. Expect the config test that loads `config.example.toml` to still pass (the example sets `feedback.enabled` explicitly? check — if it says `enabled = false` leave it; Task 9 documents the new default).

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --all-targets 2>&1 | grep -cE "^(warning|error)"
git add -A src tests
git commit -m "feat: a promote section, and recording on by default so activation can move"
```

---

### Task 2: Store plumbing for promotion

**Files:**
- Modify: `src/store/segments.rs` (new `segment_state`), `src/store/artifacts.rs` (new `artifacts_for_segment`, `set_activation`, `artifact_confirmed`), `src/store/links.rs` (new `links_touching`, `carry_link`)
- Test: each file's `mod tests`

**Interfaces:**
- Produces:
  ```rust
  impl Store {
      pub async fn segment_state(&self, corpus_id: &str, idx: i64) -> Result<Option<SegmentState>>;
      pub async fn artifacts_for_segment(&self, corpus_id: &str, idx: i64) -> Result<Vec<Chunk>>;   // all rows, any status, ORDER BY ordinal
      pub async fn set_activation(&self, id: &str, value: f64, at: i64) -> Result<()>;
      pub async fn artifact_confirmed(&self, id: &str) -> Result<bool>;   // any search_events row with verdict='hit' AND expect_id = id
      pub async fn links_touching(&self, id: &str) -> Result<Vec<Link>>;  // every row with a_id = id OR b_id = id, any state
      /// Copy `from` (one endpoint = `old`) onto the pair (`new`, other endpoint), merging on collision.
      pub async fn carry_link(&self, from: &Link, old: &str, new: &str, half_life_days: f64, at: i64) -> Result<()>;
  }
  ```

- [ ] **Step 1: Write the failing tests**

`src/store/links.rs` `mod tests` (it has a `two(&store)` helper returning two artifact ids; read it and the test around `bump_link` for how a link is seeded and read back with `get_link`):

```rust
    #[tokio::test]
    async fn carrying_a_link_copies_it_and_leaves_the_original() {
        let s = Store::memory().await.unwrap();
        let (p, x) = two(&s).await; // passage p, some artifact x
        let src = s.insert_corpus("raw2", "web", None).await.unwrap();
        let a = s
            .insert_artifacts(&src.id, &[NewArtifact {
                ordinal: 0, text: "the artifact".into(), corpus_span: None, title: None,
                category: None, tags: vec![], segment_idx: None, caveats: vec![],
            }])
            .await
            .unwrap()[0]
            .id
            .clone();
        s.bump_links(&[(p.as_str(), x.as_str())], 2.0, Some("how to mount"), 14.0, 1_000)
            .await
            .unwrap();
        let from = s.get_link(&p, &x).await.unwrap().unwrap();

        s.carry_link(&from, &p, &a, 14.0, 1_000 + 14 * 86_400).await.unwrap();

        let copied = s.get_link(&a, &x).await.unwrap().expect("the copy exists");
        // Decayed to the carry moment: one half-life later, half the weight.
        assert!((copied.weight - 1.0).abs() < 1e-6, "{}", copied.weight);
        assert_eq!(copied.bumped_at, 1_000 + 14 * 86_400);
        assert_eq!(copied.queries, from.queries);
        assert_eq!(copied.cues.len(), 1);
        assert_eq!(copied.state, LinkState::Learning);
        assert!(copied.judged_rev_a.is_none() && copied.judged_rev_b.is_none());
        // The original is still there, untouched.
        let orig = s.get_link(&p, &x).await.unwrap().unwrap();
        assert_eq!(orig.weight, from.weight);
    }

    #[tokio::test]
    async fn carrying_onto_an_existing_link_takes_the_max_not_the_sum_and_dismissed_wins() {
        let s = Store::memory().await.unwrap();
        let (p, x) = two(&s).await;
        let src = s.insert_corpus("raw2", "web", None).await.unwrap();
        let a = s
            .insert_artifacts(&src.id, &[NewArtifact {
                ordinal: 0, text: "the artifact".into(), corpus_span: None, title: None,
                category: None, tags: vec![], segment_idx: None, caveats: vec![],
            }])
            .await
            .unwrap()[0]
            .id
            .clone();
        let at = 5_000;
        s.bump_links(&[(p.as_str(), x.as_str())], 3.0, Some("q one"), 14.0, at).await.unwrap();
        s.bump_links(&[(a.as_str(), x.as_str())], 2.0, Some("q two"), 14.0, at).await.unwrap();
        // The operator dismissed the artifact's own link.
        s.set_link_state(&a, &x, LinkState::Dismissed, None, None).await.unwrap();
        let from = s.get_link(&p, &x).await.unwrap().unwrap();

        s.carry_link(&from, &p, &a, 14.0, at).await.unwrap();

        let merged = s.get_link(&a, &x).await.unwrap().unwrap();
        assert!((merged.weight - 3.0).abs() < 1e-6, "max, not 5.0: {}", merged.weight);
        assert_eq!(merged.queries, 1, "max of 1 and 1");
        assert_eq!(merged.cues.len(), 2, "cues merged");
        assert_eq!(merged.state, LinkState::Dismissed, "the operator's no is final");
    }

    #[tokio::test]
    async fn links_touching_finds_a_link_from_either_end() {
        let s = Store::memory().await.unwrap();
        let (p, x) = two(&s).await;
        s.bump_links(&[(p.as_str(), x.as_str())], 1.0, None, 14.0, 1).await.unwrap();
        assert_eq!(s.links_touching(&p).await.unwrap().len(), 1);
        assert_eq!(s.links_touching(&x).await.unwrap().len(), 1);
        assert!(s.links_touching("nobody").await.unwrap().is_empty());
    }
```

(`set_link_state`'s signature: read it at `links.rs:574` and pass what it takes — the test above assumes `(a, b, state, reason, judged_revs)`; adjust to the real one.)

`src/store/artifacts.rs` `mod tests`:

```rust
    #[tokio::test]
    async fn set_activation_writes_value_and_stamp_and_artifacts_for_segment_reads_every_status() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let mut a = nc(0, "a"); a.segment_idx = Some(3);
        let mut b = nc(1, "b"); b.segment_idx = Some(3);
        let mut c = nc(2, "c"); c.segment_idx = Some(4);
        let made = s.insert_artifacts(&src.id, &[a, b, c]).await.unwrap();
        s.set_superseded_by(&made[1].id, Some(&made[0].id)).await.unwrap();
        let seg = s.artifacts_for_segment(&src.id, 3).await.unwrap();
        assert_eq!(seg.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(), vec!["a", "b"]);

        s.set_activation(&made[0].id, 7.25, 99).await.unwrap();
        let act = s.activation_of(std::slice::from_ref(&made[0].id)).await.unwrap();
        assert_eq!(act[&made[0].id], (7.25, 99));
    }

    #[tokio::test]
    async fn artifact_confirmed_reads_a_hit_verdict_naming_it() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let made = s.insert_artifacts(&src.id, &[nc(0, "a")]).await.unwrap();
        assert!(!s.artifact_confirmed(&made[0].id).await.unwrap());
        // Record a search and judge it a hit on this artifact. Use the
        // feedback store's own API — read `record_search`/`judge` in
        // src/store/feedback.rs and the `seeded()` helper in
        // src/core/search.rs tests (~line 2380) for the shape.
        let ev = seed_search_event(&s, "q").await;
        s.judge(&ev, crate::store::feedback::Verdict::Hit /* with expect_id = made[0].id */).await.unwrap();
        assert!(s.artifact_confirmed(&made[0].id).await.unwrap());
    }
```

The second test's seeding must use the real feedback API: open `src/store/feedback.rs`, find the function that records a `SearchEvent` (`record_search` or similar, taking a `NewSearchEvent`/`SearchEvent` struct — `src/store/feedback.rs:120` shows `query_vec: Vec<f32>` in such a struct) and the one that judges (`judge(&event_id, Verdict, expect_id)` around `:470`); write a 10-line `seed_search_event` helper in the test module with those. If an existing test helper in `feedback.rs` already does it, call that.

`src/store/segments.rs` `mod tests`:

```rust
    #[tokio::test]
    async fn segment_state_reads_one_window() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        s.upsert_segments(&src.id, &[NewSegment { start_line: 1, end_line: 2, text: "t", carry_lines: 0 }])
            .await
            .unwrap();
        assert_eq!(s.segment_state(&src.id, 0).await.unwrap(), Some(SegmentState::Pending));
        s.mark_segments_verbatim(&src.id).await.unwrap();
        assert_eq!(s.segment_state(&src.id, 0).await.unwrap(), Some(SegmentState::Verbatim));
        assert_eq!(s.segment_state(&src.id, 9).await.unwrap(), None);
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib store:: 2>&1 | grep -E "^error" | sort | uniq -c | head`
Expected: the six new methods not found.

- [ ] **Step 3: Implement**

`segments.rs`:

```rust
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
```

`artifacts.rs`:

```rust
    /// Every row a window owns, whatever its status, in ordinal order. The
    /// promotion path reads this to see what it is superseding and, on a
    /// retry, what it already wrote.
    pub async fn artifacts_for_segment(&self, corpus_id: &str, idx: i64) -> Result<Vec<Chunk>> {
        let rows = sqlx::query(
            "SELECT * FROM artifacts WHERE corpus_id = ? AND segment_idx = ? ORDER BY ordinal",
        )
        .bind(corpus_id)
        .bind(idx)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_artifact).collect())
    }

    /// Set an artifact's activation outright, stamped `at`. Promotion uses it
    /// to hand a new artifact the access its passages earned; everything else
    /// goes through `bump_activation`, which adds.
    pub async fn set_activation(&self, id: &str, value: f64, at: i64) -> Result<()> {
        sqlx::query("UPDATE artifacts SET activation = ?, activated_at = ? WHERE id = ?")
            .bind(value)
            .bind(at)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Has a person ever said this artifact was the answer? A `hit` verdict
    /// naming it, on any recorded search.
    pub async fn artifact_confirmed(&self, id: &str) -> Result<bool> {
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM search_events WHERE verdict = 'hit' AND expect_id = ?",
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await?
            > 0)
    }
```

`links.rs`:

```rust
    /// Every link naming `id` at either end, in any state.
    pub async fn links_touching(&self, id: &str) -> Result<Vec<Link>> {
        let rows = sqlx::query("SELECT * FROM artifact_links WHERE a_id = ? OR b_id = ?")
            .bind(id)
            .bind(id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(row_to_link).collect())
    }

    /// Copy a link that touches `old` onto `new` and the same other endpoint,
    /// leaving the original row where it is.
    ///
    /// Learned access carries forward; recorded history does not. Weight comes
    /// across decayed to `at` and stamped `at`; `queries` and `cues` come
    /// across as they are. On collision with a link `new` already has: the
    /// larger decayed weight (max, not sum — one search returning three
    /// passages of one section is one piece of evidence), the larger
    /// `queries`, the cues merged and cut to three, `dismissed` if either side
    /// was (the operator's "not related" is final), otherwise `learning` — a
    /// verdict passed on the passage's text does not transfer to a rewrite —
    /// and `judged_rev_*` cleared. A link whose other end *is* `new`, or that
    /// would pair `new` with itself, is skipped.
    pub async fn carry_link(
        &self,
        from: &Link,
        old: &str,
        new: &str,
        half_life_days: f64,
        at: i64,
    ) -> Result<()> {
        let other = if from.a_id == old { &from.b_id } else { &from.a_id };
        if other == new || other == old {
            return Ok(());
        }
        let (a, b) = canonical(new, other);
        let incoming = decayed(from.weight, from.bumped_at, at, half_life_days);
        let mut tx = self.pool.begin_with(IMMEDIATE).await?;
        let existing = sqlx::query("SELECT * FROM artifact_links WHERE a_id = ? AND b_id = ?")
            .bind(a)
            .bind(b)
            .fetch_optional(&mut *tx)
            .await?
            .map(|r| row_to_link(&r));
        match existing {
            None => {
                let state = if from.state == LinkState::Dismissed {
                    LinkState::Dismissed
                } else {
                    LinkState::Learning
                };
                sqlx::query(
                    "INSERT INTO artifact_links
                       (a_id, b_id, weight, bumped_at, queries, cues, state, created_at)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(a)
                .bind(b)
                .bind(incoming)
                .bind(at)
                .bind(from.queries)
                .bind(serde_json::to_string(&from.cues).unwrap_or_else(|_| "[]".into()))
                .bind(state.as_str())
                .bind(now())
                .execute(&mut *tx)
                .await?;
            }
            Some(have) => {
                let weight = decayed(have.weight, have.bumped_at, at, half_life_days).max(incoming);
                let mut cues = have.cues.clone();
                for c in &from.cues {
                    match cues.iter_mut().find(|x| x.q == c.q) {
                        Some(x) => x.n = x.n.max(c.n),
                        None => cues.push(c.clone()),
                    }
                }
                cues.sort_by(|x, y| y.n.cmp(&x.n));
                cues.truncate(3);
                let state = if have.state == LinkState::Dismissed || from.state == LinkState::Dismissed {
                    LinkState::Dismissed
                } else {
                    LinkState::Learning
                };
                sqlx::query(
                    "UPDATE artifact_links
                        SET weight = ?, bumped_at = ?, queries = ?, cues = ?, state = ?,
                            reason = NULL, judged_rev_a = NULL, judged_rev_b = NULL
                      WHERE a_id = ? AND b_id = ?",
                )
                .bind(weight)
                .bind(at)
                .bind(have.queries.max(from.queries))
                .bind(serde_json::to_string(&cues).unwrap_or_else(|_| "[]".into()))
                .bind(state.as_str())
                .bind(a)
                .bind(b)
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }
```

(`Cue` needs `Clone` — check its derives; `LinkState` has `as_str`.)

- [ ] **Step 4: Run and commit**

Run: `cargo test --lib store:: 2>&1 | grep -E "^test result|FAILED|panicked"`
Expected: PASS.

```bash
cargo fmt && cargo clippy --all-targets 2>&1 | grep -cE "^(warning|error)"
git add src/store
git commit -m "feat: the store can carry a passage's access forward"
```

---

### Task 3: The trigger — engagement, at the bump

**Files:**
- Create: `src/jobs/promote.rs`
- Modify: `src/jobs/mod.rs` (`pub mod promote;`), `src/core/search.rs:415-445` (`mark_artifact_seen`), `src/jobs/associate.rs:275-290` (confirmed bump)
- Test: `src/jobs/promote.rs` `mod tests`

**Interfaces:**
- Consumes: `Store::segment_state`, `Store::activation_of`, `reset_segment(corpus, idx, keep_artifacts: true)`, `rearm_idle_seq(Stage::SegmentWindow, "segment", unit_target, idx)`, `Core.promote`, `Core.synthesis`, `Core::synthesizes()`.
- Produces: `pub async fn maybe_promote(core: &Core, ids: &[String], at: i64) -> Result<usize>` (how many windows were armed).

- [ ] **Step 1: Write the failing tests**

`src/jobs/promote.rs`:

```rust
//! Promotion: synthesis armed by evidence instead of by capture.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SynthesisMode;
    use crate::core::test_support::test_core;
    use crate::store::jobs::Stage;
    use crate::store::segments::SegmentState;

    /// A core at `earned`, recording, with one verbatim corpus of one passage.
    async fn earned_with_one_passage() -> (crate::core::Core, String, String) {
        let mut core = test_core().await;
        core.synthesis = SynthesisMode::Earned;
        core.feedback.enabled = true;
        let out = core.ingest("a single verbatim passage", "web", None).await.unwrap();
        crate::jobs::passages::capture_verbatim(&core, &out.id).await.unwrap();
        let p = core.store.artifacts_for_corpus(&out.id).await.unwrap()[0].id.clone();
        (core, out.id, p)
    }

    fn unit(corpus: &str) -> String {
        crate::jobs::window::unit_target(corpus, 0)
    }

    #[tokio::test]
    async fn a_passage_over_the_line_arms_its_window_once() {
        let (core, corpus, p) = earned_with_one_passage().await;
        // Baseline 1.0 plus one confirmed bump puts it at 4.0 exactly.
        core.store.bump_activation(std::slice::from_ref(&p), 3.0, 14.0, 1_000).await.unwrap();
        let armed = maybe_promote(&core, std::slice::from_ref(&p), 1_000).await.unwrap();
        assert_eq!(armed, 1);
        assert_eq!(core.store.segment_state(&corpus, 0).await.unwrap(), Some(SegmentState::Pending));
        assert!(core.store.segment_keeps_artifacts(&corpus, 0).await.unwrap());
        assert!(core.store.live_job(Stage::SegmentWindow, &unit(&corpus)).await.unwrap());
        // A second trigger on a window that is no longer verbatim does nothing.
        core.store.set_segment_state(&corpus, 0, SegmentState::Done, None).await.unwrap();
        let again = maybe_promote(&core, std::slice::from_ref(&p), 1_000).await.unwrap();
        assert_eq!(again, 0);
    }

    #[tokio::test]
    async fn under_the_line_nothing_is_armed() {
        let (core, corpus, p) = earned_with_one_passage().await;
        core.store.bump_activation(std::slice::from_ref(&p), 1.0, 14.0, 1_000).await.unwrap();
        assert_eq!(maybe_promote(&core, std::slice::from_ref(&p), 1_000).await.unwrap(), 0);
        assert_eq!(core.store.segment_state(&corpus, 0).await.unwrap(), Some(SegmentState::Verbatim));
    }

    #[tokio::test]
    async fn only_earned_with_a_synthesizer_promotes() {
        let (mut core, _corpus, p) = earned_with_one_passage().await;
        core.store.bump_activation(std::slice::from_ref(&p), 5.0, 14.0, 1_000).await.unwrap();
        core.synthesis = SynthesisMode::Off;
        assert_eq!(maybe_promote(&core, std::slice::from_ref(&p), 1_000).await.unwrap(), 0);
        core.synthesis = SynthesisMode::Earned;
        core.synthesizer = None;
        assert_eq!(maybe_promote(&core, std::slice::from_ref(&p), 1_000).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn retrieval_alone_never_promotes_but_one_open_afterwards_does() {
        // The threshold is checked at the opened bump, not the retrieved one:
        // ten retrievals leave the window verbatim; the first open promotes.
        let (core, corpus, p) = earned_with_one_passage().await;
        let ids = vec![p.clone()];
        for _ in 0..10 {
            core.store.bump_activation(&ids, core.activation.retrieved, 14.0, 1_000).await.unwrap();
        }
        // `mark_seen` is the retrieved site: it must not call `maybe_promote`.
        assert_eq!(core.store.segment_state(&corpus, 0).await.unwrap(), Some(SegmentState::Verbatim));
        core.mark_artifact_seen(&p);
        core.background.drain().await;
        assert_eq!(core.store.segment_state(&corpus, 0).await.unwrap(), Some(SegmentState::Pending));
    }
}
```

(`core.background.drain()` — check the name of the method that waits for spawned background writes in `src/core/background.rs`; several search tests use it.)

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib jobs::promote 2>&1 | grep -E "^error" | head -3`
Expected: `maybe_promote` not found (after adding `pub mod promote;`).

- [ ] **Step 3: Implement**

```rust
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
        let Some((value, stamp)) = activation.get(id) else { continue };
        let now_value = crate::store::links::decayed(*value, *stamp, at, core.activation.half_life_days);
        if now_value < core.promote.activation_above {
            continue;
        }
        let Ok(c) = core.store.get_artifact(id).await else { continue };
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
        tracing::info!(artifact_id = %id, corpus_id, window = idx, activation = now_value, "promoting a window");
        armed += 1;
    }
    Ok(armed)
}
```

`src/core/search.rs` `mark_artifact_seen`: inside the spawned task, after the `bump_activation` call succeeds:

```rust
                if let Err(e) = store.bump_activation(&ids, delta, half_life, at).await {
                    tracing::warn!(error = %e, "could not raise activation for opening");
                    return;
                }
                // An open is an engagement: the one kind of bump that can
                // promote. See `jobs::promote::maybe_promote`.
                if let Err(e) = crate::jobs::promote::maybe_promote(&core, &ids, at).await {
                    tracing::warn!(error = %e, "could not check the promotion threshold");
                }
```

— the task needs a `core` clone rather than just `store`: `let core = self.clone();` before the spawn and use `core.store` inside. Read the surrounding code and keep the `associating()` gate as it is.

`src/jobs/associate.rs:281` after the confirmed `bump_activation`:

```rust
        // A confirmation is the strongest engagement there is, and the other
        // bump that may promote.
        crate::jobs::promote::maybe_promote(core, std::slice::from_ref(&expect), at).await?;
```

`mark_seen` (the retrieved site, `search.rs:392-401`): **no call** — add one comment line there: "Never `maybe_promote` here: exposure is not engagement."

- [ ] **Step 4: Run and commit**

Run: `cargo test 2>&1 | grep -E "^test result|FAILED|panicked" | head -5`

```bash
cargo fmt && cargo clippy --all-targets 2>&1 | grep -cE "^(warning|error)"
git add -A src
git commit -m "feat: a passage that is read past the line arms its window — once, at the bump"
```

---

### Task 4: The window job supersedes by majority and carries access forward

**Files:**
- Modify: `src/jobs/promote.rs` (add `covered_by`, `supersede_covered`), `src/jobs/window.rs:327-350` (`write_segment_artifacts` idempotency), `src/jobs/window.rs:185-191` (`run`'s tail)
- Test: `src/jobs/promote.rs`, `src/jobs/window.rs` `mod tests`

**Interfaces:**
- Consumes: Task 2's store methods; `Core::supersede`; `decayed`.
- Produces:
  ```rust
  /// For each passage (id, span): the artifact covering a majority of its lines, if any. Pure.
  pub fn covered_by<'a>(passages: &'a [(String, CorpusSpan)], artifacts: &'a [(String, i64, CorpusSpan)]) -> Vec<(&'a str, &'a str)>;  // (passage_id, artifact_id)
  pub async fn supersede_covered(core: &Core, corpus_id: &str, idx: i64, written: &[Chunk], at: i64) -> Result<usize>;
  ```

- [ ] **Step 1: Write the failing tests**

`src/jobs/promote.rs` `mod tests`:

```rust
    use crate::store::artifacts::CorpusSpan;

    fn sp(a: i64, b: i64) -> CorpusSpan {
        CorpusSpan { start_line: a, end_line: b }
    }

    #[test]
    fn the_majority_rule_is_per_artifact_best_overlap_ties_to_the_lowest_ordinal() {
        // passage 1–20; A claims 1–11 (11 lines, majority), B claims 12–20 (9).
        let passages = vec![("p".to_string(), sp(1, 20))];
        let arts = vec![
            ("b".to_string(), 1, sp(12, 20)),
            ("a".to_string(), 0, sp(1, 11)),
        ];
        assert_eq!(covered_by(&passages, &arts), vec![("p", "a")]);
        // 30% + 30% is not a majority: nobody claims it.
        let arts = vec![("a".to_string(), 0, sp(1, 6)), ("b".to_string(), 1, sp(7, 12))];
        assert!(covered_by(&passages, &arts).is_empty());
        // Exactly half is not a majority either.
        let arts = vec![("a".to_string(), 0, sp(1, 10))];
        assert!(covered_by(&passages, &arts).is_empty());
        // A tie on overlap goes to the lowest ordinal.
        let arts = vec![("z".to_string(), 5, sp(1, 20)), ("y".to_string(), 2, sp(1, 20))];
        assert_eq!(covered_by(&passages, &arts), vec![("p", "y")]);
    }

    /// A verbatim corpus of three passages (lines 1–2, 3–4, 5–6 of one
    /// window) with activation and a link on the middle one; then a
    /// promotion whose artifact A claims lines 1–4 and B claims line 6.
    async fn promoted_fixture() -> (crate::core::Core, String, Vec<crate::store::artifacts::Chunk>, Vec<crate::store::artifacts::Chunk>) {
        let mut core = test_core().await;
        core.synthesis = SynthesisMode::Earned;
        core.feedback.enabled = true;
        let src = core.store.insert_corpus("l1\nl2\nl3\nl4\nl5\nl6", "web", None).await.unwrap();
        core.store
            .upsert_segments(&src.id, &[crate::store::segments::NewSegment { start_line: 1, end_line: 6, text: "l1\nl2\nl3\nl4\nl5\nl6", carry_lines: 0 }])
            .await
            .unwrap();
        core.store.mark_segments_verbatim(&src.id).await.unwrap();
        let na = |o: i64, t: &str, a: i64, b: i64| crate::store::artifacts::NewArtifact {
            ordinal: o, text: t.into(), corpus_span: Some(sp(a, b)), title: None, category: None,
            tags: vec![], segment_idx: Some(0), caveats: vec![],
        };
        let passages = core
            .store
            .insert_artifacts_with_provenance(&src.id, &[na(0, "l1 l2", 1, 2), na(1, "l3 l4", 3, 4), na(2, "l5 l6", 5, 6)], crate::store::artifacts::Provenance::Passage)
            .await
            .unwrap();
        // Another corpus to link to.
        let other = core.store.insert_corpus("other", "web", None).await.unwrap();
        let x = core.store.insert_artifacts(&other.id, &[na(0, "x", 1, 1)]).await.unwrap()[0].id.clone();
        core.store.bump_activation(std::slice::from_ref(&passages[1].id), 4.0, 14.0, 1_000).await.unwrap();
        core.store.bump_links(&[(passages[1].id.as_str(), x.as_str())], 2.0, Some("mid"), 14.0, 1_000).await.unwrap();
        // The promotion's artifacts, as `write_segment_artifacts` would write them.
        core.store.reset_segment(&src.id, 0, true).await.unwrap();
        let written = core
            .store
            .insert_artifacts(&src.id, &[na(0, "A covers one to four", 1, 4), na(1, "B covers six", 6, 6)])
            .await
            .unwrap();
        (core, src.id, passages, written)
    }

    #[tokio::test]
    async fn covered_passages_are_superseded_and_the_rest_stay_verbatim() {
        let (core, corpus, passages, written) = promoted_fixture().await;
        let n = supersede_covered(&core, &corpus, 0, &written, 2_000).await.unwrap();
        assert_eq!(n, 2, "passages 1 and 2 are majority-covered by A; passage 3 is half-covered by B");
        let p0 = core.store.get_artifact(&passages[0].id).await.unwrap();
        let p1 = core.store.get_artifact(&passages[1].id).await.unwrap();
        let p2 = core.store.get_artifact(&passages[2].id).await.unwrap();
        assert_eq!(p0.superseded_by.as_deref(), Some(written[0].id.as_str()));
        assert_eq!(p1.superseded_by.as_deref(), Some(written[0].id.as_str()));
        assert!(p2.in_results(), "lines 5–6: B claims one of two, not a majority");
    }

    #[tokio::test]
    async fn the_artifact_takes_the_max_decayed_activation_not_one_point_zero() {
        let (core, corpus, passages, written) = promoted_fixture().await;
        supersede_covered(&core, &corpus, 0, &written, 1_000).await.unwrap();
        let act = core.store.activation_of(&[written[0].id.clone(), passages[1].id.clone()]).await.unwrap();
        let (a_val, a_at) = act[&written[0].id];
        let (p_val, p_at) = act[&passages[1].id];
        let expect = crate::store::links::decayed(p_val, p_at, 1_000, 14.0);
        assert!((a_val - expect).abs() < 1e-6, "got {a_val}, want {expect}");
        assert_eq!(a_at, 1_000);
        assert!(a_val > 1.0);
    }

    #[tokio::test]
    async fn a_link_from_a_superseded_passage_resolves_on_the_artifact_and_the_dead_row_stays() {
        let (core, corpus, passages, written) = promoted_fixture().await;
        supersede_covered(&core, &corpus, 0, &written, 1_000).await.unwrap();
        let out = core
            .store
            .links_from(&[written[0].id.clone()], &[crate::store::links::LinkState::Learning], 14.0, 1_000, 0.0, 10)
            .await
            .unwrap();
        assert_eq!(out.len(), 1, "{out:?}");
        assert!((out[0].weight - 2.0).abs() < 1e-6);
        // The passage's own row is still there — dark, because its endpoint is superseded.
        assert_eq!(core.store.links_touching(&passages[1].id).await.unwrap().len(), 1);
        let from_passage = core
            .store
            .links_from(&[passages[1].id.clone()], &[crate::store::links::LinkState::Learning], 14.0, 1_000, 0.0, 10)
            .await
            .unwrap();
        assert!(from_passage.is_empty());
    }

    #[tokio::test]
    async fn a_re_run_under_keep_artifacts_writes_nothing_twice() {
        let (core, corpus, _passages, written) = promoted_fixture().await;
        // `write_segment_artifacts` with keep set and non-passage rows already
        // present returns those rows and inserts none.
        let again = crate::jobs::window::write_segment_artifacts(
            &core, &corpus, 0,
            vec![crate::store::artifacts::NewArtifact { ordinal: 0, text: "dup".into(), corpus_span: None, title: None, category: None, tags: vec![], segment_idx: Some(0), caveats: vec![] }],
        )
        .await
        .unwrap();
        assert_eq!(again.iter().map(|c| c.id.clone()).collect::<Vec<_>>(), written.iter().map(|c| c.id.clone()).collect::<Vec<_>>());
        assert_eq!(core.store.artifacts_for_segment(&corpus, 0).await.unwrap().len(), 5);
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib jobs::promote 2>&1 | grep -E "^error" | head -3`

- [ ] **Step 3: Implement**

In `src/jobs/promote.rs`:

```rust
use crate::store::artifacts::{Chunk, CorpusSpan};

fn overlap(a: &CorpusSpan, b: &CorpusSpan) -> i64 {
    (a.end_line.min(b.end_line) - a.start_line.max(b.start_line) + 1).max(0)
}

/// Which artifact, if any, supersedes each passage: the one whose span covers
/// a **majority** of the passage's lines — per artifact, not cumulative,
/// because `supersede` names one winner and a passage hidden behind an
/// artifact holding a third of it sends the reader to the wrong text. Best
/// overlap wins; a tie goes to the lowest ordinal. Everything else stays
/// active, verbatim, in results: promotion can only ever improve coverage.
pub fn covered_by<'a>(
    passages: &'a [(String, CorpusSpan)],
    artifacts: &'a [(String, i64, CorpusSpan)],
) -> Vec<(&'a str, &'a str)> {
    let mut out = Vec::new();
    for (pid, ps) in passages {
        let len = ps.end_line - ps.start_line + 1;
        let best = artifacts
            .iter()
            .map(|(aid, ord, asp)| (overlap(ps, asp), *ord, aid.as_str()))
            .filter(|(ov, _, _)| 2 * ov > len)
            .max_by(|x, y| x.0.cmp(&y.0).then(y.1.cmp(&x.1)));
        if let Some((_, _, aid)) = best {
            out.push((pid.as_str(), aid));
        }
    }
    out
}

/// After a promoted window's artifacts are written: supersede the passages
/// they cover and carry the passages' access forward.
///
/// Activation first, links second, the supersede last — `supersede` refuses a
/// side that is no longer active, and everything before it needs the passage
/// readable. Returns how many passages were superseded.
pub async fn supersede_covered(
    core: &Core,
    corpus_id: &str,
    idx: i64,
    written: &[Chunk],
    at: i64,
) -> Result<usize> {
    let rows = core.store.artifacts_for_segment(corpus_id, idx).await?;
    let passages: Vec<(String, CorpusSpan)> = rows
        .iter()
        .filter(|c| c.provenance == Provenance::Passage && c.in_results())
        .filter_map(|c| Some((c.id.clone(), c.corpus_span.clone()?)))
        .collect();
    let artifacts: Vec<(String, i64, CorpusSpan)> = written
        .iter()
        .filter_map(|c| Some((c.id.clone(), c.ordinal, c.corpus_span.clone()?)))
        .collect();
    let pairs = covered_by(&passages, &artifacts);
    if pairs.is_empty() {
        return Ok(0);
    }
    let half_life = core.activation.half_life_days;
    let link_half_life = core.associate.half_life_days;

    // Group by winner: one artifact may supersede several passages, and its
    // activation is the max over all of them.
    let mut by_winner: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
    for (p, a) in &pairs {
        by_winner.entry(a).or_default().push(p);
    }
    let all_superseded: std::collections::HashSet<&str> = pairs.iter().map(|(p, _)| *p).collect();
    let mut n = 0;
    for (winner, losers) in by_winner {
        let ids: Vec<String> = losers.iter().map(|s| s.to_string()).collect();
        let act = core.store.activation_of(&ids).await?;
        let carried = act
            .values()
            .map(|(v, s)| crate::store::links::decayed(*v, *s, at, half_life))
            .fold(f64::MIN, f64::max);
        if carried.is_finite() && carried > f64::MIN {
            let own = core
                .store
                .activation_of(std::slice::from_ref(&winner.to_string()))
                .await?
                .get(winner)
                .map(|(v, s)| crate::store::links::decayed(*v, *s, at, half_life))
                .unwrap_or(1.0);
            core.store.set_activation(winner, carried.max(own), at).await?;
        }
        for loser in &losers {
            for link in core.store.links_touching(loser).await? {
                let other = if link.a_id == *loser { &link.b_id } else { &link.a_id };
                // A link between two passages of this same promotion would
                // become the artifact linked to itself — or to a passage about
                // to go dark. Neither carries anything.
                if all_superseded.contains(other.as_str()) {
                    continue;
                }
                core.store.carry_link(&link, loser, winner, link_half_life, at).await?;
            }
        }
        for loser in &losers {
            if crate::jobs::try_supersede(core, loser, winner, "a passage its promotion covers").await {
                n += 1;
            }
        }
    }
    Ok(n)
}
```

`src/jobs/window.rs` `write_segment_artifacts`: after reading `keep`:

```rust
    if keep {
        // A promotion re-run: the process died between the insert and `done`.
        // Under `keep_artifacts` the write appends, so writing again would put
        // the window's artifacts in twice. Rows this window already holds that
        // are not passages are the earlier write; return them and insert none.
        let have: Vec<_> = core
            .store
            .artifacts_for_segment(corpus_id, segment_idx)
            .await?
            .into_iter()
            .filter(|c| c.provenance != crate::store::artifacts::Provenance::Passage && c.in_results())
            .collect();
        if !have.is_empty() {
            tracing::info!(corpus_id, window = segment_idx, "window already written under keep_artifacts; not writing again");
            return Ok(have);
        }
    }
```

(Note this changes the operator-reread semantics slightly: a re-read of a window that already has artifacts and `keep=1` now returns the existing rows rather than appending a second read. That is what `keep` promotion needs; for the reread button the model's new artifacts *are* the point. Distinguish: promotion sets `keep`, reread sets `keep` too. Check `reread_uncovered_ui` — it calls `reset_segment(.., true)`. To keep that path appending: gate the early return on **the window being a promotion**, i.e. `rows.iter().any(|c| c.provenance == Passage)`. A reread window has no passages (eager base), so it still appends.) Make the filter: `let is_promotion = rows.iter().any(passage)`, `if is_promotion && !have.is_empty() { return Ok(have) }`.

`run`'s tail (`window.rs:185-191`):

```rust
    let keep = core.store.segment_keeps_artifacts(corpus_id, idx).await?;
    let written =
        write_segment_artifacts(core, corpus_id, idx, proposed_to_new(idx, chunks)).await?;
    flag_unverified(core, &written, &text).await?;
    if keep {
        // A promotion: what the window's artifacts cover, they supersede, and
        // the passages' access comes with them. Under the corpus lock as a
        // second locked step — `write_segment_artifacts` took and released it.
        let _corpus = core.corpus_lock(corpus_id).await;
        let n = crate::jobs::promote::supersede_covered(core, corpus_id, idx, &written, crate::store::now())
            .await?;
        tracing::info!(corpus_id, window = idx, superseded = n, "promotion superseded its covered passages");
    }
    core.store
        .set_segment_state(corpus_id, idx, SegmentState::Done, None)
        .await?;
```

(`crate::store::now()` — check the name of the unix-seconds helper the store uses (`now()` in `artifacts.rs`); use whatever `mark_seen` uses: `now_secs()` in `core`.)

- [ ] **Step 4: Run and commit**

Run: `cargo test 2>&1 | grep -E "^test result|FAILED|panicked" | head -5`

```bash
cargo fmt && cargo clippy --all-targets 2>&1 | grep -cE "^(warning|error)"
git add -A src
git commit -m "feat: a promotion supersedes what it covers and keeps the access the passages earned"
```

---

### Task 5: Undo

**Files:**
- Modify: `src/core/ingest.rs` (new `undo_promotion`, near `unsupersede`), `src/web/ui.rs` (route + `CorpusTemplate.promoted`), `src/web/templates/corpus.html`
- Test: `src/core/ingest.rs`, `src/web/ui.rs` `mod tests`

**Interfaces:**
- Produces: `pub async fn undo_promotion(&self, corpus_id: &str, idx: i64) -> Result<()>` on `Core`; route `POST /ui/corpora/{id}/segments/{idx}/unpromote`; `CorpusTemplate.promoted: Vec<PromotedWindow { idx: i64, from: i64, to: i64 }>`.

- [ ] **Step 1: Write the failing tests**

`src/core/ingest.rs` `mod tests`:

```rust
    #[tokio::test]
    async fn undoing_a_promotion_restores_the_passages_deprecates_the_artifacts_and_resets_the_window() {
        let core = test_core().await;
        let src = core.store.insert_corpus("l1\nl2", "web", None).await.unwrap();
        core.store
            .upsert_segments(&src.id, &[crate::store::segments::NewSegment { start_line: 1, end_line: 2, text: "l1\nl2", carry_lines: 0 }])
            .await
            .unwrap();
        let na = |o: i64, t: &str| crate::store::artifacts::NewArtifact {
            ordinal: o, text: t.into(), corpus_span: Some(crate::store::artifacts::CorpusSpan { start_line: 1, end_line: 2 }),
            title: None, category: None, tags: vec![], segment_idx: Some(0), caveats: vec![],
        };
        let p = core.store.insert_artifacts_with_provenance(&src.id, &[na(0, "passage")], crate::store::artifacts::Provenance::Passage).await.unwrap();
        let a = core.store.insert_artifacts(&src.id, &[na(1, "artifact")]).await.unwrap();
        core.supersede(&p[0].id, &a[0].id).await.unwrap();
        core.store.set_segment_state(&src.id, 0, crate::store::segments::SegmentState::Done, None).await.unwrap();

        core.undo_promotion(&src.id, 0).await.unwrap();

        assert!(core.store.get_artifact(&p[0].id).await.unwrap().in_results());
        assert_eq!(core.store.get_artifact(&a[0].id).await.unwrap().status, crate::store::artifacts::ArtifactStatus::Deprecated);
        assert_eq!(core.store.segment_state(&src.id, 0).await.unwrap(), Some(crate::store::segments::SegmentState::Verbatim));
    }
```

`src/web/ui.rs` `mod tests`: a test that builds the same fixture through a core, renders `/ui/corpora/{id}`, asserts the page contains `segments/0/unpromote`, POSTs it with the `form` helper, and asserts the redirect (303/302) and that the passage is active again.

- [ ] **Step 2: Run to verify failure** — `undo_promotion` not found.

- [ ] **Step 3: Implement**

`src/core/ingest.rs`, after `unsupersede`:

```rust
    /// Put a promoted window back: its passages active, the artifacts the
    /// promotion wrote deprecated, the segment `verbatim` again so it may
    /// promote afresh. The links copied onto the artifacts and the activation
    /// they were handed stay where they are — both sides describe the same
    /// corpus lines, and the asymmetry is accepted rather than fixed.
    pub async fn undo_promotion(&self, corpus_id: &str, idx: i64) -> Result<()> {
        let rows = self.store.artifacts_for_segment(corpus_id, idx).await?;
        for c in rows.iter().filter(|c| c.provenance == Provenance::Passage && c.superseded_by.is_some()) {
            self.unsupersede(&c.id).await?;
        }
        for c in rows.iter().filter(|c| c.provenance != Provenance::Passage && c.in_results()) {
            self.deprecate(&c.id).await?;
        }
        self.store
            .set_segment_state(corpus_id, idx, SegmentState::Verbatim, None)
            .await?;
        tracing::info!(corpus_id, window = idx, "promotion undone");
        Ok(())
    }
```

(`unsupersede` refuses nothing; `deprecate` refuses a superseded row — the filter above avoids that. Import `Provenance`/`SegmentState` as the file does.)

UI: `CorpusTemplate` gains `promoted: Vec<PromotedWindow>` (`pub struct PromotedWindow { pub idx: i64, pub from: i64, pub to: i64 }`), computed in `corpus_detail` as every segment whose state is `Done` and that owns at least one `Passage` row with `superseded_by` set:

```rust
    let promoted: Vec<PromotedWindow> = segments
        .iter()
        .filter(|w| w.state == crate::store::segments::SegmentState::Done)
        .filter(|w| chunks.iter().any(|c| c.segment_idx == Some(w.idx) && c.provenance == crate::store::artifacts::Provenance::Passage && c.superseded_by.is_some()))
        .map(|w| PromotedWindow { idx: w.idx, from: w.start_line, to: w.end_line })
        .collect();
```

Route: `.route("/ui/corpora/{id}/segments/{idx}/unpromote", post(unpromote_ui))` with

```rust
async fn unpromote_ui(
    State(st): State<AppState>,
    _id: Identity,
    Path((cid, idx)): Path<(String, i64)>,
) -> Result<Response> {
    st.core.undo_promotion(&cid, idx).await?;
    Ok(Redirect::to(&format!("/ui/corpora/{cid}")).into_response())
}
```

`corpus.html`, before the bands include:

```html
{% if !promoted.is_empty() %}
<p class="muted">
  {# A window synthesis has read because its passages were read. Undo puts
     the verbatim text back in results and retires what was written. #}
  Promoted:
  {% for w in promoted %}
  <form method="post" action="/ui/corpora/{{ id }}/segments/{{ w.idx }}/unpromote" style="display:inline">
    lines {{ w.from }}–{{ w.to }}
    <button class="btn btn-ghost btn-sm" type="submit">undo</button>
  </form>
  {% endfor %}
</p>
{% endif %}
```

- [ ] **Step 4: Run and commit**

```bash
cargo test 2>&1 | grep -E "^test result|FAILED|panicked" | head -3
cargo fmt && cargo clippy --all-targets 2>&1 | grep -cE "^(warning|error)"
git add -A src
git commit -m "feat: a promotion can be undone — passages back, artifacts retired, window verbatim"
```

---

### Task 6: Re-synthesis at `eager` (ships disabled)

**Files:**
- Modify: `src/jobs/promote.rs` (`maybe_resynthesize`), `src/core/search.rs:385-401` (`mark_seen`)
- Test: `src/jobs/promote.rs` `mod tests`

**Interfaces:**
- Produces: `pub async fn maybe_resynthesize(core: &Core, hits: &[(String, i64)] /* (artifact_id, hit_count after this retrieval) */) -> Result<usize>`.

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn an_eager_artifact_shown_often_and_never_confirmed_is_re_read_from_its_segment_when_enabled() {
        let mut core = test_core().await;
        core.promote.resynthesize_after_unconfirmed = 3;
        let src = core.store.insert_corpus("l1\nl2", "web", None).await.unwrap();
        core.store
            .upsert_segments(&src.id, &[crate::store::segments::NewSegment { start_line: 1, end_line: 2, text: "l1\nl2", carry_lines: 0 }])
            .await
            .unwrap();
        core.store.set_segment_state(&src.id, 0, SegmentState::Done, None).await.unwrap();
        let a = core.store.insert_artifacts(&src.id, &[crate::store::artifacts::NewArtifact {
            ordinal: 0, text: "artifact".into(), corpus_span: None, title: None, category: None,
            tags: vec![], segment_idx: Some(0), caveats: vec![],
        }]).await.unwrap()[0].id.clone();
        // Under the line: nothing.
        assert_eq!(maybe_resynthesize(&core, &[(a.clone(), 2)]).await.unwrap(), 0);
        // At the line, unconfirmed: the window is re-armed to *replace*.
        assert_eq!(maybe_resynthesize(&core, &[(a.clone(), 3)]).await.unwrap(), 1);
        assert_eq!(core.store.segment_state(&src.id, 0).await.unwrap(), Some(SegmentState::Pending));
        assert!(!core.store.segment_keeps_artifacts(&src.id, 0).await.unwrap(), "replace, not append: the old artifacts are the problem");
        assert!(core.store.live_job(Stage::SegmentWindow, &unit(&src.id)).await.unwrap());
        // Disabled (0) never fires.
        core.promote.resynthesize_after_unconfirmed = 0;
        core.store.set_segment_state(&src.id, 0, SegmentState::Done, None).await.unwrap();
        assert_eq!(maybe_resynthesize(&core, &[(a.clone(), 99)]).await.unwrap(), 0);
    }
```

- [ ] **Step 2: Run to verify failure**

- [ ] **Step 3: Implement**

```rust
/// The `eager` counterpart of promotion: an artifact shown
/// `resynthesize_after_unconfirmed` times with no confirmation recorded
/// against it is misleading, and is re-synthesised from its source segment —
/// never from itself. `keep_artifacts = 0`: replace, because the old artifacts
/// are the problem. `0` disables it and it ships disabled.
pub async fn maybe_resynthesize(core: &Core, hits: &[(String, i64)]) -> Result<usize> {
    let line = core.promote.resynthesize_after_unconfirmed;
    if line <= 0 || core.synthesis != crate::config::SynthesisMode::Eager || !core.synthesizes() {
        return Ok(0);
    }
    let mut armed = 0;
    for (id, count) in hits {
        if *count < line {
            continue;
        }
        let Ok(c) = core.store.get_artifact(id).await else { continue };
        if c.provenance != Provenance::Captured || !c.in_results() {
            continue;
        }
        let (Some(corpus_id), Some(idx)) = (c.corpus_id.as_deref(), c.segment_idx) else { continue };
        if core.store.segment_state(corpus_id, idx).await? != Some(SegmentState::Done) {
            continue;
        }
        if core.store.artifact_confirmed(id).await? {
            continue;
        }
        core.store.reset_segment(corpus_id, idx, false).await?;
        core.store
            .rearm_idle_seq(Stage::SegmentWindow, "segment", &crate::jobs::window::unit_target(corpus_id, idx), idx)
            .await?;
        tracing::info!(artifact_id = %id, corpus_id, window = idx, shown = count, "re-synthesising an unconfirmed window");
        armed += 1;
    }
    Ok(armed)
}
```

`mark_seen` (`search.rs`): where it spawns the retrieved bump, also (only when `self.promote.resynthesize_after_unconfirmed > 0`, so the disabled path costs nothing) collect `(artifact_id, hit_counts[id] + 1)` for the results and call `maybe_resynthesize` in the same background task after the bump. The bump site stays retrieval-only for promotion.

- [ ] **Step 4: Run and commit**

```bash
cargo test 2>&1 | grep -E "^test result|FAILED|panicked" | head -3
cargo fmt && cargo clippy --all-targets 2>&1 | grep -cE "^(warning|error)"
git add -A src
git commit -m "feat: an eager artifact shown often and never confirmed can be re-read — off by default"
```

---

### Task 7: Consolidation — one exclusion, and the hide becomes a fast lane

**Files:**
- Modify: `src/jobs/relate.rs:116-135` (`classify_pair`), `src/jobs/consolidate.rs:1-31` (header)
- Test: `src/jobs/relate.rs`, `src/jobs/consolidate.rs` `mod tests` (update `a_near_identical_pair_supersedes_the_older_artifact`, `the_cluster_pass_settles_a_pair_the_relate_unit_filed`, `a_pair_the_cluster_pass_answered_stops_occupying_the_window`)

- [ ] **Step 1: Write/rewrite the tests**

`src/jobs/relate.rs` `mod tests`:

```rust
    #[tokio::test]
    async fn two_rows_from_one_window_are_never_a_pair() {
        // A promoted artifact and the passage it left standing in the same
        // window look like a duplicate pair and are not one: overlap inside a
        // window is the window job's decision, already made.
        let core = test_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let na = |o: i64, t: &str, seg: i64| crate::store::artifacts::NewArtifact {
            ordinal: o, text: t.into(), corpus_span: None, title: Some("same heading".into()),
            category: None, tags: vec![], segment_idx: Some(seg), caveats: vec![],
        };
        let same = core.store.insert_artifacts(&src.id, &[na(0, "mount the disk first", 0), na(1, "mount the disk, first", 0)]).await.unwrap();
        let other = core.store.insert_artifacts(&src.id, &[na(2, "mount the disk first please", 1)]).await.unwrap();
        for c in same.iter().chain(other.iter()) {
            crate::jobs::embed::run(&core, &c.id).await.unwrap();
        }
        run(&core, &same[0].id).await.unwrap();
        let pending = core.store.pairs_by_state(PairState::Pending, 10).await.unwrap();
        assert!(
            !pending.iter().any(|p| (p.a_id == same[0].id && p.b_id == same[1].id) || (p.a_id == same[1].id && p.b_id == same[0].id)),
            "same-window pair was filed: {pending:?}"
        );
        // Across windows of the same corpus a pair is still a question.
        assert!(
            pending.iter().any(|p| p.a_id == other[0].id || p.b_id == other[0].id),
            "cross-window pair not filed: {pending:?}"
        );
    }

    #[tokio::test]
    async fn a_pair_above_auto_supersede_is_filed_for_the_judge_not_hidden() {
        let core = test_core().await;
        let ids = seed(&core, &[("the same words", [1.0, 0.0]), ("the same words", [1.0, 0.0])]).await;
        run(&core, &ids[1]).await.unwrap();
        // Nobody is hidden…
        assert!(core.store.get_artifact(&ids[0]).await.unwrap().in_results());
        assert!(core.store.get_artifact(&ids[1]).await.unwrap().in_results());
        // …and the pair is pending, first in line by score.
        let to_judge = core.store.pairs_to_judge(10).await.unwrap();
        assert_eq!(to_judge.len(), 1);
        assert!(core.store.pairs_by_state(PairState::NearIdentical, 10).await.unwrap().is_empty());
    }
```

(The `seed` helper lives in `consolidate.rs` tests and is already imported by `relate.rs` tests; the `same words` vectors give cosine 1.0 — with the FakeEmbedder this fixture may need `embed::run` instead; read `seed` to see whether it upserts vectors directly. Use whichever gives a score ≥ `auto_supersede` — `seed` takes explicit vectors, so it does.)

`consolidate.rs`: rewrite `a_near_identical_pair_supersedes_the_older_artifact` to assert the opposite — after `sweep_and_dedupe`, nothing is superseded *by the cluster pass* and a dedupe unit was armed for the pair (rename it `a_near_identical_pair_goes_to_the_judge_first`). The two cluster-pass tests that file `NearIdentical` rows by hand (`record_settled_pair(.., NearIdentical)`) still pass: the pass still closes rows that already exist — keep them, adding a comment that no new row is filed that way.

- [ ] **Step 2: Run to verify failure**

- [ ] **Step 3: Implement**

`classify_pair` (`relate.rs:116`): replace the free-band branch

```rust
    // The free band. Filed, not acted on: …
    if score >= core.consolidate.auto_supersede {
        core.store
            .record_settled_pair(&a.id, &b.id, score, PairState::NearIdentical)
            .await?;
        return Ok(false);
    }
```

with, placed right after the `in_results` guard:

```rust
    // Two rows from one window are not a pair. Neighbours under one heading
    // are similar for how they were built, not for what they say; and a
    // promoted artifact beside the passage it left standing is the window
    // job's decision — the majority rule — already made. Sending that pair to
    // the judge would spend a call to merge, and so hide behind model text,
    // exactly the verbatim passage promotion just decided to keep.
    if a.corpus_id.is_some()
        && a.corpus_id == b.corpus_id
        && a.segment_idx.is_some()
        && a.segment_idx == b.segment_idx
    {
        return Ok(false);
    }
```

and nothing at all for `auto_supersede` — the pair falls through the containment check to `record_pair`, where `pairs_to_judge`'s `ORDER BY score DESC` puts it first. Keep `auto_supersede` in `ConsolidateConfig` (validated above `review_min` as today) and reword its doc comment: "No longer a hide. A pair at or above it is judged first — a fast lane — and the judge's `losses` check stands behind the decision, because embeddings barely distinguish negation and 'runs on ext4' / 'does not run on ext4' sit far above any realistic threshold." Update the `consolidate.rs` header paragraph "Two thresholds still divide the work…" to say the same.

- [ ] **Step 4: Run and commit**

```bash
cargo test 2>&1 | grep -E "^test result|FAILED|panicked" | head -3
cargo fmt && cargo clippy --all-targets 2>&1 | grep -cE "^(warning|error)"
git add -A src
git commit -m "feat: consolidation judges instead of hiding, and never pairs two rows of one window"
```

---

### Task 8: Origins — derived, not stored

**Files:**
- Modify: `src/store/lineage.rs` (`Origin`, `origins_of`), `src/vector/mod.rs` (`origin_corpora`), `src/jobs/embed.rs` (`payload_of` → async origins for model-written rows), `src/core/search.rs` (`cap_per_corpus`), `src/store/links.rs` (`links_from` `cross_corpus`, `links_to_judge`), `src/web/ui.rs` + `corpus.html` ("written from this corpus")
- Test: each `mod tests`

**Interfaces:**
- Produces:
  ```rust
  #[derive(Debug, Clone, PartialEq)] pub struct Origin { pub corpus_id: String, pub span: Option<CorpusSpan> }
  impl Store { pub async fn origins_of(&self, ids: &[String]) -> Result<BTreeMap<String, Vec<Origin>>>; pub async fn artifacts_originating_in(&self, corpus_id: &str) -> Result<Vec<Chunk>> /* model-written rows with a root in this corpus */ }
  pub struct VectorPayload { /* … */ #[serde(default, skip_serializing_if = "Vec::is_empty")] pub origin_corpora: Vec<String> }
  ```

- [ ] **Step 1: Write the failing tests**

`src/store/lineage.rs` `mod tests` (there is a helper that inserts a merged artifact with sources — `insert_merged`; read the existing merge test at `:398` and reuse its shape):

```rust
    #[tokio::test]
    async fn origins_of_a_merge_are_its_roots_corpora_and_spans() {
        let s = Store::memory().await.unwrap();
        let c1 = s.insert_corpus("one", "web", None).await.unwrap();
        let c2 = s.insert_corpus("two", "web", None).await.unwrap();
        let na = |t: &str, a: i64, b: i64| crate::store::artifacts::NewArtifact {
            ordinal: 0, text: t.into(), corpus_span: Some(crate::store::artifacts::CorpusSpan { start_line: a, end_line: b }),
            title: None, category: None, tags: vec![], segment_idx: Some(0), caveats: vec![],
        };
        let r1 = s.insert_artifacts(&c1.id, &[na("r1", 1, 3)]).await.unwrap()[0].id.clone();
        let r2 = s.insert_artifacts(&c2.id, &[na("r2", 7, 9)]).await.unwrap()[0].id.clone();
        let m = /* insert a merged artifact with roots r1, r2 — the helper the merge tests use */;
        let o = s.origins_of(&[m.id.clone(), r1.clone()]).await.unwrap();
        let mut got: Vec<(String, Option<i64>)> = o[&m.id].iter().map(|x| (x.corpus_id.clone(), x.span.as_ref().map(|sp| sp.start_line))).collect();
        got.sort();
        assert_eq!(got, vec![(c1.id.clone(), Some(1)), (c2.id.clone(), Some(7))]);
        assert_eq!(o[&r1], vec![Origin { corpus_id: c1.id.clone(), span: Some(crate::store::artifacts::CorpusSpan { start_line: 1, end_line: 3 }) }]);
        // Every active artifact has at least one origin.
        assert!(o.values().all(|v| !v.is_empty()));
        // And the corpus page can find the merge.
        let from_c2 = s.artifacts_originating_in(&c2.id).await.unwrap();
        assert_eq!(from_c2.iter().map(|c| c.id.clone()).collect::<Vec<_>>(), vec![m.id.clone()]);
    }
```

`src/core/search.rs` `mod tests`: extend the existing `cap_per_corpus` tests (`:1525`) with a hit whose payload has `corpus_id: ""` and `origin_corpora: vec!["a", "b"]` and assert it counts against both `a` and `b`.

`src/store/links.rs`: a test that a link between a merged artifact (roots in corpus 1) and a captured artifact in corpus 1 reads `cross_corpus == false` from `links_from`, and that `links_to_judge` does not offer it (same-corpus) while a link to corpus 2 is offered.

`src/web/ui.rs`: the corpus page of `c2` lists the merge under a "Written from this corpus" heading.

- [ ] **Step 2: Run to verify failure**

- [ ] **Step 3: Implement**

`lineage.rs`:

```rust
/// Where an artifact comes from, as corpus and lines. Derived from lineage,
/// never stored: membership *is* lineage projected, so it cannot drift.
#[derive(Debug, Clone, PartialEq)]
pub struct Origin {
    pub corpus_id: String,
    pub span: Option<CorpusSpan>,
}

impl Store {
    /// Mirror of `roots_of`, one step further: every root's corpus and span.
    /// A passage or captured artifact is its own origin; a merged or
    /// synthesized one has one origin per root. Non-empty for every active
    /// artifact — an empty answer is the same broken state `roots_of` reports.
    pub async fn origins_of(&self, ids: &[String]) -> Result<BTreeMap<String, Vec<Origin>>> {
        let roots = self.roots_of(ids).await?;
        let mut all: Vec<String> = roots.values().flatten().cloned().collect();
        all.sort();
        all.dedup();
        let rows = self.artifacts_by_ids(&all).await?;
        let by_id: std::collections::HashMap<&str, &Chunk> = rows.iter().map(|c| (c.id.as_str(), c)).collect();
        let mut out = BTreeMap::new();
        for (id, rs) in roots {
            let mut o: Vec<Origin> = rs
                .iter()
                .filter_map(|r| by_id.get(r.as_str()))
                .filter_map(|c| Some(Origin { corpus_id: c.corpus_id.clone()?, span: c.corpus_span.clone() }))
                .collect();
            o.sort_by(|a, b| (a.corpus_id.as_str(), a.span.as_ref().map(|s| s.start_line)).cmp(&(b.corpus_id.as_str(), b.span.as_ref().map(|s| s.start_line))));
            out.insert(id, o);
        }
        Ok(out)
    }

    /// Model-written artifacts with a root in this corpus: what the corpus
    /// page lists as written from it.
    pub async fn artifacts_originating_in(&self, corpus_id: &str) -> Result<Vec<Chunk>> {
        let rows = sqlx::query(
            "SELECT DISTINCT m.* FROM artifacts m
               JOIN artifact_sources s ON s.child_id = m.id
               JOIN artifacts r ON r.id = s.root_id
              WHERE r.corpus_id = ? AND m.provenance IN ('merged', 'synthesized')
              ORDER BY m.created_at",
        )
        .bind(corpus_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(crate::store::artifacts::row_to_artifact).collect())
    }
}
```

`vector/mod.rs`: add to `VectorPayload`

```rust
    /// Every corpus this artifact draws from — one for a passage or captured
    /// row, several for a merge. A projection of `artifact_sources`, the way
    /// `status` is a projection of the row; SQLite stays the authority.
    /// `cap_per_corpus` groups on it, so a merge counts against each of its
    /// corpora instead of all merges landing under one empty key.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub origin_corpora: Vec<String>,
```

and `origin_corpora: vec![]` in every literal (`grep -rn "VectorPayload {" src tests`). `embed.rs` `payload_of` stays sync and sets `origin_corpora: vec![]`; `embed_batch` (and the two single-point paths) fill it afterwards: `let origins = core.store.origins_of(&ids).await?;` then for each point `payload.origin_corpora = origins[id].iter().map(|o| o.corpus_id.clone()).collect::<BTreeSet<_>>().into_iter().collect()`. Passages/captured get their own corpus; that is fine and makes the field uniform.

`search.rs` `cap_per_corpus`: the key is `origin_corpora` when non-empty, else `corpus_id`; a hit with several origins increments each and is kept only if every one is under the cap:

```rust
        let keys: Vec<String> = if h.payload.origin_corpora.is_empty() { vec![h.payload.corpus_id.clone()] } else { h.payload.origin_corpora.clone() };
        let over = keys.iter().any(|k| seen.get(k).copied().unwrap_or(0) >= max);
        for k in &keys { *seen.entry(k.clone()).or_insert(0) += 1; }
        if !over { kept.push(h) } else { displaced.push(h) }
```

`links.rs` `links_from`: compute `cross_corpus` in Rust over `origins_of` for both endpoints — "intersection empty" — instead of `a.corpus_id`/`b.corpus_id` from SQL (keep the SQL columns, use them only when both rows have a `corpus_id` and are source text; otherwise resolve through origins — one batched `origins_of` over all endpoints after the loop). `links_to_judge`: drop the `a.corpus_id <> b.corpus_id` clause from the SQL, fetch 4× limit as before, and filter in Rust with the same intersection test; its doc comment changes accordingly.

UI: `CorpusTemplate.written_from: Vec<ArtifactView>` from `artifacts_originating_in`, rendered after `unplaced` in `corpus.html` under `<h3>Written from this corpus</h3>` with `{% for c in written_from %}{% include "_artifact.html" %}{% endfor %}`.

- [ ] **Step 4: Run and commit**

```bash
cargo test 2>&1 | grep -E "^test result|FAILED|panicked" | head -3
cargo fmt && cargo clippy --all-targets 2>&1 | grep -cE "^(warning|error)"
git add -A src
git commit -m "feat: an artifact belongs to every corpus it drew from — derived, never stored"
```

---

### Task 9: Docs and the gate

- [ ] `config.example.toml`: a `[promote]` block after `[activation]` (find it) with the two keys and their comments from the spec; in `[feedback]` the comment says "On by default: promotion reads activation…" and shows `enabled = true`. `README.md` config table: `promote.*` row; `feedback.*` row says "On by default".
- [ ] Gate: `cargo fmt --check`, clippy 0, `cargo test`, `cargo test --test eval`.
- [ ] Commit `docs: promotion, and recording on by default` and `git push`.

---

## Self-review

**Spec coverage:** §3 trigger (threshold, `>=`, engagement sites, checked at the bump) → Task 3; "activation must actually move" → Task 1; mechanics 1–4 (`verbatim→pending`, keep, arm, supersede under lock, settle→finish) → Tasks 3–4 (finish runs because every other segment is `verbatim` — plan 2); idempotency → Task 4; majority rule per artifact → Task 4 `covered_by`; eager re-synthesis → Task 6; carrying access (max activation, links copied, collisions, what does not carry) → Tasks 2, 4 — `hit_count`/`last_seen_at`/judged pairs are simply not touched; undo → Task 5; tests list in §3 → spread across Tasks 3–5 (each named bullet has a test). §6: exclusion by segment → Task 7; `auto_supersede` fast lane → Task 7; merge guards untouched → nothing to do; "dormant until use" → plan 2 already keeps passages out of relate. §7: `origins_of`, invariant, five call sites, payload → Task 8; corpus detail page, bands ("parent spans stay claimed" already holds because superseded rows keep their spans on the page — verified in plan 2 reading), `cross_corpus`, `links_to_judge`, `cap_per_corpus` → Task 8.

**Gaps recorded:** the server-side grouping (`query/groups`) from §5 remains unbuilt; `cap_per_corpus` over origins is the in-memory fallback the spec names. The "before and after shown on Ops" for eager re-synthesis (§3 table) is not built — the log line is; Ops rendering can follow when the detector is turned on.

**Placeholder scan:** Task 2's `artifact_confirmed` test and Task 8's merge-insert test name helpers the executor must find (`seed_search_event` to be written from the feedback store's API; the merge-insert helper in `lineage.rs` tests) — both have a pointer to the real API. Task 5's UI test is described rather than written out; it follows the `form`/`get_body` pattern of the other UI tests in the file. Acceptable for an inline executor with the file open; a subagent should be handed those two helpers explicitly.

**Type consistency:** `maybe_promote(core, &[String], at) -> Result<usize>` used identically in Tasks 3 (search.rs, associate.rs). `supersede_covered(core, corpus_id, idx, &[Chunk], at)` defined and called with the same shape in Task 4. `carry_link(&Link, old, new, half_life, at)` (Task 2) is what Task 4 calls. `Origin { corpus_id, span }` (Task 8) is used in its own tests only.
