# Consolidation Boundary Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop duplicate hygiene from consuming verbatim passages, and stop topical similarity alone from buying a model call.

**Architecture:** Four independent guards, each a few lines at one call site. Passages never reach `record_pair`; a merged artifact never arms a neighbour query; `insert_merged_artifact` refuses a non-captured root; and admission to the dedupe queue needs containment or a shared corpus on top of the cosine. No new tables, no new config, no new job stages.

**Tech Stack:** Rust, `sqlx` over SQLite, `tokio` tests inline in each module (`#[cfg(test)] mod tests`), `cargo test --lib`.

**Spec:** `docs/superpowers/specs/2026-08-27-consolidation-boundary-design.md`

## Global Constraints

- Test names are sentences, and each carries the bug it pins in a comment. This is a house rule visible throughout `src/jobs/` and it is not optional.
- No new configuration keys. `review_min`, `auto_supersede`, `per_point`, `dedupe_interval_mins`, `max_dedupe_per_tick` keep their current meanings and defaults.
- No schema change. `artifact_pairs.decided_by` is explicitly **out of scope** (spec §6).
- Nothing in this plan touches the running instance. The repair described in spec §7 is a separate, separately approved action.
- `Provenance::Passage` is the discriminator throughout. Never `corpus_id IS NULL`.
- Existing behaviour that must survive every task: a repeated passage inside one corpus is still superseded by containment, and two `Synthesized` rows from one window still reach the model.

---

### Task 1: A passage is never one side of a pair

Spec §4.1. Today `classify_pair` refuses a passage pair only when both sides share a corpus **and** a `segment_idx` (`src/jobs/relate.rs:149-155`). On the live base that guard fired on none of 33 pairs, because passages that duplicate each other come from different documents.

The narrow guard stays exactly where it is — it protects the promoted-artifact-beside-its-passage case, which is a different question and already answered. The new guard goes at the end, immediately before `record_pair`, so the containment supersession in between keeps running for passages.

**Files:**
- Modify: `src/jobs/relate.rs` — add the guard just above `core.store.record_pair(...)` at line 266; add one test to the inline `mod tests`.

**Interfaces:**
- Consumes: `Provenance` (already imported at `src/jobs/relate.rs:22`), `Chunk::provenance`.
- Produces: nothing new. `classify_pair`'s signature is unchanged: `async fn classify_pair(core: &Core, a: &Chunk, b: &Chunk, score: f32) -> Result<bool>`.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `src/jobs/relate.rs`. It needs a passage in a corpus of its own, which no existing helper produces, so the helper comes with it:

```rust
    /// One passage under a corpus of its own. `seed_rows` writes `captured`
    /// rows, and the case under test is two passages from two documents.
    async fn seed_passage(core: &Core, corpus: &str, text: &str) -> String {
        let src = core.store.insert_corpus(corpus, "web", None).await.unwrap();
        core.store
            .insert_artifacts_with_provenance(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: text.to_string(),
                    corpus_span: None,
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: Some(0),
                    caveats: vec![],
                }],
                Provenance::Passage,
            )
            .await
            .unwrap()
            .remove(0)
            .id
    }

    #[tokio::test]
    async fn two_passages_from_different_documents_about_one_subject_are_not_a_pair() {
        // The guard this joins asked for one corpus AND one `segment_idx`, so
        // it never saw two passages from two documents — on the live base it
        // fired on none of thirty-three pairs. Thirteen scripts teaching one
        // subject produced passages at 0.89 to 0.93 that duplicate nothing, and
        // they were merged into one synthetic document.
        let core = test_core().await;
        let a_id = seed_passage(&core, "skript-a", "Spuren sind materielle Veraenderungen.").await;
        let b_id =
            seed_passage(&core, "skript-b", "Als Spur gilt jede materielle Veraenderung.").await;
        let a = core.store.get_artifact(&a_id).await.unwrap();
        let b = core.store.get_artifact(&b_id).await.unwrap();

        classify_pair(&core, &a, &b, 0.93).await.unwrap();

        assert!(
            core.store
                .pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .is_empty(),
            "two passages from two documents are two sources, not a duplicate"
        );
    }

    #[tokio::test]
    async fn a_pair_with_a_passage_on_either_side_is_never_filed() {
        // Eleven of the live base's thirty-three pairs were a passage against a
        // captured artifact, so the rule cannot be about two passages only.
        let core = test_core().await;
        let passage = seed_passage(&core, "skript-a", "Spuren sind materielle Veraenderungen.").await;
        let captured = crate::jobs::consolidate::tests::seed_into_new_corpus(
            &core,
            "Als Spur gilt jede materielle Veraenderung.",
            [0.99, 0.05],
        )
        .await;
        let a = core.store.get_artifact(&passage).await.unwrap();
        let b = core.store.get_artifact(&captured).await.unwrap();

        classify_pair(&core, &a, &b, 0.93).await.unwrap();

        assert!(
            core.store
                .pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_repeated_passage_inside_one_corpus_is_still_superseded_by_containment() {
        // What the new guard must not take away. Containment inside one corpus
        // is deterministic, costs no call, and is the only ground on which
        // anything is hidden unasked — so the guard sits below that block, not
        // above it. Placed above, this hygiene disappears silently.
        let core = test_core().await;
        let src = core.store.insert_corpus("skript", "web", None).await.unwrap();
        let rows = core
            .store
            .insert_artifacts_with_provenance(
                &src.id,
                &[
                    crate::store::artifacts::NewArtifact {
                        ordinal: 0,
                        text: "Spuren sind materielle Veraenderungen an Personen oder Sachen."
                            .into(),
                        corpus_span: None,
                        title: None,
                        category: None,
                        tags: vec![],
                        segment_idx: Some(0),
                        caveats: vec![],
                    },
                    crate::store::artifacts::NewArtifact {
                        ordinal: 1,
                        text: "Spuren sind materielle Veraenderungen".into(),
                        corpus_span: None,
                        title: None,
                        category: None,
                        tags: vec![],
                        segment_idx: Some(1),
                        caveats: vec![],
                    },
                ],
                Provenance::Passage,
            )
            .await
            .unwrap();

        classify_pair(&core, &rows[0], &rows[1], 0.97).await.unwrap();

        let short = core.store.get_artifact(&rows[1].id).await.unwrap();
        assert!(
            !short.in_results(),
            "a passage wholly inside another in one corpus is still hidden"
        );
    }
```

- [ ] **Step 2: Run the test and watch it fail**

Run: `cargo test --lib relate::tests::two_passages_from_different relate::tests::a_pair_with_a_passage relate::tests::a_repeated_passage_inside`
Expected: the first two FAIL — the assertion trips, because `record_pair` filed a pending row. `a_repeated_passage_inside_one_corpus_is_still_superseded_by_containment` PASSes already; it pins behaviour the change must not remove, which is worth having before the change rather than after.

- [ ] **Step 3: Add the guard**

In `src/jobs/relate.rs`, immediately before the `core.store.record_pair(&a.id, &b.id, score).await?;` call at line 266:

```rust
    // A passage is the verbatim substrate, not a claim anyone made twice.
    // Passages under one heading are alike for how they were cut, and passages
    // from different documents on one subject are alike because the subject is
    // taught more than once — neither is duplication. The narrow rule above
    // asks for one corpus and one `segment_idx` and so covers only the first;
    // this is the same statement `run` already makes about the asking side
    // (a passage never queries), applied to the side that gets filed.
    //
    // Below the containment block on purpose: a passage wholly inside another
    // in the same corpus is still superseded, deterministically and without a
    // call. What ends here is the model question, not the hygiene.
    if a.provenance == Provenance::Passage || b.provenance == Provenance::Passage {
        return Ok(false);
    }

```

- [ ] **Step 4: Run the new test and the module's existing tests**

Run: `cargo test --lib relate::`
Expected: PASS, including `a_written_row_beside_its_own_passage_is_not_a_pair` and every containment test in the module. If a containment test fails, the guard was placed above the containment block instead of below it.

- [ ] **Step 5: Commit**

```bash
git add src/jobs/relate.rs
git commit -m "fix: a passage is substrate, not somebody's duplicate"
```

---

### Task 2: A merged artifact does not arm a neighbour query

Spec §4.2. `mark_indexed` already refuses to arm a relate unit for a passage (`src/jobs/embed.rs:337-340`). A merged artifact is not a passage, so it arms one, queries its neighbours, and finds the next document's passage — twenty of the base's thirty-three pairs are passage against merged, which is that edge in the data.

`Merged` specifically, **not** `is_model_written()`: that predicate also covers `Synthesized`, and `src/jobs/relate.rs:143-148` deliberately lets two written rows from one window through.

**Files:**
- Modify: `src/jobs/embed.rs:340` — extend the arming condition; add one test to the inline `mod tests`.

**Interfaces:**
- Consumes: `crate::store::artifacts::Provenance`, already named in full at this call site.
- Produces: nothing new.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `src/jobs/embed.rs`:

```rust
    #[tokio::test]
    async fn a_merged_artifact_does_not_arm_a_neighbour_query() {
        // The feedback edge. A merge embeds, arms relate, finds the next
        // document's passage on the same subject, and the ticker merges that
        // too. Fifteen merges in sixty-eight minutes on the live base, roots
        // running 2, 3, 4 … 16, ending at one synthetic artifact of ten
        // thousand characters over sixteen passages from thirteen corpora.
        let core = test_core().await;
        let roots = crate::jobs::consolidate::tests::seed(
            &core,
            &[("first wording", [1.0, 0.0]), ("second wording", [0.99, 0.05])],
        )
        .await;
        let m = core
            .store
            .insert_merged_artifact(
                &crate::store::artifacts::NewMerged {
                    title: Some("merged".into()),
                    text: "both wordings".into(),
                    category: None,
                    tags: vec![],
                    caveats: vec![],
                },
                &roots,
            )
            .await
            .unwrap();

        mark_indexed(&core, &m).await.unwrap();

        assert_eq!(
            job_state(&core, Stage::Relate, &m.id).await,
            None,
            "a merge that queries its neighbours walks the corpus one passage at a time"
        );
    }
```

`seed` writes `captured` rows, so this test still passes after Task 3 tightens what a merge root may be.

Add a second test in the same block, for what Task 2 gives up:

```rust
    #[tokio::test]
    async fn an_artifact_near_a_merge_but_near_neither_root_is_not_paired() {
        // The cost of not arming, pinned so it stays a decision. An artifact
        // that was never near either root but *is* near the merged text will
        // not be paired. Closeness to a union is weak evidence of duplication
        // with any member, which is why this is the right trade — but it is a
        // behaviour change and not an accident.
        let core = test_core().await;
        let roots = crate::jobs::consolidate::tests::seed(
            &core,
            &[("first wording", [1.0, 0.0]), ("second wording", [0.99, 0.05])],
        )
        .await;
        crate::jobs::consolidate::tests::seed(&core, &[("a third subject", [0.9, 0.44])]).await;
        let m = core
            .store
            .insert_merged_artifact(
                &crate::store::artifacts::NewMerged {
                    title: Some("merged".into()),
                    text: "both wordings".into(),
                    category: None,
                    tags: vec![],
                    caveats: vec![],
                },
                &roots,
            )
            .await
            .unwrap();

        mark_indexed(&core, &m).await.unwrap();

        assert!(
            core.store
                .pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }
```

- [ ] **Step 2: Run the test and watch it fail**

Run: `cargo test --lib a_merged_artifact_does_not_arm_a_neighbour_query`
Expected: FAIL — `job_state` returns `Some("pending")` because the unit was armed.

- [ ] **Step 3: Extend the condition**

In `src/jobs/embed.rs`, replace the condition at line 340:

```rust
    if chunk.provenance != crate::store::artifacts::Provenance::Passage
        && chunk.provenance != crate::store::artifacts::Provenance::Merged
        && let Err(e) = crate::jobs::relate::arm(core, &chunk.id, 0).await
```

and extend the comment above it, after the existing passage sentence:

```rust
    // A merged artifact is not an anchor either. It carries the union of its
    // lineage's wording, so it scores above `review_min` against more of a
    // subject than either side did, and every pair it files becomes the next
    // merge — the artifact produces its own next question. Nothing is lost:
    // whichever ordinary artifact is embedded later still finds it, so a merge
    // is simply never the second member of a new pair.
    //
    // `Merged` and not `is_model_written()`: a synthesis is ordinary dedupe
    // material, and `relate.rs:143-148` lets two written rows from one window
    // through on purpose.
```

- [ ] **Step 4: Run the module's tests**

Run: `cargo test --lib embed::`
Expected: PASS. The merge-finish arming just below (`Provenance::Merged` → supersede its roots) is a different `if` and must still be reached; if a merge-finish test fails, the two conditions were folded into one.

- [ ] **Step 5: Commit**

```bash
git add src/jobs/embed.rs
git commit -m "fix: a merge that queries its neighbours writes its own next question"
```

---

### Task 3: A merge refuses a root that is not captured

Spec §4.3. `src/store/artifacts.rs:283` states that `artifact_sources.root_id` "only ever names a `captured` artifact — the invariant the whole anti-drift rule rests on". On the live base 135 of the merge path's 135 root rows name a passage. The claim is enforced where it is made, and **not** as a table constraint: the same table holds eleven rows for two `Synthesized` artifacts, where passage sources are correct.

**Files:**
- Modify: `src/store/artifacts.rs:285` — check the resolved roots before the transaction opens; add two tests to the inline `mod tests`.

**Interfaces:**
- Consumes: `roots_of(sources) -> HashMap<String, Vec<String>>`, already called at the top of `insert_merged_artifact`.
- Produces: `insert_merged_artifact` gains a failure mode. Callers already handle `Result`; `src/jobs/merge.rs:34-50` propagates it, which leaves the pair pending and the unit retryable.

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block in `src/store/artifacts.rs`:

```rust
    #[tokio::test]
    async fn a_merge_whose_root_is_a_passage_is_refused() {
        // The invariant this file claims at `insert_merged_artifact` and the
        // live base violated in every one of its 135 merge-lineage rows. A
        // merge over passages rewrites the verbatim substrate and hides it
        // behind text that stands in no document.
        let store = test_store().await;
        let src = store.insert_corpus("skript", "web", None).await.unwrap();
        let passages = store
            .insert_artifacts_with_provenance(
                &src.id,
                &[NewArtifact {
                    ordinal: 0,
                    text: "Spuren sind materielle Veraenderungen.".into(),
                    corpus_span: None,
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: Some(0),
                    caveats: vec![],
                }],
                Provenance::Passage,
            )
            .await
            .unwrap();
        let ids: Vec<String> = passages.iter().map(|c| c.id.clone()).collect();

        let err = store
            .insert_merged_artifact(
                &NewMerged {
                    title: Some("merged".into()),
                    text: "rewritten".into(),
                    category: None,
                    tags: vec![],
                    caveats: vec![],
                },
                &ids,
            )
            .await;

        assert!(err.is_err(), "a passage is not a merge root");
    }

    #[tokio::test]
    async fn a_synthesized_artifact_may_still_name_passage_sources() {
        // Why the check is on the merge path and not on `artifact_sources`. A
        // synthesis draws on passages by design; a table constraint would break
        // it, and eleven rows in the live base are exactly this case.
        let store = test_store().await;
        let src = store.insert_corpus("skript", "web", None).await.unwrap();
        let passages = store
            .insert_artifacts_with_provenance(
                &src.id,
                &[NewArtifact {
                    ordinal: 0,
                    text: "Spuren sind materielle Veraenderungen.".into(),
                    corpus_span: None,
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: Some(0),
                    caveats: vec![],
                }],
                Provenance::Passage,
            )
            .await
            .unwrap();
        let ids: Vec<String> = passages.iter().map(|c| c.id.clone()).collect();

        let made = store
            .insert_synthesized_artifact(
                &NewSynthesized {
                    text: "Zusammenfassung".into(),
                    title: Some("Spurenkunde".into()),
                    category: None,
                    tags: vec![],
                    caveats: vec![],
                    cues: vec![],
                },
                &ids,
            )
            .await;

        assert!(made.is_ok(), "synthesis over passages is what synthesis is");
    }
```

- [ ] **Step 2: Run the tests and watch the first fail**

Run: `cargo test --lib artifacts::tests::a_merge_whose_root_is_a_passage_is_refused artifacts::tests::a_synthesized_artifact_may_still_name_passage_sources`
Expected: the first FAILs (the insert succeeds today), the second PASSes already. A failing second test means the argument shape is wrong, not the design.

- [ ] **Step 3: Add the check**

In `src/store/artifacts.rs`, after `let root_ids: BTreeSet<&String> = resolved.values().flatten().collect();` and before `let mut tx = self.pool.begin().await?;`:

```rust
        // The invariant this function's own documentation rests on, checked
        // rather than assumed. A merge over passages rewrites the verbatim
        // substrate into text that belongs to no corpus and carries no span,
        // and hides the wording someone captured behind it. On the base this
        // was written for, every merge-lineage row named a passage.
        //
        // Here and not as a constraint on `artifact_sources`: the same table
        // carries a synthesis's passage sources, where naming a passage is
        // correct.
        for root in &root_ids {
            let p: String = sqlx::query_scalar("SELECT provenance FROM artifacts WHERE id = ?")
                .bind(root.as_str())
                .fetch_one(&self.pool)
                .await?;
            if Provenance::parse(&p) != Provenance::Captured {
                return Err(crate::error::Error::Validation(format!(
                    "a merge root must be a captured artifact; {root} is {p}"
                )));
            }
        }
```

`Error::Validation` and not `Error::Internal` (`src/error.rs:14`): the caller sent a root it may not merge, which is a refused request, and `src/error.rs:35` states why the two must not be confused.

- [ ] **Step 4: Run the store's tests**

Run: `cargo test --lib artifacts::`
Expected: PASS. Merge tests elsewhere that seed passages as roots will now fail — that is the point; fix each by seeding captured rows, and note in its comment that the seed changed because a passage is no longer a legal root.

- [ ] **Step 5: Run the whole suite, because this one reaches**

Run: `cargo test`
Expected: PASS. `src/jobs/merge.rs` and `src/jobs/dedupe.rs` tests are the likely fallers.

- [ ] **Step 6: Commit**

```bash
git add src/store/artifacts.rs src/jobs/merge.rs src/jobs/dedupe.rs
git commit -m "fix: the invariant a merge rests on, checked where it is claimed"
```

---

### Task 4: Similarity alone no longer buys a call

Spec §4.4 and §4.5. A cosine cannot separate "stored twice" from "taught thirteen times", and twenty-three of the base's thirty-three pairs are cross-corpus. A pair now reaches `record_pair` only with a corroborant: one text wholly inside the other, or both from one document.

`contains_normalized` is already in this file and is a string operation, so computing it across corpora costs nothing. The same-corpus containment *supersession* block above is untouched; this only reuses its predicate.

A refused pair writes nothing at all — in particular it does not bump `artifact_links`. A link means two artifacts were used together, and a bump derived from a cosine would put an observation about text into a table whose every other row is an observation about behaviour.

**Files:**
- Modify: `src/jobs/relate.rs` — the block added in Task 1 grows the corroborant check; add two tests.
- Modify: `config.example.toml` — one sentence in `[consolidate]`.

**Interfaces:**
- Consumes: `contains_normalized(&str, &str) -> bool` from this module; `Chunk::corpus_id`.
- Produces: nothing new.

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block in `src/jobs/relate.rs`:

```rust
    #[tokio::test]
    async fn cross_corpus_similarity_alone_does_not_reach_the_model() {
        // Twenty-three of the live base's thirty-three pairs were this: two
        // documents covering one subject at 0.89 to 0.93, duplicating nothing.
        // Thirteen scripts agreeing is evidence that the claim is standard, and
        // a merge erases that they agreed.
        let core = test_core().await;
        let a_id = crate::jobs::consolidate::tests::seed_into_new_corpus(
            &core,
            "Spuren sind materielle Veraenderungen an Personen oder Sachen.",
            [1.0, 0.0],
        )
        .await;
        let b_id = crate::jobs::consolidate::tests::seed_into_new_corpus(
            &core,
            "Als Spur gilt jede materielle Veraenderung an Person oder Objekt.",
            [0.99, 0.05],
        )
        .await;
        let a = core.store.get_artifact(&a_id).await.unwrap();
        let b = core.store.get_artifact(&b_id).await.unwrap();

        classify_pair(&core, &a, &b, 0.93).await.unwrap();

        assert!(
            core.store
                .pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn containment_across_corpora_still_reaches_the_model() {
        // The case the corroborant keeps working: one document ingested twice.
        // Containment is the predicate that says one side adds nothing, which
        // is what duplication means — a score only says they are alike.
        let core = test_core().await;
        let long = "Spuren sind materielle Veraenderungen an Personen oder Sachen, \
                    die zur Tataufklaerung beitragen koennen.";
        let a_id =
            crate::jobs::consolidate::tests::seed_into_new_corpus(&core, long, [1.0, 0.0]).await;
        let b_id = crate::jobs::consolidate::tests::seed_into_new_corpus(
            &core,
            "Spuren sind materielle Veraenderungen an Personen oder Sachen,",
            [0.99, 0.05],
        )
        .await;
        let a = core.store.get_artifact(&a_id).await.unwrap();
        let b = core.store.get_artifact(&b_id).await.unwrap();

        classify_pair(&core, &a, &b, 0.93).await.unwrap();

        assert_eq!(
            core.store
                .pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn a_refused_pair_does_not_bump_a_link() {
        // The link graph stays an observation about use. `bump_link` takes the
        // cue that bound two artifacts, and the README calls the graph one
        // learned from co-retrieval; a bump derived from a cosine would make
        // the recommendation surface unable to tell the two apart.
        let core = test_core().await;
        let a_id = crate::jobs::consolidate::tests::seed_into_new_corpus(
            &core,
            "Spuren sind materielle Veraenderungen.",
            [1.0, 0.0],
        )
        .await;
        let b_id = crate::jobs::consolidate::tests::seed_into_new_corpus(
            &core,
            "Als Spur gilt jede materielle Veraenderung.",
            [0.99, 0.05],
        )
        .await;
        let a = core.store.get_artifact(&a_id).await.unwrap();
        let b = core.store.get_artifact(&b_id).await.unwrap();

        classify_pair(&core, &a, &b, 0.93).await.unwrap();

        assert!(
            core.store.get_link(&a_id, &b_id).await.unwrap().is_none(),
            "a cosine is not evidence that anyone used these together"
        );
    }
```

- [ ] **Step 2: Run them and watch the first and third fail**

Run: `cargo test --lib relate::tests::cross_corpus relate::tests::containment_across relate::tests::a_refused_pair`
Expected: `cross_corpus_similarity_alone_does_not_reach_the_model` FAILs (a pending row was filed). `containment_across_corpora_still_reaches_the_model` PASSes already. `a_refused_pair_does_not_bump_a_link` PASSes already — it pins behaviour that must not be added later, which is worth having before the change rather than after.

- [ ] **Step 3: Add the corroborant to the guard from Task 1**

In `src/jobs/relate.rs`, replace the block added in Task 1 with:

```rust
    // A passage is the verbatim substrate, not a claim anyone made twice.
    // (See Task 1's comment — unchanged in substance.)
    if a.provenance == Provenance::Passage || b.provenance == Provenance::Passage {
        return Ok(false);
    }

    // A score says two texts are alike. Duplication says one of them adds
    // nothing, and those are different claims — the comment above
    // `contains_normalized` in this file states the distinction plainly. On a
    // base of two hundred documents teaching one subject, "alike" is the
    // ordinary condition and says nothing about duplication.
    //
    // So the cosine admits nothing on its own. One of two things has to hold:
    // one text is wholly inside the other, or both came out of one document.
    // Containment is what keeps the real cross-corpus case working — the same
    // document ingested twice — while two scripts that merely cover one subject
    // are refused. Refused, and nothing is written: not a pair, and not a link
    // either, because a link means two artifacts were *used* together.
    let (long, short) = if a.text.len() >= b.text.len() {
        (a, b)
    } else {
        (b, a)
    };
    let corroborated = contains_normalized(&long.text, &short.text)
        || (a.corpus_id.is_some() && a.corpus_id == b.corpus_id);
    if !corroborated {
        tracing::debug!(a = %a.id, b = %b.id, score, "alike, but nothing says duplicate");
        return Ok(false);
    }

```


- [ ] **Step 4: Run the module and then the suite**

Run: `cargo test --lib relate::` then `cargo test`
Expected: PASS. Tests elsewhere that seeded two similar artifacts in one corpus still pass — `seed` puts everything in the corpus named `raw`, so the same-corpus arm covers them. A failure in `consolidate::` is likely a test that relied on cross-corpus similarity filing a pair; fix it by seeding into one corpus and say so in its comment.

- [ ] **Step 5: Say it in the example config**

In `config.example.toml`, in the `[consolidate]` block near `review_min`, add:

```toml
# `review_min` is a floor, not an admission. A pair reaches the model only when
# one text is wholly inside the other or both came out of one document: a cosine
# cannot tell "stored twice" from "taught by thirteen authors", and on a corpus
# of scripts covering one subject the second is the ordinary case.
```

- [ ] **Step 6: Commit**

```bash
git add src/jobs/relate.rs config.example.toml
git commit -m "fix: alike is not duplicate, and a cosine cannot tell them apart"
```

---

## After the four tasks

`cargo test` green, `cargo clippy --all-targets` clean. Nothing has touched the running instance: the fifteen merges in the live base are still there, and undoing them is spec §7 — a separate action needing its own approval and a copy of `data/users/<id>.db` taken first.

Spec §8 stays open: pair 25 was filed with a `score` of 0.0 where every other pair lies between 0.8887 and 0.9623, and no admission rule in the code can produce that. Task 1 makes that particular pair impossible, so it does not block anything, but the instance journal around 2026-08-27 07:13:47 is where the answer is.
