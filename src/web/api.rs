use crate::core::search::SearchQuery;
use crate::error::{Error, Result};
use crate::store::jobs::{FailedJob, Stage};
use crate::tenants::Tenant;
use crate::web::state::AppState;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};

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
    /// `"selection"` when `html` is a fragment the operator highlighted rather
    /// than a whole page. It exempts the capture from the extraction floor,
    /// which is a guess about whole pages: three sentences deliberately picked
    /// out are not a login wall, and refusing them for being short refuses a
    /// capture that was asked for in as many words.
    #[serde(default)]
    pub scope: Option<String>,
}

/// Whether this request may skip the extraction floor.
///
/// Only where there is a fragment to be exempt. A server-side GET has no
/// selection in it: `scope` there is a claim made by a client that never
/// looked at the page, and honouring it on the `url` path would let anyone
/// switch off the one guard that catches a login wall — which is the whole
/// reason the floor exists. So the exemption is tied to `html` having arrived
/// alongside it.
fn floor_exempt(req: &IngestRequest) -> bool {
    req.html.is_some() && req.scope.as_deref() == Some("selection")
}

#[derive(serde::Deserialize)]
pub struct ReprocessRequest {
    #[serde(default = "default_stage")]
    pub stage: String,
}
fn default_stage() -> String {
    "segment".into()
}

/// Every field is optional so a caller can correct a tag without resending —
/// and without re-embedding — the body text.
///
/// `title` and `category` are doubly optional on purpose: an absent key means
/// "leave it alone" and an explicit `null` means "clear it". Collapsing the two
/// would make a field that can be set but never unset. Tags need no such
/// distinction, because an empty list already says it.
#[derive(serde::Deserialize)]
pub struct PatchArtifactRequest {
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default, deserialize_with = "explicit_null")]
    pub title: Option<Option<String>>,
    #[serde(default, deserialize_with = "explicit_null")]
    pub category: Option<Option<String>>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

/// Tell an absent key from an explicit `null`. Serde reaches this function only
/// when the key was present, so the outer `Some` records that fact.
fn explicit_null<'de, D, T>(d: D) -> std::result::Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    serde::Deserialize::deserialize(d).map(Some)
}

/// A query-string flag, in the spellings a query string actually carries.
///
/// `serde_urlencoded` takes only `true` and `false`, so `?explain=1` — the
/// spelling in every hand-written curl and every doc example — would be a 400
/// rather than a flag. An empty value (`?explain`) is on, because writing the
/// key at all is the request.
pub(crate) fn query_flag<'de, D>(d: D) -> std::result::Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = <Option<String> as serde::Deserialize>::deserialize(d)?;
    Ok(raw.map(|v| matches!(v.trim(), "" | "1" | "true" | "yes" | "on")))
}

/// A chunk may carry this many tags, each this long.
///
/// Tags are a filter dimension and a payload index in Qdrant, not a place to
/// put prose. Unbounded input here becomes unbounded payload on every point
/// and an index that grows without limit.
const MAX_TAGS: usize = 32;
const MAX_TAG_LEN: usize = 64;
/// Long enough for any label worth filtering on.
const MAX_CATEGORY_LEN: usize = 64;
const MAX_TITLE_LEN: usize = 512;

/// Trim, drop blanks, deduplicate, and refuse what is out of bounds.
///
/// Deduplicating matters beyond tidiness: tags are ANDed in a search filter, so
/// a repeated tag is a condition Qdrant evaluates twice for the same answer.
fn clean_tags(tags: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::with_capacity(tags.len());
    for t in tags {
        let t = t.trim();
        if t.is_empty() {
            continue;
        }
        if t.chars().count() > MAX_TAG_LEN {
            return Err(Error::Validation(format!(
                "tag is longer than {MAX_TAG_LEN} characters"
            )));
        }
        if !out.iter().any(|k| k == t) {
            out.push(t.to_string());
        }
    }
    if out.len() > MAX_TAGS {
        return Err(Error::Validation(format!(
            "a chunk may carry at most {MAX_TAGS} tags, got {}",
            out.len()
        )));
    }
    Ok(out)
}

/// Trim a settable-or-clearable string field. An empty value after trimming is
/// a clear, so `""` and `null` mean the same thing rather than storing a label
/// that renders as nothing.
fn clean_optional(value: Option<String>, max: usize, field: &str) -> Result<Option<String>> {
    let Some(v) = value else {
        return Ok(None);
    };
    let v = v.trim();
    if v.is_empty() {
        return Ok(None);
    }
    if v.chars().count() > max {
        return Err(Error::Validation(format!(
            "{field} is longer than {max} characters"
        )));
    }
    Ok(Some(v.to_string()))
}

#[derive(serde::Serialize)]
pub struct StatusResponse {
    pub sources: Vec<(String, i64)>,
    pub jobs: Vec<(String, i64)>,
    pub failed: Vec<FailedJob>,
    pub oldest_pending_secs: Option<i64>,
    pub chunks: i64,
    pub vectors: u64,
}

/// Capture channels. `origin` is derived from which field arrived, not
/// hardcoded: it is the only record of how a document got here.
use crate::core::ingest::ORIGIN_WEB;
const ORIGIN_EXTENSION: &str = "extension";

/// Readability and the markdown conversion, off the async worker.
///
/// Both are synchronous walks of a DOM that can be megabytes — `fetch_max_bytes`
/// and the request body limit are both 8 MB — and run inline they hold a Tokio
/// worker for long enough to stall whatever else that thread was serving.
/// `Readability` is `!Send`, which is why this could not be awaited across;
/// inside a `spawn_blocking` closure it is created and dropped without ever
/// crossing an await, and only the owned `String` has to move.
use crate::core::extract::extract;

async fn ingest(
    tenant: Tenant,
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
        Some(raw) => {
            let u = url::Url::parse(raw).map_err(|e| Error::Validation(format!("url: {e}")))?;
            // `Url::parse` accepts `javascript:` and `data:` happily, and the
            // scheme allowlist lives in `fetch_html` — which the `html` plus
            // `url` path never calls. This value is stored and rendered as a
            // link on the corpus page, so the check belongs here too.
            if !matches!(u.scheme(), "http" | "https") {
                return Err(Error::Validation(format!(
                    "url: `{}` is not a scheme a page is read over",
                    u.scheme()
                )));
            }
            Some(u)
        }
        None => None,
    };

    // A highlighted fragment is exempt from the floor. See `floor_exempt`.
    let floor = if floor_exempt(&req) {
        0
    } else {
        tenant.core.capture.min_extracted_chars
    };

    let (text, origin) = if let Some(text) = req.text {
        (text, ORIGIN_WEB)
    } else if let Some(html) = req.html {
        (
            extract(html, parsed_url.clone(), floor).await?,
            ORIGIN_EXTENSION,
        )
    } else {
        // A link may hold a page, a PDF or an image; the core decides which,
        // the same way it does for the MCP door. 202 when what it stored is
        // still to be read, as the upload doors answer.
        let u = parsed_url.as_ref().expect("one-of check guarantees a url");
        let out = tenant.core.ingest_url(u, req.title, None).await?;
        let code = match (&out.status, out.duplicate) {
            (_, true) => StatusCode::OK,
            (
                crate::store::corpora::CorpusStatus::Extracting
                | crate::store::corpora::CorpusStatus::Describing,
                _,
            ) => StatusCode::ACCEPTED,
            _ => StatusCode::CREATED,
        };
        return Ok((code, Json(out)));
    };

    let out = tenant
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
pub(crate) fn only_a_url(body: &str) -> Option<url::Url> {
    let trimmed = body.trim();
    if trimmed.split_whitespace().count() != 1 {
        return None;
    }
    let u = url::Url::parse(trimmed).ok()?;
    matches!(u.scheme(), "http" | "https").then_some(u)
}

/// The code a stored capture answers with, in the one place the doors that now
/// need it can share: `200` for something already held, `202` while what was
/// stored is still to be read, `201` for a capture that is complete.
pub(crate) fn code_for(out: &crate::core::ingest::IngestOutcome) -> StatusCode {
    if out.duplicate {
        StatusCode::OK
    } else if matches!(
        out.status,
        crate::store::corpora::CorpusStatus::Extracting
            | crate::store::corpora::CorpusStatus::Describing
    ) {
        StatusCode::ACCEPTED
    } else {
        StatusCode::CREATED
    }
}

/// Every part of a capture upload: the text fields by name, and every file
/// part in the order it arrived.
///
/// `read_upload` cannot serve this: it takes exactly one file and refuses a
/// second, which is right for a door that stores one document and wrong for a
/// share sheet handing over four photos at once. A part counts as a file when
/// it carried a filename, which is what a browser and a platform share sheet
/// both do and what a plain text field never does.
pub(crate) async fn read_capture_parts(
    mut multipart: axum::extract::Multipart,
) -> Result<(std::collections::HashMap<String, String>, Vec<FilePart>)> {
    let mut fields: std::collections::HashMap<String, String> = Default::default();
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
            files.push(FilePart {
                filename,
                declared,
                bytes,
            });
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

/// One capture or several, in the shape a client can parse without a flag: an
/// object for a body that held one thing, an array for a share that held more.
/// Untagged, so neither is wrapped in a name the other lacks.
#[derive(serde::Serialize)]
#[serde(untagged)]
pub(crate) enum Captured {
    One(crate::core::ingest::IngestOutcome),
    Many(Vec<crate::core::ingest::IngestOutcome>),
}

/// One door for a client that has not classified what it is holding.
///
/// A share sheet hands over a blob and a maybe-URL; a shell hands over a path
/// or a pipe. Written once here, that dispatch is thirty lines; written once
/// per client, it is the reason the clients never get written. Every branch
/// ends in an ingest call the other doors already use, and `POST /corpora`
/// stays exactly as it was for a caller that does know what it holds.
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
        // Refused rather than lossily converted, for the reason the upload
        // door refuses it: a corpus is quoted back verbatim, so text that
        // arrived mangled is a fidelity loss nothing downstream can detect.
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

    if content_type.starts_with("multipart/form-data") {
        let m = axum::extract::Multipart::from_request(req, &())
            .await
            .map_err(|e| Error::Validation(format!("malformed upload: {e}")))?;
        let (mut fields, files) = read_capture_parts(m).await?;
        let title = q.title.or_else(|| fields.remove("title"));
        let note = q.note.or_else(|| fields.remove("note"));
        let mut out: Vec<crate::core::ingest::IngestOutcome> = Vec::new();

        // A share sheet sends `url` and `text` for the same share, and the
        // link is the better capture of the two: the text is usually the
        // page's title repeated. So a `url` wins and takes the text with it,
        // and the text is kept only where it stands alone.
        let shared_url = fields.remove("url").or_else(|| {
            fields
                .get("text")
                .and_then(|t| only_a_url(t).map(|u| u.to_string()))
        });
        if let Some(raw) = shared_url {
            fields.remove("text");
            let u = url::Url::parse(&raw).map_err(|e| Error::Validation(format!("url: {e}")))?;
            if !matches!(u.scheme(), "http" | "https") {
                return Err(Error::Validation(format!(
                    "url: `{}` is not a scheme a page is read over",
                    u.scheme()
                )));
            }
            out.push(
                tenant
                    .core
                    .ingest_url(&u, title.clone(), note.clone())
                    .await?,
            );
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
                    .ingest_file(
                        f.bytes.to_vec(),
                        f.filename,
                        title.clone(),
                        note.clone(),
                        ORIGIN_WEB,
                    )
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

    Err(Error::Validation(format!(
        "`{content_type}` is not a type this door reads — send text/plain, \
         application/pdf, an image, or multipart/form-data"
    )))
}

const ORIGIN_UPLOAD: &str = "upload";

/// Whether an upload's filename claims to be a PDF. Consulted on the same
/// terms as `named_txt`: only when the part carried no `Content-Type`.
fn named_pdf(filename: Option<&str>) -> bool {
    filename.is_some_and(|n| {
        std::path::Path::new(n)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
    })
}

/// Whether an upload's filename claims to be a text file.
///
/// Only consulted when the part carried no `Content-Type`. Case-insensitive,
/// because a browser on a case-preserving filesystem will happily send
/// `NOTES.TXT`, and refusing that would be a rule about shouting.
fn named_txt(filename: Option<&str>) -> bool {
    filename.is_some_and(|n| {
        std::path::Path::new(n)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("txt"))
    })
}

/// One file part of a multipart upload.
pub(crate) struct FilePart {
    pub(crate) filename: Option<String>,
    /// The part's `Content-Type`, or "" when it carried none.
    pub(crate) declared: String,
    pub(crate) bytes: axum::body::Bytes,
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
) -> Result<UploadParts> {
    let mut out = UploadParts {
        file: None,
        fields: Default::default(),
    };
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| Error::Validation(format!("malformed upload: {e}")))?
    {
        let Some(name) = field.name().map(str::to_string) else {
            continue;
        };
        if name == file_field {
            if out.file.is_some() {
                return Err(Error::Validation(format!(
                    "more than one `{file_field}` part — send one file per request"
                )));
            }
            let filename = field.file_name().map(str::to_string);
            let declared = field.content_type().unwrap_or("").to_string();
            let bytes = field
                .bytes()
                .await
                .map_err(|e| Error::Validation(format!("upload failed: {e}")))?;
            out.file = Some(FilePart {
                filename,
                declared,
                bytes,
            });
        } else if let Some(key) = text_fields.iter().copied().find(|k| *k == name) {
            let text = field
                .text()
                .await
                .map_err(|e| Error::Validation(format!("malformed upload: {e}")))?;
            if !text.trim().is_empty() {
                out.fields.insert(key, text);
            }
        }
    }
    Ok(out)
}

/// `.txt` and PDF, and nothing else. Refusing everything else by name is what
/// keeps this one from quietly ingesting the bytes of a format it cannot read.
///
/// A PDF is stored and queued; the reading happens in `Stage::Extract`.
async fn upload(
    tenant: Tenant,
    multipart: axum::extract::Multipart,
) -> Result<(StatusCode, Json<crate::core::ingest::IngestOutcome>)> {
    let parts = read_upload(multipart, "file", &["note"]).await?;
    let Some(FilePart {
        filename,
        declared,
        bytes,
    }) = parts.file
    else {
        return Err(Error::Validation("no file in the upload".into()));
    };
    let note = parts.fields.get("note").cloned();
    // A part may legally carry no `Content-Type` at all, and letting that
    // skip the check turns "`.txt` and nothing else" into "anything whose
    // bytes happen to be UTF-8" — a `.csv`, a `.json`, a page of HTML.
    // An absent type is not a pass; it just moves the question to the
    // name, which is the only other thing the sender told us.
    let kind = if declared.is_empty() {
        if named_txt(filename.as_deref()) {
            Kind::Text
        } else if named_pdf(filename.as_deref()) {
            Kind::Pdf
        } else {
            return Err(Error::Validation(
                "that upload declares no type and is named neither `.txt` nor \
                 `.pdf` — only text/plain and application/pdf are accepted"
                    .into(),
            ));
        }
    } else if declared.starts_with("text/plain") {
        Kind::Text
    } else if declared.starts_with("application/pdf") {
        Kind::Pdf
    } else {
        return Err(Error::Validation(format!(
            "that file is `{declared}` — only text/plain and application/pdf are accepted"
        )));
    };

    match kind {
        Kind::Text => {
            // The route's ceiling is the PDF one, which is many times the
            // global limit. Text does not get to ride on that: a paste is
            // bounded by `MAX_BODY_BYTES` however it arrives, and widening the
            // door for one format must not widen it for the other.
            if bytes.len() > crate::web::MAX_BODY_BYTES {
                return Err(Error::Validation(
                    "that text file is over the 8 MB limit for a text capture".into(),
                ));
            }
            // Refused rather than lossily converted: a corpus is quoted back
            // verbatim, so text that arrived mangled would be a fidelity loss
            // nothing downstream could detect.
            let text = String::from_utf8(bytes.to_vec())
                .map_err(|_| Error::Validation("that file is not valid UTF-8 text".into()))?;
            let size = bytes.len();

            let out = tenant
                .core
                .ingest_capture(
                    crate::core::ingest::Capture::new(text, ORIGIN_UPLOAD)
                        .with_note(note)
                        .with_file(filename.as_deref(), size, "text/plain"),
                )
                .await?;
            let code = if out.duplicate {
                StatusCode::OK
            } else {
                StatusCode::CREATED
            };
            Ok((code, Json(out)))
        }
        Kind::Pdf => {
            let out = tenant
                .core
                .ingest_pdf(crate::core::ingest::PdfCapture {
                    bytes: bytes.to_vec(),
                    filename,
                    title_hint: None,
                    note,
                })
                .await?;
            // 202, not 201: stored, but the extraction — the part that makes it
            // a corpus anyone can search — is still queued.
            let code = if out.duplicate {
                StatusCode::OK
            } else {
                StatusCode::ACCEPTED
            };
            Ok((code, Json(out)))
        }
    }
}

/// What the upload door decided a part is. Named rather than a bool: the two
/// branches store different things and answer with different codes.
enum Kind {
    Text,
    Pdf,
}

/// The image door. Parts: `image` (required), `title_hint`, `note`. The
/// bytes are validated and stored here; the reading happens in a job.
async fn upload_image(
    tenant: Tenant,
    multipart: axum::extract::Multipart,
) -> Result<(StatusCode, Json<crate::core::ingest::IngestOutcome>)> {
    let mut parts = read_upload(multipart, "image", &["note", "title_hint"]).await?;
    let Some(FilePart {
        filename, bytes, ..
    }) = parts.file
    else {
        return Err(Error::Validation("no image in the upload".into()));
    };
    let out = tenant
        .core
        .ingest_image(crate::core::ingest::ImageCapture {
            bytes: bytes.to_vec(),
            filename,
            title_hint: parts.fields.remove("title_hint"),
            note: parts.fields.remove("note"),
        })
        .await?;
    // 202, not 201: stored, but the reading — the part that makes it a corpus
    // anyone can search — is still queued.
    let code = if out.duplicate {
        StatusCode::OK
    } else {
        StatusCode::ACCEPTED
    };
    Ok((code, Json(out)))
}

/// The bytes as uploaded, whatever they are. The image door's `?original=1`
/// answers the same thing for a photo and stays where it is; this is the name
/// that does not lie about a PDF.
async fn get_file(tenant: Tenant, Path(id): Path<String>) -> Result<axum::response::Response> {
    use axum::response::IntoResponse;
    let Some((mime, bytes)) = tenant.core.store.attachment_original(&id).await? else {
        return Err(Error::NotFound);
    };
    Ok((
        [
            (axum::http::header::CONTENT_TYPE, mime),
            (
                axum::http::header::CACHE_CONTROL,
                "private, max-age=3600".to_string(),
            ),
        ],
        bytes,
    )
        .into_response())
}

#[derive(serde::Deserialize, Default)]
struct ImageQuery {
    #[serde(default)]
    original: Option<String>,
}

/// The preview by default; `?original=1` for the bytes as uploaded.
async fn get_image(
    tenant: Tenant,
    Path(id): Path<String>,
    Query(q): Query<ImageQuery>,
) -> Result<axum::response::Response> {
    use axum::response::IntoResponse;
    let want_original = q
        .original
        .as_deref()
        .is_some_and(|v| v == "1" || v == "true");
    let found = if want_original {
        tenant.core.store.attachment_original(&id).await?
    } else {
        tenant.core.store.attachment_preview(&id).await?
    };
    let Some((mime, bytes)) = found else {
        return Err(Error::NotFound);
    };
    Ok((
        [
            (axum::http::header::CONTENT_TYPE, mime),
            (
                axum::http::header::CACHE_CONTROL,
                "private, max-age=3600".to_string(),
            ),
        ],
        bytes,
    )
        .into_response())
}

#[derive(serde::Deserialize)]
pub struct ListParams {
    #[serde(default = "default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}
fn default_limit() -> i64 {
    50
}

async fn list_corpora(
    tenant: Tenant,
    Query(p): Query<ListParams>,
) -> Result<Json<Vec<crate::store::corpora::Corpus>>> {
    Ok(Json(
        tenant
            .core
            .store
            .list_corpora(p.limit.clamp(1, 200), p.offset.max(0))
            .await?,
    ))
}

#[derive(serde::Serialize)]
pub struct CorpusDetail {
    #[serde(flatten)]
    pub source: crate::store::corpora::Corpus,
    pub chunks: Vec<crate::store::artifacts::Chunk>,
}

async fn get_corpus(tenant: Tenant, Path(cid): Path<String>) -> Result<Json<CorpusDetail>> {
    let source = tenant.core.store.get_corpus(&cid).await?;
    let chunks = tenant.core.store.artifacts_for_corpus(&cid).await?;
    Ok(Json(CorpusDetail { source, chunks }))
}

async fn delete_corpus(tenant: Tenant, Path(cid): Path<String>) -> Result<StatusCode> {
    tenant.core.delete_corpus(&cid).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn reprocess(
    tenant: Tenant,
    Path(cid): Path<String>,
    Json(req): Json<ReprocessRequest>,
) -> Result<StatusCode> {
    let stage = Stage::parse(&req.stage)
        .ok_or_else(|| Error::Validation(format!("unknown stage `{}`", req.stage)))?;
    tenant.core.reprocess(&cid, stage).await?;
    Ok(StatusCode::ACCEPTED)
}

#[derive(serde::Deserialize)]
struct ResolveBody {
    action: crate::core::ingest::NearDupeAction,
}

/// Act on a capture parked as a near-duplicate. The decision is an operator's:
/// nothing here compares the two documents again, it only carries out what was
/// chosen.
async fn resolve_near_dupe(
    tenant: Tenant,
    Path(cid): Path<String>,
    Json(body): Json<ResolveBody>,
) -> Result<Json<serde_json::Value>> {
    tenant
        .core
        .resolve_near_duplicate(&cid, body.action)
        .await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// What consolidation has decided and what it is still asking about.
async fn consolidation(tenant: Tenant) -> Result<Json<serde_json::Value>> {
    use crate::store::pairs::PairState;
    Ok(Json(serde_json::json!({
        "superseded": tenant.core.store.superseded_artifacts(100).await?,
        // What the judge actually ruled on, listed first for the same reason
        // Ops puts it at the top: it is the one output here that cost a model
        // call, and an operator reading only `pairs` would conclude there was
        // nothing to look at.
        // Awaiting review, like the queue on Capture: a client acting on this
        // report presses the same endpoints an operator does, and those refuse
        // an artifact that is no longer active. A pair naming one is work that
        // can only come back `cannot supersede: loser … is superseded`.
        "contradictions": tenant
            .core
            .store
            .pairs_awaiting_review(PairState::Contradiction, 100)
            .await?,
        // Judge-proposed supersedes awaiting an operator's confirmation. Listed
        // for the same reason Ops renders them: without this a pair the judge
        // ruled on simply disappears from `pairs`, and an API consumer never
        // sees the proposal it left behind.
        "supersede_proposals": tenant
            .core
            .store
            .pairs_awaiting_review(PairState::Superseded, 100)
            .await?,
        // Discards awaiting confirmation, listed for the same reason. Only ever
        // rows an older base filed: a vacuous verdict is now carried out where
        // it is found, so nothing new lands here. Emitted regardless — a client
        // indexing this key breaks on a response that drops it, and the pairs
        // already in that state are still waiting on the press.
        "discard_proposals": tenant
            .core
            .store
            .pairs_awaiting_review(PairState::Vacuous, 100)
            .await?,
        // Retired, and permanently empty: `would_merge` was a verdict a person
        // confirmed, every verdict is acted on now, and the migration rewrites
        // the rows that carried it. Emitted anyway — a client indexing this key
        // breaks on a response that drops it, and an empty list already says
        // "nothing here to act on" in the language the rest of this reads.
        "merge_proposals": serde_json::Value::Array(vec![]),
        "pairs": tenant
            .core
            .store
            .pairs_awaiting_review(PairState::Pending, 100)
            .await?,
    })))
}

#[derive(serde::Deserialize)]
pub struct SearchParams {
    pub q: String,
    /// How many *ranked* hits to return. The response can be longer than this:
    /// with `[associate]` on, up to `associate.spread_max` further artifacts
    /// are appended — ones the query never matched, recalled because they are
    /// linked to what it did match. Every such row carries `via` (the ranked
    /// hit it was recalled beside) and a `score` of 0, so a client that wants
    /// only what it asked for can drop them by that field. They obey `tags` and
    /// `category` like any other row.
    ///
    /// They do *not* obey the two `include_*` flags: an associated row is always
    /// an active, unsuperseded artifact, whatever those are set to. Nothing here
    /// was asked for by name — association is a spread out of what the query did
    /// match — and a retired artifact recalled that way would be competing with
    /// the very successor that retired it. A deprecation audit gets what it
    /// needs from the ranked hits, where both flags do apply.
    pub limit: Option<usize>,
    pub tags: Option<String>,
    pub category: Option<String>,
    #[serde(default)]
    pub include_deprecated: bool,
    #[serde(default)]
    pub include_superseded: bool,
    /// Which client is asking. Only `extension` is honoured; see
    /// `Door::from_client`.
    pub door: Option<String>,
    /// `false` skips the reranker for this call — the typing opt-out.
    /// Absent means true: one deliberate question wants the best order.
    pub rerank: Option<bool>,
    /// Ask for the ranking explanation. It changes the response shape — the
    /// bare array becomes `{"results": […], "explanation": {…}}` — so only a
    /// caller that asked for it sees the envelope. It never changes the order.
    #[serde(default, deserialize_with = "query_flag")]
    pub explain: Option<bool>,
}

/// What both search doors make of one set of query parameters.
///
/// Extracted rather than repeated: `typing`, `mark` and `rerank` are decisions
/// with reasons, and two doors reaching them separately is exactly the drift a
/// second route must not introduce.
fn search_request(
    tenant: &Tenant,
    q: SearchParams,
) -> (SearchQuery, crate::store::feedback::Origin, bool) {
    use crate::store::feedback::Door;
    let door = q
        .door
        .as_deref()
        .map(Door::from_client)
        .unwrap_or(Door::Api);
    // The extension's panel is a search-as-you-type box exactly like the web
    // UI's: it debounces at 200ms, so one query arrives as a run of its own
    // prefixes. Marking those stamps `last_seen_at` on whatever "loo", "loop",
    // "loop d" happened to match, and that is the field `resurface` reads —
    // typing would quietly drain the forgotten-chunk feature and disqualify
    // those artifacts from the stale list. `src/web/ui.rs` opts out for this
    // reason; the panel has to as well. What an operator actually read is
    // stamped when they open the artifact, not while they are still typing.
    let typing = matches!(door, Door::Extension);
    let explain = q.explain.unwrap_or(false);
    let query = SearchQuery {
        q: q.q,
        limit: q.limit.unwrap_or(0),
        // Repeated `?tags=a&tags=b` is awkward in a browser query string, so
        // accept a comma-separated list.
        tags: q
            .tags
            .map(|s| {
                s.split(',')
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect()
            })
            .unwrap_or_default(),
        category: q.category.filter(|c| !c.is_empty()),
        // An API call is one deliberate question; only a typing UI opts out.
        mark: !typing,
        include_deprecated: q.include_deprecated,
        include_superseded: q.include_superseded,
        // Deliberate calls want the best order by default; a typing door
        // opts out the way the web UI's keystrokes do, and gets its answer
        // at vector-order speed. An explicit `rerank` still overrides either
        // way.
        rerank: q.rerank.unwrap_or(!typing),
        explain,
    };
    // Typing and scoped were one decision here, and they are two.
    //
    // Coalescing folds a keystroke into the query it was an early spelling of,
    // and it folds only within one scope — so a box that types has to say who
    // is typing, or two operators' panels fold into each other's queries. But a
    // shell is not a typing door and still has a subject: `--show` records the
    // open it leads to with a scope, and `jobs/pursuit.rs` attaches an
    // interaction to a search only when the scopes match. Unscoped, a
    // shell-only session opened pursuits that collected no engagement at all
    // and closed unsatisfied every time — it could widen a hole in the base and
    // never fill one.
    //
    // `Api` and `Mcp` stay unscoped on purpose: a bearer token is not a person,
    // and two agents sharing one would fold into each other's queries. The same
    // reason `sitting.rs` keeps no sitting for them.
    let origin: crate::store::feedback::Origin = match door {
        Door::Extension | Door::Cli => door.by(tenant.user.subject.clone()),
        _ => door.into(),
    };
    (query, origin, explain)
}

/// The cap `Core::search` would have applied, read the same way, so asking for
/// an explanation — or for the stages — cannot change what a door returns.
fn search_cap(tenant: &Tenant) -> Option<usize> {
    tenant
        .core
        .ranking
        .read()
        .expect("ranking lock")
        .per_source_cap
}

async fn search(tenant: Tenant, Query(q): Query<SearchParams>) -> Result<Json<serde_json::Value>> {
    let cap = search_cap(&tenant);
    let (query, origin, explain) = search_request(&tenant, q);
    let (results, outcome) = tenant.core.search_with(&query, cap, origin).await?;
    Ok(Json(if explain {
        serde_json::json!({ "results": results, "explanation": outcome.explanation })
    } else {
        serde_json::to_value(results)
            .map_err(|e| Error::Internal(format!("serialising search results: {e}")))?
    }))
}

#[derive(serde::Deserialize)]
pub struct StaleParams {
    pub limit: Option<usize>,
}

/// Active artifacts nobody has confirmed or retrieved in a while — candidates
/// for an operator to review and deprecate. Read-only: nothing here changes
/// an artifact, and nothing here feeds search ranking.
async fn stale(
    tenant: Tenant,
    Query(p): Query<StaleParams>,
) -> Result<Json<Vec<crate::core::search::SearchResult>>> {
    Ok(Json(
        tenant.core.stale_candidates(p.limit.unwrap_or(20)).await?,
    ))
}

async fn ask(
    tenant: Tenant,
    Json(req): Json<crate::core::ask::AskRequest>,
) -> Result<Json<crate::core::ask::AskResponse>> {
    // No ask model, no ask door: the route is not there. See `Core::asks`.
    if !tenant.core.asks() {
        return Err(Error::NotFound);
    }
    Ok(Json(
        tenant
            .core
            .ask(&req, crate::store::feedback::Door::Api)
            .await?,
    ))
}

#[derive(serde::Deserialize)]
pub struct ResurfaceParams {
    pub limit: Option<usize>,
}

async fn resurface(
    tenant: Tenant,
    Query(p): Query<ResurfaceParams>,
) -> Result<Json<Vec<crate::core::search::SearchResult>>> {
    Ok(Json(tenant.core.resurface(p.limit.unwrap_or(5)).await?))
}

/// Enough of a corpus to say where a passage came from, and no more.
///
/// Not the `Corpus` row itself: that carries `raw_text`, which for a captured
/// book is the whole book. Shipping it beside every single-artifact read would
/// make the cheapest door in the API the most expensive one, to say a name.
#[derive(serde::Serialize)]
pub struct SourceRef {
    pub id: String,
    pub title: Option<String>,
    pub origin: String,
    pub source_url: Option<String>,
}

/// One artifact, and the document it was captured from.
///
/// The chunk is flattened rather than nested, so every key this door has ever
/// answered with is still at the top level and no reader of it had to change.
/// `source` is `None` in the two cases that are not failures: a merged
/// artifact belongs to no single corpus, and a corpus deleted since leaves its
/// artifacts readable.
#[derive(serde::Serialize)]
pub struct ArtifactDetail {
    #[serde(flatten)]
    pub artifact: crate::store::artifacts::Chunk,
    pub source: Option<SourceRef>,
}

async fn get_artifact(tenant: Tenant, Path(cid): Path<String>) -> Result<Json<ArtifactDetail>> {
    let chunk = tenant.core.store.get_artifact(&cid).await?;
    // A reading is not refused because the document behind it is gone: the
    // artifact is the thing that was asked for and it is still here.
    let source = match &chunk.corpus_id {
        // `corpus_origin` and not `get_corpus`: the four columns below are the
        // whole of what this answers with, and the row carries `raw_text`.
        Some(id) => tenant
            .core
            .store
            .corpus_origin(id)
            .await?
            .map(|c| SourceRef {
                id: c.id,
                title: c.title_hint,
                origin: c.origin,
                source_url: c.source_url,
            }),
        None => None,
    };
    // Asking for one artifact by id is the same deliberate act the detail pane
    // records, and it is the whole of what this door can honestly say: there
    // is no navigation to have pivoted through, so no `via`, and no session to
    // belong to, because a bearer token is not a conversation. Written after
    // the read succeeds — a 404 engaged nothing.
    tenant.core.mark_artifact_seen(&cid);
    tenant
        .core
        .record_interaction(&cid, None, Some(&tenant.user.subject));
    Ok(Json(ArtifactDetail {
        artifact: chunk,
        source,
    }))
}

async fn patch_artifact(
    tenant: Tenant,
    Path(cid): Path<String>,
    Json(req): Json<PatchArtifactRequest>,
) -> Result<Json<crate::store::artifacts::Chunk>> {
    if req.text.is_none() && req.title.is_none() && req.category.is_none() && req.tags.is_none() {
        return Err(Error::Validation("no fields to update".into()));
    }
    // Validate everything before writing anything: a request half-applied and
    // then rejected leaves a chunk in a state the caller never asked for.
    let text = match &req.text {
        Some(t) if t.trim().is_empty() => {
            return Err(Error::Validation("chunk text is empty".into()));
        }
        Some(t) => Some(t.trim().to_string()),
        None => None,
    };
    let title = req
        .title
        .map(|t| clean_optional(t, MAX_TITLE_LEN, "title"))
        .transpose()?;
    // Held to the same closed list synthesis is. This is the other door into
    // the field, and a door that accepts any string is the one the subject
    // words came through in the first place. Folded rather than rejected, for
    // the same reason: the edit is about the text, and refusing the whole
    // request over a label helps nobody.
    let category = req
        .category
        .map(|c| clean_optional(c, MAX_CATEGORY_LEN, "category"))
        .transpose()?
        .map(|c| c.map(|v| crate::infer::prompt::normalize_category(&v)));
    let tags = req.tags.as_deref().map(clean_tags).transpose()?;

    tenant.core.store.get_artifact(&cid).await?;

    // The embedder is shown the title followed by the body, so either of those
    // invalidates the stored vector. A category or a tag changes only what the
    // payload says about the chunk.
    let revectorize = text.is_some() || title.is_some();

    if let Some(t) = &text {
        tenant.core.store.update_artifact_text(&cid, t).await?;
    }
    if let Some(t) = &title {
        tenant
            .core
            .store
            .update_artifact_title(&cid, t.as_deref())
            .await?;
    }
    if let Some(c) = &category {
        tenant
            .core
            .store
            .update_artifact_category(&cid, c.as_deref())
            .await?;
    }
    if let Some(t) = &tags {
        tenant.core.store.update_artifact_tags(&cid, t).await?;
    }

    let chunk = tenant.core.store.get_artifact(&cid).await?;
    if revectorize {
        tenant
            .core
            .store
            .enqueue(Stage::Embed, "artifact", &cid)
            .await?;
    } else if chunk.embed_state == crate::store::artifacts::EmbedState::Embedded {
        // Nothing the model saw has changed, so rewrite the payload in place
        // rather than spending an inference call to recompute the same vector.
        //
        // Only when there is a point to rewrite: for a chunk still waiting to
        // be embedded, this would be a request Qdrant accepts and applies to
        // nothing, and the pending job writes the whole payload anyway.
        tenant
            .core
            .vectors
            .set_payload(&crate::vector::VectorPayload {
                artifact_id: chunk.id.clone(),
                corpus_id: chunk.corpus_id.clone().unwrap_or_default(),
                text: chunk.text.clone(),
                title: chunk.title.clone(),
                category: chunk.category.clone(),
                tags: chunk.tags.clone(),
                created_at: chunk.created_at,
                last_seen_at: None,
                hit_count: None,
                status: None,
                last_verified_at: None,
                superseded_by: None,
                origin_corpora: vec![],
                provenance: None,
            })
            .await?;
    }
    Ok(Json(chunk))
}

async fn delete_artifact(tenant: Tenant, Path(cid): Path<String>) -> Result<StatusCode> {
    // Both stores, in the order that survives an interruption — see
    // `Core::delete_artifact`, which the UI button posts to as well.
    tenant.core.delete_artifact(&cid).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn status(tenant: Tenant) -> Result<Json<StatusResponse>> {
    use sqlx::Row;
    let corpus_rows = sqlx::query("SELECT status, COUNT(*) AS n FROM corpora GROUP BY status")
        .fetch_all(&tenant.core.store.pool)
        .await?;
    let chunks: i64 = sqlx::query("SELECT COUNT(*) AS n FROM artifacts")
        .fetch_one(&tenant.core.store.pool)
        .await?
        .get("n");

    Ok(Json(StatusResponse {
        sources: corpus_rows
            .iter()
            .map(|r| (r.get("status"), r.get("n")))
            .collect(),
        jobs: tenant.core.store.job_counts().await?,
        failed: tenant.core.store.failed_jobs(50).await?,
        oldest_pending_secs: tenant.core.store.oldest_pending_age().await?,
        chunks,
        // Qdrant being briefly unreachable should not fail the status page,
        // which is exactly where you look when something is wrong.
        vectors: tenant.core.vectors.count().await.unwrap_or(0),
    }))
}

/// The same ask, streamed, for a client that cannot wait in one request.
///
/// The browser extension's panel is the caller this exists for. `/ui/ask` would
/// not serve it three times over: it parks the question and streams from a
/// second GET, because a page navigates and an `EventSource` cannot POST; it
/// records against `Door::Ui`; and its frames carry rendered HTML. The panel
/// has none of those problems and one the page does not — it reads the stream
/// by hand with `fetch`, since `EventSource` cannot carry a bearer header — so
/// one POST that streams its own response is the shape that fits.
///
/// One question, one request, and no handoff to expire.
async fn ask_stream(
    tenant: Tenant,
    Json(req): Json<crate::core::ask::AskRequest>,
) -> Result<Response> {
    // No ask model, no ask door: the route is not there. See `Core::asks`.
    if !tenant.core.asks() {
        return Err(Error::NotFound);
    }
    use tokio_stream::StreamExt as _;

    let core = tenant.core.clone();
    // The door an ask came through, which names the caller and nothing more.
    // Unlike search, where `door=extension` decides how the query is recorded,
    // no ask is recorded from here: `record_ask` admits `Door::Ui` alone, so
    // this answer carries no `event_id` and is never judged. Named anyway,
    // because a question composed while reading is a different thing from one
    // typed into an API client, and the log should not have to guess.
    let origin = crate::store::feedback::Door::Extension.by(tenant.user.subject);
    let events = async_stream::stream! {
        let s = core.ask_events(&req, origin);
        tokio::pin!(s);
        while let Some(ev) = s.next().await {
            // Every arm is a frame, and the stream itself never fails. Yielding
            // an `Err` into `Sse` ends the response body where it stands, with
            // no frame at all — from the panel that is indistinguishable from
            // an answer that simply stopped, `thinking` still on screen and
            // nothing said. A failure that has words is worth the words.
            yield Ok::<_, Error>(match ev {
                // Terminal by construction: the producer is a `try_stream!` and
                // ends at its first error, so the panel sees one `error` frame
                // and nothing after it.
                Ok(e) => api_sse_event(e).unwrap_or_else(|e| error_frame(&e)),
                Err(e) => error_frame(&e),
            });
        }
    };
    // Kept alive for the same reason the page's stream is: a slow model thinks
    // for longer than a proxy's idle timeout, and a connection closed
    // mid-answer looks exactly like an answer that ended.
    Ok(Sse::new(events)
        .keep_alive(KeepAlive::default())
        .into_response())
}

/// A failure the panel can read, in the shape every other frame has.
///
/// JSON like the rest of them, so the panel's reader has one shape to parse
/// rather than one payload that is a value and one that is a bare sentence.
///
/// The words are `client_message`'s, not `Display`'s, for the reason the JSON
/// error body uses them: a store or LLM-parse failure carries schema and
/// prompt fragments, and a stream is no less a door than a status code. This
/// is also where the detail goes to the log, since a frame yielded from inside
/// the body never reaches `IntoResponse` and would otherwise fail in silence.
/// One search event as a JSON frame.
///
/// Deliberately not an arm of `api_sse_event`: a test over that function
/// asserts the browser panel handles every frame name it mentions, and a
/// search's stages are no business of the panel's.
fn search_sse_event(ev: crate::core::search::SearchEvent) -> Result<SseEvent> {
    use crate::core::search::SearchEvent::*;
    let (name, data) = match ev {
        Stages(v) => ("stages", serde_json::json!({ "stages": v })),
        Stage(s) => ("stage", serde_json::json!({ "stage": s })),
        Results(hits) => ("results", serde_json::json!({ "results": hits })),
    };
    Ok(SseEvent::default().event(name).data(
        serde_json::to_string(&data)
            .map_err(|e| Error::Internal(format!("serialising a search frame: {e}")))?,
    ))
}

/// The same search, reporting each stage as it starts.
///
/// A second route rather than a header on the first: `GET /search` answers a
/// bare array to the terminal client, the extension, `/mcp` and anything a
/// person has scripted, and a route that answers two shapes depending on what
/// was asked for is worse than two routes that each answer one.
async fn search_stream(tenant: Tenant, Query(q): Query<SearchParams>) -> Result<Response> {
    use tokio_stream::StreamExt as _;
    let cap = search_cap(&tenant);
    let (query, origin, _explain) = search_request(&tenant, q);
    let core = tenant.core.clone();
    let events = async_stream::stream! {
        let s = core.search_events(query, cap, origin);
        tokio::pin!(s);
        while let Some(ev) = s.next().await {
            // Terminal by construction: the producer ends at its first error,
            // so a client sees one `error` frame and nothing after it.
            yield Ok::<_, Error>(match ev {
                Ok(e) => search_sse_event(e).unwrap_or_else(|e| error_frame(&e)),
                Err(e) => error_frame(&e),
            });
        }
    };
    Ok(Sse::new(events)
        .keep_alive(KeepAlive::default())
        .into_response())
}

fn error_frame(e: &Error) -> SseEvent {
    if e.status().is_server_error() {
        tracing::error!(error = %e, "ask stream failed");
    } else {
        tracing::debug!(error = %e, "ask stream rejected");
    }
    SseEvent::default()
        .event("error")
        .data(serde_json::json!({ "error": e.client_message() }).to_string())
}

/// One ask event as a JSON frame.
///
/// The twin of `sse_event` in `ui.rs`, and deliberately not the same function.
/// That one renders the rail and the answer into HTML, because the page's two
/// halves of a citation have to be numbered by one server-side pass. The panel
/// builds every node with `textContent` — artifact text is whatever a captured
/// page contained — so it is sent values and renders them itself.
///
/// The frame names are shared with that mapper, and a test asserts the panel
/// names every one of them.
fn api_sse_event(ev: crate::core::ask::stream::AskEvent) -> Result<SseEvent> {
    use crate::core::ask::stream::AskEvent::*;
    let (name, data) = match ev {
        Retrieved {
            round,
            retrieved,
            shown,
            dropped,
            cliff_at,
        } => (
            "retrieved",
            serde_json::json!({
                "round": round,
                "retrieved": retrieved,
                "shown": shown,
                "dropped": dropped,
                "cliff_at": cliff_at,
            }),
        ),
        Needs(what) => ("needs", serde_json::json!({ "queries": what })),
        Citations(hits) => ("citations", serde_json::json!({ "hits": hits })),
        Reasoning(t) => ("reasoning", serde_json::json!({ "text": t })),
        Token(t) => ("token", serde_json::json!({ "text": t })),
        // The whole response, which is what the blocking door returns: the
        // panel replaces its streamed draft with it, so what is finally on
        // screen is the answer the server stands behind rather than a
        // concatenation the panel assembled.
        Done(d) => (
            "done",
            // Every field is a string, a number or a list of those, so this
            // cannot fail for any input the type admits. Returned rather than
            // unwrapped because a panic here would kill the stream mid-answer
            // with nothing said; the caller turns it into an `error` frame.
            serde_json::to_value(*d)
                .map_err(|e| Error::Internal(format!("the answer would not serialise: {e}")))?,
        ),
    };
    Ok(SseEvent::default().event(name).data(data.to_string()))
}

pub fn api_router(image_max_bytes: usize, pdf_max_bytes: usize) -> Router<AppState> {
    Router::new()
        .route("/corpora", post(ingest).get(list_corpora))
        // Its own ceiling, because this door now takes a PDF and a book is
        // many times the global limit. The text branch re-imposes
        // `MAX_BODY_BYTES` on itself inside the handler.
        // One route carrying what three carried, so its ceiling is the widest
        // of theirs; each branch re-imposes its own inside the handler.
        .route(
            "/capture",
            post(capture).layer(axum::extract::DefaultBodyLimit::max(
                pdf_max_bytes.max(image_max_bytes),
            )),
        )
        .route(
            "/corpora/upload",
            post(upload).layer(axum::extract::DefaultBodyLimit::max(pdf_max_bytes)),
        )
        // Its own ceiling: a phone photo is several times the global limit.
        .route(
            "/corpora/image",
            post(upload_image).layer(axum::extract::DefaultBodyLimit::max(image_max_bytes)),
        )
        .route("/corpora/{id}", get(get_corpus).delete(delete_corpus))
        .route("/corpora/{id}/image", get(get_image))
        .route("/corpora/{id}/file", get(get_file))
        .route("/corpora/{id}/reprocess", post(reprocess))
        .route("/corpora/{id}/resolve", post(resolve_near_dupe))
        .route("/search", get(search))
        .route("/search/stream", get(search_stream))
        .route("/ask", post(ask))
        .route("/ask/stream", post(ask_stream))
        .route("/resurface", get(resurface))
        .route("/consolidation", get(consolidation))
        .route("/consolidation/stale", get(stale))
        .route(
            "/artifacts/{id}",
            get(get_artifact)
                .patch(patch_artifact)
                .delete(delete_artifact),
        )
        .route("/vectors/sample", get(crate::web::vbg::sample))
        .route("/status", get(status))
}

#[cfg(test)]
pub(crate) mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    use crate::web::test_support::{FilePart, a_png, app_with_token, json_of, multipart};

    async fn app_and_token() -> (axum::Router, String) {
        let (app, token, _core) = app_token_and_core().await;
        (app, token)
    }

    pub async fn app_token_and_core() -> (axum::Router, String, crate::core::Core) {
        app_from_core(crate::core::test_support::test_core().await).await
    }

    /// Wrap a core a test has already adjusted — feedback switched on, say —
    /// in the real router.
    pub async fn app_from_core(
        core: crate::core::Core,
    ) -> (axum::Router, String, crate::core::Core) {
        let handle = core.clone();
        let (app, token) = app_with_token(core).await;
        (app, token, handle)
    }

    #[tokio::test]
    async fn the_consolidation_report_still_carries_every_key_it_ever_had() {
        // `would_merge` is a retired verdict and the migration rewrites those
        // rows to `pending`, so `merge_proposals` is now always empty — which
        // is a reason to stop filling it, not a reason to stop emitting it. A
        // client indexing the key breaks on a response that omits it, and an
        // empty list says "nothing to act on" in the language it already reads.
        let (app, token) = app_and_token().await;

        let res = app
            .oneshot(get("/api/v1/consolidation", Some(&token)))
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);
        let body = json_of(res).await;
        for key in [
            "superseded",
            "contradictions",
            "supersede_proposals",
            "merge_proposals",
            "pairs",
        ] {
            assert!(
                body.get(key).is_some_and(|v| v.is_array()),
                "the report dropped `{key}`: {body}"
            );
        }
    }

    /// The report is the API's copy of the review queue, and a client acting on
    /// it presses the same endpoints an operator does. A pair naming an
    /// artifact that has left results is refused by all of them, so listing it
    /// hands the client work that can only fail.
    #[tokio::test]
    async fn the_consolidation_report_leaves_out_a_pair_nobody_can_act_on() {
        let (app, token, core) = app_token_and_core().await;
        let src = core.store.insert_corpus("x", "web", None).await.unwrap();
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &["the timeout is 30 seconds", "the timeout is 90 seconds"]
                    .iter()
                    .enumerate()
                    .map(|(i, t)| crate::store::artifacts::NewArtifact {
                        ordinal: i as i64,
                        text: (*t).to_string(),
                        corpus_span: None,
                        title: None,
                        category: None,
                        tags: vec![],
                        segment_idx: None,
                        caveats: vec![],
                    })
                    .collect::<Vec<_>>(),
            )
            .await
            .unwrap();
        let (a, b) = (made[0].id.clone(), made[1].id.clone());
        core.store.record_pair(&a, &b, 0.9).await.unwrap();
        let pair = core
            .store
            .pairs_by_state(crate::store::pairs::PairState::Pending, 10)
            .await
            .unwrap()
            .remove(0);
        core.store
            .set_pair_state(
                pair.id,
                crate::store::pairs::PairState::Contradiction,
                Some("30 seconds vs 90"),
                crate::store::pairs::DecidedBy::Model,
            )
            .await
            .unwrap();
        core.deprecate(&b).await.unwrap();

        let res = app
            .oneshot(get("/api/v1/consolidation", Some(&token)))
            .await
            .unwrap();

        let body = json_of(res).await;
        assert_eq!(
            body["contradictions"].as_array().map(|v| v.len()),
            Some(0),
            "the report listed a pair whose member is out of results: {body}"
        );
    }

    #[tokio::test]
    async fn reading_an_artifact_over_the_api_is_an_open() {
        // The API door was the one place a read left no trace at all: an agent
        // could work through `GET /artifacts/{id}` all afternoon and teach the
        // offer ladder, promotion and pursuits precisely nothing.
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        let out = core
            .ingest("a verbatim passage", "web", None)
            .await
            .unwrap();
        crate::jobs::passages::capture_verbatim(&core, &out.id)
            .await
            .unwrap();
        let a = core.store.artifacts_for_corpus(&out.id).await.unwrap()[0]
            .id
            .clone();
        let (app, token, core) = app_from_core(core).await;

        let res = app
            .oneshot(get(&format!("/api/v1/artifacts/{a}"), Some(&token)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        core.background.wait_idle().await;
        let got = core
            .store
            .interactions_between(0, crate::store::now() + 1)
            .await
            .unwrap();
        assert_eq!(got.len(), 1, "the API read was not recorded: {got:?}");
        assert_eq!(got[0].artifact_id, a);
        assert_eq!(got[0].kind, "opened");
        // The API has no navigation to pivot through and no session to belong
        // to: a bearer token is not a conversation.
        assert_eq!(got[0].via, None);
    }

    /// The terminal's `--show` reads one artifact in full, and the whole point
    /// of it is that nothing is clipped: a rendering that shows two lines is
    /// the rendering that made this door necessary. The source travels with it
    /// because a passage without the document it came from is the citation
    /// problem `-a` already has.
    #[tokio::test]
    async fn one_artifact_is_readable_in_full_with_the_document_it_came_from() {
        let (app, token, core) = app_token_and_core().await;
        let src = core
            .store
            .insert_corpus("der Rohtext", "web", Some("Handbuch Mobilforensik"))
            .await
            .unwrap();
        let long = "eine Zeile\n".repeat(40);
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: long.clone(),
                    corpus_span: None,
                    title: Some("Physische Extraktion".into()),
                    category: None,
                    tags: vec!["forensik".into()],
                    segment_idx: None,
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();

        let res = app
            .clone()
            .oneshot(get(
                &format!("/api/v1/artifacts/{}", made[0].id),
                Some(&token),
            ))
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);
        let body = json_of(res).await;
        assert_eq!(body["id"], made[0].id, "{body}");
        assert_eq!(
            body["text"].as_str().unwrap(),
            long,
            "the text arrived clipped, which is the one thing this door exists to prevent"
        );
        assert_eq!(body["title"], "Physische Extraktion", "{body}");
        assert_eq!(
            body["source"]["title"], "Handbuch Mobilforensik",
            "the document it came from did not travel with it: {body}"
        );
    }

    #[tokio::test]
    async fn an_artifact_that_is_not_there_is_a_404_rather_than_an_empty_reading() {
        let (app, token) = app_and_token().await;
        let res = app
            .oneshot(get(
                "/api/v1/artifacts/01a00000-0000-7000-8000-000000000000",
                Some(&token),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    fn get(uri: &str, token: Option<&str>) -> Request<Body> {
        let mut b = Request::builder().uri(uri).method("GET");
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        b.body(Body::empty()).unwrap()
    }

    /// The whole SSE body of a GET, as text.
    async fn sse_body(app: &axum::Router, uri: &str, token: &str) -> String {
        let res = app.clone().oneshot(get(uri, Some(token))).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{uri}");
        String::from_utf8(
            axum::body::to_bytes(res.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap()
    }

    /// A shell has a subject, and the open it leads to is recorded with one.
    /// Unscoped, the two could never be joined.
    #[tokio::test]
    async fn a_cli_search_is_recorded_against_whoever_ran_it() {
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        let (app, token, core) = app_from_core(core).await;
        app.clone()
            .oneshot(get("/api/v1/search?q=journal&door=cli", Some(&token)))
            .await
            .unwrap();
        // The recording is off the request path, as everything the learning
        // layer writes is.
        core.background.wait_idle().await;
        let events = core
            .store
            .events_between(0, crate::store::now() + 1)
            .await
            .unwrap();
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(events[0].scope.is_some(), "an unscoped shell search: {events:?}");
    }

    /// The other half of the rule, and the one that must not move: a token is
    /// not a person, and two agents sharing one must not fold together.
    #[tokio::test]
    async fn a_bearer_token_is_still_not_a_person() {
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        let (app, token, core) = app_from_core(core).await;
        app.clone()
            .oneshot(get("/api/v1/search?q=journal", Some(&token)))
            .await
            .unwrap();
        core.background.wait_idle().await;
        let events = core
            .store
            .events_between(0, crate::store::now() + 1)
            .await
            .unwrap();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].scope, None, "Door::Api must stay unscoped");
    }

    #[tokio::test]
    async fn the_streaming_door_names_its_stages_and_then_answers() {
        let (app, token) = app_and_token().await;
        let body = sse_body(&app, "/api/v1/search/stream?q=journal&door=cli", &token).await;
        assert!(body.contains("event: stages"), "{body}");
        assert!(body.contains("\"embed\""), "{body}");
        assert!(body.contains("event: results"), "{body}");
        let (before, after) = body.split_once("event: results").expect("a terminal frame");
        assert!(before.contains("event: stage"), "no stage was announced: {body}");
        assert!(
            !after.contains("event: stage"),
            "a stage was reported after the results it preceded: {body}"
        );
    }

    /// The shape four clients read. It is not this change's to alter.
    #[tokio::test]
    async fn the_plain_search_door_still_answers_a_bare_array() {
        let (app, token) = app_and_token().await;
        let res = app
            .oneshot(get("/api/v1/search?q=journal", Some(&token)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert!(v.is_array(), "{v}");
    }

    fn bg_point(id: &str, v: Vec<f32>) -> crate::vector::VectorPoint {
        crate::vector::VectorPoint {
            vector: v,
            sparse: Default::default(),
            payload: crate::vector::VectorPayload {
                artifact_id: id.into(),
                corpus_id: "s".into(),
                text: format!("text of {id}"),
                title: None,
                category: None,
                tags: vec![],
                created_at: 0,
                last_seen_at: None,
                hit_count: None,
                status: None,
                last_verified_at: None,
                superseded_by: None,
                origin_corpora: vec![],
                provenance: None,
            },
        }
    }

    #[tokio::test]
    async fn the_background_sample_is_bounded_and_carries_its_tag() {
        let (app, token, core) = app_token_and_core().await;
        core.vectors
            .upsert(vec![
                bg_point("a", vec![1.0, 0.0, 0.0]),
                bg_point("b", vec![0.0, 1.0, 0.0]),
            ])
            .await
            .unwrap();

        let res = app
            .oneshot(get("/api/v1/vectors/sample", Some(&token)))
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);
        // Never a `max-age`. One URL answers with a different tenant's contents
        // depending on who is signed in, and an HTTP cache is keyed on the URL
        // alone — so a held answer is the previous account's cloud drawn for
        // the next one, and nothing can reach into a browser cache to drop it.
        // The tag and the client's `localStorage` hold the snapshot instead,
        // because that is the layer sign-out can clear — and the tag is checked
        // on every load rather than trusted for a window.
        let cache = res.headers()[header::CACHE_CONTROL]
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            cache.contains("no-store") && !cache.contains("max-age"),
            "a tenant's cloud must not be held in a browser cache: {cache}"
        );
        let body = json_of(res).await;
        let pts = body["points"].as_array().expect("points is an array");
        assert_eq!(pts.len(), 2);
        assert_eq!(body["count"], 2);
        let tag = body["tag"].as_str().expect("a tag").to_string();
        assert!(!tag.is_empty());
        for p in pts {
            let r: f64 = p
                .as_array()
                .unwrap()
                .iter()
                .map(|c| c.as_f64().unwrap().powi(2))
                .sum::<f64>()
                .sqrt();
            assert!(r <= 1.25 + 1e-3, "point escaped the outlier ceiling: {p}");
        }
    }

    #[tokio::test]
    async fn an_empty_store_yields_an_empty_cloud_not_an_error() {
        let (app, token) = app_and_token().await;
        let res = app
            .oneshot(get("/api/v1/vectors/sample", Some(&token)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = json_of(res).await;
        assert_eq!(body["points"], serde_json::json!([]));
        assert_eq!(body["count"], 0);
        // And it says so with a tag of its own, so a browser holding the cloud
        // this base used to have is told to drop it. Points that outlive the
        // vectors they were drawn from are a picture of a store that is not
        // there — which is what a held snapshot made of an emptied base.
        assert!(!body["unchanged"].as_bool().unwrap_or(false));
        assert!(body["tag"].as_str().expect("a tag").ends_with(":0"));
    }

    #[tokio::test]
    async fn a_tag_that_still_matches_the_store_saves_the_points() {
        let (app, token, core) = app_token_and_core().await;
        core.vectors
            .upsert(vec![bg_point("a", vec![1.0, 0.0, 0.0])])
            .await
            .unwrap();

        let body = json_of(
            app.clone()
                .oneshot(get("/api/v1/vectors/sample", Some(&token)))
                .await
                .unwrap(),
        )
        .await;
        let tag = body["tag"].as_str().unwrap().to_string();
        assert_eq!(body["points"].as_array().unwrap().len(), 1);

        // The same store, so the client keeps what it has and the scroll is
        // never run.
        let again = json_of(
            app.clone()
                .oneshot(get(
                    &format!("/api/v1/vectors/sample?have={tag}"),
                    Some(&token),
                ))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(again["unchanged"], true);
        assert_eq!(again["points"], serde_json::json!([]));

        // One more vector and the tag no longer describes the base: the answer
        // carries the new cloud rather than telling the client to redraw a
        // picture that has stopped being true.
        core.vectors
            .upsert(vec![bg_point("b", vec![0.0, 1.0, 0.0])])
            .await
            .unwrap();
        let changed = json_of(
            app.oneshot(get(
                &format!("/api/v1/vectors/sample?have={tag}"),
                Some(&token),
            ))
            .await
            .unwrap(),
        )
        .await;
        assert!(!changed["unchanged"].as_bool().unwrap_or(false));
        assert_eq!(changed["points"].as_array().unwrap().len(), 2);
        assert_ne!(changed["tag"].as_str().unwrap(), tag);
    }

    #[tokio::test]
    async fn the_background_sample_needs_auth() {
        let (app, _token) = app_and_token().await;
        let res = app
            .oneshot(get("/api/v1/vectors/sample", None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    /// A POST with a raw body and a content type of the caller's choosing —
    /// what every client of `/capture` sends, and what `post_json` cannot
    /// express because it fixes the type. An empty `content_type` omits the
    /// header entirely, which is a case the door has to answer for.
    pub(crate) fn raw_post(
        uri: &str,
        token: &str,
        content_type: &str,
        body: &[u8],
    ) -> Request<Body> {
        let mut b = Request::builder()
            .uri(uri)
            .method("POST")
            .header("authorization", format!("Bearer {token}"));
        if !content_type.is_empty() {
            b = b.header("content-type", content_type);
        }
        b.body(Body::from(body.to_vec())).unwrap()
    }

    fn post_json(uri: &str, token: &str, body: serde_json::Value) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .method("POST")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    pub fn patch_json(uri: &str, token: &str, body: serde_json::Value) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .method("PATCH")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn post_file(
        uri: &str,
        token: &str,
        filename: &str,
        mime: Option<&str>,
        body: &[u8],
    ) -> Request<Body> {
        multipart(
            uri,
            token,
            &[],
            &[FilePart {
                field: "file",
                filename,
                mime,
                body,
            }],
        )
    }

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
        multipart(
            uri,
            token,
            fields,
            &[FilePart {
                field: field_name,
                filename,
                mime,
                body,
            }],
        )
    }

    /// Two parts under the same file field, which no browser form sends and
    /// no handler should guess about.
    fn post_two_files(uri: &str, token: &str, field_name: &str, mime: &str) -> Request<Body> {
        multipart(
            uri,
            token,
            &[],
            &[
                FilePart {
                    field: field_name,
                    filename: "a.txt",
                    mime: Some(mime),
                    body: b"first",
                },
                FilePart {
                    field: field_name,
                    filename: "b.txt",
                    mime: Some(mime),
                    body: b"second",
                },
            ],
        )
    }

    fn a_pdf() -> Vec<u8> {
        include_bytes!("../../tests/fixtures/one-heading.pdf").to_vec()
    }

    #[tokio::test]
    async fn the_upload_door_takes_a_pdf_and_answers_accepted() {
        let (app, token, core) = app_token_and_core().await;
        let res = app
            .oneshot(post_file_with(
                "/api/v1/corpora/upload",
                &token,
                &[("note", "the quarterly plan")],
                "file",
                "plan.pdf",
                Some("application/pdf"),
                &a_pdf(),
            ))
            .await
            .unwrap();
        // 202, not 201: stored, but the reading that makes it searchable is
        // still queued — the same promise the image door makes.
        assert_eq!(res.status(), StatusCode::ACCEPTED);
        let j = json_of(res).await;
        assert_eq!(j["status"], "extracting");

        let src = core
            .store
            .get_corpus(j["id"].as_str().unwrap())
            .await
            .unwrap();
        assert_eq!(src.origin, crate::core::ingest::ORIGIN_PDF);
        assert_eq!(src.metadata["note"], "the quarterly plan");
    }

    /// A part may legally carry no `Content-Type`; the name is then the only
    /// thing the sender told us, exactly as for `.txt`.
    #[tokio::test]
    async fn a_pdf_named_pdf_but_declaring_nothing_is_still_taken() {
        let (app, token) = app_and_token().await;
        let res = app
            .oneshot(post_file_with(
                "/api/v1/corpora/upload",
                &token,
                &[],
                "file",
                "PLAN.PDF",
                None,
                &a_pdf(),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED, "case must not matter");
    }

    /// The door widened by one format, not to anything binary.
    #[tokio::test]
    async fn a_zip_is_refused_by_type_and_by_name() {
        let (app, token, core) = app_token_and_core().await;
        for mime in [Some("application/zip"), None] {
            let res = app
                .clone()
                .oneshot(post_file_with(
                    "/api/v1/corpora/upload",
                    &token,
                    &[],
                    "file",
                    "a.zip",
                    mime,
                    b"PK\x03\x04",
                ))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{mime:?}");
        }
        assert!(core.store.list_corpora(10, 0).await.unwrap().is_empty());
    }

    /// Over the global 8 MB, under the PDF ceiling: the multipart parser gets
    /// to see it, so the answer is the handler's and not the framework's 413.
    #[tokio::test]
    async fn the_upload_door_has_its_own_larger_body_limit_for_a_pdf() {
        let (app, token) = app_and_token().await;
        let mut big = a_pdf();
        big.resize(crate::web::MAX_BODY_BYTES + 1024, b' ');
        let res = app
            .oneshot(post_file_with(
                "/api/v1/corpora/upload",
                &token,
                &[],
                "file",
                "big.pdf",
                Some("application/pdf"),
                &big,
            ))
            .await
            .unwrap();
        assert_ne!(
            res.status(),
            StatusCode::PAYLOAD_TOO_LARGE,
            "the global limit is still in force on this route"
        );
    }

    #[tokio::test]
    async fn the_original_pdf_comes_back_from_the_file_route() {
        let (app, token, core) = app_token_and_core().await;
        let id = core
            .ingest_pdf(crate::core::ingest::PdfCapture {
                bytes: a_pdf(),
                filename: Some("plan.pdf".into()),
                title_hint: None,
                note: None,
            })
            .await
            .unwrap()
            .id;
        let res = app
            .oneshot(
                Request::get(format!("/api/v1/corpora/{id}/file"))
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()["content-type"], "application/pdf");
        let got = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(got.to_vec(), a_pdf(), "byte for byte as uploaded");
    }

    /// The image route answers for images. A PDF is stored without a preview,
    /// and a caller walking every corpus for a thumbnail must be told there is
    /// none rather than handed zero bytes under `image/jpeg`.
    #[tokio::test]
    async fn the_image_route_has_nothing_to_show_for_a_pdf() {
        let (app, token, core) = app_token_and_core().await;
        let id = core
            .ingest_pdf(crate::core::ingest::PdfCapture {
                bytes: a_pdf(),
                filename: Some("plan.pdf".into()),
                title_hint: None,
                note: None,
            })
            .await
            .unwrap()
            .id;
        let res = app
            .oneshot(get(&format!("/api/v1/corpora/{id}/image"), Some(&token)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn an_upload_with_two_file_parts_is_refused() {
        let (app, token, core) = app_token_and_core().await;
        for (uri, field, mime) in [
            ("/api/v1/corpora/upload", "file", "text/plain"),
            ("/api/v1/corpora/image", "image", "image/png"),
        ] {
            let res = app
                .clone()
                .oneshot(post_two_files(uri, &token, field, mime))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{uri}");
            let err = json_of(res).await["error"].as_str().unwrap().to_string();
            assert!(err.contains("one file"), "{uri}: {err}");
        }
        assert!(core.store.list_corpora(10, 0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_image_upload_is_accepted_with_its_note_and_queued() {
        let (app, token, core) = app_token_and_core().await;
        let res = app
            .clone()
            .oneshot(post_file_with(
                "/api/v1/corpora/image",
                &token,
                &[
                    ("note", "front of the router"),
                    ("title_hint", "Router label"),
                ],
                "image",
                "IMG_9.png",
                Some("image/png"),
                &a_png(),
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
            .oneshot(post_file_with(
                "/api/v1/corpora/image",
                &token,
                &[],
                "image",
                "IMG_9.png",
                Some("image/png"),
                &a_png(),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(json_of(res).await["id"], id);
    }

    #[tokio::test]
    async fn the_image_door_refuses_junk_missing_parts_and_a_closed_door() {
        let (app, token, core) = app_token_and_core().await;
        let res = app
            .clone()
            .oneshot(post_file_with(
                "/api/v1/corpora/image",
                &token,
                &[],
                "image",
                "x.jpg",
                Some("image/jpeg"),
                b"not really",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert!(
            json_of(res).await["error"]
                .as_str()
                .unwrap()
                .contains("supported image")
        );

        let res = app
            .clone()
            .oneshot(post_file_with(
                "/api/v1/corpora/image",
                &token,
                &[("note", "n")],
                "file",
                "x.png",
                Some("image/png"),
                &a_png(),
            ))
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::BAD_REQUEST,
            "wrong part name is 'no image in the upload'"
        );
        assert!(core.store.list_corpora(10, 0).await.unwrap().is_empty());

        let (app, token, _) =
            app_from_core(crate::core::test_support::test_core_without_vision().await).await;
        let res = app
            .oneshot(post_file_with(
                "/api/v1/corpora/image",
                &token,
                &[],
                "image",
                "x.png",
                Some("image/png"),
                &a_png(),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert!(
            json_of(res).await["error"]
                .as_str()
                .unwrap()
                .contains("not configured")
        );
    }

    #[tokio::test]
    async fn the_image_door_has_its_own_larger_body_limit() {
        // Over the global 8 MB, under the image ceiling: the multipart parser
        // gets to see it, so the answer is the handler's (junk → 400), not the
        // framework's 413.
        let (app, token) = app_and_token().await;
        let big = vec![0u8; crate::web::MAX_BODY_BYTES + 1024];
        let res = app
            .clone()
            .oneshot(post_file_with(
                "/api/v1/corpora/image",
                &token,
                &[],
                "image",
                "big.png",
                Some("image/png"),
                &big,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        // And the text door still stops at 8 MB. Multipart streams, so the
        // limit surfaces as a parse error inside the handler rather than a
        // 413 from the framework; either way nothing is stored.
        let (app, token, core) = app_token_and_core().await;
        let res = app
            .oneshot(post_file(
                "/api/v1/corpora/upload",
                &token,
                "big.txt",
                Some("text/plain"),
                &big,
            ))
            .await
            .unwrap();
        assert!(
            matches!(
                res.status(),
                StatusCode::BAD_REQUEST | StatusCode::PAYLOAD_TOO_LARGE
            ),
            "{}",
            res.status()
        );
        assert!(core.store.list_corpora(10, 0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_preview_and_the_original_are_served() {
        let (app, token, _core) = app_token_and_core().await;
        let res = app
            .clone()
            .oneshot(post_file_with(
                "/api/v1/corpora/image",
                &token,
                &[],
                "image",
                "p.png",
                Some("image/png"),
                &a_png(),
            ))
            .await
            .unwrap();
        let id = json_of(res).await["id"].as_str().unwrap().to_string();

        let res = app
            .clone()
            .oneshot(get(&format!("/api/v1/corpora/{id}/image"), Some(&token)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()["content-type"], "image/jpeg");
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 22)
            .await
            .unwrap();
        assert!(image::load_from_memory(&bytes).is_ok());

        let res = app
            .clone()
            .oneshot(get(
                &format!("/api/v1/corpora/{id}/image?original=1"),
                Some(&token),
            ))
            .await
            .unwrap();
        assert_eq!(res.headers()["content-type"], "image/png");
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 22)
            .await
            .unwrap();
        assert_eq!(bytes.to_vec(), a_png());

        let res = app
            .clone()
            .oneshot(get(&format!("/api/v1/corpora/{id}/image"), None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let res = app
            .oneshot(get("/api/v1/corpora/nope/image", Some(&token)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_text_upload_records_its_note_and_file_facts() {
        let (app, token, core) = app_token_and_core().await;
        let res = app
            .oneshot(post_file_with(
                "/api/v1/corpora/upload",
                &token,
                &[("note", "from the printer")],
                "file",
                "notes.txt",
                Some("text/plain"),
                b"hello there",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let id = json_of(res).await["id"].as_str().unwrap().to_string();
        let m = core.store.get_corpus(&id).await.unwrap().metadata;
        assert_eq!(m["note"], "from the printer");
        assert_eq!(m["file"]["name"], "notes.txt");
        assert_eq!(m["file"]["size"], 11);
    }

    #[tokio::test]
    async fn an_extension_search_records_its_own_door() {
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        let (app, token, core) = app_from_core(core).await;

        let res = app
            .oneshot(get(
                "/api/v1/search?q=loop+device&door=extension",
                Some(&token),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        core.background.wait_idle().await;

        // A search from the panel is the least contaminated query there is:
        // composed while reading, before anything came back. The judging page
        // can only weigh it that way if the door says which it was.
        let doors: Vec<String> = sqlx::query_scalar("SELECT door FROM search_events")
            .fetch_all(&core.store.pool)
            .await
            .unwrap();
        assert_eq!(doors, vec!["extension".to_string()]);
    }

    #[tokio::test]
    async fn a_search_from_the_panel_does_not_stamp_what_a_prefix_matched() {
        // The extension's panel is a search-as-you-type box: it debounces at
        // 200ms, so "loop device" arrives as a run of its own prefixes. If
        // those mark, every artifact "loo" happened to match gets its
        // `last_seen_at` stamped — the field `resurface` reads — and typing
        // quietly drains the forgotten-artifact feature and empties the stale
        // list. `src/web/ui.rs` opts out for this reason; so must this door.
        let core = crate::core::test_support::test_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: "the loop device is what makes this work".into(),
                    corpus_span: None,
                    title: Some("loop".into()),
                    category: Some("reference".into()),
                    tags: vec![],
                    segment_idx: None,
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();
        for c in &made {
            crate::jobs::embed::run(&core, &c.id).await.unwrap();
        }

        let (app, token, core) = app_from_core(core).await;
        let res = app
            .oneshot(get("/api/v1/search?q=loo&door=extension", Some(&token)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        core.background.wait_idle().await;

        let stamped = core
            .vectors
            .resurface(10, i64::MAX, i64::MAX)
            .await
            .unwrap()
            .into_iter()
            .filter(|h| h.payload.last_seen_at.is_some())
            .count();
        assert_eq!(
            stamped, 0,
            "typing in the panel must not stamp last_seen_at"
        );
    }

    #[tokio::test]
    async fn a_panel_keystroke_does_not_wait_on_the_reranker() {
        // The panel is the same search-as-you-type box as the web UI's, and
        // the comment on `search` says it takes the same opt-outs. The web UI
        // answers keystrokes in vector order and refines on the pause; a
        // debounced panel prefix must not pay a synchronous rerank round trip
        // either.
        let (core, reranker) = crate::core::test_support::test_core_counting_reranked_docs().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: "the loop device is what makes this work".into(),
                    corpus_span: None,
                    title: Some("loop".into()),
                    category: Some("reference".into()),
                    tags: vec![],
                    segment_idx: None,
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();
        for c in &made {
            crate::jobs::embed::run(&core, &c.id).await.unwrap();
        }
        let (app, token, _core) = app_from_core(core).await;

        let res = app
            .clone()
            .oneshot(get("/api/v1/search?q=loop&door=extension", Some(&token)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            reranker.docs_seen(),
            0,
            "a typing door's keystroke must answer in vector order, not wait \
             on a rerank call"
        );

        // A deliberate API call is one question asked on purpose, and it
        // still gets the best order by default.
        let res = app
            .oneshot(get("/api/v1/search?q=loop", Some(&token)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(
            reranker.docs_seen() > 0,
            "a deliberate API search still reranks by default"
        );
    }

    #[tokio::test]
    async fn a_deliberate_api_search_still_counts_as_seeing() {
        // The other half of the rule above: an API call is one question asked
        // on purpose, and it must go on marking what it showed.
        let core = crate::core::test_support::test_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: "the loop device is what makes this work".into(),
                    corpus_span: None,
                    title: Some("loop".into()),
                    category: Some("reference".into()),
                    tags: vec![],
                    segment_idx: None,
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();
        for c in &made {
            crate::jobs::embed::run(&core, &c.id).await.unwrap();
        }

        let (app, token, core) = app_from_core(core).await;
        let res = app
            .oneshot(get("/api/v1/search?q=loop+device", Some(&token)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        core.background.wait_idle().await;

        let stamped = core
            .vectors
            .resurface(10, i64::MAX, i64::MAX)
            .await
            .unwrap()
            .into_iter()
            .filter(|h| h.payload.last_seen_at.is_some())
            .count();
        assert!(stamped > 0, "a deliberate search still counts as seeing");
    }

    #[tokio::test]
    async fn a_client_cannot_claim_a_door_that_would_launder_its_query() {
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        let (app, token, core) = app_from_core(core).await;

        // `ask` and `judge` are never captured, so naming one would be a way
        // to have a contaminated query recorded as a clean one — or to have a
        // real one silently dropped.
        let res = app
            .oneshot(get("/api/v1/search?q=loop+device&door=judge", Some(&token)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        core.background.wait_idle().await;

        let doors: Vec<String> = sqlx::query_scalar("SELECT door FROM search_events")
            .fetch_all(&core.store.pool)
            .await
            .unwrap();
        assert_eq!(doors, vec!["api".to_string()]);
    }

    #[tokio::test]
    async fn an_uploaded_filename_is_a_file_fact_not_a_title() {
        let (app, token, core) = app_token_and_core().await;
        let res = app
            .oneshot(post_file(
                "/api/v1/corpora/upload",
                &token,
                "mounting-notes.txt",
                Some("text/plain"),
                b"alpha para\n\nbeta para",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let id = json_of(res).await["id"].as_str().unwrap().to_string();
        let src = core.store.get_corpus(&id).await.unwrap();
        assert_eq!(
            src.title_hint, None,
            "the Title stage names it; the filename is not a name"
        );
        assert_eq!(src.metadata["file"]["name"], "mounting-notes.txt");
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
                Some("text/plain"),
                &[0xff, 0xfe, 0x00, 0x41],
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert!(core.store.list_corpora(10, 0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_upload_of_the_wrong_type_is_refused_with_the_reason() {
        // This asserted a PDF until the PDF door opened. A CSV is the case it
        // was really about: bytes that decode as UTF-8 and are still not a
        // document anyone asked to capture. The reason has to name what the
        // door does take, or the sender is left guessing.
        let (app, token) = app_and_token().await;
        let res = app
            .oneshot(post_file(
                "/api/v1/corpora/upload",
                &token,
                "rows.csv",
                Some("text/csv"),
                b"a,b\n1,2\n",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let msg = json_of(res).await["error"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(msg.contains("text/csv"), "got {msg}");
        assert!(msg.contains("application/pdf"), "got {msg}");
    }

    #[tokio::test]
    async fn an_upload_with_no_declared_type_is_judged_by_its_name() {
        // A multipart part may legally carry no `Content-Type`, and treating
        // that as "fine" turned the `.txt` door into "anything that decodes as
        // UTF-8": a `.csv`, a `.json`, a page of HTML.
        let (app, token, core) = app_token_and_core().await;
        let res = app
            .clone()
            .oneshot(post_file(
                "/api/v1/corpora/upload",
                &token,
                "rows.csv",
                None,
                b"id,name\n1,alpha\n2,beta",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert!(core.store.list_corpora(10, 0).await.unwrap().is_empty());

        // Named `.txt` and nothing else to go on: accepted, as before.
        let res = app
            .oneshot(post_file(
                "/api/v1/corpora/upload",
                &token,
                "NOTES.TXT",
                None,
                b"alpha para\n\nbeta para",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
    }

    #[test]
    fn only_a_supplied_fragment_is_exempt_from_the_extraction_floor() {
        // `scope: "selection"` exempts a highlighted fragment, because three
        // sentences picked out on purpose are a real capture. There is no
        // selection in a server-side fetch, though — so on the `url` path the
        // claim is only a way to switch off the guard that catches a login
        // wall, and it must not be honoured there.
        let req = |html: Option<&str>, url: Option<&str>, scope: Option<&str>| {
            serde_json::from_value::<super::IngestRequest>(serde_json::json!({
                "html": html,
                "url": url,
                "scope": scope,
            }))
            .unwrap()
        };

        assert!(super::floor_exempt(&req(
            Some("<p>three sentences</p>"),
            Some("https://example.test/a"),
            Some("selection"),
        )));
        assert!(!super::floor_exempt(&req(
            None,
            Some("https://example.test/a"),
            Some("selection"),
        )));
        assert!(!super::floor_exempt(&req(
            Some("<p>a whole page</p>"),
            None,
            None
        )));
    }

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
                serde_json::json!({ "html": long }),
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
            assert_eq!(res.status(), StatusCode::BAD_REQUEST, "accepted {body}");
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
                serde_json::json!({ "html": html, "url": "https://example.test/notes" }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let id = json_of(res).await["id"].as_str().unwrap().to_string();

        let src = core.store.get_corpus(&id).await.unwrap();
        assert_eq!(src.origin, "extension");
        assert_eq!(
            src.source_url.as_deref(),
            Some("https://example.test/notes")
        );
        // Extraction, not the raw HTML: nothing downstream learns HTML exists.
        assert!(
            src.raw_text.contains("## Mounting"),
            "got: {}",
            src.raw_text
        );
        assert!(!src.raw_text.contains("<article>"));
    }

    #[tokio::test]
    async fn a_page_that_extracts_to_nothing_is_refused_and_stores_no_corpus() {
        let (app, token, core) = app_token_and_core().await;
        let res = app
            .oneshot(post_json(
                "/api/v1/corpora",
                &token,
                serde_json::json!({ "html": "<html><body><p>Subscribe to read.</p></body></html>" }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert!(core.store.list_corpora(10, 0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn every_api_route_rejects_an_unauthenticated_request() {
        // A missing auth check on one route is the failure mode that matters
        // most here, so assert it route by route rather than spot-checking.
        let (app, _) = app_and_token().await;
        for (method, uri) in [
            ("GET", "/api/v1/search?q=x"),
            ("GET", "/api/v1/resurface"),
            ("GET", "/api/v1/corpora"),
            ("POST", "/api/v1/corpora"),
            ("POST", "/api/v1/corpora/upload"),
            ("GET", "/api/v1/corpora/abc"),
            ("DELETE", "/api/v1/corpora/abc"),
            ("POST", "/api/v1/corpora/abc/reprocess"),
            ("POST", "/api/v1/ask"),
            ("POST", "/api/v1/ask/stream"),
            ("GET", "/api/v1/artifacts/abc"),
            ("PATCH", "/api/v1/artifacts/abc"),
            ("DELETE", "/api/v1/artifacts/abc"),
            ("GET", "/api/v1/status"),
        ] {
            let req = Request::builder()
                .uri(uri)
                .method(method)
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap();
            let res = app.clone().oneshot(req).await.unwrap();
            assert_eq!(
                res.status(),
                StatusCode::UNAUTHORIZED,
                "{method} {uri} was not protected"
            );
        }
    }

    #[tokio::test]
    async fn healthz_is_public_and_leaks_nothing() {
        let (app, _) = app_and_token().await;
        let res = app.oneshot(get("/healthz", None)).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
        assert_eq!(&bytes[..], b"ok");
    }

    #[tokio::test]
    async fn a_bad_token_is_rejected() {
        let (app, _) = app_and_token().await;
        let res = app
            .oneshot(get("/api/v1/search?q=x", Some("engram_not_a_real_token")))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_parked_capture_is_resolved_over_the_api() {
        let (app, token, core) = app_token_and_core().await;
        let body: String = (0..200)
            .map(|i| format!("step {i}: run the mount command and read its output"))
            .collect::<Vec<_>>()
            .join("\n");
        core.ingest(&body, "web", None).await.unwrap();
        while core.store.claim_job().await.unwrap().is_some() {}
        let second = core
            .ingest(&body.replacen("step 7:", "step seven:", 1), "web", None)
            .await
            .unwrap();
        assert!(second.near_duplicate.is_some());

        let res = app
            .oneshot(post_json(
                &format!("/api/v1/corpora/{}/resolve", second.id),
                &token,
                serde_json::json!({ "action": "keep_both" }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            core.store.get_corpus(&second.id).await.unwrap().status,
            crate::store::corpora::CorpusStatus::Raw
        );
    }

    #[tokio::test]
    async fn resolving_a_corpus_that_is_not_parked_is_a_bad_request() {
        let (app, token, core) = app_token_and_core().await;
        let out = core.ingest("plain text", "web", None).await.unwrap();
        let res = app
            .oneshot(post_json(
                &format!("/api/v1/corpora/{}/resolve", out.id),
                &token,
                serde_json::json!({ "action": "discard" }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn ingest_returns_201_with_an_id_and_status() {
        let (app, token) = app_and_token().await;
        let res = app
            .oneshot(post_json(
                "/api/v1/corpora",
                &token,
                serde_json::json!({"text":"a procedure","title":"t"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let v = json_of(res).await;
        assert!(v["id"].is_string());
        assert_eq!(v["status"], "raw");
        assert_eq!(v["duplicate"], false);
    }

    #[tokio::test]
    async fn ingesting_the_same_text_twice_returns_200_and_the_same_id() {
        let (app, token) = app_and_token().await;
        let body = serde_json::json!({"text":"identical"});
        let first = json_of(
            app.clone()
                .oneshot(post_json("/api/v1/corpora", &token, body.clone()))
                .await
                .unwrap(),
        )
        .await;
        let res = app
            .oneshot(post_json("/api/v1/corpora", &token, body))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let second = json_of(res).await;
        assert_eq!(first["id"], second["id"]);
        assert_eq!(second["duplicate"], true);
    }

    /// The paste-a-link door reads what the MCP door reads: a link to a PDF
    /// is stored for extraction and answered 202, like an uploaded one.
    #[tokio::test]
    async fn a_pasted_link_to_a_pdf_is_stored_for_extraction() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/plan.pdf"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(a_pdf(), "application/pdf"))
            .mount(&server)
            .await;
        let (app, token) = app_and_token().await;
        let res = app
            .oneshot(post_json(
                "/api/v1/corpora",
                &token,
                serde_json::json!({"url": format!("{}/plan.pdf", server.uri())}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);
        let body = json_of(res).await;
        assert_eq!(body["status"], "extracting");
    }

    #[tokio::test]
    async fn a_source_url_is_refused_unless_it_is_one_a_page_is_read_over() {
        // `Url::parse` accepts these, and `html` plus `url` never reaches the
        // scheme allowlist in `fetch_html`. What is stored here is rendered
        // as a link on the corpus page, in the operator's own session.
        let (app, token) = app_and_token().await;
        for bad in [
            "javascript:fetch('/api/v1/tokens')",
            "data:text/html,<script>1</script>",
            "file:///etc/passwd",
        ] {
            let res = app
                .clone()
                .oneshot(post_json(
                    "/api/v1/corpora",
                    &token,
                    serde_json::json!({"html":"<article><p>a body</p></article>","url":bad}),
                ))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::BAD_REQUEST, "accepted {bad}");
        }
    }

    #[tokio::test]
    async fn a_selection_is_not_held_to_the_whole_page_extraction_floor() {
        // The floor is a guess about whole pages: too little text out of one
        // means a login wall or an empty shell. A fragment the operator
        // highlighted and asked for by name means neither, and refusing it —
        // with a message about login walls, no less — refuses the capture
        // that was requested.
        let (app, token) = app_and_token().await;
        let short = "<p>The loop device is what makes this work.</p>";
        let res = app
            .clone()
            .oneshot(post_json(
                "/api/v1/corpora",
                &token,
                serde_json::json!({"html":short,"scope":"selection"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED, "selection refused");

        // The same fragment without the scope is still held to the floor.
        let res = app
            .clone()
            .oneshot(post_json(
                "/api/v1/corpora",
                &token,
                serde_json::json!({"html":short}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        // And a selection that extracted to nothing at all is still an error,
        // rather than an empty corpus stored in silence.
        let res = app
            .oneshot(post_json(
                "/api/v1/corpora",
                &token,
                serde_json::json!({"html":"<div></div>","scope":"selection"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn empty_ingest_is_a_400() {
        let (app, token) = app_and_token().await;
        let res = app
            .oneshot(post_json(
                "/api/v1/corpora",
                &token,
                serde_json::json!({"text":"   "}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn search_passes_filters_through_to_core() {
        let (app, token) = app_and_token().await;
        app.clone()
            .oneshot(post_json(
                "/api/v1/corpora",
                &token,
                serde_json::json!({"text":"mounting an image"}),
            ))
            .await
            .unwrap();

        let res = app
            .clone()
            .oneshot(get(
                "/api/v1/search?q=anything&limit=5&tags=fake&category=note",
                Some(&token),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(json_of(res).await.is_array());
    }

    #[tokio::test]
    async fn the_bare_search_response_is_still_an_array() {
        let (app, token) = app_and_token().await;
        let res = app
            .oneshot(get("/api/v1/search?q=anything", Some(&token)))
            .await
            .unwrap();
        assert!(
            json_of(res).await.is_array(),
            "no existing client passes `explain`, so no existing client may see \
             a different envelope"
        );
    }

    #[tokio::test]
    async fn explain_wraps_the_results_and_adds_the_pool() {
        let (app, token) = app_and_token().await;
        let res = app
            .oneshot(get("/api/v1/search?q=anything&explain=1", Some(&token)))
            .await
            .unwrap();
        let body = json_of(res).await;
        assert!(body["results"].is_array(), "got {body}");
        assert!(
            body["explanation"]["candidates_fetched"].is_number(),
            "the pool's shape is what a caller asks `explain` for: got {body}"
        );
    }

    #[tokio::test]
    async fn search_without_a_query_is_a_400() {
        let (app, token) = app_and_token().await;
        let res = app
            .oneshot(get("/api/v1/search?q=", Some(&token)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_missing_source_is_a_404() {
        let (app, token) = app_and_token().await;
        let res = app
            .oneshot(get("/api/v1/corpora/nope", Some(&token)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn an_unknown_reprocess_stage_is_a_400() {
        let (app, token) = app_and_token().await;
        let created = json_of(
            app.clone()
                .oneshot(post_json(
                    "/api/v1/corpora",
                    &token,
                    serde_json::json!({"text":"something"}),
                ))
                .await
                .unwrap(),
        )
        .await;
        let id = created["id"].as_str().unwrap();
        let res = app
            .oneshot(post_json(
                &format!("/api/v1/corpora/{id}/reprocess"),
                &token,
                serde_json::json!({"stage":"nonsense"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn internal_errors_do_not_leak_sql_to_the_client() {
        let (app, token) = app_and_token().await;
        let res = app
            .oneshot(get("/api/v1/corpora/nope", Some(&token)))
            .await
            .unwrap();
        let body = json_of(res).await.to_string();
        assert!(!body.contains("SELECT"), "{body}");
        assert!(!body.contains("sqlite"), "{body}");
    }

    #[tokio::test]
    async fn status_reports_queue_and_corpus_counts() {
        let (app, token) = app_and_token().await;
        app.clone()
            .oneshot(post_json(
                "/api/v1/corpora",
                &token,
                serde_json::json!({"text":"something"}),
            ))
            .await
            .unwrap();
        let v = json_of(
            app.oneshot(get("/api/v1/status", Some(&token)))
                .await
                .unwrap(),
        )
        .await;
        assert!(v["jobs"].is_array());
        assert!(v["sources"].is_array());
        assert!(v["failed"].is_array());
    }

    #[tokio::test]
    async fn api_ask_is_not_found_without_an_ask_model() {
        let mut core = crate::core::test_support::test_core().await;
        core.completer = None;
        let (app, token) = app_with_token(core).await;
        let res = app
            .oneshot(post_json(
                "/api/v1/ask",
                &token,
                serde_json::json!({"q": "x"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    /// The streaming twin of `/ask`, as the extension panel performs it: one
    /// POST carrying the bearer, and the answer arriving in frames.
    #[tokio::test]
    async fn api_ask_streams_the_answer_in_json_frames() {
        let (app, token, core) = app_token_and_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        crate::jobs::embed::run_corpus(&core, &out.id)
            .await
            .unwrap();

        let res = app
            .oneshot(post_json(
                "/api/v1/ask/stream",
                &token,
                serde_json::json!({"q": "what is alpha"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(
            res.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/event-stream"),
            "{:?}",
            res.headers()
        );

        let body = crate::web::test_support::body_of(res).await;
        assert!(body.contains("event: token"), "no tokens in {body}");

        // The panel builds every node with `textContent`, so the frames it is
        // sent are values rather than the UI route's rendered fragments. The
        // `done` frame therefore carries the whole `AskResponse`.
        let done = body
            .split("event: done")
            .nth(1)
            .unwrap_or_else(|| panic!("no done frame in {body}"))
            .lines()
            .find_map(|l| l.strip_prefix("data:"))
            .and_then(|d| serde_json::from_str::<serde_json::Value>(d.trim()).ok())
            .unwrap_or_else(|| panic!("the done frame carried no JSON: {body}"));
        assert!(done["answer"].is_string(), "{done}");
        assert!(done["citations"].is_array(), "{done}");
        assert!(!body.contains('<'), "a frame carried markup: {body}");
    }

    #[tokio::test]
    async fn api_ask_stream_is_not_found_without_an_ask_model() {
        let mut core = crate::core::test_support::test_core().await;
        core.completer = None;
        let (app, token) = app_with_token(core).await;
        let res = app
            .oneshot(post_json(
                "/api/v1/ask/stream",
                &token,
                serde_json::json!({"q": "x"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    /// The panel reads this stream by hand — `EventSource` cannot carry a
    /// bearer header — so a frame the server sends and the panel's switch does
    /// not name is a frame that vanishes silently. The same guard `app.js` has
    /// against `sse_event` in `ui.rs`, pointed at the other mapper.
    #[tokio::test]
    async fn every_frame_the_api_streams_is_handled_by_the_panel() {
        let api = include_str!("api.rs");
        let body = &api[api
            .find("fn api_sse_event(")
            .expect("api_sse_event is in this file")..];
        let body = &body[..body.find("\n}\n").unwrap()];
        let names: Vec<String> = body
            .split('(')
            .filter_map(|rest| rest.trim_start().strip_prefix('"'))
            .filter_map(|rest| rest.split('"').next())
            .filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_lowercase() || c == '_'))
            .map(str::to_string)
            .collect();
        assert!(
            names.len() >= 6,
            "the frame names could not be read out of api_sse_event: {names:?}"
        );

        let panel = include_str!("../../extension/shared/panel.js");
        for name in names {
            assert!(
                panel.contains(&format!("case '{name}':")),
                "the server streams a `{name}` frame and the panel ignores it"
            );
        }
    }
    #[tokio::test]
    async fn a_bare_url_in_a_text_body_is_captured_as_a_link() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/a"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                "<html><body><article><h1>Mounting</h1><p>Run mount, then check dmesg \
                 for the device name and the filesystem it found. The label is what \
                 fstab matches on, and it survives a reformat only if you set it \
                 again afterwards, which is the step everyone forgets.</p><p>A loop \
                 device needs losetup first, and the offset is in bytes rather than \
                 sectors, which is where the arithmetic usually goes wrong.</p>\
                 </article></body></html>",
                "text/html",
            ))
            .mount(&server)
            .await;

        let (app, token, core) = app_token_and_core().await;
        let res = app
            .oneshot(raw_post(
                "/api/v1/capture",
                &token,
                "text/plain",
                format!("{}/a", server.uri()).as_bytes(),
            ))
            .await
            .unwrap();
        let status = res.status();
        let v = json_of(res).await;
        assert!(status.is_success(), "{status}: {v}");
        let stored = core
            .store
            .get_corpus(v["id"].as_str().unwrap())
            .await
            .unwrap();
        // The claim: a body that is one link became a link capture — read by
        // this server as a stranger — rather than the text of the link itself.
        assert_eq!(stored.origin, crate::core::ingest::ORIGIN_FETCH);
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
        let stored = core
            .store
            .get_corpus(v["id"].as_str().unwrap())
            .await
            .unwrap();
        assert!(
            stored.raw_text.contains("is where"),
            "stored verbatim, not fetched"
        );
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
        let stored = core
            .store
            .get_corpus(v["id"].as_str().unwrap())
            .await
            .unwrap();
        assert_eq!(stored.title_hint.as_deref(), Some("A title"));
    }

    #[tokio::test]
    async fn a_multipart_share_of_three_files_is_three_corpora() {
        let (app, token) = app_and_token().await;
        let png = a_png();
        let res = app
            .oneshot(multipart(
                "/api/v1/capture",
                &token,
                &[],
                &[
                    FilePart {
                        field: "file",
                        filename: "a.txt",
                        mime: Some("text/plain"),
                        body: b"the first procedure",
                    },
                    FilePart {
                        field: "file",
                        filename: "b.txt",
                        mime: Some("text/plain"),
                        body: b"the second procedure",
                    },
                    FilePart {
                        field: "file",
                        filename: "c.png",
                        mime: Some("image/png"),
                        body: &png,
                    },
                ],
            ))
            .await
            .unwrap();
        let v = json_of(res).await;
        let arr = v.as_array().expect("many files answer with an array");
        assert_eq!(arr.len(), 3, "{v}");
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
                &[FilePart {
                    field: "file",
                    filename: "a.txt",
                    mime: Some("text/plain"),
                    body: b"a procedure worth keeping",
                }],
            ))
            .await
            .unwrap();
        let v = json_of(res).await;
        assert!(
            v["id"].is_string(),
            "one file is not wrapped in an array: {v}"
        );
    }

    #[tokio::test]
    async fn a_multipart_share_may_carry_a_url_or_text_instead_of_a_file() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/a"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                "<html><body><article><h1>Mounting</h1><p>Run mount, then check dmesg \
                 for the device name and the filesystem it found. The label is what \
                 fstab matches on, and it survives a reformat only if you set it \
                 again afterwards, which is the step everyone forgets.</p><p>A loop \
                 device needs losetup first, and the offset is in bytes rather than \
                 sectors, which is where the arithmetic usually goes wrong.</p>\
                 </article></body></html>",
                "text/html",
            ))
            .mount(&server)
            .await;

        let (app, token, core) = app_token_and_core().await;
        // A share sheet sends both for one share; the link is the better
        // capture of the two, and the text is the title repeated.
        let res = app
            .oneshot(multipart(
                "/api/v1/capture",
                &token,
                &[
                    ("url", &format!("{}/a", server.uri())),
                    ("text", "Mounting"),
                ],
                &[],
            ))
            .await
            .unwrap();
        let status = res.status();
        let v = json_of(res).await;
        assert!(status.is_success(), "{status}: {v}");
        let stored = core
            .store
            .get_corpus(v["id"].as_str().unwrap())
            .await
            .unwrap();
        assert_eq!(
            stored.origin,
            crate::core::ingest::ORIGIN_FETCH,
            "the link won, and the title beside it was not stored as a second capture"
        );
    }

    #[tokio::test]
    async fn a_multipart_body_with_nothing_in_it_is_refused() {
        let (app, token) = app_and_token().await;
        let res = app
            .oneshot(multipart(
                "/api/v1/capture",
                &token,
                &[("title", "only a title")],
                &[],
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_pdf_body_reaches_the_pdf_path_and_answers_202() {
        let (app, token) = app_and_token().await;
        let res = app
            .oneshot(raw_post(
                "/api/v1/capture",
                &token,
                "application/pdf",
                b"%PDF-1.4 tiny",
            ))
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::ACCEPTED,
            "stored, extraction still queued"
        );
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
            .oneshot(raw_post(
                "/api/v1/capture",
                &token,
                "image/png",
                &vec![0u8; over],
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
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
}

#[cfg(test)]
mod patch_tests {
    use super::tests::*;
    use crate::store::artifacts::{EmbedState, NewArtifact};
    use crate::vector::SearchFilter;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    /// One embedded chunk, and the app that can edit it.
    async fn one_artifact() -> (axum::Router, String, crate::core::Core, String) {
        let (app, token, core) = app_token_and_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    ordinal: 0,
                    text: "the body".into(),
                    corpus_span: None,
                    title: Some("a title".into()),
                    category: Some("concept".into()),
                    tags: vec!["old".into()],
                    segment_idx: None,
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();
        let cid = made[0].id.clone();
        crate::jobs::embed::run(&core, &cid).await.unwrap();
        while core.store.claim_job().await.unwrap().is_some() {}
        (app, token, core, cid)
    }

    #[tokio::test]
    async fn editing_only_tags_rewrites_the_payload_without_re_embedding() {
        // Tags are not shown to the embedding model, so recomputing the vector
        // would spend an inference call to arrive at the same numbers.
        let (app, token, core, cid) = one_artifact().await;

        let res = app
            .oneshot(patch_json(
                &format!("/api/v1/artifacts/{cid}"),
                &token,
                serde_json::json!({ "tags": ["fresh"] }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        assert!(
            core.store.claim_job().await.unwrap().is_none(),
            "a metadata edit queued a re-embed"
        );
        assert_eq!(
            core.store.get_artifact(&cid).await.unwrap().embed_state,
            EmbedState::Embedded,
            "the stored vector is still correct and must stay so"
        );

        // And the vector store agrees, so a filtered search finds it.
        let hits = core
            .vectors
            .search(
                &[0.0; crate::core::test_support::TEST_DIM],
                &Default::default(),
                10,
                &SearchFilter {
                    tags: vec!["fresh".into()],
                    category: None,
                    include_superseded: false,
                    include_deprecated: false,
                    corpus_id: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(hits.len(), 1, "the payload in Qdrant still says `old`");
    }

    #[tokio::test]
    async fn editing_the_title_does_queue_a_re_embed() {
        // The embedder is shown the title followed by the body, so a new title
        // means the stored vector describes text that no longer exists.
        let (app, token, core, cid) = one_artifact().await;

        app.oneshot(patch_json(
            &format!("/api/v1/artifacts/{cid}"),
            &token,
            serde_json::json!({ "title": "a better title" }),
        ))
        .await
        .unwrap();

        assert!(
            core.store.claim_job().await.unwrap().is_some(),
            "a title change left a stale vector in place"
        );
        assert_eq!(
            core.store.get_artifact(&cid).await.unwrap().embed_state,
            EmbedState::Pending
        );
    }

    #[tokio::test]
    async fn editing_the_text_still_queues_a_re_embed() {
        let (app, token, core, cid) = one_artifact().await;
        app.oneshot(patch_json(
            &format!("/api/v1/artifacts/{cid}"),
            &token,
            serde_json::json!({ "text": "different body" }),
        ))
        .await
        .unwrap();
        assert!(core.store.claim_job().await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_patch_that_changes_nothing_is_rejected() {
        let (app, token, _core, cid) = one_artifact().await;
        let res = app
            .oneshot(patch_json(
                &format!("/api/v1/artifacts/{cid}"),
                &token,
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_field_can_be_cleared_with_an_explicit_null() {
        // An absent key means "leave it alone", so without this a category
        // could be set and then never removed.
        let (app, token, core, cid) = one_artifact().await;
        assert_eq!(
            core.store
                .get_artifact(&cid)
                .await
                .unwrap()
                .category
                .as_deref(),
            Some("concept")
        );

        let res = app
            .oneshot(patch_json(
                &format!("/api/v1/artifacts/{cid}"),
                &token,
                serde_json::json!({ "category": null }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(core.store.get_artifact(&cid).await.unwrap().category, None);
    }

    #[tokio::test]
    async fn an_untouched_field_keeps_its_value() {
        let (app, token, core, cid) = one_artifact().await;
        app.oneshot(patch_json(
            &format!("/api/v1/artifacts/{cid}"),
            &token,
            serde_json::json!({ "tags": ["fresh"] }),
        ))
        .await
        .unwrap();

        let c = core.store.get_artifact(&cid).await.unwrap();
        assert_eq!(
            c.category.as_deref(),
            Some("concept"),
            "category was erased"
        );
        assert_eq!(c.title.as_deref(), Some("a title"), "title was erased");
    }

    #[tokio::test]
    async fn tags_are_trimmed_deduplicated_and_bounded() {
        let (app, token, core, cid) = one_artifact().await;
        app.oneshot(patch_json(
            &format!("/api/v1/artifacts/{cid}"),
            &token,
            serde_json::json!({ "tags": ["  linux ", "linux", "", "   ", "forensics"] }),
        ))
        .await
        .unwrap();
        assert_eq!(
            core.store.get_artifact(&cid).await.unwrap().tags,
            vec!["linux".to_string(), "forensics".to_string()],
            "a repeated tag is a filter condition evaluated twice for one answer"
        );
    }

    #[tokio::test]
    async fn an_unbounded_tag_list_is_refused() {
        // Tags become payload on every point and a keyword index in Qdrant.
        let (app, token, _core, cid) = one_artifact().await;
        let many: Vec<String> = (0..500).map(|i| format!("t{i}")).collect();
        let res = app
            .oneshot(patch_json(
                &format!("/api/v1/artifacts/{cid}"),
                &token,
                serde_json::json!({ "tags": many }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn an_overlong_tag_is_refused() {
        let (app, token, _core, cid) = one_artifact().await;
        let res = app
            .oneshot(patch_json(
                &format!("/api/v1/artifacts/{cid}"),
                &token,
                serde_json::json!({ "tags": ["x".repeat(500)] }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_rejected_field_leaves_the_other_fields_alone() {
        // Validation happens before any write, so a request that fails is a
        // request that changed nothing.
        let (app, token, core, cid) = one_artifact().await;
        let res = app
            .oneshot(patch_json(
                &format!("/api/v1/artifacts/{cid}"),
                &token,
                serde_json::json!({ "title": "a new title", "tags": ["x".repeat(500)] }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        let c = core.store.get_artifact(&cid).await.unwrap();
        assert_eq!(c.title.as_deref(), Some("a title"), "a half-applied PATCH");
        assert_eq!(c.embed_state, EmbedState::Embedded);
    }

    #[tokio::test]
    async fn a_blank_title_clears_it_rather_than_storing_whitespace() {
        let (app, token, core, cid) = one_artifact().await;
        app.oneshot(patch_json(
            &format!("/api/v1/artifacts/{cid}"),
            &token,
            serde_json::json!({ "title": "   " }),
        ))
        .await
        .unwrap();
        assert_eq!(core.store.get_artifact(&cid).await.unwrap().title, None);
    }

    #[tokio::test]
    async fn deleting_an_artifact_frees_whatever_it_was_hiding() {
        // Deleting a corpus heals for this reason; deleting one artifact left
        // its loser hidden in favour of an id that no longer exists until the
        // next sweep — which is never, with `consolidate.enabled = false`.
        let (app, token, core, keeper) = one_artifact().await;
        let src = core
            .store
            .insert_corpus("other", "web", None)
            .await
            .unwrap();
        let hidden = core
            .store
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    ordinal: 0,
                    text: "the older copy".into(),
                    corpus_span: None,
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: None,
                    caveats: vec![],
                }],
            )
            .await
            .unwrap()[0]
            .id
            .clone();
        core.store
            .set_superseded_by(&hidden, Some(&keeper))
            .await
            .unwrap();

        let res = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/artifacts/{keeper}"))
                    .method("DELETE")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        assert!(
            core.store
                .get_artifact(&hidden)
                .await
                .unwrap()
                .superseded_by
                .is_none(),
            "the artifact is still hidden in favour of a deleted one"
        );
    }

    #[tokio::test]
    async fn an_empty_text_is_still_rejected() {
        let (app, token, _core, cid) = one_artifact().await;
        let res = app
            .oneshot(patch_json(
                &format!("/api/v1/artifacts/{cid}"),
                &token,
                serde_json::json!({ "text": "   " }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }
}
