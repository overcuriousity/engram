# Autonomous Curation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove every place engram waits on its operator — the decide queue, parked near-duplicates, the unverified list, "Mark reviewed", the gap list, the autonomy opt-in — so a person only judges answers, edits directly, and may undo.

**Architecture:** Each job that today hands a pair or capture to "a person" gets an automatic rule where it decides (`jobs/dedupe.rs`, `jobs/merge.rs`, `jobs/reconcile.rs`, the verdict handlers). Contradictions stay `PairState::Contradiction` and are surfaced at read time on `SearchResult` and `AskResponse`. Then the UI, API routes and Android screens that offered the decisions are removed, and Insights becomes a read-only journal with undo.

**Tech Stack:** Rust (axum, askama, sqlx/SQLite, tokio), htmx templates, Kotlin/Compose Android app (`android/`).

**Spec:** `docs/superpowers/specs/2026-10-07-autonomous-curation-design.md`

## Global Constraints

- Build and test with `CARGO_INCREMENTAL=0` and cargo from `~/.cargo/bin` — the disk is 33 GB and fills. Run `cargo test <filter>` for the task's own tests, `cargo test` once at the end of each task.
- Nothing is deleted by the base. Every automatic corpus action is a deprecation, supersession or merge, journaled through `Store::record_action`, with its undo intact.
- No merge that drops a number, command or path is ever written (`jobs::merge::losses` stays on every merge path).
- The base never picks a side of a contradiction. A verdict on an answer never closes a `Contradiction` pair.
- `evolve.autonomous` behaves as `"full"` always; `max_actions_per_day` per job, default `20`, `0` means the job takes no corpus actions.
- Comment voice: match the surrounding code — full sentences, explaining *why*, no "TODO".
- Commit messages follow the repo style: `type(scope): sentence in plain words`, ending with
  ```
  Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01VzuDpkmF8VFq7oUXhVX2Wv
  ```

## Review Focus

1. **A side deleted between search and render** — a disagreement must never name an artifact that is out of results; `open_contradictions` requires both sides in results (test in Task 4).
2. **One artifact disagreeing with several others** — every partner is listed on the hit, and Ask pulls in at most `retrieve::NEIGHBOUR_MAX` partners without displacing ranked hits (test in Task 5).
3. **A positive verdict on an artifact no longer in results, or a passage** — the verdict is still recorded; the verification stamp is skipped, never an error to the person (test in Task 6).
4. **`max_actions_per_day = 0`** — the corpus jobs act on nothing, as `max_actions_per_week = 0` did; old configs with `autonomous = "off"` still load (test in Task 1).
5. **A capture still being read, or a restored placeholder, with low coverage** — no automatic reread; reread happens once per capture and never again (test in Task 7).

---

### Task 1: Always full, paced per day

**Files:**
- Modify: `src/config.rs` (`EvolveConfig` ~580-636, `Autonomy` default, `normalize` ~2367, tests ~2920-2980, 3507)
- Modify: `src/core/mod.rs:312-351` (`Budget`, `budget`, `may_act`)
- Modify: every test that sets `core.evolve.max_actions_per_week` (`src/jobs/{consolidate,reap,promote,condense,dedupe,sleep}.rs`) — rename to `max_actions_per_day`
- Modify: `config.example.toml` (`[evolve]` `autonomous` and `max_actions_per_week` blocks ~1020-1060)

**Interfaces:**
- Produces: `EvolveConfig::max_actions_per_day: u32` (default 20); `EvolveConfig::autonomous` always `Autonomy::Full` after `Config::normalize`; `Core::budget(job)` counts the last 24 h.

- [ ] **Step 1: Write the failing tests** in `src/config.rs` tests module

```rust
#[test]
fn autonomy_is_full_whatever_the_file_says() {
    for body in [r#"autonomous = "off""#, r#"autonomous = "ranking""#, "autonomous = false", ""] {
        let mut cfg: Config = toml::from_str(&format!("[evolve]\n{body}\n")).unwrap();
        cfg.normalize();
        assert_eq!(cfg.evolve.autonomous, Autonomy::Full, "{body}");
    }
}

#[test]
fn the_pace_is_per_day_and_the_week_key_is_ignored() {
    let mut cfg: Config = toml::from_str("[evolve]\nmax_actions_per_week = 3\n").unwrap();
    cfg.normalize();
    assert_eq!(cfg.evolve.max_actions_per_day, 20);
    let cfg: Config = toml::from_str("[evolve]\nmax_actions_per_day = 0\n").unwrap();
    assert_eq!(cfg.evolve.max_actions_per_day, 0);
}
```

(If `Config` needs required fields to parse, use the same minimal-body helper `loads_minimal_config` uses at `src/config.rs:3569`.)

- [ ] **Step 2: Run** `CARGO_INCREMENTAL=0 cargo test --lib config::tests::autonomy_is_full config::tests::the_pace_is_per_day` — expect FAIL (field missing / default Ranking).

- [ ] **Step 3: Implement**

In `EvolveConfig`: replace `max_actions_per_week: u32` with

```rust
    /// Corpus actions each job may take on its own in any twenty-four hours:
    /// merges, supersessions, discards, burials and condensations. A pace, not
    /// a cap: a backlog drains a day at a time rather than stopping at a weekly
    /// ceiling with nobody left to clear what is behind it. Undos are never
    /// counted. `0` stops the corpus jobs acting at all.
    pub max_actions_per_day: u32,
    /// Read only to say it is no longer read.
    #[serde(default)]
    max_actions_per_week: Option<u32>,
```

Default: `autonomous: Autonomy::Full, max_actions_per_day: 20, max_actions_per_week: None`. Rewrite the `autonomous` doc comment to say the base always runs as `"full"` and the key is read only to warn.

In `normalize`:

```rust
        if self.evolve.autonomous != Autonomy::Full {
            tracing::warn!(
                configured = self.evolve.autonomous.as_str(),
                "evolve.autonomous is no longer read: the base curates itself, and every \
                 action it takes has an undo on Insights"
            );
            self.evolve.autonomous = Autonomy::Full;
        }
        if self.evolve.max_actions_per_week.take().is_some() {
            tracing::warn!("evolve.max_actions_per_week is no longer read; see max_actions_per_day");
        }
```

(Use the existing `Autonomy` → str method at `src/config.rs:526`; name it as it is there.)

In `src/core/mod.rs` `budget`: window `crate::store::now() - 86_400`, `cap: self.evolve.max_actions_per_day`. Update the doc comments on `budget`/`may_act` ("its own day"). `may_act` keeps the `acts_on_corpus` guard — it is now always true.

Rename every `max_actions_per_week = 0` in tests to `max_actions_per_day = 0`. Update tests asserting the old default (`src/config.rs:2977`, `:3507`) to the new values.

In `config.example.toml`: replace the `autonomous` explanation (lines ~1015-1049) with a short paragraph that the base always curates itself, and replace `max_actions_per_week = 10` with:

```toml
# Corpus actions each job may take on its own in any 24 hours — merges,
# replacements, discards, burials and condensations. A pace: a backlog drains
# a day at a time. Undos are never counted. 0 stops the corpus jobs acting.
max_actions_per_day = 20
```

Also rewrite the `[consolidate]` comments that say "until you decide on Ops", "review queue", "goes to the review queue on Capture": a value conflict is kept as a disagreement both search and Ask show; nothing waits on a person.

- [ ] **Step 4: Run** `CARGO_INCREMENTAL=0 cargo test --lib` — expect PASS.

- [ ] **Step 5: Commit** `feat(evolve): the base always curates itself, a day's actions at a time`

---

### Task 2: The dedupe judge settles every pair itself

**Files:**
- Modify: `src/jobs/dedupe.rs` (`apply` ~733-866, `synthesize_asked_pair` ~534-655, `run` refusals ~180-260, `discard_both` ~895-975, `TAKEN_BACK` ~75, tests)
- Modify: `src/store/pairs.rs` (`PairState` doc comments for `Duplicate`, `Contradiction`, `Unmergeable`, `AWAITING_REVIEW`)

**Interfaces:**
- Produces: `async fn write_merge(core: &Core, p: &ArtifactPair, sources: &[String], draft: &MergedDraft, by: DecidedBy) -> Result<()>` (private to `dedupe.rs`); `async fn leave_both(core: &Core, p: &ArtifactPair, why: &str) -> Result<()>` settling `PairState::NoConflict`.
- After this task the only states `dedupe::run` settles into are `NoConflict`, `Contradiction` (a real value disagreement from the judge), `Dismissed`, `Stale`, and merged.

- [ ] **Step 1: Rewrite the failing test** — replace `a_duplicate_verdict_is_proposed_and_nothing_is_written` (`src/jobs/dedupe.rs:2520`) with:

```rust
    /// A duplicate verdict writes the merge where it is found. The verdict is
    /// not steady on every pair — see `PairState::Duplicate` — and what makes
    /// acting on it safe is the loss check, the journal and the undo, which
    /// `jobs::retract` reads against later searches.
    #[tokio::test]
    async fn a_duplicate_verdict_writes_the_merge_and_journals_it() {
        use crate::store::actions::Kind;
        let mut core = test_core().await;
        core.judge = Some(Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"duplicate","detail":"same thing",
                "merged":{"title":"Pool","text":"the pool holds sixteen connections","tags":[],"caveats":[]}}"#
                .into(),
        ])));
        let ids = seed_titled(
            &core,
            &[
                ("Pool sizing", "sixteen connections", [1.0, 0.0]),
                ("Connections", "sixteen connections", [0.93, 0.37]),
            ],
        )
        .await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &pair.to_string()).await.unwrap();

        let p = core.store.get_pair(pair).await.unwrap();
        assert!(p.merged_into.is_some(), "the merge was written");
        assert_eq!(p.decided_by, Some(DecidedBy::Model));
        let journal = core.store.open_actions(&[Kind::Merge], 10).await.unwrap();
        assert_eq!(journal.len(), 2, "one row per source");
        assert!(journal.iter().all(|a| a.evidence.get("asked_by").is_none()));
    }

    #[tokio::test]
    async fn a_verdict_repeated_over_an_undo_leaves_both_and_asks_nobody() {
        // Seed a pair, journal a supersede on ids[0] and undo it, then script a
        // `replaced` verdict naming ids[0] (the older): see the existing test
        // that exercises `TAKEN_BACK` for the seeding — reuse its setup.
        // Assert: state NoConflict, both in results, no new Supersede action.
    }

    #[tokio::test]
    async fn a_merge_that_would_lose_a_value_is_a_disagreement_not_a_card() {
        // Script a `duplicate` verdict whose draft drops "rw" from
        // "mount -o rw" vs "mount -o ro" (see the existing loss-check test
        // for the texts). Assert: state Contradiction, detail says a value is
        // stated differently, nothing merged. This stays a Contradiction on
        // purpose: two texts differing in a value is exactly what search and
        // Ask now show.
    }
```

Fill the two skeleton bodies by copying the setup of the existing tests that exercise `TAKEN_BACK` and the merge loss check in the same module (grep `TAKEN_BACK` and `refused a merge` in the tests) — change only the asserts as described.

Also update every other test in this module that asserts `PairState::Duplicate`, `PairState::Unmergeable`, `"resolve by hand"` or `Contradiction` with `TAKEN_BACK`, to the new outcome per the table in Step 3. Do not delete a test; change its asserts and rename it.

- [ ] **Step 2: Run** `CARGO_INCREMENTAL=0 cargo test --lib jobs::dedupe` — expect the new/changed tests to FAIL.

- [ ] **Step 3: Implement**

Add near `settle`:

```rust
/// Leave both sides as they are and close the question. What every refusal
/// on this path used to hand a person: the base cannot act on this pair, and
/// a person holding no more evidence than the judge is not who it asks now.
/// The reason is kept on the row, which is what the journal reads back.
async fn leave_both(core: &Core, p: &ArtifactPair, why: &str) -> Result<()> {
    if p.synthesis_asked {
        core.store.clear_pair_synthesis(p.id).await?;
    }
    settle(core, p, PairState::NoConflict, Some(why)).await
}
```

Extract the `match crate::jobs::merge::write(...)` tail of `synthesize_asked_pair` into:

```rust
async fn write_merge(
    core: &Core,
    p: &ArtifactPair,
    sources: &[String],
    draft: &MergedDraft,
    by: DecidedBy,
) -> Result<()> {
    use crate::store::actions::Kind;
    match crate::jobs::merge::write(core, draft, sources).await {
        Ok(m) => {
            let act = format!("merged into {} from {} sources", m.id, sources.len());
            for source in sources {
                let mut row = action(Kind::Merge, p, source, Some(&m.id), Some(act.as_str()));
                if by == DecidedBy::Operator
                    && let Some(evidence) = row.evidence.as_object_mut()
                {
                    evidence.insert("asked_by".into(), serde_json::json!("operator"));
                }
                core.store.record_action(&row).await?;
            }
            let why = match by {
                DecidedBy::Operator => "synthesized at an operator's request",
                _ => "the judge read both as one, and nothing either states was lost",
            };
            core.store.set_pair_merged(p.id, &m.id, Some(why), by).await
        }
        Err(Error::Validation(why)) => {
            tracing::warn!(pair = p.id, reason = %why, "the merge path refused a merge");
            leave_both(core, p, "These could not be written as one because of what one of them is made of; both stay as they are.").await
        }
        Err(e) => Err(e),
    }
}
```

(Check `merge::write`'s signature — it may take `&MergedDraft` or `MergedDraft` by value; match it. Keep `DecidedBy` variant names as they are in `store/pairs.rs`.)

`synthesize_asked_pair` ends with `write_merge(core, p, &sources, &draft, DecidedBy::Operator).await`. It stays for synthesis asks already set on old rows (Task 3 arms them); its refusals become `leave_both`.

Then change, in `dedupe.rs`:

| Site | Was | Becomes |
|---|---|---|
| `apply`, `Relation::Duplicate` | `settle(... Duplicate ...)` | if `let Some(d) = &s.merged` → `write_merge(core, &s.pair, &ids_of(&s.members), d, DecidedBy::Model)` else `core.store.ask_pair_synthesis(id)` after settling `Duplicate` (the sweep arms it) — before acting, `taken_back_before(core, Kind::Merge, &[a, b])` → `leave_both(.., TAKEN_BACK)` |
| `apply`, `Relation::Replaced`, taken back | `settle(Contradiction, TAKEN_BACK)` | `leave_both(core, &s.pair, TAKEN_BACK)` |
| `discard_both`, taken back (model only) | `settle_as(Contradiction, TAKEN_BACK)` | `leave_both` when `by == DecidedBy::Model` |
| `discard_both`, winner others hide behind | `Contradiction` "Resolve by hand" | `leave_both` with the same first sentence, "both stay as they are" |
| `run`, merged member lost its sources (~190) | `Contradiction` | `leave_both(.., "a merged member has lost its sources; both stay as they are")` |
| `run`, lineage refusal (~245) | `Unmergeable` | `leave_both` with a one-sentence reason |
| `run`/`synthesize_asked_pair`, "do not fit one call" (~344, ~566) | `Contradiction` | `leave_both(.., "these two do not fit one call; both stay as they are")` |
| `synthesize_asked_pair` loss refusal (~628) | `Unmergeable` | `leave_both(.., "every draft dropped a value one of them states; both stay as they are")` |

`TAKEN_BACK` becomes `"This was done to one of these before and taken back, so both stay as they are."`

`interpret`'s two downgrades to `Relation::Conflict` stay: a direction the newest-wins bias rejects and a merge that would drop a value are both two texts stating something differently, which is what a disagreement is. Rewrite their comments: no "hands it to a person"; say search and Ask show both.

Rewrite the module header (`dedupe.rs:30-35`) last paragraph: four verdicts act; a value conflict is kept as a disagreement both sides of which search and Ask show, and nothing waits on a person.

In `store/pairs.rs`, rewrite the doc comments of `Duplicate`, `Unmergeable`, `Superseded` and `AWAITING_REVIEW` to say nothing new is settled into them except `Duplicate` as the transient state before an armed synthesis, and that Task 3's drain clears what old bases hold.

- [ ] **Step 4: Run** `CARGO_INCREMENTAL=0 cargo test --lib jobs::dedupe jobs::consolidate store::pairs` — expect PASS. Then `cargo test --lib`.

- [ ] **Step 5: Commit** `feat(dedupe): the judge settles every pair, and nothing waits on a person`

---

### Task 3: Drain what old bases left waiting

**Files:**
- Modify: `src/jobs/consolidate.rs:475` (`arm_dedupe`)
- Modify: `src/store/pairs.rs` (new `drain_waiting`)
- Modify: `src/jobs/merge.rs:362` (`flag_orphans`) and a store helper for already-flagged rows
- Test: `src/jobs/consolidate.rs` tests module

**Interfaces:**
- Consumes: Task 2's `leave_both` semantics (`NoConflict`), `Store::ask_pair_synthesis`, `Store::accept_source_loss`, `Store::clear_artifact_flags`.
- Produces: `Store::drain_waiting_pairs(&self) -> Result<DrainCounts>` where `pub struct DrainCounts { pub closed: u64, pub to_merge: u64, pub to_replace: u64 }`; `jobs::merge::flag_orphans` renamed in meaning — it now accepts instead of flagging (keeps its name and return count).

- [ ] **Step 1: Write the failing test** in `src/jobs/consolidate.rs` tests:

```rust
    #[tokio::test]
    async fn an_old_base_has_nothing_waiting_after_two_sweeps() {
        use crate::store::pairs::{DecidedBy, PairState};
        let core = test_core().await;
        let ids = seed_titled(&core, &[
            ("A", "alpha", [1.0, 0.0]), ("B", "beta", [0.99, 0.1]),
            ("C", "gamma", [0.0, 1.0]), ("D", "delta", [0.1, 0.99]),
            ("E", "eps", [0.7, 0.7]),  ("F", "zeta", [0.71, 0.69]),
        ]).await;
        let mut pairs = Vec::new();
        for (a, b, state, detail) in [
            (0, 1, PairState::Duplicate, Some("same thing")),
            (2, 3, PairState::Unmergeable, Some("x")),
            (4, 5, PairState::Contradiction, Some(crate::jobs::dedupe::TAKEN_BACK_OLD)),
        ] {
            core.store.record_pair(&ids[a], &ids[b], 0.91).await.unwrap();
            let id = core.store.pairs_by_state(PairState::Pending, 10).await.unwrap()
                .into_iter().find(|p| p.a_id == ids[a] || p.b_id == ids[a]).unwrap().id;
            core.store.set_pair_state(id, state, detail, DecidedBy::Model).await.unwrap();
            pairs.push(id);
        }

        let first = core.store.drain_waiting_pairs().await.unwrap();
        let second = core.store.drain_waiting_pairs().await.unwrap();

        assert_eq!(first.to_merge, 1);
        assert_eq!(first.closed, 2);
        assert_eq!((second.closed, second.to_merge, second.to_replace), (0, 0, 0), "idempotent");
        assert!(core.store.get_pair(pairs[0]).await.unwrap().synthesis_asked);
        for id in &pairs[1..] {
            assert_eq!(core.store.get_pair(*id).await.unwrap().state, PairState::NoConflict);
        }
    }

    #[tokio::test]
    async fn a_real_disagreement_is_left_for_search_to_show() {
        // One Contradiction pair with an ordinary judge detail. After the
        // drain it is still Contradiction.
    }

    #[tokio::test]
    async fn a_merge_that_lost_a_source_is_accepted_without_a_flag() {
        // Use the setup of the existing `flag_orphans` test in jobs/merge.rs.
        // After `flag_orphans`, the artifact carries no `orphaned_source`
        // flag and `merged_missing_a_source` no longer returns it.
    }
```

Export the old wording as `pub(crate) const TAKEN_BACK_OLD: &str = "This was done to one of these before and taken back. Resolve by hand.";` in `dedupe.rs` (the text rows on old bases carry) — used only by the drain and its test.

- [ ] **Step 2: Run** `CARGO_INCREMENTAL=0 cargo test --lib jobs::consolidate::tests::an_old_base jobs::merge` — expect FAIL.

- [ ] **Step 3: Implement**

`Store::drain_waiting_pairs` in `store/pairs.rs`, one transaction:

```rust
    /// What a base from before autonomous curation left waiting on a person,
    /// moved into states the base answers itself. Run at the head of every
    /// dedupe sweep; a second run finds nothing, which is what makes it safe
    /// to leave there for good.
    ///
    /// - `Duplicate` with no merge: the synthesis is asked for, so the sweep
    ///   arms it like any other.
    /// - `Superseded`: put back to `Pending`, so the judge reads it again and
    ///   applies a replacement through the one path that checks newest-wins,
    ///   liveness and taken-back.
    /// - `Unmergeable`, `Oversized`, and `Contradiction` carrying a refusal
    ///   rather than a finding: `NoConflict`, both left as they are.
    pub async fn drain_waiting_pairs(&self) -> Result<DrainCounts> {
        let mut tx = self.pool.begin().await?;
        let to_merge = sqlx::query(
            "UPDATE artifact_pairs SET synthesis_asked = 1
              WHERE state = 'duplicate' AND merged_into IS NULL AND synthesis_asked = 0",
        ).execute(&mut *tx).await?.rows_affected();
        let to_replace = sqlx::query(
            "UPDATE artifact_pairs SET state = 'pending', decided_by = NULL WHERE state = 'superseded'",
        ).execute(&mut *tx).await?.rows_affected();
        let closed = sqlx::query(
            "UPDATE artifact_pairs SET state = 'no_conflict', synthesis_asked = 0
              WHERE state IN ('unmergeable', 'oversized')
                 OR (state = 'contradiction' AND (detail LIKE '%Resolve by hand%'
                                               OR detail LIKE '%resolve by hand%'))",
        ).execute(&mut *tx).await?.rows_affected();
        tx.commit().await?;
        Ok(DrainCounts { closed, to_merge, to_replace })
    }
```

Check the exact strings `PairState::as_str` writes for each variant (`store/pairs.rs`) and use those — do not trust the literals above. If `Oversized` does not exist as a stored state, drop it.

In `arm_dedupe`, first thing after the `max_dedupe_per_tick == 0` early return:

```rust
    // Free, and before the budget check: the drain writes no corpus action.
    let drained = core.store.drain_waiting_pairs().await?;
    if drained.closed + drained.to_merge + drained.to_replace > 0 {
        tracing::info!(?drained, "moved what was waiting on a person into the sweep");
    }
```

(`#[derive(Debug, Default, Clone, Copy)]` on `DrainCounts`.)

`flag_orphans` in `jobs/merge.rs`: replace `set_artifact_flags` with `core.store.accept_source_loss(&id).await?`, and before the loop accept every already-flagged row: add `Store::artifacts_flagged(flag: &str, limit) -> Result<Vec<String>>` (SELECT id FROM artifacts WHERE flags LIKE ... — match how `flags` is stored; read `set_artifact_flags`) and for each: `accept_source_loss` + `clear_artifact_flags`. Rewrite the doc comment: the merge is accepted as a merge of what remains; the detail pane still lists the sources it has. Update the existing `flag_orphans` tests to assert acceptance.

- [ ] **Step 4: Run** `CARGO_INCREMENTAL=0 cargo test --lib` — expect PASS.

- [ ] **Step 5: Commit** `feat(consolidate): what an old base left waiting is answered by the sweep`

---

### Task 4: Disagreements on search results

**Files:**
- Modify: `src/store/pairs.rs` (new `Disagreement`, `open_contradictions`)
- Modify: `src/core/search.rs` (`SearchResult` ~149-260, every `SearchResult { .. }` literal, `fill_titles` call sites 1684, 1920, 2238; new `finish_hits`)
- Modify: `src/core/ask/mod.rs:594` (the round's `fill_titles` call) and the neighbour literal ~918
- Modify: `src/web/templates/_results.html` (one line under the snippet)

**Interfaces:**
- Produces:
```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Disagreement {
    /// The artifact this row is attached to.
    pub artifact_id: String,
    /// The artifact it disagrees with.
    pub other_id: String,
    pub other_title: Option<String>,
    pub other_created_at: i64,
    /// The judge's sentence on what differs.
    pub detail: Option<String>,
}
impl Store { pub async fn open_contradictions(&self, ids: &[String]) -> Result<Vec<Disagreement>>; }
// SearchResult:
#[serde(default, skip_serializing_if = "Vec::is_empty")]
pub disagrees_with: Vec<Disagreement>,
impl Core { pub(crate) async fn finish_hits(&self, results: &mut [SearchResult]); }
```

- [ ] **Step 1: Write the failing tests**

In `store/pairs.rs` tests:

```rust
    #[tokio::test]
    async fn a_contradiction_is_read_from_both_sides_while_both_are_live() {
        let s = test_store().await;           // use the module's existing store helper
        let (a, b) = two_artifacts(&s).await; // likewise; titles "A", "B"
        s.record_pair(&a, &b, 0.9).await.unwrap();
        let id = s.pairs_by_state(PairState::Pending, 1).await.unwrap()[0].id;
        s.set_pair_state(id, PairState::Contradiction, Some("30 days there, 14 here"), DecidedBy::Model)
            .await.unwrap();

        let got = s.open_contradictions(&[a.clone(), b.clone()]).await.unwrap();
        assert_eq!(got.len(), 2, "one row per side asked about");
        let from_a = got.iter().find(|d| d.artifact_id == a).unwrap();
        assert_eq!(from_a.other_id, b);
        assert_eq!(from_a.detail.as_deref(), Some("30 days there, 14 here"));

        s.deprecate_artifact(&b).await.unwrap(); // whatever the store calls it
        assert!(s.open_contradictions(&[a]).await.unwrap().is_empty(),
            "a side out of results is not something to disagree with");
    }

    #[tokio::test]
    async fn an_artifact_disagreeing_with_two_lists_both() { /* a~b, a~c → 2 rows for [a] */ }
```

In `core/search.rs` tests: seed two artifacts with vectors (`jobs::consolidate::tests::seed_titled`), mark a `Contradiction` pair, run `core.search(...)` for a query matching both, assert each hit's `disagrees_with` names the other; and that hit order equals the order with no pair (rank unchanged).

- [ ] **Step 2: Run** `CARGO_INCREMENTAL=0 cargo test --lib store::pairs core::search` — expect FAIL.

- [ ] **Step 3: Implement**

`open_contradictions`: one query over `artifact_pairs p JOIN artifacts a ON a.id = p.a_id JOIN artifacts b ON b.id = p.b_id WHERE p.state = 'contradiction' AND (p.a_id IN (...) OR p.b_id IN (...))`, filtering both sides "in results" the same way `artifact_in_results` does (read it and reuse its SQL condition). Emit a row for each side whose id is in `ids`. Build the `IN` list with `?` holes as `ask_pair_synthesis` does.

`Core::finish_hits` in `core/search.rs`, beside `fill_titles`:

```rust
    /// Everything a hit carries that the ranking did not put there: a name
    /// borrowed from its note, and what it is known to disagree with. Once,
    /// on the way out, so every door inherits both. Best-effort like
    /// `fill_titles`: a failed read costs the lines, never the results.
    pub(crate) async fn finish_hits(&self, results: &mut [SearchResult]) {
        self.fill_titles(results).await;
        let ids: Vec<String> = results.iter().map(|r| r.artifact_id.clone()).collect();
        match self.store.open_contradictions(&ids).await {
            Ok(rows) => {
                for r in results.iter_mut() {
                    r.disagrees_with = rows.iter().filter(|d| d.artifact_id == r.artifact_id).cloned().collect();
                }
            }
            Err(e) => tracing::warn!(error = %e, "could not read disagreements; hits carry none"),
        }
    }
```

Replace the three `self.fill_titles(&mut ...)` calls in `search.rs` and the one in `ask/mod.rs:594` with `finish_hits`. Add `disagrees_with: Vec::new()` to every `SearchResult { .. }` literal (`cargo build` lists them).

In `_results.html`, under the snippet of each hit (find where `r.text`/snippet renders):

```html
{% for d in r.disagrees_with %}
<div class="muted row-sub disagrees">
  Disagrees with <a href="/ui/artifacts/{{ d.other_id }}">{{ d.other_title.as_deref().unwrap_or("another note") }}</a>
  ({{ d.other_created_at|day }}){% if let Some(x) = d.detail %}: {{ x }}{% endif %}
</div>
{% endfor %}
```

Use whatever date filter the templates already use for a short day (grep `created` in `_queue.html`/`ui.rs`); if dates are preformatted in Rust, add `other_day: String` to the template's row type instead of a filter.

- [ ] **Step 4: Run** `CARGO_INCREMENTAL=0 cargo test --lib` — expect PASS. Update `src/web/android_fixtures.rs` expectations if the search fixture changes (it should not: the field is skipped when empty).

- [ ] **Step 5: Commit** `feat(search): a result says which note disagrees with it`

---

### Task 5: Ask shows both readings

**Files:**
- Modify: `src/core/ask/mod.rs` (`AskResponse` ~49-90 and its literals ~165, ~207, ~440; `reach_sideways` ~788; `excerpts` ~617)
- Modify: `src/infer/prompt.rs:1541` (`ASK_SYSTEM`)
- Modify: `src/web/templates/_answer.html` (badge, beside the `unsupported` badge)

**Interfaces:**
- Consumes: `SearchResult::disagrees_with`, `Disagreement` (Task 4).
- Produces: `AskResponse::disagreements: Vec<Disagreement>` (`#[serde(default, skip_serializing_if = "Vec::is_empty")]`), pairs whose both sides are among the citations, one row per pair (the side cited first).

- [ ] **Step 1: Write the failing tests** in `core/ask/mod.rs` tests (use the module's existing ask harness — grep `an_answer_that_invents_a_command_reports_it_as_unsupported` ~2460 and copy its setup):

```rust
    #[tokio::test]
    async fn a_disagreeing_note_is_brought_in_and_marked() {
        // Seed A ("backup retention is 30 days", vector near the query) and
        // B ("backup retention is 14 days", vector far from it, so search
        // alone would not return B). Contradiction pair A~B, detail
        // "30 days there, 14 here". Script the asker to answer anything.
        // Ask "how long is backup retention".
        // Assert: B is among `citations`; the prompt the scripted completer
        // received contains "states this differently" and B's title;
        // `disagreements.len() == 1`.
    }

    #[tokio::test]
    async fn partners_never_displace_a_ranked_hit() {
        // Seed 3 ranked hits each disagreeing with a distinct far note.
        // Assert: the first `ranked` citations are the three ranked hits in
        // their order; partners follow; at most NEIGHBOUR_MAX were added.
    }
```

Read how the existing tests capture the prompt the `ScriptedCompleter` saw (it records calls, or the test uses a capturing completer) and assert on that.

- [ ] **Step 2: Run** `CARGO_INCREMENTAL=0 cargo test --lib core::ask` — expect FAIL.

- [ ] **Step 3: Implement**

In `reach_sideways`, before the adjacency/links loop, put disagreement partners at the front of `reached` so they take the reach's places first:

```rust
        // What a hit disagrees with goes first: an answer drawn from one side
        // of a disagreement, with the other side left out, is the one wrong
        // answer the base already knows how to avoid.
        for h in hits.iter().take(anchors) {
            for d in &h.disagrees_with {
                reached.push((d.other_id.clone(), h.artifact_id.clone(), Some("states this differently".into())));
            }
        }
```

Note `reach_sideways` runs before `finish_hits` in `retrieve_round` (~591-594). Move the `finish_hits` call to just before `reach_sideways`, and call `self.finish_hits(&mut hits[ranked..]).await` after it for the appended ones.

In `excerpts`, add a disagreement caveat per hit (passages are never one side of a pair, so the stitching path that re-reads caveats from rows is unaffected):

```rust
                let mut lines: Vec<String> = caveats.get(&h.artifact_id).cloned().unwrap_or_default();
                for d in &h.disagrees_with {
                    lines.push(format!(
                        "another note, \"{}\" ({}), states this differently{}",
                        d.other_title.as_deref().unwrap_or("untitled"),
                        crate::fmt::day(d.other_created_at), // use the repo's existing date formatter
                        d.detail.as_deref().map(|x| format!(": {x}")).unwrap_or_default(),
                    ));
                }
                ask_excerpt(i + 1, h.title.as_deref().unwrap_or_default(), &h.text, &lines)
```

`ASK_SYSTEM`: append
`"Where a caveat says another note states something differently and your answer rests on it, give both readings with their dates and cite each; do not choose between them."`

`AskResponse::disagreements`: computed where `unsupported`/`retired_only` are (~423-447) over `citations` (the kept hits):

```rust
            let cited: std::collections::HashSet<&str> = citations.iter().map(|c| c.artifact_id.as_str()).collect();
            let mut disagreements: Vec<Disagreement> = Vec::new();
            for c in &citations {
                for d in &c.disagrees_with {
                    let seen = disagreements.iter().any(|x| x.artifact_id == d.other_id && x.other_id == d.artifact_id);
                    if cited.contains(d.other_id.as_str()) && !seen {
                        disagreements.push(d.clone());
                    }
                }
            }
```

Set `disagreements: vec![]` in the two early-return literals.

`_answer.html`: next to the unsupported badge,

```html
{% for d in disagreements %}
<span class="badge" title="{{ d.detail.as_deref().unwrap_or("") }}">your notes disagree: {{ d.other_title.as_deref().unwrap_or("another note") }}</span>
{% endfor %}
```

(Find how the template receives `unsupported` — likely through a Rust view struct in `web/workspace.rs`; add the field there the same way.)

- [ ] **Step 4: Run** `CARGO_INCREMENTAL=0 cargo test --lib` — expect PASS.

- [ ] **Step 5: Commit** `feat(ask): an answer drawn from a disagreement gives both readings`

---

### Task 6: A good answer confirms what it cited

**Files:**
- Modify: `src/core/ingest.rs` (new `Core::confirm_cited` beside `verify` ~1682)
- Modify: `src/store/asks.rs` (new `used_citations`)
- Modify: `src/web/workspace.rs` (`judge_search` "hit" arm ~1000, `ask_verdict` ~926)
- Modify: `src/web/client.rs:123` (`ask_verdict`)

**Interfaces:**
- Produces: `Store::used_citations(&self, event_id: &str) -> Result<Vec<String>>`; `Core::confirm_cited(&self, ids: &[String])` (best-effort, logs, never errors); `Core::judge_ask(&self, id: &str, verdict: AskVerdict) -> Result<()>` which records and, on `Right`, confirms.

- [ ] **Step 1: Write the failing tests** in `src/core/ingest.rs` tests (or wherever `verify` is tested — grep `fn verify` tests):

```rust
    #[tokio::test]
    async fn a_right_answer_confirms_what_it_cited_and_a_wrong_one_does_not() {
        // Record an ask event with two citations, one `used = 1`, one `used = 0`
        // (see store/asks.rs tests for `record_ask` usage). Set both artifacts'
        // last_verified_at to 1.
        // core.judge_ask(&id, AskVerdict::Right) → used one > 1, unused one == 1.
        // A second event judged Wrong → its used citation stays at 1.
    }

    #[tokio::test]
    async fn confirming_an_artifact_out_of_results_is_skipped_not_an_error() {
        // deprecate the artifact; confirm_cited(&[id]) returns; last_verified_at unchanged.
    }
```

- [ ] **Step 2: Run** — expect FAIL.

- [ ] **Step 3: Implement**

```rust
    /// A judged answer is the confirmation "Still accurate" used to ask a
    /// person for. Best-effort: the verdict is the person's and is already
    /// recorded; a stamp that cannot be written costs the stamp, not the
    /// verdict. Out-of-results artifacts are skipped — confirming something
    /// search will not return changes nothing it ranks.
    pub async fn confirm_cited(&self, ids: &[String]) {
        for id in ids {
            match self.store.get_artifact(id).await {
                Ok(c) if c.in_results() => {
                    if let Err(e) = self.verify(id).await {
                        tracing::warn!(artifact_id = %id, error = %e, "could not stamp a confirmed artifact");
                    }
                }
                Ok(_) | Err(Error::NotFound) => {}
                Err(e) => tracing::warn!(artifact_id = %id, error = %e, "could not read a confirmed artifact"),
            }
        }
    }

    pub async fn judge_ask(&self, id: &str, verdict: crate::store::asks::AskVerdict) -> Result<()> {
        self.store.judge_ask(id, verdict).await?;
        if verdict == crate::store::asks::AskVerdict::Right {
            let cited = self.store.used_citations(id).await?;
            self.confirm_cited(&cited).await;
        }
        Ok(())
    }
```

`used_citations`: `SELECT artifact_id FROM ask_citations WHERE event_id = ? AND used = 1 ORDER BY n`.

Replace `tenant.core.store.judge_ask(&id, verdict)` with `tenant.core.judge_ask(&id, verdict)` in `workspace.rs` and `client.rs`. In `judge_search`'s `"hit"` arm, after `Ok(()) =>`, call `tenant.core.confirm_cited(std::slice::from_ref(&artifact_id.to_string())).await;` before returning `"hit"`.

Remove the `stale_max_hits` mention from `mark_artifact_seen`'s doc comment (`core/search.rs` ~930) if the stale list is the only reader — check with grep; Task 9 removes the list.

- [ ] **Step 4: Run** `CARGO_INCREMENTAL=0 cargo test --lib` — expect PASS.

- [ ] **Step 5: Commit** `feat(feedback): a good answer confirms the notes it was drawn from`

---

### Task 7: Low coverage is reread once, on its own

**Files:**
- Move: `reread` and `coverage_final` from `src/web/corpus.rs:132-215` to `src/core/ingest.rs` as `Core::reread_uncovered(&self, cid, from, to) -> Result<bool>` and `pub(crate) fn coverage_final`
- Modify: `src/web/corpus.rs` (handler calls the core method; `src/web/client.rs` `corpus_reread` likewise)
- Modify: `src/store/mod.rs` (migration list ~160) and `src/store/schema.sql` (`corpora.auto_reread_at INTEGER`)
- Modify: `src/store/corpora.rs` (`Corpus::auto_reread_at`, `row_to_corpus`, `mark_auto_reread`)
- Modify: `src/jobs/reconcile.rs:39` (`run`)

**Interfaces:**
- Produces: `Core::reread_uncovered(&self, cid: &str, from: i64, to: i64) -> Result<bool>`; `Store::mark_auto_reread(&self, cid: &str) -> Result<()>`; `Corpus::auto_reread_at: Option<i64>`.

- [ ] **Step 1: Write the failing tests** in `src/jobs/reconcile.rs` tests:

```rust
    #[tokio::test]
    async fn a_finished_capture_with_lost_lines_is_reread_once() {
        // Build a Ready corpus whose artifacts' spans leave lines 5..8 uncovered
        // (see web/corpus.rs tests for a reread fixture — grep `reread` in tests).
        // run(core) → a SegmentWindow job is enqueued and auto_reread_at is set.
        // Finish nothing; run(core) again → no second job is enqueued.
    }

    #[tokio::test]
    async fn a_capture_still_being_read_is_left_alone() {
        // Same corpus with status Segmenting (not coverage_final) → no job, no stamp.
    }
```

- [ ] **Step 2: Run** — expect FAIL.

- [ ] **Step 3: Implement**

Move the function body unchanged into `Core` (replace `tenant.core.` with `self.`); keep `corpus_view::bands` where it is and call it as `crate::web::corpus_view::bands` (it is already pure; if it pulls web-only types, move `bands` to `core` alongside — check its imports first).

Migration entry (same shape as `retired_at`):

```rust
            // NULL on every row that predates it, and truthfully: nothing had
            // reread a capture on its own before this column existed.
            ("corpora", "auto_reread_at", "ALTER TABLE corpora ADD COLUMN auto_reread_at INTEGER"),
```

Add the column to `schema.sql`'s `corpora` table.

In `reconcile::run`, inside the per-corpus loop after the `NeedsReview` branch:

```rust
            // Lines a finished read never turned into an artifact, read once
            // more without being asked. Once: a second miss is what the
            // document is, and the queue row still says how much is covered.
            if c.auto_reread_at.is_none() && crate::core::ingest::coverage_final(&c.status) {
                core.store.mark_auto_reread(&c.id).await?;
                if core.reread_uncovered(&c.id, 1, i64::MAX).await? {
                    armed += 1;
                }
            }
```

(Stamp first so a failing reread cannot loop. Check that `list_corpora_after` returns rows that include the new column.)

- [ ] **Step 4: Run** `CARGO_INCREMENTAL=0 cargo test --lib` — expect PASS.

- [ ] **Step 5: Commit** `feat(reconcile): lines a read missed are read again once, unasked`

---

### Task 8: Insights is a journal; the decision routes go

**Files:**
- Modify: `src/web/insights.rs` (drop `parked` ~560-600 and `unverified` ~601-635 rows, `pairs`/`more_pairs`/gaps fields and reads; rename heading text)
- Modify: `src/web/templates/insights.html` (remove "Needs you", `_decide.html` include, `_gaps.html` include; "Set aside for you" → "What the base did")
- Delete: `src/web/templates/_decide.html`, `src/web/templates/_gaps.html`
- Modify: `src/web/judge.rs:340-355` (remove `/pairs`, `/gaps`, `/gaps/forget`, `/gaps/{kind}/{id}/dismiss`, `/pairs/{id}/{dismiss,discard,supersede,synthesize}`, `/artifacts/{id}/verify`; keep `deprecate`, `reactivate`, `unsupersede`, `merges/{id}/undo`, `condensations/{id}/undo`, `/insights`, `/insights/set-aside`)
- Modify: `src/web/ops.rs:858-871` (remove `corpora/{id}/resolve`, `artifacts/{id}/verify`, `pairs/*`; keep undo/deprecate/reactivate/unsupersede)
- Modify: `src/web/api.rs:2329` (remove `/corpora/{id}/resolve`), `src/web/client.rs:28` (remove `/artifacts/{id}/reviewed`), `src/web/artifact.rs:846` (remove `/ui/artifacts/{cid}/reviewed`)
- Modify: `src/web/templates/_artifact_detail.html` (~60-70 "Hidden as stale" verify, ~157-166 "Mark reviewed" button → badge only)
- Modify: `src/core/ingest.rs` (`resolve_near_duplicate`, `NearDupeAction` — delete if nothing else calls them)
- Modify: `src/web/android_fixtures.rs` (drop fixtures for removed routes)
- Modify: `README.md` (*Judge*, *Duplicates*, *Gaps* bullets), `docs/api.md` (removed routes, new fields)

**Interfaces:**
- Consumes: nothing new.
- Produces: no route for any decision; `/ui/insights` renders measures, Last night, evolve, due, Recent, and "What the base did".

- [ ] **Step 1: Write the failing tests** in `src/web/insights.rs` tests (use the module's request helper, as the tests at ~1598-1740 do):

```rust
    #[tokio::test]
    async fn insights_asks_nothing_of_anyone() {
        // Seed a base with: a Contradiction pair, a near-dupe-flagged corpus,
        // an old unverified artifact, an open gap, and a journaled merge.
        let html = /* GET /ui/insights */;
        assert!(!html.contains("Needs you"), "{html}");
        assert!(!html.contains("Knowledge gaps"), "{html}");
        assert!(!html.contains("Still accurate"), "{html}");
        assert!(!html.contains("Keep both"), "{html}");
        assert!(html.contains("What the base did"), "{html}");
        assert!(html.contains("Undo"), "the merge keeps its undo: {html}");
    }

    #[tokio::test]
    async fn the_decision_routes_are_gone() {
        for (method, uri) in [
            ("POST", "/api/v1/pairs/1/supersede"), ("POST", "/api/v1/pairs/1/synthesize"),
            ("POST", "/api/v1/pairs/1/dismiss"),   ("POST", "/api/v1/pairs/1/discard"),
            ("GET",  "/api/v1/gaps"),              ("POST", "/api/v1/gaps/forget"),
            ("POST", "/api/v1/artifacts/x/verify"),("POST", "/api/v1/corpora/x/resolve"),
            ("POST", "/ui/ops/pairs/1/supersede"), ("POST", "/ui/ops/corpora/x/resolve"),
            ("POST", "/ui/artifacts/x/reviewed"),
        ] {
            assert_eq!(/* status of method uri, authenticated */, 404 or 405, "{method} {uri}");
        }
    }
```

(Use the same authenticated request helper the existing web tests use — grep `test_support` in `src/web`.)

- [ ] **Step 2: Run** `CARGO_INCREMENTAL=0 cargo test --lib web::insights` — expect FAIL.

- [ ] **Step 3: Implement** the removals listed under **Files**. Then `cargo build` and delete whatever becomes dead (handlers, view structs like `web::judge::PairCard`, `web::ops` decide view types, store reads only they used: `parked_corpora`, the stale-list read, gap list reads used only by the page — keep `jobs::gaps` and `core::gaps` and anything `eval`/`pursuit` call). Rewrite tests that asserted the removed UI to assert its absence or the automatic behaviour; do not leave tests for deleted code.

README: replace the *Judge*, *Duplicates* and *Gaps* bullets with:

```markdown
- **Judge** — answer *Was this what you were looking for?* under a search or an
  Ask. That is the only thing the base asks of you: Insights reads recall@10
  and MRR off those verdicts, a good answer confirms the notes it drew on, and
  the idle pass replays them to move the ranking and take back its own
  mistakes. One button forgets it.
- **Duplicates** — read at capture, judged in the background, merged or
  replaced where the judge is sure. Nothing deleted, no merge drops a number
  or a path, every action has an undo on Insights.
- **Disagreements** — two notes that state a value differently are both kept,
  and search and Ask say so, with dates, wherever either one comes up.
```

Remove the *Gaps* bullet. Update `docs/api.md`: remove the routes, document `disagrees_with` on search results and `disagreements` on ask responses with the `Disagreement` shape.

- [ ] **Step 4: Run** `CARGO_INCREMENTAL=0 cargo test` (whole suite, including `tests/`) — expect PASS. Run `cargo clippy --all-targets -- -D warnings` if the repo's CI does (check `.github/workflows`).

- [ ] **Step 5: Commit** `feat(insights): a journal of what the base did, and nothing it asks`

---

### Task 9: Android follows

**Files:**
- Delete: `android/app/src/main/kotlin/io/github/overcuriousity/engram/ui/JudgeScreens.kt`, `.../ui/Judging.kt` (check first that `Journal` does not live in them; if it does, keep the journal composable and move it to `InsightsScreen.kt`)
- Modify: `.../ui/Nav.kt` (remove `Screen.Pairs`, `Screen.Gaps` and their `composable` entries ~259-260)
- Modify: `.../ui/InsightsScreen.kt:93-94`, `.../ui/SettingsScreen.kt:244-245` (remove the lines)
- Modify: `android/core/src/main/kotlin/.../core/Transport.kt:300-330` (remove `pairSupersede`, `pairSynthesize`, `pairDiscard`, `pairDismiss`, `gapDismiss`, gap forget, verify, resolve; keep undo/deprecate/reactivate/unsupersede)
- Modify: `android/core/src/main/kotlin/.../core/read/Models.kt` (`Hit.disagreesWith`, `AskAnswer.disagreements`, new `Disagreement`; drop judging models only the removed screens used)
- Modify: `.../ui/HitRow.kt`, `.../ui/AskScreen.kt` (the line and the badge)
- Test: `android/core/src/test/kotlin/.../core/read/ModelsTest.kt`, `TransportTest.kt`

**Interfaces:**
- Consumes: JSON shape of `Disagreement` from Task 4 (`artifact_id`, `other_id`, `other_title`, `other_created_at`, `detail`).

- [ ] **Step 1: Write the failing test** in `ModelsTest.kt`:

```kotlin
    @Test fun aHitAndAnAnswerSayWhatTheyDisagreeWith() {
        val d = """{"artifact_id":"a","other_id":"b","other_title":"NAS","other_created_at":1726099200,"detail":"30 days there, 14 here"}"""
        val hit = Decode.search("""{"items":[{"artifact_id":"a","disagrees_with":[$d]}],"next":null}""").items.single()
        assertEquals("b", hit.disagreesWith.single().otherId)
        val ask = Decode.ask("""{"answer":"x","disagreements":[$d]}""")   // use the decoder AskAnswer already has
        assertEquals("30 days there, 14 here", ask.disagreements.single().detail)
        assertTrue(Decode.search("""{"items":[{"artifact_id":"a"}]}""").items.single().disagreesWith.isEmpty())
    }
```

- [ ] **Step 2: Run** `cd android && ./gradlew :core:testDebugUnitTest --tests '*ModelsTest*'` — expect FAIL. (If gradle or the SDK is unavailable on this machine, say so in the task report and rely on CI; do not skip the code.)

- [ ] **Step 3: Implement**

```kotlin
@Serializable
data class Disagreement(
    @SerialName("artifact_id") val artifactId: String,
    @SerialName("other_id") val otherId: String,
    @SerialName("other_title") val otherTitle: String? = null,
    @SerialName("other_created_at") val otherCreatedAt: Long = 0,
    val detail: String? = null,
)
```

`Hit`: `@SerialName("disagrees_with") val disagreesWith: List<Disagreement> = emptyList(),` — `AskAnswer`: `val disagreements: List<Disagreement> = emptyList(),`.

`HitRow`: under the snippet, for each `d` a muted line `"Disagrees with ${d.otherTitle ?: "another note"}" + (d.detail?.let { ": $it" } ?: "")`, clickable to `onArtifact(d.otherId)` — match how `HitRow` styles its other secondary lines. `AskScreen`: beside the unsupported badge, one badge per disagreement, `"your notes disagree: ${d.otherTitle ?: "another note"}"`.

Remove the judge screens, nav entries, settings/insights lines and transport calls. Delete the transport tests for removed calls. Remove the "judging is a mechanic to work towards removing" comment in `Nav.kt` with the entries it described.

- [ ] **Step 4: Run** `cd android && ./gradlew :core:testDebugUnitTest :app:compileDebugKotlin` — expect PASS.

- [ ] **Step 5: Commit** `feat(android): the judging screens go, and a hit says what it disagrees with`

---

### Task 10: Real-model check and branch verification

**Files:** none changed unless the check fails.

- [ ] **Step 1:** `CARGO_INCREMENTAL=0 cargo test` and `cargo clippy --all-targets -- -D warnings` — record the summary lines.
- [ ] **Step 2:** Against the user's configured endpoint (their `config.toml`), on a scratch data directory: capture "NAS backups are kept for 30 days." and, a minute later, "NAS backups are kept for 14 days.". Wait for the dedupe sweep (or run the dedupe unit for the pair via a debug path the tests use) until the pair is `contradiction`. Then `engram -a "how long are NAS backups kept"` (or `POST /api/v1/ask`). Show the user the answer verbatim and whether it gives both readings with dates, and the `disagreements` field.
- [ ] **Step 3:** If the model ignores the instruction, report it with the output — do not tune the prompt without the user.
- [ ] **Step 4:** Hand off to `superpowers:finishing-a-development-branch`.
