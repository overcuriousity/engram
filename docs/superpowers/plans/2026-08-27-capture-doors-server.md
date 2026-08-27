# Capture Doors, Server Side — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** One endpoint that captures whatever it is handed, and the phone doors that stand on it — the Android share sheet, an iOS Shortcut, and a bookmarklet.

**Architecture:** `POST /api/v1/capture` dispatches on `Content-Type` and ends in the ingest calls that already exist; no new ingest logic. The file-sniffing that today lives inside the MCP module moves to `Core` so both doors share it. `POST /ui/share` is a session-authed wrapper over the same dispatch, reached from a `share_target` entry in the web manifest. The install page gains a generated Shortcut and bookmarklet, each carrying a token minted per device by the machinery extension pairing already uses.

**Tech Stack:** Rust 2024, axum 0.8, `Tenant` extractor, askama templates, sqlx/SQLite, `cargo test` (unit tests live in `mod tests` beside the code).

**Spec:** `docs/superpowers/specs/2026-08-27-capture-doors-design.md`

## Global Constraints

- **No new dependency.** Everything here uses crates already in `Cargo.toml`.
- **No migration, no new store table, no model call.** Every task ends in `Core::ingest_capture`, `Core::ingest_url`, `Core::ingest_pdf` or `Core::ingest_image`.
- **`POST /api/v1/corpora`, `/corpora/upload` and `/corpora/image` do not change.** Their tests must still pass untouched; a diff to them is a defect in this plan's execution.
- **A parked near-duplicate is reported, never swallowed.** `IngestOutcome::near_duplicate` reaches every response added here.
- **Status codes follow the doors that exist:** `200` when `duplicate`, `202` when the stored thing is `Extracting` or `Describing`, `201` otherwise.
- **House style.** Doc comments say *why*, in prose, on anything non-obvious — match the surrounding file. Test names are sentences: `fn a_bare_url_in_a_text_body_is_captured_as_a_link()`.
- **Run `cargo fmt` and `cargo clippy --all-targets -- -D warnings` before every commit.**

---

### Task 1: `Core::ingest_file` — the sniffing moves out of MCP

Today `ingest_file` is a free function in `src/mcp/mod.rs:202` that hardcodes `ORIGIN_MCP`. The capture endpoint needs the identical PDF / image / UTF-8 decision under a different origin. Move it to `Core` and give it the origin as a parameter.

**Files:**
- Modify: `src/core/ingest.rs` (add the method; tests in its `mod tests`)
- Modify: `src/mcp/mod.rs:202-242` (delete the free function, call the method)

**Interfaces:**
- Consumes: `Core::ingest_pdf`, `Core::ingest_image`, `Core::ingest_capture`, `Capture::new/.with_title/.with_note/.with_file` — all existing.
- Produces: `Core::ingest_file(&self, bytes: Vec<u8>, filename: Option<String>, title: Option<String>, note: Option<String>, origin: &str) -> Result<IngestOutcome>`. Tasks 3 and 4 call it.

- [ ] **Step 1: Write the failing test**

In `src/core/ingest.rs`, inside `mod tests`:

```rust
#[tokio::test]
async fn a_file_is_read_as_what_its_bytes_say_it_is_under_the_origin_given() {
    let core = crate::core::test_support::test_core().await;

    let text = core
        .ingest_file(b"a procedure".to_vec(), Some("notes.txt".into()), None, None, "cli")
        .await
        .expect("text file");
    let stored = core.store.get_corpus(&text.id).await.expect("stored");
    assert_eq!(stored.origin, "cli", "the caller's origin is what is recorded");

    let png = core
        .ingest_file(crate::web::test_support::a_png(), Some("shot.png".into()), None, None, "share")
        .await
        .expect("image file");
    assert_eq!(png.status, CorpusStatus::Describing, "an image is read by a job");

    let refused = core
        .ingest_file(vec![0xff, 0xfe, 0x00], None, None, None, "cli")
        .await;
    assert!(refused.is_err(), "bytes that are no format we read are refused");
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test --lib core::ingest::tests::a_file_is_read_as_what_its_bytes_say_it_is_under_the_origin_given`
Expected: FAIL — `no method named 'ingest_file' found for struct 'Core'`.

- [ ] **Step 3: Move the function onto `Core`**

Add to the `impl Core` block in `src/core/ingest.rs`:

```rust
    /// Store a file by reading what its bytes say it is: a PDF, an image, or
    /// UTF-8 text. Nothing else — bytes we cannot read are refused rather
    /// than stored as a corpus nobody can search.
    ///
    /// `origin` is the caller's, because the doors that reach this differ in
    /// the one way a person later cares about: `/mcp` is an agent, `cli` is a
    /// shell, `share` is a phone's share sheet.
    pub async fn ingest_file(
        &self,
        bytes: Vec<u8>,
        filename: Option<String>,
        title: Option<String>,
        note: Option<String>,
        origin: &str,
    ) -> Result<IngestOutcome> {
        if bytes.starts_with(b"%PDF-") {
            return self
                .ingest_pdf(PdfCapture { bytes, filename, title_hint: title, note })
                .await;
        }
        if image::guess_format(&bytes).is_ok() {
            return self
                .ingest_image(ImageCapture { bytes, filename, title_hint: title, note })
                .await;
        }
        let size = bytes.len();
        let text = String::from_utf8(bytes).map_err(|_| {
            Error::Validation(
                "that file is neither a PDF, an image nor UTF-8 text — nothing here reads it"
                    .into(),
            )
        })?;
        self.ingest_capture(
            Capture::new(text, origin)
                .with_title(title)
                .with_note(note)
                .with_file(filename.as_deref(), size, "text/plain"),
        )
        .await
    }
```

- [ ] **Step 4: Point MCP at it and delete the old function**

In `src/mcp/mod.rs`, delete `async fn ingest_file` (lines 202-242) and replace its one call site in the `ingest` tool:

```rust
                Ok(bytes) => core.ingest_file(bytes, p.filename, p.title, p.note, ORIGIN_MCP).await,
```

- [ ] **Step 5: Run the tests**

Run: `cargo test --lib core::ingest && cargo test --lib mcp`
Expected: PASS, including the existing MCP ingest tests unchanged.

- [ ] **Step 6: Commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/core/ingest.rs src/mcp/mod.rs
git commit -m "refactor: reading a file by its bytes belongs to the core

Two doors now need the PDF/image/UTF-8 decision, and the second one is
not an agent. The origin comes from the caller instead of being the one
the only previous caller happened to have."
```

---

### Task 2: `POST /api/v1/capture` — the text branch

The endpoint and its dispatch, with only `text/plain` implemented. A body that is one `http(s)` URL and nothing else becomes a link; anything else becomes verbatim text.

**Files:**
- Modify: `src/web/api.rs` (handler, `only_a_url`, `code_for`, route registration at `:1115`)
- Test: `src/web/api.rs` `mod tests`

**Interfaces:**
- Consumes: `Tenant`, `Core::ingest_capture`, `Core::ingest_url`, `ORIGIN_WEB`.
- Produces: `fn only_a_url(body: &str) -> Option<url::Url>`, `fn code_for(out: &IngestOutcome) -> StatusCode`, and the route `/api/v1/capture`. Tasks 3, 4 and 6 extend the same handler.

- [ ] **Step 1: Write the failing tests**

```rust
    #[tokio::test]
    async fn a_bare_url_in_a_text_body_is_captured_as_a_link() {
        let (app, token) = app_and_token().await;
        let res = app
            .oneshot(raw_post("/api/v1/capture", &token, "text/plain", b"https://example.test/a"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED, "a link is fetched in a job");
        let v = json_of(res).await;
        assert!(v["id"].is_string());
    }

    #[tokio::test]
    async fn a_line_that_merely_begins_with_a_url_is_prose() {
        let (app, token, core) = app_token_and_core().await;
        let res = app
            .oneshot(raw_post(
                "/api/v1/capture",
                &token,
                "text/plain",
                b"https://example.test/a is where the procedure lives",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let v = json_of(res).await;
        let stored = core.store.get_corpus(v["id"].as_str().unwrap()).await.unwrap();
        assert!(stored.raw_text.contains("is where"), "stored verbatim, not fetched");
    }

    #[tokio::test]
    async fn a_title_and_note_ride_on_the_query_string() {
        let (app, token, core) = app_token_and_core().await;
        let res = app
            .oneshot(raw_post(
                "/api/v1/capture?title=A%20title&note=why",
                &token,
                "text/plain",
                b"a procedure worth keeping",
            ))
            .await
            .unwrap();
        let v = json_of(res).await;
        let stored = core.store.get_corpus(v["id"].as_str().unwrap()).await.unwrap();
        assert_eq!(stored.title_hint.as_deref(), Some("A title"));
    }

    #[tokio::test]
    async fn a_capture_with_no_content_type_is_refused_rather_than_guessed() {
        let (app, token) = app_and_token().await;
        let res = app
            .oneshot(raw_post("/api/v1/capture", &token, "", b"something"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }
```

And the request helper, beside `post_json` at `src/web/api.rs:1469`:

```rust
    /// A POST with a raw body and a content type of the caller's choosing —
    /// what every client of `/capture` sends, and what `post_json` cannot
    /// express because it fixes the type.
    fn raw_post(uri: &str, token: &str, content_type: &str, body: &[u8]) -> Request<Body> {
        let mut b = Request::builder()
            .uri(uri)
            .method("POST")
            .header("authorization", format!("Bearer {token}"));
        if !content_type.is_empty() {
            b = b.header("content-type", content_type);
        }
        b.body(Body::from(body.to_vec())).unwrap()
    }
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --lib web::api::tests::a_bare_url_in_a_text_body_is_captured_as_a_link`
Expected: FAIL with `404` — the route does not exist.

- [ ] **Step 3: Write the handler**

In `src/web/api.rs`:

```rust
/// What `?title=` and `?note=` carry, for every branch of `/capture`.
#[derive(serde::Deserialize, Default)]
pub struct CaptureQuery {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

/// Whether a body is one link and nothing else.
///
/// The single guess this endpoint makes, and it is made because every share
/// sheet on both platforms hands a shared link over as `text/plain`. Narrow on
/// purpose: one whitespace-separated token, parsing as a URL, over http or
/// https. A line of prose that opens with a link is prose, and a caller who
/// wants the other reading has `POST /corpora`, which asks in as many words.
fn only_a_url(body: &str) -> Option<url::Url> {
    let trimmed = body.trim();
    if trimmed.split_whitespace().count() != 1 {
        return None;
    }
    let u = url::Url::parse(trimmed).ok()?;
    matches!(u.scheme(), "http" | "https").then_some(u)
}

/// The code a stored capture answers with, in the one place the three doors
/// that now need it can share: `200` for something already held, `202` while
/// what was stored is still to be read, `201` for a capture that is complete.
fn code_for(out: &crate::core::ingest::IngestOutcome) -> StatusCode {
    use crate::store::corpora::CorpusStatus;
    if out.duplicate {
        StatusCode::OK
    } else if matches!(out.status, CorpusStatus::Extracting | CorpusStatus::Describing) {
        StatusCode::ACCEPTED
    } else {
        StatusCode::CREATED
    }
}

/// One door for a client that has not classified what it is holding.
///
/// A share sheet hands over a blob and a maybe-URL; a shell hands over a path
/// or a pipe. Written once here, that dispatch is thirty lines; written once
/// per client, it is the reason the clients never get written. Every branch
/// ends in an ingest call the other doors already use.
async fn capture(
    tenant: Tenant,
    Query(q): Query<CaptureQuery>,
    req: axum::extract::Request,
) -> Result<Response> {
    use axum::extract::FromRequest;
    let content_type = req
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    if content_type.starts_with("text/plain") {
        let bytes = axum::body::Bytes::from_request(req, &())
            .await
            .map_err(|e| Error::Validation(format!("body: {e}")))?;
        if bytes.len() > crate::web::MAX_BODY_BYTES {
            return Err(Error::Validation(
                "that text is over the 8 MB limit for a text capture".into(),
            ));
        }
        let text = String::from_utf8(bytes.to_vec())
            .map_err(|_| Error::Validation("that body is not valid UTF-8 text".into()))?;
        let out = match only_a_url(&text) {
            Some(u) => tenant.core.ingest_url(&u, q.title, q.note).await?,
            None => {
                tenant
                    .core
                    .ingest_capture(
                        crate::core::ingest::Capture::new(text, ORIGIN_WEB)
                            .with_title(q.title)
                            .with_note(q.note),
                    )
                    .await?
            }
        };
        return Ok((code_for(&out), Json(out)).into_response());
    }

    Err(Error::Validation(format!(
        "`{content_type}` is not a type this door reads — send text/plain, \
         application/pdf, an image, or multipart/form-data"
    )))
}
```

Register it beside the other capture doors in `api_router` (`src/web/api.rs:1115`), with the ceiling the widest branch needs:

```rust
        .route(
            "/capture",
            post(capture).layer(axum::extract::DefaultBodyLimit::max(pdf_max_bytes.max(image_max_bytes))),
        )
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib web::api::tests::a_bare_url --  --nocapture; cargo test --lib web::api`
Expected: the four new tests PASS, every existing `web::api` test still PASSes.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/web/api.rs
git commit -m "feat: one door for a client that has not classified what it holds

text/plain first: a body that is one link and nothing else is a link,
and everything else is the text it says it is."
```

---

### Task 3: `/capture` reads a PDF and an image from a raw body

**Files:**
- Modify: `src/web/api.rs` (two branches in `capture`)
- Test: `src/web/api.rs` `mod tests`

**Interfaces:**
- Consumes: `Core::ingest_file` (Task 1), `code_for`, `raw_post` (Task 2).
- Produces: nothing new; the branches are internal.

- [ ] **Step 1: Write the failing tests**

```rust
    #[tokio::test]
    async fn a_pdf_body_reaches_the_pdf_path_and_answers_202() {
        let (app, token) = app_and_token().await;
        let res = app
            .oneshot(raw_post("/api/v1/capture", &token, "application/pdf", b"%PDF-1.4 tiny"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED, "stored, extraction still queued");
    }

    #[tokio::test]
    async fn an_image_body_reaches_the_image_path() {
        let (app, token) = app_and_token().await;
        let res = app
            .oneshot(raw_post("/api/v1/capture", &token, "image/png", &a_png()))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn each_branch_is_bounded_by_its_own_ceiling_not_the_widest_one() {
        // The route's limit is the largest of the per-kind ones, because one
        // route now carries what three carried. That must not hand the image
        // branch the PDF's ceiling.
        let core = crate::core::test_support::test_core().await;
        let over = core.capture.image_max_bytes + 1;
        let (app, token, _core) = app_from_core(core).await;
        let res = app
            .oneshot(raw_post("/api/v1/capture", &token, "image/png", &vec![0u8; over]))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --lib web::api::tests::a_pdf_body_reaches_the_pdf_path_and_answers_202`
Expected: FAIL with `400` — the type is not read by the door yet.

- [ ] **Step 3: Add the two branches**

Inside `capture`, before the final `Err`:

```rust
    let kind_limit = if content_type.starts_with("application/pdf") {
        Some(("PDF", tenant.core.capture.pdf_max_bytes))
    } else if content_type.starts_with("image/") {
        Some(("image", tenant.core.capture.image_max_bytes))
    } else {
        None
    };
    if let Some((what, ceiling)) = kind_limit {
        let bytes = axum::body::Bytes::from_request(req, &())
            .await
            .map_err(|e| Error::Validation(format!("body: {e}")))?;
        // The route's ceiling is the widest branch's. Each kind re-imposes its
        // own here, or widening the door for a book would widen it for a photo.
        if bytes.len() > ceiling {
            return Err(Error::Validation(format!(
                "that {what} is over the {} MB limit for a {what} capture",
                ceiling / (1024 * 1024)
            )));
        }
        let out = tenant
            .core
            .ingest_file(bytes.to_vec(), None, q.title, q.note, ORIGIN_WEB)
            .await?;
        return Ok((code_for(&out), Json(out)).into_response());
    }
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib web::api`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/web/api.rs
git commit -m "feat: the capture door takes a pdf and an image as a raw body

Each kind keeps its own ceiling. One route carrying what three carried
must not hand the smaller branch the larger allowance."
```

---

### Task 4: `/capture` takes a multipart body, and many files at once

A share of four photos is four captures, not a concatenation.

**Files:**
- Modify: `src/web/api.rs` (`read_capture_parts`, the multipart branch, `Captured`)
- Test: `src/web/api.rs` `mod tests`

**Interfaces:**
- Consumes: `Core::ingest_file`, `only_a_url`, `code_for`.
- Produces: `async fn read_capture_parts(m: Multipart) -> Result<(HashMap<String, String>, Vec<FilePart>)>` and `enum Captured { One(IngestOutcome), Many(Vec<IngestOutcome>) }`. Task 6 calls both.

- [ ] **Step 1: Write the failing tests**

```rust
    #[tokio::test]
    async fn a_multipart_share_of_three_files_is_three_corpora() {
        let (app, token) = app_and_token().await;
        let res = app
            .oneshot(multipart(
                "/api/v1/capture",
                &token,
                &[],
                &[
                    FilePart { field: "file", filename: "a.txt", mime: Some("text/plain"), body: b"the first procedure" },
                    FilePart { field: "file", filename: "b.txt", mime: Some("text/plain"), body: b"the second procedure" },
                    FilePart { field: "file", filename: "c.png", mime: Some("image/png"), body: &a_png() },
                ],
            ))
            .await
            .unwrap();
        let v = json_of(res).await;
        let arr = v.as_array().expect("many files answer with an array");
        assert_eq!(arr.len(), 3);
        assert!(arr.iter().all(|o| o["id"].is_string()));
    }

    #[tokio::test]
    async fn one_file_in_a_multipart_body_answers_with_one_object() {
        let (app, token) = app_and_token().await;
        let res = app
            .oneshot(multipart(
                "/api/v1/capture",
                &token,
                &[("title", "A title")],
                &[FilePart { field: "file", filename: "a.txt", mime: Some("text/plain"), body: b"a procedure" }],
            ))
            .await
            .unwrap();
        let v = json_of(res).await;
        assert!(v["id"].is_string(), "one file is not wrapped in an array: {v}");
    }

    #[tokio::test]
    async fn a_multipart_share_may_carry_a_url_or_text_instead_of_a_file() {
        let (app, token) = app_and_token().await;
        let res = app
            .oneshot(multipart("/api/v1/capture", &token, &[("url", "https://example.test/a")], &[]))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn a_multipart_body_with_nothing_in_it_is_refused() {
        let (app, token) = app_and_token().await;
        let res = app
            .oneshot(multipart("/api/v1/capture", &token, &[("title", "only a title")], &[]))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --lib web::api::tests::a_multipart_share_of_three_files_is_three_corpora`
Expected: FAIL with `400`.

- [ ] **Step 3: Implement the branch**

```rust
/// Every part of a capture upload: the text fields by name, and every file
/// part in the order it arrived.
///
/// `read_upload` cannot serve this: it takes exactly one file and refuses a
/// second, which is right for a door that stores one document and wrong for a
/// share sheet handing over four photos at once.
async fn read_capture_parts(
    mut multipart: axum::extract::Multipart,
) -> Result<(std::collections::HashMap<String, String>, Vec<FilePart>)> {
    let mut fields = std::collections::HashMap::new();
    let mut files = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| Error::Validation(format!("malformed upload: {e}")))?
    {
        let Some(name) = field.name().map(str::to_string) else {
            continue;
        };
        if field.file_name().is_some() {
            let filename = field.file_name().map(str::to_string);
            let declared = field.content_type().unwrap_or("").to_string();
            let bytes = field
                .bytes()
                .await
                .map_err(|e| Error::Validation(format!("upload failed: {e}")))?;
            files.push(FilePart { filename, declared, bytes });
        } else {
            let text = field
                .text()
                .await
                .map_err(|e| Error::Validation(format!("malformed upload: {e}")))?;
            if !text.trim().is_empty() {
                fields.insert(name, text);
            }
        }
    }
    Ok((fields, files))
}

/// One capture or several, in the shape a client can parse without a flag:
/// an object for a body that held one thing, an array for a share that held
/// more. Untagged, so neither is wrapped in a name the other lacks.
#[derive(serde::Serialize)]
#[serde(untagged)]
enum Captured {
    One(crate::core::ingest::IngestOutcome),
    Many(Vec<crate::core::ingest::IngestOutcome>),
}
```

And in `capture`, before the final `Err`:

```rust
    if content_type.starts_with("multipart/form-data") {
        let m = axum::extract::Multipart::from_request(req, &())
            .await
            .map_err(|e| Error::Validation(format!("malformed upload: {e}")))?;
        let (mut fields, files) = read_capture_parts(m).await?;
        let title = q.title.or_else(|| fields.remove("title"));
        let note = q.note.or_else(|| fields.remove("note"));
        let mut out: Vec<crate::core::ingest::IngestOutcome> = Vec::new();

        // A share sheet sends `url` and `text` for the same share, and the
        // link is the better capture of the two: the text is the page's title
        // repeated. So a `url` wins, and the text is kept only when it stands
        // alone.
        if let Some(raw) = fields.remove("url").or_else(|| {
            fields
                .get("text")
                .and_then(|t| only_a_url(t).map(|u| u.to_string()))
        }) {
            fields.remove("text");
            let u = url::Url::parse(&raw).map_err(|e| Error::Validation(format!("url: {e}")))?;
            if !matches!(u.scheme(), "http" | "https") {
                return Err(Error::Validation(format!(
                    "url: `{}` is not a scheme a page is read over",
                    u.scheme()
                )));
            }
            out.push(tenant.core.ingest_url(&u, title.clone(), note.clone()).await?);
        } else if let Some(text) = fields.remove("text") {
            out.push(
                tenant
                    .core
                    .ingest_capture(
                        crate::core::ingest::Capture::new(text, ORIGIN_WEB)
                            .with_title(title.clone())
                            .with_note(note.clone()),
                    )
                    .await?,
            );
        }

        for f in files {
            out.push(
                tenant
                    .core
                    .ingest_file(f.bytes.to_vec(), f.filename, title.clone(), note.clone(), ORIGIN_WEB)
                    .await?,
            );
        }

        return match out.len() {
            0 => Err(Error::Validation(
                "nothing to capture: send `text`, `url`, or at least one file part".into(),
            )),
            1 => {
                let one = out.pop().expect("length checked");
                Ok((code_for(&one), Json(Captured::One(one))).into_response())
            }
            _ => {
                // The weakest true statement about the set: something here is
                // still being read if anything is, and nothing is new only if
                // every one of them was already held.
                let code = if out.iter().all(|o| o.duplicate) {
                    StatusCode::OK
                } else if out.iter().any(|o| {
                    matches!(
                        o.status,
                        crate::store::corpora::CorpusStatus::Extracting
                            | crate::store::corpora::CorpusStatus::Describing
                    )
                }) {
                    StatusCode::ACCEPTED
                } else {
                    StatusCode::CREATED
                };
                Ok((code, Json(Captured::Many(out))).into_response())
            }
        };
    }
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib web::api`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/web/api.rs
git commit -m "feat: a share of four photos is four captures

Not a concatenation, and not the one-file-per-request rule the upload
door is right to keep. A url and a text in the same share are one
share: the link wins, because the text is the title repeated."
```

---

### Task 5: The two origins and the CLI door

**Files:**
- Modify: `src/core/ingest.rs:5-25` (two constants)
- Modify: `src/store/feedback.rs:40-67` (`as_str`, `captured`, `from_client`)
- Test: `src/store/feedback.rs` `mod tests`

**Interfaces:**
- Produces: `ORIGIN_CLI = "cli"`, `ORIGIN_SHARE = "share"`, `Door::Cli`. The client plan and Task 6 use all three.

- [ ] **Step 1: Write the failing test**

In `src/store/feedback.rs`, inside `mod tests`:

```rust
    #[test]
    fn a_client_may_claim_the_cli_door_and_still_nothing_else() {
        assert_eq!(Door::from_client("cli"), Door::Cli);
        assert_eq!(Door::from_client("extension"), Door::Extension);
        // The gate that matters: a contaminated query still cannot label
        // itself clean, and a real one cannot be made to disappear.
        assert_eq!(Door::from_client("ask"), Door::Api);
        assert_eq!(Door::from_client("judge"), Door::Api);
        assert!(Door::Cli.captured(), "a query typed at a shell is judgeable");
        assert_eq!(Door::Cli.as_str(), "cli");
    }
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test --lib store::feedback::tests::a_client_may_claim_the_cli_door_and_still_nothing_else`
Expected: FAIL — `no variant named 'Cli'`.

- [ ] **Step 3: Add the variant and the origins**

In `src/store/feedback.rs`, add to the enum, then to all three methods:

```rust
    /// A search made from the terminal client. Recorded like `Ui` and `Api`,
    /// and distinguished from them for the reason `Extension` is: a query
    /// typed at a shell is composed before anything came back, about
    /// something the operator is looking at rather than something engram
    /// showed them.
    Cli,
```

`as_str`: `Door::Cli => "cli",`. `captured`: add `| Door::Cli`. `from_client`: add `"cli" => Door::Cli,`.

In `src/core/ingest.rs`, beside the other origin constants:

```rust
/// Text, a file or a link handed over by the terminal client.
pub const ORIGIN_CLI: &str = "cli";
/// A share from a phone's share sheet — the Android share target, the iOS
/// Shortcut and the bookmarklet alike. One value for all three because the
/// distinction is one the operator cannot act on.
pub const ORIGIN_SHARE: &str = "share";
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib store::feedback && cargo test --lib core::ingest`
Expected: PASS. Fix any non-exhaustive `match` the compiler names.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/store/feedback.rs src/core/ingest.rs
git commit -m "feat: the shell and the share sheet are doors with names

One arm on the allowlist that has admitted only the extension until
now; everything else a client names still falls back to the API."
```

---

### Task 6: The Android share target

**Files:**
- Create: `src/web/share.rs`
- Modify: `src/web/mod.rs` (declare the module, merge the router)
- Modify: `assets/manifest.webmanifest`
- Modify: `src/web/api.rs` (make `capture`'s dispatch reachable — see Step 3)
- Test: `src/web/share.rs` `mod tests`

**Interfaces:**
- Consumes: `read_capture_parts`, `Captured`, `only_a_url`, `code_for` from Task 4 — all four become `pub(crate)` in this task.
- Produces: `pub fn share_router() -> Router<AppState>` serving `POST /ui/share`.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use tower::ServiceExt;

    #[tokio::test]
    async fn a_share_without_a_session_is_refused() {
        let (app, _token, _core) = crate::web::api::tests::app_token_and_core().await;
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/ui/share")
                    .method("POST")
                    .header("content-type", "multipart/form-data; boundary=b")
                    .body(axum::body::Body::from("--b--\r\n"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(res.status(), StatusCode::SEE_OTHER, "an unauthenticated share must not store");
    }

    #[tokio::test]
    async fn a_shared_link_lands_on_the_corpus_it_created() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;
        let body = "--b\r\nContent-Disposition: form-data; name=\"url\"\r\n\r\nhttps://example.test/a\r\n--b--\r\n";
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/ui/share")
                    .method("POST")
                    .header("cookie", cookie)
                    .header("content-type", "multipart/form-data; boundary=b")
                    .body(axum::body::Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        let to = res.headers().get("location").unwrap().to_str().unwrap();
        assert!(to.starts_with("/ui/corpora/"), "landed on {to}");
    }

    #[test]
    fn the_manifest_declares_the_share_target() {
        let m = crate::web::assets::Assets::get("manifest.webmanifest").expect("manifest");
        let v: serde_json::Value = serde_json::from_slice(&m.data).expect("valid json");
        assert_eq!(v["share_target"]["action"], "/ui/share");
        assert_eq!(v["share_target"]["method"], "POST");
        assert_eq!(v["share_target"]["enctype"], "multipart/form-data");
    }
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --lib web::share`
Expected: FAIL — the module does not exist.

- [ ] **Step 3: Widen the visibility of Task 4's pieces**

In `src/web/api.rs`, change `fn only_a_url`, `fn code_for`, `async fn read_capture_parts`, `enum Captured` and `struct FilePart` to `pub(crate)`, and make `FilePart`'s fields `pub(crate)`.

- [ ] **Step 4: Write the share door**

`src/web/share.rs`:

```rust
//! The share sheet, which is the phone's own capture gesture.
//!
//! An installed app on Android may put itself in the system share sheet, and
//! a share arrives here as a multipart POST the platform composed — not a form
//! on a page of ours. The parts are the same ones `/api/v1/capture` reads, and
//! they are read by the same code; what differs is only the answer, which is a
//! page for a person rather than JSON for a client.

use crate::core::ingest::ORIGIN_SHARE;
use crate::error::{Error, Result};
use crate::tenants::Tenant;
use crate::web::api::{only_a_url, read_capture_parts};
use crate::web::state::AppState;
use axum::Router;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::post;

/// Store what was shared and land on the corpus it became.
///
/// The corpus page rather than a confirmation that closes itself, because it
/// is the one surface that can say *held for review* when a share is parked as
/// a near-duplicate. On a phone that is the only moment the operator would
/// ever learn that what they shared is stored but not searchable.
async fn share(tenant: Tenant, multipart: axum::extract::Multipart) -> Result<Response> {
    let (mut fields, files) = read_capture_parts(multipart).await?;
    let title = fields.remove("title");

    // A share carries `url` and `text` for the same thing, and the link is the
    // better capture: the text is usually the page's title repeated.
    if let Some(raw) = fields.remove("url").or_else(|| {
        fields
            .get("text")
            .and_then(|t| only_a_url(t).map(|u| u.to_string()))
    }) {
        let u = url::Url::parse(&raw).map_err(|e| Error::Validation(format!("url: {e}")))?;
        if !matches!(u.scheme(), "http" | "https") {
            return Err(Error::Validation(format!(
                "url: `{}` is not a scheme a page is read over",
                u.scheme()
            )));
        }
        let out = tenant.core.ingest_url(&u, title, None).await?;
        return Ok(Redirect::to(&format!("/ui/corpora/{}", out.id)).into_response());
    }

    if let Some(text) = fields.remove("text") {
        let out = tenant
            .core
            .ingest_capture(
                crate::core::ingest::Capture::new(text, ORIGIN_SHARE).with_title(title),
            )
            .await?;
        return Ok(Redirect::to(&format!("/ui/corpora/{}", out.id)).into_response());
    }

    // Several files land on the first of them: a list of four ids is not a
    // destination, and the queue on the capture page shows the rest arriving.
    let mut first = None;
    for f in files {
        let out = tenant
            .core
            .ingest_file(f.bytes.to_vec(), f.filename, title.clone(), None, ORIGIN_SHARE)
            .await?;
        first.get_or_insert(out.id);
    }
    match first {
        Some(id) => Ok(Redirect::to(&format!("/ui/corpora/{id}")).into_response()),
        None => Err(Error::Validation("that share carried nothing to capture".into())),
    }
}

pub fn share_router() -> Router<AppState> {
    Router::new().route("/ui/share", post(share))
}
```

Declare `mod share;` in `src/web/mod.rs` and merge `share::share_router()` where the other routers are merged (`src/web/mod.rs:76` area).

- [ ] **Step 5: Add the manifest entry**

In `assets/manifest.webmanifest`, after `"shortcuts"`:

```json
  "share_target": {
    "action": "/ui/share",
    "method": "POST",
    "enctype": "multipart/form-data",
    "params": {
      "title": "title",
      "text": "text",
      "url": "url",
      "files": [{ "name": "file", "accept": ["image/*", "application/pdf", "text/*"] }]
    }
  }
```

- [ ] **Step 6: Run the tests**

Run: `cargo test --lib web::share && cargo test --lib web::`
Expected: PASS.

- [ ] **Step 7: Verify the cookie on a real device, and record the answer**

The session cookie is `SameSite=Lax` (`src/auth/mod.rs:30`) and a share-target POST is dispatched by the platform, not by a page on this origin. Install the PWA on an Android phone against a real deployment, share a link from any app, and observe.

- **The share lands on a corpus page:** the cookie arrives. Nothing more to build.
- **The share lands on the login page:** the cookie does not arrive. Build the two-hop fallback — `POST /ui/share` stashes the parts under a one-time id and redirects to `GET /ui/share/{id}`, which authenticates normally and completes the capture. No credential in the manifest either way.

Write the observed result into the spec's §5 and commit that edit with the code.

- [ ] **Step 8: Commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/web/share.rs src/web/mod.rs src/web/api.rs assets/manifest.webmanifest docs/superpowers/specs/2026-08-27-capture-doors-design.md
git commit -m "feat: engram is in the phone's share sheet

The parts a share arrives in are the ones /api/v1/capture already
reads. What differs is the answer: the corpus page, because it is the
one surface that can say a share was held for review."
```

---

### Task 7: The Shortcut and the bookmarklet, on the install page

**Files:**
- Modify: `src/web/extension.rs` (mint a device token, hand the template what it needs)
- Modify: `src/web/templates/extension.html`
- Test: `src/web/extension.rs` `mod tests`

**Interfaces:**
- Consumes: `crate::auth::tokens::mint(&control, name, subject, user_agent)` (`src/auth/tokens.rs:33`), `request_origin(&HeaderMap)` (`src/web/pair.rs:57`).
- Produces: nothing other tasks consume.

- [ ] **Step 1: Write the failing tests**

```rust
    #[tokio::test]
    async fn the_install_page_carries_a_working_token_for_the_phone_doors() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;
        let res = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/extension/install")
                    .header("cookie", &cookie)
                    .header("host", "engram.test")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = crate::web::test_support::body_of(res).await;
        assert!(body.contains("/api/v1/capture"), "the doors post to the capture endpoint");

        // The token on the page is a real one: it opens the door it is for.
        let token = body
            .split("engram_")
            .nth(1)
            .map(|rest| format!("engram_{}", rest.split(['"', '\'', '<', ' ']).next().unwrap()))
            .expect("a minted token on the page");
        let res = app
            .oneshot(crate::web::api::tests::raw_post(
                "/api/v1/capture",
                &token,
                "text/plain",
                b"shared from a phone",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn each_visit_mints_its_own_device_token() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = crate::web::test_support::app_with_cookie(core.clone()).await;
        let get = || {
            app.clone().oneshot(
                axum::http::Request::builder()
                    .uri("/extension/install")
                    .header("cookie", &cookie)
                    .header("host", "engram.test")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
        };
        let first = crate::web::test_support::body_of(get().await.unwrap()).await;
        let second = crate::web::test_support::body_of(get().await.unwrap()).await;
        assert_ne!(first, second, "two devices must not share one revocable credential");
    }
```

Make `raw_post` `pub(crate)` in `src/web/api.rs`'s test module so this test can use it.

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --lib web::extension`
Expected: FAIL — the page carries no token.

- [ ] **Step 3: Mint the device token in the handler**

In `src/web/extension.rs`'s `install_page`, before rendering:

```rust
    // A fresh token per visit, named for the device that asked. The Shortcut
    // and the bookmarklet are credentials that live on a phone rather than in
    // a browser's extension storage, and the only way that is tolerable is if
    // each is revocable alone — which means never sharing one between devices,
    // and never asking a person to copy one by hand.
    let (_row, device_token) = crate::auth::tokens::mint(
        &tenant.core.store.control,
        "phone",
        &tenant.user.subject,
        headers
            .get(axum::http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok()),
    )
    .await?;
    let origin = crate::web::pair::request_origin(&headers)
        .unwrap_or_else(|| "https://engram.invalid".into());
```

Add `device_token` and `origin` to the template struct.

- [ ] **Step 4: Add the two doors to the template**

In `src/web/templates/extension.html`, a section beside the extension downloads:

```html
<section class="doors">
  <h2>On a phone</h2>
  <p>
    Android: install engram from the browser's menu and it joins the system
    share sheet. Nothing else to set up.
  </p>
  <p>
    iOS: Safari has no share-target support, so the share sheet is reached
    through Shortcuts. Both of these carry a token minted for this device and
    revocable on its own from Ops.
  </p>
  <a class="button" download="engram-capture.shortcut"
     href="data:application/json;charset=utf-8,{{ shortcut_json|urlencode }}">
    Download the iOS Shortcut
  </a>
  <p class="hint">
    Drag this to the bookmarks bar, or save it as a bookmark on the phone:
  </p>
  <a class="bookmarklet" href="javascript:(function(){fetch('{{ origin }}/api/v1/capture?title='+encodeURIComponent(document.title),{method:'POST',headers:{'content-type':'text/plain','authorization':'Bearer {{ device_token }}'},body:(window.getSelection().toString()||location.href)}).then(function(r){alert(r.ok?'Captured.':'engram refused it: '+r.status)});})()">
    Capture to engram
  </a>
</section>
```

Build `shortcut_json` in the handler as the Shortcuts "Get Contents of URL" action posting the share input to `{origin}/api/v1/capture` with the bearer header, `serde_json::to_string`'d into the template field.

- [ ] **Step 5: Run the tests**

Run: `cargo test --lib web::extension`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/web/extension.rs src/web/templates/extension.html
git commit -m "feat: the phone doors are generated, never hand-assembled

A credential living in a shortcut and a bookmark is the whole
difficulty, so each is minted for the device that asked and revocable
on its own. Nothing is copied by a person."
```

---

### Task 8: The roadmap says what is built

**Files:**
- Modify: `ROADMAP.md`

- [ ] **Step 1: Move these doors into the "what is built" paragraph**

Add to the built list, in the register the file uses: the capture door that reads what it is handed; the phone's share sheet; the Shortcut and bookmarklet minted per device; the shell as a door — and note that bulk doors wait on a bulk-safe near-duplicate policy, naming §8 of the spec.

- [ ] **Step 2: Commit**

```bash
git add ROADMAP.md
git commit -m "docs: the doors are built, and the one that is not says why"
```

---

## Self-Review

**Spec coverage.** §3 → Tasks 2, 3, 4. §4/§4a → the client plan, not this one. §5 → Task 6 (including the device verification the spec demands). §6 → Task 7. §7 → Task 5. §8 → Task 8 (recorded, deliberately unbuilt). §9's server-side items → Tasks 2-7.

**Type consistency.** `Core::ingest_file(bytes, filename, title, note, origin)` is defined in Task 1 and called with that argument order in Tasks 3, 4 and 6. `code_for`, `only_a_url`, `read_capture_parts`, `Captured` and `FilePart` are defined in Tasks 2 and 4 and widened to `pub(crate)` in Task 6 before Task 6 uses them. `Door::Cli`, `ORIGIN_CLI` and `ORIGIN_SHARE` come from Task 5; Task 6 uses `ORIGIN_SHARE`, and `ORIGIN_CLI` is used only by the client plan — so **Task 5 must land before Task 6**.

**Known ordering constraint.** Tasks are sequential: 1 → 2 → 3 → 4 → 5 → 6 → 7 → 8. Only Task 7 could be done out of order, and only after Task 2.
