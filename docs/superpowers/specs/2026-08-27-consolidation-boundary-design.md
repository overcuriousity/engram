# Consolidation boundary — Design

Date: 2026-08-27
Status: draft
Extends `src/jobs/relate.rs`, `src/jobs/embed.rs`, `src/store/artifacts.rs`,
`src/store/pairs.rs`.
Narrows an invariant the store already claims. See §4.3.

## 1. Why

On 2026-08-27 between 07:27 and 08:35 the running base wrote fifteen merged
artifacts in one chain. Each took the previous one and exactly one further
passage: the root counts run 2, 3, 4 … 16 without a gap, and the text grows from
1,255 to 12,123 characters. What stands at the end is a single active artifact
of 10,011 characters titled *Grundlagen der digitalen Forensik und
Beweismittel*, and behind it sixteen verbatim passages drawn from **thirteen
different corpora** are hidden from search.

That is the outcome `src/store/schema.sql:51` and the ROADMAP's fidelity rule
exist to prevent — a synthetic summary standing where stored passages used to.
It happened without anyone deciding it: all fifteen merges hang off a pair with
`merged_into` set, so every one of them came from the dedupe ticker. The
eighteen remaining pairs in the base were closed by an operator.

The numbers that matter, read from the live base:

| | |
|---|---|
| Passages | 11,473 |
| Captured artifacts | 9 |
| Synthesized | 2 |
| Merged | 15 |
| Corpora | 226 |
| Pairs ever filed | 33 |

Of those 33 pairs: 20 are passage against merged, 11 passage against captured,
one passage against passage, and **one** captured against captured. In its
entire life this subsystem has seen a single pair of the material it was
designed for, and an operator dismissed it.

### 1.1 The existing guard never fired

`classify_pair` refuses a pair only when the two sides share a corpus **and** a
`segment_idx` **and** one of them is a passage (`src/jobs/relate.rs:149-155`).
Nine of the 33 pairs sit in one corpus, and eight of those have different
segments — 4 against 3, 2 against 0, 1 against 0, 3 against 0. The ninth has
matching segments but is captured on both sides, so the passage clause fails.

The guard's hit rate on this base is zero.

### 1.2 The feedback edge

A merged artifact is not a passage, so nothing stops it querying. It embeds,
`mark_indexed` arms a relate unit for it (`src/jobs/embed.rs:340`), the unit
queries `per_point` neighbours, finds the next document's passage on the same
subject, and files a pair the ticker then merges. Twenty of the 33 pairs are
passage against merged, which is that edge in the data: merged artifacts did not
exist before the first merge.

The chain stopped only when `merge::losses` began refusing every further
merge — the five operator-closed rows still carrying the escalation text from
`src/jobs/dedupe.rs:348`.

## 2. The confusion this rests on

Similarity says two texts are alike. Duplication says one of them adds nothing.
The repository states the distinction plainly where containment is defined
(`src/jobs/relate.rs:108`): "Not a similarity — containment. A score says two
texts are alike; this says one of them adds nothing."

Admission to the model is nonetheless a cosine, `review_min`. On a base of 226
documents teaching one subject, passages from different scripts land at 0.89 to
0.93 while duplicating nothing — it is the same material taught by different
authors. Twenty-three of the 33 pairs are cross-corpus. A cosine cannot separate
"stored twice" from "taught thirteen times", and where it decides admission the
second is handled as the first.

The model did not fail here. Its verdicts are in the base and they are correct:
"Artifact A is a short excerpt from the much larger and more comprehensive
Artifact B." The defect is that the question was asked.

## 3. Goal

Duplicate hygiene sees only material that can be duplicated, and asks the model
only where something beyond topical similarity says it might be.

### Non-goals

- **Deciding which of two similar documents is better.** Out, as before.
- **Reducing storage.** Passages are the substrate and stay whole.
- **Changing what synthesis does.** `insert_synthesized_artifact` legitimately
  records passage sources (§4.3), and nothing here touches it.
- **Retrieval-time behaviour.** Unchanged. Everything here is background.

## 4. The change

### 4.1 The passage rule moves to `provenance`

In `classify_pair`: if either side is `Provenance::Passage`, no pair is filed.

This is the rule `relate::run` already applies to the asking side
(`src/jobs/relate.rs:41`) — a passage does not query. The pair side was given a
narrower rule for a different reason (one synthesis call emitting the same
passage twice), and that reason does not generalise to two passages from two
documents. Passages are the verbatim substrate; they are not artifacts anyone
claimed were duplicates of each other.

The containment supersession that the same block performs for a repeated passage
inside one corpus is kept and is now the *whole* of what passages participate
in: same corpus, one text wholly inside the other, deterministic, no model call.

### 4.2 A merged artifact does not arm a relate unit

At `src/jobs/embed.rs:340`, skip the arming when the artifact's provenance is
`Merged`.

`Merged` specifically, **not** `is_model_written()`. That predicate also covers
`Synthesized`, and a synthesis is ordinary dedupe material: the comment at
`src/jobs/relate.rs:143-148` records deliberately letting two written rows from
one window through, because one synthesis call emitting the same passage twice
is a defect worth finding. Excluding all model-written artifacts would take that
away to fix something else.

This removes the edge in §1.2 at its source, and it holds even if §4.1 is ever
loosened. The completeness argument survives: a merged artifact is never the
second member of a new pair, because whichever ordinary artifact is embedded
later still finds it.

What is given up: an artifact that already existed, was never near either root,
and *is* near the merged text will not be paired. Closeness to a union is weak
evidence of duplication with any member, so refusing that pair is right on its
own terms — but it is a behaviour change and gets a test that says so.

### 4.3 The root invariant is enforced, and narrowed to the merge path

`src/store/artifacts.rs:283` claims that `artifact_sources.root_id` "only ever
names a `captured` artifact — the invariant the whole anti-drift rule rests on".
In the live base, 135 of the merge path's 135 root rows name a passage.

The claim is enforced in `insert_merged_artifact`, which refuses a root whose
provenance is not `captured`, and **not** on the table: the same table carries
eleven rows for two `synthesized` artifacts, where passage sources are correct
and intended. A constraint on `artifact_sources` would break synthesis.

Refusing loudly is the point. The state this feature can produce silently is the
one worth a hard failure.

### 4.4 Admission needs a corroborant

`review_min` stops being sufficient. A pair reaches the model only when the
score clears `review_min` **and** one of two things holds:

- **containment** — one text is wholly inside the other, already computed in
  `classify_pair` and already the only ground on which anything is hidden
  unasked;
- **same corpus** — the two came out of one document, so one plausibly restates
  the other.

Cross-corpus similarity with neither is not a duplicate question.

Containment keeps the genuine cross-corpus case working: the same document
ingested twice produces texts that contain one another, and that pair is still
admitted. What is refused is two different documents that merely cover one
subject.

The trade-off, stated rather than hidden: two sources that say the same thing in
different words, in different corpora, are no longer merged at all — not
automatically, and not by a proposal an operator could apply. Four things argue
for it. Thirteen scripts agreeing is itself evidence, and a merge erases that
they agreed. `merge::losses` covers values and machine literals but not prose,
so the paraphrase case is exactly the one its net does not catch. A merged
artifact has `corpus_id` and `corpus_span` NULL by construction, so merged
paraphrase is wording that stands in no document a reader can be sent to. And
the complaint a merge answers — the same thing three times in one result list —
is a presentation problem with a presentation fix.

The case this gets wrong is one document re-ingested in another *form*:
reformatted, re-paginated, or translated — and `src/jobs/merge.rs:328` records
that a rewrite between German and English is routine here. A proposal path for
those was considered and **cut**: this collection does not hold the same lecture
in two versions. Recorded so it is a decision and not an oversight; if that
changes, `PairState::Superseded` plus `apply_supersede_ui` is the path, and no
new mechanism is needed to add it.

### 4.5 What falls out is written nowhere

A pair refused by §4.4 files nothing, and in particular does **not** bump
`artifact_links`. A link means two artifacts were used together — `bump_link`
takes the cue that bound them, and the README calls the graph one learned from
co-retrieval. A bump derived from a cosine would put an observation about text
into a table whose every other row is an observation about behaviour. Two
documents on one subject link on their own, from the first search that shows
them together.

This also closes a gap in the verdict contract: `Distinct` means different
subjects, and there is no verdict for "same subject, legitimately separate
sources". None is needed once such pairs are never asked about.

## 5. Configuration

No new knobs, no changed defaults. §4.4 narrows what `review_min` admits without
changing what it is. `config.example.toml`'s `[consolidate]` block gains a
sentence saying that similarity alone no longer admits a pair, and why.

## 6. Not in this change

**`artifact_pairs.decided_by`.** Whether a pair was settled by the model or by
an operator is today reconstructable only by accident — `dismiss_pair_ui` passes
`None` and nulls `detail` (`src/web/ui.rs:2487`) while `apply_supersede_ui`
carries it through (`src/web/ui.rs:2574`). A column saying so plainly is worth
having and is one line of schema, but it belongs to review and not to the
boundary. Separate change.

## 7. Repairing the running instance

**Separate from the code change and requiring explicit approval before it is
run.** Nothing here is a consequence of deploying §4.

The fifteen merges are undone with `merge::undo`: the roots are reactivated, the
merged artifact goes to `deprecated` rather than being deleted, and the pairs
that named it are set `Dismissed` in the same action. The last part is not
optional — `record_pair` is `INSERT OR IGNORE` and a dismissed row is what makes
the undo survive the next sweep (`src/jobs/consolidate.rs:700` pins the bug).

Sixteen passages from thirteen corpora return to search; one synthetic document
leaves it. `data/users/<id>.db` is copied first.

## 8. Open question

The chain's ignition is pair 25, filed 2026-08-27 07:13:47 between two passages,
and its `score` is **0.0**. Every other pair in the base lies between 0.8887 and
0.9623. `relate` skips anything below `review_min`, so that row cannot have come
from the neighbour loop as written.

This is not resolved here and no mechanism is proposed for it. The place to look
is the instance journal at that timestamp; the warning "neighbour came back with
no cosine" (`src/jobs/relate.rs:65`) would appear there if that path was
involved. §4.1 makes this particular pair impossible either way, which is why it
does not block the change — but a score no admission rule can produce is worth
understanding before it appears somewhere the guard does not cover.

## 9. Testing

**The boundary**
- `a_pair_with_a_passage_on_either_side_is_never_filed` — the rule §4.1 makes
  general. Both cross-corpus, which is where the old guard missed every case.
- `two_passages_from_different_documents_about_one_subject_are_not_a_pair` —
  carries the FAT12/FAT16/FAT32 lesson forward to the substrate: thirteen
  scripts on one syllabus are thirteen sources.
- `a_repeated_passage_inside_one_corpus_is_still_superseded_by_containment` —
  what §4.1 must not break.
- `a_merged_artifact_does_not_arm_a_neighbour_query`
- `an_artifact_near_a_merge_but_near_neither_root_is_not_paired` — the cost of
  §4.2, pinned so it is a decision and not a regression.

**The invariant**
- `a_merge_whose_root_is_a_passage_is_refused` — the 135 rows, made loud.
- `a_synthesized_artifact_may_still_name_passage_sources` — why the check is on
  the merge path and not the table.

**Admission**
- `cross_corpus_similarity_alone_does_not_reach_the_model`
- `containment_across_corpora_still_reaches_the_model` — the re-ingested
  document.
- `a_refused_pair_does_not_bump_a_link` — §4.5. The link graph stays an
  observation about use.

## 10. What this does not change

Retrieval still returns whole artifacts and never generated prose. A captured
artifact is still never rewritten in place. Merging, where it still happens,
keeps its verification, its lineage and its undo. Detection stays complete for
the material it now covers: one neighbour query per non-passage artifact, no
sampling, coverage independent of base size.
