# Self-tuning, stage 2: the idle pass, adoption, watch and revert

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a base move its own ranking parameters on the evidence use leaves behind, and take the move back on the same evidence, without anybody pressing anything.

**Architecture:** Not a new engine. `run_sweep` already gathers pairs, ranks them over a grid, gates candidates and picks a winner; stage 2 moves the apply step inside it, records what the winner promised, and adds a watch after. The one structural discovery this plan is built on is below, and it halves the work.

**Spec:** `docs/superpowers/specs/2026-09-04-self-tuning-design.md` — read it before task 1. This plan implements its **Stage 2**, minus the retrieval parameters (see the last section for why they are their own plan).

**Depends on:** stage 1 (`docs/superpowers/plans/2026-09-04-self-tuning-stage-1.md`), merged. Generations, observations and the four write sites exist.

## The discovery this plan is shaped by

A negative observation **cannot score a candidate counterfactually.** A give-up says *this list did not answer*; whether some other configuration's list would have is unknowable, because that list was never shown to anybody. The same is true of an unsupported literal.

So the two halves of the loop use different evidence, and neither needs machinery invented for it:

| Half | Evidence | Mechanism |
|---|---|---|
| **Adoption** — is this candidate better? | Positives only, counterfactually | `pairs_to_replay` → `grid` → `recommend`. **Already built.** Stage 1 wired observations into the first of those. |
| **Watch** — was adopting it right? | Positives *and* negatives, as rates actually observed | A lived comparison between two generations. No re-ranking, no counterfactual. |

That is why the spec says a weak negative "may revert and may never adopt" — it is not only a rule about strength, it is the only thing a negative is *capable* of. Nothing in this plan needs a scoring function beyond the two that exist.

## Global Constraints

- **Rust 1.94**, `cargo fmt --all --check`, `cargo clippy --all-targets --locked -- -D warnings` clean, all tests runnable with no infrastructure.
- **No gate is a tuned constant.** Every threshold reads the evidence in front of it. `recommend` — two net better, no aggregate loss, ties keep the current value — is the shape; generalize it, never replace it with a number. A base with four observations adopts nothing because four cannot clear a gate.
- **`config.toml` is never written by this loop.** Stage 1 established the split: the file holds the starting point and the envelope, the database holds the live generation. `write_ranking` stays the *human* apply path on `/ui/insights` and is not called from here.
- **Serving stays deterministic.** One generation is live; no request ever sees a candidate. Any change that makes the same query rank differently in two sittings is out of scope and against the design.
- **The pass spends no inference.** Vector reads only, from one bounded permit pool, and only while nobody is waiting.
- **Autonomy ships off.** `evolve.autonomous = false`, for the same reason `feed_sweep` does.
- Commit after every task, lowercase sentence subjects in the repo's style.

---

### Task 1: Generations gain a state machine and a memory

**Files:** Modify `src/store/generations.rs`, `src/store/schema.sql` (the `generations` table gains nothing — `run_id`, `predicted` and `state` are already there, unused)

**Interfaces produced:**
- `Store::adopt_generation(&self, g: &NewGeneration, run_id: &str, predicted: f64) -> Result<String>` — records and makes live, carrying what it promised
- `Store::revert_generation(&self, id: &str) -> Result<Option<Generation>>` — marks `id` as `reverted`, makes its parent live again, returns the parent
- `Store::tried_candidates(&self, since: i64) -> Result<Vec<GenerationParams>>` — the parameter sets already reverted, so the chooser does not offer them again
- `NewGeneration` gains nothing; `Generation` gains `pub predicted: Option<f64>` and `pub state: String`

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn a_reverted_generation_hands_the_base_back_to_its_parent() {
    let store = Store::memory().await.unwrap();
    let first = store.record_generation(&sample()).await.unwrap();
    let mut second = sample();
    second.parent_id = Some(first.clone());
    second.params.recency_weight = 0.25;
    let second_id = store.adopt_generation(&second, "run-1", 0.04).await.unwrap();

    let back = store.revert_generation(&second_id).await.unwrap().expect("a parent");
    assert_eq!(back.id, first);
    assert_eq!(store.live_generation().await.unwrap().unwrap().id, first);
}

#[tokio::test]
async fn a_reverted_candidate_is_not_offered_again() {
    // Without this the pass proposes the same losing candidate every quiet
    // period, adopts it, watches it fail, and reverts — forever.
    let store = Store::memory().await.unwrap();
    let first = store.record_generation(&sample()).await.unwrap();
    let mut second = sample();
    second.parent_id = Some(first);
    second.params.recency_weight = 0.25;
    let id = store.adopt_generation(&second, "run-1", 0.04).await.unwrap();
    store.revert_generation(&id).await.unwrap();

    let tried = store.tried_candidates(0).await.unwrap();
    assert!(tried.iter().any(|p| p.recency_weight == 0.25));
}

#[tokio::test]
async fn a_generation_with_no_parent_cannot_be_reverted() {
    let store = Store::memory().await.unwrap();
    let id = store.record_generation(&sample()).await.unwrap();
    assert!(store.revert_generation(&id).await.unwrap().is_none());
    assert_eq!(
        store.live_generation().await.unwrap().unwrap().id,
        id,
        "a base with nowhere to go back to stays where it is"
    );
}

#[tokio::test]
async fn what_a_generation_promised_is_kept_with_it() {
    let store = Store::memory().await.unwrap();
    let id = store.adopt_generation(&sample(), "run-1", 0.04).await.unwrap();
    let live = store.live_generation().await.unwrap().unwrap();
    assert_eq!(live.id, id);
    assert_eq!(live.predicted, Some(0.04));
}
```

- [ ] **Step 2: Run to verify they fail** — `cargo test --lib store::generations`, expected: `cannot find function adopt_generation`.

- [ ] **Step 3: Implement.** `adopt_generation` is `record_generation` with `run_id` and `predicted` bound instead of `NULL`. `revert_generation` is one `BEGIN IMMEDIATE` transaction: read the row's `parent_id`, return `Ok(None)` if it has none, otherwise set the row `reverted` and its parent `live`. `tried_candidates` selects `params` from rows in state `reverted` created after `since`, deserialized.

- [ ] **Step 4: Run to verify they pass** — 4 new tests green.

- [ ] **Step 5: Commit**

```bash
git add src/store/generations.rs
git commit -m "feat(evolve): a generation can be taken back, and a failed candidate is remembered"
```

---

### Task 2: What a generation actually scored while it was live

**Files:** Create `src/eval/lived.rs`; modify `src/eval/mod.rs`

**Interfaces produced:**
- `pub struct Lived { pub positives: usize, pub negatives: f32, pub observations: usize }`
- `pub async fn lived(core: &Core, generation_id: &str) -> Result<Lived>`
- `pub fn holds_up(new: &Lived, old: &Lived) -> bool` — the watch gate

This is the half of the loop that cannot be counterfactual. `negatives` is a weighted sum rather than a count, because a give-up is a quarter of an unsupported literal and the strengths already say so.

`holds_up` is `recommend` pointed at rates instead of ranks, and it is deliberately **conservative in one direction**: an adopted generation is kept unless the predecessor would clear the gate against it. A tie keeps the newer one, because reverting is also a change and the same rule that says a knob moving nothing keeps its value says a generation that lost nothing keeps its place.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn a_generation_that_earned_more_positives_holds_up() {
    let new = Lived { positives: 12, negatives: 1.0, observations: 13 };
    let old = Lived { positives: 6, negatives: 4.0, observations: 10 };
    assert!(holds_up(&new, &old));
}

#[tokio::test]
async fn a_generation_that_lost_ground_does_not_hold_up() {
    let new = Lived { positives: 3, negatives: 6.0, observations: 9 };
    let old = Lived { positives: 9, negatives: 1.0, observations: 10 };
    assert!(!holds_up(&new, &old));
}

#[test]
fn a_tie_keeps_the_newer_generation() {
    let a = Lived { positives: 5, negatives: 2.0, observations: 7 };
    assert!(holds_up(&a, &a), "reverting is a change too");
}

#[test]
fn too_few_observations_hold_up_rather_than_revert() {
    // Not a floor anybody chose: two observations cannot clear a gate in
    // either direction, and the one that fires on no evidence must be the one
    // that changes nothing.
    let new = Lived { positives: 0, negatives: 1.0, observations: 1 };
    let old = Lived { positives: 40, negatives: 0.0, observations: 40 };
    assert!(holds_up(&new, &old), "one observation decides nothing");
}

#[tokio::test]
async fn lived_counts_only_what_happened_under_that_generation() {
    let (core, first) = base().await;
    observe(&core, &first, Source::Cited).await;
    let second = another_generation(&core, &first).await;
    observe(&core, &second, Source::GaveUp).await;

    assert_eq!(lived(&core, &first).await.unwrap().positives, 1);
    assert_eq!(lived(&core, &second).await.unwrap().positives, 0);
    assert!(lived(&core, &second).await.unwrap().negatives > 0.0);
}
```

- [ ] **Step 2: Run to verify they fail** — `cargo test --lib eval::lived`.

- [ ] **Step 3: Implement.** `lived` is one aggregate query over `observations` for that generation, `excluded_at IS NULL`, summing strengths above and below zero separately. `holds_up` compares positives-per-observation against negatives-per-observation and requires the *old* generation to beat the new one by more than one observation could account for before it returns `false` — the same "two net better" shape, in the direction that changes nothing on thin evidence.

- [ ] **Step 4: Run to verify they pass.**

- [ ] **Step 5: Commit**

```bash
git add src/eval/lived.rs src/eval/mod.rs
git commit -m "feat(evolve): what a generation scored while it was actually serving"
```

---

### Task 3: The candidate chooser

**Files:** Modify `src/eval/sweep.rs`

**Interfaces produced:**
- `pub fn candidates(current: RankingParams, tried: &[GenerationParams], budget: usize) -> Vec<RankingParams>`

The one genuinely new algorithm here, and it exists because the fixed 5×4 grid does not survive a wider parameter set: it is 20 candidates today and every axis added multiplies it. `candidates` draws a bounded set instead — the running configuration always (it is the baseline), then neighbours of it on each axis, then a few drawn further out — and never offers a parameter set already reverted.

Deliberately not a learned sampler. Neighbours-first is the whole heuristic: a knob that helps usually helps a little, and the loop runs every quiet period, so a long walk is reached in small steps that each get their own watch.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn the_running_configuration_is_always_among_the_candidates() {
    let current = RankingParams { recency_weight: 0.05, per_source_cap: Some(3) };
    assert!(candidates(current, &[], 8).contains(&current));
}

#[test]
fn a_reverted_candidate_is_never_offered() {
    let current = RankingParams { recency_weight: 0.05, per_source_cap: Some(3) };
    let tried = vec![GenerationParams { recency_weight: 0.1, per_source_cap: Some(3) }];
    let out = candidates(current, &tried, 8);
    assert!(!out.iter().any(|c| c.recency_weight == 0.1 && c.per_source_cap == Some(3)));
}

#[test]
fn the_budget_is_respected_and_neighbours_come_first() {
    let current = RankingParams { recency_weight: 0.05, per_source_cap: Some(3) };
    let out = candidates(current, &[], 4);
    assert!(out.len() <= 4);
    assert!(
        out.iter().any(|c| c.per_source_cap == Some(2) || c.per_source_cap == Some(5)),
        "a neighbour on the cap axis must be reachable inside a small budget"
    );
}

#[test]
fn every_candidate_moves_at_most_one_knob() {
    // `moved` is what keeps a result about caps from arriving wearing a
    // recency change; the chooser must not hand it a candidate that already
    // moved both.
    let current = RankingParams { recency_weight: 0.05, per_source_cap: Some(3) };
    for c in candidates(current, &[], 12) {
        assert!(moved(c, current) <= 1, "{c:?}");
    }
}
```

- [ ] **Step 2: Run to verify they fail.**
- [ ] **Step 3: Implement** as described: current first, then one-step neighbours on each axis, then wider steps, filtering `tried`, truncated to `budget`.
- [ ] **Step 4: Run to verify they pass.**
- [ ] **Step 5: Commit**

```bash
git add src/eval/sweep.rs
git commit -m "feat(evolve): candidates are chosen a step at a time rather than enumerated"
```

---

### Task 4: The idle pass adopts instead of recommending

**Files:** Create `src/jobs/tune.rs`; modify `src/jobs/mod.rs`, `src/config.rs`, `config.example.toml`

**Interfaces produced:**
- `pub async fn run(core: &Core) -> Result<Option<String>>` — the adopted generation's id, or `None`
- `EvolveConfig` gains `pub autonomous: bool` (default `false`) and `pub idle_secs: i64` (default 1800)

The pass is `run_sweep`'s body with three changes: `candidates` instead of `grid`, observations prioritised by prediction error when the replay set is bounded, and the winner adopted as a generation rather than written into `eval_runs` as a recommendation. The `eval_runs` row is still written — it is the journal, and the generation names it through `run_id`.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn a_pass_with_autonomy_off_changes_nothing() {
    let (core, _) = seeded_with_observations().await;
    assert!(run(&core).await.unwrap().is_none());
}

#[tokio::test]
async fn a_candidate_that_clears_the_gate_becomes_the_live_generation() {
    let (mut core, before) = seeded_with_observations().await;
    core.evolve.autonomous = true;
    let adopted = run(&core).await.unwrap().expect("a candidate cleared");

    let live = core.store.live_generation().await.unwrap().unwrap();
    assert_eq!(live.id, adopted);
    assert_ne!(live.id, before, "the base moved");
    assert!(live.predicted.is_some(), "it must say what it promised");
    assert_eq!(*core.ranking.read().unwrap(), live.params.into(), "and serve under it");
}

#[tokio::test]
async fn a_pass_that_finds_nothing_better_leaves_the_generation_alone() {
    let (mut core, before) = seeded_with_nothing_to_gain().await;
    core.evolve.autonomous = true;
    assert!(run(&core).await.unwrap().is_none());
    assert_eq!(core.store.live_generation().await.unwrap().unwrap().id, before);
}

#[tokio::test]
async fn the_pass_never_writes_the_operators_config_file() {
    // The file is the starting point and the envelope. A loop that rewrote it
    // every quiet period would turn a commented file into a machine's.
    let (mut core, _) = seeded_with_observations().await;
    core.evolve.autonomous = true;
    let before = std::fs::read_to_string(&config_path()).unwrap();
    run(&core).await.unwrap();
    assert_eq!(std::fs::read_to_string(&config_path()).unwrap(), before);
}
```

- [ ] **Step 2: Run to verify they fail.**
- [ ] **Step 3: Add the two config keys** with their reasoning in `config.example.toml`, in the voice of the `[evolve]` keys already there.
- [ ] **Step 4: Implement** `jobs::tune::run`, and register it so it fires after `idle_secs` of quiet — the same cold/ticker machinery `jobs::observe` hangs off.
- [ ] **Step 5: Run to verify they pass, then the whole suite.**
- [ ] **Step 6: Commit**

```bash
git add src/jobs/tune.rs src/jobs/mod.rs src/config.rs config.example.toml
git commit -m "feat(evolve): a quiet base moves its own ranking, and says what it expects to gain"
```

---

### Task 5: The watch, and the revert

**Files:** Modify `src/jobs/tune.rs`

**Interfaces produced:** none new. `run` gains a branch it takes *before* considering a new candidate.

The rule: a live generation with a parent and a prediction is **under watch**. Every pass compares its `lived` score against its parent's; while `holds_up` says yes, the pass returns without proposing anything. When it says no, the generation is reverted and its parameters remembered.

Nothing is proposed while a generation is under watch. One change at a time is what makes the journal readable and the revert exact, and it is what stops a base walking three knobs away from anything it measured.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn a_generation_under_watch_blocks_a_new_proposal() {
    let (mut core, _) = adopted_and_watching().await;
    core.evolve.autonomous = true;
    let live_before = core.store.live_generation().await.unwrap().unwrap().id;
    assert!(run(&core).await.unwrap().is_none(), "one change at a time");
    assert_eq!(core.store.live_generation().await.unwrap().unwrap().id, live_before);
}

#[tokio::test]
async fn a_generation_that_lost_ground_reverts_itself() {
    let (mut core, parent) = adopted_and_watching().await;
    core.evolve.autonomous = true;
    observe_badly_under_live(&core, 12).await;

    run(&core).await.unwrap();
    assert_eq!(
        core.store.live_generation().await.unwrap().unwrap().id, parent,
        "the base put itself back"
    );
}

#[tokio::test]
async fn a_reverted_generation_is_not_proposed_again_on_the_next_pass() {
    let (mut core, _) = adopted_and_watching().await;
    core.evolve.autonomous = true;
    observe_badly_under_live(&core, 12).await;
    let reverted = core.store.live_generation().await.unwrap().unwrap().params;
    run(&core).await.unwrap();

    let next = run(&core).await.unwrap();
    if let Some(id) = next {
        let g = core.store.live_generation().await.unwrap().unwrap();
        assert_ne!(g.params, reverted, "{id} re-proposed what had just failed");
    }
}
```

- [ ] **Step 2: Run to verify they fail.**
- [ ] **Step 3: Implement** the watch branch at the top of `run`.
- [ ] **Step 4: Run to verify they pass.**
- [ ] **Step 5: Commit**

```bash
git add src/jobs/tune.rs
git commit -m "feat(evolve): a base watches what it changed, and puts it back when it did not hold"
```

---

### Task 6: The anchor, and suspension

**Files:** Create `src/eval/anchor.rs`; modify `src/jobs/tune.rs`, `src/eval/mod.rs`

**Interfaces produced:**
- `pub async fn agreement(core: &Core) -> Result<Option<Agreement>>` — over searches carrying **both** a human verdict and an observation, how often the two say the same thing
- `pub fn trustworthy(a: &Agreement) -> bool`

The one safeguard everything else leans on. Self-generated evidence is one step removed from *the person got their answer*, and this is what notices when the two have come apart. Not a tuned threshold: agreement is trustworthy while it beats chance by more than one disagreement could account for — the same shape as every other gate here.

When it stops being trustworthy, `run` adopts nothing, reverts nothing, keeps recording, and says so. Suspension is a state, not a failure.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn agreement_that_beats_chance_is_trustworthy() {
    assert!(trustworthy(&Agreement { agreed: 18, disagreed: 2 }));
}

#[test]
fn agreement_no_better_than_chance_is_not() {
    assert!(!trustworthy(&Agreement { agreed: 10, disagreed: 10 }));
}

#[test]
fn two_verdicts_decide_nothing_either_way() {
    assert!(trustworthy(&Agreement { agreed: 2, disagreed: 0 }),
        "thin evidence must not suspend a base any more than it moves one");
}

#[tokio::test]
async fn a_base_whose_evidence_stopped_agreeing_suspends_itself() {
    let (mut core, before) = seeded_with_observations().await;
    core.evolve.autonomous = true;
    disagree_loudly(&core, 20).await;

    assert!(run(&core).await.unwrap().is_none());
    assert_eq!(core.store.live_generation().await.unwrap().unwrap().id, before);
}
```

- [ ] **Step 2: Run to verify they fail.**
- [ ] **Step 3: Implement.** `agreement` joins `search_events` carrying a verdict with observations on the same event's artifacts; agreement is a positive observation on a search judged right, or its absence on one judged wrong.
- [ ] **Step 4: Run to verify they pass.**
- [ ] **Step 5: Commit**

```bash
git add src/eval/anchor.rs src/eval/mod.rs src/jobs/tune.rs
git commit -m "feat(evolve): a base that stopped agreeing with its operator stops moving"
```

---

### Task 7: Ops says what the base has been doing

**Files:** Modify `src/web/insights.rs` and its template

**Interfaces produced:** none.

Autonomy the operator cannot see is autonomy they cannot judge. The page gains: the live generation and its parameters, whether it is under watch and what it promised, the last few adoptions and reverts with their outcomes, and — first, in plain words — whether autonomy is suspended and why.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn insights_names_the_live_generation_and_whether_it_is_watched() {
    let (core, _) = adopted_and_watching().await;
    let body = render_insights(&core).await;
    assert!(body.contains("under watch"), "{body}");
}

#[tokio::test]
async fn a_suspended_base_says_so_before_anything_else() {
    let (core, _) = suspended().await;
    let body = render_insights(&core).await;
    let suspended_at = body.find("not moving").expect("said");
    let history_at = body.find("adopted").unwrap_or(usize::MAX);
    assert!(suspended_at < history_at, "the reason comes before the history");
}
```

- [ ] **Step 2–4:** red, implement, green.
- [ ] **Step 5: Commit**

```bash
git add src/web/insights.rs src/web/templates
git commit -m "feat(evolve): the insights page says what the base changed, and what it took back"
```

---

## What this stage does not do

- **The retrieval parameters.** Candidate pool depth, rerank on/off, `prime_lift`, spread. They need the pass to genuinely re-search rather than re-sort a stored list, which changes its cost from vector reads over a cached pool to a query per candidate per observation. That is a different performance conversation and it belongs in its own plan — and the spec already says these move only once the reordering loop has a track record in the journal. **Stage 2b.**
- **The corpus jobs.** Stage 3, and it needs this stage's adoption history before earned autonomy means anything.
- **Any live experiment.** One generation is live and the same query ranks the same way twice. Nothing here may spend that.
