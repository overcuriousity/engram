# Segment context — Design

Date: 2026-08-12
Status: approved
Refines the segmentation behaviour in `2026-08-09-engram-design.md`.
Supersedes nothing.

## 1. Why

The synthesizer is asked to produce artifacts that stand alone — "Resolve
pronouns and implicit references: 'this command' becomes the actual command"
— and is then handed a window of text with no way to know what document it
belongs to or what came immediately before it.

At line 840 of an administration guide, a window opens mid-procedure. The
model cannot resolve "the datastore created above" because the creation is in
the previous window. It cannot record that the procedure is specific to
version 3.x on Debian 12, because that is stated in the document's opening and
the opening is eight hundred lines away. The `caveats` field asks for exactly
that kind of qualification — "a prerequisite, a version or platform it is
specific to" — and for every window after the first, the model has nothing to
draw on.

The result is artifacts that are locally accurate and globally unmoored: right
about the command, silent about what it applies to.

## 2. What each window is given

Four blocks, in this order:

```
DOCUMENT OPENING   first H tokens of raw_text     omitted for window 0, or when
                                                  it would overlap the window
PRECEDING CONTEXT  last O tokens of C(i-1)        omitted for window 0
INPUT              C(i), the stored span          the only extractable block
FOLLOWING CONTEXT  first O tokens of C(i+1)       omitted for the last window
```

`C(i)` is window `i`'s stored core span. The two roles are distinct and were
kept distinct on purpose:

- **The document opening** answers "what is this". It is global framing:
  product, version, platform. Identical bytes for every window of a corpus.
- **The neighbouring context** answers "where am I". It is local continuity:
  what the pronouns point at, which procedure is in progress. Different bytes
  for every window.

The document's *ending* is deliberately not included. A document closes with
references, appendices and changelogs, which frame nothing and cost the same
as the opening.

Every context block is cut at a line boundary and carries no line numbers.
Only INPUT is numbered, so `user_prompt`'s "the input below starts at line N"
keeps meaning what it means today.

The ordering is load-bearing beyond readability: the system prompt followed by
the document opening is a byte-identical prefix across every window of a
corpus, which a prompt cache or a llama.cpp slot can reuse. Everything that
varies sits after it.

## 3. Context comes from the stored windows

> **Revised during implementation, 2026-08-12.** This section originally
> specified that context be derived at call time from `raw_text` and the
> neighbouring line ranges, with no schema change. That was wrong, and the
> pre-send guard of section 6 is what caught it. Line numbers cannot address a
> unit smaller than a line, and the splitter cuts *inside* a line for a corpus
> that has none — so re-derivation returned the whole document for window 0 and
> an empty string for every window after it. The windows now carry their text.

Each window row stores the text the splitter produced for it, alongside the
line range it came from. The range keeps its one remaining job — rendering
where an artifact came from — and the text is what is actually sent. Rows are
still written `ON CONFLICT DO NOTHING`, one per window.

Both context kinds are read from the neighbouring rows. A retry of window 4
reconstructs byte-identical context from the stored windows, so the
idempotency `upsert_segments` and `pending_segments` depend on survives — and
survives more strongly than the original design allowed, since the text no
longer depends on `raw_text` being sliced the same way twice.

It also fixes the shape of a rejected alternative. A generated summary would
be denser than a verbatim opening, but producing one requires a call that
reads the whole corpus, which is the cost segmentation exists to avoid. The
two ways around that are worse:

- A **rolling summary** spends output tokens on every window and compounds its
  own errors — a misreading in window 2 poisons windows 3 through 40 with
  nothing to catch it.
- **Titles so far**, built from artifacts already emitted, costs no extra call
  at all, and is rejected on architecture rather than price: it makes window
  `k`'s prompt depend on window `k-1`'s *output*. Today window 4 can be
  retried without window 3, and `write_segment_artifacts` replaces one
  window's chunks without disturbing the others. That independence is what
  made a partially-failed corpus recoverable. It is not worth a preamble.

A verbatim opening is also source text rather than synthetic text. When it is
wrong, it is wrong in the way the document is wrong.

## 4. Duplicate suppression is structural

Overlap puts the same passage in the input of two windows, and the fidelity
rule means a duplicate artifact is flagged rather than merged — so duplicates
accumulate rather than resolve. The prompt will state that context blocks are
reference-only. Small local models follow such instructions unevenly, so a
backstop is required that does not depend on the model's cooperation.

`locate_span` already searches for an artifact's text within the window text.
It is now given the **core text only**, and the outcome decides:

| Locates in core | Locates in a context block | Action                            |
| --------------- | -------------------------- | --------------------------------- |
| yes             | —                          | normal path, span computed as today |
| no              | yes                        | **drop the artifact**             |
| no              | no                         | keep it, and `flag_unverified` as today |

The third row is why the rule is shaped this way rather than "drop whatever
fails to locate". An artifact the model reworded heavily fails to locate
anywhere, and today that is flagged rather than discarded. Checking the
context blocks explicitly separates *came from next door* from *was rewritten
hard*, and only the first is a duplicate.

Dropped artifacts are logged with the window index and the block they were
found in, because a rising count is the signal that the prompt's
reference-only instruction is not landing with the configured model.

## 5. Budget

`segment_tokens` already subtracts `prompt_overhead` from the context before
dividing what remains between input and output. The context budget — `H + 2*O`
plus the fence text — is folded into `prompt_overhead`, so the core window
shrinks by exactly what context consumes and the assembled prompt still fits.
No other call site changes.

Defaults, configurable, expressed as absolute token caps rather than fractions
so that a large context makes them negligible instead of proportionally
wasteful:

| Knob | Default | Meaning                                  |
| ---- | ------- | ---------------------------------------- |
| `H`  | 200     | tokens of document opening               |
| `O`  | 150     | tokens of each neighbouring context block |

The price, stated plainly: 500 tokens off each window's core. At a 4k core
that is roughly 14% more windows and therefore 14% more synthesizer calls. At
8k it is roughly 7%. Setting `O` to 0 disables overlap; setting `H` to 0
disables the opening. Both at 0 reproduces today's behaviour exactly for any corpus today handles,
which is also how the change is tested. The single exception is the corpus of
section 6, which today is not windowed at all.

## 6. The single-line floor

`split_into_segments` currently guarantees nothing about the size of what it
returns. Its cut is expressed as `overflows && (at_boundary || buf_tokens >=
segment_tokens)`, and `overflows` is guarded by `!buf.is_empty()` — so a
corpus consisting of one very long line, which is what a paste from a PDF or a
chat transcript looks like, produces exactly one window of unbounded size.
That window is sent to the synthesizer, overflows the model's context, and
fails; the error is retryable and `store/jobs.rs:179` states there is no
terminal state, so it retries with growing backoff forever.

`split_into_segments` gains a character-level cut as the last resort, for a
single line that exceeds the budget on its own. Boundary preference is
otherwise unchanged — headings, then blank lines, then a hard line cut.

A guard at the top of the synthesize loop checks the window before it is sent.
It holds windows to **twice** the budget rather than to the budget itself,
which is what the splitter actually promises: it flushes once the buffer has
reached the budget, and `flush` then prepends the carried heading, so a window
legitimately lands somewhat over. Twice is the bound the splitter's own
`text_with_no_structure_still_splits_by_line_cap` has always asserted. What
must never happen is unbounded.

This is the same lesson as the embed loop of the same week: a splitter that can
return something it was asked to shrink but did not, plus a caller that assumes
it shrank, is a job that spins.

## 7. Testing

Each of these is a failing test before it is an implemented behaviour.

1. **Context assembly is reproducible.** Building window `i`'s prompt twice
   from the same stored spans yields identical bytes.
2. **Window 0 has no preceding context and the last window no following
   context**, and neither case emits an empty fence.
3. **The opening is omitted when it overlaps the window itself**, so early
   windows do not carry their own first lines twice.
4. **Spans stay in core coordinates.** An artifact drawn from the middle of a
   window resolves to the same corpus lines with context enabled and disabled.
5. **An artifact located only in a context block is dropped**; one located
   nowhere is kept and flagged. This is the table in section 4, one test per
   row.
6. **`H = 0, O = 0` reproduces today's windowing exactly**, span for span,
   against a fixture corpus that today windows correctly — which excludes the
   single-line corpus of item 7.
7. **A corpus of one long line with no newlines is windowed within budget**,
   and every window satisfies the pre-send guard.
8. **The core budget shrinks by the context budget**, so an assembled prompt
   for a worst-case window stays within the model's context.

## 8. Risks accepted

**Mid-flight corpora will have double-covered tails.** Window geometry is
re-derived from config on every run, but spans are written `ON CONFLICT DO
NOTHING`. A corpus already holds rows for all of its windows; under the
smaller core budget the same text needs more of them, so the next run appends
rows at indices past the old count, describing the tail of the document under
the *new* geometry — lines the old rows already cover. Those lines are
synthesized twice, and the duplicates are flagged rather than merged.

This was a live decision: the guard is a `DELETE FROM segments WHERE
corpus_id = ?` for any corpus not fully `done`, a few lines and no schema
change, and it was deliberately not taken. Note that `pending_segments`
includes `failed` windows on purpose, so a corpus can sit in the affected
state indefinitely.

**The backstop is not total.** An artifact the model extracted from
`FOLLOWING CONTEXT` *and* reworded past recognition locates in neither block
and is kept, flagged, as a duplicate. The drop counter in section 4 is how
this becomes visible rather than silent.

**Geometry still depends on live config.** Changing the model or the context
configuration re-windows every corpus that is not yet complete. This predates
the change and is unaffected by it.

**A window's stored text and its line range can disagree.** For a corpus the
splitter had to cut inside a line, several windows report overlapping or
identical ranges, because a range cannot express a sub-line unit. The text is
authoritative; the range is a pointer for rendering source, and for such a
corpus it points coarsely. Nothing reads the range expecting to reproduce the
window.
