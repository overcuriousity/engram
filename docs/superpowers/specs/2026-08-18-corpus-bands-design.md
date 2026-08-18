# The corpus page in bands, and a nav that holds still

Two things, one branch. The nav width regressed when the page measures split,
and the corpus page's "never reached an artifact" section shipped as a list of
a hundred line numbers between the source and the artifacts — technically true
and useless to act on.

## The nav

`nav.top` is rendered inside `.shell`. When search and housekeeping opted into
`.shell-wide`, the nav went with them: it is 60rem on Capture, Corpus, Ask,
Judge and Settings, and 110rem on Search and Housekeeping. Navigating moves it.
Its bottom border stops at the same varying edge, so the rule under the header
changes length too.

Chrome should not take its width from the content beneath it. The nav moves out
of `.shell` into its own full-bleed `<header>`: the border spans the viewport,
and the row inside it is capped at `110rem` — the wider of the two measures, so
the nav aligns with content on the pages that use it and simply runs wider than
the reading column on the pages that do not. Chrome that is always the same
width is worth more than chrome that sometimes lines up.

## What is wrong with the gap list

Three things, and only the third is cosmetic.

**It measures the wrong thing.** Red comes from `content_coverage`, which asks
whether over half of a line's distinctive words survived into the artifact text.
Synthesis rewrites — that is what it is for — so a faithfully paraphrased line
scores as missed. On a 978-line corpus this produced about a hundred ranges,
most of them single lines, many of them lines whose content did reach an
artifact in other words.

**It is detached from what it describes.** A list of `line 3`, `line 7`,
`line 32` sits between the source and the artifacts, naming lines the reader
then has to go and find. The anchors help; they do not make the list a place to
work.

**It cannot be acted on precisely.** One button re-reads every window holding a
gap. There is no way to say "this passage, not the other forty".

## Red means unclaimed

The measure changes. A line is red when **no artifact claims to have been
written from it** — when no captured artifact's `corpus_span` covers it.

This is what the heading already says. It is also structural rather than
statistical: `resolve_span` (`window.rs:280`) computes each artifact's span
itself from its text, clamped into its own window, using the model's
`corpus_lines` only as a hint — so every captured artifact has a span, and the
spans partition the document into passages that were claimed and passages that
were not. Those come out few and large where the old measure came out many and
tiny, and every one of them is a real re-read target.

`content_coverage` stays exactly as it is. It still computes the `% covered`
figure on the Recent list, which answers a different and worth-asking question:
not *was this passage claimed* but *did its wording survive*.

`verify::uncovered_ranges` has no remaining caller once the page stops using it,
and is deleted with its tests. The `line_coverage` pass it shares with
`content_coverage` stays.

### Where the two measures disagree

They will. A corpus can read `55% covered` on Recent and have no unclaimed
passages at all: every line inside some artifact's span, only the wording
rewritten. Following the warning would then land on a page with nothing red —
which is the dead end this whole feature exists to remove.

So the corpus page states both. The bands answer "what was never claimed". One
line under the heading answers "how much of the wording survived", and the
`#uncovered` anchor lands on that line when there are no red bands to land on.
Neither number is presented as the other, and neither is hidden because it is
inconvenient.

## Bands

`corpus_detail` stops passing a flat list of lines and a separate list of
artifacts. It passes bands:

```rust
/// One stretch of the source and whatever was written from it.
struct Band {
    from: i64,
    to: i64,
    lines: Vec<CorpusLine>,
    /// Empty for a gap band — the whole point of the band.
    artifacts: Vec<ArtifactView>,
    /// The segment holding these lines, for the re-read button. `None` for a
    /// corpus with no segment rows, which can offer no re-read.
    window_idx: Option<i64>,
}
```

Assembly walks the source once. For each line, the set of captured artifacts
whose `corpus_span` covers it; a band ends where that set changes. Rules the
walk has to get right:

- **Overlapping spans** produce a band per distinct set, so two artifacts
  overlapping on lines 40–45 give three bands: A alone, A+B, B alone. No merging
  and no arbitrary tie-breaks.
- **A run covered by nobody** is a gap band, and renders red.
- **A run of only blank lines** joins the preceding band rather than becoming a
  gap. `content_coverage` already ignores blank lines; a red sliver between two
  paragraphs would be noise with nothing to re-read.
- **A gap at the very head or tail** of the document is an ordinary gap band.
- **Merged artifacts never appear.** They belong to no corpus and carry no span
  into one. `artifacts_for_corpus` already returns only this corpus's own.

The bands **replace** the "Raw corpus" card and the "Artifacts" list beneath
it. Both said the same things in two places that could not be read together,
which is the complaint. Every captured artifact has a span, so every artifact
appears in exactly one band and nothing is dropped by removing the list; the
per-artifact controls (`edit`, delete) move into the band cell with it.

**Every line keeps its `L<n>` anchor.** An artifact's "open at these lines" link
and the `?from=&to=` highlight both address lines by that id, and banding must
not break them: the anchors move onto the rows inside the bands, unchanged.

Rendered as a two-column grid, one row per band, heights ragged: a band is as
tall as its source, and its right cell is as tall as it needs to be. Nothing is
folded, nothing is hidden, nothing scrolls inside itself. The page is as tall as
the document, which it already is today.

## Re-read, one band at a time

`POST /ui/corpora/{id}/reread` takes a `from` line naming the band. It finds the
segment window holding that line, resets it, and enqueues one `SegmentWindow`
job — the same mechanism as today, aimed at one place instead of all of them.
The window is wider than the gap, which is what "with the context around it"
means: the model reads the passage in its surroundings, not stripped of them.

Nothing already written from this capture is replaced. What comes back is added,
and anything it repeats is folded by the dedupe sweep like any other near
duplicate. The confirmation says so, because that is not obvious.

Two red bands can fall inside one window, and pressing either re-reads both. The
button says which lines it will actually read rather than only naming the band
it sits in.

The page-level "read them all" button is removed. Every re-read is now a
deliberate, scoped decision, and there is no single click that spends eight
model calls.

## What keeps today's rendering

- **Restored placeholders.** The page already warns that its text is the corpus's
  own artifacts joined back together, and that line numbers and spans mean
  nothing there. Bands would be a claim that arrangement cannot support, so a
  restored corpus renders as it does now.
- **An image corpus with no reading yet.** No text, no spans, nothing to band.
- **An image corpus that has been read** gets bands like any other: its spans
  point into the transcription, which is what the page shows and labels.

## Testing

Band assembly is a pure function of lines and spans, and is unit tested as one:
adjacent spans, overlapping spans, a gap at the head, a gap at the tail, a run
of blank lines between two spans, one artifact spanning the whole document, and
a corpus with no artifacts at all (one gap band over everything).

Page tests: every line still carries its `L<n>` anchor and a `?from=&to=` deep
link still highlights; a gap band renders a re-read button carrying the right
`from`; a
covered band renders its artifacts in the same row; posting a re-read resets
exactly one segment and leaves the others alone; a restored corpus renders no
bands; the `#uncovered` anchor exists whether or not anything is red.

The nav: one test asserting the header renders outside `.shell` on both a narrow
page and a wide one.

## What this does not do

- No change to how coverage is computed or to the number on Recent.
- No folding, virtualisation or pagination of long documents.
- No editing of source text, ever.
- No change to what synthesis writes — only to how the page reads what it wrote.
