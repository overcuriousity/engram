# Autonomous curation: nothing waits on a person

Written 2026-10-07, against `master` at `2534c5e`.

## What it is for

engram still has places where the base stops and waits for its operator: the
decide queue on Insights, parked near-duplicate captures, the "unverified" list,
"Mark reviewed" flags, the knowledge-gap list, and the opt-in to corpus
autonomy with its weekly cap. Each is a chore. This design removes every one.

After it, a person touches the base in exactly these ways:

- **Judging answers** — "Was this what you were looking for?" under a search or
  an Ask, the verdict bar, and "nothing here".
- **Direct edits** — capture, delete, edit an artifact, the reminder controls
  (out of scope here and unchanged).
- **Undo** — a quiet button on each journal row. Offered, never asked for.

Everything else the base decides on its own, reversibly, and the operator's
verdicts on answers are the signal that tells it when it got something wrong.

## Decisions taken

- **Change each job where it decides.** Every point in the code that hands a
  pair or a capture to "a person" gets an automatic rule in the job that already
  owns it. No new "steward" job: that would split one decision across two
  places and leave the waiting states alive in the schema.
- **Contradictions are not resolved; they are shown.** Deciding which of two
  facts is current stays something no model does. Both sides stay in results,
  and search and Ask say they disagree. A judged answer does *not* pick a side;
  the pair closes only when one side leaves results.
- **Corpus autonomy is always full**, paced per job per day rather than capped
  per week.
- **Insights becomes a read-only journal** with an undo on each row.
- **Nothing is deleted by the base.** Unchanged: every automatic action is a
  deprecation, supersession or merge with an undo, and the self-retract loop in
  `jobs::retract` keeps reading them against later searches.

## 1. What replaces each human decision

| Today, waiting on a person | From now on |
|---|---|
| `Relation::Duplicate` settles `PairState::Duplicate` and waits for "Synthesize" (`jobs/dedupe.rs`, the `Duplicate` arm of `apply`) | The merge is written where it is found, through the existing synthesis path and its loss check: no merge that drops a number, command or path is written. Journaled as `Kind::Merge` under `Job::Dedupe`. A merge the loss check or the lineage rule refuses closes the pair `NoConflict` with the reason as detail. |
| `Relation::Conflict` → `PairState::Contradiction` on the decide queue | Stays `Contradiction`. Never queued, never drawn as a card. Surfaced at read time (section 2). |
| `TAKEN_BACK` — a verdict repeated over a taken-back action goes to a person as `Contradiction` | Settles `NoConflict` with `TAKEN_BACK` rewritten to say both were left as they are. The base does not repeat what was undone. |
| `Unmergeable`; "do not fit one call"; "lost its sources"; lineage refusal | Settles `NoConflict` with the reason as detail. Both stay in results. |
| Old `Superseded` proposals awaiting "apply supersede" | Applied through the `Relation::Replaced` path (newest-wins check, live checks, taken-back check). |
| Parked near-duplicate corpus (`near_dupe_of` set; Replace / Keep both / Discard) | Cleared as keep-both automatically at capture time. Both captures are already read and searchable since `89fc0b7`; their artifacts meet the dedupe judge like any others. `near_dupe_of` is still recorded as provenance. |
| "Unverified" set-aside rows (Still accurate / Hide) | Removed. A positive verdict (search or Ask) whose cited/opened artifact is X calls `Core::verify(X)`. A negative verdict does nothing to age. Age keeps entering ranking as it does today. |
| `orphaned_source` flag, "Mark reviewed" | The sweep that sets the flag calls `accept_source_loss` itself and does not set the flag. |
| `literals_unverified` flag, "Mark reviewed" | Stays as a badge on the artifact, no button — the same stance Ask takes on `unsupported`. |
| Knowledge-gap list (Save / Forget / dismiss) | List removed from Insights and Android. Recording, grouping (`jobs::gaps`), cover-on-capture and pursuit stay: they feed evaluation. "Nothing here" under a search stays — it is a verdict on an answer. |
| Low coverage on a capture | The sweep rereads the uncovered lines once on its own (the path `reread_uncovered_ui` takes today), recorded on the corpus so it never runs twice. The figure stays on the queue row as information; the link stays. |
| `evolve.autonomous` opt-in to "full"; `max_actions_per_week = 10` | Always full. `max_actions_per_day` per job (default 20). `Core::budget` keeps its shape with a 24-hour window. |

## 2. Showing a disagreement

### Store

`Store::open_contradictions(&[artifact_id]) -> Vec<Disagreement>`: pairs in
`PairState::Contradiction` where either member is in the given set and both
members are in results. `Disagreement { pair_id, artifact_id, other_id,
other_title, other_created_at, detail }`, one row per side asked about.

### Search

`SearchResult` gains `disagrees_with: Vec<Disagreement>` (empty skipped in
serde). Filled in one store read over the page of results, after ranking; it
never changes rank. The results fragment draws one line under the snippet:

> Disagrees with "NAS backup schedule" (12 Sep): retention is 30 days there, 14 here.

The title links to the other artifact.

### Ask

Before packing the excerpts:

1. For every excerpt with an open contradiction, the other side is appended if
   it is not already among the hits — appended the way linked neighbours are
   ("reached sideways"), so it does not displace a ranked hit and is subject to
   the same budget.
2. Excerpts that take part in a disagreement print their capture date in the
   header and a line `Disagrees with [n]: <detail>` after their caveats
   (`prompt::ask_excerpt` takes the extra lines).
3. `ASK_SYSTEM` gains one sentence: where an answer rests on an excerpt marked
   `Disagrees with`, give both readings with their dates and cite each, rather
   than choosing between them.
4. `AskResponse` gains `disagreements: Vec<Disagreement>` for the pairs whose
   both sides were shown. The answer pane badges them, as it badges
   `unsupported`, so the disagreement is visible whatever the model wrote.

### Closing

When either member leaves results — deleted, deprecated, superseded by a newer
capture through the ordinary judge — the existing lifecycle path moves the pair
to `PairState::Stale` and the line stops appearing. Nothing else closes a
contradiction.

### Other doors

`/api/v1` and MCP serialize the same structs and get the fields for free.
Android draws the same line on `HitRow` and the same badge in `AskScreen`.

## 3. Removals, config, existing bases

### Web UI

- Insights: "Needs you", `_decide.html` and `_gaps.html` go. "Set aside for you"
  becomes "What the base did": the merged / generated / hidden / buried rows
  with their reason and an Undo; the `parked` and `unverified` row kinds go.
  Undos by a person and by the base stay told apart.
- Artifact detail: "Mark reviewed" and the stale verify action go; flags render
  as badges.

### Routes removed (`/ui/...` and `/api/v1/...`)

Pair dismiss / discard / supersede / synthesize; `/gaps`, `/gaps/forget`,
`/gaps/{kind}/{id}/dismiss`; `/artifacts/{id}/verify`;
`/corpora/{id}/resolve`; `/artifacts/{id}/reviewed`; `/pairs` (the list).
Kept: every undo (merge, condensation, unsupersede, reactivate), deprecate as a
direct edit, delete, and every verdict route.

### Android

Remove `JudgeScreens.kt`, `Judging.kt`, `Screen.Pairs`, `Screen.Gaps` and the
matching `Transport` calls (`pairSupersede`, `pairSynthesize`, `pairDiscard`,
`pairDismiss`, `gapDismiss`, gap forget, verify, resolve). Journal and Insights
stay, read-only apart from undo. Contained mode runs the same Rust core and
inherits the rules.

### Config

- `evolve.autonomous` is no longer read for corpus actions; the base behaves as
  `"full"`. A present key logs one warning at start and does not refuse it.
  `Autonomy` stays as a type only where the ranking/generation code needs it,
  pinned to `Full`.
- `max_actions_per_week` is replaced by `max_actions_per_day`; the old key is
  ignored with a warning.
- `config.example.toml` comments that say "decide on Ops", "review queue" or
  "goes to a person" are rewritten.

### Existing bases

A drain runs at the start of every dedupe sweep, through the ordinary paced
units, so it is idempotent and needs no migration flag:

- `Duplicate` pairs → arm the merge.
- `Superseded` pairs → apply via the `Replaced` path.
- `Contradiction` pairs whose detail is `TAKEN_BACK`, `Unmergeable`, and the
  "resolve by hand" refusals → `NoConflict`.
- Real contradictions → left; now surfaced.
- Corpora with an unresolved near-duplicate flag → keep-both.
- `orphaned_source` flags → accepted.

### Docs

README sections *Judge*, *Duplicates* and *Gaps*, and `docs/api.md`.

## 4. Testing

Inline `#[tokio::test]` with `infer::fake`, as the suite does now.

- **Each rule:** a duplicate verdict writes a merge and journals it; a refused
  merge closes the pair with nothing queued; a taken-back verdict closes
  `NoConflict`; a near-duplicate capture is cleared keep-both; a positive
  verdict citing X resets `last_verified_at`, a negative one does not;
  `orphaned_source` is accepted without a flag; low coverage is reread once and
  never twice; the daily pace stops a job at `max_actions_per_day` and resumes
  after 24 hours.
- **Contradictions:** search fills `disagrees_with` on both sides; Ask appends
  the missing side, prints `Disagrees with [n]`, fills `disagreements`;
  deleting one side stales the pair and the field empties.
- **Drain:** a base seeded with every old waiting state has nothing waiting
  after two sweeps, and the second sweep changes nothing.
- **Removals:** removed routes 404; Insights renders with no "Needs you" and no
  gap list, and undo still works. Tests asserting on the decide queue are
  rewritten against the new behaviour, not deleted.
- **Config:** `autonomous = "off"` loads, runs as full, warns;
  `max_actions_per_week` is ignored with a warning.
- **Android:** removed `Transport` tests go; a new test parses the disagreement
  fields; the app builds without the judge screens.
- **Real model, by hand:** one Ask against a seeded contradiction on the
  configured endpoint, to see that the model follows the new `ASK_SYSTEM`
  sentence. The fake backend cannot tell us that.

## Left out

- Reminders and their controls.
- Letting a verdict pick a side of a contradiction.
- Any change to ranking, reap, condense or the generation loop beyond the
  budget window.
