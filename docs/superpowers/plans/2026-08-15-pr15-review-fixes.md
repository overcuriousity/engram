# PR 15 Review Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix every finding from the multi-agent review of PR 15 (feat/image-capture) — the eight ranked ones and the ones the reviewer cut as minor — so no captured image can end in a state with no operator recovery.

**Architecture:** The image door (`Core::ingest_image`) and the vision stage (`jobs/describe.rs`) get a real failure model: a reading that cannot happen parks the corpus as `failed` with the reason in `metadata.describe.error`, and a new `reprocess(Describe)` re-queues the read from the stored original ("Re-read" on the corpus page). Permanent 4xx from any inference endpoint becomes a new non-retryable `Error::InferenceRejected`, and the job runner learns what to do with it for the two stages that carry knowledge (Embed: split chunk by chunk; Describe: park). Both capture doors write their rows in one transaction, share one corpus INSERT and one multipart reader, and stop seeding `title_hint` from a filename. Image decoding runs under `spawn_blocking` with dimension/allocation limits, and duplicate detection happens before the decode.

**Tech Stack:** Rust, sqlx/SQLite, tokio, axum, `image` crate. Tests are in-file `#[cfg(test)] mod tests` using `crate::core::test_support::{test_core, test_core_with_describer, test_core_without_vision}` and `crate::infer::fake::FakeDescriber`.

**Spec:** The findings come from the `/code-review 15` run recorded in the "Findings and decisions" section below. The branch's own design spec is `docs/superpowers/plans/2026-08-15-image-capture.md`.

## Global Constraints

- Run `cargo test` after every implementation step; run `cargo fmt` before every commit. `cargo clippy --all-targets` must stay clean.
- Comment style: comments state constraints the code cannot show, in the codebase's essay style. Never "fixed per review".
- Test names are full snake_case sentences, matching the existing suites.
- Commit after each task with a conventional-commit message.
- No schema changes: `corpora`, `attachments`, `jobs` columns are as they are on the branch.
- Statuses: `describing → raw → … → ready`; the only new terminal state used here is the existing `failed`.

## Findings and decisions

Ranked findings (task number in parentheses):

1. Empty vision reading parks the corpus in `needs_review` with `near_dupe_of` NULL — a state no UI/API path can act on (Task 4, Task 5).
2. `reprocess(Synthesize)` on an image still `describing` (unguarded "Re-segment" button) flips it to `raw` with empty text, synthesize marks it `failed`, the pending Describe no-ops (Task 5, Task 6).
3. Permanent 4xx from the vision endpoint is retried forever; Describe has no exhaustion arm and never sets `failed` (Task 1, Task 2, Task 3).
4. `image::prepare` + SHA-256 run synchronously on the async worker (Task 8).
5. `load_from_memory_with_format` has no dimension/allocation limits (Task 7).
6. Filename → `title_hint` fallback disarms the Title stage for every image (Task 9).
7. Text upload handler drains all parts and the last `file` part silently wins (Task 10).
8. Image hashing re-implements `content_hash` inline and runs after the expensive decode (Task 8).

Minor findings the reviewer cut, included by the user's request:

9. Non-transactional corpus / attachment / enqueue writes in `ingest_image`, and the same insert-then-enqueue pattern in `ingest_capture` (Task 11).
10. `insert_image_corpus` duplicates `insert_corpus_with_signature`'s INSERT (Task 11).
11. `upload` and `upload_image` duplicate the multipart loop (Task 10).
12. The near-dupe park block in `describe.rs` duplicates `ingest_capture`'s (Task 4, Task 11).

Decisions from the user (2026-08-15):

- **`reprocess(Describe)` is in scope.** It re-queues the vision read of the stored original; the corpus page gets a "Re-read" button for image corpora. Re-segment is refused while an image corpus is unread.
- **4xx classification applies to all roles via `post_json`.** 400/404/413/415/422 (and any other 4xx except 408 and 429) become non-retryable. Embed's "exhausted → split chunk by chunk" path is preserved for the non-retryable case.
- **Filename → `title_hint` fallback goes for both doors** (images and text uploads). The filename stays in `metadata.file.name`.
- **Both doors become transactional.**

Decision made in planning, flagged for the executor: when the vision role is *not configured*, an exhausted Describe job keeps waiting at the backoff ceiling (the existing test `without_a_vision_role_the_job_waits_rather_than_failing_the_corpus` pins that intent). Every other exhausted or rejected Describe parks the corpus as `failed`.

---

### Task 1: `Error::InferenceRejected` — a non-retryable inference failure

**Files:**
- Modify: `src/error.rs` (enum around line 10; `retryable()` line 42; `status()` line 52; tests line 109)
- Modify: `src/infer/openai.rs:30-63` (`post_json`)
- Modify: `src/infer/fake.rs:533-585` (`FakeDescriber`)

**Interfaces:**
- Produces: `Error::InferenceRejected { role: &'static str, detail: String }` — `retryable() == false`, HTTP status `502 BAD_GATEWAY` (same as `Inference`; check what `Inference` maps to in `status()` and use the same arm).
- Produces: `pub fn permanent_upstream_status(status: reqwest::StatusCode) -> bool` in `src/infer/openai.rs`.
- Produces: `FakeDescriber::rejecting(msg: &str) -> Self` — `describe()` returns `Error::InferenceRejected { role: "vision", detail: msg }`.

- [ ] **Step 1: Write the failing tests**

In `src/error.rs` tests, extend `retryable_only_for_transient_failures`:

```rust
        // The endpoint answered and said no: another attempt sends the same
        // request and gets the same answer.
        assert!(
            !Error::InferenceRejected {
                role: "vision",
                detail: "HTTP 400: model does not accept images".into()
            }
            .retryable()
        );
```

In `src/infer/openai.rs` add a `#[cfg(test)] mod tests` (or extend the existing one) with:

```rust
    #[test]
    fn a_4xx_is_permanent_except_the_two_that_mean_try_again() {
        use reqwest::StatusCode as S;
        assert!(permanent_upstream_status(S::BAD_REQUEST));
        assert!(permanent_upstream_status(S::NOT_FOUND));
        assert!(permanent_upstream_status(S::PAYLOAD_TOO_LARGE));
        assert!(permanent_upstream_status(S::UNSUPPORTED_MEDIA_TYPE));
        assert!(permanent_upstream_status(S::UNPROCESSABLE_ENTITY));
        assert!(!permanent_upstream_status(S::REQUEST_TIMEOUT));
        assert!(!permanent_upstream_status(S::TOO_MANY_REQUESTS));
        assert!(!permanent_upstream_status(S::INTERNAL_SERVER_ERROR));
        assert!(!permanent_upstream_status(S::BAD_GATEWAY));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test error:: infer::openai::tests`
Expected: compile error — `InferenceRejected` and `permanent_upstream_status` do not exist.

- [ ] **Step 3: Implement**

`src/error.rs`, after the `Inference` variant:

```rust
    /// The endpoint understood the request and refused it — a model that does
    /// not take images, a body over its limit, a name it does not serve.
    /// Kept apart from `Inference` because the two ask opposite things of a
    /// worker: one is a wait, the other is the same answer for as long as the
    /// same request is sent.
    #[error("inference[{role}] rejected: {detail}")]
    InferenceRejected { role: &'static str, detail: String },
```

`retryable()` is a `matches!` over the retryable set, so `InferenceRejected` is non-retryable by omission — leave the match as is. In `status()`, add `InferenceRejected` to the same arm as `Inference`.

`src/infer/openai.rs`:

```rust
/// A 4xx that says the request itself is wrong. 408 and 429 are 4xx by number
/// but "come back later" by meaning, and stay retryable.
pub fn permanent_upstream_status(status: reqwest::StatusCode) -> bool {
    status.is_client_error()
        && status != reqwest::StatusCode::REQUEST_TIMEOUT
        && status != reqwest::StatusCode::TOO_MANY_REQUESTS
}
```

In `post_json`, replace the `!status.is_success()` block:

```rust
    if !status.is_success() {
        let body = res.text().await.unwrap_or_default();
        // Truncate: an upstream error page can be megabytes, and this string
        // ends up in a job's last_error column.
        let detail: String = body.chars().take(400).collect();
        let detail = format!("HTTP {status}: {detail}");
        return Err(if permanent_upstream_status(status) {
            Error::InferenceRejected { role, detail }
        } else {
            Error::Inference { role, detail }
        });
    }
```

`src/infer/fake.rs` — `FakeDescriber` gets a `reject: bool` field (default false), a constructor, and the `describe` failure arm honours it:

```rust
    /// The endpoint's "no", not its "not now": what a non-multimodal model
    /// answers with, and what a worker must not retry.
    pub fn rejecting(msg: &str) -> Self {
        let mut d = Self::failing(msg);
        d.reject = true;
        d
    }
```

```rust
        match &self.fail_with {
            Some(m) if self.reject => Err(Error::InferenceRejected {
                role: "vision",
                detail: m.clone(),
            }),
            Some(m) => Err(Error::Inference {
                role: "vision",
                detail: m.clone(),
            }),
            None => Ok(self.reply.clone()),
        }
```

(Adjust to the actual shape of the existing match; `Default` for `FakeDescriber` must set `reject: false`.)

- [ ] **Step 4: Run tests**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/error.rs src/infer/openai.rs src/infer/fake.rs
git commit -m "feat(infer): a 4xx from the endpoint is a rejection, not a retry"
```

---

### Task 2: The job runner handles a rejection for the two stages that carry knowledge

**Files:**
- Modify: `src/jobs/mod.rs:44-140` (`run_claimed`)
- Modify: `src/jobs/describe.rs` (new `pub async fn park_failed`)
- Test: `src/jobs/mod.rs` tests, `src/jobs/describe.rs` tests

**Interfaces:**
- Consumes: `Error::InferenceRejected` (Task 1), `FakeDescriber::rejecting` (Task 1).
- Produces: `describe::park_failed(core: &Core, corpus_id: &str, reason: &str) -> Result<()>` — writes `metadata.describe = {"error": reason}` and sets status `Failed`. Used by Task 3 and Task 4 too.

Behaviour to implement in `run_claimed`:

- `Err(e) if e.retryable()`, `(Stage::Describe, _) if exhausted`: if `core.describer.is_none()` → keep the existing `fail_job` behaviour (log "vision role not configured; waiting"). Otherwise → `describe::park_failed(core, target, &e.to_string())` then `complete_job`.
- The final `Err(e)` (non-retryable) arm gains a match on stage:
  - `(Stage::Embed, "corpus")` → `embed::split_into_artifact_jobs` then `complete_job` (the same thing the exhausted arm does; extract the existing block into a local `async fn split_or_fail(core, &job, &e)` so both arms call it).
  - `(Stage::Embed, _)` (single artifact) → `fail_job(MAX_ATTEMPTS)`, `mark_embed_failed`, `settle_corpus` — the same three writes the retryable-exhausted path already does; extract into `async fn settle_failed_artifact(core, &job)`.
  - `(Stage::Describe, _)` → `describe::park_failed(...)` then `complete_job`.
  - everything else → today's `fail_job(job.id, MAX_ATTEMPTS, ...)`.

- [ ] **Step 1: Write the failing tests**

In `src/jobs/describe.rs` tests:

```rust
    #[tokio::test]
    async fn a_rejected_reading_parks_the_corpus_as_failed_with_the_reason() {
        let core = test_core_with_describer(Arc::new(FakeDescriber::rejecting(
            "HTTP 400: this model does not accept images",
        )))
        .await;
        let id = captured(&core, 6, None).await;
        assert!(crate::jobs::run_one(&core).await.unwrap());
        let src = core.store.get_corpus(&id).await.unwrap();
        assert_eq!(src.status, CorpusStatus::Failed);
        assert!(
            src.metadata["describe"]["error"]
                .as_str()
                .unwrap()
                .contains("does not accept images")
        );
        assert!(
            !core.store.live_job(Stage::Describe, &id).await.unwrap(),
            "the job is closed, not re-armed"
        );
    }

    #[tokio::test]
    async fn a_reading_that_keeps_failing_parks_the_corpus_after_its_attempts() {
        let d = Arc::new(FakeDescriber::failing("gpu on fire"));
        let core = test_core_with_describer(d.clone()).await;
        let id = captured(&core, 7, None).await;
        // Each run_one claims, fails, and re-arms with backoff; the store's
        // clock is what gates the next claim, so drive the row directly.
        for _ in 0..crate::store::jobs::MAX_ATTEMPTS {
            core.store.reset_backoff_for_tests(Stage::Describe, &id).await;
            assert!(crate::jobs::run_one(&core).await.unwrap());
        }
        let src = core.store.get_corpus(&id).await.unwrap();
        assert_eq!(src.status, CorpusStatus::Failed);
        assert!(src.metadata["describe"]["error"].as_str().unwrap().contains("gpu on fire"));
        assert!(!core.store.live_job(Stage::Describe, &id).await.unwrap());
    }

    #[tokio::test]
    async fn without_a_vision_role_an_exhausted_job_keeps_waiting() {
        let core = test_core_without_vision().await;
        let src = core.store
            .insert_image_corpus("h", "image", None, &serde_json::json!({}))
            .await.unwrap().into_corpus();
        core.store.enqueue(Stage::Describe, "corpus", &src.id).await.unwrap();
        for _ in 0..crate::store::jobs::MAX_ATTEMPTS + 1 {
            core.store.reset_backoff_for_tests(Stage::Describe, &src.id).await;
            assert!(crate::jobs::run_one(&core).await.unwrap());
        }
        assert_eq!(core.store.get_corpus(&src.id).await.unwrap().status, CorpusStatus::Describing);
        assert!(core.store.live_job(Stage::Describe, &src.id).await.unwrap());
    }
```

Look at how `src/store/jobs.rs` tests (around line 557, the `MAX_ATTEMPTS + 3` loop) drive a job past its backoff and use the same mechanism. If there is no helper, add `#[cfg(test)] pub async fn reset_backoff_for_tests(&self, stage: Stage, target_id: &str)` to `src/store/jobs.rs` that runs `UPDATE jobs SET run_after = 0 WHERE stage = ? AND target_id = ?`.

In `src/jobs/mod.rs` tests, add an embed test only if a fake embedder can be made to reject; `FakeEmbedder` cannot fail today. Add `FakeEmbedder::rejecting(msg)` (mirror `FakeDescriber::rejecting`) plus a `test_core_with_embedder(Arc<FakeEmbedder>)` in `src/core/mod.rs::test_support` mirroring `test_core_with_describer`, then:

```rust
    #[tokio::test]
    async fn a_batch_embed_the_endpoint_rejects_is_retried_chunk_by_chunk() {
        let core = test_core_with_embedder(Arc::new(FakeEmbedder::rejecting("HTTP 413"))).await;
        let src = core.ingest("alpha para\n\nbeta para", "web", None).await.unwrap();
        // Drive to the batch embed job and run it once.
        while let Some(job) = core.store.claim_job().await.unwrap() {
            if job.stage == Stage::Embed && job.target_kind == "corpus" {
                run_claimed(&core, job).await.unwrap();
                break;
            }
            run_claimed(&core, job).await.unwrap();
        }
        let per_chunk = core.store.pending_artifacts_for_corpus(&src.id).await.unwrap();
        assert!(!per_chunk.is_empty());
        for c in per_chunk {
            assert!(core.store.live_job(Stage::Embed, &c.id).await.unwrap(), "per-chunk unit armed");
        }
    }
```

(If the synthesize path in tests does not reach Embed without extra steps, mirror how `draining_the_queue_takes_a_source_all_the_way_to_ready` in the same module gets there.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test jobs::`
Expected: FAIL — `park_failed`, `reset_backoff_for_tests`, `FakeEmbedder::rejecting`, `test_core_with_embedder` missing; then behavioural failures.

- [ ] **Step 3: Implement**

`src/jobs/describe.rs`:

```rust
/// The read is not going to happen — the model said no, or said nothing for
/// as long as we were willing to ask. The photo stays; the corpus says why it
/// stopped, on the page and on Ops, and `reprocess(Describe)` is the way back.
pub async fn park_failed(core: &Core, corpus_id: &str, reason: &str) -> Result<()> {
    let src = core.store.get_corpus(corpus_id).await?;
    let mut meta = src.metadata.clone();
    meta["describe"] = serde_json::json!({ "error": reason });
    core.store.set_corpus_metadata(corpus_id, &meta).await?;
    core.store.set_corpus_status(corpus_id, CorpusStatus::Failed).await?;
    tracing::warn!(corpus_id, reason, "image could not be read; parked as failed");
    Ok(())
}
```

`src/jobs/mod.rs`: restructure `run_claimed` per the behaviour list above. The retryable-exhausted match gains:

```rust
                // The photo is stored, so nothing is lost by stopping — but a
                // corpus shown as in flight forever is a lie. Unless the role is
                // simply not configured, which is a wait, not a failure.
                (Stage::Describe, _) if exhausted && core.describer.is_some() => {
                    tracing::warn!(error = %e, "could not read this image; parking it");
                    describe::park_failed(core, &job.target_id, &e.to_string()).await?;
                    core.store.complete_job(job.id).await?;
                }
```

The non-retryable arm:

```rust
        Err(e) => {
            tracing::error!(error = %e, "job failed permanently");
            match (job.stage, job.target_kind.as_str()) {
                // Refused as a batch does not mean refused as chunks; the same
                // isolation the exhausted path buys is worth buying here.
                (Stage::Embed, "corpus") => split_or_fail(core, &job, &e).await?,
                (Stage::Embed, _) => settle_failed_artifact(core, &job, &e).await?,
                (Stage::Describe, _) => {
                    describe::park_failed(core, &job.target_id, &e.to_string()).await?;
                    core.store.complete_job(job.id).await?;
                }
                _ => {
                    core.store
                        .fail_job(job.id, MAX_ATTEMPTS, &e.to_string())
                        .await?;
                }
            }
            Ok(true)
        }
```

with the two helpers holding the bodies currently inline in the exhausted arms (unchanged logic, moved).

- [ ] **Step 4: Run tests**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/jobs/mod.rs src/jobs/describe.rs src/store/jobs.rs src/infer/fake.rs src/core/mod.rs
git commit -m "feat(jobs): a read that cannot happen parks the image as failed; a rejected batch embed splits"
```

---

### Task 3: `reprocess(Describe)` — re-read a stored image

**Files:**
- Modify: `src/core/ingest.rs:947-1043` (`reprocess`)
- Modify: `src/jobs/describe.rs` (doc comment near line 9 already fine; nothing else)
- Test: `src/core/ingest.rs` tests

**Interfaces:**
- Consumes: `describe::park_failed` (Task 2) only conceptually — the reverse operation.
- Produces: `Core::reprocess(id, Stage::Describe)` succeeds for a corpus with an image attachment; refuses (Validation) for one without. Needs `Store::attachment_for_corpus` (exists, `src/store/attachments.rs:59`) and two new store methods:
  - `Store::clear_described_text(&self, id: &str) -> Result<()>` — `UPDATE corpora SET raw_text = '', shingles = '', updated_at = ? WHERE id = ?`.
  - `Store::clear_describe_error(&self, id) ` is not needed: read the metadata, remove the `describe` key, `set_corpus_metadata`.

Semantics of Re-read: same cleanup as re-segment (vectors, artifacts, segments, coverage, window jobs, Title job, near-dupe park, dangling supersessions), then clear the read text and signature, drop `metadata.describe`, set status `Describing`, delete any closed Describe job row and enqueue Describe. Refuse when `self.describer.is_none()` with the same message `ingest_image` uses.

- [ ] **Step 1: Write the failing tests**

In `src/core/ingest.rs` tests (the module already imports `ImageCapture`, `Stage`, `CorpusStatus`):

```rust
    fn a_png(seed: u8) -> Vec<u8> {
        use image::{ImageBuffer, Rgb};
        let img = ImageBuffer::from_fn(16, 16, |x, y| Rgb([seed, x as u8, y as u8]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    #[tokio::test]
    async fn re_reading_a_failed_image_clears_the_reason_and_queues_describe_again() {
        let core = test_core().await; // test_core has a default FakeDescriber
        let id = core
            .ingest_image(ImageCapture { bytes: a_png(1), filename: None, title_hint: None, note: None })
            .await.unwrap().id;
        crate::jobs::describe::park_failed(&core, &id, "HTTP 400").await.unwrap();
        core.store.complete_job(core.store.claim_job().await.unwrap().unwrap().id).await.unwrap();

        core.reprocess(&id, Stage::Describe).await.unwrap();

        let src = core.store.get_corpus(&id).await.unwrap();
        assert_eq!(src.status, CorpusStatus::Describing);
        assert!(src.metadata.get("describe").is_none());
        let job = core.store.claim_job().await.unwrap().expect("describe re-armed");
        assert_eq!((job.stage, job.target_id.as_str()), (Stage::Describe, id.as_str()));
    }

    #[tokio::test]
    async fn re_reading_a_ready_image_starts_it_over_from_the_pixels() {
        let core = test_core().await;
        let id = core
            .ingest_image(ImageCapture { bytes: a_png(2), filename: None, title_hint: None, note: None })
            .await.unwrap().id;
        while crate::jobs::run_one(&core).await.unwrap() {}
        assert_eq!(core.store.get_corpus(&id).await.unwrap().status, CorpusStatus::Ready);

        core.reprocess(&id, Stage::Describe).await.unwrap();

        let src = core.store.get_corpus(&id).await.unwrap();
        assert_eq!(src.status, CorpusStatus::Describing);
        assert_eq!(src.raw_text, "");
        assert!(src.shingles.is_empty());
        assert!(core.store.artifacts_for_corpus(&id).await.unwrap().is_empty());
        while crate::jobs::run_one(&core).await.unwrap() {}
        assert_eq!(core.store.get_corpus(&id).await.unwrap().status, CorpusStatus::Ready);
    }

    #[tokio::test]
    async fn a_text_corpus_cannot_be_re_read() {
        let core = test_core().await;
        let src = core.ingest("some text", "web", None).await.unwrap();
        assert!(matches!(
            core.reprocess(&src.id, Stage::Describe).await,
            Err(Error::Validation(_))
        ));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test core::ingest::tests::re_reading core::ingest::tests::a_text_corpus_cannot`
Expected: FAIL — `reprocess(Describe)` returns the "not supported yet" Validation error; `clear_described_text` missing.

- [ ] **Step 3: Implement**

Extract the cleanup shared by Synthesize and Describe into a private method on `Core` in `src/core/ingest.rs`, keeping every existing comment with the line it explains:

```rust
    /// Everything a rerun of the pipeline from `raw_text` has to forget first.
    /// Shared by re-segment and re-read; the comments on each line are the
    /// reasons the line exists.
    async fn forget_derived_work(&self, id: &str) -> Result<()> {
        self.vectors.delete_by_corpus(id).await?;
        for c in self.store.artifacts_for_corpus(id).await? {
            self.store.delete_artifact(&c.id).await?;
        }
        self.store.clear_segments(id).await?;          // (existing comment)
        self.store.clear_corpus_coverage(id).await?;   // (existing comment)
        self.store.delete_window_jobs(id).await?;      // (existing comment)
        self.store.delete_job(Stage::Title, id).await?; // (existing comment)
        self.store.set_near_dupe(id, None, None).await?; // (existing comment)
        Ok(())
    }
```

Then in `reprocess`:

```rust
            Stage::Synthesize | Stage::Enrich => {
                self.forget_derived_work(&src.id).await?;
                self.store.set_corpus_status(&src.id, CorpusStatus::Raw).await?;
                self.heal_dangling_supersessions().await?;
                self.store.enqueue(Stage::Synthesize, "corpus", &src.id).await?;
            }
            // ...
            // A stored image can always be read again — with a better model, or
            // after the endpoint that refused it is fixed. The reading and
            // everything derived from it are replaced wholesale, because a chunk
            // of the old reading has no span in the new one.
            Stage::Describe => {
                if self.store.attachment_for_corpus(&src.id).await?.is_none() {
                    return Err(Error::Validation(
                        "only a captured image can be re-read".into(),
                    ));
                }
                if self.describer.is_none() {
                    return Err(Error::Validation(
                        "image capture is not configured — set [infer.vision] to enable it".into(),
                    ));
                }
                self.forget_derived_work(&src.id).await?;
                self.store.clear_described_text(&src.id).await?;
                let mut meta = src.metadata.clone();
                if let Some(m) = meta.as_object_mut() {
                    m.remove("describe");
                }
                self.store.set_corpus_metadata(&src.id, &meta).await?;
                self.store.set_corpus_status(&src.id, CorpusStatus::Describing).await?;
                self.heal_dangling_supersessions().await?;
                self.store.enqueue(Stage::Describe, "corpus", &src.id).await?;
            }
```

`src/store/corpora.rs`, next to `set_described_text`:

```rust
    /// The reverse of `set_described_text`, for a re-read: no text and no
    /// signature, so the row is comparable to nothing until the model speaks.
    pub async fn clear_described_text(&self, id: &str) -> Result<()> {
        sqlx::query("UPDATE corpora SET raw_text = '', shingles = '', updated_at = ? WHERE id = ?")
            .bind(now())
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
```

Note: `enqueue` uses `Guard::Any`, so a Describe row closed by `complete_job` is re-armed — no explicit delete needed.

- [ ] **Step 4: Run tests**

Run: `cargo test`
Expected: PASS. Also update `an_unknown_reprocess_stage_is_a_400` neighbours in `src/web/api.rs` if any test asserted that `describe` is refused (grep `not supported yet`).

- [ ] **Step 5: Commit**

```bash
git add src/core/ingest.rs src/store/corpora.rs src/web/api.rs
git commit -m "feat(ingest): reprocess(describe) re-reads a stored image from the pixels"
```

---

### Task 4: An empty reading parks as `failed`; the near-dupe park is shared

**Files:**
- Modify: `src/jobs/describe.rs:41-56` and `:63-93`
- Modify: `src/core/ingest.rs` (new `pub(crate) async fn park_or_queue`)
- Test: `src/jobs/describe.rs` tests (`an_empty_reading_parks_the_corpus_with_the_reason`)

**Interfaces:**
- Consumes: `describe::park_failed` (Task 2).
- Produces: `Core::park_or_queue(&self, corpus_id: &str, near: Option<&NearDuplicate>) -> Result<()>` — `Some` → `set_near_dupe` + status `NeedsReview` + info log; `None` → status `Raw` + enqueue Synthesize. `describe::run` calls it after `set_described_text`. (`ingest_capture` stops needing it in Task 11, where the park is written at insert time; until then leave `ingest_capture` as is.)

- [ ] **Step 1: Change the test**

Rename and rewrite `an_empty_reading_parks_the_corpus_with_the_reason` → `an_empty_reading_parks_the_corpus_as_failed_with_the_reason`: assert `CorpusStatus::Failed` instead of `NeedsReview`, and add:

```rust
        assert!(src.near_dupe_of.is_none(), "not a near-duplicate; not on the review queue");
        assert!(core.store.parked_corpora(10).await.unwrap().is_empty());
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test an_empty_reading_parks`
Expected: FAIL on the status assertion.

- [ ] **Step 3: Implement**

`src/jobs/describe.rs`, the empty branch becomes:

```rust
    if text.trim().is_empty() {
        // Not a near-duplicate, so not the review queue: that page offers
        // "keep" and "discard" against another corpus, and there is none.
        park_failed(core, corpus_id, "the model returned no text for this image").await?;
        return Ok(());
    }
```

and the tail:

```rust
    let sig = crate::store::shingle::signature(&text);
    let near = core.store.find_near_duplicate(&sig, core.consolidate.near_dupe_min).await?;
    core.store.set_described_text(corpus_id, &text, sig).await?;
    core.park_or_queue(corpus_id, near.as_ref()).await?;
    tracing::info!(corpus_id, chars = text.len(), parked = near.is_some(), "image read");
    Ok(())
```

`src/core/ingest.rs`, on `impl Core`:

```rust
    /// The fork every capture reaches once its text is known: parked next to
    /// what it resembles, or queued for synthesis. The status write is here so
    /// no caller can park without saying what it parked beside.
    pub(crate) async fn park_or_queue(
        &self,
        corpus_id: &str,
        near: Option<&crate::store::corpora::NearDuplicate>,
    ) -> Result<()> {
        match near {
            Some(n) => {
                self.store.set_near_dupe(corpus_id, Some(&n.corpus_id), Some(n.similarity)).await?;
                self.store.set_corpus_status(corpus_id, CorpusStatus::NeedsReview).await?;
                tracing::info!(corpus_id, near = %n.corpus_id, similarity = n.similarity,
                    "looks like an existing corpus; parked for review");
            }
            None => {
                self.store.set_corpus_status(corpus_id, CorpusStatus::Raw).await?;
                self.store.enqueue(Stage::Synthesize, "corpus", corpus_id).await?;
            }
        }
        Ok(())
    }
```

- [ ] **Step 4: Run tests**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/jobs/describe.rs src/core/ingest.rs
git commit -m "fix(describe): an empty reading is a failure with a way back, not a review item"
```

---

### Task 5: Re-segment refuses an unread image

**Files:**
- Modify: `src/core/ingest.rs` (`reprocess`, `Stage::Synthesize | Stage::Enrich` arm)
- Test: `src/core/ingest.rs` tests

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn re_segmenting_an_image_that_has_not_been_read_is_refused() {
        let core = test_core().await;
        let id = core
            .ingest_image(ImageCapture { bytes: a_png(3), filename: None, title_hint: None, note: None })
            .await.unwrap().id;
        // Still describing.
        assert!(matches!(core.reprocess(&id, Stage::Synthesize).await, Err(Error::Validation(_))));
        // Failed before any text was read.
        crate::jobs::describe::park_failed(&core, &id, "HTTP 400").await.unwrap();
        assert!(matches!(core.reprocess(&id, Stage::Synthesize).await, Err(Error::Validation(_))));
        // The describe job is untouched either way.
        assert!(core.store.live_job(Stage::Describe, &id).await.unwrap());
        assert_eq!(core.store.get_corpus(&id).await.unwrap().status, CorpusStatus::Failed);
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test re_segmenting_an_image_that_has_not_been_read`
Expected: FAIL — reprocess succeeds and flips to Raw.

- [ ] **Step 3: Implement**

At the top of the `Stage::Synthesize | Stage::Enrich` arm:

```rust
                // Re-segmenting starts from `raw_text`, and an image whose read
                // has not landed has none. Flipping it to `raw` would have
                // synthesis fail on empty text and the pending read then find
                // a corpus that is no longer `describing` — a photo never read,
                // by way of a button that promised to process it.
                if src.status == CorpusStatus::Describing
                    || (src.origin == ORIGIN_IMAGE && src.raw_text.trim().is_empty())
                {
                    return Err(Error::Validation(
                        "this image has not been read yet — re-read it instead".into(),
                    ));
                }
```

- [ ] **Step 4: Run tests**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/core/ingest.rs
git commit -m "fix(ingest): re-segment refuses an image that has no reading yet"
```

---

### Task 6: Corpus page — "Re-read" for images, "Re-segment" only when there is text

**Files:**
- Modify: `src/web/templates/corpus.html:4-18`
- Modify: `src/web/ui.rs:368-392` (`CorpusTemplate`), `:800-835` (construction), `:935-943` (`reprocess_ui`)
- Test: `src/web/ui.rs` tests

**Interfaces:**
- Produces: `POST /ui/corpora/{id}/reprocess` accepts an optional form field `stage` (`synthesize` default, `describe`), parsed with `Stage::parse`.
- Template fields: `image: bool` (exists), new `unread: bool` = `image && (status == describing || lines.is_empty())`.

- [ ] **Step 1: Write the failing tests**

In `src/web/ui.rs` tests (use the module's existing helpers for building the app and posting forms; look at how `/ui/corpora/abc/reprocess` is exercised around line 2500):

```rust
    #[tokio::test]
    async fn an_unread_image_page_offers_re_read_and_not_re_segment() {
        let (app, core) = ui_app_and_core().await; // whatever the module's helper is called
        let id = core.ingest_image(crate::core::ingest::ImageCapture {
            bytes: a_png(1), filename: Some("p.png".into()), title_hint: None, note: None,
        }).await.unwrap().id;
        let html = get_html(&app, &format!("/ui/corpora/{id}")).await;
        assert!(html.contains("Re-read"));
        assert!(!html.contains("Re-segment"));
    }

    #[tokio::test]
    async fn the_re_read_button_queues_describe() {
        let (app, core) = ui_app_and_core().await;
        let id = core.ingest_image(/* as above */).await.unwrap().id;
        crate::jobs::describe::park_failed(&core, &id, "HTTP 400").await.unwrap();
        let res = post_form(&app, &format!("/ui/corpora/{id}/reprocess"), "stage=describe").await;
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(core.store.get_corpus(&id).await.unwrap().status, CorpusStatus::Describing);
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test web::ui::tests::an_unread_image_page web::ui::tests::the_re_read_button`
Expected: FAIL.

- [ ] **Step 3: Implement**

`corpus.html`:

```html
  {# Re-segmenting a placeholder would delete the restored artifacts and pay the
     model to re-derive them from their own text. Nothing to re-segment. An
     image with no reading yet has nothing to re-segment either. #}
  {% if !restored && !unread %}
  <form method="post" action="/ui/corpora/{{ id }}/reprocess" style="display:inline">
    <button class="btn btn-sm" type="submit">Re-segment</button>
  </form>
  {% endif %}
  {% if image %}
  <form method="post" action="/ui/corpora/{{ id }}/reprocess" style="display:inline"
        onsubmit="return confirm('Read the photo again? The current reading and its artifacts are replaced.')">
    <input type="hidden" name="stage" value="describe">
    <button class="btn btn-sm" type="submit">Re-read</button>
  </form>
  {% endif %}
```

`ui.rs`: add `unread: bool` to `CorpusTemplate` with a doc comment; set `unread: image && (s.status == CorpusStatus::Describing || lines.is_empty())` (compute before `lines` is moved). `reprocess_ui`:

```rust
#[derive(serde::Deserialize, Default)]
struct ReprocessForm {
    #[serde(default)]
    stage: Option<String>,
}

async fn reprocess_ui(
    State(st): State<AppState>,
    _id: Identity,
    Path(cid): Path<String>,
    form: Option<axum::Form<ReprocessForm>>,
) -> Result<Response> {
    let stage = match form.and_then(|f| f.0.stage) {
        None => crate::store::jobs::Stage::Synthesize,
        Some(s) => crate::store::jobs::Stage::parse(&s)
            .ok_or_else(|| Error::Validation(format!("unknown stage `{s}`")))?,
    };
    st.core.reprocess(&cid, stage).await?;
    Ok(Redirect::to(&format!("/ui/corpora/{cid}")).into_response())
}
```

(Check whether the existing form posts with a body; if the extractor rejects an empty body, use `Option<Form<_>>` as shown or `axum::extract::RawForm`.)

- [ ] **Step 4: Run tests**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/web/templates/corpus.html src/web/ui.rs
git commit -m "feat(ui): re-read a captured image; re-segment only once it has a reading"
```

---

### Task 7: Decode limits for uploaded images

**Files:**
- Modify: `src/core/image.rs:25-70` (`prepare`)
- Test: `src/core/image.rs` tests

**Interfaces:**
- Produces: constants `MAX_IMAGE_EDGE: u32 = 12_000` and `MAX_DECODE_BYTES: u64 = 256 * 1024 * 1024` in `src/core/image.rs`; `prepare` refuses over-limit images with `Error::Validation`.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn an_image_declaring_absurd_dimensions_is_refused_before_it_is_decoded() {
        // A valid PNG header for a 20000x20000 RGBA image, no pixel data worth
        // the name: the decoder must stop at the header.
        let mut png = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut png, 20_000, 20_000);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let _ = enc.write_header(); // header only; the stream is truncated
        }
        let e = prepare(&png, 2048).unwrap_err();
        assert!(matches!(e, Error::Validation(_)), "{e}");
        assert!(e.to_string().contains("large"), "{e}");
    }
```

If the `png` crate is not a direct dependency, build the header by hand: PNG signature + IHDR chunk (width/height big-endian, depth 8, colour type 6) with a correct CRC via the `crc32fast` crate if present, else copy the 33 bytes from a real 1×1 PNG and patch the width/height and CRC. Whichever the executor picks, the test must not add a new dependency without noting it in the commit.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test core::image::tests::an_image_declaring_absurd`
Expected: FAIL — either the decode error message doesn't mention "large" or the process allocates 1.6 GB and the assertion on the message fails.

- [ ] **Step 3: Implement**

```rust
/// Longest side a capture may have. Phone sensors top out around 9 000 px on
/// the long edge; the cap exists for the file that *claims* to be bigger, which
/// costs width × height × 4 bytes before a single pixel is checked.
pub const MAX_IMAGE_EDGE: u32 = 12_000;
/// Ceiling on what the decoder may allocate for one image.
pub const MAX_DECODE_BYTES: u64 = 256 * 1024 * 1024;
```

Replace `image::load_from_memory_with_format` with:

```rust
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_EDGE);
    limits.max_image_height = Some(MAX_IMAGE_EDGE);
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    reader.limits(limits);
    let decoded = reader.decode().map_err(|e| match e {
        image::ImageError::Limits(_) => Error::Validation(format!(
            "that image is too large to read — at most {MAX_IMAGE_EDGE} pixels on a side"
        )),
        e => Error::Validation(format!("that image could not be decoded: {e}")),
    })?;
```

(`ImageReader::with_format` and `Limits` exist in `image` 0.25; check `Cargo.toml` for the version.)

- [ ] **Step 4: Run tests**

Run: `cargo test core::image`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/core/image.rs
git commit -m "fix(image): cap decode dimensions and allocation before reading the pixels"
```

---

### Task 8: Hash first, then decode off the async worker

**Files:**
- Modify: `src/core/ingest.rs:248-320` (`ingest_image`)
- Modify: `src/store/corpora.rs:118` (`content_hash`)
- Test: `src/core/ingest.rs` tests, `src/store/corpora.rs` tests

**Interfaces:**
- Produces: `pub fn content_hash(bytes: impl AsRef<[u8]>) -> String` in `src/store/corpora.rs` — the existing `&str` callers keep compiling.
- `ingest_image` order: hash → `find_by_hash` (duplicate short-circuit) → `spawn_blocking(prepare)` → insert.

- [ ] **Step 1: Write the failing tests**

`src/store/corpora.rs` tests:

```rust
    #[test]
    fn content_hash_is_one_function_for_text_and_bytes() {
        assert_eq!(content_hash("abc"), content_hash(b"abc"));
        assert_eq!(content_hash("abc"), hex::encode(Sha256::digest(b"abc")));
    }
```

`src/core/ingest.rs` tests:

```rust
    #[tokio::test]
    async fn a_duplicate_image_is_recognised_without_being_decoded() {
        let core = test_core().await;
        let bytes = a_png(4);
        let first = core.ingest_image(ImageCapture { bytes: bytes.clone(), filename: None, title_hint: None, note: None }).await.unwrap();
        // The same hash, but bytes that would not decode: if the door hashed
        // and looked up before decoding, this is a duplicate; if it decoded
        // first, it is a 400.
        let src = core.store.get_corpus(&first.id).await.unwrap();
        assert_eq!(src.content_hash, crate::store::corpora::content_hash(&bytes));
        let again = core.ingest_image(ImageCapture { bytes, filename: None, title_hint: None, note: None }).await.unwrap();
        assert!(again.duplicate);
        assert_eq!(again.id, first.id);
    }
```

(The "would not decode" half is not directly testable without a hash collision; the test pins the hash algorithm and the duplicate answer. The ordering is enforced by code review of the diff.)

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test content_hash_is_one_function a_duplicate_image_is_recognised`
Expected: compile error on `content_hash(b"abc")`.

- [ ] **Step 3: Implement**

`src/store/corpora.rs`:

```rust
/// The dedupe key of a corpus: text for a capture, the file's bytes for an
/// image. One function, so the two doors can never drift onto different keys
/// for the same column.
pub fn content_hash(bytes: impl AsRef<[u8]>) -> String {
    hex::encode(Sha256::digest(bytes.as_ref()))
}
```

`ingest_image`:

```rust
        let hash = content_hash(&c.bytes);
        if let Some(existing) = self.store.find_by_hash(&hash).await? {
            tracing::info!(corpus_id = %existing.id, "duplicate image, returning existing source");
            return Ok(IngestOutcome { id: existing.id, status: existing.status, duplicate: true, near_duplicate: None });
        }
        // Decoding, EXIF, the preview and its re-encode are a synchronous walk
        // over up to `image_max_bytes` of pixels. Held on a Tokio worker that
        // is seconds during which search, health and the queue poll on that
        // thread all wait; see `web::api::extract` for the same move.
        let edge = self.capture.image_preview_edge;
        let (bytes, prepared) = tokio::task::spawn_blocking(move || {
            let prepared = super::image::prepare(&c.bytes, edge)?;
            Ok::<_, Error>((c.bytes, prepared))
        })
        .await
        .map_err(|e| Error::Internal(format!("image preparation did not finish: {e}")))??;
```

Restructure so `c` is destructured before the closure (`let ImageCapture { bytes, filename, title_hint, note } = c;`) and only `bytes` moves in; the remaining code uses `bytes` where it used `c.bytes`. Remove `use sha2::Digest`/`hex` from `ingest.rs` if now unused.

- [ ] **Step 4: Run tests**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/core/ingest.rs src/store/corpora.rs
git commit -m "perf(ingest): hash and dedupe an image before decoding it, off the async worker"
```

---

### Task 9: No door seeds `title_hint` from a filename

**Files:**
- Modify: `src/core/ingest.rs` (`ingest_image`, the `title_hint` line ~277)
- Modify: `src/web/api.rs:342-350` (`upload`, drop `.with_title(filename.clone())`)
- Test: `src/web/api.rs:1395` (`an_uploaded_filename_becomes_the_title_hint` → rewrite), `src/core/ingest.rs` tests

- [ ] **Step 1: Rewrite the tests**

`src/web/api.rs`: rename `an_uploaded_filename_becomes_the_title_hint` → `an_uploaded_filename_is_a_file_fact_not_a_title` and assert:

```rust
        assert_eq!(src.title_hint, None, "the Title stage names it; the filename is not a name");
        assert_eq!(src.metadata["file"]["name"], "mounting-notes.txt");
```

`src/core/ingest.rs` tests:

```rust
    #[tokio::test]
    async fn an_image_filename_is_kept_as_a_file_fact_and_not_used_as_its_title() {
        let core = test_core().await;
        let id = core.ingest_image(ImageCapture {
            bytes: a_png(5), filename: Some("photo.jpg".into()), title_hint: None, note: None,
        }).await.unwrap().id;
        let src = core.store.get_corpus(&id).await.unwrap();
        assert_eq!(src.title_hint, None);
        assert_eq!(src.metadata["file"]["name"], "photo.jpg");
        while crate::jobs::run_one(&core).await.unwrap() {}
        assert!(core.store.get_corpus(&id).await.unwrap().title_hint.is_some(), "the Title stage named it");
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test an_uploaded_filename_is_a_file_fact an_image_filename_is_kept`
Expected: FAIL on the `title_hint == None` assertions.

- [ ] **Step 3: Implement**

`ingest_image`: `let title_hint = c.title_hint;` (drop the `or_else`). Add the comment:

```rust
        // A filename is a file fact, not a name: `photo.jpg` and `image.png`
        // are what a camera and a clipboard call everything. Seeding the title
        // from it would disarm the one stage that can name the capture.
```

`api.rs::upload`: remove `.with_title(filename.clone())`; the filename already flows through `.with_file(...)`.

- [ ] **Step 4: Run tests**

Run: `cargo test`
Expected: PASS. Check `src/web/api.rs` for other tests asserting a filename title (grep `title_hint.as_deref(), Some("` in the upload tests) and update them the same way.

- [ ] **Step 5: Commit**

```bash
git add src/core/ingest.rs src/web/api.rs
git commit -m "fix(ingest): a filename is a file fact, not a title; let the Title stage name uploads"
```

---

### Task 10: One multipart reader for both doors; a second file part is a 400

**Files:**
- Modify: `src/web/api.rs:279-300` (`upload`), `:361-395` (`upload_image`)
- Test: `src/web/api.rs` tests

**Interfaces:**
- Produces, in `src/web/api.rs`:

```rust
struct FilePart {
    filename: Option<String>,
    declared: String,       // Content-Type or ""
    bytes: axum::body::Bytes,
}

struct UploadParts {
    file: Option<FilePart>,
    /// The text fields asked for, by name; only non-blank values are kept.
    fields: std::collections::HashMap<&'static str, String>,
}

/// Drain a multipart body: one file part under `file_field`, any of
/// `text_fields` as text. Order-independent, because a browser sends `note`
/// before or after the file depending on the form. A second part under
/// `file_field` is refused rather than silently winning or losing: two files
/// in one request is a client bug, and whichever we picked would be wrong for
/// half of them.
async fn read_upload(
    mut multipart: axum::extract::Multipart,
    file_field: &'static str,
    text_fields: &[&'static str],
) -> Result<UploadParts>
```

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn an_upload_with_two_file_parts_is_refused() {
        let (app, token, _core) = app_token_and_core().await;
        // Build the body by hand: two `file` parts.
        let boundary = "xyz";
        let body = format!(
            "--{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\nContent-Type: text/plain\r\n\r\nfirst\r\n\
             --{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"b.txt\"\r\nContent-Type: text/plain\r\n\r\nsecond\r\n\
             --{b}--\r\n",
            b = boundary
        );
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/corpora/upload")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", format!("multipart/form-data; boundary={boundary}"))
            .body(Body::from(body))
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert!(json_of(res).await["error"].as_str().unwrap().contains("one file"));
    }
```

(Model the request construction on `post_file_with` at line 960.) Add the mirror test for `/api/v1/corpora/image` with two `image` parts.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test an_upload_with_two_file_parts_is_refused`
Expected: FAIL — 201 (second file ingested).

- [ ] **Step 3: Implement**

```rust
async fn read_upload(
    mut multipart: axum::extract::Multipart,
    file_field: &'static str,
    text_fields: &[&'static str],
) -> Result<UploadParts> {
    let mut out = UploadParts { file: None, fields: Default::default() };
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| Error::Validation(format!("malformed upload: {e}")))?
    {
        let Some(name) = field.name().map(str::to_string) else { continue };
        if name == file_field {
            if out.file.is_some() {
                return Err(Error::Validation(format!(
                    "more than one `{file_field}` part — send one file per request"
                )));
            }
            let filename = field.file_name().map(str::to_string);
            let declared = field.content_type().unwrap_or("").to_string();
            let bytes = field.bytes().await
                .map_err(|e| Error::Validation(format!("upload failed: {e}")))?;
            out.file = Some(FilePart { filename, declared, bytes });
        } else if let Some(key) = text_fields.iter().copied().find(|k| *k == name) {
            let text = field.text().await
                .map_err(|e| Error::Validation(format!("malformed upload: {e}")))?;
            if !text.trim().is_empty() {
                out.fields.insert(key, text);
            }
        }
    }
    Ok(out)
}
```

`upload`:

```rust
    let parts = read_upload(multipart, "file", &["note"]).await?;
    let Some(FilePart { filename, declared, bytes }) = parts.file else {
        return Err(Error::Validation("no file in the upload".into()));
    };
    let note = parts.fields.get("note").cloned();
    // ... existing type/UTF-8 checks and ingest_capture call unchanged
```

`upload_image`:

```rust
    let mut parts = read_upload(multipart, "image", &["note", "title_hint"]).await?;
    let Some(FilePart { filename, bytes, .. }) = parts.file else {
        return Err(Error::Validation("no image in the upload".into()));
    };
    let out = st.core.ingest_image(crate::core::ingest::ImageCapture {
        bytes: bytes.to_vec(),
        filename,
        title_hint: parts.fields.remove("title_hint"),
        note: parts.fields.remove("note"),
    }).await?;
```

Note `note` previously kept a blank value and `ingest_capture`'s `clean_note` dropped it; keeping only non-blank in `read_upload` is equivalent. Verify `a_text_upload_records_its_note_and_file_facts` and `an_image_upload_is_accepted_with_its_note_and_queued` still pass.

- [ ] **Step 4: Run tests**

Run: `cargo test web::api`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/web/api.rs
git commit -m "refactor(api): one multipart reader for both doors; a second file part is refused"
```

---

### Task 11: One corpus INSERT, and each door's writes in one transaction

**Files:**
- Modify: `src/store/corpora.rs:150-250` (`insert_corpus_with_signature`), `:455-495` (`insert_image_corpus`)
- Modify: `src/store/attachments.rs:40-57` (`insert_attachment`)
- Modify: `src/store/jobs.rs:153-215` (`enqueue` / `upsert_job`)
- Modify: `src/core/ingest.rs` (`ingest_capture` ~150-245, `ingest_image` ~248-320)
- Test: `src/store/corpora.rs` tests, `src/core/ingest.rs` tests, `src/jobs/describe.rs` tests (the `insert_image_corpus("h", ...)` call sites)

**Interfaces:**

In `src/store/corpora.rs`:

```rust
/// What a capture wants done in the same transaction as its row.
pub enum Followup {
    /// Queue this stage. Text captures queue Synthesize; images queue Describe.
    Queue(Stage),
    /// Park beside a near-duplicate: `near_dupe_of`, its score, and
    /// `needs_review`, written on the row itself. No job.
    Park { of: String, similarity: f64 },
}

/// The one INSERT every corpus row goes through, on whatever executor the
/// caller is inside. `ON CONFLICT(content_hash) DO NOTHING`; returns whether
/// the row was written.
async fn insert_corpus_row<'e>(
    exec: impl sqlx::Executor<'e, Database = sqlx::Sqlite>,
    src: &Corpus,
) -> Result<bool>

pub async fn insert_corpus_with_signature(
    &self,
    raw_text: &str,
    origin: &str,
    title_hint: Option<&str>,
    shingles: Vec<u64>,
    source_url: Option<&str>,
    metadata: &serde_json::Value,
    followup: Followup,           // NEW
) -> Result<Insertion>

pub async fn insert_image_corpus(
    &self,
    content_hash: &str,
    origin: &str,
    title_hint: Option<&str>,
    metadata: &serde_json::Value,
    attachment: &super::attachments::NewImage<'_>,   // NEW: everything but corpus_id
) -> Result<Insertion>
```

In `src/store/attachments.rs`: `NewAttachment` keeps its shape; add `pub(crate) async fn insert_attachment_with<'e>(exec: impl Executor<'e, Database = Sqlite>, a: &NewAttachment<'_>) -> Result<i64>` and have `insert_attachment` call it with `&self.pool`. Add `pub struct NewImage<'a> { kind, mime, filename, bytes, preview, width, height }` — `NewAttachment` minus `corpus_id` — with `fn for_corpus(&self, corpus_id: &'a str) -> NewAttachment<'a>`.

In `src/store/jobs.rs`: `pub(crate) async fn enqueue_with<'e>(exec: impl Executor<'e, Database = Sqlite>, stage, target_kind, target_id) -> Result<()>` — the `Guard::Any` upsert on the given executor; `upsert_job` calls it with `&self.pool` for `Guard::Any` (or generalise `upsert_job` to take the executor and keep the guard; executor's choice, but one statement text, no duplication).

Insertion semantics: `insert_corpus_with_signature` opens `let mut tx = self.pool.begin().await?;`, calls `insert_corpus_row(&mut *tx, &src)`; if `false` → `tx.rollback()`, `find_by_hash`, `Insertion::Existing`; else apply the followup (`Queue` → `enqueue_with(&mut *tx, ...)`; `Park` is already on the row: build `src` with `near_dupe_of`, `near_dupe_score`, `status: NeedsReview` before insert), commit, `Insertion::Created(src)`. `insert_image_corpus` does row + `insert_attachment_with` + `enqueue_with(Describe)` in one tx.

- [ ] **Step 1: Write the failing tests**

`src/store/corpora.rs` tests:

```rust
    #[tokio::test]
    async fn a_text_capture_and_its_job_land_together() {
        let s = Store::memory().await.unwrap();
        let ins = s.insert_corpus_with_signature("hello", "web", None, vec![], None,
            &serde_json::json!({}), Followup::Queue(Stage::Synthesize)).await.unwrap();
        let src = ins.into_corpus();
        assert_eq!(src.status, CorpusStatus::Raw);
        assert!(s.live_job(Stage::Synthesize, &src.id).await.unwrap());
    }

    #[tokio::test]
    async fn a_parked_capture_is_written_parked_with_no_job() {
        let s = Store::memory().await.unwrap();
        let other = s.insert_corpus("other", "web", None).await.unwrap();
        let src = s.insert_corpus_with_signature("hello", "web", None, vec![], None,
            &serde_json::json!({}), Followup::Park { of: other.id.clone(), similarity: 0.9 })
            .await.unwrap().into_corpus();
        assert_eq!(src.status, CorpusStatus::NeedsReview);
        assert_eq!(src.near_dupe_of.as_deref(), Some(other.id.as_str()));
        assert!(!s.live_job(Stage::Synthesize, &src.id).await.unwrap());
        assert_eq!(s.parked_corpora(10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_image_row_its_attachment_and_its_job_land_together() {
        let s = Store::memory().await.unwrap();
        let img = NewImage { kind: "image", mime: "image/png", filename: Some("x.png"),
            bytes: b"orig", preview: b"prev", width: Some(10), height: Some(20) };
        let src = s.insert_image_corpus("hash-1", "image", None, &serde_json::json!({}), &img)
            .await.unwrap().into_corpus();
        assert_eq!(src.status, CorpusStatus::Describing);
        assert!(s.attachment_for_corpus(&src.id).await.unwrap().is_some());
        assert!(s.live_job(Stage::Describe, &src.id).await.unwrap());
        // The same hash again: Existing, and nothing new written.
        assert!(matches!(
            s.insert_image_corpus("hash-1", "image", None, &serde_json::json!({}), &img).await.unwrap(),
            Insertion::Existing(_)
        ));
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test store::corpora`
Expected: compile errors (`Followup`, `NewImage`, new signatures).

- [ ] **Step 3: Implement**

`insert_corpus_row`:

```rust
async fn insert_corpus_row<'e>(
    exec: impl sqlx::Executor<'e, Database = sqlx::Sqlite>,
    src: &Corpus,
) -> Result<bool> {
    let res = sqlx::query(
        "INSERT INTO corpora (id, raw_text, origin, title_hint, content_hash, status, created_at, updated_at,
                              shingles, source_url, metadata, near_dupe_of, near_dupe_score)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(content_hash) DO NOTHING",
    )
    .bind(&src.id).bind(&src.raw_text).bind(&src.origin).bind(&src.title_hint)
    .bind(&src.content_hash).bind(src.status.as_str()).bind(src.created_at).bind(src.updated_at)
    .bind(super::shingle::encode(&src.shingles)).bind(&src.source_url).bind(src.metadata.to_string())
    .bind(&src.near_dupe_of).bind(src.near_dupe_score)
    .execute(exec)
    .await?;
    Ok(res.rows_affected() > 0)
}
```

(Verify column names `near_dupe_of` / `near_dupe_score` against `src/store/schema.sql`; keep the `ensure_restored_corpus` INSERT separate — it writes `restored_at` and is a different row kind.)

`insert_corpus_with_signature` builds `src` as today, then:

```rust
        let (status, near_dupe_of, near_dupe_score) = match &followup {
            Followup::Park { of, similarity } => (CorpusStatus::NeedsReview, Some(of.clone()), Some(*similarity)),
            Followup::Queue(_) => (CorpusStatus::Raw, None, None),
        };
        let src = Corpus { status, near_dupe_of, near_dupe_score, /* ...as before */ };
        let mut tx = self.pool.begin().await?;
        if !insert_corpus_row(&mut *tx, &src).await? {
            tx.rollback().await?;
            let existing = self.find_by_hash(&src.content_hash).await?.ok_or_else(|| {
                Error::Store("capture conflicted with a corpus that then vanished".into())
            })?;
            return Ok(Insertion::Existing(existing));
        }
        if let Followup::Queue(stage) = followup {
            super::jobs::enqueue_with(&mut *tx, stage, "corpus", &src.id).await?;
        }
        tx.commit().await?;
        Ok(Insertion::Created(src))
```

Keep the existing doc comment about the concurrent-writer window and add: "The job is armed in the same transaction as the row: a process that dies between the two would otherwise leave a `raw` corpus nothing will ever pick up."

`insert_image_corpus` builds a `Corpus` with `raw_text: ""`, `shingles: vec![]`, `status: Describing`, `source_url: None`, `restored_at: None`, `coverage: None`, near fields `None`, then:

```rust
        let mut tx = self.pool.begin().await?;
        if !insert_corpus_row(&mut *tx, &src).await? {
            tx.rollback().await?;
            let existing = self.find_by_hash(content_hash).await?.ok_or_else(|| {
                Error::Store("image capture conflicted with a corpus that then vanished".into())
            })?;
            return Ok(Insertion::Existing(existing));
        }
        super::attachments::insert_attachment_with(&mut *tx, &attachment.for_corpus(&src.id)).await?;
        super::jobs::enqueue_with(&mut *tx, Stage::Describe, "corpus", &src.id).await?;
        tx.commit().await?;
        Ok(Insertion::Created(src))
```

`ingest_capture`: replace the insert + the `match &near` block with:

```rust
        let followup = match &near {
            Some(n) => Followup::Park { of: n.corpus_id.clone(), similarity: n.similarity },
            None => Followup::Queue(Stage::Synthesize),
        };
        let src = match self.store.insert_corpus_with_signature(text, origin, title_hint, sig,
            c.source_url.as_deref(), &c.metadata, followup).await? { /* as before */ };
        match &near {
            Some(n) => tracing::info!(corpus_id = %src.id, near = %n.corpus_id, similarity = n.similarity,
                "capture looks like an existing corpus; parked for review"),
            None => tracing::info!(corpus_id = %src.id, origin, bytes = text.len(), "ingested"),
        }
```

`ingest_image`: build `NewImage { kind: "image", mime: prepared.mime, filename: filename.as_deref(), bytes: &bytes, preview: &prepared.preview_jpeg, width: Some(..), height: Some(..) }` and pass it to `insert_image_corpus`; delete the separate `insert_attachment` and `enqueue` calls. Update `insert_corpus` (the plain wrapper at line ~150) to pass `Followup::Queue(Stage::Synthesize)`? No — `insert_corpus` is used by tests that then run the queue themselves; check its callers (`grep -n "insert_corpus(" src`) and keep its behaviour (no job) by giving `Followup` a third variant only if a caller needs one. Prefer: `insert_corpus` passes `Followup::Queue(Stage::Synthesize)` if every caller is a test that drains the queue anyway, else add `Followup::Nothing`. Executor decides after grepping; note the choice in the commit message.

Update the two `insert_image_corpus("h", "image", None, &json!({}))` call sites in `src/jobs/describe.rs` tests to pass a `NewImage` (any bytes).

- [ ] **Step 4: Run tests**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/store src/core/ingest.rs src/jobs/describe.rs
git commit -m "refactor(store): one corpus insert; each capture door writes its row, attachment and job in one transaction"
```

---

### Task 12: Docs

**Files:**
- Modify: `README.md` (image capture section added by 57aaf58), `ROADMAP.md`, `src/core/ingest.rs` (remove the "later feature" comment if any remains)

- [ ] **Step 1: Update**

In README's image capture section, add one paragraph: a photo whose reading fails (endpoint refuses, model returns nothing, retries exhausted) is shown as `failed` with the reason on its page; "Re-read" (or `POST /api/v1/corpora/{id}/reprocess {"stage":"describe"}`) reads it again from the stored original. In ROADMAP, move "re-read a stored image" from later to done, if listed.

- [ ] **Step 2: Commit**

```bash
git add README.md ROADMAP.md
git commit -m "docs: re-read and failure states of image capture"
```

---

## Self-review

- Finding 1 → Tasks 4 (Failed, not NeedsReview) and 3/6 (recovery). Finding 2 → Tasks 5, 6. Finding 3 → Tasks 1, 2, 3. Finding 4 → Task 8. Finding 5 → Task 7. Finding 6 → Task 9. Finding 7 → Task 10. Finding 8 → Task 8. Minor 9/10/12 → Task 11 (+4). Minor 11 → Task 10.
- Type consistency: `park_failed(core, id, reason)` used identically in Tasks 2–6; `Followup::{Queue, Park}` and `NewImage` only in Task 11; `content_hash(impl AsRef<[u8]>)` in Task 8 is what Task 11's `insert_image_corpus` caller already computes.
- Order matters: Task 11 changes `insert_image_corpus`'s signature, which Task 2's test calls with the old one — the executor updates that test in Task 11 (listed).
