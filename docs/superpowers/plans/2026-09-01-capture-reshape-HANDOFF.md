# Capture reshape — handoff (session ended 2026-09-01, context limit)

**Spec:** `docs/superpowers/specs/2026-09-01-capture-reshape-design.md`
**Plan:** `docs/superpowers/plans/2026-09-01-capture-reshape.md`
**Worktree:** `.claude/worktrees/reshape`, branch `feat/reshape` (off `feat/time`)
**State:** full test suite GREEN (2341 lib + integrations), everything committed.

## Commits on feat/reshape

- `e06f9cf` Task 1 — real tokenizer behind `TokenCounter` (bundled Qwen3.8
  `assets/tokenizer.json`, `infer.tokenizer` path-or-URL with one-time cached
  download beside the store, estimator fallback; `reqwest` `blocking` feature
  for the one boot-time fetch on its own OS thread).
- `46e8c00` Task 2 — `text-splitter` is the chunker; `carry_lines`/heading
  carry retired (DB column kept, written 0). Line ranges partition via
  next-chunk-start; first window claims line 1.
- `c9283d4` Tasks 3+4 — `SynthesisMode` and its keys gone; `[infer.synthesize]`
  required (`refuse_removed_keys` names `infer.synthesis`,
  `infer.segment_tokens`); `Core.synthesizer` non-Option; promotion kept, its
  mode gate gone; `maybe_resynthesize` gone; the size fork: `capture_verbatim`
  arms `SegmentWindow` with `keep_artifacts` for a one-window corpus.
- `887f9cc` Task 5 — the judged reply: `SegmentInput.judge`,
  `Synthesizer::segment_judged` (defaulted, so fakes stay small),
  `parse_judged_response` (lenient `moment`/`events`/`links`; salvage drops
  the judgement, keeps artifacts), `judged_artifacts_schema`, prompt rewritten
  to target the embedder + tags/pinned + the JUDGE contract (absorbed the old
  REMIND wording).
- `9155ec0` Tasks 6+7+8 — `jobs/judgement.rs` applies the judgement (moments,
  journal filing via `JOURNALABLE` now living there, links validated against
  shown neighbor ids via `Store::relate_synthesized`, forced-remind may stand
  undated as `Source::Cue`); window job builds `JudgeAsk` + neighbor context
  (`neighbours()` on the first passage, embeds it inline if the embed race
  isn't done); `jobs/moments.rs`, `Stage::Moments`, the prototype classifier,
  cue tables, all date rules, `core.reminder`, `core.protos`, the REMIND
  prompt family and dead `time.intent_at` deleted; `core/moments.rs` rewritten
  pruned (Intent, PROTOTYPES-as-examples, refusals, zone, RRULE subset,
  complete/uncomplete_moment); intent echo → **fate echo** (rides the search
  response; over-long-paste guard branch carries it too).

## What remains (plan Tasks 9–11 + loose ends)

1. **Task 9 — judge page dies, tune-apply moves to insights.** Untouched so
   far: `src/web/judge.rs` still exists with all routes; `judge_pending` nav;
   templates `judge.html`, `_judge_*.html`. Move `tune_apply`/`tune_fragment`
   (+`_judge_tune.html`) to insights at `/ui/insights/tune/{run_id}/apply`
   behind `CanJudge`; move `pub fn ago` into `ui.rs` (used at
   `ui.rs::artifact rows`); fix `insights.html`'s "Review some" link (the
   Retrieval readout already lives on insights). Steal judge.rs's web test
   harness before deleting. `tests/eval.rs:~175` mentions judging wording.
2. **Task 10 — docs.** README (modes paragraph, "Judge" bullet, no-model
   claims), `config.example.toml` final pass (the `[infer]` header is done;
   check `neighbor` budget key documented — `context_neighbor_tokens` on
   `[infer.synthesize]`, default 1024, NOT yet in the example file; also
   `chunk_tokens`/promotion prose still says "off/earned" in places — grep
   `earned\|eager\|synthesis =` in example + README + `docs/evaluation.md`).
3. **Task 11 — end-to-end.** `cargo clippy --all-targets`; fresh instance
   (operator's recipe + a reachable `[infer.synthesize]` endpoint, now
   mandatory) with the four spot checks in the plan.
4. **Loose ends worth knowing:**
   - `judgement.rs` events test uses `event_moments_between`; day-page
     verification of LLM events is only unit-level so far.
   - `set_reminder(.., true)` now re-arms the artifact's window
     (reset+enqueue_seq) instead of the dead Moments stage.
   - `docs/superpowers/plans/2026-09-01-capture-reshape.md` Tasks 3–8 checkboxes
     were executed with deviations (recorded below); the plan file itself was
     not check-marked.
   - `app.js` still has the char-based `size-hint` (CHARS_PER_SEGMENT) beside
     the server fate echo — decide whether to delete the JS hint in Task 9/10
     (it no longer knows the real budget; the fate echo does).

## Deviations from the plan (all deliberate, all in the spec or commits)

- No `POST /ui/capture/probe`: the fate echo rides the search response (and
  the over-long guard). Spec §5 carries the note.
- `promote.resynthesize_after_unconfirmed` is silently ignored rather than
  refused (config-rs `raw.get` doesn't reach it reliably; it shipped disabled
  so silence loses nothing).
- Verbatim windows read directly are keep-implied (`keep |= state == Verbatim`
  in the window job) so no path can delete verbatim passages.
- Supersession stays majority-per-artifact: a small capture's joint passage
  often remains live beside the two artifacts covering it — tests assert
  3 live rows for the classic two-paragraph fixture, deliberately.
- Old-DB "pre-window chunks" re-segmentation test is `#[ignore]`d (legacy
  migration path, per repo rule against compat machinery).
- Title job survives but is normally unreachable (capture derives a local
  title); it still runs for a corpus whose title is cleared then settled.

## Prompt for the next session

> Continue executing `docs/superpowers/plans/2026-09-01-capture-reshape.md`
> in the existing worktree `.claude/worktrees/reshape` (branch
> `feat/reshape`; enter it with EnterWorktree path=.claude/worktrees/reshape).
> Read `docs/superpowers/plans/2026-09-01-capture-reshape-HANDOFF.md` first —
> Tasks 1–8 are done and committed green; only Task 9 (delete the judge deck
> page, move tune-apply to insights), Task 10 (README/config.example/
> evaluation.md rewrite) and Task 11 (clippy + fresh-instance end-to-end)
> remain, plus the loose ends the handoff lists. Use
> superpowers:executing-plans; keep the suite green per task; verify dead
> code is ACTUALLY removed after each deletion (grep for orphans + zero
> compiler warnings); commit per task in the repo's commit style. When the
> plan is complete, use superpowers:finishing-a-development-branch.
