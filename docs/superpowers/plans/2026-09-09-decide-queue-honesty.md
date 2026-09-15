# Decide Queue Honesty Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop the decide queue from claiming two artifacts disagree when nothing judged them, and give an operator a way to synthesize a pair that genuinely covers the same ground.

**Architecture:** Five independent changes plus one verification probe. `interference()` stops writing pairs, which removes the queue's only current producer and heals a sentinel collision by consequence. The judge's prose stops standing in as the reason an action was taken. The dedupe response schema puts the reasoning before the label. A near-duplicate threshold moves onto evidence. And a new operator-intent column on `artifact_pairs` lets a button record "these cover the same ground" and hand the writing to the job queue, where every other inference in this tree already lives.

**Tech Stack:** Rust, axum + askama (`src/web`), sqlx/SQLite (`src/store`), tokio test harness. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-09-09-decide-queue-honesty-design.md`

## Global Constraints

- **Never string-match model prose to infer meaning.** The tree paid for that once (`infer::facts`, post-mortem above `dedupe_prompt` in `src/infer/prompt.rs`). Cross-checks are structural or they are inference; they are never token comparison.
- **No inference call from a web handler.** Every model call is a `Stage` on the job queue. `src/web` calls no judge and no synthesizer.
- **`strict` JSON schemas require every property they list.** A listed-but-optional property is rejected outright by the hosted APIs (see the `dedupe_schema` doc comment). Any new schema lists and requires every field, and sets `additionalProperties: false`.
- **Migrations are additive `ALTER TABLE` entries** in the table inside `src/store/mod.rs::migrate` (around line 140), written as `(table, column, sql)` triples. `migrate` cannot drop a column.
- **Tests:** `cargo test --lib <name>` for a single test. The suite is large — run targeted tests per task, and one full `cargo test` at the end, reported afterwards rather than gating each commit.
- **Commits** end with:
  ```
  Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01DBPwtgFZnXyWPAuGTvhYsx
  ```
- **The production base is not touched by any code task.** Task 8 is the only one that runs against it, and only on explicit go-ahead from the user.

## File Structure

| File | Responsibility | Task |
|---|---|---|
| `src/jobs/sleep.rs` | `interference()` detects and counts; no longer writes | 1 |
| `src/web/templates/_sleep.html` | reports observed, not filed | 1 |
| `src/jobs/tune.rs`, `src/jobs/retract.rs`, `src/jobs/retention.rs` | field doc-comments for the interference count | 1 |
| `src/jobs/dedupe.rs` | action detail (2); the `synthesis_asked` branch (7) | 2, 7 |
| `src/web/insights.rs` | merge journal line | 2 |
| `src/infer/prompt.rs` | `dedupe_schema` field order (3); synthesis prompt (7) | 3, 7 |
| `src/config.rs`, `config.example.toml` | `near_dupe_min` default | 4 |
| `src/store/mod.rs` | migration for `synthesis_asked` | 5 |
| `src/store/pairs.rs` | `ArtifactPair::synthesis_asked`, setter, clear | 5 |
| `src/web/ops.rs` | `mergeable` on `PairRow`; the synthesize route | 6 |
| `src/web/templates/_decide.html` | the Synthese button and the pending line | 6 |

**Task order matters in two places.** Task 3 begins with a probe against the live judge, because a negative result changes what Task 3 ships. Tasks 5 → 6 → 7 are strictly sequential: the column, then the button that sets it, then the job that reads it.

---

### Task 1: `interference()` stops filing pairs

Removes the queue's only current producer. Detection and counting stay; the writes go. This also settles the `score == 0.0` collision by consequence — afterwards the link judge in `src/jobs/associate.rs:546` is the only writer of an exact zero, which is the single meaning `src/web/ops.rs` documents.

**Files:**
- Modify: `src/jobs/sleep.rs:446-554` (the `interference` function)
- Modify: `src/jobs/sleep.rs` tests near lines 990-1090
- Modify: `src/web/templates/_sleep.html` (the interference line)
- Modify: `src/jobs/tune.rs:75-76`, `src/jobs/retract.rs:37-38`, `src/jobs/retention.rs:44-45` (doc comments)

**Interfaces:**
- Consumes: nothing.
- Produces: `interference(core, live, started) -> Result<(usize, bool)>` keeps its signature. The `usize` changes meaning from "pairs filed" to "interferers observed". No caller's types change.

- [ ] **Step 1: Rewrite the existing test to assert nothing is filed**

In `src/jobs/sleep.rs`, rename `interference_files_one_pending_pair_and_never_the_same_pair_twice` and replace its assertions. The fixture is unchanged — only what it checks changes.

```rust
    #[tokio::test]
    async fn interference_counts_what_it_sees_and_files_no_pair() {
        let (mut core, a1, _a2, b) = two_corpora().await;
        core.evolve.autonomous = crate::config::Autonomy::Full;
        let live = live_generation(&core).await;
        // A probe for a1 that b — another corpus — has outranked twice.
        // Hand-written results keep the fake embedder's ordering out of it.
        let pid = core
            .store
            .record_rehearsal(&NewRehearsal {
                class: Class::Cue,
                query: "q".into(),
                query_vec: vec![0.0; crate::core::test_support::TEST_DIM],
                embed_model: core.embedder.model().to_string(),
                artifact_id: a1.clone(),
                source_id: None,
            })
            .await
            .unwrap()
            .unwrap();
        for _ in 0..2 {
            core.store
                .record_rehearsal_result(&crate::store::rehearsals::NewResult {
                    rehearsal_id: pid.clone(),
                    generation_id: live.id.clone(),
                    rank: Some(2),
                    outranked_by: vec![b.clone()],
                })
                .await
                .unwrap();
        }
        let (seen, _) = interference(&core, &live, crate::store::now())
            .await
            .unwrap();
        assert_eq!(seen, 1, "the interferer is still counted");
        assert!(
            core.store.pair_between(&a1, &b).await.unwrap().is_none(),
            "retrieval competition is not a dedupe finding and files no pair"
        );
    }
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test --lib interference_counts_what_it_sees_and_files_no_pair`
Expected: FAIL on the `is_none()` assertion — `interference()` still files the pair.

- [ ] **Step 3: Remove the writing from `interference()`**

In `src/jobs/sleep.rs`, inside `for x in found {`, delete the `pair_between` guard, the `detail` string, the `score` lookup and the `record_pair_with_detail` call. Delete the `may_act()` early return too: it guarded spending, and after this nothing is spent. Rename the accumulator `filed` to `seen` throughout, including both early returns and the final `Ok((seen, false))`.

The loop body becomes:

```rust
        for x in found {
            // `outranked_by` is free-form JSON written when the rehearsal ran,
            // and an id in it can name an artifact a burial or a merge has
            // since taken away. Not counted either: an id the base no longer
            // has is not an observation about anything still in results.
            if gone.contains(&x) {
                tracing::debug!(owner = %owner.id, outranker = %x, "an outranker the base no longer has");
                continue;
            }
            // A pair the base once acted on and took back is a person's now,
            // and their decision is not re-observed at them.
            if core.store.action_was_undone(&owner.id, Kind::Merge).await?
                || core
                    .store
                    .action_was_undone(&owner.id, Kind::Supersede)
                    .await?
                || core.store.action_was_undone(&x, Kind::Merge).await?
                || core.store.action_was_undone(&x, Kind::Supersede).await?
            {
                continue;
            }
            seen += 1;
        }
```

Keep the `activity_since` early return at the top of the outer loop: not writing is not a reason to keep working while somebody is using the base.

- [ ] **Step 4: Correct the function's doc comment**

Directly above `pub async fn interference`, replace the doc comment with:

```rust
/// Which artifacts stand above an owner in every one of its retained
/// rehearsals — retrieval competition, counted for Ops. Returns
/// (interferers observed, stopped early).
///
/// It files nothing, and that is the point. It used to record an
/// `artifact_pairs` row per interferer, which put a ranking observation onto a
/// queue whose cards make claims about meaning: two documents sharing a
/// template outrank each other constantly and agree about nothing, and the
/// page said "these two disagree" about every one of them. It also carried
/// neither passage guard its sibling producers have (`relate::arm`,
/// `associate::judge`), so it was the only reason passage pairs reached
/// consolidation at all. And it invented a `0.0` score out of a missed
/// neighbour lookup, colliding with the marker `web::ops` reads as "found by
/// co-retrieval, never measured".
```

- [ ] **Step 5: Run the test and the neighbouring ones**

Run: `cargo test --lib interference`
Expected: PASS. The sibling test that asserts a missing outranker does not fail the sweep must be updated the same way if it asserts on `pair_between` — change it to assert the count only.

- [ ] **Step 6: Correct the wording everywhere the count is named**

The three field doc-comments all read "Pairs rule 3 filed for interference." Change each to "Interferers rule 3 observed." in:
- `src/jobs/tune.rs:75-76`
- `src/jobs/retract.rs:37-38`
- `src/jobs/retention.rs:44-45`

In `src/web/templates/_sleep.html`, find the line rendering the interference count and change it from filing language to observation language, so the page does not promise cards that will never appear.

- [ ] **Step 7: Add a regression test for the sentinel**

In `src/jobs/sleep.rs` tests, add:

```rust
    /// `web::ops` reads an exact zero as "found by co-retrieval, never
    /// measured". Interference used to manufacture one out of a missed
    /// neighbour lookup, which put two meanings on one value.
    #[tokio::test]
    async fn interference_writes_no_pair_carrying_a_zero_score() {
        let (mut core, a1, _a2, b) = two_corpora().await;
        core.evolve.autonomous = crate::config::Autonomy::Full;
        let live = live_generation(&core).await;
        let pid = core
            .store
            .record_rehearsal(&NewRehearsal {
                class: Class::Cue,
                query: "q".into(),
                query_vec: vec![0.0; crate::core::test_support::TEST_DIM],
                embed_model: core.embedder.model().to_string(),
                artifact_id: a1.clone(),
                source_id: None,
            })
            .await
            .unwrap()
            .unwrap();
        for _ in 0..2 {
            core.store
                .record_rehearsal_result(&crate::store::rehearsals::NewResult {
                    rehearsal_id: pid.clone(),
                    generation_id: live.id.clone(),
                    rank: Some(2),
                    outranked_by: vec![b.clone()],
                })
                .await
                .unwrap();
        }
        interference(&core, &live, crate::store::now()).await.unwrap();
        for p in core
            .store
            .pairs_by_state(crate::store::pairs::PairState::Pending, 100)
            .await
            .unwrap()
        {
            assert!(
                p.score != 0.0 || p.detail.as_deref() == Some("link"),
                "an exact zero means the link judge filed it, and nothing else"
            );
        }
    }
```

- [ ] **Step 8: Run both tests**

Run: `cargo test --lib interference`
Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add src/jobs/sleep.rs src/jobs/tune.rs src/jobs/retract.rs src/jobs/retention.rs src/web/templates/_sleep.html
git commit -m "$(cat <<'EOF'
fix(sleep): retrieval competition is not a claim about meaning

`interference()` filed an `artifact_pairs` row per interferer, and the decide
card renders those as "these two disagree". Two documents sharing a template
outrank each other constantly and agree about nothing.

It also carried neither passage guard its sibling producers have, so it was the
only reason passage pairs reached consolidation, and it invented a 0.0 score
out of a missed neighbour lookup — colliding with the marker `web::ops` reads
as "found by co-retrieval, never measured".

It now detects and counts, and writes nothing.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DBPwtgFZnXyWPAuGTvhYsx
EOF
)"
```

---

### Task 2: an action's reason stops being the judge's prose

One field serves two roles today — what the model thought, and why the base acted — and only the first can argue against the second. The live base holds the proof: a merge whose recorded justification ends "so they are distinct".

**Files:**
- Modify: `src/jobs/dedupe.rs:595-604` (the `record_action` call in the `Relation::Duplicate` arm)
- Modify: `src/web/insights.rs` around line 750 (the merge journal line)
- Test: `src/jobs/dedupe.rs` tests

**Interfaces:**
- Consumes: nothing.
- Produces: `Kind::Merge` rows in `corpus_actions` carry a description of the act. The judge's sentence remains reachable through the pair, written by `set_pair_merged` — unchanged.

- [ ] **Step 1: Write the failing test**

In the `mod tests` of `src/jobs/dedupe.rs`, add:

```rust
    /// The judge's sentence says what the model thought. It is not the reason
    /// the base acted, and it can contradict the act: the live base holds a
    /// merge justified by "…so they are distinct".
    #[tokio::test]
    async fn a_merge_action_records_the_act_and_not_the_judges_sentence() {
        let core = test_core().await;
        let ids = seed(
            &core,
            &[
                ("the timeout is 30s", [0.99, 0.01]),
                ("the timeout is 30 seconds", [0.99, 0.02]),
            ],
        )
        .await;
        core.store.record_pair(&ids[0], &ids[1], 0.95).await.unwrap();

        run(&core).await.unwrap();

        let rows = core.store.open_actions(&[Kind::Merge], 10).await.unwrap();
        assert!(!rows.is_empty(), "the merge was applied");
        for r in rows {
            let d = r.detail.unwrap_or_default();
            assert!(
                d.contains("merged"),
                "the action says what happened, got {d:?}"
            );
        }
    }
```

The fixture mirrors `undoing_a_supersession_puts_the_question_back` in the same file; copy its `seed`/`test_core` usage and its fake-judge setup so the verdict is a `duplicate`.

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test --lib a_merge_action_records_the_act_and_not_the_judges_sentence`
Expected: FAIL — the detail is the judge's sentence, which does not contain "merged".

- [ ] **Step 3: Give the action its own description**

In `src/jobs/dedupe.rs`, in the `Relation::Duplicate` arm, replace the `s.detail.as_deref()` argument to `action(...)` with a description of the act:

```rust
            // One row per original, naming the merge, before the pair says it
            // happened: a merge with no row is what the journal exists to end.
            //
            // The action's own description, not `s.detail`. That field is the
            // judge's sentence — what the model thought — and it can argue
            // against the act it was being used to justify: the base holds a
            // merge recorded as "…so they are distinct". The sentence is not
            // lost; `set_pair_merged` below carries it onto the pair, which is
            // where a reader looks for what the judge said.
            let act = format!("merged into {} from {} sources", m.id, sources.len());
            for source in &sources {
                core.store
                    .record_action(&action(
                        Kind::Merge,
                        &s.pair,
                        source,
                        Some(&m.id),
                        Some(act.as_str()),
                    ))
                    .await?;
            }
```

Leave the `set_pair_merged(s.pair.id, &m.id, s.detail.as_deref(), DecidedBy::Model)` call below it exactly as it is.

- [ ] **Step 4: Run the test**

Run: `cargo test --lib a_merge_action_records_the_act_and_not_the_judges_sentence`
Expected: PASS.

- [ ] **Step 5: Fix the journal line that read the old field**

`src/web/insights.rs` around line 750 renders the merge journal from `corpus_actions`. It now prints the act rather than the judge's prose, which is correct and needs no code change — but check the surrounding copy still reads correctly, and if the template presents the detail as a justification ("because …"), reword it to present it as what happened.

- [ ] **Step 6: Run the insights tests**

Run: `cargo test --lib insights`
Expected: PASS. Fix any test asserting the old detail text.

- [ ] **Step 7: Commit**

```bash
git add src/jobs/dedupe.rs src/web/insights.rs
git commit -m "$(cat <<'EOF'
fix(dedupe): a merge's journal line says what happened, not what the model thought

`corpus_actions.detail` carried the judge's sentence as the justification for
the action. One field, two roles — and only the first can contradict the
second. The base holds a merge recorded as "Both artifacts describe the same
veterinary practice … so they are distinct".

The action row now describes the act. The judge's sentence is unchanged on the
pair, where `set_pair_merged` already put it.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DBPwtgFZnXyWPAuGTvhYsx
EOF
)"
```

---

### Task 3: reason before verdict

The response format is sent with `"strict": true` and compiled into "a grammar the decoder cannot leave" (`src/infer/openai.rs:888`). `relation` is the first property of every variant, so the model must name the label before writing a word of justification — and the justification, written second, is where the better reasoning shows up.

**This task opens with a probe.** The four `anyOf` variants are discriminated by `relation`; with `detail` first they share a common prefix and the branch is not decided until `relation` arrives. Grammar compilers handle common prefixes, but the tree warns that a grammar is only as good as the endpoint honouring it. Find out before committing.

**Files:**
- Modify: `src/infer/prompt.rs:2224` (`dedupe_schema`)
- Modify: `src/infer/prompt.rs:1197` (`DEDUPE_SYSTEM`, the example shape)
- Test: `src/infer/prompt.rs` tests

**Interfaces:**
- Consumes: nothing.
- Produces: no signature changes. `parse_dedupe` is untouched — it deserializes through serde and is order-independent.

- [ ] **Step 1: Probe the live judge**

Read the judge's endpoint and model out of the deployment's config, then send one request carrying the reordered schema and a two-artifact prompt, and check the reply parses and its `relation` is one of the five allowed values.

```bash
ssh svc-engram@engram.mikoshi.cc 'grep -A6 "\[judge\]\|\[infer\]" ~/engram/config.toml'
```

Build the reordered schema by hand (one variant is enough to learn the answer) and POST it to the endpoint with `"strict": true`. Confirm three things: the call is not rejected outright, the reply is valid JSON against the schema, and `detail` precedes `relation` in the raw bytes.

**If the endpoint rejects the schema or ignores the ordering:** stop, report it, and ship only the `DEDUPE_SYSTEM` half of this task — the example shape and an explicit instruction to state the reason first. Then F3 from the spec (a second inference pass) becomes necessary and gets its own plan. Do not force the schema change through.

- [ ] **Step 2: Write the failing test**

In the `mod tests` of `src/infer/prompt.rs`:

```rust
    /// The model must state why before it names what. `relation` first made
    /// the label a commitment taken before any reasoning was written, and the
    /// reasoning then diverged from it — the base holds a merge whose recorded
    /// reason concludes the two artifacts are distinct.
    #[test]
    fn every_dedupe_variant_asks_for_the_reason_before_the_verdict() {
        let schema = dedupe_schema();
        let variants = schema["properties"]["verdict"]["anyOf"]
            .as_array()
            .expect("the verdict is a union of variants");
        assert_eq!(variants.len(), 4);
        for v in variants {
            let required = v["required"].as_array().expect("required is a list");
            assert_eq!(
                required[0].as_str(),
                Some("detail"),
                "detail comes first in required, got {required:?}"
            );
            let first_property = v["properties"]
                .as_object()
                .expect("properties is an object")
                .keys()
                .next()
                .map(String::as_str);
            assert_eq!(
                first_property,
                Some("detail"),
                "detail comes first in properties"
            );
        }
    }

    /// Order is a generation concern, not a parsing one. A reply written
    /// either way still reads.
    #[test]
    fn a_verdict_parses_with_the_reason_before_or_after_the_relation() {
        let before = r#"{"verdict":{"detail":"same thing twice","relation":"distinct"}}"#;
        let after = r#"{"verdict":{"relation":"distinct","detail":"same thing twice"}}"#;
        for body in [before, after] {
            let v = parse_dedupe(body).expect("both orders parse");
            assert_eq!(v.relation, Relation::Distinct);
            assert_eq!(v.detail.as_deref(), Some("same thing twice"));
        }
    }
```

`serde_json::Value` preserves object key order only with the `preserve_order` feature. Check `Cargo.toml`: if it is not enabled, assert on the `required` array alone — it is an ordinary JSON array and its order is always preserved — and drop the `properties` half of the first test.

- [ ] **Step 3: Run and watch it fail**

Run: `cargo test --lib every_dedupe_variant_asks_for_the_reason_before_the_verdict`
Expected: FAIL — `required[0]` is `"relation"`.

- [ ] **Step 4: Reorder the schema**

In `dedupe_schema()`, move `detail` ahead of `relation` in all four variants, in both `properties` and `required`. The `duplicate` variant becomes:

```rust
                    {
                        "type": "object",
                        "properties": {
                            "detail": {"type": "string"},
                            "relation": {"type": "string", "enum": ["duplicate"]},
                            "merged": merged
                        },
                        "required": ["detail", "relation", "merged"],
                        "additionalProperties": false
                    },
```

Apply the same move to the `replaced`, `conflict` and `distinct` variants, leaving every other property, enum and pattern untouched.

- [ ] **Step 5: Record why, above the function**

Add to the `dedupe_schema` doc comment:

```rust
/// `detail` precedes `relation` in every variant, and that ordering is the
/// point of the schema rather than a formatting choice. Under `strict` the
/// schema is a grammar, so the model emits the fields in this order — and with
/// `relation` first it had to commit to a label before writing a word of
/// justification. The justification, written second, is where the better
/// reasoning appears: the base holds a merge the model labelled `duplicate`
/// and then justified with "…so they are distinct". Nothing reconciles the two
/// afterwards, and nothing should try — agreement between a label and a
/// sentence of prose is not a question token comparison can answer. It is
/// prevented here instead.
///
/// The four variants are still discriminated by `relation`, which now arrives
/// second; they share a common prefix until it does. This was verified against
/// the live judge before it was committed.
```

- [ ] **Step 6: Update the example shape in `DEDUPE_SYSTEM`**

In `src/infer/prompt.rs:1197`, change the shape line to:

```
{"verdict": {"detail": "...", "relation": "duplicate", "merged": {"title": "...", "text": "...", "category": "...", "caveats": []}}}
```

and reorder the bullet list below it so `detail` is described first. Change its description to name the order: `detail: one short sentence saying why, written before you name the relation. Always.`

- [ ] **Step 7: Run the prompt tests**

Run: `cargo test --lib prompt`
Expected: PASS, including the existing `DEDUPE_SYSTEM` assertions at `src/infer/prompt.rs:3324-3325`.

- [ ] **Step 8: Commit**

```bash
git add src/infer/prompt.rs
git commit -m "$(cat <<'EOF'
fix(infer): the dedupe judge states its reason before its verdict

Under `strict` the response schema is a grammar the decoder cannot leave, and
`relation` was the first property of every variant — so the model committed to
a label before writing any justification. The justification, written second, is
where the better reasoning shows up: the base holds a merge labelled
`duplicate` and justified with "…so they are distinct".

Reconciling the two after the fact would mean comparing a label against free
prose, which is not a question token matching can answer. This prevents the
divergence instead. `parse_dedupe` is unchanged and reads either order.

Verified against the live judge: the variants share a common prefix until
`relation` arrives, and the endpoint honours it.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DBPwtgFZnXyWPAuGTvhYsx
EOF
)"
```

---

### Task 4: `near_dupe_min` onto evidence

Measured over all 3916 corpus pairs in the live base with the tree's own estimator: the one true duplicate scores 0.789, and the next-highest pair in the entire base scores 0.036. The shipped threshold of 0.90 sits just above the only case that matters.

**Files:**
- Modify: `src/config.rs:932`
- Modify: `config.example.toml:475`
- Test: `src/config.rs` tests

**Interfaces:**
- Consumes: nothing.
- Produces: `ConsolidateConfig::near_dupe_min` defaults to `0.60`.

- [ ] **Step 1: Write the failing test**

In the `mod tests` of `src/config.rs`:

```rust
    /// Measured over the live base with `store::shingle::similarity`: the one
    /// real duplicate — one article captured through two doors — scores 0.789,
    /// and the next-highest of all 3916 corpus pairs scores 0.036. A threshold
    /// has to sit in that gap, and 0.90 sat above the only case in it.
    #[test]
    fn near_dupe_min_catches_a_recapture_and_not_a_related_document() {
        let c = Config::default();
        assert!(
            c.consolidate.near_dupe_min <= 0.78,
            "a re-capture through another door scored 0.789"
        );
        assert!(
            c.consolidate.near_dupe_min > 0.10,
            "the highest false candidate in the base scored 0.036"
        );
    }
```

Match the surrounding tests' way of building a default config — if they use a helper rather than `Config::default()`, use that.

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test --lib near_dupe_min_catches_a_recapture_and_not_a_related_document`
Expected: FAIL — `0.90 <= 0.78` is false.

- [ ] **Step 3: Move the default**

In `src/config.rs:932`, change `near_dupe_min: 0.90,` to `near_dupe_min: 0.60,` and record the evidence in the field's doc comment at line 884:

```rust
    /// Estimated Jaccard over word shingles above which a capture is parked as
    /// a near-duplicate of one already held.
    ///
    /// 0.60 is measured rather than chosen. Over the live base's 3916 corpus
    /// pairs, the one real duplicate — a news article captured once through
    /// the web door and once through the journal door, differing only in
    /// extraction boilerplate — scores 0.789, and the next-highest pair in the
    /// whole base scores 0.036. Everything between those two numbers is empty,
    /// so the threshold sits in the middle of a gap rather than on a slope.
    /// It shipped at 0.90, which is above the only case it had to catch.
    pub near_dupe_min: f64,
```

- [ ] **Step 4: Update the shipped example**

In `config.example.toml:475`, change `near_dupe_min = 0.90` to `near_dupe_min = 0.60` and extend the comment above it to say a re-capture through a different door differs by more than a tenth in boilerplate alone.

- [ ] **Step 5: Run the config tests**

Run: `cargo test --lib config`
Expected: PASS. If a test asserts the old default, update it and say why in its comment.

- [ ] **Step 6: Commit**

```bash
git add src/config.rs config.example.toml
git commit -m "$(cat <<'EOF'
fix(config): near_dupe_min sits in the gap the base actually has

Over the live base's 3916 corpus pairs, the one real duplicate scores 0.789 and
the next-highest scores 0.036. Nothing lies between. The threshold shipped at
0.90, just above the only case it had to catch, so one article captured through
two doors stayed in the base twice.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DBPwtgFZnXyWPAuGTvhYsx
EOF
)"
```

---

### Task 5: `synthesis_asked` on the pair

The operator's judgement — *these two cover the same ground* — recorded where the pair lives. Deliberately a column and not a `PairState`: the state describes what the judge found, and this describes what a person decided. They are different facts and both are worth keeping.

**Files:**
- Modify: `src/store/mod.rs` (the migration table, around line 140)
- Modify: `src/store/pairs.rs:198-230` (`ArtifactPair`), plus the row mapper and a setter
- Test: `src/store/pairs.rs` tests

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `ArtifactPair.synthesis_asked: bool`
  - `Store::ask_pair_synthesis(&self, id: i64) -> Result<()>` — sets the flag
  - `Store::clear_pair_synthesis(&self, id: i64) -> Result<()>` — clears it

- [ ] **Step 1: Write the failing test**

In the `mod tests` of `src/store/pairs.rs`:

```rust
    /// The operator's judgement, kept apart from the judge's. `state` says
    /// what the model found; this says what a person decided to do about it.
    #[tokio::test]
    async fn a_pair_remembers_that_a_person_asked_for_a_synthesis() {
        let s = test_store().await;
        let ids = seed_two_artifacts(&s).await;
        s.record_pair(&ids[0], &ids[1], 0.91).await.unwrap();
        let p = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].clone();
        assert!(!p.synthesis_asked, "nobody has asked yet");

        s.ask_pair_synthesis(p.id).await.unwrap();
        let after = s.get_pair(p.id).await.unwrap();
        assert!(after.synthesis_asked);
        assert_eq!(after.state, PairState::Pending, "the judge's finding stands");

        s.clear_pair_synthesis(p.id).await.unwrap();
        assert!(!s.get_pair(p.id).await.unwrap().synthesis_asked);
    }
```

Use whatever the file's other tests use to build a store and two artifacts; `seed_two_artifacts` is a stand-in for that existing helper.

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test --lib a_pair_remembers_that_a_person_asked_for_a_synthesis`
Expected: FAIL to compile — no `synthesis_asked` field, no `ask_pair_synthesis`.

- [ ] **Step 3: Add the migration**

In the `(table, column, sql)` table inside `src/store/mod.rs::migrate`, beside the `decided_by` entry:

```rust
            // Not nullable and defaulted to 0, unlike `decided_by` above: the
            // absence of a request is a real answer about every existing row,
            // and "nobody asked" is exactly true of all of them.
            (
                "artifact_pairs",
                "synthesis_asked",
                "ALTER TABLE artifact_pairs ADD COLUMN synthesis_asked INTEGER NOT NULL DEFAULT 0",
            ),
```

- [ ] **Step 4: Add the field and the two writers**

In `src/store/pairs.rs`, add to `ArtifactPair` after `judge_unreadable`:

```rust
    /// An operator pressed Synthese on this pair: they have judged that the two
    /// cover the same ground, and the writing is what remains. A column rather
    /// than a `PairState` on purpose — `state` records what the judge found,
    /// and overwriting it would lose that finding to record a different kind
    /// of fact.
    pub synthesis_asked: bool,
```

Add `synthesis_asked` to every `SELECT` that builds an `ArtifactPair` and to the row mapper, reading it as `r.get::<i64, _>("synthesis_asked") != 0`. Then add the two writers beside `set_pair_merged`:

```rust
    /// Record that a person asked for this pair to be synthesized. The dedupe
    /// unit reads it and takes the write-only prompt instead of the verdict
    /// prompt: the judgement has been made, and only the writing is left.
    pub async fn ask_pair_synthesis(&self, id: i64) -> Result<()> {
        sqlx::query("UPDATE artifact_pairs SET synthesis_asked = 1 WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Clear it. Called when the merge path refuses the draft, so the card
    /// stops promising a synthesis that will not arrive.
    pub async fn clear_pair_synthesis(&self, id: i64) -> Result<()> {
        sqlx::query("UPDATE artifact_pairs SET synthesis_asked = 0 WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
```

- [ ] **Step 5: Run the test**

Run: `cargo test --lib a_pair_remembers_that_a_person_asked_for_a_synthesis`
Expected: PASS.

- [ ] **Step 6: Run the whole pairs module**

Run: `cargo test --lib store::pairs`
Expected: PASS. Any `ArtifactPair` literal in a test needs the new field.

- [ ] **Step 7: Commit**

```bash
git add src/store/mod.rs src/store/pairs.rs
git commit -m "$(cat <<'EOF'
feat(store): a pair remembers that a person asked for a synthesis

`state` records what the judge found. This records what an operator decided to
do about it, which is a different fact and worth keeping alongside rather than
on top of.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DBPwtgFZnXyWPAuGTvhYsx
EOF
)"
```

---

### Task 6: the Synthese button

The card gains a fourth answer. It renders only where a merge can actually be written, so it never offers what the merge path will refuse.

**Files:**
- Modify: `src/web/ops.rs:378-392` (`PairRow`), `pair_rows` around 424-530, `routes()` at 632-648
- Modify: `src/web/templates/_decide.html`
- Test: `src/web/ops.rs` tests

**Interfaces:**
- Consumes: `Store::ask_pair_synthesis` from Task 5.
- Produces:
  - `PairRow.mergeable: bool`, `PairRow.synthesis_asked: bool`
  - route `POST /ui/ops/pairs/{id}/synthesize` → `ask_pair_synthesis_ui`

- [ ] **Step 1: Write the failing tests**

In the `mod tests` of `src/web/ops.rs`:

```rust
    /// `insert_merged_artifact` refuses a lineage that names anything but
    /// captured roots. Offering a button the merge path will refuse is a press
    /// that answers with a validation error, so the card does not offer it.
    #[tokio::test]
    async fn the_synthesize_button_is_offered_only_where_a_merge_can_be_written() {
        let tenant = test_tenant().await;
        let passages = seed_pair_of_passages(&tenant).await;
        let (rows, _) = pair_rows(&tenant).await.unwrap();
        let row = rows
            .iter()
            .find(|r| r.a_id == passages.0)
            .expect("the pair is on the queue");
        assert!(
            !row.mergeable,
            "a passage is stored source text and a merge may not rewrite it"
        );
    }

    /// The press records a judgement and arms a unit. It writes no artifact
    /// and calls no model: no route in this tree does.
    #[tokio::test]
    async fn pressing_synthesize_records_the_ask_and_writes_nothing() {
        let tenant = test_tenant().await;
        let (pair_id, a, b) = seed_mergeable_pair(&tenant).await;
        let before = tenant.core.store.count_artifacts().await.unwrap();

        let app = router(tenant.clone());
        let res = app
            .oneshot(form(
                &format!("/ui/ops/pairs/{pair_id}/synthesize"),
                &cookie,
                "",
            ))
            .await
            .unwrap();
        assert!(res.status().is_success() || res.status().is_redirection());

        let p = tenant.core.store.get_pair(pair_id).await.unwrap();
        assert!(p.synthesis_asked, "the ask is recorded");
        assert_eq!(
            tenant.core.store.count_artifacts().await.unwrap(),
            before,
            "the route writes no artifact"
        );
        assert!(
            tenant.core.store.get_artifact(&a).await.unwrap().in_results()
                && tenant.core.store.get_artifact(&b).await.unwrap().in_results(),
            "and hides neither side"
        );
    }
```

Model the fixtures and the `oneshot(form(...))` call on the existing test at `src/web/ops.rs:942`, which posts to `/ui/ops/merges/{merged}/undo`. `count_artifacts` is a stand-in — use whatever the module already has, or count via `pairs_by_state`/a direct query.

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test --lib the_synthesize_button_is_offered_only_where_a_merge_can_be_written`
Expected: FAIL to compile — no `mergeable` field, no route.

- [ ] **Step 3: Add the two fields to `PairRow`**

In `src/web/ops.rs`, after `vacuous`:

```rust
    /// Every root of both members is `Captured`, so `insert_merged_artifact`
    /// will accept a merge over them. The same check `jobs::dedupe` makes at
    /// admission (`dedupe.rs:185-187`), asked here so the card does not offer a
    /// button whose press can only come back a validation error.
    pub mergeable: bool,
    /// An operator has already pressed Synthese and the writing is queued. The
    /// row says so instead of offering the buttons again.
    pub synthesis_asked: bool,
```

- [ ] **Step 4: Fill them in `pair_rows`**

Inside the loop, before `pairs.push(PairRow { … })`:

```rust
            // The lineage check `jobs::dedupe` makes before it calls the model.
            // A passage is its own root, so a pair of passages fails it — which
            // is the common case and exactly the one worth not offering.
            let member_ids = vec![p.a_id.clone(), p.b_id.clone()];
            let root_map = tenant.core.store.roots_of(&member_ids).await?;
            let all_roots: Vec<String> = root_map.values().flatten().cloned().collect();
            let mergeable = !all_roots.is_empty()
                && tenant
                    .core
                    .store
                    .artifacts_by_ids(&all_roots)
                    .await?
                    .iter()
                    .all(|r| r.provenance == crate::store::artifacts::Provenance::Captured);
```

and add `mergeable,` and `synthesis_asked: p.synthesis_asked,` to the struct literal.

- [ ] **Step 5: Add the route handler**

Beside `apply_pair_supersede_ui` in `src/web/ops.rs`:

```rust
/// Record that this pair should become one artifact, and arm the unit that
/// will write it.
///
/// The press is the judgement: an operator has read both sides and decided
/// they cover the same ground. What is left is the writing, and the writing is
/// an inference call — so it goes where every other inference in this tree
/// goes, onto the job queue, where the budget, the backoff and the attempt
/// count live. Nothing is written here and no model is called: no route in
/// `src/web` calls one.
async fn ask_pair_synthesis_ui(tenant: Tenant, Path(pid): Path<i64>) -> UiResult<Response> {
    let pair = tenant.core.store.get_pair(pid).await?;
    // The same refusal the Keep buttons make: every button on this card acts
    // on both sides, and all of them refuse an artifact that is not active.
    let (a, b) = (
        tenant.core.store.get_artifact(&pair.a_id).await?,
        tenant.core.store.get_artifact(&pair.b_id).await?,
    );
    if !a.in_results() || !b.in_results() {
        return Err(crate::error::Error::Validation(
            "one of these has already left results".into(),
        ));
    }
    tenant.core.store.ask_pair_synthesis(pid).await?;
    tenant
        .core
        .store
        .rearm_idle_seq(
            crate::store::jobs::Stage::Dedupe,
            "pair",
            &pid.to_string(),
            0,
        )
        .await?;
    // Back where it was pressed, like its neighbours.
    Ok(redirect_back())
}
```

Match the return type and the redirect helper the neighbouring handlers use — read `dismiss_pair_ui` and copy its shape exactly, including its `ReturnTo` handling if it has one.

- [ ] **Step 6: Register the route**

In `routes()`:

```rust
        .route(
            "/ui/ops/pairs/{id}/synthesize",
            post(ask_pair_synthesis_ui),
        )
```

- [ ] **Step 7: Add the button to the card**

In `src/web/templates/_decide.html`, inside the `<div class="row">`, before the Discard form:

```html
    {# The fourth answer, and the only one that ends with a new artifact rather
       than with one of these two hidden behind the other. Offered only where
       the merge path will accept it: `insert_merged_artifact` refuses a
       lineage that names stored source text, and a button whose press can only
       return a validation error is worse than no button.

       Titles out of `data-*` for the reason the Keep buttons read theirs that
       way: escaped into an attribute an apostrophe survives, escaped into a
       string literal inside an attribute it does not. #}
    {% if p.mergeable %}
    <form method="post" action="/ui/ops/pairs/{{ p.id }}/synthesize"
          data-a="{{ p.a_title }}" data-b="{{ p.b_title }}"
          onsubmit="return confirm('Write one artifact from “' + this.dataset.a + '” and “' + this.dataset.b + '”, and hide both behind it? You can undo this from Insights.')">
      <button class="btn btn-sm" type="submit">Synthese</button>
    </form>
    {% endif %}
```

and wrap the whole `<div class="row">` so a pair already asked about says so instead:

```html
  {% if p.synthesis_asked %}
  <div class="decide-finding">A synthesis was asked for; it is written on the next pass.</div>
  {% else %}
  <div class="row">
    …existing four forms…
  </div>
  {% endif %}
```

- [ ] **Step 8: Run the tests**

Run: `cargo test --lib ops`
Expected: PASS, including the existing `every_ops_button_in_the_artifact_pane_returns_to_where_it_was_pressed`, which holds the new button to the same rule.

- [ ] **Step 9: Commit**

```bash
git add src/web/ops.rs src/web/templates/_decide.html
git commit -m "$(cat <<'EOF'
feat(ui): a pair that covers the same ground can be asked to become one

The card offered Keep, Keep, Discard and Dismiss — every answer either hid one
artifact behind the other or left both standing. Two artifacts that genuinely
cover the same ground wanted a fifth thing: one artifact that says what both
said.

The press records the operator's judgement and arms a dedupe unit; the writing
is an inference call and goes on the queue, where the budget and the backoff
are. The button renders only where the merge path will accept the lineage.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DBPwtgFZnXyWPAuGTvhYsx
EOF
)"
```

---

### Task 7: the job writes the synthesis

A pair carrying the flag skips the verdict — a person has already given it — and takes a write-only prompt. From the draft on, the existing `Relation::Duplicate` tail runs unchanged.

**Files:**
- Modify: `src/infer/prompt.rs` (add `SYNTHESIZE_SYSTEM`, `synthesize_schema`, `parse_synthesis`)
- Modify: `src/jobs/dedupe.rs` (branch in `run`, before the verdict call)
- Test: `src/infer/prompt.rs` tests, `src/jobs/dedupe.rs` tests

**Interfaces:**
- Consumes: `ArtifactPair.synthesis_asked` and `clear_pair_synthesis` (Task 5); `merge::write(core, &MergedDraft, &[String]) -> Result<Chunk>`.
- Produces: `parse_synthesis(body: &str) -> Result<MergedDraft>`, `synthesize_schema() -> serde_json::Value`, `SYNTHESIZE_SYSTEM: &str`.

- [ ] **Step 1: Write the failing parser test**

In the `mod tests` of `src/infer/prompt.rs`:

```rust
    /// The operator has judged; the model only writes. There is no relation to
    /// parse and no verdict to downgrade — a reply that carries no usable text
    /// is an error, not a "distinct".
    #[test]
    fn a_synthesis_reply_parses_into_a_draft() {
        let body = r#"{"merged":{"title":"Praxis","text":"Alles zusammen.","category":"reference","caveats":[]}}"#;
        let d = parse_synthesis(body).expect("a well-formed draft parses");
        assert_eq!(d.title.as_deref(), Some("Praxis"));
        assert_eq!(d.text, "Alles zusammen.");
        assert!(d.caveats.is_empty());
    }

    #[test]
    fn a_synthesis_reply_with_no_text_is_an_error() {
        let body = r#"{"merged":{"title":"Praxis","text":"","category":"reference","caveats":[]}}"#;
        assert!(parse_synthesis(body).is_err(), "an empty body is no draft");
    }

    #[test]
    fn the_synthesis_schema_requires_every_field_it_lists() {
        let s = synthesize_schema();
        let m = &s["properties"]["merged"];
        let required = m["required"].as_array().expect("required is a list");
        for f in ["text", "title", "category", "caveats"] {
            assert!(
                required.iter().any(|r| r.as_str() == Some(f)),
                "strict rejects a listed-but-optional property; {f} must be required"
            );
        }
        assert_eq!(m["additionalProperties"], serde_json::json!(false));
    }
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test --lib synthesis`
Expected: FAIL to compile — `parse_synthesis` and `synthesize_schema` do not exist.

- [ ] **Step 3: Add the prompt, the schema and the parser**

In `src/infer/prompt.rs`, beside their dedupe counterparts:

```rust
/// Writing one artifact from two, when a person has already judged that the two
/// say the same thing.
///
/// Deliberately not `DEDUPE_SYSTEM` with a forced branch. That prompt's whole
/// body is about *deciding*, and the decision here has been made by the
/// operator who pressed the button — asking the model to decide again invites
/// it to answer "distinct" and leave the press with nothing to show for it.
/// The merge-writing rules are the same rules, because they are the same
/// requirement.
pub const SYNTHESIZE_SYSTEM: &str = r#"You are given two knowledge artifacts that a person has already judged to cover the same ground. Your job is not to decide whether they do — that is settled. Write one artifact that says everything both of them said.

The merged text must contain every number, version, date, path, flag, command and error string that appeared in either input. If two of those disagree, keep both and say which artifact each came from; dropping one is the failure this task exists to avoid.

It must read as one self-contained artifact rather than a list of sources, and it must stand on its own without them: a reader who never sees the originals must not be left with a dangling reference.

The title names the subject. A body that never says what it is about is the failure to avoid here — an artifact titled "FAT32 Specifications" may open with "32 Bit Clusternummern" and never name FAT32 again, and that title is what makes it findable.

Reply with JSON only, no commentary, in exactly this shape:

{"merged": {"title": "...", "text": "...", "category": "...", "caveats": []}}

- text: the merged artifact. Never empty.
- title: what it is about.
- category: one of the listed categories.
- caveats: the conditions under which it does not apply; an empty list when there are none."#;

/// The response format for `SYNTHESIZE_SYSTEM`.
///
/// Every property is listed and required, for the reason `dedupe_schema`
/// gives: under `strict` a listed-but-optional property is not a looser schema,
/// it is a rejected one.
pub fn synthesize_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "merged": {
                "type": "object",
                "properties": {
                    "text": {"type": "string"},
                    "title": {"type": "string"},
                    "category": {"type": "string", "enum": CATEGORIES},
                    "caveats": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["text", "title", "category", "caveats"],
                "additionalProperties": false
            }
        },
        "required": ["merged"],
        "additionalProperties": false
    })
}

/// One synthesis, parsed. Unlike `parse_dedupe` there is no salvage and no
/// downgrade: the judgement was a person's, so a reply that carries no text is
/// a failed call rather than a different answer.
pub fn parse_synthesis(body: &str) -> Result<MergedDraft> {
    #[derive(serde::Deserialize)]
    struct Raw {
        merged: RawMerged,
    }
    #[derive(serde::Deserialize)]
    struct RawMerged {
        text: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        category: Option<String>,
        #[serde(default)]
        caveats: Vec<String>,
    }
    let raw: Raw = serde_json::from_str(strip_fences(body))
        .map_err(|e| Error::Validation(format!("a synthesis reply that could not be read: {e}")))?;
    if raw.merged.text.trim().is_empty() {
        return Err(Error::Validation("a synthesis with no text".into()));
    }
    Ok(MergedDraft {
        title: raw.merged.title,
        text: raw.merged.text,
        category: raw.merged.category,
        tags: Vec::new(),
        caveats: raw.merged.caveats,
    })
}
```

Use whatever `parse_dedupe` uses to strip code fences and to build its error — read it first and match it exactly rather than inventing `strip_fences`.

- [ ] **Step 4: Run the parser tests**

Run: `cargo test --lib synthesis`
Expected: PASS.

- [ ] **Step 5: Write the failing job test**

In the `mod tests` of `src/jobs/dedupe.rs`:

```rust
    /// A person pressed Synthese. The verdict prompt is not asked — the
    /// judgement is theirs — and the artifact is written from the write-only
    /// prompt instead.
    #[tokio::test]
    async fn a_pair_a_person_asked_about_is_written_without_being_judged_again() {
        let core = test_core().await;
        let ids = seed(
            &core,
            &[
                ("the clinic opens at 08:00", [0.99, 0.01]),
                ("the clinic treats small animals", [0.99, 0.02]),
            ],
        )
        .await;
        core.store.record_pair(&ids[0], &ids[1], 0.88).await.unwrap();
        let pid = core
            .store
            .pairs_by_state(PairState::Pending, 10)
            .await
            .unwrap()[0]
            .id;
        core.store.ask_pair_synthesis(pid).await.unwrap();

        run(&core).await.unwrap();

        let p = core.store.get_pair(pid).await.unwrap();
        assert!(p.merged_into.is_some(), "a merged artifact answered the pair");
        let m = core
            .store
            .get_artifact(p.merged_into.as_deref().unwrap())
            .await
            .unwrap();
        assert_eq!(m.provenance, Provenance::Merged);
        for id in &ids {
            let side = core.store.get_artifact(id).await.unwrap();
            assert!(!side.in_results(), "both sources are behind the synthesis");
        }
    }

    /// The merge path still refuses a lineage naming stored source text. When
    /// it does, the flag is cleared so the card stops promising a synthesis.
    #[tokio::test]
    async fn a_refused_synthesis_clears_the_ask() {
        let core = test_core().await;
        let ids = seed_passages(&core, 2).await;
        core.store.record_pair(&ids[0], &ids[1], 0.88).await.unwrap();
        let pid = core
            .store
            .pairs_by_state(PairState::Pending, 10)
            .await
            .unwrap()[0]
            .id;
        core.store.ask_pair_synthesis(pid).await.unwrap();

        run(&core).await.unwrap();

        let p = core.store.get_pair(pid).await.unwrap();
        assert!(p.merged_into.is_none());
        assert!(!p.synthesis_asked, "the card stops promising what will not come");
    }
```

`seed_passages` is a stand-in — build passages the way the file's existing tests build them, or set `provenance` directly after seeding.

- [ ] **Step 6: Run and watch them fail**

Run: `cargo test --lib a_pair_a_person_asked_about_is_written_without_being_judged_again`
Expected: FAIL — the pair takes the ordinary verdict path.

- [ ] **Step 7: Branch in `dedupe::run`**

In `src/jobs/dedupe.rs`, after the lineage refusal at 185-187 and before the prompt is built, add the branch. Keep the lineage check above it: a person asking for a synthesis does not make a passage mergeable, and the refusal path clears the flag.

```rust
    // A person has already judged this pair. Asking the verdict prompt would
    // invite the model to overturn them and answer "distinct", which leaves
    // the press with nothing to show for it — so the judgement is skipped and
    // only the writing is asked for.
    if p.synthesis_asked {
        let Some(judge) = core.judge.clone() else {
            return Ok(());
        };
        let user = synthesis_prompt(&members);
        let reply = judge
            .ask(crate::infer::prompt::SYNTHESIZE_SYSTEM, &user, Some(("synthesis", crate::infer::prompt::synthesize_schema())))
            .await?;
        let draft = crate::infer::prompt::parse_synthesis(&reply)?;
        let sources: Vec<String> = members.iter().map(|m| m.id.clone()).collect();
        return match crate::jobs::merge::write(core, &draft, &sources).await {
            Ok(m) => {
                let act = format!("merged into {} from {} sources", m.id, sources.len());
                for source in &sources {
                    core.store
                        .record_action(&action(
                            Kind::Merge,
                            &p,
                            source,
                            Some(&m.id),
                            Some(act.as_str()),
                        ))
                        .await?;
                }
                core.store
                    .set_pair_merged(p.id, &m.id, Some("synthesized at an operator's request"), DecidedBy::Operator)
                    .await
            }
            Err(Error::Validation(why)) => {
                tracing::warn!(pair = p.id, reason = %why, "the merge path refused an operator's synthesis");
                core.store.clear_pair_synthesis(p.id).await?;
                settle(
                    core,
                    &p,
                    PairState::Contradiction,
                    Some(
                        "These could not be merged: the merge was refused because of \
                         what one of them is made of. Resolve by hand.",
                    ),
                )
                .await
            }
            Err(e) => Err(e),
        };
    }
```

Match `judge.ask(...)` to the real trait method the verdict path uses further down the same function — read it and copy the call shape, including how it passes the schema and how it counts attempts.

Add `synthesis_prompt(&members)` beside `build_prompt` in the same file, rendering the two artifacts under their titles exactly as `dedupe_prompt` does, without the letters (nothing is named in a reply here) and without the sources block.

- [ ] **Step 8: Run the job tests**

Run: `cargo test --lib dedupe`
Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add src/infer/prompt.rs src/jobs/dedupe.rs
git commit -m "$(cat <<'EOF'
feat(dedupe): an operator's synthesis is written, not judged again

A pair somebody pressed Synthese on skips the verdict prompt. The judgement was
theirs, and asking the model to make it again invites a "distinct" that leaves
the press with nothing to show for it — so only the writing is asked for.

The lineage refusal still stands above it: a person asking does not make stored
source text mergeable. When the merge path refuses, the ask is cleared so the
card stops promising a synthesis that will not arrive.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DBPwtgFZnXyWPAuGTvhYsx
EOF
)"
```

---

### Task 8: the production base — **gated on explicit go-ahead**

Two one-off cleanups against `~/engram/data/users/b92f3671f8808fa0.db`. **Do not run any part of this task until the user says so in this session.** Nothing here is part of the code change, and the code tasks are complete and shippable without it.

**Files:** none in the tree. A script under the session scratchpad.

- [ ] **Step 1: Ask, and wait**

State what will change: 24 pairs move to `dismissed`, and one corpus is marked a near-duplicate of another. Nothing is deleted. Wait for an explicit yes.

- [ ] **Step 2: Back up first**

```bash
ssh svc-engram@engram.mikoshi.cc \
  'python3 -c "import sqlite3,time; s=sqlite3.connect(\"/home/svc-engram/engram/data/users/b92f3671f8808fa0.db\"); d=sqlite3.connect(\"/home/svc-engram/backup-%s.db\"%int(time.time())); s.backup(d); print(\"ok\")"'
```

- [ ] **Step 3: Dismiss the 24 interference pairs**

Only rows whose detail is the mechanical refusal and whose `judge_attempts` is 0 — never a pair a judge actually ruled on.

```sql
UPDATE artifact_pairs
   SET state = 'dismissed',
       detail = 'filed by interference, which measured retrieval competition rather than meaning; nothing judged these'
 WHERE state = 'contradiction'
   AND judge_attempts = 0;
```

Verify the count is 24 before committing the transaction, and stop if it is not.

- [ ] **Step 4: Resolve the heise duplicate**

The two corpora are `01a04547-ca17-72f1-a56c-d8ace9fc51f9` (5282 chars, `web`, no `source_url`) and `01a073f3-3438-73d2-be9f-0faf21420229` (5650 chars, `journal`, carries the `source_url`). Keep the journal capture — it is longer and it knows where it came from.

Prefer the existing route over hand-written SQL: `POST /ui/ops/corpora/{id}/resolve` (`resolve_near_dupe_ui`) already does this and journals it. Read that handler first; it may require `near_dupe_of` to be set before it will act, in which case set it to the observed 0.789 and then press the route.

- [ ] **Step 5: Confirm and report**

Re-run the survey queries: no `contradiction` pairs awaiting review, no pair carrying a `0.0` score that is not the link judge's, and one corpus resolved. Report the before and after counts.

---

## Self-Review

**Spec coverage.** A → Task 1. B → Task 1, Step 7 (no code, asserted). C → Task 8. D → Task 4. E → Tasks 5, 6, 7. F1 → Task 2. F2 → Task 3. F3 is explicitly out of scope in the spec and has no task, correctly.

**Placeholder scan.** Three fixture helpers are named as stand-ins rather than shown: `seed_two_artifacts` (Task 5), `seed_mergeable_pair` / `seed_pair_of_passages` / `count_artifacts` (Task 6), `seed_passages` (Task 7). Each is flagged in its step with instructions to use the module's existing helper. This is deliberate — inventing helper signatures that do not match the file would be worse than naming the gap — but the implementer must read the surrounding tests first in those three tasks.

Two call shapes are likewise flagged rather than guessed: `judge.ask(...)` in Task 7 Step 7, and the redirect helper in Task 6 Step 5. Both say to read the neighbouring code and copy it.

**Type consistency.** `synthesis_asked` is a `bool` on `ArtifactPair` (Task 5), read as `p.synthesis_asked` in `pair_rows` (Task 6) and in `dedupe::run` (Task 7). `ask_pair_synthesis(i64)` and `clear_pair_synthesis(i64)` take the pair id in all three tasks. `parse_synthesis` returns `MergedDraft`, which is what `merge::write(core, &MergedDraft, &[String])` takes — matching the real signature at `src/jobs/merge.rs:34`. `mergeable` and `synthesis_asked` are both added to `PairRow` in Task 6 Step 3 and both used in the template in Step 7.

**One risk carried forward.** Task 3 can fail at its first step, and that is a real outcome rather than a setback: if the endpoint will not honour a reordered union, the schema half is dropped, the prompt half ships alone, and F3 needs its own plan. Nothing downstream of Task 3 depends on it.
