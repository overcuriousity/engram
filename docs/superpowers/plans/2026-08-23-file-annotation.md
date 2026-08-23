# The box is the file's annotation — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A staged file turns the workspace's one box into that file's annotation, and that annotation becomes a searchable artifact instead of unfindable metadata.

**Architecture:** Three doors (`ingest_capture`, `ingest_pdf`, `ingest_image`) gain one shared helper that writes the note as an artifact on the file's own corpus — `corpus_span: None`, `segment_idx: None`, so it claims no line of the document and `renumber_artifacts` sorts it to ordinal 0 unaided. The 2000-character cap stops truncating storage and moves into the vision prompt, the only place unboundedness costs anything. On the front end the dedicated note input is deleted and `assets/app.js` reads the box at press time, so attach-then-type and type-then-attach are the same act.

**Tech Stack:** Rust, `sqlx`/SQLite, `tokio`, Askama templates, vanilla ES5-style JS + htmx.

**Spec:** `docs/superpowers/specs/2026-08-23-file-annotation-design.md`

## Global Constraints

- **No new endpoint, no new table, no new model call.** The note is embedded by the corpus-level Embed job the capture already arms.
- **The three upload doors' wire format is unchanged.** `note` stays an optional multipart field (`src/web/api.rs:364,470`).
- **A duplicate capture writes nothing.** The note artifact is written only on `Insertion::Created` — never on `Insertion::Existing`.
- **Comments in this codebase say *why*, in prose, at the density of the surrounding file.** Match it. Do not add narration to code that does not need it.
- **`assets/app.js` is ES5-flavoured**: `var`, `function`, no arrow functions, no `const`/`let`, no template literals.
- **Tests are `#[tokio::test]` inside `mod tests` in the same file as the code.**
- Build with `cargo build`; if memory is tight use `./build-lowmem.sh`.

---

### Task 1: A note becomes an artifact on the file's corpus

**Files:**
- Modify: `src/core/ingest.rs` — add helper, call from three doors, tests

**Already imported** in `src/core/ingest.rs`'s `mod tests` (`:1140-1150`) — do not re-import: `Capture`, `ImageCapture`, `MAX_NOTE_CHARS`, `PdfCapture`, `test_core`, `Stage`, `CorpusStatus`, and the `a_pdf_fixture()` / `a_seeded_png()` helpers.

**Interfaces:**
- Consumes: `Store::insert_artifacts(&str, &[NewArtifact]) -> Result<Vec<Chunk>>` (defaults to `Provenance::Captured`, `src/store/artifacts.rs:453`); `Store::rearm_idle_seq(Stage, &str, &str, i64)` (`src/store/jobs.rs:340`)
- Produces: `Core::attach_note_artifact(&self, corpus_id: &str, note: Option<&str>) -> Result<()>` — private to `ingest.rs`, called by all three doors

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block in `src/core/ingest.rs`:

```rust
    /// The sentence most worth searching on must be an artifact, not a
    /// caption. Embedding runs over artifact chunks and never over metadata,
    /// so a note that stays in `metadata["note"]` cannot be found at all.
    #[tokio::test]
    async fn a_note_on_a_pdf_becomes_a_span_less_artifact_on_its_corpus() {
        let core = test_core().await;
        let out = core
            .ingest_pdf(PdfCapture {
                bytes: a_pdf_fixture(),
                filename: Some("lease.pdf".into()),
                title_hint: None,
                note: Some("  scan of the Reinhardt lease, break clause is p.3  ".into()),
            })
            .await
            .unwrap();

        let all = core.store.artifacts_for_corpus(&out.id).await.unwrap();
        assert_eq!(all.len(), 1, "the note, and nothing extracted yet");
        let n = &all[0];
        assert_eq!(n.text, "scan of the Reinhardt lease, break clause is p.3");
        assert_eq!(
            n.corpus_span, None,
            "the note is about the file, not a line of it"
        );
        assert_eq!(n.segment_idx, None, "it belongs to no window");
        assert_eq!(n.ordinal, 0);
        assert_eq!(n.provenance, crate::store::artifacts::Provenance::Captured);
        assert_eq!(n.title, None);
    }

    /// One helper, three doors, so a fourth cannot forget it.
    #[tokio::test]
    async fn every_door_that_takes_a_note_writes_it_as_an_artifact() {
        let describer = std::sync::Arc::new(crate::infer::fake::FakeDescriber::default());
        let core = crate::core::test_support::test_core_with_describer(describer).await;

        let img = core
            .ingest_image(ImageCapture {
                bytes: a_seeded_png(11),
                filename: Some("IMG_9.png".into()),
                title_hint: None,
                note: Some("front of the router".into()),
            })
            .await
            .unwrap();
        let a = core.store.artifacts_for_corpus(&img.id).await.unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].text, "front of the router");

        let txt = core
            .ingest_capture(Capture::new("the file's own text", "upload").with_note(
                Some("from the printer".into()),
            ))
            .await
            .unwrap();
        let a = core.store.artifacts_for_corpus(&txt.id).await.unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].text, "from the printer");
    }

    #[tokio::test]
    async fn a_capture_with_no_usable_note_writes_no_artifact() {
        let core = test_core().await;
        let none = core
            .ingest_capture(Capture::new("text one", "upload"))
            .await
            .unwrap();
        assert!(
            core.store.artifacts_for_corpus(&none.id).await.unwrap().is_empty()
        );

        let blank = core
            .ingest_capture(Capture::new("text two", "upload").with_note(Some("   ".into())))
            .await
            .unwrap();
        assert!(
            core.store.artifacts_for_corpus(&blank.id).await.unwrap().is_empty(),
            "whitespace is not an annotation"
        );
    }

    /// A scan with no text layer parks as `failed` and never reaches `settle`,
    /// which is what normally arms the embed. Without arming it here, the one
    /// thing a person typed about an unreadable document waits forever.
    #[tokio::test]
    async fn a_note_arms_the_embed_so_a_parked_capture_still_becomes_findable() {
        let core = test_core().await;
        let out = core
            .ingest_pdf(PdfCapture {
                bytes: a_pdf_fixture(),
                filename: Some("scan.pdf".into()),
                title_hint: None,
                note: Some("the survey nobody can OCR".into()),
            })
            .await
            .unwrap();

        let pending = core
            .store
            .pending_artifacts_for_corpus(&out.id)
            .await
            .unwrap();
        assert_eq!(pending.len(), 1, "the note is waiting for a vector");
        assert!(
            core.store
                .live_job(Stage::Embed, &out.id)
                .await
                .unwrap(),
            "a corpus embed job must be armed by the capture itself"
        );
    }

    /// Re-uploading the same file must not stack a second note on it.
    #[tokio::test]
    async fn a_duplicate_upload_writes_no_second_note() {
        let core = test_core().await;
        let bytes = a_pdf_fixture();
        let first = core
            .ingest_pdf(PdfCapture {
                bytes: bytes.clone(),
                filename: Some("plan.pdf".into()),
                title_hint: None,
                note: Some("the quarterly plan".into()),
            })
            .await
            .unwrap();
        let again = core
            .ingest_pdf(PdfCapture {
                bytes,
                filename: Some("plan.pdf".into()),
                title_hint: None,
                note: Some("a second thought about it".into()),
            })
            .await
            .unwrap();
        assert_eq!(first.id, again.id);
        assert!(again.duplicate);
        assert_eq!(
            core.store.artifacts_for_corpus(&first.id).await.unwrap().len(),
            1
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib core::ingest::tests::a_note -- --nocapture`
Expected: FAIL — `artifacts_for_corpus` returns an empty vector, so the length assertions trip.

- [ ] **Step 3: Write the helper**

Add to the `impl Core` block in `src/core/ingest.rs`, directly after `park_or_queue` (which ends at `ingest.rs:297`):

```rust
    /// The operator's sentence about a file, written where it can be found.
    ///
    /// Embedding runs over artifact chunks and never over metadata, so a note
    /// left in `metadata["note"]` is invisible to search — on a PDF or a text
    /// upload absolutely, and on a photograph only as whatever the vision
    /// model happened to echo. This is the same words as their own artifact.
    ///
    /// `corpus_span: None` is the point: the note is *about* the file and is
    /// no line *of* it, so it claims no span and nothing tries to read it
    /// beside lines it did not come from. `segment_idx: None` puts it ahead of
    /// every window in `renumber_artifacts`, which orders by
    /// `COALESCE(segment_idx, 0), ordinal, rowid` — so it settles at ordinal 0
    /// with no help from either artifact writer.
    ///
    /// The embed is armed here rather than left to `settle`. A scan with no
    /// text layer parks as `failed` and never reaches settling, and that is
    /// exactly the capture whose note is the only text anyone will ever have.
    async fn attach_note_artifact(&self, corpus_id: &str, note: Option<&str>) -> Result<()> {
        let Some(text) = note.map(str::trim).filter(|n| !n.is_empty()) else {
            return Ok(());
        };
        self.store
            .insert_artifacts(
                corpus_id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: text.to_string(),
                    corpus_span: None,
                    // A heading is something a document gave. This had none.
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: None,
                    caveats: vec![],
                }],
            )
            .await?;
        self.store
            .rearm_idle_seq(Stage::Embed, "corpus", corpus_id, 0)
            .await?;
        Ok(())
    }
```

- [ ] **Step 4: Call it from the text door**

In `ingest_capture`, replace the `Ok(IngestOutcome { ... })` at `src/core/ingest.rs:262-267` with:

```rust
        self.attach_note_artifact(&src.id, c.metadata["note"].as_str())
            .await?;

        Ok(IngestOutcome {
            id: src.id,
            status: src.status,
            duplicate: false,
            near_duplicate: near,
        })
```

`c` is borrowed, never destructured, in `ingest_capture` (`src/core/ingest.rs:193-196`), so it is still in scope here. Do not call `clean_note` again — `with_note` (`src/core/ingest.rs:125`) already trimmed the value now sitting in `metadata`.

- [ ] **Step 5: Call it from the PDF door**

In `ingest_pdf`, `note` is moved into `clean_note` at `src/core/ingest.rs:334`. Keep a copy before that, and write the artifact only on the created branch. Replace `src/core/ingest.rs:333-366` with:

```rust
        let note = clean_note(note);
        if let Some(n) = &note {
            metadata["note"] = serde_json::json!(n);
        }

        let inserted = self
            .store
            .insert_attached_corpus(
                &hash,
                ORIGIN_PDF,
                title_hint.as_deref(),
                &metadata,
                crate::store::corpora::Reading::EXTRACTION,
                &crate::store::attachments::NewFile {
                    kind: "pdf",
                    mime: "application/pdf",
                    filename: filename.as_deref(),
                    bytes: &bytes,
                    preview: &[],
                    width: None,
                    height: None,
                },
            )
            .await?;
        Ok(match inserted {
            // A file already in the base keeps the note it was captured with.
            // Stacking a second one per re-upload is how a corpus grows a pile
            // of near-identical captions nobody wrote twice on purpose.
            Insertion::Existing(c) => IngestOutcome::existing(&c),
            Insertion::Created(c) => {
                self.attach_note_artifact(&c.id, note.as_deref()).await?;
                IngestOutcome {
                    id: c.id,
                    status: c.status,
                    duplicate: false,
                    near_duplicate: None,
                }
            }
        })
```

- [ ] **Step 6: Call it from the image door**

In `ingest_image`, the same shape. `note` is consumed by `clean_note` at `src/core/ingest.rs:417`; hold the cleaned value, and after the `Insertion::Created(src) => src` match resolves (`src/core/ingest.rs:433-449`) add the call before the `Ok(...)`:

```rust
        let note = clean_note(note);
        if let Some(n) = &note {
            metadata["note"] = serde_json::Value::String(n.clone());
        }
```

and after the `tracing::info!` at `src/core/ingest.rs:450-455`:

```rust
        self.attach_note_artifact(&src.id, note.as_deref()).await?;
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test --lib core::ingest`
Expected: PASS, including the pre-existing note tests at `ingest.rs:1157`, `2222` and `2296`.

- [ ] **Step 8: Prove the ordinal claim against the real passage writer**

The whole design rests on `renumber_artifacts` ordering by `COALESCE(segment_idx, 0), ordinal, rowid` (`src/store/artifacts.rs:918`). Add to `mod tests` in `src/jobs/passages.rs`:

```rust
    /// The note is written at capture and the passages arrive later, each
    /// numbered from 0 within its own window. Renumbering has to put the note
    /// first and push the document down by one — with no change to this
    /// writer, which is the whole reason the note carries no `segment_idx`.
    #[tokio::test]
    async fn a_note_sorts_ahead_of_the_document_it_annotates() {
        // `passages.rs`'s `mod tests` (`:221-224`) imports neither of these.
        let core = crate::core::test_support::test_core().await;
        let out = core
            .ingest_capture(
                crate::core::ingest::Capture::new(
                    "# Heading\n\nThe body of the uploaded document.",
                    "upload",
                )
                .with_note(Some("printout from the hallway scanner".into())),
            )
            .await
            .unwrap();

        capture_verbatim(&core, &out.id).await.unwrap();

        let all = core.store.artifacts_for_corpus(&out.id).await.unwrap();
        assert!(all.len() >= 2, "the note and at least one passage");
        assert_eq!(all[0].text, "printout from the hallway scanner");
        assert_eq!(all[0].ordinal, 0);
        assert_eq!(all[0].corpus_span, None);
        assert_eq!(all[1].ordinal, 1, "ordinals stay continuous");
        assert!(
            all[1].corpus_span.is_some(),
            "a passage still anchors to its lines"
        );
    }
```

- [ ] **Step 9: Run it**

Run: `cargo test --lib jobs::passages::tests::a_note_sorts_ahead`
Expected: PASS with no production change. If it fails, the design's central assumption is wrong — stop and report rather than editing the renumberer.

Note: no separate retrieval test is needed. `seed_from` (`src/core/search.rs:1148-1166`) already builds every search-suite fixture with `corpus_span: None` and `segment_idx: None`, so span-less artifacts being fully retrievable is proven by the existing suite.

- [ ] **Step 10: Commit**

```bash
git add src/core/ingest.rs src/jobs/passages.rs
git commit -m "feat(ingest): the note a person typed becomes something they can find

Embedding runs over artifact chunks, so a note in metadata was
invisible to search: on a PDF or a text upload entirely, on a photo
only as whatever the vision model echoed back. It is now an artifact
on the file's own corpus, span-less because it is about the file and
is no line of it.

renumber_artifacts orders by COALESCE(segment_idx, 0), ordinal, rowid,
so a segment-less note written at capture settles at ordinal 0 with no
change to either artifact writer. The embed is armed at ingest rather
than left to settle, so a scan that parks as failed still makes the
one sentence anybody typed about it findable."
```

---

### Task 2: The cap stops truncating storage and bounds the prompt instead

**Files:**
- Modify: `src/core/ingest.rs:24-36` — `MAX_NOTE_CHARS` doc, `clean_note`
- Modify: `src/core/ingest.rs:2296` — the existing cap test
- Modify: `src/infer/prompt.rs:1310-1317` — `describe_context`

**Interfaces:**
- Consumes: `crate::core::ingest::MAX_NOTE_CHARS` (already `pub`)
- Produces: nothing new. `clean_note` keeps its signature `fn clean_note(note: Option<String>) -> Option<String>`; `describe_context(&serde_json::Value) -> String` keeps its.

- [ ] **Step 1: Write the failing tests**

In `src/infer/prompt.rs`, inside `mod tests`:

```rust
    /// The note leads the vision prompt, so an unbounded one would swamp the
    /// description or overrun the call. This is now the only place the cap
    /// still earns its keep — nothing stored is truncated any more.
    #[test]
    fn describe_context_bounds_the_note_it_spends_on_a_model_call() {
        let long = "x".repeat(crate::core::ingest::MAX_NOTE_CHARS + 500);
        let m = serde_json::json!({ "note": long });
        let ctx = describe_context(&m);
        let kept = ctx
            .lines()
            .next()
            .unwrap()
            .trim_start_matches("Context from the person who captured this: ");
        assert_eq!(kept.chars().count(), crate::core::ingest::MAX_NOTE_CHARS);
    }
```

In `src/core/ingest.rs`, replace the existing `a_note_is_capped_and_a_blank_one_is_dropped` (`ingest.rs:2296`) with:

```rust
    /// A note is no longer truncated on the way into storage. It is an
    /// artifact like any other, and `embed::run_with_limit` splits an oversize
    /// chunk into siblings — so length is the embedder's problem, not a
    /// silent amputation at the door.
    #[tokio::test]
    async fn a_long_note_is_stored_whole_and_a_blank_one_is_dropped() {
        let core = test_core().await;
        let long = "x".repeat(MAX_NOTE_CHARS + 50);
        let out = core
            .ingest_capture(Capture::new("some text", "upload").with_note(Some(long.clone())))
            .await
            .unwrap();
        let src = core.store.get_corpus(&out.id).await.unwrap();
        assert_eq!(src.metadata["note"].as_str().unwrap(), long);
        let all = core.store.artifacts_for_corpus(&out.id).await.unwrap();
        assert_eq!(all[0].text, long, "the artifact keeps every character");

        let out = core
            .ingest_capture(Capture::new("other text", "upload").with_note(Some("   ".into())))
            .await
            .unwrap();
        assert!(
            core.store
                .get_corpus(&out.id)
                .await
                .unwrap()
                .metadata
                .get("note")
                .is_none()
        );
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --lib infer::prompt::tests::describe_context_bounds core::ingest::tests::a_long_note`
Expected: FAIL — the prompt test sees `MAX_NOTE_CHARS + 500` characters (nothing truncates yet in the prompt); the ingest test sees `MAX_NOTE_CHARS` where it wants the full string.

- [ ] **Step 3: Stop truncating storage**

Replace `src/core/ingest.rs:24-36` with:

```rust
/// Longest note spent on a vision call. Context, not a document: the note is
/// the lead line of the describe prompt, and an unbounded one swamps the
/// description or overruns the request.
///
/// It bounds that copy and nothing else. A note is stored whole — it is an
/// artifact like any other text, and `jobs::embed` cuts an oversize chunk into
/// siblings rather than losing the tail of it. Truncating on the way in was a
/// silent amputation with no receipt anywhere.
pub const MAX_NOTE_CHARS: usize = 2000;

/// The user's context for a capture, cleaned: trimmed, `None` when there is
/// nothing in it.
fn clean_note(note: Option<String>) -> Option<String> {
    let n = note?.trim().to_string();
    if n.is_empty() {
        return None;
    }
    Some(n)
}
```

- [ ] **Step 4: Bound the prompt**

Replace `src/infer/prompt.rs:1312-1317` with:

```rust
    if let Some(note) = metadata["note"].as_str().filter(|n| !n.trim().is_empty()) {
        // The stored note is whole; this copy is the one that costs tokens.
        let n = note.trim();
        lines.push(format!(
            "Context from the person who captured this: {}",
            n.chars()
                .take(crate::core::ingest::MAX_NOTE_CHARS)
                .collect::<String>()
        ));
    }
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib core::ingest infer::prompt`
Expected: PASS. `describe_context_leads_with_the_note_then_the_facts_and_omits_what_is_absent` (`prompt.rs:2261`) must still pass — the note still leads.

- [ ] **Step 6: Run the whole suite**

Run: `cargo test`
Expected: PASS. Anything asserting a truncated stored note is now wrong and should be updated to assert the whole note; anything asserting the vision prompt is bounded is now right.

- [ ] **Step 7: Commit**

```bash
git add src/core/ingest.rs src/infer/prompt.rs
git commit -m "fix(ingest): bound the note where it costs, not where it is kept

MAX_NOTE_CHARS truncated on the way into metadata, silently and with no
receipt. Now that a note is an artifact, length is the embedder's
problem — jobs::embed cuts an oversize chunk into siblings — so nothing
stored is cut.

The one place unboundedness still costs something is the vision prompt,
where the note is the lead line. The cap moves there and bounds only
the copy spent on the call."
```

---

### Task 3: The box becomes the note, and one press is one capture

**Files:**
- Modify: `src/web/templates/workspace.html:74-86` — delete the note input from the staged box
- Modify: `assets/app.js` — `boxVerbs` sync (`:826-839`), `captureVerb` stage/unstage/send/click (`:893-1037`, `:1153-1163`)

**Interfaces:**
- Consumes: `VERB_SYNC` (the synthetic event `boxVerbs` listens for), `refreshRail()`, `receipt()`, `clearReceipts()` — all already in `captureVerb`'s scope
- Produces: `send(file, note)` — the second parameter is new; `stage`/`unstage` keep their signatures

- [ ] **Step 1: Delete the note input**

In `src/web/templates/workspace.html`, remove the comment and the input at lines 77-83 (the `{# Context for the file… #}` block and `<input class="input" type="text" name="note" …>`), leaving the staged box as thumbnail, name and Remove. Replace the comment above the staged box (`workspace.html:74-76`, ending `…is the photo. #}`) with a sentence saying where the note went:

```
     The note is not in here any more. A file makes the box below this one the
     file's annotation — one place to type, whichever order the two arrive in.
     Two boxes on screen and no rule saying which one the words in front of you
     belong to is the thing this replaced. #}
```

- [ ] **Step 2: Disarm Ask while a file is staged**

In `boxVerbs`'s `sync` (`assets/app.js:832-839`), replace the loop with:

```js
      var hasText = !!box.value.trim();
      var stagedEl = document.getElementById('staged');
      var hasFile = !!(stagedEl && !stagedEl.hidden);
      for (var i = 0; i < buttons.length; i++) {
        buttons[i].disabled = buttons[i].getAttribute('data-verb') === 'capture'
          ? !(hasText || hasFile)
          // A staged file has made the box that file's note. Asking a note is
          // not a thing to do, and the answer would land beside a file the
          // question was never about.
          : (!hasText || hasFile);
      }
```

- [ ] **Step 3: Make the box go quiet**

In `captureVerb`, replace the `noteBox` lookup (`assets/app.js:893`) with the placeholder the box wears while it is a note, and add the guard. Put this beside the `staged` declarations at `assets/app.js:903-908`:

```js
    // What the box says when it is annotating a file rather than searching.
    var SEARCH_HINT = box.getAttribute('placeholder');
    var NOTE_HINT = 'What is it, why keep it?';

    // Typing an annotation into a live search box is an embedding call, an
    // activation bump and a Judge-queue row per phrase, for text nobody asked
    // as a question. The form's own requests are cancelled while a file waits;
    // Capture does not go through the form, and Ask is disabled above.
    form.addEventListener('htmx:beforeRequest', function (e) {
      if (staged) e.preventDefault();
    });
```

- [ ] **Step 4: Swap the placeholder on stage and restore it on unstage**

In `stage` (`assets/app.js:948-980`), replace the focus block at `:976-978` with:

```js
      box.setAttribute('placeholder', NOTE_HINT);
      // Only where a pointer says there is a hardware keyboard — the rule the
      // box already follows. On a phone this would throw the software keyboard
      // over the thumbnail the operator is checking, which is the picture they
      // just took.
      if (!restore && window.matchMedia('(hover: hover)').matches) box.focus();
```

In `unstage` (`assets/app.js:909-926`), before the `VERB_SYNC` dispatch at `:925`:

```js
      // The box is a search box again, with whatever was typed still in it:
      // Remove is the way out and must never be the thing that eats it.
      box.setAttribute('placeholder', SEARCH_HINT);
```

- [ ] **Step 5: Send the box as the note**

In `send`, change the signature and the two `noteBox` reads. Replace `assets/app.js:983` and `:1000`:

```js
    function send(file, note) {
```

```js
      if (note) payload.append('note', note);
```

and in the success branch (`assets/app.js:1029-1030`), replace `if (noteBox) noteBox.value = '';` with:

```js
            box.value = '';
            box.dispatchEvent(new Event(VERB_SYNC, { bubbles: true }));
            refreshRail();
```

removing the now-duplicated `refreshRail()` on the line below it. The failure branch is unchanged: `stage(file, true)` puts the file back, and the note is still in the box because nothing cleared it.

- [ ] **Step 6: One press, one capture**

Replace the click handler at `assets/app.js:1153-1163` with:

```js
    verb.addEventListener('click', function (e) {
      e.preventDefault();
      clearReceipts();
      // A staged file is one capture, annotated — never two. The box is read
      // here, at press time, which is what makes the order the file and the
      // words arrived in stop mattering.
      if (staged) {
        var file = staged;
        var note = box.value.trim();
        unstage();
        send(file, note);
        return;
      }
      postText();
    });
```

- [ ] **Step 7: Drop `from_ask` when a file is staged**

No code change is needed — `postText()` is the only sender of `from_ask` and it is no longer reached with a file staged. Confirm by reading `assets/app.js:1110-1134`: `from_ask` is read inside `postText` only. Note this in the commit rather than adding a guard for a path that cannot be taken.

- [ ] **Step 8: Build and run the app**

Run: `cargo build && cargo run`
Then walk both orders in a browser at `/ui`:

1. Type "scan of the lease, break clause p.3" → results appear as you type. Attach a PDF → results stop updating, placeholder reads "What is it, why keep it?", **Ask** dims, **Capture** stays armed, text untouched. Press Capture → one receipt, one artifact.
2. Attach the PDF first → box is already quiet. Type the same sentence → no requests fire (check the network panel). Press Capture → the same single annotated artifact.
3. Attach, type, then press **Remove** → the text is still there, the placeholder is back, **Ask** re-arms, and typing searches again.
4. With no file: type and press Capture → unchanged from today.
5. Search for a phrase from the note → the note comes back, pointing at the file.

- [ ] **Step 9: Commit**

```bash
git add src/web/templates/workspace.html assets/app.js
git commit -m "feat(capture): a staged file makes the box that file's note

Text plus a file ran two captures from one press, with two boxes to
type in and no rule saying which words belonged to which. Now a staged
file makes the one box the file's annotation: one press, one capture,
and the box is read at press time so type-then-attach and
attach-then-type reach the same place.

The box goes quiet while a file waits — no search on keystroke, Ask
disarmed, placeholder swapped. An annotation typed into a live search
box is an embedding call, an activation bump and a Judge row per
phrase, for text nobody asked as a question. Remove gives all of it
back with the text untouched.

from_ask needs no guard: postText is its only sender and a staged file
no longer reaches it."
```

---

## Final verification

- [ ] `cargo test` passes.
- [ ] `cargo clippy --all-targets` is clean of new warnings.
- [ ] The five browser walks in Task 3 Step 8 all behave as written.
- [ ] `git log --oneline -3` shows the three commits.
- [ ] Spec §9 "deleting the file takes the note" needs no task: the note is an
      artifact on the file's corpus like any other, and corpus deletion already
      removes them. No new code path, so no new test.
