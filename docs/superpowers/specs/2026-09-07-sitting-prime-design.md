# `[sitting] prime` joins the ladder

Written 2026-09-07. A follow-on to
`docs/superpowers/specs/2026-09-05-self-tuning-stage-3-design.md`, whose Part A
put three knobs on the ranking ladder and left this one off. Everything in that
spec stands; this adds the fourth knob and corrects the reason the third was
left out.

## The defect

`[sitting] prime` ships off, and nothing but a person editing `config.toml` can
ever turn it on. It is on no ladder, it is not a field of `GenerationParams`,
and no sweep offers it as a candidate. That alone would be a conservative
default. What makes it a defect is the reason it can never be measured, which
is a single line in `src/core/search.rs`:

```rust
let sitting: HashSet<String> = match self.sitting.prime {
    true  => origin.session…touched…,
    false => Default::default(),
};
```

The sitting membership is only *recorded* when the knob is already on. Its two
siblings in the same `Priming` do not behave this way: `activation` is computed
unconditionally, and `due` is gated on `time.lift`, which ships `true`. The
sitting is the only priming input whose evidence collection is gated on its own
switch.

So the loop is closed shut. The knob ships off, therefore no sitting is ever
recorded, therefore there is nothing to replay, therefore the idle pass can
never measure it, therefore it stays off. `src/core/recommend.rs:345` already
names this shape — "a recommender with no visible hit rate becomes
`[sitting] prime`: a default nobody ever moved because nobody could see its
effect." The tree was describing its own bug.

Two consequences follow from the same place. `sitting.prime = true` is a no-op
today whatever an operator does, because `prime()` returns early on `lift == 0`
and the shipped `associate.prime_lift` is `0`. And `GenerationParams` cannot
express the knob, so even a sweep that wanted to offer it has nowhere to put
the answer.

## What is already built, and what `docs/evaluation.md` gets wrong

The measurement apparatus this needs exists and is running. `docs/evaluation.md`
§5 says a feature reading `Origin::session` "cannot move a number here, because
the harness searches through `Door::Ui` with no session attached". That is true
of the offline export harness in `src/eval/export.rs` and false of the runtime
idle pass, which is a different instrument the section never mentions:

- `src/eval/sweep.rs` replays recorded observations under candidate parameter
  sets through `Door::Judge`, and it is the thing that adopts.
- `src/eval/lived.rs` reads what a generation earned while it was actually
  serving, with no replay at all.
- `src/eval/rehearsed.rs` scores a candidate on the base's own probes. It may
  refuse and may revert; it never adopts.
- `src/eval/anchor.rs` checks the self-generated evidence against human
  verdicts, and suspends the loop when the two come apart.

The replay path already carries priming end to end. `NewEvent.context` is a
`Option<Priming>` persisted to the `search_context` table
(`src/store/feedback.rs:513`), read back by `pairs_to_replay`
(`src/eval/sweep.rs:514`), handed to the replay through `Origin::primed_as`
(`src/eval/sweep.rs:329`), and honoured by `search.rs:1700` on the Judge door
where priming is otherwise off. `Priming` is `{ activation, sitting, due }` —
the sitting field is already there, and its own comment says why: "Kept so the
idle pass can replay the search at another lift."

Nothing about a run-scoring harness is needed. One gate is the whole blocker.

## Why the counterfactual is honest

The worry worth answering is circularity: priming on "this sitting already
opened it" and then scoring on "did they open a sitting member" sounds
self-fulfilling, and it is the family of loop the activation weights exist to
close — `retrieved = 0.0`, because being surfaced is not use.

It is not circular during recording. The evidence is gathered while
`sitting.prime` is off, so the ranking the searcher saw was not influenced by
the sitting at all. They opened that artifact at its honest, unprimed rank. The
replay then asks a clean question: it was served at rank 7, and with the knob
on it would have been at rank 3. Nothing caused anything.

The loop only becomes possible after adoption, when a higher rank starts
producing more opens. That is the ordinary feedback risk every adopted knob
carries, and it is what the lived watch, the probe anchor and the verdict
anchor are for. Two brakes are already in place: only an artifact *open* marks
a sitting touched (`src/web/artifact.rs:572` is the sole caller — search
exposure marks nothing), and `retrieved = 0.0` keeps exposure out of activation
as well.

The alternative considered and rejected was to score the knob only on
observations whose artifact was *not* in the recorded sitting. That is not a
conservative version of the measurement; it is a broken one. Sitting priming
lifts only sitting members, and a member's accessibility is floored at `1.0` in
`prime()`, which makes it strictly harder for a non-member to climb past it and
easier for a member to climb past the non-member. A non-member's rank under the
knob is therefore always equal or worse. Excluding members would measure the
knob's collateral damage and nothing else, guaranteeing it never wins — an
elaborate way of hard-coding it off.

## The design

### 1. The sitting is recorded whether or not it is used

`Priming.sitting` becomes the real touched set on every search that primes and
carries a session, independent of the knob. What the knob gates is the *use* of
that set, not its collection — which is how `activation` has always behaved.

Serving must not change while the knob is off. `prime()` therefore takes the
flag and ignores the sitting set entirely when it is false, for the lift **and**
for the `in_sitting` badge. The badge is set today from a set that is empty when
the knob is off, so gating it on the flag reproduces present behaviour exactly.
Making the badge unconditional is a defensible separate change — carrying is
always on and the rail already shows where you have been — but it is a visible
change nobody asked for, and it stays out of this one.

### 2. The knob moves from config into the generation

`sitting_prime: bool` joins `RankingParams` (`src/core/ranking.rs`) and
`GenerationParams` (`src/store/generations.rs`), with
`#[serde(default = "crate::config::default_sitting_prime")]` on the stored
field so rows written before this change deserialise as the shipped `false`,
the way the stage-3a knobs did.

`RankingParams::from_config` gains a `&SittingConfig` argument. `search.rs`
reads `params.sitting_prime` rather than `self.sitting.prime`, which is what
lets a replay ask the counterfactual and what lets an adopted generation change
the next request's behaviour. `Core.sitting` stays as the startup value that
seeds the first generation.

`apply_learn_mode` needs no change: it resolves `sitting.prime` to `false`
under `off` and `learning` before `RankingParams::from_config` reads it, and
under those modes `evolve.autonomous` is already forced to `off`, so no
generation can reintroduce it from underneath.

`write_back` gains `doc["sitting"]["prime"]`, so an adopted generation is
written to the file in the key an operator would have typed. `rerank` remains
the exception it already is — it has no key of its own.

### 3. A two-rung axis, offered only when it can do anything

`SITTING_PRIMES: [bool; 2] = [false, true]` sits beside `PRIME_LIFTS` in
`ranking.rs`, and `sweep::candidates` offers the flip on the same
neighbours-first walk as the other ladders. `moved()` counts it, so the
one-knob-per-candidate invariant continues to hold.

The axis is **conditional on `current.prime_lift > 0`**. With the lift at zero
the flip is a guaranteed no-op: `prime()` returns early, so the candidate ties,
and a tie keeps the current value. Nothing stops it being offered again. A tied
candidate is never adopted, so it never becomes a generation, so it never
reaches `tried_candidates` — which holds only the `reverted` and `refused` —
and the pass would therefore offer the same dead flip every quiet period
forever, paying one rank per pair each time to re-measure a tie it can prove
without ranking anything. This mirrors the treatment `rerank` already gets: a
knob meaningless where nothing downstream can act on it is not offered.

The practical consequence is an order: the pass walks the `prime_lift` ladder
first, and the sitting axis appears only once a lift has been adopted on its
own evidence. That is the right order anyway. The sitting shares the lift's
budget, so asking about it before the budget is non-zero is asking about
nothing.

`tune::BUDGET` becomes conditional for the same reason: 19 as today when
`prime_lift == 0`, 20 when it is above zero. The two existing invariants —
`every_candidate_moves_at_most_one_knob` and
`the_pass_budget_covers_every_rung_on_every_axis` — are restated over both
cases rather than relaxed.

### 4. What the operator sees

`params_str` in `src/web/insights.rs` gains `sitting on/off`, so a generation
row on `/ui/insights` says what it is running. Nothing else on the surface
changes: a hit the sitting lifted already says so on the row, and the existing
`primed` and `in_sitting` labels already distinguish "this moved" from "you
have been here".

## Testing

Serving, in `src/core/search.rs`:

- `prime()` ignores the sitting set for both the lift and the badge when
  `sitting_prime` is false, and honours it for both when true.
- A search with the knob off still records a `Priming` whose `sitting` names
  what the session has touched. This is the regression test for the defect
  itself.
- A search on a door with no session records an empty sitting, knob either way.

The replay, in `src/eval/sweep.rs`:

- A pair carrying a recorded sitting ranks the touched artifact higher under
  `sitting_prime = true` than under `false`, at a non-zero lift.
- The same pair ties across the flip at `prime_lift = 0`.

The chooser:

- `candidates()` offers no sitting flip at `prime_lift == 0`, and exactly one
  at every rung above it.
- `moved()` counts the flip; the one-knob invariant holds over the grid in both
  budget cases.

Storage, in `src/store/generations.rs`:

- `GenerationParams` round-trips the field, and a JSON row written without it
  deserialises to `false`.

Config, in `src/config.rs`:

- `apply_learn_mode` still resolves `sitting.prime` to `false` under `off` and
  `learning`, and still leaves an explicitly written key alone.
- `write_back` writes `[sitting] prime`, and `--print-config` reports it.

## Out of scope, and noted

`[time] lift` ships `true` and is equally inert at `prime_lift = 0`. A knob
that is on and doing nothing is worth surfacing — a startup warning, or a line
on `--print-config` — but it is a separate change and this spec does not make
it.

`docs/evaluation.md` §5 needs correcting on two counts: it describes the
offline export harness as though it were the only instrument, and it cites
ROADMAP.md as saying `[sitting] prime` stays off and unmeasured, which
ROADMAP.md does not say and never did. The correction ships with this work
because the paragraph is the reason the knob was left off the ladder.
