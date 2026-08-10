# Autonomous ingestion — Design

Date: 2026-08-10
Status: approved
Refines the ingestion and consolidation behaviour in
`2026-08-09-engram-design.md`. Supersedes nothing.

## 1. Why

engram is meant to keep a knowledge base the way an organism keeps itself:
work that fails gets tried again, damage that can be repaired locally is
repaired, and the operator hears about the one class of problem no machine can
settle. What it does instead today is hand every imperfection to a person as a
button.

The Ops screen of a two-corpus development instance carried, on 2026-08-10:

- Six artifacts flagged `span_unverified`, each offering to spend a model call.
- One artifact flagged `literals_unverified` for `Binär: 0010 1001 1111 1001`,
  whose digits are verbatim on line 635 of the source. The model added a label.
- One pair queued at 0.9488 with the same title on both sides, from one
  synthesis call.
- A quarter of one document — segment 1, lines 189–395 — with no artifacts at
  all, because the inference endpoint returned `502` for ten minutes while it
  loaded a model, and the job gave up permanently inside the first minute.

None of those four is a knowledge decision. Every one of them waits on a human.

## 2. The line

A person is asked exactly one kind of question: **two artifacts state a fact
differently and nothing in the system knows which is current.**

Everything else resolves itself, keeps an audit trail, and can be undone.
Nothing is ever deleted or rewritten to achieve it — the fidelity rule from the
original design still holds: consolidation hides, flags or asks, and never
merges.

## 3. Nothing is terminal

**Retry becomes unbounded.** `MAX_ATTEMPTS` currently means *give up*: five
attempts with backoff `2,4,8,16,32s`, then the segments that were tried are
marked `failed` and the job is closed. Against an endpoint that takes ten
minutes to warm up, the whole budget is spent in the first sixty seconds and
the work is lost until someone notices.

It comes to mean *slow down*. Backoff keeps doubling to a ceiling of six hours
and the job stays pending forever. A permanently unprocessable segment
therefore costs about four calls a day, which is visible and cheap; an endpoint
down for a weekend costs nothing and heals when it returns.

**A reconciliation sweep** runs on the consolidation timer and re-arms anything
unfinished that no job covers: a segment in `failed` or `pending` with no
synthesize job, an artifact `pending` embed with no embed job. It is the
heartbeat — the guarantee that work cannot fall through a crack permanently,
whatever the crack was.

**Ops reports state, not chores.** "Failed jobs" becomes "retrying, next
attempt in 4h".

## 4. Spans are derived, never adjudicated

Today the model asserts `corpus_lines`, engram checks the claim, and a claim
that fails the check becomes a flag on the artifact plus a button that spends a
model call to re-synthesise the whole segment.

The claim is not worth this. `locate_span` can now find an artifact's text in
its segment even when the source is hard-wrapped and synthesis reflowed it, so
the span is computable locally from data already stored. Therefore:

- The span is derived locally, always.
- The model's `corpus_lines` is used only when derivation finds nothing, as a
  hint, unclamped and unchecked.
- Derivation that finds nothing falls back to the segment, silently.
- `FLAG_SPAN` is removed. Not suppressed — not generated, because engram stops
  disagreeing with itself about a number it computes.

The reconciliation sweep re-derives spans for artifacts whose stored span came
from a model claim, which costs no inference and clears the existing flags.

## 5. Literal checks stop crying wolf

`missing_literals` compares a fenced line as a whole. A model that writes

```
Binär: 0010 1001 1111 1001
```

for a source that says `wird binär 0010 1001 1111 1001` has invented nothing;
it has labelled something. The check keeps its purpose — a command that gets
pasted into a root shell must have come from the source — by comparing the
machine-shaped part of a line rather than the line: strip a leading
`Word:`-style label before looking for the remainder.

What survives is a genuine miss. It stays on the artifact as a note the reader
sees in context, and it leaves Ops entirely. It is information about one
artifact, not a task for the base.

## 6. Duplicates that do not disagree are not questions

The review band between `review_min` and `auto_supersede` currently queues
every pair for a person. Most of them have nothing to decide.

- A queued pair whose fact tokens do not differ is closed automatically as
  `no_conflict`, with both artifacts kept. This is the prefilter that already
  exists, applied without waiting for the judge to be enabled.
- A pair whose facts differ goes to the judge when it is on, and a confirmed
  contradiction is the one thing shown to the operator.
- Auto-hiding stays at `auto_supersede`. It is deliberately *not* lowered into
  the review band: the original design records that two genuinely distinct
  artifacts about one subsystem sit at 0.88 routinely, and hiding on that score
  costs knowledge rather than duplication.
- One case is added, because it is a defect rather than a judgement: an
  artifact whose text is wholly contained in another artifact **from the same
  corpus** is a synthesis stutter — one call emitting the same passage twice —
  and the shorter one is hidden with the usual undo.

## 7. What Ops becomes

Two things:

1. **Contradictions.** Both texts, both sources, and no default answer.
2. **Health.** What is retrying and when it next runs; what is hidden and by
   what; coverage per corpus; queue depth.

Undo is the only button.

## 8. Testing

Every part is a pure function or a job step over fakes, as the rest of the
codebase already is:

- Backoff: the ceiling is reached and never exceeded; a job is never closed as
  permanently failed.
- Reconciliation: a corpus with a failed segment and no job gets one; a corpus
  that is complete gets nothing; the sweep is idempotent across two runs.
- Spans: a derived span wins over a model claim; a claim is used when
  derivation finds nothing; no flag is produced in either case.
- Literals: a labelled fenced line is not a miss; an invented command still is.
- Duplicates: a pair with no differing facts closes itself and both artifacts
  survive; a contained artifact from the same corpus is hidden; a contained
  artifact from a *different* corpus is not.

## 9. Out of scope

- Changing what gets embedded.
- Any automatic rewrite or merge of artifact text.
- Lowering `auto_supersede`.
- Migrating existing rows: the development base can be recomputed or rebuilt,
  and no deployed instance exists to preserve.
