use crate::core::fetch::only_a_url;
use crate::core::search::SearchQuery;
use crate::error::{Error, Result};
use crate::store::jobs::{FailedJob, Stage};
use crate::tenants::Tenant;
use crate::web::page::{Cursor, Page};
use crate::web::state::AppState;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};

/// One of `text`, `html` or `url`; more than one is refused, since any precedence would discard input.
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
    /// `"selection"` when `html` is a highlighted fragment; it exempts the capture from the extraction floor.
    #[serde(default)]
    pub scope: Option<String>,
}

/// Only a fragment that arrived as `html` is exempt: on the `url` path `scope` would switch off the login-wall guard.
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

/// `title` and `category`: an absent key leaves the field alone, an explicit `null` clears it.
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

/// Serde calls this only when the key is present, so the outer `Some` records presence.
fn explicit_null<'de, D, T>(d: D) -> std::result::Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    serde::Deserialize::deserialize(d).map(Some)
}

/// Also accepts `1`, `0` and an empty value (on); `serde_urlencoded` takes only `true` and `false`.
pub(crate) fn query_flag<'de, D>(d: D) -> std::result::Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = <Option<String> as serde::Deserialize>::deserialize(d)?;
    Ok(raw.map(|v| matches!(v.trim(), "" | "1" | "true" | "yes" | "on")))
}

/// Tags become a Qdrant payload index on every point, so they are bounded.
const MAX_TAGS: usize = 32;
const MAX_TAG_LEN: usize = 64;
const MAX_CATEGORY_LEN: usize = 64;
const MAX_TITLE_LEN: usize = 512;

/// Deduplicates because tags are ANDed in the search filter.
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

/// An empty value after trimming clears the field, so `""` and `null` agree.
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
    /// `None` while `[learn]` is off: absent, not zeroed, since zeroes read as a failure.
    pub learning: Option<Learning>,
    /// `None` while the sweep is off.
    pub reap: Option<ReapStatus>,
    /// Whether a speech model is configured, so `POST /transcribe` is open.
    pub transcribe: bool,
    /// The doors a client draws or hides on the same rule: `/ask`, images, verdicts and gaps, offers.
    pub asks: bool,
    /// Without a synthesizer captures stay verbatim and nothing is promoted or read for reminders.
    pub synthesizes: bool,
    pub vision: bool,
    pub learn: bool,
    pub recommend: bool,
    /// Units waiting on a generation model; on a phone, what the closed gate holds back.
    pub waiting_generation: i64,
    pub held: HeldBrief,
    /// Null on an empty base.
    pub last_kept: Option<LastKept>,
    /// In the `Accept-Language` language, from the classifier's own prototypes.
    pub examples: Examples,
    /// Whether the base is young enough that the idle column still teaches
    /// what a paste becomes.
    pub teach: bool,
}

#[derive(serde::Serialize)]
pub struct HeldBrief {
    pub corpora: i64,
    pub artifacts: i64,
}

#[derive(serde::Serialize)]
pub struct LastKept {
    pub id: String,
    pub label: String,
    pub named: bool,
    pub at: i64,
}

#[derive(serde::Serialize)]
pub struct Examples {
    pub lang: &'static str,
    pub remind: &'static str,
}

#[derive(serde::Serialize)]
pub struct ReapStatus {
    pub judged: i64,
    pub reaped: i64,
    pub rescued: i64,
    pub retired_waiting: i64,
}

/// The counts the insights page renders, in a shape a shell can read.
#[derive(serde::Serialize)]
pub struct Learning {
    pub pursuits_open: i64,
    pub pursuits_unsatisfied: i64,
    pub from_pursuits: i64,
    pub gaps_open: i64,
}

/// `origin` is derived from which field arrived; it is the only record of how a document got here.
use crate::core::ingest::ORIGIN_WEB;
const ORIGIN_EXTENSION: &str = "extension";

/// Off the async worker: these DOM walks can be megabytes, and `Readability` is `!Send`.
use crate::core::extract::extract;

async fn ingest(
    tenant: Tenant,
    headers: axum::http::HeaderMap,
    Json(req): Json<IngestRequest>,
) -> Result<(StatusCode, Json<crate::core::ingest::IngestOutcome>)> {
    let lang = crate::web::state::capture_lang(&tenant, &headers).await;
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

    // Validated here too: the `html` plus `url` path never calls `fetch_html`, and the value is rendered as a link.
    let parsed_url = req
        .url
        .as_deref()
        .map(crate::core::fetch::parse_readable)
        .transpose()?;

    let floor = if floor_exempt(&req) {
        0
    } else {
        tenant.core.capture.min_extracted_chars
    };

    let mut title = req.title;
    let (text, origin) = if let Some(text) = req.text {
        (text, ORIGIN_WEB)
    } else if let Some(html) = req.html {
        let page = extract(html, parsed_url.clone(), floor).await?;
        title = title.or(page.title);
        (page.markdown, ORIGIN_EXTENSION)
    } else {
        // 202 when what was stored is still to be read, as the upload doors answer.
        let u = parsed_url
            .as_ref()
            .ok_or_else(|| Error::Validation("url: missing".into()))?;
        let out = tenant.core.ingest_url(u, title, None, lang).await?;
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
                .with_lang(lang)
                .with_title(title)
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

#[derive(serde::Deserialize, Default)]
pub struct CaptureQuery {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    /// The door's IANA zone, so a date in the text is read where it was written.
    #[serde(default)]
    pub tz: Option<String>,
    /// `remind` to skip the classifier and date the note as a reminder.
    #[serde(default)]
    pub intent: Option<String>,
    /// The ask event's id when the box was filled from an answer; the capture then records the question and its sources.
    #[serde(default)]
    pub from_ask: Option<String>,
}

/// Validated here so a typo is a 400, not a silently ignored parameter.
pub(crate) struct CaptureTime {
    pub tz: Option<String>,
    pub intent: Option<crate::core::moments::Intent>,
}

pub(crate) fn capture_time(tz: Option<String>, intent: Option<String>) -> Result<CaptureTime> {
    // An unknown zone is a 400; falling back to UTC would shift every date in the note.
    let tz = match tz.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        None => None,
        Some(name) => match name.parse::<chrono_tz::Tz>() {
            Ok(z) => Some(z.name().to_string()),
            Err(_) => {
                return Err(Error::Validation(format!(
                    "tz={name}: not an IANA zone name"
                )));
            }
        },
    };
    let intent = match intent.as_deref().map(str::trim).filter(|i| !i.is_empty()) {
        None => None,
        Some("remind") => Some(crate::core::moments::Intent::Remind),
        Some(other) => {
            return Err(Error::Validation(format!(
                "intent={other}: only `remind` can be asked for"
            )));
        }
    };
    Ok(CaptureTime { tz, intent })
}

/// `intent` cannot apply to a fetched page or a file, so it is refused.
/// `tz` is ambient and let through: refusing it turned away captures from every client that sends it.
pub(crate) fn refuse_time_fields(intent: Option<&str>) -> Result<()> {
    if !intent.is_some_and(|v| !v.trim().is_empty()) {
        return Ok(());
    }
    Err(Error::Validation(
        "intent only applies to a text capture — a link or a file is read by the server, \
         and any date in it comes from what it turns out to be"
            .into(),
    ))
}

/// `200` already held, `202` still to be read, `201` complete.
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

/// Unlike `read_upload`, accepts many files (a share sheet's photos); a part with a filename is a file.
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

/// Untagged: an object for one capture, an array for several.
#[derive(serde::Serialize)]
#[serde(untagged)]
pub(crate) enum Captured {
    One(crate::core::ingest::IngestOutcome),
    Many(Vec<crate::core::ingest::IngestOutcome>),
}

fn not_text(e: crate::core::ingest::NotText, undecodable: &str) -> Error {
    Error::Validation(match e {
        crate::core::ingest::NotText::Undecodable => undecodable.into(),
        crate::core::ingest::NotText::Nul => crate::core::ingest::NotText::NUL_MESSAGE.into(),
    })
}

/// For a client that has not classified what it holds; every branch ends in an existing ingest call.
async fn capture(
    tenant: Tenant,
    Query(q): Query<CaptureQuery>,
    req: axum::extract::Request,
) -> Result<Response> {
    use axum::extract::FromRequest;
    let lang = crate::web::state::capture_lang(&tenant, req.headers()).await;
    let content_type = req
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let time = capture_time(q.tz.clone(), q.intent.clone())?;

    if content_type.starts_with("text/plain") {
        let bytes = axum::body::Bytes::from_request(req, &())
            .await
            .map_err(|e| Error::Validation(format!("body: {e}")))?;
        if bytes.len() > crate::web::MAX_BODY_BYTES {
            return Err(Error::Validation(
                "that text is over the 8 MB limit for a text capture".into(),
            ));
        }
        // Refused rather than lossily converted: see `decode_text`.
        let text = crate::core::ingest::decode_text(bytes.to_vec())
            .map_err(|e| not_text(e, "that body is not valid UTF-8 text"))?;
        let out = match only_a_url(&text) {
            Some(u) => {
                // The body is one link, so this is a fetch; see `refuse_time_fields`.
                refuse_time_fields(q.intent.as_deref())?;
                tenant.core.ingest_url(&u, q.title, q.note, lang).await?
            }
            None => {
                let mut c = crate::core::ingest::Capture::new(text, ORIGIN_WEB)
                    .with_lang(lang)
                    .with_title(q.title)
                    .with_note(q.note)
                    .with_tz(time.tz)
                    .with_intent(time.intent);
                if let Some(ask) = q.from_ask.as_deref().filter(|a| !a.is_empty())
                    && let Some(ev) = tenant.core.store.ask_event(ask).await?
                {
                    c = c.with_ask(&ev.id, &ev.question, &ev.citations);
                }
                tenant.core.ingest_capture(c).await?
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
        // Read before `capture_time` takes them, so the refusal can name form fields too.
        let raw_tz = fields.remove("tz");
        let raw_intent = fields.remove("intent");
        let time = capture_time(
            q.tz.clone().or_else(|| raw_tz.clone()),
            q.intent.clone().or_else(|| raw_intent.clone()),
        )?;
        let mut out: Vec<crate::core::ingest::IngestOutcome> = Vec::new();

        // A share sheet sends `url` and `text` together; the url wins, since the text usually repeats the title.
        let shared_url = fields.remove("url").or_else(|| {
            fields
                .get("text")
                .and_then(|t| only_a_url(t).map(|u| u.to_string()))
        });
        // No text capture here, so no `intent`; judged after `shared_url`, since a url takes the text with it.
        if shared_url.is_some() || !fields.contains_key("text") {
            refuse_time_fields(q.intent.as_deref().or(raw_intent.as_deref()))?;
        }
        if let Some(raw) = shared_url {
            // A link that cannot be read is stored as the link and this text; see `Core::ingest_link`.
            let shared = fields.remove("text");
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
                    .ingest_link(&u, title.clone(), note.clone(), shared, lang)
                    .await?,
            );
        } else if let Some(text) = fields.remove("text") {
            out.push(
                tenant
                    .core
                    .ingest_capture(
                        crate::core::ingest::Capture::new(text, ORIGIN_WEB)
                            .with_lang(lang)
                            .with_title(title.clone())
                            .with_note(note.clone())
                            .with_tz(time.tz.clone())
                            .with_intent(time.intent),
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
                        lang,
                    )
                    .await?,
            );
        }

        return match out.len() {
            0 => Err(Error::Validation(
                "nothing to capture: send `text`, `url`, or at least one file part".into(),
            )),
            1 => {
                let one = out
                    .pop()
                    .ok_or_else(|| Error::Internal("one capture expected".into()))?;
                Ok((code_for(&one), Json(Captured::One(one))).into_response())
            }
            _ => {
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
        // Each kind re-imposes its own ceiling, or widening the route for one would widen it for all.
        if bytes.len() > ceiling {
            return Err(Error::Validation(format!(
                "that {what} is over the {} MB limit for a {what} capture",
                ceiling / (1024 * 1024)
            )));
        }
        refuse_time_fields(q.intent.as_deref())?;
        let out = tenant
            .core
            .ingest_file(bytes.to_vec(), None, q.title, q.note, ORIGIN_WEB, lang)
            .await?;
        return Ok((code_for(&out), Json(out)).into_response());
    }

    Err(Error::Validation(format!(
        "`{content_type}` is not a type this door reads — send text/plain, \
         application/pdf, an image, or multipart/form-data"
    )))
}

const ORIGIN_UPLOAD: &str = "upload";

/// Consulted only when the part carried no `Content-Type`.
fn named_pdf(filename: Option<&str>) -> bool {
    filename.is_some_and(|n| {
        std::path::Path::new(n)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
    })
}

/// Consulted only when the part carried no `Content-Type`; case-insensitive.
fn named_txt(filename: Option<&str>) -> bool {
    filename.is_some_and(|n| {
        std::path::Path::new(n)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("txt"))
    })
}

pub(crate) struct FilePart {
    pub(crate) filename: Option<String>,
    /// The part's `Content-Type`, or "" when it carried none.
    pub(crate) declared: String,
    pub(crate) bytes: axum::body::Bytes,
}

struct UploadParts {
    file: Option<FilePart>,
    /// Only non-blank values are kept.
    fields: std::collections::HashMap<&'static str, String>,
}

/// A second file part is refused: whichever one won would be wrong for half the clients.
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

/// `.txt` and PDF only; anything else is refused by name.
async fn upload(
    tenant: Tenant,
    headers: axum::http::HeaderMap,
    multipart: axum::extract::Multipart,
) -> Result<(StatusCode, Json<crate::core::ingest::IngestOutcome>)> {
    let lang = crate::web::state::capture_lang(&tenant, &headers).await;
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
    // An absent `Content-Type` moves the check to the filename; it is not a pass.
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
            // Text is bounded by `MAX_BODY_BYTES` even though the route allows PDF-sized bodies.
            if bytes.len() > crate::web::MAX_BODY_BYTES {
                return Err(Error::Validation(
                    "that text file is over the 8 MB limit for a text capture".into(),
                ));
            }
            // Refused rather than lossily converted: see `decode_text`.
            let text = crate::core::ingest::decode_text(bytes.to_vec())
                .map_err(|e| not_text(e, "that file is not valid UTF-8 text"))?;
            let size = bytes.len();

            let out = tenant
                .core
                .ingest_capture(
                    crate::core::ingest::Capture::new(text, ORIGIN_UPLOAD)
                        .with_lang(lang)
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
                    lang,
                })
                .await?;
            // 202: stored, but extraction is still queued.
            let code = if out.duplicate {
                StatusCode::OK
            } else {
                StatusCode::ACCEPTED
            };
            Ok((code, Json(out)))
        }
    }
}

enum Kind {
    Text,
    Pdf,
}

/// Parts: `image` (required), `title_hint`, `note`; the reading happens in a job.
async fn upload_image(
    tenant: Tenant,
    headers: axum::http::HeaderMap,
    multipart: axum::extract::Multipart,
) -> Result<(StatusCode, Json<crate::core::ingest::IngestOutcome>)> {
    let lang = crate::web::state::capture_lang(&tenant, &headers).await;
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
            lang,
        })
        .await?;
    // 202: stored, but the reading is still queued.
    let code = if out.duplicate {
        StatusCode::OK
    } else {
        StatusCode::ACCEPTED
    };
    Ok((code, Json(out)))
}

/// The ETag of a corpus's bytes, or the `304` that ends the request; from `content_hash`, so the blob is not read.
/// A preview's tag carries the build version, since a later build may derive it differently.
async fn bytes_tag(
    tenant: &Tenant,
    id: &str,
    variant: &str,
    headers: &axum::http::HeaderMap,
) -> Result<std::result::Result<String, Response>> {
    let Some(hash) = tenant.core.store.content_hash_of(id).await? else {
        return Err(Error::NotFound);
    };
    let tag = format!("\"{variant}-{hash}\"");
    let asked = headers
        .get(axum::http::header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok());
    if asked.is_some_and(|a| crate::web::etag::matches(a, &tag)) {
        let mut res = StatusCode::NOT_MODIFIED.into_response();
        if let Ok(v) = axum::http::HeaderValue::from_str(&tag) {
            res.headers_mut().insert(axum::http::header::ETAG, v);
        }
        return Ok(Err(res));
    }
    Ok(Ok(tag))
}

async fn get_file(
    tenant: Tenant,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<axum::response::Response> {
    let tag = match bytes_tag(&tenant, &id, "file", &headers).await? {
        Ok(tag) => tag,
        Err(not_modified) => return Ok(not_modified),
    };
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
            (axum::http::header::ETAG, tag),
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
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<ImageQuery>,
) -> Result<axum::response::Response> {
    let want_original = q
        .original
        .as_deref()
        .is_some_and(|v| v == "1" || v == "true");
    let variant = if want_original {
        "original".to_string()
    } else {
        format!("preview-{}", env!("CARGO_PKG_VERSION"))
    };
    let tag = match bytes_tag(&tenant, &id, &variant, &headers).await? {
        Ok(tag) => tag,
        Err(not_modified) => return Ok(not_modified),
    };
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
            (axum::http::header::ETAG, tag),
        ],
        bytes,
    )
        .into_response())
}

#[derive(serde::Deserialize)]
pub struct ListParams {
    #[serde(default = "default_limit")]
    pub limit: i64,
    /// The `next` of the page before. See `web::page`.
    pub after: Option<String>,
}
fn default_limit() -> i64 {
    50
}

/// Lists carry summaries; `GET /corpora/{id}` carries `raw_text`.
#[derive(serde::Serialize)]
pub struct CorpusRow {
    pub id: String,
    pub origin: String,
    /// A stored name (`title_hint`), or the text's opening while no name is stored.
    pub label: String,
    pub named: bool,
    pub status: crate::store::corpora::CorpusStatus,
    pub created_at: i64,
    pub updated_at: i64,
    pub coverage: Option<f64>,
    pub near_dupe_of: Option<String>,
    pub near_dupe_score: Option<f64>,
    pub source_url: Option<String>,
    pub restored_at: Option<i64>,
    pub metadata: serde_json::Value,
}

impl From<crate::store::corpora::CorpusSummary> for CorpusRow {
    fn from(c: crate::store::corpora::CorpusSummary) -> Self {
        CorpusRow {
            named: c.title_hint.is_some(),
            label: crate::web::ui::corpus_label(c.title_hint, &c.opening, &c.origin),
            id: c.id,
            origin: c.origin,
            status: c.status,
            created_at: c.created_at,
            updated_at: c.updated_at,
            coverage: c.coverage,
            near_dupe_of: c.near_dupe_of,
            near_dupe_score: c.near_dupe_score,
            source_url: c.source_url,
            restored_at: c.restored_at,
            metadata: c.metadata,
        }
    }
}

async fn list_corpora(
    tenant: Tenant,
    Query(p): Query<ListParams>,
) -> Result<Json<Page<CorpusRow>>> {
    let limit = p.limit.clamp(1, 200);
    let before = match p.after.as_deref() {
        Some(a) => {
            let c = Cursor::decode(a)?;
            Some((c.at, c.id))
        }
        None => None,
    };
    let rows = tenant
        .core
        .store
        .list_corpus_summaries(before.as_ref(), limit + 1)
        .await?;
    let rows: Vec<CorpusRow> = rows.into_iter().map(CorpusRow::from).collect();
    Ok(Json(Page::of(rows, limit as usize, |r| Cursor {
        at: r.created_at,
        id: r.id.clone(),
    })))
}

#[derive(serde::Serialize)]
pub struct CorpusDetail {
    #[serde(flatten)]
    pub source: crate::store::corpora::Corpus,
    pub chunks: Vec<crate::store::artifacts::Chunk>,
}

async fn get_corpus(tenant: Tenant, Path(cid): Path<String>) -> Result<Json<CorpusDetail>> {
    let source = tenant.core.store.get_corpus(&cid).await?;
    let chunks = tenant
        .core
        .store
        .artifacts_for_corpus(&cid)
        .await?
        .into_iter()
        .filter(|c| c.status != crate::store::artifacts::ArtifactStatus::Trashed)
        .collect::<Vec<_>>();
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

async fn consolidation(tenant: Tenant) -> Result<Json<serde_json::Value>> {
    use crate::store::pairs::PairState;
    Ok(Json(serde_json::json!({
        "superseded": tenant.core.store.superseded_artifacts(100).await?,
        // Only pairs whose sides are both still in results; a hidden side disagrees with nothing a reader can reach.
        "contradictions": tenant
            .core
            .store
            .pairs_awaiting_review(PairState::Contradiction, 100)
            .await?,
        // Only older bases file these; emitted regardless, since clients index the key.
        "supersede_proposals": tenant
            .core
            .store
            .pairs_awaiting_review(PairState::Superseded, 100)
            .await?,
        // Only older bases file these; emitted regardless, since clients index the key.
        "discard_proposals": tenant
            .core
            .store
            .pairs_awaiting_review(PairState::Vacuous, 100)
            .await?,
        // Retired and always empty; emitted because clients index the key.
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
    /// Ranked hits; `[associate]` may append rows carrying `via` and a `score` of 0.
    /// Appended rows obey `tags` and `category` but not `include_*`: they are always active artifacts.
    pub limit: Option<usize>,
    pub tags: Option<String>,
    pub category: Option<String>,
    #[serde(default)]
    pub include_deprecated: bool,
    #[serde(default)]
    pub include_superseded: bool,
    /// Only `extension` is honoured; see `Door::from_client`.
    pub door: Option<String>,
    /// `false` skips the reranker (the typing opt-out); absent means true.
    pub rerank: Option<bool>,
    /// Changes the response to `{"results", "explanation"}`; never changes the order.
    #[serde(default, deserialize_with = "query_flag")]
    pub explain: Option<bool>,
}

/// Shared by both search doors so `typing`, `mark` and `rerank` cannot drift apart.
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
    // The extension and the app search as you type; marking prefixes would stamp `last_seen_at` and drain `resurface`.
    let typing = matches!(door, Door::Extension | Door::App);
    let explain = q.explain.unwrap_or(false);
    let query = SearchQuery {
        q: q.q,
        limit: q.limit.unwrap_or(0),
        // Also accepts a comma-separated list; repeated `?tags=` is awkward in a browser.
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
        mark: !typing,
        include_deprecated: q.include_deprecated,
        include_superseded: q.include_superseded,
        // An explicit `rerank` overrides the typing default.
        rerank: q.rerank.unwrap_or(!typing),
        explain,
    };
    // Scoped by door so two operators' typing does not coalesce and pursuits can match shell opens.
    // `Api` and `Mcp` stay unscoped: a bearer token is not a person.
    let origin: crate::store::feedback::Origin = match door {
        Door::Extension | Door::Cli | Door::App => door.by(tenant.user.subject.clone()),
        _ => door.into(),
    };
    (query, origin, explain)
}

/// Read as `Core::search` reads it, so asking for an explanation cannot change what a door returns.
fn search_cap(tenant: &Tenant) -> Option<usize> {
    tenant
        .core
        .ranking
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .per_source_cap
}

async fn search(tenant: Tenant, Query(q): Query<SearchParams>) -> Result<Json<serde_json::Value>> {
    let cap = search_cap(&tenant);
    let (query, origin, explain) = search_request(&tenant, q);
    let (results, outcome) = tenant.core.search_with(&query, cap, origin).await?;
    let mut body = search_body(&results, explain.then_some(&outcome.explanation))?;
    if let Some(ev) = outcome.event {
        body["event"] = serde_json::Value::String(ev);
    }
    // Best-effort: failing to read a continuation marker must not cost the results.
    let ids: Vec<String> = results.iter().map(|r| r.artifact_id.clone()).collect();
    if let Ok(next) = tenant.core.store.continuations_of(&ids).await
        && let Some(items) = body["items"].as_array_mut()
    {
        for (item, r) in items.iter_mut().zip(&results) {
            if let Some(n) = next.get(&r.artifact_id) {
                item["continues_to"] = serde_json::Value::String(n.clone());
            }
        }
    }
    Ok(Json(body))
}

/// `explain` adds a key beside `items` and changes nothing else.
/// Rows go out as `SearchResult` serialises them, so `weak` and `past_cliff` cannot be dropped.
pub(crate) fn search_body(
    results: &[crate::core::search::SearchResult],
    explanation: Option<&crate::core::explain::SearchExplanation>,
) -> Result<serde_json::Value> {
    let mut body = serde_json::json!({ "items": results, "next": null });
    if let Some(e) = explanation {
        body["explanation"] = serde_json::to_value(e)
            .map_err(|e| Error::Internal(format!("serialising the explanation: {e}")))?;
        // The server's own sentence, so every door says the same one.
        body["reranked"] = serde_json::Value::Bool(e.reranked);
        if let Some(items) = body["items"].as_array_mut() {
            for (item, r) in items.iter_mut().zip(results) {
                if let Some(why) = r.explanation.as_ref().and_then(crate::web::ui::why_ranked) {
                    item["why_ranked"] = serde_json::Value::String(why);
                }
            }
        }
    }
    Ok(body)
}

#[derive(serde::Deserialize)]
pub struct StaleParams {
    pub limit: Option<usize>,
}

/// Read-only: nothing here changes an artifact or feeds ranking.
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
    Query(q): Query<AskParams>,
    Json(req): Json<crate::core::ask::AskRequest>,
) -> Result<Json<crate::core::ask::AskResponse>> {
    // No ask model, no ask door: the route is not there. See `Core::asks`.
    if !tenant.core.asks() {
        return Err(Error::NotFound);
    }
    let origin = ask_origin(&q, &tenant, crate::store::feedback::Door::Api);
    Ok(Json(tenant.core.ask(&req, origin).await?))
}

async fn decide(
    tenant: Tenant,
    Query(q): Query<AskParams>,
    Json(body): Json<crate::core::decide::wire::DecideBody>,
) -> Result<Json<crate::core::decide::Decision>> {
    let req = body.into_request()?;
    let origin = ask_origin(&q, &tenant, crate::store::feedback::Door::Api);
    Ok(Json(tenant.core.decide(&req, origin).await?))
}

/// In the query rather than the body: `AskRequest` is built by hand by many internal callers.
#[derive(serde::Deserialize)]
pub struct AskParams {
    pub door: Option<String>,
}

/// `default` differs per route; a bearer token is not a person, so it gets no scope.
fn ask_origin(
    q: &AskParams,
    tenant: &Tenant,
    default: crate::store::feedback::Door,
) -> crate::store::feedback::Origin {
    use crate::store::feedback::Door;
    let door = q.door.as_deref().map(Door::from_client).unwrap_or(default);
    match door {
        Door::Extension | Door::Cli | Door::App => door.by(tenant.user.subject.clone()),
        _ => door.into(),
    }
}

#[derive(serde::Deserialize)]
pub struct ResurfaceParams {
    pub limit: Option<usize>,
}

async fn resurface(
    tenant: Tenant,
    Query(p): Query<ResurfaceParams>,
) -> Result<Json<Page<crate::core::search::SearchResult>>> {
    Ok(Json(Page::whole(
        tenant.core.resurface(p.limit.unwrap_or(5)).await?,
    )))
}

/// Not the `Corpus` row, which carries `raw_text` (for a book, the whole book).
#[derive(serde::Serialize)]
pub struct SourceRef {
    pub id: String,
    pub title: Option<String>,
    pub origin: String,
    pub source_url: Option<String>,
}

/// The chunk is flattened so every key this door has answered with stays top level.
/// `source` is `None` for a merged artifact and for one whose corpus was deleted.
#[derive(serde::Serialize)]
pub struct ArtifactDetail {
    #[serde(flatten)]
    pub artifact: crate::store::artifacts::Chunk,
    pub source: Option<SourceRef>,
    /// Set only where `?event=` named the caller's own search and the artifact is in results; without it there is no verdict bar.
    pub search_event: Option<String>,
}

#[derive(serde::Deserialize)]
pub struct OpenParams {
    pub event: Option<String>,
}

async fn get_artifact(
    tenant: Tenant,
    Path(cid): Path<String>,
    Query(p): Query<OpenParams>,
) -> Result<Json<ArtifactDetail>> {
    let chunk = tenant.core.store.get_artifact(&cid).await?;
    // Attributed under the same three guards as the web pane; see `artifact::artifact_detail`.
    let mut search_event = None;
    if tenant.core.learn.enabled
        && chunk.in_results()
        && let Some(event) = p.event.as_deref()
        && tenant
            .core
            .store
            .event_is_mine(event, &tenant.user.subject)
            .await?
        && tenant.core.store.open_event(event, &cid).await?
    {
        search_event = Some(event.to_string());
    }
    // A deleted document does not make its artifact unreadable.
    let source = match &chunk.corpus_id {
        // `corpus_origin`, not `get_corpus`: the row carries `raw_text`.
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
    // No `via` and no session: a bearer token is not a conversation. Written after the read, so a 404 engages nothing.
    tenant.core.mark_artifact_seen(&cid);
    tenant
        .core
        .record_interaction(&cid, None, Some(&tenant.user.subject));
    Ok(Json(ArtifactDetail {
        artifact: chunk,
        source,
        search_event,
    }))
}

#[derive(serde::Serialize)]
pub struct OfferCard {
    pub artifact_id: String,
    /// The artifact's name, or — with `named: false` — its snippet standing in.
    pub label: String,
    pub named: bool,
    pub snippet: String,
    /// The ladder's word: `pattern`, `similar`, `tentative`, `random`; `seen` and an open send it back.
    pub rung: &'static str,
    pub slot: Option<i64>,
    /// How many earlier occasions the offer rests on. Zero on `random`.
    pub events: i64,
    /// The signals that decided it, largest first. Empty on `random`.
    pub blocks: Vec<&'static str>,
    pub at: Option<i64>,
    pub at_tz: Option<String>,
}

/// A POST because it records the situation whether or not anything is offered; the body is stored as sent.
/// `{"offer": null}` is the ordinary answer, not an error.
async fn context(tenant: Tenant, bundle: String) -> Json<serde_json::Value> {
    let offer = crate::web::ui::compute_offer(&tenant, &bundle)
        .await
        .map(|(o, snippet)| OfferCard {
            named: !o.title.is_empty(),
            label: if o.title.is_empty() {
                snippet.clone()
            } else {
                o.title
            },
            artifact_id: o.artifact_id,
            snippet,
            rung: o.rung.as_str(),
            slot: o.slot,
            events: o.events,
            blocks: o.blocks,
            at: o.at,
            at_tz: o.at_tz,
        });
    Json(serde_json::json!({ "offer": offer }))
}

#[derive(serde::Deserialize)]
struct SeenBody {
    artifact_id: String,
    rung: String,
    slot: Option<i64>,
}

/// The only writer of `recommended_shown`, so a client calls it once the card is on screen. Always `204`.
async fn context_seen(tenant: Tenant, Json(b): Json<SeenBody>) -> StatusCode {
    crate::web::ui::record_seen(&tenant, &b.artifact_id, &b.rung, b.slot).await;
    StatusCode::NO_CONTENT
}

/// A sub-resource: a tree is up to two hundred reads, and reading one records no open.
/// No history answers the empty tree, not a 404; `truncated` says the tree stops early.
async fn artifact_lineage(
    tenant: Tenant,
    Path(id): Path<String>,
) -> Result<Json<crate::web::lineage_view::Lineage>> {
    tenant.core.store.get_artifact(&id).await?;
    Ok(Json(
        crate::web::lineage_view::build(&tenant.core.store, &id).await?,
    ))
}

#[derive(serde::Serialize)]
pub struct VersionRow {
    pub n: i64,
    pub title: Option<String>,
    pub text: String,
    pub caveats: Vec<String>,
    pub created_at: i64,
}

/// Oldest first. Read-only: restoring one is a decision and lives with the decisions.
async fn artifact_versions(
    tenant: Tenant,
    Path(id): Path<String>,
) -> Result<Json<Page<VersionRow>>> {
    tenant.core.store.get_artifact(&id).await?;
    let rows = tenant.core.store.versions_of(&id).await?;
    Ok(Json(Page::whole(
        rows.into_iter()
            .map(|v| VersionRow {
                n: v.n,
                title: v.title,
                text: v.text,
                caveats: v.caveats,
                created_at: v.created_at,
            })
            .collect(),
    )))
}

/// A label that says whether it is a name, and the opening of the text.
#[derive(serde::Serialize)]
pub struct RelatedRow {
    pub id: String,
    pub label: String,
    pub named: bool,
    pub snippet: String,
    /// Only on `seen_together`; a neighbour needs no why.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    /// Only on `seen_together`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub corpus_title: Option<String>,
}

#[derive(serde::Serialize)]
pub struct RelatedAnswer {
    pub related: Vec<RelatedRow>,
    /// What this has been reached for together with, learned from co-retrieval
    /// rather than resemblance. Empty while `[learn]` is off.
    pub seen_together: Vec<RelatedRow>,
    /// Where this passage stops mid-sentence.
    pub continues_at: Option<String>,
}

/// Its own route: the detail records an open, and a vector store outage must not refuse the text.
async fn artifact_related(tenant: Tenant, Path(id): Path<String>) -> Result<Json<RelatedAnswer>> {
    let c = tenant.core.store.get_artifact(&id).await?;
    let d = crate::web::artifact::pivots(&tenant.core, &c).await;
    let row = |id: String, title: String, snippet: String| RelatedRow {
        named: !title.is_empty(),
        label: if title.is_empty() {
            snippet.clone()
        } else {
            title
        },
        id,
        snippet,
        why: None,
        corpus_title: None,
    };
    Ok(Json(RelatedAnswer {
        related: d
            .related
            .into_iter()
            .map(|r| row(r.id, r.title, r.snippet))
            .collect(),
        seen_together: d
            .seen_together
            .into_iter()
            .map(|r| RelatedRow {
                why: r.why,
                corpus_title: Some(r.corpus_title),
                ..row(r.id, r.title, r.snippet)
            })
            .collect(),
        continues_at: d.continues_at,
    }))
}

/// `in_span` marks the lines the artifact was drawn from, as opposed to context.
#[derive(serde::Serialize)]
pub struct SourceLine {
    pub number: i64,
    pub text: String,
    pub in_span: bool,
}

#[derive(serde::Serialize)]
pub struct SourceSlice {
    /// `None` for a merged artifact and for one whose document is gone.
    pub corpus_id: Option<String>,
    /// `lines 118–141`, `line 6`, or `extraction lines 118–141` where the lines are docling's.
    pub label: String,
    pub lines: Vec<SourceLine>,
}

/// The same slice as the pane (`corpus_view::slice`), so the two agree; empty for a merge.
async fn artifact_source(tenant: Tenant, Path(id): Path<String>) -> Result<Json<SourceSlice>> {
    let c = tenant.core.store.get_artifact(&id).await?;
    let src = match &c.corpus_id {
        Some(cid) => match tenant.core.store.get_corpus(cid).await {
            Ok(s) => Some(s),
            // A document deleted since leaves its artifacts readable, and this
            // answers with nothing to show rather than a 404 for the artifact.
            Err(Error::NotFound) => None,
            Err(e) => return Err(e),
        },
        None => None,
    };
    let slice = match &src {
        Some(s) => crate::web::corpus_view::slice(s, c.corpus_span.as_ref(), 3),
        None => crate::web::corpus_view::CorpusSlice::default(),
    };
    Ok(Json(SourceSlice {
        corpus_id: src.map(|s| s.id),
        label: slice.label,
        lines: slice
            .lines
            .into_iter()
            .map(|l| SourceLine {
                number: l.number,
                text: l.text,
                in_span: l.in_span,
            })
            .collect(),
    }))
}

/// Nothing is stored. `404` where no speech model is configured; `GET /status` reports it as `transcribe`.
async fn transcribe(tenant: Tenant, multipart: axum::extract::Multipart) -> Result<Response> {
    let text = crate::web::workspace::hear(&tenant, multipart).await?;
    Ok((
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        text,
    )
        .into_response())
}

async fn patch_artifact(
    tenant: Tenant,
    Path(cid): Path<String>,
    Json(req): Json<PatchArtifactRequest>,
) -> Result<Json<crate::store::artifacts::Chunk>> {
    if req.text.is_none() && req.title.is_none() && req.category.is_none() && req.tags.is_none() {
        return Err(Error::Validation("no fields to update".into()));
    }
    // Validate everything before writing anything, so a rejected request leaves nothing half-applied.
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
    // Folded onto the same closed list synthesis uses, rather than rejected.
    let category = req
        .category
        .map(|c| clean_optional(c, MAX_CATEGORY_LEN, "category"))
        .transpose()?
        .map(|c| c.map(|v| crate::infer::prompt::normalize_category(&v)));
    let tags = req.tags.as_deref().map(clean_tags).transpose()?;

    tenant.core.store.get_artifact(&cid).await?;

    // The embedder sees title and body, so only those invalidate the stored vector.
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
        // Only when the chunk has a point: a pending embed job writes the whole payload anyway.
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

/// Into Trash; `POST …/restore` takes it back for 30 days.
async fn delete_artifact(tenant: Tenant, Path(cid): Path<String>) -> Result<StatusCode> {
    tenant.core.trash_artifact(&cid).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn restore_artifact(tenant: Tenant, Path(cid): Path<String>) -> Result<StatusCode> {
    tenant.core.restore_artifact(&cid).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn status(tenant: Tenant, headers: axum::http::HeaderMap) -> Result<Json<StatusResponse>> {
    let (held_corpora, held_artifacts) = tenant.core.store.held_brief().await?;
    let last_kept = tenant
        .core
        .store
        .recent_captures(1)
        .await?
        .into_iter()
        .next()
        .map(|(id, title_hint, origin, created_at, opening)| LastKept {
            named: title_hint.is_some(),
            label: crate::web::ui::corpus_label(title_hint, &opening, &origin),
            id,
            at: created_at,
        });
    let (example_remind, example_lang) = crate::web::workspace::examples(&tenant.core, &headers);
    let corpus_rows = sqlx::query!("SELECT status, COUNT(*) AS n FROM corpora GROUP BY status")
        .fetch_all(&tenant.core.store.pool)
        .await?;
    let chunks: i64 = sqlx::query_scalar!("SELECT COUNT(*) AS n FROM artifacts")
        .fetch_one(&tenant.core.store.pool)
        .await?;

    // Counted, not paged: a capped list reports its page size as the total.
    // The predicates are the store's, so the terminal and the browser count the same rows.
    let learning = match tenant.core.learn.enabled {
        false => None,
        true => Some(Learning {
            pursuits_open: tenant.core.store.count_pursuits("open").await?,
            // Still on the gap list: a later answering capture leaves `unsatisfied` alone.
            pursuits_unsatisfied: tenant
                .core
                .store
                .count_open_pursuit_gaps(tenant.core.embedder.model())
                .await?,
            from_pursuits: tenant.core.store.count_synthesized_artifacts().await?,
            // `?`, not `unwrap_or_default`: a zero here would read as a healthy base.
            gaps_open: tenant
                .core
                .store
                .count_open_gaps(tenant.core.embedder.model(), tenant.core.weak_below())
                .await?,
        }),
    };

    let reap = if tenant.core.reap.enabled && tenant.core.judge.is_some() {
        let last = tenant.core.store.last_sweep_run("reap").await?;
        let n = |d: &serde_json::Value, k: &str| d.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
        let detail: serde_json::Value = last
            .as_ref()
            .and_then(|r| serde_json::from_str(&r.detail).ok())
            .unwrap_or(serde_json::Value::Null);
        Some(ReapStatus {
            judged: n(&detail, "judged"),
            reaped: n(&detail, "reaped"),
            rescued: n(&detail, "rescued"),
            retired_waiting: tenant.core.store.retired_unreaped_count().await?,
        })
    } else {
        None
    };

    Ok(Json(StatusResponse {
        learning,
        reap,
        transcribe: tenant.core.transcriber.is_some(),
        asks: tenant.core.asks(),
        synthesizes: tenant.core.synthesizes(),
        vision: tenant.core.describer.is_some(),
        learn: tenant.core.learn.enabled,
        recommend: tenant.core.recommends(),
        waiting_generation: tenant
            .core
            .store
            .control
            .waiting_on(
                &tenant.core.store.subject,
                &crate::store::jobs::Stage::GENERATORS,
            )
            .await?,
        held: HeldBrief {
            corpora: held_corpora,
            artifacts: held_artifacts,
        },
        last_kept,
        examples: Examples {
            lang: example_lang,
            remind: example_remind,
        },
        teach: held_corpora < crate::web::ui::TEACH_UNTIL_SOURCES,
        sources: corpus_rows.into_iter().map(|r| (r.status, r.n)).collect(),
        jobs: tenant.core.store.job_counts().await?,
        failed: tenant.core.store.failed_jobs(50).await?,
        oldest_pending_secs: tenant.core.store.oldest_pending_age().await?,
        chunks,
        // Qdrant briefly unreachable must not fail the status page.
        vectors: tenant.core.vectors.count().await.unwrap_or(0),
    }))
}

/// For the extension's panel: it reads the stream with `fetch` (EventSource cannot send a bearer header), so one POST streams its own response.
async fn ask_stream(
    tenant: Tenant,
    Query(q): Query<AskParams>,
    Json(req): Json<crate::core::ask::AskRequest>,
) -> Result<Response> {
    // No ask model, no ask door: the route is not there. See `Core::asks`.
    if !tenant.core.asks() {
        return Err(Error::NotFound);
    }
    use tokio_stream::StreamExt as _;

    let core = tenant.core.clone();
    // The panel names no door and keeps `Extension`.
    let origin = ask_origin(&q, &tenant, crate::store::feedback::Door::Extension);
    let events = async_stream::stream! {
        let s = core.ask_events(&req, origin);
        tokio::pin!(s);
        while let Some(ev) = s.next().await {
            // Every arm is a frame: an `Err` into `Sse` ends the body with no frame, which reads as an answer that stopped.
            yield Ok::<_, Error>(match ev {
                // Terminal: the `try_stream!` producer ends at its first error.
                Ok(e) => api_sse_event(e).unwrap_or_else(|e| error_frame("ask", &e)),
                Err(e) => error_frame("ask", &e),
            });
        }
    };
    // Kept alive: a slow model outlasts a proxy's idle timeout.
    Ok(Sse::new(events)
        .keep_alive(KeepAlive::default())
        .into_response())
}

/// One POST streams its own response, as `ask_stream` does.
async fn decide_stream(
    tenant: Tenant,
    Query(q): Query<AskParams>,
    Json(body): Json<crate::core::decide::wire::DecideBody>,
) -> Result<Response> {
    use tokio_stream::StreamExt as _;

    let req = body.into_request()?;
    let core = tenant.core.clone();
    let origin = ask_origin(&q, &tenant, crate::store::feedback::Door::Api);
    let events = async_stream::stream! {
        let s = core.decide_events(&req, origin);
        tokio::pin!(s);
        while let Some(ev) = s.next().await {
            // Terminal: the `try_stream!` producer ends at its first error.
            yield Ok::<_, Error>(match ev {
                Ok(e) => decide_sse_event(e).unwrap_or_else(|e| error_frame("decide", &e)),
                Err(e) => error_frame("decide", &e),
            });
        }
    };
    // Kept alive: sixteen reads on a slow model outlast a proxy's idle timeout.
    Ok(Sse::new(events)
        .keep_alive(KeepAlive::default())
        .into_response())
}

fn decide_sse_event(ev: crate::core::decide::DecideEvent) -> Result<SseEvent> {
    use crate::core::decide::DecideEvent::*;
    let (name, data) = match ev {
        Retrieved { passages } => ("retrieved", serde_json::json!({ "passages": passages })),
        Read(r) => (
            "read",
            serde_json::json!({
                "rank": r.rank,
                "of": r.of,
                "artifact_id": r.artifact_id,
                "title": r.title,
                "bears": r.bears,
                "bearing": r.bearing,
                "read": crate::core::decide::wire::read_json(&r.read),
            }),
        ),
        Done(d) => (
            "done",
            serde_json::to_value(*d)
                .map_err(|e| Error::Internal(format!("the decision would not serialise: {e}")))?,
        ),
    };
    Ok(SseEvent::default().event(name).data(data.to_string()))
}

/// Not an arm of `api_sse_event`: a test asserts the panel handles every frame name there.
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

/// A second route: `GET /search` answers a bare array to many clients, and one route should answer one shape.
async fn search_stream(tenant: Tenant, Query(q): Query<SearchParams>) -> Result<Response> {
    use tokio_stream::StreamExt as _;
    let cap = search_cap(&tenant);
    let (query, origin, _explain) = search_request(&tenant, q);
    let core = tenant.core.clone();
    let events = async_stream::stream! {
        let s = core.search_events(query, cap, origin);
        tokio::pin!(s);
        while let Some(ev) = s.next().await {
            // Terminal: the producer ends at its first error.
            yield Ok::<_, Error>(match ev {
                Ok(e) => search_sse_event(e).unwrap_or_else(|e| error_frame("search", &e)),
                Err(e) => error_frame("search", &e),
            });
        }
    };
    Ok(Sse::new(events)
        .keep_alive(KeepAlive::default())
        .into_response())
}

/// `stream` names the route in the log line.
fn error_frame(stream: &'static str, e: &Error) -> SseEvent {
    if e.status().is_server_error() {
        tracing::error!(error = %e, stream, "stream failed");
    } else {
        tracing::debug!(error = %e, stream, "stream rejected");
    }
    SseEvent::default()
        .event("error")
        .data(serde_json::json!({ "error": e.client_message() }).to_string())
}

/// Sends values, not HTML: the panel builds nodes with `textContent`. A test asserts the panel names every frame.
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
        // The panel replaces its draft with this, so the final screen is the server's answer.
        Done(d) => (
            "done",
            // Cannot fail for this type; returned rather than unwrapped so a panic cannot kill the stream.
            serde_json::to_value(*d)
                .map_err(|e| Error::Internal(format!("the answer would not serialise: {e}")))?,
        ),
    };
    Ok(SseEvent::default().event(name).data(data.to_string()))
}

pub fn api_router(image_max_bytes: usize, pdf_max_bytes: usize) -> Router<AppState> {
    Router::new()
        .route("/corpora", post(ingest).get(list_corpora))
        // Its ceiling is the widest branch's; each branch re-imposes its own inside the handler.
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
        .route("/search", get(search))
        .route("/search/stream", get(search_stream))
        .route("/ask", post(ask))
        .route("/ask/stream", post(ask_stream))
        .route("/decide", post(decide))
        .route("/decide/stream", post(decide_stream))
        .route("/resurface", get(resurface))
        .route("/days/{date}", get(crate::web::day::api_day))
        .route("/context", post(context))
        .route("/context/seen", post(context_seen))
        .route("/consolidation", get(consolidation))
        .route("/consolidation/stale", get(stale))
        .route(
            "/artifacts/{id}",
            get(get_artifact)
                .patch(patch_artifact)
                .delete(delete_artifact),
        )
        .route("/artifacts/{id}/restore", post(restore_artifact))
        .route("/artifacts/{id}/lineage", get(artifact_lineage))
        .route("/artifacts/{id}/versions", get(artifact_versions))
        .route("/artifacts/{id}/related", get(artifact_related))
        .route("/artifacts/{id}/source", get(artifact_source))
        // A recording is a few hundred kilobytes of PCM; the global limit is for text.
        .route(
            "/transcribe",
            post(transcribe).layer(axum::extract::DefaultBodyLimit::max(image_max_bytes)),
        )
        .route("/vectors/sample", get(crate::web::vbg::sample))
        .route("/status", get(status))
        .route("/moments", get(list_moments))
        .route("/moments/{id}/done", post(moment_done))
        .route("/moments/{id}/undone", post(moment_undone))
        .route("/moments/{id}/snooze", post(moment_snooze))
        .route("/moments/{id}/unsnooze", post(moment_unsnooze))
        .route("/artifacts/{id}/moments", post(set_moment))
        .merge(crate::web::judge::routes())
        .merge(crate::web::client::routes())
        .merge(crate::web::push::routes())
        .merge(crate::web::app::routes())
        // Every JSON read above, and any added later, revalidates by ETag.
        .layer(axum::middleware::from_fn(crate::web::etag::layer))
}

#[derive(serde::Deserialize)]
pub struct MomentsQuery {
    /// The lower bound of `kind=event` only; accepted but ignored on `due`.
    #[serde(default)]
    pub from: Option<i64>,
    /// Defaults to `time.horizon_hours` out.
    #[serde(default)]
    pub to: Option<i64>,
    /// `due` (the default) or `event`.
    #[serde(default)]
    pub kind: Option<String>,
}

/// `due`: open rows only, undated last; `event`: what refers to the window.
/// `due` has no lower bound, so overdue reminders are always listed.
async fn list_moments(
    tenant: Tenant,
    Query(q): Query<MomentsQuery>,
) -> Result<Json<Page<crate::store::moments::DueRow>>> {
    let now = tenant.core.clock.now();
    let from = q.from.unwrap_or(now);
    let to =
        q.to.unwrap_or(now + tenant.core.time.horizon_hours as i64 * 3_600);
    let rows = match q.kind.as_deref().unwrap_or("due") {
        // `now`, not `from`: `open_due`'s first argument is the snooze instant, and `from` would un-hide snoozed rows.
        "due" => tenant.core.store.open_due(now, to).await?,
        "event" => tenant.core.store.event_moments_between(from, to).await?,
        other => return Err(Error::Validation(format!("kind={other}: `due` or `event`"))),
    };
    Ok(Json(Page::whole(rows)))
}

/// `404` where there was no open row to strike, as `moment_snooze` answers.
async fn moment_done(tenant: Tenant, Path(id): Path<String>) -> Result<StatusCode> {
    if !tenant.core.complete_moment(&id).await? {
        return Err(Error::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn moment_undone(tenant: Tenant, Path(id): Path<String>) -> Result<StatusCode> {
    // Through `Core::uncomplete_moment`, so the successor is disarmed and the retired note restored.
    tenant.core.uncomplete_moment(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(serde::Deserialize)]
pub struct SnoozeBody {
    pub until: i64,
}

/// `until` must be later than now: a past snooze collapses the lead ladder and loses the 48 h to 30 min pushes.
/// 404 where nothing was put aside.
async fn moment_snooze(
    tenant: Tenant,
    Path(id): Path<String>,
    Json(b): Json<SnoozeBody>,
) -> Result<StatusCode> {
    let now = tenant.core.clock.now();
    if !(YEAR_ONE..=END_OF_9999).contains(&b.until) {
        return Err(Error::Validation(format!(
            "until={}: outside the range a date can be spelled in",
            b.until
        )));
    }
    if b.until <= now {
        return Err(Error::Validation(
            "until must be in the future; to put a reminder back on the band, \
             DELETE the snooze instead"
                .into(),
        ));
    }
    // An undated reminder has nothing to be put aside until, and `open_due` lists it whatever the snooze says.
    if let Some(m) = tenant.core.store.moment(&id).await?
        && m.at.is_none()
    {
        return Err(Error::Validation(
            "this reminder has no date, so there is nothing to put it aside until; \
             set a date first"
                .into(),
        ));
    }
    if !tenant.core.store.snooze(&id, b.until).await? {
        return Err(Error::NotFound);
    }
    tenant.core.store.rearm_remind().await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn moment_unsnooze(tenant: Tenant, Path(id): Path<String>) -> Result<StatusCode> {
    tenant.core.store.unsnooze(&id).await?;
    tenant.core.store.rearm_remind().await?;
    Ok(StatusCode::NO_CONTENT)
}

use crate::core::moments::{END_OF_9999, YEAR_ONE};

#[derive(serde::Deserialize)]
pub struct NewMomentBody {
    pub at: i64,
    #[serde(default)]
    pub tz: Option<String>,
    /// `due` (the default) or `event`.
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub rule: Option<String>,
}

/// A moment somebody set, on an artifact they name.
async fn set_moment(
    tenant: Tenant,
    Path(aid): Path<String>,
    Json(b): Json<NewMomentBody>,
) -> Result<(StatusCode, Json<serde_json::Value>)> {
    tenant.core.store.get_artifact(&aid).await?;
    let kind = match b.kind.as_deref().unwrap_or("due") {
        "due" => crate::store::moments::Kind::Due,
        "event" => crate::store::moments::Kind::Event,
        other => return Err(Error::Validation(format!("kind={other}: `due` or `event`"))),
    };
    if let Some(rule) = b.rule.as_deref() {
        crate::core::moments::validate_rule(rule).map_err(Error::Validation)?;
    }
    // `at` must be a representable timestamp: the band computes `abs(at - now)`.
    if chrono::DateTime::from_timestamp(b.at, 0).is_none()
        || !(YEAR_ONE..=END_OF_9999).contains(&b.at)
    {
        return Err(Error::Validation(format!(
            "at={}: seconds since the epoch, within the years 1 to 9999",
            b.at
        )));
    }
    // An unknown zone is refused rather than falling through to UTC.
    let tz = match b.tz.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        None => "UTC".to_string(),
        Some(name) => match name.parse::<chrono_tz::Tz>() {
            Ok(z) => z.name().to_string(),
            Err(_) => {
                return Err(Error::Validation(format!(
                    "tz={name}: not an IANA zone name"
                )));
            }
        },
    };
    let id = tenant
        .core
        .store
        .insert_moment(&crate::store::moments::NewMoment {
            artifact_id: aid,
            kind,
            at: Some(b.at),
            tz,
            rule: b.rule,
            source: crate::store::moments::Source::Set,
            span: None,
            series_id: None,
        })
        .await?;
    tenant.core.store.rearm_remind().await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!({ "id": id }))))
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

    /// Wraps a core a test has already adjusted in the real router.
    pub async fn app_from_core(
        core: crate::core::Core,
    ) -> (axum::Router, String, crate::core::Core) {
        let handle = core.clone();
        let (app, token) = app_with_token(core).await;
        (app, token, handle)
    }

    #[tokio::test]
    async fn the_consolidation_report_still_carries_every_key_it_ever_had() {
        // `merge_proposals` is always empty but still emitted, since clients index the key.
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
        assert_eq!(got[0].via, None);
    }

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

    async fn corpus_with_due(core: &crate::core::Core, rule: Option<&str>) -> (String, String) {
        use crate::store::moments::{Kind, NewMoment, Source};
        use chrono::TimeZone;
        let out = core
            .ingest_capture(crate::core::ingest::Capture::new("Pay rent", "ui"))
            .await
            .unwrap();
        crate::jobs::test_support::drain(core).await;
        let aid = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .into_iter()
            .find(|c| c.in_results())
            .expect("a live artifact")
            .id;
        let at = chrono_tz::Tz::Europe__Berlin
            .with_ymd_and_hms(2026, 9, 1, 9, 0, 0)
            .unwrap()
            .timestamp();
        let id = core
            .store
            .insert_moment(&NewMoment {
                artifact_id: aid,
                kind: Kind::Due,
                at: Some(at),
                tz: "Europe/Berlin".into(),
                rule: rule.map(str::to_string),
                source: Source::Cue,
                span: None,
                series_id: None,
            })
            .await
            .unwrap();
        (out.id, id)
    }

    #[tokio::test]
    async fn undo_through_the_api_brings_the_note_back_with_the_row() {
        let (app, token, core) = app_token_and_core().await;
        let (cid, id) = corpus_with_due(&core, None).await;

        let done = app
            .clone()
            .oneshot(post_json(
                &format!("/api/v1/moments/{id}/done"),
                &token,
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(done.status(), StatusCode::NO_CONTENT);
        assert!(
            core.store.is_retired(&cid).await.unwrap(),
            "completing the last reminder retires the note"
        );

        let undone = app
            .oneshot(post_json(
                &format!("/api/v1/moments/{id}/undone"),
                &token,
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(undone.status(), StatusCode::NO_CONTENT);
        assert!(
            !core.store.is_retired(&cid).await.unwrap(),
            "and undoing brings it back"
        );
    }

    #[tokio::test]
    async fn striking_a_reminder_that_is_not_there_is_a_404_and_not_a_204() {
        let (app, token, core) = app_token_and_core().await;
        let (_, id) = corpus_with_due(&core, None).await;

        let first = app
            .clone()
            .oneshot(post_json(
                &format!("/api/v1/moments/{id}/done"),
                &token,
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::NO_CONTENT, "there was a row");

        let again = app
            .clone()
            .oneshot(post_json(
                &format!("/api/v1/moments/{id}/done"),
                &token,
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(
            again.status(),
            StatusCode::NOT_FOUND,
            "and now there is not"
        );

        let never = app
            .oneshot(post_json(
                "/api/v1/moments/no-such-moment/done",
                &token,
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(never.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn undo_through_the_api_takes_the_next_occurrence_back_too() {
        let (app, token, core) = app_token_and_core().await;
        let (_, id) = corpus_with_due(&core, Some("FREQ=MONTHLY;BYMONTHDAY=1")).await;
        app.clone()
            .oneshot(post_json(
                &format!("/api/v1/moments/{id}/done"),
                &token,
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(
            core.store.open_due(0, i64::MAX).await.unwrap().len(),
            1,
            "the successor is armed"
        );

        app.oneshot(post_json(
            &format!("/api/v1/moments/{id}/undone"),
            &token,
            serde_json::json!({}),
        ))
        .await
        .unwrap();
        let open = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(
            open.len(),
            1,
            "one open row, not the reopened one plus its successor"
        );
        assert_eq!(open[0].moment.id, id, "and it is the row that was undone");
    }

    fn get(uri: &str, token: Option<&str>) -> Request<Body> {
        let mut b = Request::builder().uri(uri).method("GET");
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        b.body(Body::empty()).unwrap()
    }

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

    #[tokio::test]
    async fn a_cli_search_is_recorded_against_whoever_ran_it() {
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        let (app, token, core) = app_from_core(core).await;
        app.clone()
            .oneshot(get("/api/v1/search?q=journal&door=cli", Some(&token)))
            .await
            .unwrap();
        core.background.wait_idle().await;
        let events = core
            .store
            .events_between(0, crate::store::now() + 1)
            .await
            .unwrap();
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(
            events[0].scope.is_some(),
            "an unscoped shell search: {events:?}"
        );
    }

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
    async fn status_says_nothing_about_learning_while_the_layer_is_off() {
        let (app, token) = app_and_token().await;
        let v = json_of(
            app.oneshot(get("/api/v1/status", Some(&token)))
                .await
                .unwrap(),
        )
        .await;
        assert!(v["learning"].is_null(), "{v}");
        assert!(v["chunks"].is_i64(), "{v}");
        assert!(v["jobs"].is_array(), "{v}");
    }

    #[tokio::test]
    async fn status_counts_what_the_base_has_been_learning() {
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        let (app, token, _core) = app_from_core(core).await;
        let v = json_of(
            app.oneshot(get("/api/v1/status", Some(&token)))
                .await
                .unwrap(),
        )
        .await;
        let l = &v["learning"];
        assert!(l.is_object(), "{v}");
        for k in [
            "pursuits_open",
            "pursuits_unsatisfied",
            "from_pursuits",
            "gaps_open",
        ] {
            assert!(l[k].is_i64(), "{k} is missing from {l}");
        }
    }

    #[tokio::test]
    async fn a_shell_question_is_recorded_and_the_panel_is_unchanged() {
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        let (app, token, core) = app_from_core(core).await;
        let out = core
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        crate::jobs::embed::run_corpus(&core, &out.id)
            .await
            .unwrap();

        let res = app
            .clone()
            .oneshot(post_json(
                "/api/v1/ask/stream?door=cli",
                &token,
                serde_json::json!({"q": "what is alpha"}),
            ))
            .await
            .unwrap();
        crate::web::test_support::body_of(res).await;
        core.background.wait_idle().await;
        let asks = core
            .store
            .asks_between(0, crate::store::now() + 1)
            .await
            .unwrap();
        assert_eq!(asks.len(), 1, "a shell question reached nothing: {asks:?}");
        assert!(asks[0].scope.is_some(), "{asks:?}");

        let res = app
            .oneshot(post_json(
                "/api/v1/ask/stream",
                &token,
                serde_json::json!({"q": "what is bravo"}),
            ))
            .await
            .unwrap();
        crate::web::test_support::body_of(res).await;
        core.background.wait_idle().await;
        assert_eq!(
            core.store
                .asks_between(0, crate::store::now() + 1)
                .await
                .unwrap()
                .len(),
            1,
            "the panel's asks are not recorded and that has not changed"
        );
    }

    #[tokio::test]
    async fn the_streaming_door_names_its_stages_and_then_answers() {
        let (app, token) = app_and_token().await;
        let body = sse_body(&app, "/api/v1/search/stream?q=journal&door=cli", &token).await;
        assert!(body.contains("event: stages"), "{body}");
        assert!(body.contains("\"embed\""), "{body}");
        assert!(body.contains("event: results"), "{body}");
        let (before, after) = body.split_once("event: results").expect("a terminal frame");
        assert!(
            before.contains("event: stage"),
            "no stage was announced: {body}"
        );
        assert!(
            !after.contains("event: stage"),
            "a stage was reported after the results it preceded: {body}"
        );
    }

    #[tokio::test]
    async fn the_plain_search_door_answers_the_list_envelope() {
        let (app, token) = app_and_token().await;
        let res = app
            .oneshot(get("/api/v1/search?q=journal", Some(&token)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v = json_of(res).await;
        assert!(v["items"].is_array() && v["next"].is_null(), "{v}");
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
        // Never `max-age`: one URL answers per tenant and a browser cache cannot be cleared on sign-out.
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

    pub(crate) fn post_json(uri: &str, token: &str, body: serde_json::Value) -> Request<Body> {
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

    pub(crate) fn a_pdf() -> Vec<u8> {
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

    /// Over the global 8 MB, under the PDF ceiling: the handler answers, not the framework's 413.
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
                lang: crate::infer::lang::Lang::default(),
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

    #[tokio::test]
    async fn the_image_route_has_nothing_to_show_for_a_pdf() {
        let (app, token, core) = app_token_and_core().await;
        let id = core
            .ingest_pdf(crate::core::ingest::PdfCapture {
                bytes: a_pdf(),
                filename: Some("plan.pdf".into()),
                title_hint: None,
                note: None,
                lang: crate::infer::lang::Lang::default(),
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
    async fn the_image_door_refuses_junk_and_missing_parts_and_stores_without_vision() {
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
        assert_eq!(res.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn the_image_door_has_its_own_larger_body_limit() {
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
        // Multipart streams, so the 8 MB limit surfaces as a parse error inside the handler.
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

        let doors: Vec<String> = sqlx::query_scalar!("SELECT door FROM search_events")
            .fetch_all(&core.store.pool)
            .await
            .unwrap();
        assert_eq!(doors, vec!["extension".to_string()]);
    }

    #[tokio::test]
    async fn a_search_from_the_panel_does_not_stamp_what_a_prefix_matched() {
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

        let res = app
            .oneshot(get("/api/v1/search?q=loop+device&door=judge", Some(&token)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        core.background.wait_idle().await;

        let doors: Vec<String> = sqlx::query_scalar!("SELECT door FROM search_events")
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
                &[0xc3, 0x28, 0x41],
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert!(core.store.list_corpora(10, 0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_upload_of_utf16_without_a_byte_order_mark_is_refused() {
        let (app, token, core) = app_token_and_core().await;
        let body: Vec<u8> = "Du hast eine Datei an Ronny gesendet"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let res = app
            .oneshot(post_file(
                "/api/v1/corpora/upload",
                &token,
                "chat.txt",
                Some("text/plain"),
                &body,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(
            String::from_utf8_lossy(&body).contains("UTF-16"),
            "the refusal does not name the cause: {body:?}"
        );
        assert!(core.store.list_corpora(10, 0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_upload_of_the_wrong_type_is_refused_with_the_reason() {
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
        // A missing auth check on one route is the failure that matters, so every route is asserted.
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
            ("POST", "/api/v1/decide"),
            ("POST", "/api/v1/decide/stream"),
            ("POST", "/api/v1/decisions/x/verdict"),
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
        assert!(json_of(res).await["items"].is_array());
    }

    #[tokio::test]
    async fn explain_adds_the_pool_beside_the_items_and_changes_nothing_else() {
        let (app, token) = app_and_token().await;
        let plain = json_of(
            app.clone()
                .oneshot(get("/api/v1/search?q=anything", Some(&token)))
                .await
                .unwrap(),
        )
        .await;
        let explained = json_of(
            app.oneshot(get("/api/v1/search?q=anything&explain=1", Some(&token)))
                .await
                .unwrap(),
        )
        .await;
        assert!(plain.get("explanation").is_none(), "got {plain}");
        assert_eq!(plain["items"], explained["items"]);
        assert_eq!(plain["next"], explained["next"]);
        assert!(
            explained["explanation"]["candidates_fetched"].is_number(),
            "the pool's shape is what a caller asks `explain` for: got {explained}"
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

    /// The body as written: `json!` would sort a choice's options, and their order is meaning.
    fn post_text(uri: &str, token: &str, body: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .method("POST")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    async fn deciding_app(
        rule: impl Fn(
            &str,
            &[crate::infer::decide::Question],
        ) -> crate::error::Result<crate::infer::decide::Answers>
        + Send
        + Sync
        + 'static,
        learn: bool,
    ) -> (axum::Router, String) {
        let mut core = crate::core::decide::tests::three_base().await;
        core.learn.enabled = learn;
        crate::core::decide::tests::deciding(&mut core, rule);
        app_with_token(core).await
    }

    /// `(event, data)` in order.
    fn sse_frames(body: &str) -> Vec<(String, serde_json::Value)> {
        body.split("\n\n")
            .filter_map(|f| {
                let event = f.lines().find_map(|l| l.strip_prefix("event:"))?.trim();
                let data = f.lines().find_map(|l| l.strip_prefix("data:"))?.trim();
                Some((event.to_string(), serde_json::from_str(data).ok()?))
            })
            .collect()
    }

    #[tokio::test]
    async fn api_decide_answers_a_bare_question_with_yes_or_no() {
        let (app, token) = deciding_app(crate::core::decide::tests::marked(0.9), true).await;
        let res = app
            .oneshot(post_json(
                "/api/v1/decide",
                &token,
                serde_json::json!({"q": "Keep bge-m3?"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v = json_of(res).await;
        assert_eq!(v["answers"]["verdict"]["type"], "noul", "{v}");
        assert!(v["standing"]["verdict"].is_string(), "{v}");
        assert_eq!(v["read"], 3);
        assert!(
            v.get("event_id").is_none(),
            "the API door is not recorded: {v}"
        );
    }

    #[tokio::test]
    async fn api_decide_takes_system_ones_questions_in_the_callers_order() {
        let (app, token) = deciding_app(crate::core::decide::tests::marked(0.9), false).await;
        let res = app
            .oneshot(post_text(
                "/api/v1/decide",
                &token,
                r#"{"q":"Should I keep the bge-m3 embedder?","questions":{"verdict":{"type":"choice","instructions":"Which embedder do the notes favour?","criteria":{"switch":"Move to EmbeddingGemma","keep":"Stay on bge-m3"}}},"situation":"The GPU has 12 GB.","limit":8,"tags":[],"category":null,"explain":false}"#,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = crate::web::test_support::body_of(res).await;
        assert!(body.contains("\"probabilities\":{\"switch\":"), "{body}");
    }

    #[tokio::test]
    async fn api_decide_says_not_held_with_200() {
        let (app, token) = deciding_app(crate::core::decide::tests::none_bear(), false).await;
        let res = app
            .oneshot(post_json(
                "/api/v1/decide",
                &token,
                serde_json::json!({"q": "Keep bge-m3?"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v = json_of(res).await;
        assert!(v["answers"].is_null(), "{v}");
        assert_eq!(v["evidence"], serde_json::json!([]));
        assert_eq!(v["standing"]["verdict"], "not_held");
        assert!(v["prior"]["verdict"].is_object(), "{v}");
    }

    #[tokio::test]
    async fn api_decide_refuses_a_bad_request_in_one_sentence() {
        let (app, token) = deciding_app(crate::core::decide::tests::marked(0.9), false).await;
        let nine: serde_json::Map<String, serde_json::Value> = (0..9)
            .map(|i| (format!("q{i}"), serde_json::json!({"type": "noul"})))
            .collect();
        for body in [
            serde_json::json!({"q": ""}),
            serde_json::json!({"q": "x", "questions": {"bears": {"type": "noul"}}}),
            serde_json::json!({"q": "x", "questions": nine}),
            serde_json::json!({"q": "x", "questions": {"v": {"type": "box"}}}),
        ] {
            let res = app
                .clone()
                .oneshot(post_json("/api/v1/decide", &token, body.clone()))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{body}");
            let v = json_of(res).await;
            assert!(v["error"].is_string(), "{body} -> {v}");
        }
    }

    #[tokio::test]
    async fn api_decide_explain_adds_the_state() {
        let (app, token) = deciding_app(crate::core::decide::tests::marked(0.9), false).await;
        for (explain, has) in [(true, true), (false, false)] {
            let v = json_of(
                app.clone()
                    .oneshot(post_json(
                        "/api/v1/decide",
                        &token,
                        serde_json::json!({"q": "Keep bge-m3?", "explain": explain}),
                    ))
                    .await
                    .unwrap(),
            )
            .await;
            assert_eq!(v.get("state").is_some_and(|s| s.is_string()), has, "{v}");
        }
    }

    #[tokio::test]
    async fn api_decide_stream_sends_retrieved_reads_and_done() {
        let (app, token) = deciding_app(crate::core::decide::tests::marked(0.9), false).await;
        let blocking = json_of(
            app.clone()
                .oneshot(post_json(
                    "/api/v1/decide",
                    &token,
                    serde_json::json!({"q": "Keep bge-m3?"}),
                ))
                .await
                .unwrap(),
        )
        .await;
        let res = app
            .oneshot(post_json(
                "/api/v1/decide/stream",
                &token,
                serde_json::json!({"q": "Keep bge-m3?"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(
            res.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/event-stream")
        );
        let body = crate::web::test_support::body_of(res).await;
        let frames = sse_frames(&body);
        let names: Vec<&str> = frames.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            ["retrieved", "read", "read", "read", "done"],
            "{body}"
        );
        assert_eq!(frames[0].1, serde_json::json!({"passages": 3}));
        for (_, r) in &frames[1..4] {
            assert!(r["bearing"].is_boolean(), "{r}");
            assert_eq!(r["of"], 3);
            assert!(
                r["artifact_id"].is_string() && r["bears"].is_number(),
                "{r}"
            );
            assert_eq!(r["read"]["verdict"]["type"], "noul", "{r}");
        }
        let done = &frames[4].1;
        let keys =
            |v: &serde_json::Value| v.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
        assert_eq!(keys(done), keys(&blocking));
        assert_eq!(done["answers"], blocking["answers"]);
    }

    #[tokio::test]
    async fn api_decide_stream_ends_on_an_error_frame() {
        let (app, token) = deciding_app(
            |_, _| Err(crate::error::Error::Internal("the rule erred".into())),
            false,
        )
        .await;
        let res = app
            .oneshot(post_json(
                "/api/v1/decide/stream",
                &token,
                serde_json::json!({"q": "Keep bge-m3?"}),
            ))
            .await
            .unwrap();
        let body = crate::web::test_support::body_of(res).await;
        let frames = sse_frames(&body);
        let (name, data) = frames.last().unwrap_or_else(|| panic!("no frames: {body}"));
        assert_eq!(name, "error", "{body}");
        assert!(data["error"].is_string());
    }

    #[tokio::test]
    async fn a_decision_from_the_app_is_recorded_and_judged() {
        let (app, token) = deciding_app(crate::core::decide::tests::marked(0.9), true).await;
        let decide = |body: &'static str| {
            let app = app.clone();
            let token = token.clone();
            async move {
                json_of(
                    app.oneshot(post_text("/api/v1/decide?door=app", &token, body))
                        .await
                        .unwrap(),
                )
                .await
            }
        };
        let judge = |id: String, body: serde_json::Value| {
            let app = app.clone();
            let token = token.clone();
            async move {
                let res = app
                    .oneshot(post_json(
                        &format!("/api/v1/decisions/{id}/verdict"),
                        &token,
                        body,
                    ))
                    .await
                    .unwrap();
                (res.status(), json_of(res).await)
            }
        };

        let d = decide(
            r#"{"q":"Keep bge-m3?","questions":{"verdict":{"type":"choice","criteria":{"keep":"Stay","switch":"Move"}}}}"#,
        )
        .await;
        let id = d["event_id"]
            .as_str()
            .expect("the app door is recorded")
            .to_string();
        let (status, v) = judge(id.clone(), serde_json::json!({"verdict": "right"})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            v,
            serde_json::json!({"verdict": "right", "correct": {"verdict": "keep"}})
        );
        let (_, v) = judge(id.clone(), serde_json::json!({"verdict": "none"})).await;
        assert_eq!(v, serde_json::json!({"verdict": null, "correct": null}));
        let (status, v) = judge(id.clone(), serde_json::json!({"verdict": "maybe"})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(v["error"].is_string());

        let d = decide(
            r#"{"q":"Which?","questions":{"verdict":{"type":"choice","criteria":{"a":"A","b":"B","c":"C"}}}}"#,
        )
        .await;
        let id = d["event_id"].as_str().unwrap().to_string();
        let (status, v) = judge(
            id.clone(),
            serde_json::json!({"verdict": "wrong", "correct": {"verdict": "c"}}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            v,
            serde_json::json!({"verdict": "wrong", "correct": {"verdict": "c"}})
        );
        let (status, _) = judge(
            id.clone(),
            serde_json::json!({"verdict": "wrong", "correct": {"verdict": "zzz"}}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, _) = judge("nope".into(), serde_json::json!({"verdict": "right"})).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// A frame name the panel's switch does not handle vanishes silently.
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

    /// One test over all three doors: `with_lang` is a builder, so a branch that forgets it still compiles.
    #[tokio::test]
    async fn every_door_that_takes_text_stamps_the_language() {
        let (app, token, core) = app_token_and_core().await;
        core.store
            .control
            .set_lang("user-1", Some(crate::infer::lang::Lang::De))
            .await
            .unwrap();
        let id_of = |v: serde_json::Value| v["id"].as_str().unwrap().to_string();

        let ingested = id_of(
            json_of(
                app.clone()
                    .oneshot(post_json(
                        "/api/v1/corpora",
                        &token,
                        serde_json::json!({"text": "eine notiz durch die json-tür"}),
                    ))
                    .await
                    .unwrap(),
            )
            .await,
        );
        let raw = id_of(
            json_of(
                app.clone()
                    .oneshot(raw_post(
                        "/api/v1/capture",
                        &token,
                        "text/plain",
                        "eine notiz durch die rohe tür".as_bytes(),
                    ))
                    .await
                    .unwrap(),
            )
            .await,
        );
        let shared = id_of(
            json_of(
                app.clone()
                    .oneshot(multipart(
                        "/api/v1/capture",
                        &token,
                        &[("text", "eine notiz durch die teilen-tür")],
                        &[],
                    ))
                    .await
                    .unwrap(),
            )
            .await,
        );

        for id in [&ingested, &raw, &shared] {
            let stored = core.store.get_corpus(id).await.unwrap();
            assert_eq!(
                crate::infer::lang::of_corpus(&stored.metadata),
                crate::infer::lang::Lang::De,
                "{} was stored to be read in English",
                stored.raw_text
            );
        }
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

    #[tokio::test]
    async fn intent_is_refused_where_it_would_be_dropped() {
        let (app, token) = app_and_token().await;
        let text = |uri: &str, body: &str| {
            Request::builder()
                .uri(uri)
                .method("POST")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "text/plain")
                .body(Body::from(body.to_string()))
                .unwrap()
        };
        let res = app
            .clone()
            .oneshot(text(
                "/api/v1/capture?intent=remind",
                "https://example.com/a",
            ))
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::BAD_REQUEST,
            "intent was accepted and dropped"
        );
        let res = app
            .clone()
            .oneshot(raw_post(
                "/api/v1/capture?intent=remind",
                &token,
                "image/png",
                &a_png(),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let res = app
            .oneshot(multipart(
                "/api/v1/capture",
                &token,
                &[
                    ("text", "Erinnere mich morgen an die Bank."),
                    ("tz", "Europe/Berlin"),
                    ("intent", "remind"),
                ],
                &[],
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn a_zone_beside_a_link_or_a_file_never_costs_the_capture() {
        let (app, token) = app_and_token().await;
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/capture?tz=Europe/Berlin")
                    .method("POST")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "text/plain")
                    .body(Body::from("https://example.com/a"))
                    .unwrap(),
            )
            .await
            .unwrap();
        let said = String::from_utf8(
            axum::body::to_bytes(res.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(
            !said.contains("only applies"),
            "the door refused a link over its zone: {said}"
        );
        let res = app
            .clone()
            .oneshot(raw_post(
                "/api/v1/capture?tz=Europe/Berlin",
                &token,
                "image/png",
                &a_png(),
            ))
            .await
            .unwrap();
        assert!(res.status().is_success(), "{}", res.status());
        let res = app
            .clone()
            .oneshot(multipart(
                "/api/v1/capture",
                &token,
                &[("tz", "Europe/Berlin")],
                &[FilePart {
                    field: "file",
                    filename: "a.txt",
                    mime: Some("text/plain"),
                    body: b"a procedure worth keeping",
                }],
            ))
            .await
            .unwrap();
        assert!(res.status().is_success(), "{}", res.status());
        let res = app
            .oneshot(multipart(
                "/api/v1/capture?tz=Europe/Berlin",
                &token,
                &[("text", "https://example.com/b")],
                &[],
            ))
            .await
            .unwrap();
        let said = String::from_utf8(
            axum::body::to_bytes(res.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(
            !said.contains("only applies"),
            "the door refused a shared link over its zone: {said}"
        );
    }
}

#[cfg(test)]
mod patch_tests {
    use super::tests::*;
    use super::tests::{app_token_and_core, post_json};
    use crate::store::artifacts::{EmbedState, NewArtifact};
    use crate::vector::SearchFilter;
    use crate::web::test_support::json_of;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    async fn one_artifact() -> (axum::Router, String, crate::core::Core, String) {
        let (app, token, core) = app_token_and_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    text: "the body".into(),
                    title: Some("a title".into()),
                    category: Some("concept".into()),
                    tags: vec!["old".into()],
                    ..Default::default()
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
                    text: "the older copy".into(),
                    ..Default::default()
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

    #[tokio::test]
    async fn the_corpus_list_is_an_envelope_of_rows_without_their_text() {
        let (app, token, core) = app_token_and_core().await;
        core.store
            .insert_corpus("the whole of a very long book", "web", Some("A book"))
            .await
            .unwrap();
        core.store
            .insert_corpus("no title, only an opening", "web", None)
            .await
            .unwrap();

        let body = json_of(
            app.oneshot(bearer_get("/api/v1/corpora", &token))
                .await
                .unwrap(),
        )
        .await;
        let items = body["items"].as_array().expect("items is a list");
        assert_eq!(items.len(), 2);
        assert!(body["next"].is_null(), "two rows are one page: {body}");
        for row in items {
            assert!(row.get("raw_text").is_none(), "a list row carried its text");
        }
        let named = items.iter().find(|r| r["label"] == "A book").unwrap();
        assert_eq!(named["named"], true);
        let unnamed = items.iter().find(|r| r["named"] == false).unwrap();
        assert_eq!(unnamed["label"], "no title, only an opening");
    }

    #[tokio::test]
    async fn a_capture_between_two_pages_neither_repeats_nor_skips_a_row() {
        let (app, token, core) = app_token_and_core().await;
        let mut ids = vec![];
        for n in 0..3 {
            ids.push(
                core.store
                    .insert_corpus(&format!("text {n}"), "web", None)
                    .await
                    .unwrap()
                    .id,
            );
        }
        let first = json_of(
            app.clone()
                .oneshot(bearer_get("/api/v1/corpora?limit=2", &token))
                .await
                .unwrap(),
        )
        .await;
        let next = first["next"].as_str().expect("a second page").to_string();

        core.store
            .insert_corpus("landed between", "web", None)
            .await
            .unwrap();

        let second = json_of(
            app.oneshot(bearer_get(
                &format!("/api/v1/corpora?limit=2&after={next}"),
                &token,
            ))
            .await
            .unwrap(),
        )
        .await;
        assert!(second["next"].is_null());
        let mut seen: Vec<String> = first["items"]
            .as_array()
            .unwrap()
            .iter()
            .chain(second["items"].as_array().unwrap())
            .map(|r| r["id"].as_str().unwrap().to_string())
            .collect();
        seen.sort();
        ids.sort();
        assert_eq!(seen, ids, "every original row once, and only those");
    }

    #[tokio::test]
    async fn a_cursor_this_server_did_not_issue_is_a_400() {
        let (app, token, _core) = app_token_and_core().await;
        let res = app
            .oneshot(bearer_get("/api/v1/corpora?after=not-a-cursor", &token))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    /// The failure is silent: a list without these fields still looks like a list.
    #[test]
    fn the_envelope_carries_a_rows_weakness_and_the_cliff() {
        let rows = vec![
            crate::cli::search::fixture::hit("sure", 0.9, false, false),
            crate::cli::search::fixture::hit("loose", 0.2, true, true),
        ];
        let body = super::search_body(&rows, None).unwrap();
        assert!(
            body["items"][0].get("past_cliff").is_none(),
            "absent means false"
        );
        assert_eq!(body["items"][1]["weak"], true);
        assert_eq!(body["items"][1]["past_cliff"], true);
        assert!(body["next"].is_null());
    }

    #[tokio::test]
    async fn a_file_revalidates_by_its_content_hash() {
        let (app, token, core) = app_token_and_core().await;
        let id = core
            .ingest_pdf(crate::core::ingest::PdfCapture {
                bytes: super::tests::a_pdf(),
                filename: Some("plan.pdf".into()),
                title_hint: None,
                note: None,
                lang: crate::infer::lang::Lang::default(),
            })
            .await
            .unwrap()
            .id;
        let uri = format!("/api/v1/corpora/{id}/file");
        let first = app.clone().oneshot(bearer_get(&uri, &token)).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let tag = first.headers()["etag"].to_str().unwrap().to_string();

        let again = app
            .clone()
            .oneshot(bearer_get_if(&uri, &token, &tag))
            .await
            .unwrap();
        assert_eq!(again.status(), StatusCode::NOT_MODIFIED);
        assert!(crate::web::test_support::body_of(again).await.is_empty());

        let image = app
            .oneshot(bearer_get_if(
                &format!("/api/v1/corpora/{id}/image?original=1"),
                &token,
                &tag,
            ))
            .await
            .unwrap();
        assert_ne!(image.status(), StatusCode::NOT_MODIFIED);
    }

    fn bearer_get_if(uri: &str, token: &str, tag: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .header("authorization", format!("Bearer {token}"))
            .header("if-none-match", tag)
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn a_read_answers_an_etag_and_a_304_to_its_own_tag() {
        let (app, token, _core) = app_token_and_core().await;
        let res = app
            .clone()
            .oneshot(bearer_get("/api/v1/corpora", &token))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()["cache-control"], "private, no-cache");
        let tag = res.headers()["etag"].to_str().unwrap().to_string();

        let again = app
            .oneshot(bearer_get_if("/api/v1/corpora", &token, &tag))
            .await
            .unwrap();
        assert_eq!(again.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(again.headers()["etag"].to_str().unwrap(), tag);
        assert!(crate::web::test_support::body_of(again).await.is_empty());
    }

    #[tokio::test]
    async fn a_capture_changes_the_tag_of_the_list_it_lands_in() {
        let (app, token, core) = app_token_and_core().await;
        let before = app
            .clone()
            .oneshot(bearer_get("/api/v1/corpora", &token))
            .await
            .unwrap();
        let tag = before.headers()["etag"].to_str().unwrap().to_string();
        core.store.insert_corpus("x", "web", None).await.unwrap();

        let after = app
            .oneshot(bearer_get_if("/api/v1/corpora", &token, &tag))
            .await
            .unwrap();
        assert_eq!(after.status(), StatusCode::OK, "a stale tag was honoured");
        assert_ne!(after.headers()["etag"].to_str().unwrap(), tag);
    }

    #[tokio::test]
    async fn an_error_and_a_post_carry_no_etag() {
        let (app, token, _core) = app_token_and_core().await;
        let missing = app
            .clone()
            .oneshot(bearer_get("/api/v1/corpora/nope", &token))
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        assert!(missing.headers().get("etag").is_none());

        let post = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/moments/nope/done")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(post.headers().get("etag").is_none());
    }

    fn bearer_get(uri: &str, token: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn moments_are_listed_set_completed_and_snoozed_over_the_api() {
        let (app, token, core) = app_token_and_core().await;
        let out = core
            .ingest_capture(crate::core::ingest::Capture::new("Pay rent", "api"))
            .await
            .unwrap();
        crate::jobs::test_support::drain(&core).await;
        let aid = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .into_iter()
            .find(|c| c.in_results())
            .expect("a live artifact")
            .id;
        let at = crate::store::now() + 600;
        let res = app
            .clone()
            .oneshot(post_json(
                &format!("/api/v1/artifacts/{aid}/moments"),
                &token,
                serde_json::json!({"at": at, "tz": "Europe/Berlin", "kind": "due", "rule": "FREQ=MONTHLY;BYMONTHDAY=1"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let id = json_of(res).await["id"].as_str().unwrap().to_string();
        let list = json_of(
            app.clone()
                .oneshot(bearer_get("/api/v1/moments?kind=due", &token))
                .await
                .unwrap(),
        )
        .await;
        let list = list["items"].clone();
        assert_eq!(list[0]["moment"]["id"], id);
        assert_eq!(list[0]["opening"], "Pay rent");
        assert!(!list[0]["title"].as_str().unwrap_or_default().is_empty());
        let res = app
            .clone()
            .oneshot(post_json(
                &format!("/api/v1/moments/{id}/done"),
                &token,
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        let list = json_of(
            app.clone()
                .oneshot(bearer_get(
                    &format!("/api/v1/moments?kind=due&to={}", at + 40 * 86_400),
                    &token,
                ))
                .await
                .unwrap(),
        )
        .await;
        let list = list["items"].clone();
        assert_eq!(list.as_array().unwrap().len(), 1, "the next occurrence");
        assert_ne!(list[0]["moment"]["id"], id);
        let next = list[0]["moment"]["id"].as_str().unwrap().to_string();
        let res = app
            .clone()
            .oneshot(post_json(
                &format!("/api/v1/moments/{next}/snooze"),
                &token,
                serde_json::json!({"until": at + 60 * 86_400}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        assert!(
            core.store
                .moment(&next)
                .await
                .unwrap()
                .unwrap()
                .snoozed_until
                .is_some()
        );
    }

    #[tokio::test]
    async fn a_snooze_into_the_past_is_refused_and_an_unknown_row_is_a_404() {
        let (app, token, core) = app_token_and_core().await;
        let out = core
            .ingest_capture(crate::core::ingest::Capture::new("Pay rent", "api"))
            .await
            .unwrap();
        crate::jobs::test_support::drain(&core).await;
        let aid = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .into_iter()
            .find(|c| c.in_results())
            .expect("a live artifact")
            .id;
        let at = crate::store::now() + 600;
        let res = app
            .clone()
            .oneshot(post_json(
                &format!("/api/v1/artifacts/{aid}/moments"),
                &token,
                serde_json::json!({"at": at, "tz": "Europe/Berlin", "kind": "due"}),
            ))
            .await
            .unwrap();
        let id = json_of(res).await["id"].as_str().unwrap().to_string();

        for until in [0, -1, crate::store::now(), i64::MAX] {
            let res = app
                .clone()
                .oneshot(post_json(
                    &format!("/api/v1/moments/{id}/snooze"),
                    &token,
                    serde_json::json!({ "until": until }),
                ))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::BAD_REQUEST, "until={until}");
        }
        assert!(
            core.store
                .moment(&id)
                .await
                .unwrap()
                .unwrap()
                .snoozed_until
                .is_none(),
            "nothing was written"
        );

        let res = app
            .clone()
            .oneshot(post_json(
                "/api/v1/moments/no-such-row/snooze",
                &token,
                serde_json::json!({"until": at + 86_400}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_moments_window_in_the_future_does_not_unhide_a_snoozed_row() {
        let (app, token, core) = app_token_and_core().await;
        let out = core
            .ingest_capture(crate::core::ingest::Capture::new("Pay rent", "api"))
            .await
            .unwrap();
        crate::jobs::test_support::drain(&core).await;
        let aid = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .into_iter()
            .find(|c| c.in_results())
            .expect("a live artifact")
            .id;
        let now = crate::store::now();
        let res = app
            .clone()
            .oneshot(post_json(
                &format!("/api/v1/artifacts/{aid}/moments"),
                &token,
                serde_json::json!({"at": now + 600, "tz": "Europe/Berlin", "kind": "due"}),
            ))
            .await
            .unwrap();
        let id = json_of(res).await["id"].as_str().unwrap().to_string();
        let res = app
            .clone()
            .oneshot(post_json(
                &format!("/api/v1/moments/{id}/snooze"),
                &token,
                serde_json::json!({"until": now + 30 * 86_400}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);

        let list = json_of(
            app.clone()
                .oneshot(bearer_get(
                    &format!(
                        "/api/v1/moments?kind=due&from={}&to={}",
                        now + 40 * 86_400,
                        now + 47 * 86_400
                    ),
                    &token,
                ))
                .await
                .unwrap(),
        )
        .await;
        let list = list["items"].clone();
        assert!(
            list.as_array().unwrap().is_empty(),
            "the row is put aside until it is not: {list}"
        );
    }

    #[tokio::test]
    async fn a_rule_outside_the_subset_is_a_400_with_the_reason() {
        let (app, token, core) = app_token_and_core().await;
        let out = core
            .ingest_capture(crate::core::ingest::Capture::new("Pay rent", "api"))
            .await
            .unwrap();
        crate::jobs::test_support::drain(&core).await;
        let aid = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .into_iter()
            .find(|c| c.in_results())
            .expect("a live artifact")
            .id;
        let res = app
            .oneshot(post_json(
                &format!("/api/v1/artifacts/{aid}/moments"),
                &token,
                serde_json::json!({"at": 1, "rule": "FREQ=HOURLY"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let body = json_of(res).await.to_string();
        assert!(body.contains("HOURLY"), "{body}");
    }

    #[tokio::test]
    async fn capture_accepts_tz_and_intent() {
        let (app, token, core) = app_token_and_core().await;
        let text = |uri: &str, body: &str| {
            Request::builder()
                .uri(uri)
                .method("POST")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "text/plain")
                .body(Body::from(body.to_string()))
                .unwrap()
        };
        let res = app
            .clone()
            .oneshot(text("/api/v1/capture?tz=Europe/Berlin", "Long day."))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let id = json_of(res).await["id"].as_str().unwrap().to_string();
        let c = core.store.get_corpus(&id).await.unwrap();
        assert_eq!(c.origin, "web");
        assert_eq!(c.metadata["tz"], "Europe/Berlin");

        let res = app
            .clone()
            .oneshot(text(
                "/api/v1/capture?intent=remind",
                "call the bank tomorrow",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let id = json_of(res).await["id"].as_str().unwrap().to_string();
        assert_eq!(
            core.store.get_corpus(&id).await.unwrap().metadata["intent"],
            "remind"
        );
    }

    #[tokio::test]
    async fn a_zone_the_table_does_not_know_is_refused_and_not_quietly_read_in_utc() {
        let (app, token, core) = app_token_and_core().await;
        let text = |uri: &str, body: &str| {
            Request::builder()
                .uri(uri)
                .method("POST")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "text/plain")
                .body(Body::from(body.to_string()))
                .unwrap()
        };
        let res = app
            .clone()
            .oneshot(text(
                "/api/v1/capture?tz=Europe/Berlim",
                "call the bank tomorrow",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert!(
            json_of(res).await["error"]
                .as_str()
                .unwrap_or_default()
                .contains("Europe/Berlim")
        );

        let res = app
            .oneshot(text(
                "/api/v1/capture?tz=Europe%2FBerlin",
                "call the bank tomorrow",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let id = json_of(res).await["id"].as_str().unwrap().to_string();
        assert_eq!(
            core.store.get_corpus(&id).await.unwrap().metadata["tz"],
            "Europe/Berlin"
        );
    }

    #[tokio::test]
    async fn setting_a_moment_refuses_an_instant_no_reader_can_do_arithmetic_in() {
        let (app, token, core) = app_token_and_core().await;
        let out = core
            .ingest_capture(crate::core::ingest::Capture::new("Send the invoice", "ui"))
            .await
            .unwrap();
        crate::jobs::test_support::drain(&core).await;
        let aid = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .into_iter()
            .find(|c| c.in_results())
            .expect("a live artifact")
            .id;

        for at in [i64::MIN, i64::MAX, -62_135_596_801i64, 253_402_300_800i64] {
            let res = app
                .clone()
                .oneshot(post_json(
                    &format!("/api/v1/artifacts/{aid}/moments"),
                    &token,
                    serde_json::json!({ "at": at }),
                ))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::BAD_REQUEST, "at={at}");
        }
        assert!(
            core.store.open_due(0, i64::MAX).await.unwrap().is_empty(),
            "and nothing was written"
        );
        let res = app
            .oneshot(post_json(
                &format!("/api/v1/artifacts/{aid}/moments"),
                &token,
                serde_json::json!({ "at": -2_208_988_800i64, "kind": "event" }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn setting_a_moment_refuses_a_zone_the_table_does_not_know() {
        let (app, token, core) = app_token_and_core().await;
        let out = core
            .ingest_capture(crate::core::ingest::Capture::new("Send the invoice", "ui"))
            .await
            .unwrap();
        crate::jobs::test_support::drain(&core).await;
        let aid = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .into_iter()
            .find(|c| c.in_results())
            .expect("a live artifact")
            .id;

        let res = app
            .clone()
            .oneshot(post_json(
                &format!("/api/v1/artifacts/{aid}/moments"),
                &token,
                serde_json::json!({ "at": 1_800_000_000i64, "tz": "Europe/Berlim" }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert!(
            core.store.open_due(0, i64::MAX).await.unwrap().is_empty(),
            "and nothing was written"
        );

        let res = app
            .oneshot(post_json(
                &format!("/api/v1/artifacts/{aid}/moments"),
                &token,
                serde_json::json!({ "at": 1_800_000_000i64, "tz": "Europe/Berlin" }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        assert_eq!(
            core.store.open_due(0, i64::MAX).await.unwrap()[0].moment.tz,
            "Europe/Berlin"
        );
    }
}

/// The doors the phone uses that the browser reaches otherwise.
#[cfg(test)]
mod phone_doors {
    use super::tests::{app_from_core, app_token_and_core};
    use crate::web::test_support::{FilePart, body_of, json_of, multipart};
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    async fn app_and_token() -> (axum::Router, String) {
        let (app, token, _core) = app_token_and_core().await;
        (app, token)
    }

    fn get(uri: &str, token: Option<&str>) -> Request<Body> {
        let mut b = Request::builder().uri(uri).method("GET");
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        b.body(Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn related_lists_the_neighbours_and_the_links_the_pane_lists() {
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        let out = core
            .ingest("alpha line\n\nbravo line\n\ncharlie line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        crate::jobs::embed::run_corpus(&core, &out.id)
            .await
            .unwrap();
        let arts = core.store.artifacts_for_corpus(&out.id).await.unwrap();
        assert!(arts.len() > 1);
        core.store
            .bump_link(
                &arts[0].id,
                &arts[1].id,
                5.0,
                Some("mount forensic image"),
                30.0,
                crate::store::now(),
            )
            .await
            .unwrap();
        let (app, token, _core) = app_from_core(core).await;

        let v = json_of(
            app.oneshot(get(
                &format!("/api/v1/artifacts/{}/related", arts[0].id),
                Some(&token),
            ))
            .await
            .unwrap(),
        )
        .await;
        let related = v["related"].as_array().unwrap();
        assert!(!related.is_empty(), "{v}");
        assert!(
            related.iter().all(|r| r["id"] != arts[0].id),
            "an artifact is not its own neighbour: {v}"
        );
        assert!(
            related
                .iter()
                .all(|r| r["label"].is_string() && r["named"].is_boolean()),
            "every row is a label that says whether it is a name: {v}"
        );
        let seen = v["seen_together"].as_array().unwrap();
        assert_eq!(seen.len(), 1, "{v}");
        assert_eq!(seen[0]["id"], arts[1].id);
        assert_eq!(seen[0]["why"], "when asking: mount forensic image");
        assert!(seen[0]["corpus_title"].is_string(), "{v}");
        assert_eq!(
            v["continues_at"], arts[1].id,
            "\"alpha line\" stops mid-sentence, so the way onward is the next passage: {v}"
        );
    }

    #[tokio::test]
    async fn source_answers_the_lines_an_artifact_was_drawn_from_and_the_context_around_them() {
        let (app, token, core) = app_token_and_core().await;
        let doc = core
            .store
            .insert_corpus(
                "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight",
                "web",
                Some("Lines"),
            )
            .await
            .unwrap();
        let arts = core
            .store
            .insert_artifacts(
                &doc.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: "four five".into(),
                    corpus_span: Some(crate::store::artifacts::CorpusSpan {
                        start_line: 4,
                        end_line: 5,
                        source: crate::store::artifacts::SpanSource::Located,
                    }),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();

        let v = json_of(
            app.oneshot(get(
                &format!("/api/v1/artifacts/{}/source", arts[0].id),
                Some(&token),
            ))
            .await
            .unwrap(),
        )
        .await;
        assert_eq!(v["corpus_id"], doc.id);
        assert_eq!(v["label"], "lines 4–5");
        let lines = v["lines"].as_array().unwrap();
        assert_eq!(lines.first().unwrap()["number"], 1);
        assert_eq!(lines.last().unwrap()["number"], 8);
        let claimed: Vec<i64> = lines
            .iter()
            .filter(|l| l["in_span"] == true)
            .map(|l| l["number"].as_i64().unwrap())
            .collect();
        assert_eq!(claimed, vec![4, 5], "{v}");
        assert_eq!(lines[3]["text"], "four");
    }

    #[tokio::test]
    async fn a_merge_has_no_source_lines_and_says_so_without_a_404() {
        let (app, token, core) = app_token_and_core().await;
        let merged = core
            .store
            .insert_merged_artifact(
                &crate::store::artifacts::NewMerged {
                    text: "Both, merged.".into(),
                    title: Some("Merged".into()),
                    ..Default::default()
                },
                &[],
            )
            .await
            .unwrap();
        let res = app
            .oneshot(get(
                &format!("/api/v1/artifacts/{}/source", merged.id),
                Some(&token),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v = json_of(res).await;
        assert!(v["corpus_id"].is_null(), "{v}");
        assert!(v["lines"].as_array().unwrap().is_empty(), "{v}");
    }

    #[tokio::test]
    async fn status_says_whether_the_transcribe_door_is_open() {
        let (app, token) = app_and_token().await;
        let v = json_of(
            app.oneshot(get("/api/v1/status", Some(&token)))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(
            v["transcribe"], false,
            "shipped default has no speech model: {v}"
        );

        let core = crate::core::test_support::test_core_with_transcriber(std::sync::Arc::new(
            crate::infer::fake::FakeTranscriber::default(),
        ))
        .await;
        let (app, token, _core) = app_from_core(core).await;
        let v = json_of(
            app.oneshot(get("/api/v1/status", Some(&token)))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(v["transcribe"], true, "{v}");
    }

    #[tokio::test]
    async fn transcribe_hands_back_the_words_over_a_bearer_token() {
        let heard = std::sync::Arc::new(crate::infer::fake::FakeTranscriber::saying(
            "tombstones in leveldb",
        ));
        let core = crate::core::test_support::test_core_with_transcriber(heard.clone()).await;
        let (app, token, core) = app_from_core(core).await;
        let before = core.store.list_corpora(10, 0).await.unwrap().len();

        let res = app
            .clone()
            .oneshot(multipart(
                "/api/v1/transcribe",
                &token,
                &[],
                &[FilePart {
                    field: "audio",
                    filename: "recording.wav",
                    mime: Some("audio/wav"),
                    body: b"RIFF fake",
                }],
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers()[header::CONTENT_TYPE],
            "text/plain; charset=utf-8"
        );
        assert_eq!(body_of(res).await, "tombstones in leveldb");
        assert_eq!(heard.calls(), 1);
        assert_eq!(heard.last_mime(), "audio/wav");
        assert_eq!(
            core.store.list_corpora(10, 0).await.unwrap().len(),
            before,
            "dictation is not a capture"
        );

        let res = app
            .clone()
            .oneshot(multipart(
                "/api/v1/transcribe",
                &token,
                &[],
                &[FilePart {
                    field: "audio",
                    filename: "recording.wav",
                    mime: Some("audio/wav"),
                    body: b"",
                }],
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(body_of(res).await, "");
        assert_eq!(heard.calls(), 1);
    }

    #[tokio::test]
    async fn transcribe_is_a_404_where_no_speech_model_is_configured() {
        let (app, token) = app_and_token().await;
        let res = app
            .oneshot(multipart(
                "/api/v1/transcribe",
                &token,
                &[],
                &[FilePart {
                    field: "audio",
                    filename: "recording.wav",
                    mime: Some("audio/wav"),
                    body: b"RIFF fake",
                }],
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }
}
