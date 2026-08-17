# Frontend adjustments

A review of the browser interface — capture, search and housekeeping — turned up
a set of issues an operator meets on every visit. This is the design for fixing
them. It ships as one branch on `feat/ask-harness`.

The changes are mostly presentational, with three exceptions that are not:
`category` becomes a closed vocabulary and needs a backfill, low coverage gains
somewhere to go and something to do, and housekeeping splits into two pages.

## What is wrong

Grouped by the surface it appears on. Each item is referred to by its number
below; the sections that follow say what happens to it.

**Capture**

1. The Capture button sits between the textarea and the note input that feeds
   the same page — the submit control is above one of its own fields.
2. The note input says "for the file" and is always visible, including when
   there is no file and never will be.
3. Every pending pair leads with `These two disagree — N% alike.`; the artifact
   titles, which are the content, are buried mid-sentence.
4. `the merge would have lost 1, 4` — lowercase, and the values it names are
   bare numerals carrying no evidence.

**Recent**

5. Two formats in one column: a `55% covered` badge on some rows, plain
   `239 artifacts · 72%` on others. The badge widths make the timestamps wobble,
   and the two rows read as two different kinds of thing.
6. The low-coverage warning leads nowhere. Nothing on the corpus page mentions
   coverage, so the badge states a problem and offers no way to act on it.

**Search**

7. The KIND row carries two taxonomies: form words (`concept`, `procedure`) and
   subject words (`System Administration`, `Cryptography`), because `category`
   is an unconstrained string the model fills in freely.
8. The TAG row carries the same concept twice in two languages —
   `forensics`/`forensik`, `security`/`sicherheit` — because nothing normalises
   model-written tags.
9. The tag row truncates at the facet limit with no indication that it has.
10. `embed 0ms · total 10ms` is debug telemetry rendered to the operator.
11. Column proportions waste the viewport: the artifact pane is narrow enough to
    break a filename across six lines while the space beside the source pane
    sits empty.
12. The source pane clips lines horizontally behind a hairline scrollbar.
13. `LINES 576–576 HIGHLIGHTED` — a single line stated as a range, and the line
    it points at is `....`.
14. Clicking a result never paints a selected state.
15. Query-term highlighting appears on one card and not the rest.
16. A result with no excerpt collapses to a title, so card heights swing from
    one line to four.
17. No result count.
18. Cards recalled by association are styled identically to ranked hits.

**Housekeeping**

19. `1236 artifacts, 1236 embedded. 1523 done. 45 links, 0 named, 0 waiting on
    the judge.` — the job count reads as an artifact count, and nothing says
    what `done`, `links` or `named` count.
20. Rows that cannot be told apart: an artifact merged from two sources both
    titled exactly as the result is.
21. Two names for one reversal: `Undo merge` and `Put it back`.
22. Both tables are unbounded, unsearchable and unsorted.
23. One page holds six tables plus the browser extension, API tokens and
    feedback purge.
24. Two API tokens with the same name, both never used, no way to tell which is
    which.

**Global**

25. Every page is locked to a 60rem measure, including the tables and the
    three-pane search that have no reason to be.
26. Housekeeping rows carry four underlined links each and read as a wall.
27. `covered`, `done`, `embedded`, `named`, `links` and `alike` are used without
    a consistent meaning across pages.

Two things the review raised are **not** defects and are left alone. The
relevance cliff already renders (`_results.html:25`) and was simply absent from
a result set with nothing past it. The replacement glyphs in the source pane are
in the stored `raw_text` — PDF bullets that did not map — and rewriting stored
source to make it render prettily is the one thing this system does not do.

## Width

`.shell` is a single `max-width: 60rem` applied to every page. It is right for
prose and wrong for everything tabular.

`layout.html` gains a `{% block shell_class %}`. The default stays `60rem`, so
capture and artifact reading are unchanged. Search and housekeeping opt into
`.shell-wide { max-width: 110rem }`.

On search the filter form takes its own `max-width: 48rem; margin-inline: auto`,
so the input stays narrow and centred over a workspace that is not.
`.workspace` goes from `20rem 1fr` to `22rem 1fr`, and `.split` inside the pane
becomes `1fr 1.2fr` — that split is what renders as the third column. The
existing 60rem and 40rem breakpoints stay as the collapse ladder down to one
column on a phone.

Fixes 25 and 11, and is what makes 12 solvable rather than merely rearranged —
a pane with room in it is the precondition for wrapping lines instead of
clipping them.

## Capture

The Capture button moves out of `<form>` and carries `form="capture"`. DOM order
becomes textarea, drop zone, note input, button. This is valid HTML, htmx still
intercepts the submit through the form's own `hx-post`, and the note input stays
outside the form — which is why it was placed after the button in the first
place: the form posts urlencoded and the file goes multipart to a different
endpoint. Fixes 1.

The note input moves directly under the drop zone it serves and its placeholder
becomes `Note for the file you drop next (optional) — what is it, why keep it?`.
It cannot be revealed on drop, because a drop uploads immediately — the note has
to be fillable before the file arrives. Fixes 2.

The pending-pair card leads with the titles:

> **Speicherorte der MS Mail App** vs **MS Mail App File Locations** — disagree,
> 94% alike.

Fixes 3.

`dedupe.rs:297` stops writing the loss sentence. The check itself is unchanged
and the pair still escalates to `Conflict`, so it still reaches the operator —
what goes is a detail line whose evidence is a list of bare tokens. Cards
without a judge-written detail simply show none. Fixes 4.

## Coverage

Every settled row in Recent renders one shape: `N artifacts · X% covered`, laid
out on a grid so the timestamp column holds still whatever the row says. Rows
that are not settled keep their status badge, which is a different statement and
should look like one. Below `LOW_COVERAGE` the percentage takes the warning
colour — the warning is carried by colour, not by a different layout. Fixes 5.

The percentage on a low row links to `/ui/corpora/{id}#uncovered`, where the
corpus page gains an **Uncovered** section: the line ranges no artifact carried,
and a **Read these again** button.

This needs two new pieces:

- `verify::uncovered_ranges(raw_text, segments) -> Vec<(i64, i64)>` — the same
  per-line token-recall logic `content_coverage` already runs, returning the
  ranges instead of the fraction. Both are refactored onto one pass over the
  lines so the number and the ranges can never disagree.
- `POST /ui/corpora/{id}/reread` — enqueues `Stage::SegmentWindow` for each
  uncovered range, then `recompute_coverage` when they settle.

Fixes 6.

## Search filters

`category` is a field about **form** — what kind of thing an artifact is. It is
not a field about subject, and the subject words in it arrived only because
nothing stopped them. It becomes a closed enum in the synthesis schema:

    concept, procedure, reference, snippet, configuration, definition,
    example, other

These are form words, true of any corpus. Constraining the field is what keeps a
domain out of the schema; leaving it open is what let one in. Parsing maps
anything off-enum to `other` rather than rejecting the artifact.

The backfill rewrites off-enum values in the `artifacts` table and patches the
matching Qdrant payloads. Fixes 7, and 9 as a consequence — a closed vocabulary
cannot outgrow the facet row.

Model-written tags go: `prompt.rs` stops asking for them, and the TAG facet row
and the chips in `_artifact.html` and `_artifact_detail.html` are removed. No
domain-agnostic vocabulary exists for subject terms, so the choice is between
leaving them unnormalised forever and not generating them.

**The `tags` field itself stays.** It is load-bearing beyond the UI:
`qdrant.rs:54` uses a `pinned` tag as the ranking-boost channel precisely
because `PATCH /api/v1/artifacts/{id}` can edit tags without re-embedding,
`api.rs` exposes tags as a public search filter and PATCH field, and
`mcp/mod.rs:23` prints them. Removing the field would break pinning and the API
contract to fix a facet row. So it survives as a caller- and system-controlled
channel, with nothing writing to it automatically. Existing tags stay in the
database — they are still true, still filterable through the API, and simply not
rendered. Fixes 8.

## Search results

`#timing` leaves the page. The measurement stays available as a `Server-Timing`
header, which is where a browser already knows to show it. Fixes 10.

The result count is stated once above the list. Fixes 17.

`app.js:142` sets `aria-selected` only from the arrow-key handler, so clicking a
result leaves every card `aria-selected="false"` and the styling at
`app.css:338` never applies. This is a defect, not a design gap. An
`htmx:afterSwap` handler on `#pane` marks the card whose href matches what was
loaded, and the arrow-key path keeps working unchanged. Fixes 14.

Query-term highlighting is applied to every card rather than whichever one it
currently reaches. Fixes 15.

`.rail-snippet` gets a minimum height so a result with no excerpt cannot
collapse to a title. Fixes 16.

Associated cards get a treatment distinct from ranked hits — they were not
ranked against the query and should not look as though they were. The existing
`Recalled by association` rule stays; the cards under it stop borrowing the
ranked card's shape. Fixes 18.

The source pane soft-wraps with a hanging indent so no line is cut off, and its
height is matched to the artifact pane beside it. A single-line citation is
labelled `Line 576`, not `576–576`. Where the cited span contains only
punctuation or whitespace, the shown window widens to the nearest lines with
content and the label says the span is thin. Fixes 12, 13.

## Housekeeping

The page splits by what its contents are about.

`/ui/ops` keeps everything about the corpus: merged, deprecated, worth a second
look, hidden as near-identical, captures waiting on a decision, retrying.

A new `/ui/settings` takes everything about the installation: the browser
extension, API tokens, and the feedback recording notice with its purge. The
quiet link at the foot of Capture stays pointed at housekeeping; settings is
reached from a link beside it. Neither enters the top nav, which stays
deliberately minimal. Fixes 23.

The stats sentence names its units, so a job count cannot be read as an artifact
count:

> 1236 artifacts, all embedded. 1523 jobs done, none queued. 45 links between
> artifacts, 0 named, 0 waiting on the judge.

Fixes 19.

Merged and hidden rows gain a second line under each title — created date and
opening words — so two artifacts with the same title are distinguishable. Fixes
20.

`Undo merge` and `Put it back` both become `Undo`; the column heading carries
the meaning, as it already does for the icon columns further down the page.
Fixes 21.

Both tables cap at 25 rows with a `show all N` control. Sorting and search are
not added: these tables are read to answer "what happened to X", and the answer
to that is a search across the base, not a column sort. Fixes 22.

The tokens table shows the last-used time as a relative phrase and the user
agent that minted the token, which is what distinguishes two tokens with the
same name. Fixes 24.

In both tables only the primary artifact link stays underlined; source links go
unstyled until hover. Fixes 26.

## Vocabulary

One word per concept, applied in the templates:

- **covered** — the share of a capture's content that survived into artifacts.
- **embedded** — a vector exists for this artifact.
- **jobs** — queued background work, always said with the word `jobs`.
- **links** — recorded relations between two artifacts.

No glossary page: a glossary is what a system writes instead of using its words
consistently. Fixes 27.

## Testing

`ui.rs` already renders pages in tests and asserts on their markup
(`the_capture_page_lists_knowledge_gaps_by_group_and_lets_one_be_covered` is the
pattern). The same approach covers:

- The capture page's DOM order: the note input precedes the Capture button, and
  the note input is not inside the posted form.
- A settled Recent row renders `N artifacts · X% covered`, and a low-coverage
  row links to the corpus anchor.
- An off-enum category is stored as `other`; an on-enum one is stored as itself.
- No tag chips render on an artifact that has tags, and the tags survive a round
  trip through the API.
- `/ui/settings` serves the tokens table and `/ui/ops` no longer does.
- A pair whose only detail was a loss message renders no detail line.
- Clicking a result marks it selected (asserted on the handler's behaviour, in
  the existing JS-free way: the fragment carries what the handler needs).

Unit tests outside the web layer:

- `uncovered_ranges` against a corpus with a failed segment returns that
  segment's lines, and agrees with `content_coverage` on the same input.
- The backfill leaves SQLite and Qdrant agreeing on every artifact's category.

## What this does not do

- No dark mode work, no palette changes. The colour scheme is deliberate.
- No pagination beyond the 25-row cap; no sort or search on housekeeping tables.
- No change to how pairs are found, judged or scored — only to how they read.
- No rewriting of stored source text, whatever it contains.
