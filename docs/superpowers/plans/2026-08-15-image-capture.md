# Image Capture Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Photos and images become first-class corpora: uploaded instantly, stored verbatim, read into markdown by a vision model in a background job, then processed by the existing text pipeline untouched.

**Architecture:** A new `attachments` table holds the original bytes and a derived preview; a generic `corpora.metadata` JSON column holds file facts, EXIF and the user's `note`. `POST /api/v1/corpora/image` validates, decodes, extracts EXIF, builds the preview, inserts a corpus in the new `describing` status and enqueues the new `Describe` job stage. That stage calls the new `Describer` trait (optional `[infer.vision]` role, inheriting the synthesize endpoint when its own is absent), writes the markdown into `raw_text`, runs the existing near-duplicate check, and hands off to `Synthesize`. The capture page's drop zone accepts images from picker/camera/drag/paste plus an optional context note; the corpus page renders the photo and metadata with the transcription labelled as derived.

**Tech Stack:** Rust 2024 / axum 0.8 / sqlx 0.9 SQLite / askama / htmx; new crates `image` (jpeg, png, webp features only) and `kamadak-exif`. OpenAI-compatible chat/completions with content-array (`image_url` data URI) messages.

**Spec:** `docs/superpowers/specs/2026-08-15-image-capture-design.md`

## Global Constraints

- The capture request path makes **no inference call**. Only `Stage::Describe` calls the vision model.
- Original image bytes are stored untouched; only the preview is re-encoded.
- `corpora.raw_text` stays `NOT NULL`; it is `''` while `describing`.
- New columns go through `ADDED_COLUMNS` in `src/store/mod.rs` (append-only, with default). New tables go in `src/store/schema.sql` as `CREATE TABLE IF NOT EXISTS`.
- Every background inference call goes through `core.gate.background()` and reports `succeeded()`/`failed(&e)`.
- Supported image inputs: JPEG, PNG, WebP — sniffed from bytes, never trusted from the declared mime.
- Config: `[infer.vision]` absent → image capture disabled; `model` required when present; `base_url`/`api_key` inherit from `[infer.synthesize]` when absent; `timeout_secs` default 120.
- Config: `capture.image_max_bytes` default `25 * 1024 * 1024`; `capture.image_preview_edge` default `2048`.
- Origin value for image corpora: `image`. New corpus status: `describing`. New job stage string: `describe`.
- Metadata JSON namespaces: `note` (string), `file` (`name,size,mime,width,height`), `exif` (`taken_at,camera,orientation,gps{lat,lon,alt},tags{...}`), `describe` (`error`).
- Every new inference-free store/API function has a test; every refusal is a distinct, tested error message.
- Run `cargo test` (all green) and `cargo clippy --all-targets -- -D warnings` before every commit. Commit message trailer: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.

## File map

| File | Responsibility |
|---|---|
| `Cargo.toml` | add `image`, `kamadak-exif` |
| `src/config.rs` | `VisionRole`, `CaptureConfig.image_*`, redaction |
| `config.example.toml` | documented `[infer.vision]` + capture keys |
| `src/store/schema.sql` | `attachments` table |
| `src/store/mod.rs` | `ADDED_COLUMNS` gets `corpora.metadata`; `pub mod attachments` |
| `src/store/attachments.rs` (new) | `Attachment`, insert/get |
| `src/store/corpora.rs` | `CorpusStatus::Describing`, `Corpus.metadata`, `insert_image_corpus`, `set_described_text`, `set_corpus_metadata`, metadata on `insert_corpus_with_signature` |
| `src/store/jobs.rs` | `Stage::Describe` |
| `src/core/image.rs` (new) | `prepare()`: sniff, decode, EXIF→JSON, orientation, preview |
| `src/infer/mod.rs` | `Describer` trait |
| `src/infer/openai.rs` | `HttpDescriber` |
| `src/infer/fake.rs` | `FakeDescriber` |
| `src/infer/prompt.rs` | `DESCRIBE_SYSTEM`, `describe_context()` |
| `src/core/mod.rs` | `Core.describer`, wiring, test_support |
| `src/core/ingest.rs` | `Capture.metadata`, `ImageCapture`, `Core::ingest_image` |
| `src/jobs/describe.rs` (new) + `src/jobs/mod.rs` | the `Describe` stage |
| `src/web/api.rs` | `upload_image`, `get_image`, `note` on `upload`, per-route body limit |
| `src/web/mod.rs` | pass `image_max_bytes` into `api_router` |
| `src/web/ui.rs`, `templates/capture.html`, `templates/corpus.html`, `src/web/corpus_view.rs` | UI |
| `src/main.rs` | vision probe |

---

### Task 1: Config — the vision role and image capture limits

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/config.rs` (CaptureConfig ~line 27, InferConfig ~line 286, `redacted` ~line 634, tests ~line 660+)
- Modify: `config.example.toml` (after `[infer.ask]` block ~line 113; `[capture]` block ~line 261)

**Interfaces:**
- Produces:
  ```rust
  pub struct VisionRole { pub model: String, pub base_url: Option<String>, pub api_key: Option<String>, pub timeout_secs: u64 }
  impl VisionRole { pub fn resolve(&self, synth: &SynthesizeRole) -> (String, Option<String>) } // (base_url, api_key)
  pub struct InferConfig { ..., pub vision: Option<VisionRole> }
  pub struct CaptureConfig { ..., pub image_max_bytes: usize, pub image_preview_edge: u32 }
  ```

- [ ] **Step 1: Add the crates**

Run: `cargo add image --no-default-features --features jpeg,png,webp && cargo add kamadak-exif`
Expected: `Cargo.toml` gains both lines (image 0.25.x, kamadak-exif 0.6.x). Add a comment above them:
```toml
# Decoding, orientation and the downscaled preview of a captured image. Only
# the three formats a phone or a browser actually hands over.
image = { version = "0.25", default-features = false, features = ["jpeg", "png", "webp"] }
# EXIF is where the phone says when and where a photo was taken.
kamadak-exif = "0.6"
```

- [ ] **Step 2: Write the failing config tests**

Append to the `tests` module in `src/config.rs`:

```rust
    #[test]
    fn vision_is_off_unless_configured() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, MINIMAL);
        let cfg = Config::load(Some(&p)).unwrap();
        assert!(cfg.infer.vision.is_none(), "vision must default to disabled");
        assert_eq!(cfg.capture.image_max_bytes, 25 * 1024 * 1024);
        assert_eq!(cfg.capture.image_preview_edge, 2048);
    }

    #[test]
    fn a_vision_role_without_its_own_endpoint_inherits_synthesize() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, &format!("{MINIMAL}\n[infer.vision]\nmodel = \"qwen-vl\"\n"));
        let cfg = Config::load(Some(&p)).unwrap();
        let v = cfg.infer.vision.as_ref().expect("configured");
        assert_eq!(v.model, "qwen-vl");
        assert_eq!(v.timeout_secs, 120);
        let (url, key) = v.resolve(&cfg.infer.synthesize);
        assert_eq!(url, cfg.infer.synthesize.base_url);
        assert_eq!(key, cfg.infer.synthesize.api_key);
    }

    #[test]
    fn a_dedicated_vision_endpoint_wins_over_synthesize() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            &format!(
                "{MINIMAL}\n[infer.vision]\nmodel = \"qwen-vl\"\nbase_url = \"http://vision:9000/v1\"\napi_key = \"vk\"\n"
            ),
        );
        let cfg = Config::load(Some(&p)).unwrap();
        let (url, key) = cfg.infer.vision.as_ref().unwrap().resolve(&cfg.infer.synthesize);
        assert_eq!(url, "http://vision:9000/v1");
        assert_eq!(key.as_deref(), Some("vk"));
        assert!(!cfg.redacted().contains("\"vk\""), "vision key leaked");
    }

    #[test]
    fn the_example_config_documents_the_vision_role() {
        let text = std::fs::read_to_string("config.example.toml").unwrap();
        assert!(text.contains("[infer.vision]"), "example config must show the vision block");
        assert!(text.contains("image_max_bytes"));
    }
```

- [ ] **Step 3: Run to verify they fail**

Run: `cargo test config::tests::vision -- --nocapture 2>&1 | tail -20`
Expected: compile error — `vision`, `image_max_bytes` unknown.

- [ ] **Step 4: Implement**

In `CaptureConfig` add fields and defaults:
```rust
    /// Bytes an uploaded image may weigh. A phone photo is 3–8 MB; this is the
    /// per-route ceiling for the image door only, the global body limit stays.
    pub image_max_bytes: usize,
    /// Longest edge, in pixels, of the preview the vision model is shown and
    /// the UI displays. The original is stored untouched regardless.
    pub image_preview_edge: u32,
```
and in `Default`: `image_max_bytes: 25 * 1024 * 1024, image_preview_edge: 2048,`.

After `RerankRole` add:
```rust
/// The vision model that reads a captured image into text. Optional: absent
/// means the image door is closed. `base_url` and `api_key` are optional
/// because the common case is the synthesize endpoint serving a multimodal
/// model too — then only `model` needs saying.
#[derive(Debug, Deserialize, Clone)]
pub struct VisionRole {
    pub model: String,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default = "default_vision_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_vision_timeout_secs() -> u64 {
    120
}

impl VisionRole {
    /// The endpoint and key this role actually calls: its own where given,
    /// the synthesize role's otherwise.
    pub fn resolve(&self, synth: &SynthesizeRole) -> (String, Option<String>) {
        (
            self.base_url.clone().unwrap_or_else(|| synth.base_url.clone()),
            self.api_key.clone().or_else(|| synth.api_key.clone()),
        )
    }
}
```
Add `#[serde(default)] pub vision: Option<VisionRole>,` to `InferConfig`. In `redacted()` add:
```rust
        if let Some(v) = c.infer.vision.as_mut() {
            v.api_key = v.api_key.as_ref().map(|_| R.into());
        }
```

In `config.example.toml`, after the rerank block:
```toml
# Reading captured images. Optional and disabled by default: with this block
# absent the image door is closed and the capture page offers text only.
# `model` is required. `base_url` and `api_key` default to the synthesize
# role's, which is the common case — one local server hosting a multimodal
# model. Set them when a separate vision endpoint serves the images.
# [infer.vision]
# model = "qwen2.5-vl-7b"
# base_url = "http://localhost:8002/v1"
# api_key = ""
# timeout_secs = 120
```
Wait — the test asserts the literal `[infer.vision]` is present; a commented line contains it, so this passes while keeping the block off by default. In the `[capture]` block append:
```toml
# Bytes an uploaded image may weigh. The image door has its own ceiling
# because a phone photo is several times the 8 MB the rest of the API allows.
image_max_bytes = 26214400
# Longest edge of the preview shown to the vision model and in the UI. The
# original is stored untouched either way.
image_preview_edge = 2048
```

- [ ] **Step 5: Run and commit**

Run: `cargo test config:: 2>&1 | tail -5 && cargo clippy --all-targets -- -D warnings 2>&1 | tail -3`
Expected: all pass, no warnings.
```bash
git add Cargo.toml Cargo.lock src/config.rs config.example.toml
git commit -m "feat(config): optional vision role and image capture limits"
```

---

### Task 2: Store — status, stage, metadata column, attachments table

**Files:**
- Modify: `src/store/schema.sql` (after the `corpora` table)
- Modify: `src/store/mod.rs` (`ADDED_COLUMNS`, `pub mod attachments`)
- Create: `src/store/attachments.rs`
- Modify: `src/store/corpora.rs`
- Modify: `src/store/jobs.rs` (`Stage`)
- Modify: `src/web/ui.rs:221-232` (`status_badge`), `:722` (`in_flight`)

**Interfaces:**
- Produces:
  ```rust
  // corpora.rs
  pub enum CorpusStatus { Describing, Raw, ... }            // "describing"
  pub struct Corpus { ..., pub metadata: serde_json::Value } // '{}' when nothing was recorded
  Store::insert_corpus_with_signature(raw_text, origin, title_hint, shingles, source_url, metadata: &serde_json::Value) -> Result<Insertion>
  Store::insert_image_corpus(content_hash: &str, origin: &str, title_hint: Option<&str>, metadata: &serde_json::Value) -> Result<Insertion>
  Store::set_described_text(id: &str, text: &str, shingles: Vec<u64>) -> Result<()>
  Store::set_corpus_metadata(id: &str, metadata: &serde_json::Value) -> Result<()>
  // attachments.rs
  pub struct Attachment { pub id: i64, pub corpus_id: String, pub kind: String, pub mime: String, pub filename: Option<String>, pub bytes: Vec<u8>, pub preview: Vec<u8>, pub width: Option<i64>, pub height: Option<i64>, pub created_at: i64 }
  pub struct NewAttachment<'a> { pub corpus_id: &'a str, pub kind: &'a str, pub mime: &'a str, pub filename: Option<&'a str>, pub bytes: &'a [u8], pub preview: &'a [u8], pub width: Option<i64>, pub height: Option<i64> }
  Store::insert_attachment(&NewAttachment) -> Result<i64>
  Store::attachment_for_corpus(corpus_id: &str) -> Result<Option<Attachment>>
  Store::attachment_preview(corpus_id: &str) -> Result<Option<(String /*mime*/, Vec<u8>)>>   // always image/jpeg
  Store::attachment_original(corpus_id: &str) -> Result<Option<(String, Vec<u8>)>>
  // jobs.rs
  Stage::Describe  // "describe"
  ```

- [ ] **Step 1: Failing tests**

Append to `src/store/corpora.rs` tests:
```rust
    #[tokio::test]
    async fn an_image_corpus_starts_describing_with_no_text_and_its_metadata() {
        let s = Store::memory().await.unwrap();
        let meta = serde_json::json!({"file": {"name": "a.jpg"}, "note": "whiteboard"});
        let ins = s
            .insert_image_corpus("hash-1", "image", Some("a.jpg"), &meta)
            .await
            .unwrap();
        let src = ins.into_corpus();
        assert_eq!(src.status, CorpusStatus::Describing);
        assert_eq!(src.raw_text, "");
        assert_eq!(src.content_hash, "hash-1");
        let back = s.get_corpus(&src.id).await.unwrap();
        assert_eq!(back.metadata["note"], "whiteboard");
        assert_eq!(back.status, CorpusStatus::Describing);

        // The same photo again is the same row.
        assert!(matches!(
            s.insert_image_corpus("hash-1", "image", None, &meta).await.unwrap(),
            Insertion::Existing(e) if e.id == src.id
        ));
    }

    #[tokio::test]
    async fn describing_writes_the_text_and_signature_but_keeps_the_hash() {
        let s = Store::memory().await.unwrap();
        let src = s
            .insert_image_corpus("hash-2", "image", None, &serde_json::json!({}))
            .await
            .unwrap()
            .into_corpus();
        let sig = crate::store::shingle::signature("hello world");
        s.set_described_text(&src.id, "hello world", sig.clone()).await.unwrap();
        let back = s.get_corpus(&src.id).await.unwrap();
        assert_eq!(back.raw_text, "hello world");
        assert_eq!(back.shingles, sig);
        assert_eq!(back.content_hash, "hash-2");
        // Status is the caller's decision, not this write's.
        assert_eq!(back.status, CorpusStatus::Describing);
    }

    #[tokio::test]
    async fn metadata_defaults_to_an_empty_object_and_can_be_replaced() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("plain", "web", None).await.unwrap();
        assert_eq!(src.metadata, serde_json::json!({}));
        s.set_corpus_metadata(&src.id, &serde_json::json!({"note": "n"}))
            .await
            .unwrap();
        assert_eq!(s.get_corpus(&src.id).await.unwrap().metadata["note"], "n");
    }
```

Create `src/store/attachments.rs` with tests at the bottom:
```rust
//! The bytes a corpus was captured from, when it was not text.
//!
//! One row per image corpus today. The original is kept exactly as uploaded —
//! that is the verbatim source, the way `raw_text` is for a paste — and the
//! preview is derived once, for the model and the screen.

use super::{Store, now};
use crate::error::Result;
use sqlx::Row;

#[derive(Debug, Clone)]
pub struct Attachment {
    pub id: i64,
    pub corpus_id: String,
    pub kind: String,
    pub mime: String,
    pub filename: Option<String>,
    pub bytes: Vec<u8>,
    pub preview: Vec<u8>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub created_at: i64,
}

pub struct NewAttachment<'a> {
    pub corpus_id: &'a str,
    pub kind: &'a str,
    pub mime: &'a str,
    pub filename: Option<&'a str>,
    pub bytes: &'a [u8],
    pub preview: &'a [u8],
    pub width: Option<i64>,
    pub height: Option<i64>,
}

/// What every preview is encoded as. See `core::image::prepare`.
pub const PREVIEW_MIME: &str = "image/jpeg";

impl Store {
    pub async fn insert_attachment(&self, a: &NewAttachment<'_>) -> Result<i64> {
        let res = sqlx::query(
            "INSERT INTO attachments (corpus_id, kind, mime, filename, bytes, preview, width, height, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(a.corpus_id)
        .bind(a.kind)
        .bind(a.mime)
        .bind(a.filename)
        .bind(a.bytes)
        .bind(a.preview)
        .bind(a.width)
        .bind(a.height)
        .bind(now())
        .execute(&self.pool)
        .await?;
        Ok(res.last_insert_rowid())
    }

    pub async fn attachment_for_corpus(&self, corpus_id: &str) -> Result<Option<Attachment>> {
        let row = sqlx::query(
            "SELECT id, corpus_id, kind, mime, filename, bytes, preview, width, height, created_at
             FROM attachments WHERE corpus_id = ? ORDER BY id LIMIT 1",
        )
        .bind(corpus_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| Attachment {
            id: r.get("id"),
            corpus_id: r.get("corpus_id"),
            kind: r.get("kind"),
            mime: r.get("mime"),
            filename: r.get("filename"),
            bytes: r.get("bytes"),
            preview: r.get("preview"),
            width: r.get("width"),
            height: r.get("height"),
            created_at: r.get("created_at"),
        }))
    }

    /// The preview alone. Separate from `attachment_for_corpus` so serving a
    /// thumbnail does not read the original's megabytes off disk.
    pub async fn attachment_preview(&self, corpus_id: &str) -> Result<Option<(String, Vec<u8>)>> {
        let row = sqlx::query("SELECT preview FROM attachments WHERE corpus_id = ? ORDER BY id LIMIT 1")
            .bind(corpus_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| (PREVIEW_MIME.to_string(), r.get("preview"))))
    }

    pub async fn attachment_original(&self, corpus_id: &str) -> Result<Option<(String, Vec<u8>)>> {
        let row = sqlx::query("SELECT mime, bytes FROM attachments WHERE corpus_id = ? ORDER BY id LIMIT 1")
            .bind(corpus_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| (r.get("mime"), r.get("bytes"))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_attachment_round_trips_and_goes_with_its_corpus() {
        let s = Store::memory().await.unwrap();
        let src = s
            .insert_image_corpus("h", "image", None, &serde_json::json!({}))
            .await
            .unwrap()
            .into_corpus();
        s.insert_attachment(&NewAttachment {
            corpus_id: &src.id,
            kind: "image",
            mime: "image/png",
            filename: Some("x.png"),
            bytes: b"orig",
            preview: b"prev",
            width: Some(10),
            height: Some(20),
        })
        .await
        .unwrap();

        let a = s.attachment_for_corpus(&src.id).await.unwrap().unwrap();
        assert_eq!(a.bytes, b"orig");
        assert_eq!(a.preview, b"prev");
        assert_eq!(a.mime, "image/png");
        assert_eq!((a.width, a.height), (Some(10), Some(20)));
        assert_eq!(
            s.attachment_preview(&src.id).await.unwrap().unwrap(),
            (PREVIEW_MIME.to_string(), b"prev".to_vec())
        );
        assert_eq!(
            s.attachment_original(&src.id).await.unwrap().unwrap(),
            ("image/png".to_string(), b"orig".to_vec())
        );

        s.delete_corpus(&src.id).await.unwrap();
        assert!(s.attachment_for_corpus(&src.id).await.unwrap().is_none());
    }
}
```

Append to `src/store/jobs.rs` tests (or create a `tests` module if there is none — check with `grep -n "mod tests" src/store/jobs.rs`):
```rust
    #[test]
    fn describe_is_a_stage_that_round_trips_its_name() {
        assert_eq!(Stage::Describe.as_str(), "describe");
        assert_eq!(Stage::parse("describe"), Some(Stage::Describe));
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test store:: 2>&1 | grep -E "^error|cannot find" | head`
Expected: compile errors for `Describing`, `insert_image_corpus`, `metadata`, `Stage::Describe`, module `attachments`.

- [ ] **Step 3: Implement**

`src/store/schema.sql`, after the `corpora` table:
```sql
-- The bytes an image corpus was captured from. `bytes` is the upload exactly
-- as it arrived — the verbatim source, as `raw_text` is for a paste — and
-- `preview` is the one derived copy: orientation applied, downscaled, JPEG.
CREATE TABLE IF NOT EXISTS attachments (
  id         INTEGER PRIMARY KEY,
  corpus_id  TEXT    NOT NULL REFERENCES corpora(id) ON DELETE CASCADE,
  kind       TEXT    NOT NULL,
  mime       TEXT    NOT NULL,
  filename   TEXT,
  bytes      BLOB    NOT NULL,
  preview    BLOB    NOT NULL,
  width      INTEGER,
  height     INTEGER,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS attachments_corpus ON attachments(corpus_id);
```
Also add `metadata TEXT NOT NULL DEFAULT '{}'` as the last column of the `corpora` table in `schema.sql` (fresh databases), with a comment: `-- What a door knew about the capture beyond the text: a note, file facts, EXIF. Namespaced JSON, '{}' when nothing was recorded.` Check how `schema_columns` parses lines (`src/store/mod.rs` ~line 240): a column line starts with its name, so this is fine.

`src/store/mod.rs`: `pub mod attachments;` and append to `ADDED_COLUMNS`:
```rust
            // Arrived with image capture. Every corpus predating it recorded
            // nothing beyond its text, which is what the empty object says.
            ("corpora", "metadata", "TEXT NOT NULL DEFAULT '{}'"),
```

`src/store/jobs.rs`: add variant with doc comment
```rust
    /// One image, one vision call: reads a captured image into the markdown
    /// that becomes its `raw_text`, then hands off to `Synthesize`.
    Describe,
```
plus `Stage::Describe => "describe"` and `"describe" => Some(Stage::Describe)`.

`src/store/corpora.rs`:
- `CorpusStatus`: add `Describing` as the first variant with doc `/// An image whose text has not been read yet. Only image corpora hold it.`; `as_str` → `"describing"`, `parse` → `"describing" => CorpusStatus::Describing`.
- `Corpus`: add `pub metadata: serde_json::Value,` with doc from the schema comment.
- `row_to_corpus`: `metadata: r.get::<Option<String>, _>("metadata").and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_else(|| serde_json::json!({})),`
- `insert_corpus_with_signature`: add trailing param `metadata: &serde_json::Value`, set `metadata: metadata.clone()` on the struct, add `metadata` to the INSERT column list and `.bind(src.metadata.to_string())`. Update `insert_corpus` to pass `&serde_json::json!({})`, and update the one caller in `src/core/ingest.rs` (`ingest_capture`) to pass `&c.metadata` — that field arrives in Task 6; for now pass `&serde_json::json!({})` there and note it.
- `ensure_restored_corpus`: no change (column defaults).
- New:
```rust
    /// The row for a captured image. There is no text yet — the vision stage
    /// writes it — so the hash is the caller's, over the image bytes, and the
    /// row starts in `describing`. Same conflict handling as a text capture:
    /// the same photo twice is one row.
    pub async fn insert_image_corpus(
        &self,
        content_hash: &str,
        origin: &str,
        title_hint: Option<&str>,
        metadata: &serde_json::Value,
    ) -> Result<Insertion> {
        let at = now();
        let id = new_id();
        let res = sqlx::query(
            "INSERT INTO corpora (id, raw_text, origin, title_hint, content_hash, status, created_at, updated_at, shingles, metadata)
             VALUES (?, '', ?, ?, ?, ?, ?, ?, '', ?)
             ON CONFLICT(content_hash) DO NOTHING",
        )
        .bind(&id)
        .bind(origin)
        .bind(title_hint)
        .bind(content_hash)
        .bind(CorpusStatus::Describing.as_str())
        .bind(at)
        .bind(at)
        .bind(metadata.to_string())
        .execute(&self.pool)
        .await?;
        let existing = self.find_by_hash(content_hash).await?.ok_or_else(|| {
            Error::Store("image capture conflicted with a corpus that then vanished".into())
        })?;
        Ok(if res.rows_affected() == 0 {
            Insertion::Existing(existing)
        } else {
            Insertion::Created(existing)
        })
    }

    /// What the vision stage read. Text and signature together, so the row is
    /// never comparable-by-shingle to something it does not say. Status is
    /// left to the caller, who knows whether this parks or proceeds.
    pub async fn set_described_text(&self, id: &str, text: &str, shingles: Vec<u64>) -> Result<()> {
        let res = sqlx::query(
            "UPDATE corpora SET raw_text = ?, shingles = ?, updated_at = ? WHERE id = ?",
        )
        .bind(text)
        .bind(super::shingle::encode(&shingles))
        .bind(now())
        .bind(id)
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    pub async fn set_corpus_metadata(&self, id: &str, metadata: &serde_json::Value) -> Result<()> {
        let res = sqlx::query("UPDATE corpora SET metadata = ?, updated_at = ? WHERE id = ?")
            .bind(metadata.to_string())
            .bind(now())
            .bind(id)
            .execute(&self.pool)
            .await?;
        if res.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }
```

`src/web/ui.rs`: `status_badge` → `Describing | Raw | Segmenting | Segmented | Embedding => "badge-accent"`. The `in_flight` match at ~line 722 lists terminal states, so `Describing` is in flight already — verify by reading it, no change needed. Fix every other exhaustive `match` on `CorpusStatus` the compiler reports (`grep -rn "CorpusStatus::Raw =>" src`).

- [ ] **Step 4: Run and commit**

Run: `cargo test 2>&1 | tail -5 && cargo clippy --all-targets -- -D warnings 2>&1 | tail -3`
Expected: all green.
```bash
git add src/store src/web/ui.rs src/core/ingest.rs
git commit -m "feat(store): describing status, describe stage, corpus metadata and attachments"
```

---

### Task 3: `core::image::prepare` — sniff, decode, EXIF, orientation, preview

**Files:**
- Create: `src/core/image.rs`
- Modify: `src/core/mod.rs` (`pub mod image;`)

**Interfaces:**
- Produces:
  ```rust
  pub struct PreparedImage { pub mime: &'static str, pub width: u32, pub height: u32, pub preview_jpeg: Vec<u8>, pub exif: serde_json::Value /* {} when none */ }
  pub fn prepare(bytes: &[u8], preview_edge: u32) -> Result<PreparedImage>   // Error::Validation on unsupported/undecodable
  pub fn file_facts(name: Option<&str>, size: usize, img: &PreparedImage) -> serde_json::Value  // {"name","size","mime","width","height"}
  pub fn exif_to_json(exif: &exif::Exif) -> serde_json::Value
  ```

- [ ] **Step 1: Write the failing tests**

Create `src/core/image.rs` with this test module (implementation follows in step 3):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgb};
    use std::io::Cursor;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = ImageBuffer::from_fn(w, h, |x, _| Rgb([(x % 256) as u8, 0, 0]));
        let mut out = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    fn jpeg(w: u32, h: u32) -> Vec<u8> {
        let img = ImageBuffer::from_fn(w, h, |x, _| Rgb([0, (x % 256) as u8, 0]));
        let mut out = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, image::ImageFormat::Jpeg)
            .unwrap();
        out.into_inner()
    }

    /// A JPEG carrying the given EXIF fields in an APP1 segment, spliced in
    /// right after SOI — which is where every camera puts it.
    fn jpeg_with_exif(w: u32, h: u32, fields: &[exif::Field]) -> Vec<u8> {
        let mut writer = exif::experimental::Writer::new();
        for f in fields {
            writer.push_field(f);
        }
        let mut blob = Cursor::new(Vec::new());
        writer.write(&mut blob, false).unwrap();
        let blob = blob.into_inner();
        let mut app1 = Vec::new();
        app1.extend_from_slice(&[0xFF, 0xE1]);
        let len = (blob.len() + 6 + 2) as u16;
        app1.extend_from_slice(&len.to_be_bytes());
        app1.extend_from_slice(b"Exif\0\0");
        app1.extend_from_slice(&blob);
        let plain = jpeg(w, h);
        let mut out = plain[..2].to_vec();
        out.extend_from_slice(&app1);
        out.extend_from_slice(&plain[2..]);
        out
    }

    fn ascii(tag: exif::Tag, s: &str) -> exif::Field {
        exif::Field {
            tag,
            ifd_num: exif::In::PRIMARY,
            value: exif::Value::Ascii(vec![s.as_bytes().to_vec()]),
        }
    }

    #[test]
    fn a_png_is_decoded_measured_and_previewed_as_jpeg() {
        let p = prepare(&png(300, 100), 2048).unwrap();
        assert_eq!(p.mime, "image/png");
        assert_eq!((p.width, p.height), (300, 100));
        assert_eq!(p.exif, serde_json::json!({}));
        let prev = image::load_from_memory(&p.preview_jpeg).unwrap();
        assert_eq!(image::guess_format(&p.preview_jpeg).unwrap(), image::ImageFormat::Jpeg);
        // Not upscaled: smaller than the edge stays its own size.
        assert_eq!((prev.width(), prev.height()), (300, 100));
    }

    #[test]
    fn a_large_image_is_downscaled_to_the_edge_keeping_its_ratio() {
        let p = prepare(&jpeg(4000, 2000), 1000).unwrap();
        let prev = image::load_from_memory(&p.preview_jpeg).unwrap();
        assert_eq!((prev.width(), prev.height()), (1000, 500));
        // The recorded size is the original's.
        assert_eq!((p.width, p.height), (4000, 2000));
    }

    #[test]
    fn exif_orientation_is_applied_to_the_preview_and_recorded() {
        let orient = exif::Field {
            tag: exif::Tag::Orientation,
            ifd_num: exif::In::PRIMARY,
            value: exif::Value::Short(vec![6]), // rotate 90° CW
        };
        let p = prepare(&jpeg_with_exif(400, 200, &[orient]), 2048).unwrap();
        let prev = image::load_from_memory(&p.preview_jpeg).unwrap();
        assert_eq!((prev.width(), prev.height()), (200, 400));
        assert_eq!((p.width, p.height), (200, 400), "dimensions are as displayed");
        assert_eq!(p.exif["orientation"], 6);
    }

    #[test]
    fn exif_facts_are_mapped_and_the_rest_kept_as_tags() {
        let fields = vec![
            ascii(exif::Tag::DateTimeOriginal, "2026:08:09 14:12:03"),
            ascii(exif::Tag::Make, "Apple"),
            ascii(exif::Tag::Model, "iPhone 15"),
            ascii(exif::Tag::Software, "17.5"),
        ];
        let p = prepare(&jpeg_with_exif(64, 64, &fields), 2048).unwrap();
        assert_eq!(p.exif["taken_at"], "2026-08-09T14:12:03");
        assert_eq!(p.exif["camera"], "Apple iPhone 15");
        assert_eq!(p.exif["tags"]["Software"], "17.5");
        assert!(p.exif.get("gps").is_none());
    }

    #[test]
    fn gps_is_converted_to_decimal_degrees() {
        let dms = |d: u32, m: u32, s: u32| {
            exif::Value::Rational(vec![
                exif::Rational { num: d, denom: 1 },
                exif::Rational { num: m, denom: 1 },
                exif::Rational { num: s * 100, denom: 100 },
            ])
        };
        let f = |tag, value| exif::Field { tag, ifd_num: exif::In::PRIMARY, value };
        let fields = vec![
            f(exif::Tag::GPSLatitude, dms(48, 12, 30)),
            ascii(exif::Tag::GPSLatitudeRef, "N"),
            f(exif::Tag::GPSLongitude, dms(16, 22, 0)),
            ascii(exif::Tag::GPSLongitudeRef, "W"),
            f(exif::Tag::GPSAltitude, exif::Value::Rational(vec![exif::Rational { num: 1710, denom: 10 }])),
        ];
        let p = prepare(&jpeg_with_exif(64, 64, &fields), 2048).unwrap();
        let g = &p.exif["gps"];
        assert!((g["lat"].as_f64().unwrap() - 48.208333).abs() < 1e-4, "{g}");
        assert!((g["lon"].as_f64().unwrap() + 16.366667).abs() < 1e-4, "{g}");
        assert!((g["alt"].as_f64().unwrap() - 171.0).abs() < 1e-6);
    }

    #[test]
    fn junk_and_unsupported_formats_are_refused_with_the_reason() {
        let e = prepare(b"not an image at all", 2048).unwrap_err();
        assert!(matches!(e, crate::error::Error::Validation(_)));
        assert!(e.to_string().contains("not a supported image"), "{e}");

        // A GIF header sniffs as an image but is not one of the three.
        let e = prepare(b"GIF89a\x01\x00\x01\x00\x80\x00\x00", 2048).unwrap_err();
        assert!(e.to_string().contains("gif"), "{e}");
    }

    #[test]
    fn file_facts_carry_name_size_and_dimensions() {
        let p = prepare(&png(30, 10), 2048).unwrap();
        let f = file_facts(Some("IMG_1.png"), 1234, &p);
        assert_eq!(f, serde_json::json!({"name": "IMG_1.png", "size": 1234, "mime": "image/png", "width": 30, "height": 10}));
        assert!(file_facts(None, 1, &p).get("name").is_none());
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test core::image 2>&1 | grep -E "^error" | head -3`
Expected: `prepare`, `file_facts` not found.

- [ ] **Step 3: Implement**

Top of `src/core/image.rs`:
```rust
//! What happens to an uploaded image before anything is stored: sniff the
//! format, decode it, read its EXIF, and derive the one preview the vision
//! model is shown and the UI displays. Pure functions; no I/O.

use crate::error::{Error, Result};
use image::{DynamicImage, ImageFormat};
use std::io::Cursor;

pub struct PreparedImage {
    /// Of the original, from its bytes rather than from what the client said.
    pub mime: &'static str,
    /// As displayed: after the EXIF orientation is applied.
    pub width: u32,
    pub height: u32,
    /// Orientation applied, longest edge at most `preview_edge`, JPEG.
    pub preview_jpeg: Vec<u8>,
    /// See `exif_to_json`. `{}` when the file carries none.
    pub exif: serde_json::Value,
}

/// JPEG quality of the preview: high enough that small print in a photographed
/// page survives, well below the original's size.
const PREVIEW_QUALITY: u8 = 85;

pub fn prepare(bytes: &[u8], preview_edge: u32) -> Result<PreparedImage> {
    let format = image::guess_format(bytes)
        .map_err(|_| Error::Validation("that upload is not a supported image (JPEG, PNG or WebP)".into()))?;
    let mime = match format {
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::Png => "image/png",
        ImageFormat::WebP => "image/webp",
        other => {
            return Err(Error::Validation(format!(
                "that image is {} — only JPEG, PNG and WebP are accepted",
                other.extensions_str().first().copied().unwrap_or("of an unsupported format")
            )));
        }
    };
    let decoded = image::load_from_memory_with_format(bytes, format)
        .map_err(|e| Error::Validation(format!("that image could not be decoded: {e}")))?;

    let exif = read_exif(bytes);
    let exif_json = exif.as_ref().map(exif_to_json).unwrap_or_else(|| serde_json::json!({}));
    let orientation = exif_json["orientation"]
        .as_u64()
        .and_then(|o| image::metadata::Orientation::from_exif(o as u8))
        .unwrap_or(image::metadata::Orientation::NoTransforms);

    let mut img = decoded;
    img.apply_orientation(orientation);
    let (width, height) = (img.width(), img.height());
    let preview_jpeg = encode_preview(&img, preview_edge)?;

    Ok(PreparedImage { mime, width, height, preview_jpeg, exif: exif_json })
}

fn read_exif(bytes: &[u8]) -> Option<exif::Exif> {
    exif::Reader::new()
        .read_from_container(&mut Cursor::new(bytes))
        .ok()
}

fn encode_preview(img: &DynamicImage, edge: u32) -> Result<Vec<u8>> {
    let scaled = if img.width().max(img.height()) > edge {
        img.thumbnail(edge, edge)
    } else {
        img.clone()
    };
    // JPEG has no alpha; a PNG with transparency is flattened rather than refused.
    let rgb = DynamicImage::ImageRgb8(scaled.to_rgb8());
    let mut out = Cursor::new(Vec::new());
    let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, PREVIEW_QUALITY);
    rgb.write_with_encoder(enc)
        .map_err(|e| Error::Internal(format!("preview encoding failed: {e}")))?;
    Ok(out.into_inner())
}

/// The `file` namespace of a corpus's metadata.
pub fn file_facts(name: Option<&str>, size: usize, img: &PreparedImage) -> serde_json::Value {
    let mut v = serde_json::json!({
        "size": size,
        "mime": img.mime,
        "width": img.width,
        "height": img.height,
    });
    if let Some(n) = name {
        v["name"] = serde_json::Value::String(n.to_string());
    }
    v
}

/// The `exif` namespace: the handful of facts worth naming, then every other
/// tag under `tags` by name so nothing the file carries is thrown away.
pub fn exif_to_json(exif: &exif::Exif) -> serde_json::Value {
    use exif::{In, Tag};
    let mut out = serde_json::Map::new();

    let ascii = |tag: Tag| -> Option<String> {
        exif.get_field(tag, In::PRIMARY).and_then(|f| match &f.value {
            exif::Value::Ascii(v) => v.first().map(|b| String::from_utf8_lossy(b).trim().to_string()),
            _ => None,
        })
    };

    if let Some(dt) = ascii(Tag::DateTimeOriginal) {
        // "2026:08:09 14:12:03" → "2026-08-09T14:12:03", offset appended when the
        // file says one.
        let mut iso = dt.replacen(':', "-", 2).replacen(' ', "T", 1);
        if let Some(off) = ascii(Tag::OffsetTimeOriginal) {
            iso.push_str(&off);
        }
        out.insert("taken_at".into(), iso.into());
    }
    let camera: Vec<String> = [ascii(Tag::Make), ascii(Tag::Model)].into_iter().flatten().collect();
    if !camera.is_empty() {
        out.insert("camera".into(), camera.join(" ").into());
    }
    if let Some(o) = exif.get_field(Tag::Orientation, In::PRIMARY).and_then(|f| f.value.get_uint(0)) {
        out.insert("orientation".into(), o.into());
    }
    if let Some(gps) = gps_json(exif) {
        out.insert("gps".into(), gps);
    }

    let mut tags = serde_json::Map::new();
    for f in exif.fields() {
        if f.ifd_num != In::PRIMARY {
            continue;
        }
        let name = f.tag.to_string();
        // Already named above, or binary noise nobody reads back.
        if matches!(f.tag, Tag::MakerNote | Tag::UserComment) || name.starts_with("Tag(") {
            continue;
        }
        let value: String = f.display_value().with_unit(exif).to_string().chars().take(200).collect();
        tags.insert(name, value.into());
    }
    if !tags.is_empty() {
        out.insert("tags".into(), tags.into());
    }
    serde_json::Value::Object(out)
}

fn gps_json(exif: &exif::Exif) -> Option<serde_json::Value> {
    use exif::{In, Tag};
    let dms = |tag: Tag, r: Tag, neg: &str| -> Option<f64> {
        let f = exif.get_field(tag, In::PRIMARY)?;
        let exif::Value::Rational(v) = &f.value else { return None };
        if v.len() < 3 {
            return None;
        }
        let deg = v[0].to_f64() + v[1].to_f64() / 60.0 + v[2].to_f64() / 3600.0;
        let sign = match exif.get_field(r, In::PRIMARY).map(|f| f.display_value().to_string()) {
            Some(s) if s.trim() == neg => -1.0,
            _ => 1.0,
        };
        Some(deg * sign)
    };
    let lat = dms(Tag::GPSLatitude, Tag::GPSLatitudeRef, "S")?;
    let lon = dms(Tag::GPSLongitude, Tag::GPSLongitudeRef, "W")?;
    let mut g = serde_json::json!({ "lat": lat, "lon": lon });
    if let Some(f) = exif.get_field(Tag::GPSAltitude, In::PRIMARY)
        && let exif::Value::Rational(v) = &f.value
        && let Some(a) = v.first()
    {
        g["alt"] = serde_json::json!(a.to_f64());
    }
    Some(g)
}
```
Add `pub mod image;` to `src/core/mod.rs`. If `image::metadata::Orientation::from_exif` or `apply_orientation` are missing in the resolved `image` version, run `cargo doc -p image --open`-equivalent `grep -rn "fn from_exif\|fn apply_orientation" ~/.cargo/registry/src/*/image-0.25*/src/` and adapt; both exist from 0.25.2. `GPSLatitudeRef` display value renders as `N`/`S` — if the test fails on sign, match on the raw `Ascii` bytes instead as `ascii()` does.

- [ ] **Step 4: Run and commit**

Run: `cargo test core::image 2>&1 | tail -12 && cargo clippy --all-targets -- -D warnings 2>&1 | tail -3`
Expected: 7 tests pass.
```bash
git add src/core/image.rs src/core/mod.rs
git commit -m "feat(image): decode, exif and preview for a captured image"
```

---

### Task 4: Inference — `Describer` trait, HTTP and fake implementations, prompt

**Files:**
- Modify: `src/infer/mod.rs` (after `Completer`)
- Modify: `src/infer/prompt.rs` (append)
- Modify: `src/infer/openai.rs` (after `HttpCompleter`)
- Modify: `src/infer/fake.rs` (append)
- Modify: `src/core/mod.rs` (`Core.describer`, `from_config`, `test_support`)
- Modify: `src/main.rs:157` (probe)

**Interfaces:**
- Produces:
  ```rust
  #[async_trait] pub trait Describer: Send + Sync { async fn describe(&self, image_jpeg: &[u8], context: &str) -> Result<String>; }
  pub struct HttpDescriber; impl HttpDescriber { pub fn new(model: &str, base_url: &str, api_key: Option<&str>, timeout_secs: u64) -> Self }
  pub struct FakeDescriber { pub reply: String, pub fail_with: Option<String>, pub calls: AtomicUsize, pub last_context: Mutex<String> }
  impl FakeDescriber { pub fn saying(s: &str) -> Self; pub fn failing(msg: &str) -> Self; pub fn calls(&self) -> usize; pub fn last_context(&self) -> String }
  pub const DESCRIBE_SYSTEM: &str;
  pub fn describe_context(metadata: &serde_json::Value) -> String;
  Core { pub describer: Option<Arc<dyn Describer>>, ... }
  test_support::test_core_with_describer(d: Arc<FakeDescriber>) -> Core; test_support::test_core_without_vision() -> Core
  ```

- [ ] **Step 1: Failing tests**

Append to `src/infer/prompt.rs` tests (find `mod tests`; add if absent):
```rust
    #[test]
    fn describe_context_leads_with_the_note_then_the_facts_and_omits_what_is_absent() {
        let m = serde_json::json!({
            "note": "whiteboard from Tuesday planning",
            "file": {"name": "IMG_2041.jpeg"},
            "exif": {"taken_at": "2026-08-09T14:12:03", "camera": "Apple iPhone 15",
                     "gps": {"lat": 48.2082, "lon": 16.3738}}
        });
        let ctx = describe_context(&m);
        let note_at = ctx.find("whiteboard from Tuesday planning").unwrap();
        let taken_at = ctx.find("2026-08-09T14:12:03").unwrap();
        assert!(note_at < taken_at, "{ctx}");
        assert!(ctx.contains("48.2082"), "{ctx}");
        assert!(ctx.contains("Apple iPhone 15"));
        assert!(ctx.contains("IMG_2041.jpeg"));

        let bare = describe_context(&serde_json::json!({}));
        assert!(!bare.contains("taken"), "{bare}");
        assert!(!bare.contains("GPS"), "{bare}");
        assert!(bare.contains("Read the image"), "{bare}");
    }
```

Append to the tests module in `src/infer/openai.rs`:
```rust
    #[tokio::test]
    async fn the_describer_sends_the_image_as_a_data_url_beside_the_context() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "# Whiteboard\n\n- item"}}]
            })))
            .mount(&server)
            .await;
        let d = HttpDescriber::new("vl", &format!("{}/v1", server.uri()), Some("k"), 30);
        let out = d.describe(b"\xFF\xD8jpegbytes", "Photo taken 2026-08-09").await.unwrap();
        assert_eq!(out, "# Whiteboard\n\n- item");

        let req = &server.received_requests().await.unwrap()[0];
        assert_eq!(req.headers.get("authorization").unwrap(), "Bearer k");
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(body["model"], "vl");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], prompt::DESCRIBE_SYSTEM);
        let parts = body["messages"][1]["content"].as_array().unwrap();
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "Photo taken 2026-08-09");
        assert_eq!(parts[1]["type"], "image_url");
        let url = parts[1]["image_url"]["url"].as_str().unwrap();
        assert!(url.starts_with("data:image/jpeg;base64,"), "{url}");
        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(url.trim_start_matches("data:image/jpeg;base64,"))
            .unwrap();
        assert_eq!(decoded, b"\xFF\xD8jpegbytes");
    }

    #[tokio::test]
    async fn a_describer_error_is_an_inference_error_for_the_vision_role() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503).set_body_string("busy"))
            .mount(&server)
            .await;
        let d = HttpDescriber::new("vl", &server.uri(), None, 30);
        let e = d.describe(b"x", "").await.unwrap_err();
        assert!(matches!(e, Error::Inference { role: "vision", .. }), "{e}");
        assert!(e.retryable());
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test infer:: 2>&1 | grep -E "^error" | head -3`
Expected: `describe_context`, `HttpDescriber`, `DESCRIBE_SYSTEM` not found.

- [ ] **Step 3: Implement**

`src/infer/mod.rs`, after `Completer`:
```rust
/// Reads a captured image into text. One call per image; the caller has
/// already decoded, oriented and downscaled the picture into a JPEG.
#[async_trait]
pub trait Describer: Send + Sync {
    /// `context` is what is known about the capture beyond its pixels — the
    /// user's note, when and where it was taken. Markdown comes back.
    async fn describe(&self, image_jpeg: &[u8], context: &str) -> Result<String>;
}
```

`src/infer/prompt.rs`, append:
```rust
pub const DESCRIBE_SYSTEM: &str = r#"You read images for a personal knowledge base and write down everything in them worth keeping, as markdown.

Rules:
- Transcribe any visible text faithfully and completely. Keep its structure: headings as headings, lists as lists, tables as markdown tables, code as code blocks. Do not correct, summarize or reorder it.
- Where there is no text, or beside it, describe what is shown: diagrams (their parts and how they connect), charts (axes, series, the values that can be read), scenes, objects, people's roles if evident, places, labels, brands, numbers, dates. Name what is identifiable.
- Prefer specifics over impressions. Do not pad, do not speculate beyond what is visible, do not add advice.
- You may be given context about the capture: a note from the person who took it, when and where it was taken, the device. Where it is relevant, weave it in naturally so the text can be found again by it — as a short opening line or where it explains the content — but do not repeat it mechanically or invent detail around it.
- Output markdown only. No preamble, no closing remarks, no mention of these instructions."#;

/// The user turn's text part for `Describer::describe`: the note first, then
/// the facts the file carried, each only when present.
pub fn describe_context(metadata: &serde_json::Value) -> String {
    let mut lines: Vec<String> = Vec::new();
    if let Some(note) = metadata["note"].as_str().filter(|n| !n.trim().is_empty()) {
        lines.push(format!("Context from the person who captured this: {}", note.trim()));
    }
    let mut facts: Vec<String> = Vec::new();
    let exif = &metadata["exif"];
    if let Some(t) = exif["taken_at"].as_str() {
        facts.push(format!("taken {t}"));
    }
    if let (Some(lat), Some(lon)) = (exif["gps"]["lat"].as_f64(), exif["gps"]["lon"].as_f64()) {
        facts.push(format!("GPS {lat:.4},{lon:.4}"));
    }
    if let Some(c) = exif["camera"].as_str() {
        facts.push(format!("device {c}"));
    }
    if let Some(n) = metadata["file"]["name"].as_str() {
        facts.push(format!("file {n}"));
    }
    if !facts.is_empty() {
        lines.push(format!("Capture facts: {}.", facts.join(", ")));
    }
    lines.push("Read the image and write down everything worth keeping.".into());
    lines.join("\n")
}
```

`src/infer/openai.rs`, after `HttpCompleter` impl:
```rust
// ── Describer ────────────────────────────────────────────────────────────────

pub struct HttpDescriber {
    client: reqwest::Client,
    base_url: String,
    model: String,
    api_key: Option<String>,
}

impl HttpDescriber {
    pub fn new(model: &str, base_url: &str, api_key: Option<&str>, timeout_secs: u64) -> Self {
        Self {
            client: client(timeout_secs),
            base_url: base_url.to_string(),
            model: model.to_string(),
            api_key: api_key.map(str::to_string),
        }
    }
}

#[async_trait]
impl super::Describer for HttpDescriber {
    async fn describe(&self, image_jpeg: &[u8], context: &str) -> Result<String> {
        use base64::Engine;
        let data_url = format!(
            "data:image/jpeg;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(image_jpeg)
        );
        let body = json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": prompt::DESCRIBE_SYSTEM},
                {"role": "user", "content": [
                    {"type": "text", "text": context},
                    {"type": "image_url", "image_url": {"url": data_url}}
                ]}
            ],
            "temperature": 0.2,
        });
        let started = std::time::Instant::now();
        let v = post_json(
            "vision",
            &self.client,
            url(&self.base_url, "chat/completions"),
            self.api_key.as_deref(),
            body,
        )
        .await?;
        tracing::info!(
            ms = started.elapsed().as_millis(),
            tokens = v["usage"]["completion_tokens"].as_u64(),
            "vision call finished"
        );
        v["choices"][0]["message"]["content"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| Error::Inference {
                role: "vision",
                detail: "no message content".into(),
            })
    }
}
```
Add `Describer` to the `use super::{...}` list at the top or keep the `super::Describer` path — either compiles.

`src/infer/fake.rs`, append:
```rust
/// Answers every image with one scripted reply, or one scripted failure, and
/// remembers what context it was shown.
pub struct FakeDescriber {
    pub reply: String,
    pub fail_with: Option<String>,
    calls: std::sync::atomic::AtomicUsize,
    last_context: std::sync::Mutex<String>,
}

impl Default for FakeDescriber {
    fn default() -> Self {
        Self::saying("# Photo\n\nA whiteboard listing three tasks: ship, test, rest.")
    }
}

impl FakeDescriber {
    pub fn saying(reply: &str) -> Self {
        Self {
            reply: reply.into(),
            fail_with: None,
            calls: Default::default(),
            last_context: Default::default(),
        }
    }
    pub fn failing(msg: &str) -> Self {
        let mut d = Self::saying("");
        d.fail_with = Some(msg.into());
        d
    }
    pub fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
    pub fn last_context(&self) -> String {
        self.last_context.lock().unwrap().clone()
    }
}

#[async_trait]
impl super::Describer for FakeDescriber {
    async fn describe(&self, _image_jpeg: &[u8], context: &str) -> Result<String> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        *self.last_context.lock().unwrap() = context.to_string();
        match &self.fail_with {
            Some(m) => Err(Error::Inference { role: "vision", detail: m.clone() }),
            None => Ok(self.reply.clone()),
        }
    }
}
```
(Check the top of `fake.rs` for its `use` lines; `Error` and `Result` from `crate::error` and `async_trait` are already imported for the other fakes.)

`src/core/mod.rs`:
- import `HttpDescriber` and `Describer`;
- field after `completer`: 
  ```rust
      /// The vision model, when one is configured. `None` closes the image door.
      pub describer: Option<Arc<dyn Describer>>,
  ```
- `from_config`: 
  ```rust
              describer: cfg.infer.vision.as_ref().map(|v| {
                  let (base_url, api_key) = v.resolve(&cfg.infer.synthesize);
                  Arc::new(HttpDescriber::new(&v.model, &base_url, api_key.as_deref(), v.timeout_secs))
                      as Arc<dyn Describer>
              }),
  ```
- `test_support::build`: `describer: Some(Arc::new(FakeDescriber::default())),` and add:
  ```rust
      /// A core whose vision model is the given fake, for asserting what it was
      /// asked and answering what a test needs.
      pub async fn test_core_with_describer(d: Arc<FakeDescriber>) -> Core {
          let mut core = build(Arc::new(FakeSynthesizer::default()), None).await;
          core.describer = Some(d);
          core
      }

      /// The shipped default: no `[infer.vision]`, image door closed.
      pub async fn test_core_without_vision() -> Core {
          let mut core = build(Arc::new(FakeSynthesizer::default()), None).await;
          core.describer = None;
          core
      }
  ```
  and import `FakeDescriber` there.

`src/main.rs`, after the rerank probe:
```rust
    if let Some(v) = &cfg.infer.vision {
        let (base_url, api_key) = v.resolve(&cfg.infer.synthesize);
        engram::infer::openai::probe("vision", &base_url, api_key.as_deref()).await;
    } else {
        tracing::info!("vision not configured; the image door is closed");
    }
```

- [ ] **Step 4: Run and commit**

Run: `cargo test 2>&1 | tail -5 && cargo clippy --all-targets -- -D warnings 2>&1 | tail -3`
```bash
git add src/infer src/core/mod.rs src/main.rs
git commit -m "feat(infer): Describer role — vision call, fake, prompt"
```

---

### Task 5: `Core::ingest_image` — the door's logic, and `note` on text captures

**Files:**
- Modify: `src/core/ingest.rs` (`Capture` ~line 60, `ingest_capture` ~line 105, tests at the bottom)

**Interfaces:**
- Consumes: `core::image::{prepare, file_facts}`, `Store::insert_image_corpus`, `Store::insert_attachment`, `Stage::Describe`, `Core.describer`.
- Produces:
  ```rust
  pub struct Capture { ..., pub metadata: serde_json::Value }   // default {}
  impl Capture { pub fn with_note(self, note: Option<String>) -> Self; pub fn with_file(self, name: Option<&str>, size: usize, mime: &str) -> Self }
  pub const ORIGIN_IMAGE: &str = "image";
  pub const MAX_NOTE_CHARS: usize = 2000;
  pub struct ImageCapture { pub bytes: Vec<u8>, pub filename: Option<String>, pub title_hint: Option<String>, pub note: Option<String> }
  Core::ingest_image(&self, c: ImageCapture) -> Result<IngestOutcome>   // status Describing on create; duplicate=true on same bytes
  ```

- [ ] **Step 1: Failing tests**

Append to `src/core/ingest.rs` tests:
```rust
    fn a_png() -> Vec<u8> {
        use image::{ImageBuffer, Rgb};
        let img = ImageBuffer::from_fn(40, 20, |x, _| Rgb([(x * 6) as u8, 0, 0]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    #[tokio::test]
    async fn an_image_capture_stores_the_original_and_queues_describe_without_calling_the_model() {
        let describer = std::sync::Arc::new(crate::infer::fake::FakeDescriber::default());
        let core = crate::core::test_support::test_core_with_describer(describer.clone()).await;
        let bytes = a_png();
        let out = core
            .ingest_image(ImageCapture {
                bytes: bytes.clone(),
                filename: Some("IMG_1.png".into()),
                title_hint: None,
                note: Some("  the kitchen whiteboard ".into()),
            })
            .await
            .unwrap();
        assert_eq!(out.status, CorpusStatus::Describing);
        assert!(!out.duplicate);
        assert_eq!(describer.calls(), 0, "capture must not call the model");

        let src = core.store.get_corpus(&out.id).await.unwrap();
        assert_eq!(src.origin, ORIGIN_IMAGE);
        assert_eq!(src.raw_text, "");
        assert_eq!(src.title_hint.as_deref(), Some("IMG_1.png"));
        assert_eq!(src.metadata["note"], "the kitchen whiteboard");
        assert_eq!(src.metadata["file"]["name"], "IMG_1.png");
        assert_eq!(src.metadata["file"]["mime"], "image/png");
        assert_eq!(src.metadata["file"]["width"], 40);

        let a = core.store.attachment_for_corpus(&out.id).await.unwrap().unwrap();
        assert_eq!(a.bytes, bytes, "the original is stored byte for byte");
        assert_eq!(a.mime, "image/png");
        assert!(image::load_from_memory(&a.preview).is_ok());

        let job = core.store.claim_job().await.unwrap().expect("a job was queued");
        assert_eq!(job.stage, Stage::Describe);
        assert_eq!(job.target_id, out.id);
    }

    #[tokio::test]
    async fn the_same_photo_twice_is_a_duplicate_before_any_model_call() {
        let core = crate::core::test_support::test_core().await;
        let first = core
            .ingest_image(ImageCapture { bytes: a_png(), filename: None, title_hint: None, note: None })
            .await
            .unwrap();
        let again = core
            .ingest_image(ImageCapture { bytes: a_png(), filename: None, title_hint: None, note: None })
            .await
            .unwrap();
        assert!(again.duplicate);
        assert_eq!(again.id, first.id);
        assert_eq!(core.store.list_corpora(10, 0).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn without_a_vision_role_the_image_door_is_closed() {
        let core = crate::core::test_support::test_core_without_vision().await;
        let e = core
            .ingest_image(ImageCapture { bytes: a_png(), filename: None, title_hint: None, note: None })
            .await
            .unwrap_err();
        assert!(matches!(e, Error::Validation(_)));
        assert!(e.to_string().contains("not configured"), "{e}");
        assert!(core.store.list_corpora(10, 0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn junk_is_refused_before_anything_is_stored() {
        let core = crate::core::test_support::test_core().await;
        let e = core
            .ingest_image(ImageCapture { bytes: b"nope".to_vec(), filename: Some("x.jpg".into()), title_hint: None, note: None })
            .await
            .unwrap_err();
        assert!(matches!(e, Error::Validation(_)));
        assert!(core.store.list_corpora(10, 0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_note_is_capped_and_a_blank_one_is_dropped() {
        let core = crate::core::test_support::test_core().await;
        let long = "x".repeat(MAX_NOTE_CHARS + 50);
        let out = core
            .ingest_capture(Capture::new("some text", "upload").with_note(Some(long)))
            .await
            .unwrap();
        let src = core.store.get_corpus(&out.id).await.unwrap();
        assert_eq!(src.metadata["note"].as_str().unwrap().len(), MAX_NOTE_CHARS);

        let out = core
            .ingest_capture(Capture::new("other text", "upload").with_note(Some("   ".into())))
            .await
            .unwrap();
        assert!(core.store.get_corpus(&out.id).await.unwrap().metadata.get("note").is_none());
    }

    #[tokio::test]
    async fn a_text_capture_carries_its_file_facts() {
        let core = crate::core::test_support::test_core().await;
        let out = core
            .ingest_capture(Capture::new("hello", "upload").with_file(Some("n.txt"), 5, "text/plain"))
            .await
            .unwrap();
        let m = core.store.get_corpus(&out.id).await.unwrap().metadata;
        assert_eq!(m["file"], serde_json::json!({"name": "n.txt", "size": 5, "mime": "text/plain"}));
    }
```
Add `use super::*;`-visible imports the tests need (`ImageCapture`, `ORIGIN_IMAGE`, `MAX_NOTE_CHARS` are in scope via `super::*` once defined).

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test core::ingest::tests 2>&1 | grep -E "^error" | head -3`

- [ ] **Step 3: Implement**

In `src/core/ingest.rs`:
```rust
/// The channel an image arrives through. Its own value, like `upload`, so
/// the queue and the detail page can tell a photo from a paste.
pub const ORIGIN_IMAGE: &str = "image";
/// Longest note kept. Context, not a document: someone wanting to say more
/// than this has a paste box.
pub const MAX_NOTE_CHARS: usize = 2000;

/// The user's context for a capture, cleaned: trimmed, capped, `None` when
/// there is nothing in it.
fn clean_note(note: Option<String>) -> Option<String> {
    let n = note?.trim().to_string();
    if n.is_empty() {
        return None;
    }
    Some(n.chars().take(MAX_NOTE_CHARS).collect())
}
```
`Capture` gains `pub metadata: serde_json::Value` (init `serde_json::json!({})` in `new`) with doc `/// What the door knew beyond the text. Namespaced; see the schema comment.` and:
```rust
    pub fn with_note(mut self, note: Option<String>) -> Self {
        if let Some(n) = clean_note(note) {
            self.metadata["note"] = serde_json::Value::String(n);
        }
        self
    }

    /// The `file` facts of an uploaded text file.
    pub fn with_file(mut self, name: Option<&str>, size: usize, mime: &str) -> Self {
        let mut f = serde_json::json!({ "size": size, "mime": mime });
        if let Some(n) = name {
            f["name"] = serde_json::Value::String(n.to_string());
        }
        self.metadata["file"] = f;
        self
    }
```
In `ingest_capture`, replace the `serde_json::json!({})` placeholder from Task 2 with `&c.metadata`.

```rust
/// One image, whichever door it arrived through.
#[derive(Debug, Clone)]
pub struct ImageCapture {
    pub bytes: Vec<u8>,
    pub filename: Option<String>,
    pub title_hint: Option<String>,
    pub note: Option<String>,
}

impl Core {
    /// Store the image and queue the vision stage. Like `ingest_capture`, this
    /// makes no inference call: the phone gets its answer the moment the bytes
    /// are safe, and a dead vision endpoint costs a wait, not a photo.
    pub async fn ingest_image(&self, c: ImageCapture) -> Result<IngestOutcome> {
        if self.describer.is_none() {
            return Err(Error::Validation(
                "image capture is not configured — set [infer.vision] to enable it".into(),
            ));
        }
        let prepared = super::image::prepare(&c.bytes, self.capture.image_preview_edge)?;
        let hash = hex::encode(sha2::Sha256::digest(&c.bytes));
        if let Some(existing) = self.store.find_by_hash(&hash).await? {
            tracing::info!(corpus_id = %existing.id, "duplicate image, returning existing source");
            return Ok(IngestOutcome { id: existing.id, status: existing.status, duplicate: true, near_duplicate: None });
        }

        let mut metadata = serde_json::json!({
            "file": super::image::file_facts(c.filename.as_deref(), c.bytes.len(), &prepared),
        });
        if prepared.exif.as_object().is_some_and(|o| !o.is_empty()) {
            metadata["exif"] = prepared.exif.clone();
        }
        if let Some(n) = clean_note(c.note) {
            metadata["note"] = serde_json::Value::String(n);
        }
        let title_hint = c.title_hint.or_else(|| c.filename.clone());

        let src = match self
            .store
            .insert_image_corpus(&hash, ORIGIN_IMAGE, title_hint.as_deref(), &metadata)
            .await?
        {
            Insertion::Created(src) => src,
            Insertion::Existing(existing) => {
                return Ok(IngestOutcome { id: existing.id, status: existing.status, duplicate: true, near_duplicate: None });
            }
        };
        self.store
            .insert_attachment(&crate::store::attachments::NewAttachment {
                corpus_id: &src.id,
                kind: "image",
                mime: prepared.mime,
                filename: c.filename.as_deref(),
                bytes: &c.bytes,
                preview: &prepared.preview_jpeg,
                width: Some(prepared.width as i64),
                height: Some(prepared.height as i64),
            })
            .await?;
        self.store.enqueue(Stage::Describe, "corpus", &src.id).await?;
        tracing::info!(corpus_id = %src.id, bytes = c.bytes.len(), mime = prepared.mime, "image captured; queued for reading");
        Ok(IngestOutcome { id: src.id, status: CorpusStatus::Describing, duplicate: false, near_duplicate: None })
    }
}
```
Add `use sha2::Digest;` at the top of the file.

- [ ] **Step 4: Run and commit**

Run: `cargo test core::ingest 2>&1 | tail -5 && cargo clippy --all-targets -- -D warnings 2>&1 | tail -3`
```bash
git add src/core/ingest.rs
git commit -m "feat(ingest): image door — store, hash, queue describe; note and file facts on captures"
```

---

### Task 6: `Stage::Describe` — the job

**Files:**
- Create: `src/jobs/describe.rs`
- Modify: `src/jobs/mod.rs` (`pub mod describe;`, dispatch arm)

**Interfaces:**
- Consumes: `Core.describer`, `Store::attachment_preview`, `Store::set_described_text`, `Store::set_corpus_metadata`, `Store::find_near_duplicate`, `Store::set_near_dupe`, `prompt::describe_context`, `core.gate.background()`.
- Produces: `pub async fn run(core: &Core, corpus_id: &str) -> Result<()>`

- [ ] **Step 1: Failing tests**

Create `src/jobs/describe.rs` with this test module:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ingest::ImageCapture;
    use crate::core::test_support::{test_core_with_describer, test_core_without_vision};
    use crate::infer::fake::FakeDescriber;
    use crate::store::corpora::CorpusStatus;
    use crate::store::jobs::Stage;
    use std::sync::Arc;

    fn a_png(seed: u8) -> Vec<u8> {
        use image::{ImageBuffer, Rgb};
        let img = ImageBuffer::from_fn(32, 32, |x, y| Rgb([seed, x as u8, y as u8]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img).write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    async fn captured(core: &crate::core::Core, seed: u8, note: Option<&str>) -> String {
        core.ingest_image(ImageCapture { bytes: a_png(seed), filename: Some("p.png".into()), title_hint: None, note: note.map(str::to_string) })
            .await
            .unwrap()
            .id
    }

    #[tokio::test]
    async fn describing_writes_the_text_and_hands_off_to_synthesize() {
        let d = Arc::new(FakeDescriber::saying("# Board\n\n- ship\n- test"));
        let core = test_core_with_describer(d.clone()).await;
        let id = captured(&core, 1, Some("kitchen board")).await;
        core.store.claim_job().await.unwrap(); // the Describe job
        run(&core, &id).await.unwrap();

        let src = core.store.get_corpus(&id).await.unwrap();
        assert_eq!(src.status, CorpusStatus::Raw);
        assert_eq!(src.raw_text, "# Board\n\n- ship\n- test");
        assert!(!src.shingles.is_empty());
        assert!(d.last_context().contains("kitchen board"), "{}", d.last_context());
        assert!(d.last_context().contains("p.png"));
        let next = core.store.claim_job().await.unwrap().expect("synthesize queued");
        assert_eq!(next.stage, Stage::Synthesize);
        assert_eq!(next.target_id, id);
    }

    #[tokio::test]
    async fn the_whole_pipeline_takes_a_photo_to_ready() {
        let core = test_core_with_describer(Arc::new(FakeDescriber::default())).await;
        let id = captured(&core, 2, None).await;
        while crate::jobs::run_one(&core).await.unwrap() {}
        let src = core.store.get_corpus(&id).await.unwrap();
        assert_eq!(src.status, CorpusStatus::Ready, "{:?}", src.status);
        assert!(!core.store.artifacts_for_corpus(&id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_failing_model_leaves_the_corpus_describing_and_the_error_retryable() {
        let core = test_core_with_describer(Arc::new(FakeDescriber::failing("gpu on fire"))).await;
        let id = captured(&core, 3, None).await;
        let e = run(&core, &id).await.unwrap_err();
        assert!(e.retryable());
        assert_eq!(core.store.get_corpus(&id).await.unwrap().status, CorpusStatus::Describing);
    }

    #[tokio::test]
    async fn an_empty_reading_parks_the_corpus_with_the_reason() {
        let core = test_core_with_describer(Arc::new(FakeDescriber::saying("  \n"))).await;
        let id = captured(&core, 4, None).await;
        core.store.claim_job().await.unwrap();
        run(&core, &id).await.unwrap();
        let src = core.store.get_corpus(&id).await.unwrap();
        assert_eq!(src.status, CorpusStatus::NeedsReview);
        assert!(src.metadata["describe"]["error"].as_str().unwrap().contains("no text"));
        assert!(core.store.claim_job().await.unwrap().is_none(), "nothing further queued");
    }

    #[tokio::test]
    async fn a_reading_that_matches_an_existing_corpus_is_parked_as_a_near_duplicate() {
        let text = "The quarterly plan lists three goals: ship the beta, hire two engineers, and cut latency in half by autumn.";
        let core = test_core_with_describer(Arc::new(FakeDescriber::saying(text))).await;
        let first = core.ingest(text, "web", None).await.unwrap();
        let id = captured(&core, 5, None).await;
        core.store.claim_job().await.unwrap(); // synthesize for the paste
        core.store.claim_job().await.unwrap(); // describe
        run(&core, &id).await.unwrap();
        let src = core.store.get_corpus(&id).await.unwrap();
        assert_eq!(src.status, CorpusStatus::NeedsReview);
        assert_eq!(src.near_dupe_of.as_deref(), Some(first.id.as_str()));
        assert_eq!(src.raw_text, text, "the reading is kept even when parked");
    }

    #[tokio::test]
    async fn a_job_for_a_corpus_that_is_gone_is_not_found() {
        let core = test_core_with_describer(Arc::new(FakeDescriber::default())).await;
        assert!(matches!(run(&core, "nope").await, Err(crate::error::Error::NotFound)));
    }

    #[tokio::test]
    async fn without_a_vision_role_the_job_waits_rather_than_failing_the_corpus() {
        // Configured when the photo was taken, removed before it was read: the
        // job stays queued at the backoff ceiling until the role comes back.
        let core = test_core_without_vision().await;
        let src = core.store.insert_image_corpus("h", "image", None, &serde_json::json!({})).await.unwrap().into_corpus();
        let e = run(&core, &src.id).await.unwrap_err();
        assert!(e.retryable(), "{e}");
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test jobs::describe 2>&1 | grep -E "^error" | head -3`

- [ ] **Step 3: Implement**

Top of `src/jobs/describe.rs`:
```rust
//! The vision stage: one captured image, one call, and a corpus that from
//! here on is text like any other.

use crate::core::Core;
use crate::error::{Error, Result};
use crate::store::corpora::CorpusStatus;
use crate::store::jobs::Stage;

pub async fn run(core: &Core, corpus_id: &str) -> Result<()> {
    let src = core.store.get_corpus(corpus_id).await?;
    if src.status != CorpusStatus::Describing {
        tracing::info!(corpus_id, status = src.status.as_str(), "already described; nothing to do");
        return Ok(());
    }
    let Some(describer) = core.describer.as_ref() else {
        // Not a validation error: the photo is stored and the job should wait
        // for the role, not be dropped.
        return Err(Error::Inference { role: "vision", detail: "no vision role configured".into() });
    };
    let Some((_, preview)) = core.store.attachment_preview(corpus_id).await? else {
        return Err(Error::Store(format!("image corpus {corpus_id} has no attachment")));
    };
    let context = crate::infer::prompt::describe_context(&src.metadata);

    let permit = core.gate.background().await;
    let read = describer.describe(&preview, &context).await;
    match &read {
        Ok(_) => permit.succeeded(),
        Err(e) => permit.failed(e),
    }
    let text = read?;

    if text.trim().is_empty() {
        let mut meta = src.metadata.clone();
        meta["describe"] = serde_json::json!({ "error": "the model returned no text for this image" });
        core.store.set_corpus_metadata(corpus_id, &meta).await?;
        core.store.set_corpus_status(corpus_id, CorpusStatus::NeedsReview).await?;
        tracing::warn!(corpus_id, "vision model returned nothing; parked for review");
        return Ok(());
    }

    let sig = crate::store::shingle::signature(&text);
    let near = core.store.find_near_duplicate(&sig, core.consolidate.near_dupe_min).await?;
    core.store.set_described_text(corpus_id, &text, sig).await?;
    match near {
        Some(n) => {
            core.store.set_near_dupe(corpus_id, Some(&n.corpus_id), Some(n.similarity)).await?;
            core.store.set_corpus_status(corpus_id, CorpusStatus::NeedsReview).await?;
            tracing::info!(corpus_id, near = %n.corpus_id, similarity = n.similarity, "reading looks like an existing corpus; parked for review");
        }
        None => {
            core.store.set_corpus_status(corpus_id, CorpusStatus::Raw).await?;
            core.store.enqueue(Stage::Synthesize, "corpus", corpus_id).await?;
            tracing::info!(corpus_id, chars = text.len(), "image read; queued for synthesis");
        }
    }
    Ok(())
}
```
Check `find_near_duplicate`'s signature (`src/store/corpora.rs:335`) and `set_near_dupe` (`:366`) and match them exactly. In `src/jobs/mod.rs`: `pub mod describe;` and the dispatch arm `(Stage::Describe, _) => describe::run(core, &job.target_id).await,`. Note `set_corpus_status` on a NeedsReview near-dupe path: `find_near_duplicate` may need the corpus's own signature excluded — read its implementation; it compares against stored signatures and this corpus's is `''` until `set_described_text`, so calling `find_near_duplicate` **before** `set_described_text` (as written) is what keeps it from matching itself.

- [ ] **Step 4: Run and commit**

Run: `cargo test jobs:: 2>&1 | tail -5 && cargo clippy --all-targets -- -D warnings 2>&1 | tail -3`
```bash
git add src/jobs
git commit -m "feat(jobs): describe stage reads an image into its corpus text"
```

---

### Task 7: API — image upload, image serving, `note` on text upload

**Files:**
- Modify: `src/web/api.rs` (`upload` ~line 279, router ~line 698, tests)
- Modify: `src/web/mod.rs:53-66` (`router`)

**Interfaces:**
- Consumes: `Core::ingest_image`, `Capture::with_note/with_file`, `Store::attachment_preview/attachment_original`.
- Produces: `pub fn api_router(image_max_bytes: usize) -> Router<AppState>`; routes `POST /api/v1/corpora/image`, `GET /api/v1/corpora/{id}/image[?original=1]`.

- [ ] **Step 1: Failing tests**

In `src/web/api.rs` tests, generalize the multipart helper: rename `post_file` → keep it, and add a sibling that carries extra text fields:
```rust
    /// Like `post_file`, with text fields sent before the file part.
    fn post_file_with(
        uri: &str,
        token: &str,
        fields: &[(&str, &str)],
        field_name: &str,
        filename: &str,
        mime: Option<&str>,
        body: &[u8],
    ) -> Request<Body> {
        const B: &str = "engramtestboundary";
        let mut buf: Vec<u8> = Vec::new();
        for (k, v) in fields {
            buf.extend_from_slice(
                format!("--{B}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n").as_bytes(),
            );
        }
        let typed = match mime {
            Some(m) => format!("Content-Type: {m}\r\n"),
            None => String::new(),
        };
        buf.extend_from_slice(
            format!(
                "--{B}\r\nContent-Disposition: form-data; name=\"{field_name}\"; filename=\"{filename}\"\r\n{typed}\r\n"
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

    fn a_png() -> Vec<u8> {
        use image::{ImageBuffer, Rgb};
        let img = ImageBuffer::from_fn(24, 12, |x, _| Rgb([x as u8 * 10, 0, 0]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img).write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[tokio::test]
    async fn an_image_upload_is_accepted_with_its_note_and_queued() {
        let (app, token, core) = app_token_and_core().await;
        let res = app
            .clone()
            .oneshot(post_file_with(
                "/api/v1/corpora/image", &token,
                &[("note", "front of the router"), ("title_hint", "Router label")],
                "image", "IMG_9.png", Some("image/png"), &a_png(),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);
        let body = json_of(res).await;
        assert_eq!(body["status"], "describing");
        let id = body["id"].as_str().unwrap().to_string();
        let src = core.store.get_corpus(&id).await.unwrap();
        assert_eq!(src.title_hint.as_deref(), Some("Router label"));
        assert_eq!(src.metadata["note"], "front of the router");
        assert_eq!(src.origin, "image");

        // The same bytes again: 200, same id.
        let res = app
            .oneshot(post_file_with("/api/v1/corpora/image", &token, &[], "image", "IMG_9.png", Some("image/png"), &a_png()))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(json_of(res).await["id"], id);
    }

    #[tokio::test]
    async fn the_image_door_refuses_junk_missing_parts_and_a_closed_door() {
        let (app, token, core) = app_token_and_core().await;
        let res = app.clone()
            .oneshot(post_file_with("/api/v1/corpora/image", &token, &[], "image", "x.jpg", Some("image/jpeg"), b"not really"))
            .await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert!(json_of(res).await["error"].as_str().unwrap().contains("supported image"));

        let res = app.clone()
            .oneshot(post_file_with("/api/v1/corpora/image", &token, &[("note", "n")], "file", "x.png", Some("image/png"), &a_png()))
            .await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "wrong part name is 'no image in the upload'");
        assert!(core.store.list_corpora(10, 0).await.unwrap().is_empty());

        let (app, token, _) = app_from_core(crate::core::test_support::test_core_without_vision().await).await;
        let res = app
            .oneshot(post_file_with("/api/v1/corpora/image", &token, &[], "image", "x.png", Some("image/png"), &a_png()))
            .await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert!(json_of(res).await["error"].as_str().unwrap().contains("not configured"));
    }

    #[tokio::test]
    async fn the_image_door_has_its_own_larger_body_limit() {
        // Over the global 8 MB, under the image ceiling: the multipart parser
        // gets to see it, so the answer is the handler's (junk → 400), not the
        // framework's 413.
        let (app, token) = app_and_token().await;
        let big = vec![0u8; crate::web::MAX_BODY_BYTES + 1024];
        let res = app.clone()
            .oneshot(post_file_with("/api/v1/corpora/image", &token, &[], "image", "big.png", Some("image/png"), &big))
            .await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        // And the text door still stops at 8 MB.
        let res = app
            .oneshot(post_file("/api/v1/corpora/upload", &token, "big.txt", Some("text/plain"), &big))
            .await.unwrap();
        assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn the_preview_and_the_original_are_served() {
        let (app, token, _core) = app_token_and_core().await;
        let res = app.clone()
            .oneshot(post_file_with("/api/v1/corpora/image", &token, &[], "image", "p.png", Some("image/png"), &a_png()))
            .await.unwrap();
        let id = json_of(res).await["id"].as_str().unwrap().to_string();

        let res = app.clone().oneshot(get(&format!("/api/v1/corpora/{id}/image"), Some(&token))).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()["content-type"], "image/jpeg");
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 22).await.unwrap();
        assert!(image::load_from_memory(&bytes).is_ok());

        let res = app.clone().oneshot(get(&format!("/api/v1/corpora/{id}/image?original=1"), Some(&token))).await.unwrap();
        assert_eq!(res.headers()["content-type"], "image/png");
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 22).await.unwrap();
        assert_eq!(bytes.to_vec(), a_png());

        let res = app.clone().oneshot(get(&format!("/api/v1/corpora/{id}/image"), None)).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let res = app.oneshot(get("/api/v1/corpora/nope/image", Some(&token))).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_text_upload_records_its_note_and_file_facts() {
        let (app, token, core) = app_token_and_core().await;
        let res = app
            .oneshot(post_file_with("/api/v1/corpora/upload", &token, &[("note", "from the printer")], "file", "notes.txt", Some("text/plain"), b"hello there"))
            .await.unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let id = json_of(res).await["id"].as_str().unwrap().to_string();
        let m = core.store.get_corpus(&id).await.unwrap().metadata;
        assert_eq!(m["note"], "from the printer");
        assert_eq!(m["file"]["name"], "notes.txt");
        assert_eq!(m["file"]["size"], 11);
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test web::api::tests::the_image 2>&1 | grep -E "^error|test result" | head`
Expected: 404s / compile errors.

- [ ] **Step 3: Implement**

Restructure `upload` to collect parts first (a `note` may precede or follow the file):
```rust
async fn upload(
    State(st): State<AppState>,
    _id: Identity,
    mut multipart: axum::extract::Multipart,
) -> Result<(StatusCode, Json<crate::core::ingest::IngestOutcome>)> {
    let mut note: Option<String> = None;
    let mut file: Option<(Option<String>, String, axum::body::Bytes)> = None; // (filename, declared type, bytes)
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| Error::Validation(format!("malformed upload: {e}")))?
    {
        match field.name() {
            Some("note") => note = Some(field.text().await.map_err(|e| Error::Validation(format!("malformed upload: {e}")))?),
            Some("file") => {
                let filename = field.file_name().map(str::to_string);
                let declared = field.content_type().unwrap_or("").to_string();
                let bytes = field.bytes().await.map_err(|e| Error::Validation(format!("upload failed: {e}")))?;
                file = Some((filename, declared, bytes));
            }
            _ => {}
        }
    }
    let Some((filename, declared, bytes)) = file else {
        return Err(Error::Validation("no file in the upload".into()));
    };
    // (the existing type checks, verbatim, moved here — the `declared.is_empty()` /
    //  `named_txt` branch and the `starts_with("text/plain")` branch)
    let text = String::from_utf8(bytes.to_vec())
        .map_err(|_| Error::Validation("that file is not valid UTF-8 text".into()))?;
    let size = bytes.len();
    let out = st
        .core
        .ingest_capture(
            crate::core::ingest::Capture::new(text, ORIGIN_UPLOAD)
                .with_title(filename.clone())
                .with_note(note)
                .with_file(filename.as_deref(), size, "text/plain"),
        )
        .await?;
    let code = if out.duplicate { StatusCode::OK } else { StatusCode::CREATED };
    Ok((code, Json(out)))
}
```
Keep the existing comments about absent content types where the checks land. Then:
```rust
/// The image door. Parts: `image` (required), `title_hint`, `note`. The
/// bytes are validated and stored here; the reading happens in a job.
async fn upload_image(
    State(st): State<AppState>,
    _id: Identity,
    mut multipart: axum::extract::Multipart,
) -> Result<(StatusCode, Json<crate::core::ingest::IngestOutcome>)> {
    let mut note = None;
    let mut title_hint = None;
    let mut image: Option<(Option<String>, axum::body::Bytes)> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| Error::Validation(format!("malformed upload: {e}")))?
    {
        let text_of = |f: axum::extract::multipart::Field<'_>| async move {
            f.text().await.map_err(|e| Error::Validation(format!("malformed upload: {e}")))
        };
        match field.name() {
            Some("note") => note = Some(text_of(field).await?),
            Some("title_hint") => title_hint = Some(text_of(field).await?).filter(|t| !t.trim().is_empty()),
            Some("image") => {
                let filename = field.file_name().map(str::to_string);
                let bytes = field.bytes().await.map_err(|e| Error::Validation(format!("upload failed: {e}")))?;
                image = Some((filename, bytes));
            }
            _ => {}
        }
    }
    let Some((filename, bytes)) = image else {
        return Err(Error::Validation("no image in the upload".into()));
    };
    let out = st
        .core
        .ingest_image(crate::core::ingest::ImageCapture {
            bytes: bytes.to_vec(),
            filename,
            title_hint,
            note,
        })
        .await?;
    // 202, not 201: stored, but the reading — the part that makes it a corpus
    // anyone can search — is still queued.
    let code = if out.duplicate { StatusCode::OK } else { StatusCode::ACCEPTED };
    Ok((code, Json(out)))
}

#[derive(serde::Deserialize, Default)]
struct ImageQuery {
    #[serde(default)]
    original: Option<String>,
}

async fn get_image(
    State(st): State<AppState>,
    _id: Identity,
    Path(id): Path<String>,
    Query(q): Query<ImageQuery>,
) -> Result<axum::response::Response> {
    let want_original = q.original.as_deref().is_some_and(|v| v == "1" || v == "true");
    let found = if want_original {
        st.core.store.attachment_original(&id).await?
    } else {
        st.core.store.attachment_preview(&id).await?
    };
    let Some((mime, bytes)) = found else {
        return Err(Error::NotFound);
    };
    Ok((
        [(axum::http::header::CONTENT_TYPE, mime), (axum::http::header::CACHE_CONTROL, "private, max-age=3600".to_string())],
        bytes,
    )
        .into_response())
}
```
Router:
```rust
pub fn api_router(image_max_bytes: usize) -> Router<AppState> {
    Router::new()
        .route("/corpora", post(ingest).get(list_corpora))
        .route("/corpora/upload", post(upload))
        // Its own ceiling: a phone photo is several times the global limit.
        .route(
            "/corpora/image",
            post(upload_image).layer(axum::extract::DefaultBodyLimit::max(image_max_bytes)),
        )
        .route("/corpora/{id}", get(get_corpus).delete(delete_corpus))
        .route("/corpora/{id}/image", get(get_image))
        // ... rest unchanged
```
`src/web/mod.rs`: `.nest("/api/v1", api::api_router(state.core.capture.image_max_bytes))`. Grep for other `api_router()` callers (`grep -rn "api_router(" src`) and fix them. If the per-route limit does not override the outer one in the body-limit test, move `.layer(DefaultBodyLimit::max(MAX_BODY_BYTES))` in `web::router` **before** `.nest("/api/v1", ...)` is not the fix — the fix is `DefaultBodyLimit::disable()` on the image route followed by `axum::extract::RequestBodyLimitLayer`-free manual check: `tower_http::limit::RequestBodyLimitLayer::new(image_max_bytes)` from `tower-http` feature `limit`. Try the plain per-route `DefaultBodyLimit::max` first; axum documents that the innermost applies.

- [ ] **Step 4: Run and commit**

Run: `cargo test web:: 2>&1 | tail -5 && cargo clippy --all-targets -- -D warnings 2>&1 | tail -3`
```bash
git add src/web/api.rs src/web/mod.rs
git commit -m "feat(api): image upload and serving; note on text uploads"
```

---

### Task 8: UI — capture page inputs and the image corpus view

**Files:**
- Modify: `src/web/ui.rs` (`CaptureTemplate` ~line 297, `capture_page` ~line 501, `CorpusTemplate` ~line 362, `corpus_detail` ~line 770, queue label ~line 745)
- Modify: `src/web/templates/capture.html`
- Modify: `src/web/templates/corpus.html`
- Modify: `src/web/corpus_view.rs`

**Interfaces:**
- `CaptureTemplate { ..., vision_enabled: bool }`
- `CorpusTemplate { ..., image: bool, derived: bool, meta_rows: Vec<(String, String)>, note: Option<String> }`
- `corpus_view::for_corpus` returns `ImageTranscript` for `origin == "image"`, whose labels read `transcription lines a–b` / `transcription`.

- [ ] **Step 1: Failing tests**

`src/web/corpus_view.rs` tests:
```rust
    #[tokio::test]
    async fn an_image_corpus_labels_its_lines_as_transcription() {
        let s = crate::store::Store::memory().await.unwrap();
        let src = s.insert_image_corpus("h", "image", None, &serde_json::json!({})).await.unwrap().into_corpus();
        s.set_described_text(&src.id, "a\nb\nc", vec![]).await.unwrap();
        let src = s.get_corpus(&src.id).await.unwrap();
        let view = for_corpus(&src);
        assert_eq!(view.slice(&src, Some(&CorpusSpan { start_line: 2, end_line: 2 }), 0).label, "transcription lines 2–2");
        assert_eq!(view.slice(&src, None, 0).label, "transcription");
    }
```
`src/web/ui.rs` tests (find the module's app helper — it reuses `crate::web::api::tests::app_from_core`; follow the pattern of an existing page test that asserts on HTML, e.g. grep `capture_page` or `"/ui/capture"` in the tests):
```rust
    #[tokio::test]
    async fn the_capture_page_offers_images_only_when_vision_is_configured() {
        let (app, _t, _c) = crate::web::api::tests::app_from_core(crate::core::test_support::test_core().await).await;
        let html = page(app, "/ui/capture").await;   // use the module's existing helper for an authenticated GET
        assert!(html.contains("image/*"), "picker accepts images");
        assert!(html.contains("name=\"note\""), "the context field is there");

        let (app, _t, _c) = crate::web::api::tests::app_from_core(crate::core::test_support::test_core_without_vision().await).await;
        let html = page(app, "/ui/capture").await;
        assert!(!html.contains("image/*"));
        assert!(html.contains("accept=\".txt,text/plain\""));
    }

    #[tokio::test]
    async fn an_image_corpus_page_shows_the_photo_its_facts_and_the_reading_as_derived() {
        let core = crate::core::test_support::test_core().await;
        let src = core.store.insert_image_corpus("h", "image", Some("IMG.png"), &serde_json::json!({
            "note": "front porch",
            "file": {"name": "IMG.png", "width": 4, "height": 2},
            "exif": {"taken_at": "2026-08-09T14:12:03", "camera": "Pixel", "gps": {"lat": 1.5, "lon": 2.5}}
        })).await.unwrap().into_corpus();
        core.store.set_described_text(&src.id, "# Porch\n\nblue door", vec![]).await.unwrap();
        let (app, _t, _c) = crate::web::api::tests::app_from_core(core).await;
        let html = page(app, &format!("/ui/corpora/{}", src.id)).await;
        assert!(html.contains(&format!("/api/v1/corpora/{}/image", src.id)), "img src");
        assert!(html.contains("front porch"));
        assert!(html.contains("2026-08-09T14:12:03"));
        assert!(html.contains("1.5"));
        assert!(html.contains("Transcription"), "the text is labelled as derived, not 'Raw corpus'");
        assert!(html.contains("blue door"));
    }
```
If no `page(app, uri)` helper exists in `ui.rs` tests, add one: mint a token via `crate::auth::tokens::mint`, GET with `authorization: Bearer`, and read the body to a `String` (mirror `api::tests::get` + `to_bytes`).

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test web::ui::tests::the_capture_page web::corpus_view 2>&1 | grep -E "^error|panicked" | head`

- [ ] **Step 3: Implement**

`src/web/corpus_view.rs`:
```rust
/// An image corpus: the lines are the model's reading of the picture, and the
/// label says so, because a span into a transcription is a claim about what
/// the model wrote, not about what the photo shows.
pub struct ImageTranscript;

impl CorpusView for ImageTranscript {
    fn slice(&self, source: &Corpus, span: Option<&CorpusSpan>, context: usize) -> CorpusSlice {
        let mut s = TextLines.slice(source, span, context);
        s.label = match span {
            Some(sp) => format!("transcription lines {}–{}", sp.start_line, sp.end_line),
            None => "transcription".into(),
        };
        s
    }
}

pub fn for_corpus(source: &Corpus) -> Box<dyn CorpusView> {
    if source.origin == crate::core::ingest::ORIGIN_IMAGE {
        Box::new(ImageTranscript)
    } else {
        Box::new(TextLines)
    }
}
```
Update the module doc comment's "One implementation today" sentence.

`src/web/ui.rs`:
- `CaptureTemplate` gets `vision_enabled: bool`; `capture_page` sets `vision_enabled: st.core.describer.is_some()`.
- `CorpusTemplate` gets:
  ```rust
      /// An image corpus: the page shows the photo, and the lines below are the
      /// model's reading of it rather than the source itself.
      image: bool,
      /// Rows of what the door recorded about the capture, already formatted.
      meta_rows: Vec<(String, String)>,
      note: Option<String>,
  ```
- In `corpus_detail`, compute:
  ```rust
      let image = s.origin == crate::core::ingest::ORIGIN_IMAGE;
      let note = s.metadata["note"].as_str().map(str::to_string);
      let meta_rows = metadata_rows(&s.metadata);
  ```
  and add:
  ```rust
  /// The metadata worth a row on the corpus page, in reading order. Everything
  /// else the file carried is in the JSON, one API call away.
  fn metadata_rows(m: &serde_json::Value) -> Vec<(String, String)> {
      let mut rows = Vec::new();
      let exif = &m["exif"];
      if let Some(t) = exif["taken_at"].as_str() { rows.push(("Taken".into(), t.into())); }
      if let Some(c) = exif["camera"].as_str() { rows.push(("Camera".into(), c.into())); }
      if let (Some(lat), Some(lon)) = (exif["gps"]["lat"].as_f64(), exif["gps"]["lon"].as_f64()) {
          rows.push(("Location".into(), format!("{lat}, {lon}")));
      }
      let f = &m["file"];
      if let Some(n) = f["name"].as_str() { rows.push(("File".into(), n.into())); }
      if let (Some(w), Some(h)) = (f["width"].as_u64(), f["height"].as_u64()) {
          rows.push(("Size".into(), format!("{w}×{h}")));
      }
      if let Some(e) = m["describe"]["error"].as_str() { rows.push(("Reading".into(), e.into())); }
      rows
  }
  ```
- Queue label (~line 745): when `s.raw_text` is empty and `s.title_hint` is `None`, fall back to `"photo"` for image origin: `.unwrap_or_else(|| if s.origin == ORIGIN_IMAGE && s.raw_text.is_empty() { "photo".into() } else { markdown::snippet(&s.raw_text, 60) })`.

`src/web/templates/capture.html`: replace the drop label and its script with:
```html
  <label class="row muted" id="drop"
         style="border:1px dashed var(--line);padding:.6rem;border-radius:.4rem">
    {% if vision_enabled %}
    <input type="file" name="file" accept=".txt,text/plain,image/*" hidden>
    <span>…or drop a <code>.txt</code> file or an image here — on a phone, tap to take a photo.</span>
    {% else %}
    <input type="file" name="file" accept=".txt,text/plain" hidden>
    <span>…or drop a <code>.txt</code> file here.</span>
    {% endif %}
  </label>
  {# Context for whatever file comes next: a sentence about what it is. Sent
     with the file, then cleared. For an image the vision model reads it too. #}
  <input class="input" type="text" name="note" maxlength="2000"
         placeholder="Add context for the file (optional) — what is it, why keep it?">
```
and the script's `send`:
```js
    var noteBox = document.querySelector('input[name="note"]');
    var VISION = {% if vision_enabled %}true{% else %}false{% endif %};
    function send(file) {
      if (!file) return;
      var isImage = file.type.indexOf('image/') === 0;
      var result = document.getElementById('capture-result');
      if (isImage && !VISION) {
        result.textContent = 'Image capture is not configured on this server.';
        return;
      }
      var payload = new FormData();
      if (noteBox && noteBox.value.trim()) payload.append('note', noteBox.value.trim());
      payload.append(isImage ? 'image' : 'file', file, file.name || (isImage ? 'photo.jpg' : 'paste.txt'));
      var url = isImage ? '/api/v1/corpora/image' : '/api/v1/corpora/upload';
      fetch(url, { method: 'POST', body: payload })
        .then(function (r) {
          return r.json()
            .catch(function () { return { error: 'engram answered ' + r.status + '.' }; })
            .then(function (j) { return [r.ok, j]; });
        })
        .catch(function () { return [false, { error: 'engram is unreachable.' }]; })
        .then(function (pair) {
          result.textContent = pair[0]
            ? (isImage ? 'Captured — the photo is queued to be read.' : 'Captured.')
            : (pair[1].error || 'Upload failed.');
          if (pair[0]) { if (noteBox) noteBox.value = ''; htmx.trigger(document.body, 'captured'); }
        });
    }
    if (drop) {
      picker.addEventListener('change', function () { send(picker.files[0]); });
      drop.addEventListener('dragover', function (e) { e.preventDefault(); });
      drop.addEventListener('drop', function (e) { e.preventDefault(); send(e.dataTransfer.files[0]); });
    }
    // A pasted screenshot goes the same way as a dropped one.
    document.addEventListener('paste', function (e) {
      var items = (e.clipboardData && e.clipboardData.items) || [];
      for (var i = 0; i < items.length; i++) {
        if (items[i].kind === 'file' && items[i].type.indexOf('image/') === 0) {
          e.preventDefault();
          send(items[i].getAsFile());
          return;
        }
      }
    });
```
Keep the existing comments about the 8 MB reply not being JSON and the same-origin cookie. Note the surrounding `<form>` posts urlencoded text; the `note` input is inside it — give it `form="none"`-equivalent by placing it **outside** the `<form>` (right after `</form>`, before `#capture-result`) so a text paste does not send it. Adjust the markup accordingly.

`src/web/templates/corpus.html`: after the `source_url` block add:
```html
{% if image %}
<div class="card">
  <div class="card-head"><span class="card-title">Photo</span>
    <span class="spacer"></span>
    <a class="quiet-link" href="/api/v1/corpora/{{ id }}/image?original=1">original</a>
  </div>
  <a href="/api/v1/corpora/{{ id }}/image?original=1">
    <img src="/api/v1/corpora/{{ id }}/image" alt="captured image" style="max-width:100%;height:auto;border-radius:.4rem">
  </a>
  {% if let Some(n) = note %}<p><b>Note:</b> {{ n }}</p>{% endif %}
  {% if !meta_rows.is_empty() %}
  <table class="meta">
    {% for r in meta_rows %}<tr><td class="muted">{{ r.0 }}</td><td>{{ r.1 }}</td></tr>{% endfor %}
  </table>
  {% endif %}
</div>
{% else if let Some(n) = note %}
<p><b>Note:</b> {{ n }}</p>
{% endif %}
```
and change the raw-corpus card title to:
```html
    <span class="card-title">{% if restored %}Restored artifacts{% else if image %}Transcription{% else %}Raw corpus{% endif %}</span>
    {% if image %}<span class="muted">— the model's reading of the photo, not the source itself</span>{% endif %}
```
When `image` and `lines` is empty (still describing), show `<p class="muted">Not read yet — the photo is queued for the vision model.</p>` inside the card instead of the empty table.

- [ ] **Step 4: Run, eyeball, commit**

Run: `cargo test web:: 2>&1 | tail -5 && cargo clippy --all-targets -- -D warnings 2>&1 | tail -3`
Optionally run the server with a `[infer.vision]` block against a local model and drop a photo on `/ui/capture`; watch the job log for `image read; queued for synthesis`.
```bash
git add src/web
git commit -m "feat(ui): image capture from picker, camera, drop and paste; image corpus page"
```

---

### Task 9: Docs and roadmap

**Files:**
- Modify: `README.md` (capture doors section), `ROADMAP.md`

- [ ] **Step 1: README** — in the section listing capture doors, add one bullet: images (JPEG/PNG/WebP) via the capture page, phone camera through the PWA, or `POST /api/v1/corpora/image` (multipart `image`, optional `note`, `title_hint`); requires `[infer.vision]`; the original is kept and served at `GET /api/v1/corpora/{id}/image?original=1`.
- [ ] **Step 2: ROADMAP** — mark image capture done under the "File upload, then PDF" line and note that `attachments` + `CorpusView` are where PDF slots in.
- [ ] **Step 3: Commit**
```bash
git add README.md ROADMAP.md
git commit -m "docs: image capture door"
```

---

## Self-review against the spec

- §1 data model — Task 2 (table, column, statuses, hash) ✔; `exif.tags` "everything the file carries" — Task 3 ✔.
- §2 flow/pipeline — Task 5 (door, no inference), Task 6 (stage, empty → parked with reason, near-dupe → parked, retry via error) ✔.
- §2 system prompt — Task 4 ✔.
- §3 config three modes, probe, redaction, capture keys — Tasks 1, 4 ✔.
- §4 UI: picker/camera/drop/paste, context field, disabled state — Task 8 ✔. Offline: unchanged ✔.
- §5 API endpoints, per-route limit, `note` on `.txt` upload, image serving, detail pane, ops (nothing needed) — Tasks 7, 8 ✔.
- §6 note table — Tasks 5, 6, 7 ✔ (text synthesis does not read the note).
- §7 tests — each task carries its own; end-to-end in Task 6.
- Type consistency: `insert_image_corpus(hash, origin, title_hint, &metadata)`, `set_described_text(id, text, Vec<u64>)`, `attachment_preview -> Option<(String, Vec<u8>)>`, `FakeDescriber::{saying,failing,calls,last_context}`, `test_core_with_describer(Arc<FakeDescriber>)`, `test_core_without_vision()`, `api_router(usize)` — used identically across tasks.
