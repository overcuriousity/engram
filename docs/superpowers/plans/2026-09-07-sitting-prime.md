# `[sitting] prime` on the ladder — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `[sitting] prime` a knob the idle pass can measure and adopt on its own, by recording the sitting whether or not the knob is on and putting the flag on the ranking ladder.

**Architecture:** The knob moves out of `Core.sitting` and into `RankingParams`/`GenerationParams`, so a generation can carry it and a replay can ask the counterfactual. `Priming.sitting` becomes unconditional — the knob gates the *use* of the sitting, never its collection — and `prime()` takes the flag so serving is byte-for-byte unchanged while it is off. The sweep gains a two-rung axis offered only where `prime_lift > 0`, because below that the flip is a guaranteed tie.

**Tech Stack:** Rust, sqlx/SQLite, `toml_edit`, inline `#[cfg(test)] mod tests`, `#[tokio::test]`.

**Spec:** `docs/superpowers/specs/2026-09-07-sitting-prime-design.md`

## Global Constraints

- **Serving must not change while the knob is off.** The `in_sitting` badge and the lift are both gated on the flag. A test proves the badge is still absent at `sitting_prime = false`.
- **Old rows must decode.** Every new `GenerationParams` field carries `#[serde(default = "…")]` pointing at the shipped value, as the stage-3a knobs do. No migration.
- **One knob per candidate.** `sweep::moved()` must count the new field, and `every_candidate_moves_at_most_one_knob` must keep passing.
- **The sitting axis is never offered at `prime_lift == 0`.** A tie is never adopted, never becomes a generation, and so never reaches `tried_candidates` (which holds only `reverted` and `refused`) — an unguarded axis would be re-measured every quiet period forever.
- **House style.** Comments are full sentences explaining *why*, in the voice of the surrounding code. Run `cargo fmt` before every commit.
- Verify with `cargo test --lib` and `cargo clippy --all-targets -- -D warnings`.

---

### Task 1: `sitting_prime` becomes a ranking parameter

Pure plumbing. The field exists, round-trips, and is written back to the file — with no behaviour attached yet.

**Files:**
- Modify: `src/config.rs` (new default fn near `default_prime_lift` at :1045; `write_ranking` at :2075-2101; `ranking_keys_in_env` at :2114-2133)
- Modify: `src/core/ranking.rs:34-101` (`RankingParams`, `Default`, `from_config`) and its tests at :124-240
- Modify: `src/store/generations.rs:19-58` (`GenerationParams`, `From<RankingParams>`)
- Modify: `src/core/mod.rs:563-568` (the one production `from_config` call)

**Interfaces:**
- Consumes: nothing.
- Produces: `crate::config::default_sitting_prime() -> bool`; `RankingParams.sitting_prime: bool`; `GenerationParams.sitting_prime: bool`; `RankingParams::from_config(&VectorConfig, &AssociateConfig, &ConsolidateConfig, &SittingConfig, bool) -> Self` — note the new fourth positional argument, **before** `reranker_configured`.

- [ ] **Step 1: Write the failing tests**

In `src/core/ranking.rs`, inside `mod tests`:

```rust
    #[test]
    fn the_sitting_flag_is_read_from_the_file_and_ships_off() {
        let sitting = crate::config::SittingConfig { prime: true };
        let p = RankingParams::from_config(
            &vector_config(3),
            &Default::default(),
            &Default::default(),
            &sitting,
            false,
        );
        assert!(p.sitting_prime, "the file's value is the starting rung");
        assert!(
            !RankingParams::default().sitting_prime,
            "and the shipped rung is off"
        );
    }
```

In `src/store/generations.rs`, inside `mod tests`:

```rust
    #[test]
    fn a_generation_written_before_the_sitting_knob_decodes_as_off() {
        // Rows predate the field. They ran under the shipped value, and the
        // serde default is how that is said without a migration.
        let raw = r#"{"recency_weight":0.05,"per_source_cap":3,
                      "candidate_multiplier":3,"recency_half_life_days":180,
                      "prime_lift":0,"spread_max":3,"rerank":false,
                      "review_min":0.88}"#;
        let p: GenerationParams = serde_json::from_str(raw).unwrap();
        assert!(!p.sitting_prime);
    }

    #[test]
    fn the_sitting_flag_survives_a_round_trip() {
        let p = GenerationParams {
            sitting_prime: true,
            ..Default::default()
        };
        let back: GenerationParams = serde_json::from_str(&serde_json::to_string(&p).unwrap())
            .unwrap();
        assert_eq!(back, p);
    }
```

In `src/config.rs`, inside `mod tests`, beside the existing `write_ranking` test near :3269:

```rust
    #[test]
    fn writing_a_generation_back_names_the_sitting_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[sitting]\nprime = false\n").unwrap();
        let p = crate::core::ranking::RankingParams {
            sitting_prime: true,
            ..Default::default()
        };
        write_ranking(&path, &p).unwrap();
        let out = std::fs::read_to_string(&path).unwrap();
        assert!(out.contains("prime = true"), "{out}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib the_sitting_flag_is_read_from_the_file_and_ships_off a_generation_written_before_the_sitting_knob_decodes_as_off`
Expected: FAIL to compile — `no field \`sitting_prime\``, `default_sitting_prime` not found.

- [ ] **Step 3: Add the shipped default**

In `src/config.rs`, immediately after `default_prime_lift` (:1045-1049):

```rust
/// The shipped `sitting.prime`: off. Read by the generation shapes so a row
/// written before the knob was on the ladder decodes as what it ran under.
pub(crate) fn default_sitting_prime() -> bool {
    false
}
```

- [ ] **Step 4: Add the field to `RankingParams`**

In `src/core/ranking.rs`, after the `prime_lift` field (:48):

```rust
    /// Whether what this sitting has already touched may take part in the
    /// lift. It shares `prime_lift`'s budget rather than having one of its
    /// own, so at a lift of zero this changes nothing.
    pub sitting_prime: bool,
```

In `impl Default` (after :69):

```rust
            sitting_prime: crate::config::default_sitting_prime(),
```

Change `from_config` (:81-101) to take the sitting configuration and read it:

```rust
    pub fn from_config(
        cfg: &VectorConfig,
        associate: &crate::config::AssociateConfig,
        consolidate: &crate::config::ConsolidateConfig,
        sitting: &crate::config::SittingConfig,
        reranker_configured: bool,
    ) -> Self {
```

and, beside `prime_lift: associate.prime_lift` (:98):

```rust
            sitting_prime: sitting.prime,
```

- [ ] **Step 5: Add the field to `GenerationParams`**

In `src/store/generations.rs`, after the `prime_lift` field (:31):

```rust
    /// On the ladder later than the other three, so a row from before it
    /// decodes as the shipped `false`.
    #[serde(default = "crate::config::default_sitting_prime")]
    pub sitting_prime: bool,
```

and in `From<RankingParams>` (after :53):

```rust
            sitting_prime: p.sitting_prime,
```

If the file also has a `From<GenerationParams> for RankingParams` or a `hydrate` that constructs `RankingParams`, add the field there too — grep the file for `prime_lift` and mirror every site.

- [ ] **Step 6: Update the production and test call sites of `from_config`**

`src/core/mod.rs:563-568` becomes:

```rust
                crate::core::ranking::RankingParams::from_config(
                    &cfg.vector,
                    &cfg.associate,
                    &cfg.consolidate,
                    &cfg.sitting,
                    cfg.infer.rerank.is_some(),
                ),
```

Then insert `&Default::default(),` before the trailing bool at each existing test call in `src/core/ranking.rs`: lines 126, 148, 153, 179, 200 and 210. Compile errors will name any site this list misses.

- [ ] **Step 6b: Correct the count in the `GenerationParams` doc comment**

`src/store/generations.rs:14` calls these "the seven the idle pass may move".
It was already wrong — there are eight — and this makes it nine. Say the
number once or not at all:

```rust
/// The knobs a generation holds: everything the idle pass may move. Stored as
/// JSON so the set can widen without a migration — the retrieval knobs and the
/// sitting flip all arrived after the first rows were written, and those rows
/// still read.
```

- [ ] **Step 6c: Confirm the learn modes still shut the knob off**

`apply_learn_mode` (`src/config.rs:2329`) resolves `sitting.prime` to `false`
under `off` and `learning`, and it runs on the `Config` before
`RankingParams::from_config` reads it — so no generation can reintroduce the
knob from underneath. Two existing tests already assert this
(`src/config.rs:3546` and `:3590`). No new test; confirm they still pass:

Run: `cargo test --lib learn_mode`
Expected: PASS, with `assert!(!cfg.sitting.prime)` holding in both.

- [ ] **Step 7: Write the key back, and declare it shadowable**

In `src/config.rs::write_ranking`, after the `spread_max` line (:2096):

```rust
    doc["sitting"]["prime"] = toml_edit::value(p.sitting_prime);
```

In `ranking_keys_in_env`'s `matches!` arm (:2123-2130), after `"ENGRAM__ASSOCIATE__SPREAD_MAX"`:

```rust
                    | "ENGRAM__SITTING__PRIME"
```

- [ ] **Step 8: Run the tests to verify they pass**

Run: `cargo test --lib`
Expected: PASS, whole suite green. Nothing observable has changed yet — `search.rs` still reads `self.sitting.prime`.

- [ ] **Step 9: Format, lint and commit**

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
git add src/config.rs src/core/ranking.rs src/core/mod.rs src/store/generations.rs
git commit -m "feat(ranking): sitting.prime becomes a ranking parameter

A generation could not express the knob, so no sweep could ever offer
one. The field joins RankingParams and GenerationParams with the serde
default the stage-3a knobs use, and write_ranking names the key an
operator would have typed. Nothing reads it yet."
```

---

### Task 2: the sitting is recorded whether or not it is used

The defect itself. After this task the evidence accumulates from every search, and serving is unchanged while the knob is off.

**Files:**
- Modify: `src/core/search.rs:685-700` (`prime` signature and the badge), `:1699-1746` (the gate and the call), and the `prime` tests at :3400-3560
- Modify: `src/core/mod.rs:253-254, :585, :906` (remove the now-dead `Core.sitting` field)

**Interfaces:**
- Consumes: `RankingParams.sitting_prime` from Task 1.
- Produces: `fn prime(results, activation, margin, lift, sitting, sitting_prime: bool, due) -> Vec<SearchResult>` — the new `sitting_prime` argument sits **between** `sitting` and `due`.

- [ ] **Step 1: Write the failing tests**

In `src/core/search.rs`, inside the `prime` tests module:

```rust
    #[test]
    fn the_sitting_moves_nothing_and_badges_nothing_while_the_knob_is_off() {
        // The whole promise of recording the sitting unconditionally: what
        // serving does must not change until the loop adopts the knob.
        let sitting = std::collections::HashSet::from(["d".to_string()]);
        let out = prime(
            ranked(&["a", "b", "c", "d"]),
            &HashMap::new(),
            0.5,
            2,
            &sitting,
            false,
            &Default::default(),
        );
        assert_eq!(order(&out), vec!["a", "b", "c", "d"], "nothing moved");
        assert!(
            out.iter().all(|r| !r.in_sitting),
            "and nothing is badged either"
        );
    }

    #[test]
    fn the_sitting_lifts_and_badges_once_the_knob_is_on() {
        let sitting = std::collections::HashSet::from(["d".to_string()]);
        let out = prime(
            ranked(&["a", "b", "c", "d"]),
            &HashMap::new(),
            0.5,
            2,
            &sitting,
            true,
            &Default::default(),
        );
        assert_eq!(order(&out), vec!["a", "d", "b", "c"]);
        assert!(out[1].primed && out[1].in_sitting);
    }
```

And, in the async part of `src/core/search.rs`'s tests, the regression test for the defect:

```rust
    #[tokio::test]
    async fn a_search_records_the_sitting_even_though_the_knob_is_off() {
        // The defect this whole change exists to fix: the sitting used to be
        // collected only when the knob was already on, so no evidence ever
        // accumulated and the idle pass could never measure it. The knob
        // gates the use of the sitting, never its collection.
        let mut core = test_core().await;
        core.learn.enabled = true;
        seed_from(&core, "one", &[("alpha text", "note", &[])]).await;
        reembed_all(&core).await;
        let a = id_of(&core, "alpha text").await;

        assert!(
            !core.ranking.read().unwrap().sitting_prime,
            "the fixture must run with the knob off, or this proves nothing"
        );
        core.sittings
            .touched("sess", &a, now_secs(), core.pursuit.idle_secs as i64);

        let (_, outcome) = core
            .search_with(
                &q("alpha text"),
                None,
                Door::Ui.in_sitting(Some("sess".to_string())),
            )
            .await
            .unwrap();
        core.background.wait_idle().await;

        let event = outcome.event.expect("the UI door waits for its capture");
        let ctx = core
            .store
            .search_context(&event)
            .await
            .unwrap()
            .expect("a priming search records what priming read");
        assert!(
            ctx.sitting.contains(&a),
            "the touched artifact must be in the recorded sitting: {:?}",
            ctx.sitting
        );
    }
```

And the door with nothing to say, which must stay silent either way:

```rust
    #[tokio::test]
    async fn a_door_with_no_session_records_an_empty_sitting() {
        // An access token is not a conversation. `Origin::session` is `None`
        // everywhere but the web door, and collecting the sitting
        // unconditionally must not change that.
        let mut core = test_core().await;
        core.learn.enabled = true;
        seed_from(&core, "one", &[("alpha text", "note", &[])]).await;
        reembed_all(&core).await;
        let a = id_of(&core, "alpha text").await;
        core.sittings
            .touched("sess", &a, now_secs(), core.pursuit.idle_secs as i64);

        // The same live sitting exists; this search simply does not belong to
        // it, because the door cannot name one.
        let (_, outcome) = core.search_with(&q("alpha text"), None, Door::Ui).await.unwrap();
        core.background.wait_idle().await;

        let event = outcome.event.expect("the UI door waits for its capture");
        let ctx = core.store.search_context(&event).await.unwrap().unwrap();
        assert!(
            ctx.sitting.is_empty(),
            "a search with no session names no sitting: {:?}",
            ctx.sitting
        );
    }
```

> `q`, `seed_from`, `reembed_all`, `id_of` and `test_core` are the helpers the surrounding tests already use; `Door::Ui.in_sitting(..)` is `Origin::in_sitting` at `src/store/feedback.rs:164`. If `search_with`'s `cap` argument needs a value other than `None` for the fixture to return the artifact, pass `Some(3)`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib the_sitting_moves_nothing_and_badges_nothing_while_the_knob_is_off a_search_records_the_sitting_even_though_the_knob_is_off a_door_with_no_session_records_an_empty_sitting`
Expected: FAIL — the two `prime` tests fail to compile (`prime` takes 6 arguments, not 7), and `a_search_records_the_sitting_even_though_the_knob_is_off` fails on its assertion because `ctx.sitting` is empty. `a_door_with_no_session_records_an_empty_sitting` passes already; it is the guard that the fix does not overreach.

- [ ] **Step 3: Gate `prime` on the flag rather than on an empty set**

In `src/core/search.rs`, change the signature (:685-693) and the top of the body:

```rust
fn prime(
    mut results: Vec<SearchResult>,
    activation: &HashMap<String, f64>,
    margin: f64,
    lift: usize,
    sitting: &std::collections::HashSet<String>,
    sitting_prime: bool,
    due: &std::collections::HashSet<String>,
) -> Vec<SearchResult> {
    // The knob gates the *use* of the sitting, never its collection: the
    // `Priming` this search records names what the session touched either
    // way, which is the whole of what lets the idle pass replay this search
    // with the sitting on and find out whether it should be. Held to the
    // badge as well as the lift, so serving is unchanged until the loop
    // adopts it.
    let empty = std::collections::HashSet::new();
    let sitting = match sitting_prime {
        true => sitting,
        false => &empty,
    };
    // Marked before anything can return. `in_sitting` is a fact about the row —
```

The rest of the body is untouched: every later use of `sitting` now reads the shadowed binding.

- [ ] **Step 4: Collect the sitting unconditionally**

In `src/core/search.rs`, replace the gate at :1704-1721 with:

```rust
            // Collected whatever the knob says, and used only where it says
            // so — see `prime`. Recording this is what makes the knob
            // measurable at all: gated on itself, it produced no evidence,
            // so the idle pass could never move it. Empty on every door with
            // no session, for the reason in `Origin::session`.
            let sitting: std::collections::HashSet<String> = origin
                .session
                .as_deref()
                .map(|s| {
                    self.sittings
                        .read(s, now_secs(), self.pursuit.idle_secs as i64)
                        .touched
                        .into_iter()
                        .collect()
                })
                .unwrap_or_default();
```

and pass the flag at the `prime(...)` call (:1734-1741):

```rust
            results = prime(
                results,
                &priming.activation,
                self.associate.prime_margin,
                params.prime_lift,
                &priming.sitting,
                params.sitting_prime,
                &priming.due,
            );
```

- [ ] **Step 5: Remove the now-dead `Core.sitting`**

The knob lives in `core.ranking` now; a second copy on `Core` that nothing reads is a lie about where it lives. Delete all three lines:

- `src/core/mod.rs:253-254` — the doc comment and `pub sitting: crate::config::SittingConfig,`
- `src/core/mod.rs:585` — `sitting: cfg.sitting.clone(),`
- `src/core/mod.rs:906` — `sitting: crate::config::SittingConfig::default(),`

`crate::config::SittingConfig` itself stays: it is the file's shape, and `RankingParams::from_config` reads it.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --lib`
Expected: PASS. Pay attention to the pre-existing `prime` tests — they gain a `false` or `true` argument and their assertions must not change. Any that passed a non-empty sitting and expected a lift now need `true`.

- [ ] **Step 7: Format, lint and commit**

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
git add src/core/search.rs src/core/mod.rs
git commit -m "fix(search): record the sitting whether or not priming uses it

The sitting membership was collected only when sitting.prime was already
on, so the knob could produce no evidence about itself, so the idle pass
could never measure it, so it stayed off forever. Its two siblings in the
same Priming do not behave this way: activation is unconditional and due
is gated on a knob that ships on.

The knob now gates the use of the sitting rather than its collection, and
prime() holds it to the badge as well as the lift, so serving is
unchanged while it is off."
```

---

### Task 3: a two-rung axis, offered only where it can do anything

**Files:**
- Modify: `src/core/ranking.rs:24-32` (the ladder constants)
- Modify: `src/eval/sweep.rs:71-155` (`candidates`), `:224-233` (`moved`), and the chooser tests at :1648-1700
- Modify: `src/jobs/tune.rs:60` (`BUDGET`)

**Interfaces:**
- Consumes: `RankingParams.sitting_prime` from Task 1.
- Produces: `crate::core::ranking::SITTING_PRIMES: [bool; 2]`; `tune::BUDGET` becomes `20`.

- [ ] **Step 1: Write the failing tests**

In `src/eval/sweep.rs`, inside `mod tests`:

```rust
    #[test]
    fn the_sitting_flip_is_not_offered_where_it_can_do_nothing() {
        // At a lift of zero the flip is a guaranteed tie — `prime` returns
        // early — and a tie is never adopted, never becomes a generation, and
        // so never reaches `tried_candidates`, which holds only the reverted
        // and the refused. Offered here it would be re-measured every quiet
        // period forever, at one rank per pair, to prove something arithmetic
        // already proves.
        let current = RankingParams::default();
        assert_eq!(current.prime_lift, 0, "the shipped rung");
        let grid = candidates(current, &[], crate::jobs::tune::BUDGET);
        assert!(
            grid.iter().all(|c| !c.sitting_prime),
            "no sitting flip at a lift of zero"
        );
    }

    #[test]
    fn the_sitting_flip_is_offered_once_a_lift_has_been_adopted() {
        let current = RankingParams {
            prime_lift: 2,
            ..RankingParams::default()
        };
        let grid = candidates(current, &[], crate::jobs::tune::BUDGET);
        let flips: Vec<bool> = grid
            .iter()
            .map(|c| c.sitting_prime)
            .filter(|s| *s != current.sitting_prime)
            .collect();
        assert_eq!(flips, vec![true], "exactly one flip, and it is the other rung");
    }
```

Replace the existing `the_pass_budget_covers_every_rung_on_every_axis` (:1670-1681) with a version that states both cases:

```rust
    #[test]
    fn the_pass_budget_covers_every_rung_on_every_axis() {
        // A tie keeps the current value, so an improvement two rungs out
        // behind a rung that ties would never be reached by a pass that only
        // tried the nearest step. The budget has to reach the whole ladder —
        // including the sitting flip, which only exists above a zero lift.
        let at_zero = candidates(RankingParams::default(), &[], usize::MAX);
        assert_eq!(at_zero.len(), crate::jobs::tune::BUDGET - 1, "{at_zero:?}");
        let lifted = candidates(
            RankingParams {
                prime_lift: 2,
                ..RankingParams::default()
            },
            &[],
            usize::MAX,
        );
        assert_eq!(lifted.len(), crate::jobs::tune::BUDGET, "{lifted:?}");
    }
```

The one-knob invariant is deliberately *not* re-asserted here: a lifted
candidate differs from `RankingParams::default()` on two knobs by
construction, so checking it against that baseline would be either wrong or
vacuous. It belongs in `every_candidate_moves_at_most_one_knob`, against each
grid's own baseline, which the next edit extends.

and extend `every_candidate_moves_at_most_one_knob` (:1649-1668) so it also checks the lifted grid against its own baseline:

```rust
        let lifted_base = RankingParams {
            prime_lift: 2,
            ..RankingParams::default()
        };
        for c in &candidates(lifted_base, &[], 64) {
            assert!(moved(*c, lifted_base) <= 1, "{c:?}");
        }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib the_sitting_flip_is_not_offered_where_it_can_do_nothing the_sitting_flip_is_offered_once_a_lift_has_been_adopted the_pass_budget_covers_every_rung_on_every_axis`
Expected: FAIL — no flip is ever produced, and the budget assertions are off by one.

- [ ] **Step 3: Add the ladder constant**

In `src/core/ranking.rs`, after `PRIME_LIFTS` (:26):

```rust
/// The rungs for `sitting_prime`. Two, because it is a switch — the ladder
/// shape is kept so the chooser treats it like every other axis.
pub const SITTING_PRIMES: [bool; 2] = [false, true];
```

- [ ] **Step 4: Offer the axis in `candidates`**

In `src/eval/sweep.rs`, import it at :76 and build the axis after `lifts` (:97-101):

```rust
    // The sitting shares the lift's budget, so below a non-zero lift the flip
    // is a guaranteed tie and offering it would burn a rank per pair every
    // quiet period, forever, on a question arithmetic already answers. Not
    // offered where it can do nothing, the way `rerank` is not offered where
    // no reranker is configured. The practical effect is an order: the lift
    // ladder is walked first, and the sitting is asked about only once there
    // is a budget for it to share.
    let sittings: Vec<bool> = match current.prime_lift > 0 {
        true => SITTING_PRIMES
            .iter()
            .copied()
            .filter(|s| *s != current.sitting_prime)
            .collect(),
        false => vec![],
    };
```

Add `sittings.len()` to the `longest` array (:105-112), and push inside the interleaving loop, after the `prime_lift` arm (:140-145):

```rust
        if let Some(sitting_prime) = sittings.get(i) {
            out.push(RankingParams {
                sitting_prime: *sitting_prime,
                ..current
            });
        }
```

- [ ] **Step 5: Count the knob, and widen the budget**

In `src/eval/sweep.rs::moved` (:224-233), add a term:

```rust
        + usize::from(cand.sitting_prime != current.sitting_prime)
```

In `src/jobs/tune.rs:60`:

```rust
/// The widest grid the chooser can build: every rung of every ladder, plus
/// the sitting flip, which exists only above a zero lift. A cap, so a pass at
/// the shipped lift simply builds one fewer.
pub(crate) const BUDGET: usize = 20;
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --lib`
Expected: PASS, whole suite green.

- [ ] **Step 7: Format, lint and commit**

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
git add src/core/ranking.rs src/eval/sweep.rs src/jobs/tune.rs
git commit -m "feat(tune): the sitting flip joins the chooser's ladders

Two rungs, offered only where prime_lift is above zero: the sitting shares
the lift's budget, so below that the flip is a guaranteed tie, and a tie is
never adopted and so never reaches tried_candidates — the pass would offer
it again every quiet period forever. rerank is held off the same way where
no reranker is configured."
```

---

### Task 4: the counterfactual works end to end

The point of the whole change, asserted once: a recorded sitting, replayed, moves a rank.

**Files:**
- Modify: `src/eval/sweep.rs` (tests only)

**Interfaces:**
- Consumes: `sweep::rank_of(&Core, &Pair, RankingParams, bool)` at `:302`; `Pair { query, satisfies, query_vec, priming, served_rank }` at `:239`; `crate::core::search::Priming { activation, sitting, due }`; `test_support::seeded()` at `:855`.

- [ ] **Step 1: Write the failing test**

In `src/eval/sweep.rs`, inside `mod tests`:

```rust
    #[tokio::test]
    async fn a_recorded_sitting_replays_and_moves_a_rank() {
        // The whole point: evidence gathered while the knob was off is enough
        // to ask what the knob would have done. Honest as a counterfactual
        // precisely because the searcher saw the unprimed order — the sitting
        // influenced nothing about the list this replays.
        let (core, order) = test_support::seeded().await;
        let buried = order.last().expect("the fixture ranks several").clone();

        let pair = Pair {
            query: test_support::QUERY.to_string(),
            satisfies: vec![buried.clone()],
            query_vec: None,
            priming: Some(crate::core::search::Priming {
                activation: Default::default(),
                sitting: std::collections::HashSet::from([buried]),
                due: Default::default(),
            }),
            served_rank: None,
        };

        let off = RankingParams {
            prime_lift: 2,
            sitting_prime: false,
            ..*core.ranking.read().unwrap()
        };
        let on = RankingParams {
            sitting_prime: true,
            ..off
        };

        let before = rank_of(&core, &pair, off, false).await.unwrap();
        let after = rank_of(&core, &pair, on, false).await.unwrap();
        assert!(
            after < before,
            "the sitting must lift the artifact it names: {before:?} -> {after:?}"
        );
    }

    #[tokio::test]
    async fn a_recorded_sitting_ties_across_the_flip_at_a_zero_lift() {
        // Why the axis is not offered there: nothing to measure.
        let (core, order) = test_support::seeded().await;
        let buried = order.last().unwrap().clone();
        let pair = Pair {
            query: test_support::QUERY.to_string(),
            satisfies: vec![buried.clone()],
            query_vec: None,
            priming: Some(crate::core::search::Priming {
                activation: Default::default(),
                sitting: std::collections::HashSet::from([buried]),
                due: Default::default(),
            }),
            served_rank: None,
        };
        let off = RankingParams {
            prime_lift: 0,
            sitting_prime: false,
            ..*core.ranking.read().unwrap()
        };
        let on = RankingParams {
            sitting_prime: true,
            ..off
        };
        assert_eq!(
            rank_of(&core, &pair, off, false).await.unwrap(),
            rank_of(&core, &pair, on, false).await.unwrap()
        );
    }
```

> `rank_of` returns `Option<usize>`, so `after < before` compares `Option`s — `None` sorts below `Some`, which would pass vacuously if the artifact fell out of the list entirely. Assert `after.is_some()` first if the fixture makes that reachable. The fixture's six chunks all sit inside `LIMIT`, so it should not be.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib a_recorded_sitting_replays_and_moves_a_rank a_recorded_sitting_ties_across_the_flip_at_a_zero_lift`
Expected: the tie test PASSES already; the lift test FAILS if anything in Tasks 1–3 was wired wrong. Both passing on the first run is the expected, correct outcome — this task asserts behaviour the earlier tasks built rather than adding any.

- [ ] **Step 3: If the lift test fails, fix the wiring**

The likely causes, in order: `prime()` is not receiving `params.sitting_prime`; `search_inner` still reads a removed `self.sitting`; the replay path at `search.rs:1700` is not entered because `origin.replay` is `None` (check `rank_of` at `:327-330` sets `primed_as`). Do not weaken the test.

- [ ] **Step 4: Format, lint and commit**

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
git add src/eval/sweep.rs
git commit -m "test(sweep): a recorded sitting replays and moves a rank

The counterfactual the whole change exists to make possible, asserted once
end to end, plus the tie at a zero lift that is the reason the axis is not
offered there."
```

---

### Task 5: what the operator reads

**Files:**
- Modify: `src/web/insights.rs:1164-1176` (`params_str`)
- Modify: `docs/evaluation.md:250-258` (the stale §5 bullet) and `:176-177` (the knob table)
- Modify: `config.example.toml:780-787` (the `[sitting] prime` comment)

**Interfaces:**
- Consumes: `GenerationParams.sitting_prime` from Task 1.
- Produces: nothing further.

- [ ] **Step 1: Write the failing test**

In `src/web/insights.rs`, inside `mod tests`:

```rust
    #[test]
    fn a_generation_says_whether_the_sitting_is_taking_part() {
        let p = crate::store::generations::GenerationParams {
            sitting_prime: true,
            ..Default::default()
        };
        assert!(params_str(&p).contains("sitting on"), "{}", params_str(&p));
        assert!(
            params_str(&Default::default()).contains("sitting off"),
            "{}",
            params_str(&Default::default())
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib a_generation_says_whether_the_sitting_is_taking_part`
Expected: FAIL — the rendered line has no `sitting` in it.

- [ ] **Step 3: Name it in the line**

In `src/web/insights.rs::params_str` (:1165-1176), extend the format string and its arguments:

```rust
    format!(
        "recency {:.2}, cap {}, pool ×{}, half-life {}d, lift {}, sitting {}, spread {}, rerank {}, review {:.2}",
        p.recency_weight,
        cap_str(p.per_source_cap),
        p.candidate_multiplier,
        p.recency_half_life_days,
        p.prime_lift,
        if p.sitting_prime { "on" } else { "off" },
        p.spread_max,
        if p.rerank { "on" } else { "off" },
        p.review_min
    )
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --lib a_generation_says_whether_the_sitting_is_taking_part`
Expected: PASS.

- [ ] **Step 5: Correct `docs/evaluation.md`**

Replace the first bullet of §5 ("**Anything about a sequence of queries.**", :250-258) with:

```markdown
- **Anything about a sequence of queries, *in this harness*.** The harness
  below scores each pair independently against a static index through
  `Door::Ui` with no session attached, so it cannot see continuity within one
  sitting. The runtime idle pass can: it replays through `Door::Judge` with the
  `Priming` the original search recorded — activation, sitting and due —
  handed back via `Origin::primed_as`. `[sitting] prime` is measured there, on
  the ladder in `src/core/ranking.rs`, and not here.
```

In the knob table (:176-177), add a row after "Priming margin":

```markdown
| Sitting priming | `ENGRAM__SITTING__PRIME` | Whether what this sitting has touched takes part in the lift. Shares `prime_lift`'s budget, so it does nothing at `0`. Swept by the idle pass, not by this harness. | MRR |
```

Then confirm nothing else in the file still claims ROADMAP.md says the knob stays off:

Run: `grep -n -i "roadmap" docs/evaluation.md`
Expected: no hit that attributes a claim about `[sitting] prime` to ROADMAP.md. Delete any that remains — ROADMAP.md is about a single-process first run and has never mentioned this knob.

- [ ] **Step 6: Correct `config.example.toml`**

Replace the `[sitting] prime` comment block (:781-787) with:

```toml
# Let what this sitting has touched lift a result.
#
# The only part of the sitting that moves an order, and the same query ranking
# differently in two sittings is exactly what is disorienting about it — so it
# ships off, the lift shares the one budget `associate.prime_lift` bounds,
# rank 0 never moves, and a hit it lifted says so on the row. Off is the
# starting rung rather than a verdict: every search records what the sitting
# held whether or not this is on, and a base with `evolve.autonomous` on
# replays that evidence and moves this from here on what it finds — but only
# once `associate.prime_lift` is above zero, since below that there is nothing
# for the sitting to share.
prime = false
```

- [ ] **Step 7: Verify the whole suite and commit**

```bash
cargo fmt
cargo test --lib
cargo clippy --all-targets -- -D warnings
git add src/web/insights.rs docs/evaluation.md config.example.toml
git commit -m "docs: say where the sitting knob is measured, and that it is

A generation row now names the sitting, evaluation.md §5 stops describing
the offline harness as the only instrument, and it stops citing ROADMAP.md
for a claim ROADMAP.md never made. The config comment says off is a
starting rung rather than a verdict."
```

---

## Verification

After Task 5, confirm the whole thing rather than the last commit:

```bash
cargo fmt --check
cargo test --lib
cargo clippy --all-targets -- -D warnings
```

Then check the shipped default is still off, from the outside:

```bash
cargo run -- --print-config | grep -iE "sitting|prime"
```

Expected: `prime = false` under `[sitting]`, and `prime_lift = 0` under `[associate]` — the feature still ships deactivated. What changed is that the loop can now measure it and turn it on.
