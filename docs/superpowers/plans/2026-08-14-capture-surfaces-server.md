# Capture Surfaces (Server) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give engram three more ways in — a `.txt` upload, a pasted URL, and an HTML body posted by a browser extension — all converging on one extraction stage and one `ingest`.

**Architecture:** `extract()` exists once, in `src/core/extract.rs`: HTML in, markdown out. The extension supplies HTML it has already rendered and authenticated; `src/core/fetch.rs` supplies HTML for the no-tab-open case. Both hand the same function the same kind of input, so a page captured either way produces the same corpus. Nothing downstream learns HTML exists — `split → synthesize → embed → consolidate` still sees text and a provenance label.

**Tech Stack:** Rust 2024 (rustc 1.94), axum 0.8, tokio, sqlx/SQLite, askama, reqwest (rustls). New: `dom_smoothie` (readability), `html2md` (markdown), `url`. Tests are `#[tokio::test]` with `tower::ServiceExt::oneshot` against `crate::web::router`, and `wiremock` for anything that leaves the process.

**Spec:** `docs/superpowers/specs/2026-08-13-capture-surfaces-design.md`

**Companion plan:** `docs/superpowers/plans/2026-08-14-capture-extension.md` — the extension itself. It depends on this plan (Tasks 5, 7, 8) and nothing here depends on it.

## Global Constraints

- **Extraction below the floor is an error, not a capture.** `capture.min_extracted_chars = 200`. This is the guard against the silent failure the whole feature exists to avoid: a server-side GET that returns a cookie banner must not become a corpus.
- `capture.fetch_timeout_secs = 30`, `capture.fetch_max_bytes = 8388608`. The 8 MB `MAX_BODY_BYTES` in `src/web/mod.rs:23` governs what clients send **us** and says nothing about what we go and fetch; `fetch_max_bytes` is a separate ceiling that happens to share the number.
- **`dom_smoothie::Readability` is `!Send`.** It holds a `dom_query::Document`. It must never be alive across an `.await`, or every async handler that calls it stops being `Send` and axum refuses the route. Confine it to a synchronous function (`html_to_markdown`) whose locals are dropped before it returns. Do not `spawn_blocking` it either — that also requires `Send`.
- **`schema.sql` cannot alter a table.** Adding `source_url` to `corpora` is a one-line addition there; the file's own column check (`migrate`) parses one column per line, so keep that format. An existing database must be recreated — accepted while the project is in testing.
- `origin` is a channel label and `source_url` is a location. Never overload one with the other. Valid origins after this plan: `web`, `mcp`, `extension`, `fetch`, `upload`.
- Every task ends green: `cargo test`, `cargo clippy --all-targets` (no warnings), `cargo fmt --check`.
- Commit at the end of every task. Branch: `feat/capture-surfaces`.

## File Structure

| File | Responsibility |
|---|---|
| `src/core/extract.rs` | **new** — `html_to_markdown`: readability + markdown, and the character floor |
| `src/core/fetch.rs` | **new** — `fetch_html`: scheme check, timeout, byte ceiling, content-type check |
| `src/web/pair.rs` | **new** — `/ui/pair`, the redirect allowlist, `request_origin` |
| `src/web/templates/pair.html` | **new** — the pairing page |
| `src/core/mod.rs` | `pub mod extract; pub mod fetch;`, `capture: CaptureConfig` on `Core` |
| `src/core/ingest.rs` | `Capture` struct, `ingest_capture`, `ingest` becomes a wrapper |
| `src/config.rs` | `CaptureConfig` |
| `config.example.toml` | the `[capture]` block |
| `src/store/schema.sql` | `corpora.source_url` |
| `src/store/corpora.rs` | `source_url` on `Corpus`, on insert, in `row_to_corpus` |
| `src/web/api.rs` | one-of `text`/`html`/`url`, `/corpora/upload`, `?door=` on search |
| `src/web/mod.rs` | mount `pair::pair_router()` |
| `src/store/feedback.rs` | `Door::Extension` |
| `src/web/corpus_view.rs`, `src/web/templates/` | render `source_url`; drop target on the capture page |

Extraction and fetching live under `src/core/` rather than `src/web/`, because they are capture-path work that the MCP surface and any future CLI reach the same way the HTTP API does. Nothing in either file knows about axum.

---

### Task 1: The `[capture]` config block

**Files:**
- Modify: `src/config.rs`
- Modify: `src/core/mod.rs` (field on `Core`, `from_config`, `test_support::build`)
- Modify: `config.example.toml`

**Interfaces:**
- Produces: `crate::config::CaptureConfig { fetch_timeout_secs: u64, fetch_max_bytes: usize, min_extracted_chars: usize }`, `Default`. `Core.capture: CaptureConfig`.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block at the bottom of `src/config.rs`:

```rust
    #[test]
    fn the_capture_defaults_are_the_documented_ones() {
        let c = CaptureConfig::default();
        assert_eq!(c.fetch_timeout_secs, 30);
        assert_eq!(c.fetch_max_bytes, 8 * 1024 * 1024);
        // The floor below which extraction is reported as a failure rather
        // than stored as a corpus.
        assert_eq!(c.min_extracted_chars, 200);
    }

    #[test]
    fn the_example_config_carries_the_capture_block() {
        let cfg = Config::load(Some(std::path::Path::new("config.example.toml"))).unwrap();
        assert_eq!(cfg.capture.min_extracted_chars, 200);
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib config::tests::the_capture_defaults_are_the_documented_ones`
Expected: FAIL — `cannot find type CaptureConfig in this scope`.

- [ ] **Step 3: Add the config type**

In `src/config.rs`, add the field to `Config` (after `pacing`):

```rust
    #[serde(default)]
    pub capture: CaptureConfig,
```

and the type, next to `ConsolidateConfig`:

```rust
/// What the two supplied-from-outside capture paths are allowed to cost.
///
/// The fetch limits are deliberately separate from `MAX_BODY_BYTES`: that one
/// bounds what a client may send us, and says nothing about what we go and
/// retrieve on their behalf.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct CaptureConfig {
    /// Ceiling on a server-side GET. Generous, but it is a network fetch and
    /// not a local model call, so it is not measured in minutes.
    pub fetch_timeout_secs: u64,
    /// Bytes read from a fetched URL before the transfer is abandoned.
    pub fetch_max_bytes: usize,
    /// Characters an extraction must yield to count as a capture. Below this,
    /// the page reduced to navigation and boilerplate: report it, store
    /// nothing. A corpus that silently holds a cookie banner instead of the
    /// document is the failure this whole path is shaped to prevent.
    pub min_extracted_chars: usize,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            fetch_timeout_secs: 30,
            fetch_max_bytes: 8 * 1024 * 1024,
            min_extracted_chars: 200,
        }
    }
}
```

- [ ] **Step 4: Put it on `Core`**

In `src/core/mod.rs`, add to the `Core` struct after `feedback`:

```rust
    /// Limits for the upload, link and extension capture paths. Read on the
    /// request path, so it lives here rather than being threaded down.
    pub capture: crate::config::CaptureConfig,
```

In `from_config`, after `feedback: cfg.feedback.clone(),`:

```rust
            capture: cfg.capture.clone(),
```

In `test_support::build`, after the `feedback:` line:

```rust
            capture: crate::config::CaptureConfig::default(),
```

- [ ] **Step 5: Document it in the example config**

Append to `config.example.toml`:

```toml
[capture]
# The upload, paste-a-link and extension doors.
#
# Seconds a server-side GET may take. This is a network fetch, not a model
# call, so it is not measured in minutes.
fetch_timeout_secs = 30
# Bytes read from a fetched URL before the transfer is abandoned. Separate
# from the 8 MB request-body limit: that one bounds what you send engram,
# this one bounds what engram goes and gets.
fetch_max_bytes = 8388608
# Characters an extraction must yield to count as a capture. A page that
# reduces to less than this was a login wall or a cookie banner, and is
# reported as a failure rather than stored. Fidelity outranks convenience:
# a corpus that quietly holds the subscribe prompt instead of the article is
# worse than no capture at all.
min_extracted_chars = 200
```

- [ ] **Step 6: Run the tests**

Run: `cargo test --lib config::`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/config.rs src/core/mod.rs config.example.toml
git commit -m "feat(capture): add the [capture] config block"
```

---

### Task 2: HTML to markdown

**Files:**
- Create: `src/core/extract.rs`
- Modify: `src/core/mod.rs` (add `pub mod extract;`)
- Modify: `Cargo.toml`

**Interfaces:**
- Consumes: `CaptureConfig::min_extracted_chars` (Task 1) — passed in as an argument, not read from `Core`, so the function stays free of the world.
- Produces: `pub fn html_to_markdown(html: &str, base_url: Option<&url::Url>, min_chars: usize) -> Result<String>`.

The spec writes this as `html_to_markdown(html, base_url)`. The floor is passed explicitly instead of read from config inside, because a pure function of its arguments is what lets the "reduces to boilerplate" test state its own threshold rather than mutating global config.

- [ ] **Step 1: Add the dependencies**

In `Cargo.toml`, under `[dependencies]`:

```toml
# Readability: strips navigation, headers, footers and asides, leaving the
# article. Hand-rolling these heuristics would be worse code than importing an
# implementation of them.
dom_smoothie = "0.18.0"
# HTML → markdown. Pre-1.0, and the fallback if that becomes unacceptable is
# plain-text extraction, which costs the splitter its heading boundaries.
html2md = "0.2.15"
url = "2"
```

- [ ] **Step 2: Write the failing tests**

Create `src/core/extract.rs` containing only this test module and the `use` lines above it:

```rust
use crate::error::{Error, Result};

#[cfg(test)]
mod tests {
    use super::*;

    const ARTICLE: &str = r#"
        <html><head><title>Mounting an image</title></head><body>
          <nav><a href="/">home</a><a href="/about">about</a></nav>
          <article>
            <h1>Mounting an image</h1>
            <p>The loop device is what makes this work at all, and the
               paragraph has to be long enough that readability scores it as
               content rather than as furniture, so here is some more of it.</p>
            <h2>Read-only first</h2>
            <p>Always mount read-only until you have a hash of the source
               image, because a mount that replays a dirty journal writes to
               the evidence you were trying to preserve.</p>
            <p><a href="/notes/hashing">Hashing notes</a></p>
          </article>
          <footer>© nobody</footer>
        </body></html>
    "#;

    #[test]
    fn extraction_keeps_headings_the_splitter_needs() {
        // `src/infer/split.rs` prefers a heading boundary over a blank line and
        // carries the last heading into the next window. Extraction that
        // flattened <h2> would cost every artifact downstream a worse slice.
        let md = html_to_markdown(ARTICLE, None, 10).unwrap();
        assert!(
            md.contains("## Read-only first"),
            "the h2 must survive as a markdown heading, got:\n{md}"
        );
        assert!(crate::infer::split::is_heading_for_test("## Read-only first"));
    }

    #[test]
    fn extraction_drops_the_furniture() {
        let md = html_to_markdown(ARTICLE, None, 10).unwrap();
        assert!(!md.contains("© nobody"), "footer survived:\n{md}");
        assert!(!md.contains("about"), "navigation survived:\n{md}");
    }

    #[test]
    fn a_relative_link_is_resolved_against_the_page_it_came_from() {
        let base = url::Url::parse("https://example.test/notes/mounting").unwrap();
        let md = html_to_markdown(ARTICLE, Some(&base), 10).unwrap();
        assert!(
            md.contains("https://example.test/notes/hashing"),
            "a captured document's references must still point somewhere, got:\n{md}"
        );
    }

    #[test]
    fn a_page_that_reduces_to_boilerplate_is_refused_not_captured() {
        // A login wall. It extracts to almost nothing, and the caller is told
        // so rather than handed a corpus made of the subscribe prompt.
        let wall = "<html><body><div id=\"root\"></div>\
                    <p>Subscribe to read.</p></body></html>";
        let err = html_to_markdown(wall, None, 200).unwrap_err();
        assert!(
            matches!(err, Error::Validation(ref m) if m.contains("extracted")),
            "expected a validation error naming extraction, got {err:?}"
        );
    }

    #[test]
    fn html_that_is_not_a_document_at_all_is_an_error_not_a_panic() {
        let err = html_to_markdown("", None, 200).unwrap_err();
        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
    }
}
```

`is_heading_for_test` does not exist yet — Step 3 adds it. It is there so this test asserts against the splitter's real rule rather than against a copy of it that can drift.

- [ ] **Step 3: Expose the splitter's heading rule**

In `src/infer/split.rs`, directly under `fn is_heading`:

```rust
/// The splitter's own heading rule, for tests elsewhere that must assert
/// against it rather than against a second copy of it that can drift.
#[cfg(test)]
pub fn is_heading_for_test(line: &str) -> bool {
    is_heading(line)
}
```

- [ ] **Step 4: Run the tests to verify they fail**

Run: `cargo test --lib core::extract`
Expected: FAIL — `cannot find function html_to_markdown`.

- [ ] **Step 5: Write the implementation**

Prepend to `src/core/extract.rs`, above the test module:

```rust
use crate::error::{Error, Result};

/// Turn a rendered page into the markdown the segmenter wants.
///
/// Markdown rather than plain text on purpose. `src/infer/split.rs` splits a
/// corpus on headings first and a token budget second, so extraction that
/// flattens `<h2>` into an undistinguished line costs the segmenter its
/// primary boundary and every artifact downstream is drawn from a worse slice.
/// The structure the page already had is structure the splitter can use.
///
/// `base_url` resolves relative links, so a captured document's references
/// still point somewhere a year later.
///
/// Synchronous, and it must stay that way. `Readability` holds a
/// `dom_query::Document`, which is `!Send`; alive across an `.await` it would
/// make the enclosing future `!Send` and axum would refuse the handler. Every
/// non-`Send` value here is created and dropped inside this call.
pub fn html_to_markdown(html: &str, base_url: Option<&url::Url>, min_chars: usize) -> Result<String> {
    let article = {
        let mut readability =
            dom_smoothie::Readability::new(html, base_url.map(url::Url::as_str), None)
                .map_err(|e| Error::Validation(format!("could not read the page: {e}")))?;
        readability
            .parse()
            .map_err(|e| Error::Validation(format!("could not read the page: {e}")))?
    };

    let markdown = html2md::parse_html(&article.content).trim().to_string();

    // The guard the whole path exists for. A server-side GET does not fail
    // loudly when it is served a login wall — it succeeds, and returns the
    // wall. Counting what survived extraction is how that becomes an error
    // instead of a corpus nobody can tell apart from a real one.
    if markdown.chars().count() < min_chars {
        return Err(Error::Validation(format!(
            "only {} characters extracted, below the {min_chars} the capture needs — \
             the page was probably a login wall or an empty shell",
            markdown.chars().count()
        )));
    }
    Ok(markdown)
}
```

- [ ] **Step 6: Wire the module in**

In `src/core/mod.rs`, with the other `pub mod` lines at the top:

```rust
pub mod extract;
```

- [ ] **Step 7: Run the tests**

Run: `cargo test --lib core::extract`
Expected: PASS — all five.

If `extraction_drops_the_furniture` fails because readability kept the nav, the fixture is too short for the scorer, not the code: lengthen the `<p>` bodies. Do not weaken the assertion.

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml Cargo.lock src/core/extract.rs src/core/mod.rs src/infer/split.rs
git commit -m "feat(capture): extract a rendered page to markdown"
```

---

### Task 3: Fetching a URL

**Files:**
- Create: `src/core/fetch.rs`
- Modify: `src/core/mod.rs` (add `pub mod fetch;`)

**Interfaces:**
- Consumes: `CaptureConfig` (Task 1).
- Produces: `pub async fn fetch_html(url: &url::Url, cfg: &crate::config::CaptureConfig) -> Result<String>`.

- [ ] **Step 1: Write the failing tests**

Create `src/core/fetch.rs` with only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn cfg() -> crate::config::CaptureConfig {
        crate::config::CaptureConfig::default()
    }

    #[tokio::test]
    async fn fetch_refuses_a_non_http_scheme() {
        for bad in ["file:///etc/passwd", "ftp://example.test/x", "data:text/html,x"] {
            let u = url::Url::parse(bad).unwrap();
            let err = fetch_html(&u, &cfg()).await.unwrap_err();
            assert!(
                matches!(err, Error::Validation(ref m) if m.contains("scheme")),
                "accepted {bad}: {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn fetch_returns_the_body_of_an_html_page() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/page"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw("<html><body><p>hello</p></body></html>", "text/html"),
            )
            .mount(&server)
            .await;
        let u = url::Url::parse(&format!("{}/page", server.uri())).unwrap();
        let body = fetch_html(&u, &cfg()).await.unwrap();
        assert!(body.contains("hello"));
    }

    #[tokio::test]
    async fn fetch_refuses_a_non_html_content_type_by_name() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/doc.pdf"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(b"%PDF-1.7".to_vec(), "application/pdf"))
            .mount(&server)
            .await;
        let u = url::Url::parse(&format!("{}/doc.pdf", server.uri())).unwrap();
        let err = fetch_html(&u, &cfg()).await.unwrap_err();
        assert!(
            matches!(err, Error::Validation(ref m) if m.contains("application/pdf")),
            "the refused type must be named: {err:?}"
        );
    }

    #[tokio::test]
    async fn fetch_stops_at_the_byte_ceiling() {
        let server = MockServer::start().await;
        let big = "x".repeat(4096);
        Mock::given(method("GET"))
            .and(path("/big"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(big, "text/html"))
            .mount(&server)
            .await;
        let u = url::Url::parse(&format!("{}/big", server.uri())).unwrap();
        let small = crate::config::CaptureConfig {
            fetch_max_bytes: 1024,
            ..crate::config::CaptureConfig::default()
        };
        let err = fetch_html(&u, &small).await.unwrap_err();
        assert!(
            matches!(err, Error::Validation(ref m) if m.contains("1024")),
            "the ceiling must be named: {err:?}"
        );
    }

    #[tokio::test]
    async fn an_upstream_error_status_is_named_not_swallowed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/gone"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let u = url::Url::parse(&format!("{}/gone", server.uri())).unwrap();
        let err = fetch_html(&u, &cfg()).await.unwrap_err();
        assert!(
            matches!(err, Error::Validation(ref m) if m.contains("404")),
            "the status must reach the caller: {err:?}"
        );
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib core::fetch`
Expected: FAIL — `cannot find function fetch_html`.

- [ ] **Step 3: Write the implementation**

Prepend to `src/core/fetch.rs`:

```rust
use crate::config::CaptureConfig;
use crate::error::{Error, Result};

/// Retrieve a page for the paste-a-link door.
///
/// This is an anonymous client: no session, no subscription, no JavaScript
/// engine. It sees what a logged-out stranger sees, which is why it is the
/// *second* supplier into the extractor and not the only one — the extension
/// hands over a page the browser has already rendered and authenticated. What
/// this path can do is bounded by that, and its limits are its own.
///
/// Every failure is named rather than swallowed. The URL is operator input on
/// an authenticated endpoint, so an upstream 404 or a PDF where HTML was
/// expected is a bad request here, not a server fault — `Error::Validation`
/// carries the reason back and renders as 400.
///
/// Out of scope, deliberately: blocking loopback and private-range addresses.
/// The endpoint is authenticated and single-operator, so the only caller who
/// could aim it at the local network is the person who runs the machine.
pub async fn fetch_html(url: &url::Url, cfg: &CaptureConfig) -> Result<String> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Error::Validation(format!(
            "unsupported scheme `{}` — only http and https are fetched",
            url.scheme()
        )));
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(cfg.fetch_timeout_secs))
        // A redirect chain that never ends is a timeout dressed up as
        // progress. Ten is what every other client settles on.
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .map_err(|e| Error::Validation(format!("could not build a client: {e}")))?;

    let res = client
        .get(url.clone())
        .send()
        .await
        .map_err(|e| Error::Validation(format!("fetch failed: {e}")))?;

    if !res.status().is_success() {
        return Err(Error::Validation(format!(
            "fetch failed: the server answered {}",
            res.status()
        )));
    }

    // Checked before reading the body, so a 200 MB video is refused by name
    // rather than fed to the extractor a chunk at a time.
    let content_type = res
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if !matches!(essence.as_str(), "text/html" | "application/xhtml+xml") {
        return Err(Error::Validation(format!(
            "that URL is `{essence}`, not HTML"
        )));
    }

    // Streamed rather than `.text()`, because `Content-Length` is a claim and
    // the ceiling has to hold against a server that lies about it or omits it.
    let mut res = res;
    let mut bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = res
        .chunk()
        .await
        .map_err(|e| Error::Validation(format!("fetch failed mid-transfer: {e}")))?
    {
        if bytes.len() + chunk.len() > cfg.fetch_max_bytes {
            return Err(Error::Validation(format!(
                "that page is larger than the {} byte fetch ceiling",
                cfg.fetch_max_bytes
            )));
        }
        bytes.extend_from_slice(&chunk);
    }

    String::from_utf8(bytes)
        .map_err(|_| Error::Validation("that page is not valid UTF-8".into()))
}
```

- [ ] **Step 4: Wire the module in**

In `src/core/mod.rs`:

```rust
pub mod fetch;
```

- [ ] **Step 5: Run the tests**

Run: `cargo test --lib core::fetch`
Expected: PASS — all five.

- [ ] **Step 6: Commit**

```bash
git add src/core/fetch.rs src/core/mod.rs
git commit -m "feat(capture): fetch a URL for the paste-a-link door"
```

---

### Task 4: `source_url` on a corpus

**Files:**
- Modify: `src/store/schema.sql`
- Modify: `src/store/corpora.rs`
- Modify: `src/core/ingest.rs`
- Modify: `src/web/corpus_view.rs` and the corpus template that renders it

**Interfaces:**
- Produces:
  - `Corpus.source_url: Option<String>`
  - `Store::insert_corpus_with_signature(raw_text, origin, title_hint, shingles, source_url: Option<&str>) -> Result<Insertion>`
  - `crate::core::ingest::Capture { text: String, origin: String, title_hint: Option<String>, source_url: Option<String> }` with `Capture::new(text, origin)`, `.with_title(Option<String>)`, `.with_source_url(Option<String>)`
  - `Core::ingest_capture(&self, c: Capture) -> Result<IngestOutcome>`
  - `Core::ingest(text, origin, title_hint)` unchanged in signature — now a wrapper over `ingest_capture`.

The spec writes `ingest(text, origin, title_hint, source_url)`. A fourth positional argument would mean editing ~20 existing call sites in unrelated test modules to add `None`, spreading this task across files it has no business in. A `Capture` builder carries the same four values, and the old three-argument call keeps meaning what it meant.

- [ ] **Step 1: Write the failing test**

In the `mod tests` block of `src/core/ingest.rs`:

```rust
    #[tokio::test]
    async fn a_capture_remembers_where_it_came_from() {
        let core = crate::core::test_support::test_core().await;
        let out = core
            .ingest_capture(
                Capture::new("alpha para\n\nbeta para", "extension")
                    .with_source_url(Some("https://example.test/notes".into())),
            )
            .await
            .unwrap();
        let src = core.store.get_corpus(&out.id).await.unwrap();
        // The channel and the location are two different facts. Overloading
        // one with the other loses the channel and leaves the URL unqueryable.
        assert_eq!(src.origin, "extension");
        assert_eq!(src.source_url.as_deref(), Some("https://example.test/notes"));
    }

    #[tokio::test]
    async fn an_ordinary_capture_has_no_source_url() {
        let core = crate::core::test_support::test_core().await;
        let out = core.ingest("alpha\n\nbeta", "web", None).await.unwrap();
        let src = core.store.get_corpus(&out.id).await.unwrap();
        assert_eq!(src.source_url, None);
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib core::ingest::tests::a_capture_remembers`
Expected: FAIL — `cannot find struct Capture`.

- [ ] **Step 3: Add the column**

In `src/store/schema.sql`, inside `CREATE TABLE IF NOT EXISTS corpora`, after `near_dupe_score REAL,` — one column per line, which is what `migrate`'s column check parses:

```sql
  -- Where this text was read, when it was read somewhere. `origin` is the
  -- channel it arrived through and this is the location it came from; one
  -- column cannot be both without losing the channel.
  source_url      TEXT,
```

- [ ] **Step 4: Carry it through the store**

In `src/store/corpora.rs`:

Add to `struct Corpus`, after `near_dupe_score`:

```rust
    /// The page this was captured from, for the two doors that know one.
    /// `None` for a paste, an upload or an MCP capture.
    pub source_url: Option<String>,
```

Add to `row_to_corpus`:

```rust
        source_url: r.get("source_url"),
```

Change `insert_corpus_with_signature` to take `source_url: Option<&str>`, set it on the constructed `Corpus`, and extend the statement:

```rust
        let res = sqlx::query(
            "INSERT INTO corpora (id, raw_text, origin, title_hint, content_hash, status, created_at, updated_at, shingles, source_url)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(content_hash) DO NOTHING",
        )
```

with `.bind(&src.source_url)` appended after the shingles bind.

In `insert_corpus`, pass `None`. In `ensure_restored_corpus`, the column simply stays absent from the statement and defaults to NULL — a placeholder rebuilt from vector payloads has no page it came from.

- [ ] **Step 5: Add `Capture` and `ingest_capture`**

In `src/core/ingest.rs`, above `impl Core`:

```rust
/// One capture, whichever door it arrived through.
///
/// A struct rather than four positional arguments: `origin` and `source_url`
/// are two different facts about the same event, and every existing caller
/// that has nothing to say about the second should not have to say `None`.
#[derive(Debug, Clone)]
pub struct Capture {
    pub text: String,
    /// The channel: `web`, `mcp`, `extension`, `fetch`, `upload`.
    pub origin: String,
    pub title_hint: Option<String>,
    /// Where the text was read. Provenance, never an instruction: nothing
    /// downstream ever fetches this.
    pub source_url: Option<String>,
}

impl Capture {
    pub fn new(text: impl Into<String>, origin: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            origin: origin.into(),
            title_hint: None,
            source_url: None,
        }
    }

    pub fn with_title(mut self, title: Option<String>) -> Self {
        self.title_hint = title;
        self
    }

    pub fn with_source_url(mut self, url: Option<String>) -> Self {
        self.source_url = url;
        self
    }
}
```

Rename the body of `ingest` to `ingest_capture(&self, c: Capture)`, replacing `text` with `&c.text`, `origin` with `&c.origin`, `title_hint` with `c.title_hint.as_deref()`, and passing `c.source_url.as_deref()` to `insert_corpus_with_signature`. Then:

```rust
    /// Store the text and queue processing. Deliberately makes no inference
    /// call: capture must stay instant and must survive a dead endpoint.
    pub async fn ingest(
        &self,
        text: &str,
        origin: &str,
        title_hint: Option<&str>,
    ) -> Result<IngestOutcome> {
        self.ingest_capture(
            Capture::new(text, origin).with_title(title_hint.map(str::to_string)),
        )
        .await
    }
```

- [ ] **Step 6: Run the tests**

Run: `cargo test --lib core::ingest`
Expected: PASS. `cargo test` overall must also pass — no existing call site changed.

- [ ] **Step 7: Render it**

In the corpus detail template (`src/web/templates/corpus.html`), under the existing origin line:

```html
{% if let Some(u) = source_url %}
{# The one hop back to where this was read. A link, not text: a URL you
   cannot click is a URL you retype. #}
<p class="muted">From <a href="{{ u }}" rel="noreferrer noopener">{{ u }}</a></p>
{% endif %}
```

Add `source_url: Option<String>` to the template struct in `src/web/ui.rs` (`CorpusTemplate`, or whichever struct backs `corpus.html`) and populate it from `src.source_url.clone()`. Run `cargo test` — askama fails the build if the field is missing, which is the check.

- [ ] **Step 8: Commit**

```bash
git add src/store/schema.sql src/store/corpora.rs src/core/ingest.rs src/web/ui.rs src/web/templates/corpus.html
git commit -m "feat(capture): a corpus remembers the page it was read from"
```

---

### Task 5: One endpoint, three bodies

**Files:**
- Modify: `src/web/api.rs` (`IngestRequest`, `ingest`)

**Interfaces:**
- Consumes: `html_to_markdown` (Task 2), `fetch_html` (Task 3), `Capture`/`ingest_capture` (Task 4), `Core.capture` (Task 1).
- Produces: `POST /api/v1/corpora` accepting exactly one of `text`, `html`, `url`.

| Body | Behaviour | `origin` |
|---|---|---|
| `text` | as today | `web` |
| `html` + optional `url` | extract; `url` is provenance, not an instruction | `extension` |
| `url` alone | fetch, then extract | `fetch` |

- [ ] **Step 1: Write the failing tests**

In `mod tests` in `src/web/api.rs`:

```rust
    #[tokio::test]
    async fn capture_accepts_exactly_one_of_text_html_or_url() {
        let (app, token) = app_and_token().await;

        let long = "<html><body><article><h2>Loop devices</h2><p>".to_string()
            + &"the article body has to clear the extraction floor, so it says \
                rather more than it needs to. "
                .repeat(6)
            + "</p></article></body></html>";

        let res = app
            .clone()
            .oneshot(post_json(
                "/api/v1/corpora",
                &token,
                serde_json::json!({"html": long}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);

        for body in [
            serde_json::json!({}),
            serde_json::json!({"text": "a", "url": "https://example.test/"}),
            serde_json::json!({"text": "a", "html": "<p>b</p>"}),
        ] {
            let res = app
                .clone()
                .oneshot(post_json("/api/v1/corpora", &token, body.clone()))
                .await
                .unwrap();
            assert_eq!(
                res.status(),
                StatusCode::BAD_REQUEST,
                "accepted {body}"
            );
        }
    }

    #[tokio::test]
    async fn an_html_capture_records_its_url_as_provenance_not_as_origin() {
        let (app, token, core) = app_token_and_core().await;
        let html = "<html><body><article><h2>Mounting</h2><p>".to_string()
            + &"read-only until you have a hash of the source image. ".repeat(8)
            + "</p></article></body></html>";
        let res = app
            .oneshot(post_json(
                "/api/v1/corpora",
                &token,
                serde_json::json!({"html": html, "url": "https://example.test/notes"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let id = json_of(res).await["id"].as_str().unwrap().to_string();

        let src = core.store.get_corpus(&id).await.unwrap();
        assert_eq!(src.origin, "extension");
        assert_eq!(src.source_url.as_deref(), Some("https://example.test/notes"));
        // Extraction, not the raw HTML: nothing downstream learns HTML exists.
        assert!(src.raw_text.contains("## Mounting"), "got: {}", src.raw_text);
        assert!(!src.raw_text.contains("<article>"));
    }

    #[tokio::test]
    async fn a_page_that_extracts_to_nothing_is_refused_and_stores_no_corpus() {
        let (app, token, core) = app_token_and_core().await;
        let res = app
            .oneshot(post_json(
                "/api/v1/corpora",
                &token,
                serde_json::json!({"html": "<html><body><p>Subscribe to read.</p></body></html>"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert!(core.store.list_corpora(10, 0).await.unwrap().is_empty());
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib web::api::tests::capture_accepts_exactly_one`
Expected: FAIL — the request deserialises without `text` and 422s, or panics on the missing field.

- [ ] **Step 3: Widen the request**

Replace `IngestRequest` in `src/web/api.rs`:

```rust
/// One of `text`, `html` or `url` — never two.
///
/// Supplying more than one is a validation error rather than a precedence
/// rule, because every precedence rule here would silently discard something
/// the caller meant to capture.
#[derive(serde::Deserialize)]
pub struct IngestRequest {
    #[serde(default)]
    pub text: Option<String>,
    /// HTML the browser has already rendered and authenticated.
    #[serde(default)]
    pub html: Option<String>,
    /// With `html`: where it came from, and the base for relative links.
    /// Alone: the page to fetch.
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
}
```

- [ ] **Step 4: Rewrite the handler**

```rust
/// Capture channels. `origin` is derived from which field arrived, not
/// hardcoded: it is the only record of how a document got here.
const ORIGIN_WEB: &str = "web";
const ORIGIN_EXTENSION: &str = "extension";
const ORIGIN_FETCH: &str = "fetch";

async fn ingest(
    State(st): State<AppState>,
    _id: Identity,
    Json(req): Json<IngestRequest>,
) -> Result<(StatusCode, Json<crate::core::ingest::IngestOutcome>)> {
    let supplied = [
        req.text.is_some(),
        req.html.is_some(),
        // A `url` alongside `html` is provenance, not a second body.
        req.url.is_some() && req.html.is_none(),
    ]
    .iter()
    .filter(|p| **p)
    .count();
    if supplied != 1 {
        return Err(Error::Validation(
            "supply exactly one of `text`, `html` or `url`".into(),
        ));
    }

    let parsed_url = match &req.url {
        Some(raw) => Some(
            url::Url::parse(raw).map_err(|e| Error::Validation(format!("url: {e}")))?,
        ),
        None => None,
    };

    let (text, origin) = if let Some(text) = req.text {
        (text, ORIGIN_WEB)
    } else if let Some(html) = req.html {
        // Synchronous and self-contained: `Readability` is !Send and must not
        // be alive across the awaits above or below it.
        let md = crate::core::extract::html_to_markdown(
            &html,
            parsed_url.as_ref(),
            st.core.capture.min_extracted_chars,
        )?;
        (md, ORIGIN_EXTENSION)
    } else {
        let u = parsed_url.as_ref().expect("one-of check guarantees a url");
        let html = crate::core::fetch::fetch_html(u, &st.core.capture).await?;
        let md = crate::core::extract::html_to_markdown(
            &html,
            parsed_url.as_ref(),
            st.core.capture.min_extracted_chars,
        )?;
        (md, ORIGIN_FETCH)
    };

    let out = st
        .core
        .ingest_capture(
            crate::core::ingest::Capture::new(text, origin)
                .with_title(req.title)
                .with_source_url(parsed_url.map(|u| u.to_string())),
        )
        .await?;
    // 201 for a new capture, 200 when the text was already stored.
    let code = if out.duplicate {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((code, Json(out)))
}
```

- [ ] **Step 5: Run the tests**

Run: `cargo test --lib web::api`
Expected: PASS, including the existing `every_api_route_rejects_an_unauthenticated_request`.

- [ ] **Step 6: Commit**

```bash
git add src/web/api.rs
git commit -m "feat(capture): one endpoint, three bodies"
```

---

### Task 6: Uploading a `.txt`

**Files:**
- Modify: `Cargo.toml` (axum `multipart` feature)
- Modify: `src/web/api.rs` (route + handler)
- Modify: `src/web/templates/capture.html` (drop target)

**Interfaces:**
- Produces: `POST /api/v1/corpora/upload` — multipart, `text/plain` only, UTF-8 only, filename becomes `title_hint`, `origin` is `upload`.

- [ ] **Step 1: Enable the feature**

In `Cargo.toml`:

```toml
axum = { version = "0.8.9", features = ["macros", "multipart"] }
```

- [ ] **Step 2: Write the failing tests**

In `mod tests` in `src/web/api.rs`, add a multipart helper and the tests:

```rust
    /// A minimal multipart body. Hand-rolled rather than pulling a builder in
    /// for four tests.
    fn post_file(uri: &str, token: &str, filename: &str, mime: &str, body: &[u8]) -> Request<Body> {
        const B: &str = "engramtestboundary";
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(
            format!(
                "--{B}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n\
                 Content-Type: {mime}\r\n\r\n"
            )
            .as_bytes(),
        );
        buf.extend_from_slice(body);
        buf.extend_from_slice(format!("\r\n--{B}--\r\n").as_bytes());
        Request::builder()
            .uri(uri)
            .method("POST")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", format!("multipart/form-data; boundary={B}"))
            .body(Body::from(buf))
            .unwrap()
    }

    #[tokio::test]
    async fn an_uploaded_filename_becomes_the_title_hint() {
        let (app, token, core) = app_token_and_core().await;
        let res = app
            .oneshot(post_file(
                "/api/v1/corpora/upload",
                &token,
                "mounting-notes.txt",
                "text/plain",
                b"alpha para\n\nbeta para",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let id = json_of(res).await["id"].as_str().unwrap().to_string();
        let src = core.store.get_corpus(&id).await.unwrap();
        assert_eq!(src.title_hint.as_deref(), Some("mounting-notes.txt"));
        assert_eq!(src.origin, "upload");
        assert_eq!(src.source_url, None);
    }

    #[tokio::test]
    async fn an_upload_that_is_not_utf8_is_refused() {
        let (app, token, core) = app_token_and_core().await;
        let res = app
            .oneshot(post_file(
                "/api/v1/corpora/upload",
                &token,
                "notes.txt",
                "text/plain",
                &[0xff, 0xfe, 0x00, 0x41],
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert!(core.store.list_corpora(10, 0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_upload_of_the_wrong_type_is_refused_with_the_reason() {
        let (app, token) = app_and_token().await;
        let res = app
            .oneshot(post_file(
                "/api/v1/corpora/upload",
                &token,
                "notes.pdf",
                "application/pdf",
                b"%PDF-1.7",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let msg = json_of(res).await["error"].as_str().unwrap_or_default().to_string();
        assert!(msg.contains("application/pdf"), "got {msg}");
    }
```

Adjust the `["error"]` key in the last test to whatever `Error::into_response` actually emits — check `src/error.rs` and match it.

- [ ] **Step 3: Run to verify they fail**

Run: `cargo test --lib web::api::tests::an_uploaded_filename`
Expected: FAIL — 404, no such route.

- [ ] **Step 4: Write the handler**

In `src/web/api.rs`:

```rust
const ORIGIN_UPLOAD: &str = "upload";

/// `.txt` and nothing else, for now. PDF is a `SourceView` implementation and
/// a later plan; refusing everything else by name is what keeps this one from
/// quietly ingesting the bytes of a format it cannot read.
async fn upload(
    State(st): State<AppState>,
    _id: Identity,
    mut multipart: axum::extract::Multipart,
) -> Result<(StatusCode, Json<crate::core::ingest::IngestOutcome>)> {
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| Error::Validation(format!("malformed upload: {e}")))?
    {
        if field.name() != Some("file") {
            continue;
        }
        let filename = field.file_name().map(str::to_string);
        let content_type = field.content_type().unwrap_or("").to_string();
        if !content_type.is_empty() && !content_type.starts_with("text/plain") {
            return Err(Error::Validation(format!(
                "that file is `{content_type}` — only text/plain is accepted"
            )));
        }
        let bytes = field
            .bytes()
            .await
            .map_err(|e| Error::Validation(format!("upload failed: {e}")))?;
        // Refused rather than lossily converted: a corpus is quoted back
        // verbatim, so text that arrived mangled would be a fidelity loss
        // nothing downstream could detect.
        let text = String::from_utf8(bytes.to_vec())
            .map_err(|_| Error::Validation("that file is not valid UTF-8 text".into()))?;

        let out = st
            .core
            .ingest_capture(
                crate::core::ingest::Capture::new(text, ORIGIN_UPLOAD).with_title(filename),
            )
            .await?;
        let code = if out.duplicate {
            StatusCode::OK
        } else {
            StatusCode::CREATED
        };
        return Ok((code, Json(out)));
    }
    Err(Error::Validation("no file in the upload".into()))
}
```

Add the route in `api_router`:

```rust
        .route("/corpora/upload", post(upload))
```

- [ ] **Step 5: Run the tests**

Run: `cargo test --lib web::api`
Expected: PASS. `every_api_route_rejects_an_unauthenticated_request` covers the new route if it enumerates routes — extend its list if it does not.

- [ ] **Step 6: Add the drop target**

In `src/web/templates/capture.html`, inside the form, directly after the `<textarea>`:

```html
  {# A drop target rather than a second page. The file goes to the API
     endpoint, not through this form, because the form posts a urlencoded body
     and this one is multipart. #}
  <label class="row muted" id="drop" style="border:1px dashed var(--line);padding:.6rem;border-radius:.4rem">
    <input type="file" name="file" accept=".txt,text/plain" hidden>
    <span>…or drop a <code>.txt</code> file here.</span>
  </label>
```

and in the page's existing `<script>` block:

```js
    var drop = document.getElementById('drop');
    var picker = drop && drop.querySelector('input[type=file]');
    function send(file) {
      if (!file) return;
      var body = new FormData();
      body.append('file', file);
      fetch('/api/v1/corpora/upload', { method: 'POST', body: body })
        .then(function (r) { return r.json().then(function (j) { return [r.ok, j]; }); })
        .then(function (pair) {
          var result = document.getElementById('capture-result');
          // The server's reason, verbatim. A generic "upload failed" would
          // hide the two things that actually go wrong here: wrong type and
          // wrong encoding.
          result.textContent = pair[0] ? 'Captured.' : (pair[1].error || 'Upload failed.');
          if (pair[0]) htmx.trigger(document.body, 'captured');
        });
    }
    if (drop) {
      picker.addEventListener('change', function () { send(picker.files[0]); });
      drop.addEventListener('dragover', function (e) { e.preventDefault(); });
      drop.addEventListener('drop', function (e) {
        e.preventDefault();
        send(e.dataTransfer.files[0]);
      });
    }
```

The session cookie authenticates this `fetch`; it is same-origin, so no token is involved.

- [ ] **Step 7: Verify by hand**

Run: `cargo run` — drop a `.txt` on `/ui/capture`, confirm it appears under "Recent"; drop a PDF, confirm the refusal names the type.

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml Cargo.lock src/web/api.rs src/web/templates/capture.html
git commit -m "feat(capture): upload a .txt file"
```

---

### Task 7: `Door::Extension`

**Files:**
- Modify: `src/store/feedback.rs`
- Modify: `src/web/api.rs` (`SearchParams`, `search`)

**Interfaces:**
- Produces: `Door::Extension` (`as_str` → `"extension"`, `captured()` → true), `Door::from_client(&str) -> Door`, `?door=extension` on `GET /api/v1/search`.

A selection-search from the extension is the least contaminated query there is — the operator highlights the paragraph they are staring at, having seen nothing engram returned. The judging page and the eval export can only tell those from UI searches if the door records it.

- [ ] **Step 1: Write the failing tests**

In `src/store/feedback.rs` tests:

```rust
    #[test]
    fn only_the_extension_may_name_its_own_door() {
        // The door is how a search is weighted later, so a client that could
        // name any of them could label an `ask` retrieval as a deliberate
        // query and quietly poison the eval set.
        assert!(matches!(Door::from_client("extension"), Door::Extension));
        for other in ["ui", "judge", "ask", "mcp", "", "nonsense"] {
            assert!(
                matches!(Door::from_client(other), Door::Api),
                "client named {other}"
            );
        }
    }

    #[test]
    fn an_extension_search_is_captured_like_a_ui_one() {
        assert!(Door::Extension.captured());
        assert_eq!(Door::Extension.as_str(), "extension");
    }
```

In `src/web/api.rs` tests:

```rust
    #[tokio::test]
    async fn an_extension_search_records_its_own_door() {
        let (app, token, mut core) = app_token_and_core().await;
        core.feedback.enabled = true;
        let (app, token) = rebuild_app_with(core.clone(), token);

        let res = app
            .oneshot(get("/api/v1/search?q=loop+device&door=extension", Some(&token)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        core.background.drain().await;

        let events = core.store.recent_feedback(10).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].door, "extension");
    }
```

`rebuild_app_with` and `recent_feedback` may not exist under those names — read `src/store/feedback.rs` and the feedback tests in `src/core/search.rs` and use whatever the existing capture tests use to read events back and to switch `feedback.enabled` on. Do not add a second way to do either.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib feedback::tests::only_the_extension`
Expected: FAIL — no variant `Extension`.

- [ ] **Step 3: Add the variant**

In `src/store/feedback.rs`, in `enum Door`:

```rust
    /// A search made from the browser extension, usually over a selection on
    /// the page being read. Recorded like `Ui` and `Api`, and distinguished
    /// from them because it is the strongest uncontaminated query there is:
    /// composed before anything came back, about text the operator is looking
    /// at rather than text engram showed them.
    Extension,
```

In `as_str`: `Door::Extension => "extension",`. In `captured`: `matches!(self, Door::Ui | Door::Api | Door::Mcp | Door::Extension)`.

Then, on `impl Door`:

```rust
    /// The door a client is allowed to claim for itself.
    ///
    /// Only `extension`. Everything else falls back to `Api`, because a client
    /// that could name `Ask` or `Judge` could mark a contaminated query as a
    /// clean one, which is the exact thing the judging loop exists to prevent.
    pub fn from_client(raw: &str) -> Door {
        match raw {
            "extension" => Door::Extension,
            _ => Door::Api,
        }
    }
```

- [ ] **Step 4: Accept it on search**

In `src/web/api.rs`, add to `SearchParams`:

```rust
    /// Which client is asking. Only `extension` is honoured; see
    /// `Door::from_client`.
    pub door: Option<String>,
```

and in `search`, replace the hardcoded door:

```rust
    let door = q
        .door
        .as_deref()
        .map(crate::store::feedback::Door::from_client)
        .unwrap_or(crate::store::feedback::Door::Api);
    Ok(Json(st.core.search(&query, door).await?))
```

Note the borrow: `q.door` is read before `q.q` is moved into `SearchQuery`, or reorder so it is.

- [ ] **Step 5: Run the tests**

Run: `cargo test`
Expected: PASS. Any `match` on `Door` elsewhere (the judging page, `src/eval/export.rs`) fails to compile until it handles the new variant — handle it as `Ui` and `Api` are handled there.

- [ ] **Step 6: Commit**

```bash
git add src/store/feedback.rs src/web/api.rs src/web/judge.rs src/eval/export.rs
git commit -m "feat(capture): record searches made from the extension"
```

---

### Task 8: Pairing

**Files:**
- Create: `src/web/pair.rs`
- Create: `src/web/templates/pair.html`
- Modify: `src/web/mod.rs` (`pub mod pair;`, merge `pair::pair_router()`)

**Interfaces:**
- Consumes: `crate::auth::tokens::mint` (`src/auth/tokens.rs:31`), `Identity`.
- Produces: `GET /ui/pair` (authenticated page), `POST /ui/pair` (mints and redirects), `pub fn request_origin(headers: &HeaderMap) -> Option<String>`, `pub fn is_extension_redirect(raw: &str) -> bool`.

Bearer tokens and the UI that mints them already exist. This adds the one thing the extension needs that the Ops page cannot give it: a redirect back into the extension carrying the token, so no credential is ever written into a downloadable file or read aloud from a screen.

- [ ] **Step 1: Write the failing tests**

Create `src/web/pair.rs` with only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{HeaderMap, HeaderValue, Request, StatusCode};
    use tower::ServiceExt;

    #[test]
    fn only_a_browser_extension_redirect_sink_is_accepted() {
        // These two hosts do not serve anything: the browser intercepts them
        // and routes the response back into the extension that started the
        // flow. Anything else is somewhere a token could actually be read.
        assert!(is_extension_redirect("https://abcdefg.chromiumapp.org/"));
        assert!(is_extension_redirect("https://abcdefg.extensions.allizom.org/"));
        for bad in [
            "https://evil.test/",
            "http://abcdefg.chromiumapp.org/",       // not https
            "https://chromiumapp.org.evil.test/",    // suffix in the wrong place
            "https://evil.test/#.chromiumapp.org",
            "javascript:alert(1)",
            "",
        ] {
            assert!(!is_extension_redirect(bad), "accepted {bad}");
        }
    }

    #[test]
    fn the_pairing_page_carries_its_own_origin() {
        // The extension must learn which origin to request host permission
        // for without the operator typing it. The deployment knows it; the
        // static, signed manifest cannot.
        let mut h = HeaderMap::new();
        h.insert("host", HeaderValue::from_static("engram.example"));
        h.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        assert_eq!(request_origin(&h).as_deref(), Some("https://engram.example"));

        let mut plain = HeaderMap::new();
        plain.insert("host", HeaderValue::from_static("localhost:8080"));
        assert_eq!(request_origin(&plain).as_deref(), Some("http://localhost:8080"));

        assert_eq!(request_origin(&HeaderMap::new()), None);
    }

    #[tokio::test]
    async fn pairing_mints_a_working_token_and_hands_it_back_through_the_browser() {
        let (app, _token, core) = crate::web::api::tests::app_token_and_core().await;
        let redirect = "https://abcdefg.chromiumapp.org/";
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui/pair")
                    .method("POST")
                    .header("host", "engram.example")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "redirect_uri={}&state=nonce123",
                        urlencoding_of(redirect)
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        let location = res.headers()["location"].to_str().unwrap().to_string();
        assert!(location.starts_with(redirect), "got {location}");
        // In the fragment, not the query: a query string reaches server logs
        // and the browser's history in a way a fragment does not.
        let fragment = location.split_once('#').unwrap().1;
        let token = fragment
            .split('&')
            .find_map(|kv| kv.strip_prefix("token="))
            .unwrap();
        assert!(fragment.contains("state=nonce123"));
        assert!(fragment.contains("origin=http%3A%2F%2Fengram.example"));

        let id = crate::auth::tokens::verify(&core.store, &percent_decode(token))
            .await
            .unwrap();
        assert_eq!(id.subject, "user-1");
    }

    #[tokio::test]
    async fn pairing_refuses_a_redirect_that_is_not_an_extension() {
        let (app, _token, _core) = crate::web::api::tests::app_token_and_core().await;
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui/pair")
                    .method("POST")
                    .header("host", "engram.example")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from("redirect_uri=https%3A%2F%2Fevil.test%2F&state=x"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    fn urlencoding_of(s: &str) -> String {
        s.replace(':', "%3A").replace('/', "%2F")
    }

    fn percent_decode(s: &str) -> String {
        s.replace("%2F", "/").replace("%2B", "+").replace("%3D", "=")
    }
}
```

`app_token_and_core` is `pub` inside `src/web/api.rs`'s test module already; if `mod tests` there is not reachable from here, make it `pub(crate) mod tests` under `#[cfg(test)]`.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib web::pair`
Expected: FAIL — `cannot find function is_extension_redirect`.

- [ ] **Step 3: Write the module**

Prepend to `src/web/pair.rs`:

```rust
use crate::auth::Identity;
use crate::error::{Error, Result};
use crate::web::state::AppState;
use askama::Template;
use axum::Form;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

/// Hosts a browser reserves for extension redirect sinks.
///
/// `browser.identity.launchWebAuthFlow` hands the response back to the
/// extension that started the flow instead of loading these; nothing is served
/// from them. That is what makes a redirect to one safe and a redirect
/// anywhere else an open redirect carrying a credential.
const EXTENSION_REDIRECT_HOSTS: [&str; 2] = ["chromiumapp.org", "extensions.allizom.org"];

/// Whether a redirect target is one of those sinks.
///
/// Matched on the parsed host, never on the raw string: `https://evil.test/#.chromiumapp.org`
/// ends with the right characters and is not the right host.
pub fn is_extension_redirect(raw: &str) -> bool {
    let Ok(u) = url::Url::parse(raw) else {
        return false;
    };
    if u.scheme() != "https" {
        return false;
    }
    let Some(host) = u.host_str() else {
        return false;
    };
    EXTENSION_REDIRECT_HOSTS
        .iter()
        .any(|h| host == *h || host.ends_with(&format!(".{h}")))
}

/// The origin this deployment is being reached at.
///
/// Learned from the request rather than configured, because the signed
/// extension is one artifact serving every deployment: an XPI is signed over
/// its contents, so a manifest rewritten per host would invalidate the
/// signature that makes one-click install work. The download and pairing pages
/// carry the origin instead, and the extension asks for host permission for
/// that one origin.
pub fn request_origin(headers: &HeaderMap) -> Option<String> {
    let host = headers.get(header::HOST)?.to_str().ok()?;
    if host.is_empty() {
        return None;
    }
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("http");
    Some(format!("{scheme}://{host}"))
}

#[derive(Template)]
#[template(path = "pair.html")]
struct PairTemplate {
    theme: String,
    origin: String,
    redirect_uri: String,
    state: String,
}

#[derive(serde::Deserialize)]
pub struct PairParams {
    #[serde(default)]
    pub redirect_uri: String,
    #[serde(default)]
    pub state: String,
}

/// The page the extension opens through `launchWebAuthFlow`.
///
/// A button rather than an automatic mint. The operator sees which origin they
/// are pairing with and presses something; a flow that minted on page load
/// would hand a token to anything that could get this URL opened.
async fn pair_page(
    State(_st): State<AppState>,
    _id: Identity,
    headers: HeaderMap,
    axum::extract::Query(p): axum::extract::Query<PairParams>,
) -> Result<Response> {
    if !is_extension_redirect(&p.redirect_uri) {
        return Err(Error::Validation(
            "that redirect does not belong to a browser extension".into(),
        ));
    }
    Ok(crate::web::ui::HtmlTemplate(PairTemplate {
        theme: "light".into(),
        origin: request_origin(&headers).unwrap_or_default(),
        redirect_uri: p.redirect_uri,
        state: p.state,
    })
    .into_response())
}

async fn pair_submit(
    State(st): State<AppState>,
    id: Identity,
    headers: HeaderMap,
    Form(p): Form<PairParams>,
) -> Result<Response> {
    if !is_extension_redirect(&p.redirect_uri) {
        return Err(Error::Validation(
            "that redirect does not belong to a browser extension".into(),
        ));
    }
    let origin = request_origin(&headers).unwrap_or_default();
    let (_, plaintext) =
        crate::auth::tokens::mint(&st.core.store, "browser extension", &id.subject).await?;

    // The fragment, not the query: a fragment is never sent to a server and
    // does not land in a proxy log or in browsing history the way a query
    // string does. `launchWebAuthFlow` hands the whole URL to the extension,
    // fragment included.
    let location = format!(
        "{}#token={}&state={}&origin={}",
        p.redirect_uri,
        urlencode(&plaintext),
        urlencode(&p.state),
        urlencode(&origin),
    );
    tracing::info!(subject = %id.subject, "extension paired");
    Ok((StatusCode::SEE_OTHER, [(header::LOCATION, location)]).into_response())
}

/// Percent-encode everything outside the unreserved set. Small and local
/// rather than a dependency: three values, all of them ASCII.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn pair_router() -> Router<AppState> {
    Router::new().route("/ui/pair", get(pair_page).post(pair_submit))
}
```

`HtmlTemplate` lives in `src/web/ui.rs`; make it `pub(crate)` if it is private.

- [ ] **Step 4: Write the template**

Create `src/web/templates/pair.html`:

```html
{% extends "layout.html" %}
{% block title %}Pair the extension — engram{% endblock %}
{% block content %}
<h2>Pair the extension</h2>
{# The origin is stated because it is the one thing the operator can check:
   the extension will ask the browser for permission to reach exactly this
   host, and nothing else. #}
<p>This gives the browser extension a token for <strong>{{ origin }}</strong>.</p>
<form method="post" action="/ui/pair">
  <input type="hidden" name="redirect_uri" value="{{ redirect_uri }}">
  <input type="hidden" name="state" value="{{ state }}">
  <button class="btn btn-accent" type="submit">Pair</button>
</form>
<p class="muted">You can revoke it any time under Housekeeping → API tokens.</p>
{% endblock %}
```

- [ ] **Step 5: Mount it**

In `src/web/mod.rs`: `pub mod pair;` with the other modules, and `.merge(pair::pair_router())` after `.merge(ui::ui_router())`.

- [ ] **Step 6: Run the tests**

Run: `cargo test --lib web::pair`
Expected: PASS — all five.

- [ ] **Step 7: Commit**

```bash
git add src/web/pair.rs src/web/templates/pair.html src/web/mod.rs src/web/ui.rs
git commit -m "feat(capture): pair the extension without writing a token to a file"
```

---

## Done when

- `cargo test` and `cargo clippy --all-targets` are clean.
- `POST /api/v1/corpora` accepts one of `text`, `html`, `url` and refuses two.
- A `.txt` dropped on `/ui/capture` becomes a corpus whose `title_hint` is the filename.
- A page that extracts to boilerplate is refused and stores nothing.
- A corpus captured from a URL renders a link back to it.
- `/ui/pair` mints a token and redirects into an extension sink, and refuses any other redirect.

The extension that consumes Tasks 5, 7 and 8 is `docs/superpowers/plans/2026-08-14-capture-extension.md`.
