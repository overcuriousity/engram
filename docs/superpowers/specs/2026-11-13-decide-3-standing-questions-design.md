# Standing questions: the person's own readings of every note

Date: 2026-11-13. Spec 3 of 3. Depends on spec 1 (the role and `Decider`) and
on spec 2 (`2026-11-13-decide-2-judgement-design.md`): the scan, the
`artifact_readings` table, bands and tuning. Lowest priority; build it last.

## Intent

Spec 2 has the base ask its own questions of every artifact: what category,
does it go out of date, does it stand alone. The machinery does not care who
wrote the question. This spec lets the person write some.

A standing question is a yes-or-no question asked once of every note, old and
new. Its answers become a filter in the rail and, if wanted, a push when a new
note matches. It is what a tag would be if nobody had to apply it, and what a
saved search would be if it judged meaning rather than matched it.

The owner is not yet sure how they would use this. So the spec is small, and
most of its design goes into the one place where such a feature usually fails:
finding out, before saving, whether a question works.

What it is for, concretely:

- **A project's inbox.** "Does this bear on the house purchase?"
- **Things to act on.** "Does this ask me to do something I have not done?"
- **Things to re-check.** "Does this quote a price or a contract term?"
- **Commitments.** "Does this record a promise made to someone, or by someone
  to me?"
- **Agents.** An agent over MCP or REST gets a judged subset instead of a
  ranked list: `search` with `question=`.

## Goals

- Write a standing question, try it against the base, save it.
- Every live artifact gets an answer; new ones at capture, old ones by the
  scan.
- A chip per question in the rail filters search and, with an empty box,
  lists matches newest first.
- Optionally, a push when a newly captured note matches.
- The person can say an answer was wrong; that tunes the question's threshold.
- The same over REST and MCP.

## Non-goals

- `choice` and `score` standing questions. Yes/no only: a filter is binary,
  and Clef-flash is weakest on many classes.
- Questions about pairs ("contradicts something pinned"). Spec 2's wider pair
  net covers contradiction.
- Questions that trigger writing, tagging, moving or deleting. A standing
  question reads; it never acts on an artifact.
- Sharing questions between users.

## Anti-goals

- **A question nobody can trust.** A saved question whose matches are visibly
  wrong and cannot be corrected teaches the person to ignore the chip.
- **Cost that grows unseen.** Each question is one more reading of every
  artifact. The count is capped and the backfill is visible.
- **Push noise.** A question that would have pushed more than a handful of
  times last week says so before push is turned on.

## Constraints

- At most eight standing questions per user. They ride in the same request
  as spec 2's readings: on the owner's server a question added to a state
  already being read costs about 0.1 s, while a separate request would read
  the artifact again.
- Standing questions are readings in spec 2's sense: stored in
  `artifact_readings`, matched at `match_at`, never banded, never an unsure
  row.
- Instructions at most 500 characters, as magicshell validates.
- The manifest; push goes through the channels `jobs/remind.rs` already uses.

## Approaches

1. **A filter only at search time.** Ask the question of the hits of each
   search. No backfill, but slow on every search and unable to list matches.
2. **A reading stored per artifact (chosen).** Asked once, stored in
   `artifact_readings` beside spec 2's readings, filtered like any attribute.
   Costs a backfill per question; everything after is free.
3. **An embedding of the question as a soft filter.** No model call, but that
   is search, which engram already has.

The owner handed the how to the designer; approach 2 is the designer's pick.

## A standing question

- name: one to three words, shown on the chip
- instructions: the yes-or-no question
- match_at: the probability from which an artifact matches; starts at 0.70
- push: off by default
- version: bumped when the instructions change, which discards its readings
  and starts the backfill again

Stored per user in `standing_questions`. Readings go to `artifact_readings`
under the id `user:<id>`.

## Writing one

Settings gains a section "Questions". Creating or editing one is a single form
with a live trial:

```
Name          [ House        ]
Question      [ Does this bear on buying the house in Lindenweg?        ]
                                                              [ Try ]

Would match 6 of 20 tried
  yes 96   Offer from the notary, 2026-10-02
  yes 91   Bank call: fixed rate until 2036
  yes 74   Roof survey summary
  ─ ─ ─ ─  matches from 70
  no  41   Kitchen measurements
  no  12   Qdrant sizing
  …
[ Save ]   Asking the whole base takes about 15 min (1,840 notes).
```

- **Try** asks the question of 20 artifacts: the 12 nearest to the question's
  own embedding and 8 drawn at random. Sorted by probability, with the line
  where matching starts.
- Each tried row opens the artifact. The person adjusts the wording and tries
  again; nothing is stored until Save.
- The estimate is the note count times the measured time per reading: about
  half a second for a note of passage size on the owner's server, so a trial
  of 20 takes about ten seconds and shows its rows as they arrive. The
  backfill of a new question re-reads every artifact once; it runs in the
  scan, inside spec 2's `max_per_tick`, and yields to the person.
- With no trial run, Save runs one first.

## Where the answers show

- **Rail.** A chip per question beside the Kind chips. Selected, search is
  restricted to matching artifacts; with an empty box the rail lists matches,
  newest first. Chips combine with each other and with Kind.
- **While backfilling,** the chip carries the share done ("House · 40 %") and
  filters what has been read so far.
- **Artifact detail.** Matching questions show as a line of names. Each has
  "Not this"; a detail page opened through a chip also offers "Add to …" for
  questions that did not match. Both store the person's answer, which
  overrides the reading for that artifact and survives a version bump.
- **Push.** On capture, a match on a question with push on sends one
  notification: the question's name and the note's title. Turning push on
  shows how many notes captured in the last seven days would have pushed.

## Thresholds

`match_at` follows spec 2's tuning rule, with the person's "Not this" and
"Add to" as outcomes: too many "Not this" among matches raises it, "Add to" on
near misses lowers it, in steps of 0.02 within `[0.50, 0.95]`, once at least
ten corrections exist. The Questions section shows the current value and lets
the person set it.

## REST and MCP

- `GET /api/v1/questions`: id, name, instructions, match_at, push, version,
  read, total, matches.
- `POST /api/v1/questions`, `PATCH /api/v1/questions/{id}`,
  `DELETE /api/v1/questions/{id}`.
- `POST /api/v1/questions/try {instructions}`: the trial, without saving.
- `GET /api/v1/search?question=<id>` (repeatable); `q` may be empty when a
  question is given.
- `POST /api/v1/artifacts/{id}/questions/{qid} {answer: true|false|null}`:
  the person's override.
- MCP: `search` gains `question` (a name or id); a new tool `questions` lists
  them. No tool creates or edits one.
- CLI: `engram -s --question House "roof"`.

## Stages

1. Storage, the reading at capture and in the scan, the override.
2. Settings: the form with the trial.
3. Chips, the list with an empty box, the detail line.
4. Push.
5. REST, MCP, CLI.
6. Threshold tuning.

## Left to the builder

- Whether the filter is a Qdrant payload field (needed for filtered vector
  search at scale) or a SQLite post-filter with refill; the contained SQLite
  vector store must behave the same.
- How chips overflow on a phone.
- The sampling of the eight random trial artifacts.
- Whether deleting a question keeps its readings for an undo window.

## Calls made by the designer

- Yes/no only.
- A cap of eight, and 0.70 as the starting `match_at`.
- A trial is required before a question is saved.
- Standing questions never act on an artifact.
- No question creation over MCP: an agent writing questions that then run on
  every capture is a cost the person did not choose.

## Done when

- A question saved on a base of N artifacts ends with N readings, and a chip
  that lists exactly the artifacts at or above `match_at` plus the person's
  additions, minus their removals.
- Editing the instructions discards readings but keeps the person's overrides.
- A ninth question is refused with one sentence.
- With `[infer.decide]` unset, Settings shows the section as not configured
  and no chip exists.
- The checks in `AGENTS.md` pass.
