# Image capture — design

Date: 2026-08-15
Status: approved in brainstorm, awaiting implementation plan

## Goal

Capture photos and images (drag-and-drop, file picker, phone camera via the
PWA, clipboard paste) as first-class corpora. A vision model reads the image
into markdown, which then flows through the existing text pipeline untouched.
The original image is retained as the verbatim source. Image metadata (file
facts + full EXIF including GPS) is recorded, and the metadata column is
generic so other doors can use it. Every file-type door additionally accepts a
free-text `note` giving user context for the upload.

## Decisions taken in brainstorm

| Question | Decision |
|---|---|
| Fate of the image | Kept permanently as the corpus source; transcription is derived |
| What the model produces | Mixed: transcribe visible text faithfully as markdown, describe diagrams/scenes/objects, capture everything worth preserving |
| Metadata | Everything: file facts + all EXIF incl. GPS |
| Metadata scope | Generic `corpora.metadata` JSON column now; images populate it; other doors opportunistically, no retrofit |
| Metadata use | Compact summary is fed to the vision model so dates/places become searchable; raw metadata stored on the corpus |
| Where the vision call happens | New background job stage `Describe`, never in the request path (capture stays instant, endpoint-independent) |
| EXIF / resize | Server-side from the original upload; per-route body limit raised |
| User context | Optional `note` on all file-type doors; stored in metadata; for images fed to the vision model |

Rejected: synchronous vision call at the door (breaks the "capture makes no
inference call" rule; a phone upload would block on GPU latency and fail when
the endpoint is down).

## 1. Data model

### `attachments` (new table)

```sql
CREATE TABLE IF NOT EXISTS attachments (
  id         INTEGER PRIMARY KEY,
  corpus_id  INTEGER NOT NULL REFERENCES corpora(id) ON DELETE CASCADE,
  kind       TEXT    NOT NULL,          -- 'image'
  mime       TEXT    NOT NULL,          -- of the original
  filename   TEXT,                      -- original filename if supplied
  bytes      BLOB    NOT NULL,          -- original upload, untouched
  preview    BLOB    NOT NULL,          -- derived JPEG, orientation applied, ~2048px long edge
  width      INTEGER, height INTEGER,   -- of the original, post-orientation
  created_at TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS attachments_corpus ON attachments(corpus_id);
```

Original bytes are the verbatim source and are never re-encoded. The preview
is derived once at upload (decode with `image`, apply EXIF orientation,
downscale, encode JPEG) and is what goes to the model and the UI. Storage stays
in SQLite so backup/deploy remains one file. Supported inputs: JPEG, PNG, WebP
(iOS transcodes HEIC to JPEG on web upload; no libheif).

### `corpora.metadata` (new column)

Appended via `ADDED_COLUMNS`: `metadata TEXT NOT NULL DEFAULT '{}'`.
Namespaced JSON; namespaces never collide across doors:

```json
{
  "note": "whiteboard from Tuesday planning",
  "file": { "name": "IMG_2041.jpeg", "size": 4812733, "mime": "image/jpeg",
            "width": 4032, "height": 3024 },
  "exif": { "taken_at": "2026-08-09T14:12:03", "camera": "Apple iPhone 15",
            "orientation": 6, "gps": { "lat": 48.2082, "lon": 16.3738, "alt": 171.0 } }
}
```

- `note` — user-supplied context, any door. Plain string, trimmed, capped
  (e.g. 2000 chars).
- `file` — for any file upload. The existing `.txt` door starts writing
  `file.name/size/mime` since it has them in hand. No other retrofit.
- `exif` — images only. Parsed with `kamadak-exif` from the original. Fields
  present only when the tag exists. `taken_at` from DateTimeOriginal (naive
  local time as written; offset added if OffsetTimeOriginal exists).
  Everything the file carries is kept, GPS included — deliberate choice for a
  personal knowledge base.

### `content_hash`

For image corpora: SHA-256 of the *original* bytes. Re-uploading the same
photo dedupes at the door before any inference. Text-level near duplicates
(two photos of the same page) are caught by the existing shingle check once
the transcription exists (see §2 step 3).

### Corpus status

New status `describing`, preceding `raw`. Only image-backed corpora ever hold
it. `raw_text` is `''` while describing (column stays NOT NULL).

Origin value for the new door: `image`.

## 2. Capture flow and pipeline

```
POST image ──► validate mime/size ─► decode ─► EXIF ─► preview
           ──► insert corpus {origin:'image', status:'describing', raw_text:'',
                              content_hash: sha256(original), metadata}
           ──► insert attachment ─► enqueue Stage::Describe ─► 202
```

No inference in the request path.

### `Stage::Describe` (new job stage)

Dispatched from `jobs::mod::run_claimed` like every stage; inference passes
through the shared `InferenceGate`.

1. Load corpus, attachment preview, metadata.
2. Build the context line from metadata: user `note` first (verbatim, labeled
   as user-provided context), then the compact facts —
   `Photo taken 2026-08-09 14:12, GPS 48.2082,16.3738, Apple iPhone 15,
   file IMG_2041.jpeg`. Omit whatever is absent.
3. Call `Describer::describe(preview_jpeg, context)`.
4. Result handling:
   - Non-empty markdown → write `raw_text`, compute shingles, run the existing
     near-duplicate check (near-dupe → `needs_review`, same as text captures),
     else status `raw`, enqueue `Synthesize`.
   - Empty/whitespace → park as `needs_review` with a `flag_detail`-style
     reason on the corpus so ops shows why. Image remains viewable.
   - Transport/model error → job error, existing retry with backoff. Corpus
     stays visibly in `describing`. Nothing is lost.

From `raw` onward the pipeline (`Synthesize → SegmentWindow → Title → Embed →
Relate/Dedupe/Consolidate`) is untouched; it sees a text corpus. Coverage,
spans and artifacts work against the transcription.

### System prompt

`DESCRIBE_SYSTEM` in `src/infer/prompt.rs`. Intent: read the image the way a
careful archivist would. Transcribe any visible text faithfully, preserving
structure (headings, lists, tables) as markdown. Describe diagrams, charts,
scenes and objects with everything worth preserving; name entities that are
identifiable. Do not pad, do not speculate beyond what is visible, do not
summarize away specifics. Where the provided capture context (user note,
date, place, device) is relevant, incorporate it naturally so it can be
recalled later; do not repeat it mechanically. Output markdown only, no
preamble.

## 3. Config and inference layer

```toml
[infer.vision]              # section absent → image capture disabled
model = "qwen2.5-vl-7b"     # required when present
base_url = "http://..."     # optional; absent → inherit [infer.synthesize].base_url
api_key = "..."             # optional; absent → inherit [infer.synthesize].api_key
timeout_secs = 120          # default 120
```

This maps the three requested modes onto the existing optional-role pattern
(`rerank`):

- disabled — section absent; `POST .../image` returns 4xx `image capture is
  not configured`; capture UI renders without image affordances.
- main LLM — section present with `model` only; endpoint/key inherited.
- dedicated — section present with its own `base_url`/`api_key`.

Startup probe warns (never fails) like other roles. `--print-config` redacts
`vision.api_key`. Env override works as everywhere (`ENGRAM__INFER__VISION__MODEL`).

New `[capture]` keys: `image_max_bytes` (default 25 MiB, per-route body
limit), `image_preview_edge` (default 2048).

### Inference

New trait in `src/infer/mod.rs`:

```rust
pub trait Describer: Send + Sync {
    fn describe(&self, image_jpeg: &[u8], context: &str)
        -> impl Future<Output = Result<String>> + Send;
}
```

`HttpDescriber` in `src/infer/openai.rs` builds the OpenAI content-array
form: system = `DESCRIBE_SYSTEM`; user content =
`[{type:"text", text: context}, {type:"image_url", image_url:{url:"data:image/jpeg;base64,…"}}]`.
Reads `choices[0].message.content`. Existing string-only traits are untouched.
`FakeDescriber` in `src/infer/fake.rs` (scripted responses/errors) like every
other trait. `Core` gains `describer: Option<Arc<dyn Describer>>`, wired in
`Core::from_config` and `test_support::build`.

## 4. Capture UI and PWA

`capture.html`'s `#drop` zone becomes the single entry for all files, with
one JS handler branching on file type:

- File picker / camera: hidden input `accept=".txt,text/plain,image/*"`. No
  forced `capture=` attribute — the OS sheet offers both camera and gallery.
  PWA start URL is already the capture page: open → tap → shoot → done.
- Drag-and-drop of image files (desktop).
- Clipboard `paste` of image data (screenshots).
- **Context field**: one optional textarea "Add context (optional)" next to
  the drop zone. Its value is sent as `note` with whichever file is dropped,
  picked or pasted next, then cleared. Same control serves `.txt` and images.

Text files → existing `/api/v1/corpora/upload` (now also carrying `note`);
images → the new image endpoint. Success shows the existing "captured"
confirmation plus a hint that the photo is queued for reading. When vision is
disabled, the template omits image affordances (flag from `Core`) and the
handler rejects images with the clear message.

Service worker unchanged: no offline queueing (documented stance). Photos
taken offline fail visibly.

## 5. API and detail pane

- `POST /api/v1/corpora/image` — multipart: `image` (required),
  `title_hint`, `note` (optional). Per-route `DefaultBodyLimit` of
  `capture.image_max_bytes`; the global 8 MiB stays for everything else.
  Refusals, each distinct and tested: unsupported mime, undecodable image,
  too large, vision not configured. Filename → `title_hint` fallback. Returns
  the same shape as other capture doors. Origin `image`.
- `POST /api/v1/corpora/upload` — gains optional `note`; writes
  `metadata.note` and `metadata.file`.
- `GET /api/v1/corpora/{id}/image` — serves the preview JPEG;
  `?original=1` serves the original bytes with its stored mime. Auth like all
  routes.
- Detail pane: a `CorpusView` implementation for image-backed corpora (the
  extension point ROADMAP reserves for non-text sources). Renders the photo,
  the metadata (note, taken-at, camera, location, dimensions), and the
  transcription labeled *derived — model reading of the image*. Line-based
  span highlighting continues to work against the transcription.
- Markdown sanitizer: unchanged. The image is rendered by the view template
  from the GET route, not embedded in markdown.
- Ops: nothing new; `describing` corpora and stuck `Describe` jobs appear via
  the existing jobs/retry machinery.

## 6. Note handling across doors (this feature's scope)

| Door | `note` stored | `note` consumed |
|---|---|---|
| image | yes | fed to `Describe` as user context |
| `.txt` upload | yes | displayed only |
| paste / JSON / MCP / extension | not in scope | — |

Feeding `note` into text synthesis would touch the untouched pipeline and is
explicitly a follow-up.

## 7. Testing

- Unit: EXIF extraction (GPS, DateTimeOriginal, orientation) and preview
  generation on small fixture images (JPEG with tags, PNG without); metadata
  JSON shape; context-line builder incl. note ordering and omission of absent
  fields; `Describer` request body shape.
- Endpoint: the four refusals; successful upload creates corpus + attachment
  + `Describe` job and returns 202; exact re-upload dedupes at the door;
  `note` and `file` land in metadata for both image and `.txt` doors;
  per-route body limit while global limit test still holds.
- Pipeline: with `FakeDescriber`, image upload → `describing → raw → … →
  ready` with embedded artifacts; erroring fake leaves corpus in `describing`
  with job retried; empty fake parks as `needs_review`; near-dupe transcription
  parks as `needs_review`.
- Config: absent / inherit / dedicated resolve correctly; redaction.
- UI: template hides image affordances when disabled; manifest/sw tests
  unchanged.

## Dependencies

`image` (decode/resize/encode; features limited to jpeg/png/webp),
`kamadak-exif`. Pure Rust, no system libraries.

## Out of scope

Offline queueing; HEIC decoding; PDF; feeding `note` to text synthesis;
re-describing an image with a different model (the retained original makes
this a later feature, not a schema change).
