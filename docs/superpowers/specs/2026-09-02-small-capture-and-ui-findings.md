# Small captures and the workspace UI — findings, 2026-09-02

Verified on prod (`engram.mikoshi.cc`, HEAD `3f99e78`, clean tree, binary
built 14:31 UTC, capture 14:43 UTC). Model on both tiers:
`gpgpu/qwen3-5:9b-q8_0-32768-nothink`.

## Bugs

1. **One-sentence note yields four synthesized artifacts.** Corpus
   `01a06293…`, input "erinnere mich an den Gastroentereologentermin,
   Freitag 13:45 uhr." (65 chars). Reply 818 tokens; two artifacts dropped as
   context-only; four stored, titled "Event: Friday 13:45", "Reminder
   Intent…", "Event Date: 2026-09-05", "Link to…". They restate the JUDGE
   fields (`moment`, `events`, `links`) as artifacts. Causes: the system
   prompt says "split into more artifacts", the JUDGE block names three
   things, nothing forbids artifacts about the judgement, and `window.rs`
   has no cap on artifact count relative to input size.
2. **Verbatim passage never superseded.** `superseded=0` on every
   post-fork small capture (3 of 3). `covered_by` breaks span ties by lowest
   ordinal; here that is the one `unplaced` artifact, so the vector check
   never runs. Cross-language rewrites (German passage, English artifact)
   also fail both the verbatim-line rule and, likely, `traceable_min` 0.75.
3. **Wrong date.** "Freitag" on Wed 2026-09-02 resolved to 2026-09-05, a
   Saturday. Moment row confirms (`at=1788608700`). No weekday-vs-date check
   in `judgement.rs`.
4. **Synthesis in the browser's language, not the note's.** Corpus
   `lang=en` came from `Accept-Language` via `capture_lang`; the note is
   German. The prompt and therefore the artifacts came out English.
5. **Small-model bloat.** "Heute nachmittag um 1600 die butter mitnehmen"
   became a "## Procedure" with numbered steps. Same cause as 1.

## UI, confirmed against code

- Box reads as a form field; nothing on the page is primary.
- Canvas `#vec-bg` at opacity 0.72 never dims while typing; its axis lines
  cross the transparent topbar.
- Ask is disabled at opacity 0.4 on teal and still reads as the lit
  primary; Capture reads disabled; Attach reads enabled.
- Rail head says only "N results"; no close/loose split for mixed sets.
- Phone: hint and example chips `display:none`; placeholder clipped; the
  idle column lives inside the fixed bottom bar, leaving the top empty.
- Due badge meaning is tooltip-only (primed/loose/model-written already
  have a visible `rail-why` sentence).
- Empty base: three sentences (box hint, pane-idle, idle-foot) say one
  thing.
- Corpus page: "100% of wording survived" is trivially true while the
  passage is itself an artifact; passage card title repeats its body.
- Offer card meta line "Pattern · weekday, hour, network · like 26.08.,
  20:36" is internal vocabulary.
- Idle foot prints the whole raw first line as an underlined link.
- Keyhint says "keep", the button says "Capture".

## Rejected review claims (already true on this branch)

Box empties after Capture; capture feedback sits under the box; example
chips exist; Kind chips hidden on idle; keyhint absent on empty base; nav is
Insights + Settings; a Browse tab contradicts the documented design.

## Deferred, with reason

- Moving Kind chips under the rail head: the chips ride the search form's
  `hx-params`; moving them needs `hx-include` plumbing and a `change`
  listener re-scope. Separate change.
- Verb-row density beyond the above: Stop and the spinner only appear
  during an ask.
- The stray glyph after "Delete" on the corpus page: not reproduced in the
  templates or CSS; needs a browser inspection.
- Corpus page half-width source column: the band grid is deliberate
  ("a band is as tall as its source").

## What the branch answered

Every finding above is addressed by the plan at
`docs/superpowers/plans/2026-09-02-small-capture-and-ui.md`, with one
exception recorded below.

Bugs 1 and 5 are answered by the artifact allowance in `src/jobs/window.rs`
(one artifact per thirty input tokens, located artifacts kept first) and by
the paragraph added to all ten system prompts saying the judgement is not an
artifact. Bug 2 is answered by the tie rule in `covered_by`, which now
prefers a placed span over the whole-window fallback before falling back to
the lowest ordinal. Bug 3 is answered by `weekday_named` and
`onto_named_weekday` in `src/jobs/judgement.rs`: a weekday the note names
moves the model's resolved instant onto that weekday, and the move is logged
at warn.

Bug 4 is **not** addressed, by the operator's decision on 2026-09-02. The
capture language stays what the Settings choice or the browser's
`Accept-Language` says. A detector reading the text would be a third,
arbitrary source of the answer and one more thing for a reader to hold in
their head. The cross-language half of bug 2 stands with it: a German passage
and an English artifact still fail the verbatim-line rule, and only the tie
rule above makes the vector check reachable at all.

Each UI finding has its own commit, in the order listed: the verb accent and
the grey disabled verb, the fading cloud and the painted topbar, the phone
chips and the idle bar in the flow, the loose count in the rail head, the due
sentence on the row, the empty base introducing itself once, the corpus
page's coverage line and passage title, the offer card's sentence, and the
one-line idle foot.

The deferred items above stay deferred; nothing on this branch touched them.
