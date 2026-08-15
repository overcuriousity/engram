use crate::auth::Identity;
use crate::core::search::SearchQuery;
use crate::error::{Error, Result};
use crate::store::jobs::{FailedJob, Stage};
use crate::web::state::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
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
const ORIGIN_WEB: &str = "web";
const ORIGIN_EXTENSION: &str = "extension";
const ORIGIN_FETCH: &str = "fetch";

/// Readability and the markdown conversion, off the async worker.
///
/// Both are synchronous walks of a DOM that can be megabytes — `fetch_max_bytes`
/// and the request body limit are both 8 MB — and run inline they hold a Tokio
/// worker for long enough to stall whatever else that thread was serving.
/// `Readability` is `!Send`, which is why this could not be awaited across;
/// inside a `spawn_blocking` closure it is created and dropped without ever
/// crossing an await, and only the owned `String` has to move.
async fn extract(html: String, url: Option<url::Url>, min_chars: usize) -> Result<String> {
    tokio::task::spawn_blocking(move || {
        crate::core::extract::html_to_markdown(&html, url.as_ref(), min_chars)
    })
    .await
    // A `JoinError` is a panic in `dom_smoothie` or `html2md` — two parsers
    // fed whatever a remote page contained — or a cancelled runtime. Neither
    // is anything the caller did, so it must not come back as a 400 telling
    // them their page was malformed while the crash goes unrecorded.
    .map_err(|e| Error::Internal(format!("extraction did not finish: {e}")))?
}

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
        st.core.capture.min_extracted_chars
    };

    let (text, origin) = if let Some(text) = req.text {
        (text, ORIGIN_WEB)
    } else if let Some(html) = req.html {
        (
            extract(html, parsed_url.clone(), floor).await?,
            ORIGIN_EXTENSION,
        )
    } else {
        let u = parsed_url.as_ref().expect("one-of check guarantees a url");
        let html = crate::core::fetch::fetch_html(u, &st.core.capture).await?;
        (
            extract(html, parsed_url.clone(), floor).await?,
            ORIGIN_FETCH,
        )
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

const ORIGIN_UPLOAD: &str = "upload";

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

/// `.txt` and nothing else, for now. PDF is a `SourceView` implementation and
/// a later plan; refusing everything else by name is what keeps this one from
/// quietly ingesting the bytes of a format it cannot read.
async fn upload(
    State(st): State<AppState>,
    _id: Identity,
    mut multipart: axum::extract::Multipart,
) -> Result<(StatusCode, Json<crate::core::ingest::IngestOutcome>)> {
    let mut note: Option<String> = None;
    // (filename, declared type, bytes). Collected rather than acted on in the
    // loop, because a `note` part may come before or after the file.
    let mut file: Option<(Option<String>, String, axum::body::Bytes)> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| Error::Validation(format!("malformed upload: {e}")))?
    {
        match field.name() {
            Some("note") => {
                note = Some(
                    field
                        .text()
                        .await
                        .map_err(|e| Error::Validation(format!("malformed upload: {e}")))?,
                )
            }
            Some("file") => {
                let filename = field.file_name().map(str::to_string);
                let declared = field.content_type().unwrap_or("").to_string();
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|e| Error::Validation(format!("upload failed: {e}")))?;
                file = Some((filename, declared, bytes));
            }
            _ => {}
        }
    }
    let Some((filename, declared, bytes)) = file else {
        return Err(Error::Validation("no file in the upload".into()));
    };
    // A part may legally carry no `Content-Type` at all, and letting that
    // skip the check turns "`.txt` and nothing else" into "anything whose
    // bytes happen to be UTF-8" — a `.csv`, a `.json`, a page of HTML.
    // An absent type is not a pass; it just moves the question to the
    // name, which is the only other thing the sender told us.
    if declared.is_empty() {
        if !named_txt(filename.as_deref()) {
            return Err(Error::Validation(
                "that upload declares no type and is not named `.txt` — \
                 only text/plain is accepted"
                    .into(),
            ));
        }
    } else if !declared.starts_with("text/plain") {
        return Err(Error::Validation(format!(
            "that file is `{declared}` — only text/plain is accepted"
        )));
    }
    // Refused rather than lossily converted: a corpus is quoted back
    // verbatim, so text that arrived mangled would be a fidelity loss
    // nothing downstream could detect.
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
    let code = if out.duplicate {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((code, Json(out)))
}

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
        match field.name() {
            Some("note") => {
                note = Some(
                    field
                        .text()
                        .await
                        .map_err(|e| Error::Validation(format!("malformed upload: {e}")))?,
                )
            }
            Some("title_hint") => {
                title_hint = Some(
                    field
                        .text()
                        .await
                        .map_err(|e| Error::Validation(format!("malformed upload: {e}")))?,
                )
                .filter(|t: &String| !t.trim().is_empty())
            }
            Some("image") => {
                let filename = field.file_name().map(str::to_string);
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|e| Error::Validation(format!("upload failed: {e}")))?;
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
    let code = if out.duplicate {
        StatusCode::OK
    } else {
        StatusCode::ACCEPTED
    };
    Ok((code, Json(out)))
}

#[derive(serde::Deserialize, Default)]
struct ImageQuery {
    #[serde(default)]
    original: Option<String>,
}

/// The preview by default; `?original=1` for the bytes as uploaded.
async fn get_image(
    State(st): State<AppState>,
    _id: Identity,
    Path(id): Path<String>,
    Query(q): Query<ImageQuery>,
) -> Result<axum::response::Response> {
    use axum::response::IntoResponse;
    let want_original = q
        .original
        .as_deref()
        .is_some_and(|v| v == "1" || v == "true");
    let found = if want_original {
        st.core.store.attachment_original(&id).await?
    } else {
        st.core.store.attachment_preview(&id).await?
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
    State(st): State<AppState>,
    _id: Identity,
    Query(p): Query<ListParams>,
) -> Result<Json<Vec<crate::store::corpora::Corpus>>> {
    Ok(Json(
        st.core
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

async fn get_corpus(
    State(st): State<AppState>,
    _id: Identity,
    Path(cid): Path<String>,
) -> Result<Json<CorpusDetail>> {
    let source = st.core.store.get_corpus(&cid).await?;
    let chunks = st.core.store.artifacts_for_corpus(&cid).await?;
    Ok(Json(CorpusDetail { source, chunks }))
}

async fn delete_corpus(
    State(st): State<AppState>,
    _id: Identity,
    Path(cid): Path<String>,
) -> Result<StatusCode> {
    st.core.delete_corpus(&cid).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn reprocess(
    State(st): State<AppState>,
    _id: Identity,
    Path(cid): Path<String>,
    Json(req): Json<ReprocessRequest>,
) -> Result<StatusCode> {
    let stage = Stage::parse(&req.stage)
        .ok_or_else(|| Error::Validation(format!("unknown stage `{}`", req.stage)))?;
    st.core.reprocess(&cid, stage).await?;
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
    State(st): State<AppState>,
    _id: Identity,
    Path(cid): Path<String>,
    Json(body): Json<ResolveBody>,
) -> Result<Json<serde_json::Value>> {
    st.core.resolve_near_duplicate(&cid, body.action).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// What consolidation has decided and what it is still asking about.
async fn consolidation(
    State(st): State<AppState>,
    _id: Identity,
) -> Result<Json<serde_json::Value>> {
    use crate::store::pairs::PairState;
    Ok(Json(serde_json::json!({
        "superseded": st.core.store.superseded_artifacts(100).await?,
        // What the judge actually ruled on, listed first for the same reason
        // Ops puts it at the top: it is the one output here that cost a model
        // call, and an operator reading only `pairs` would conclude there was
        // nothing to look at.
        "contradictions": st
            .core
            .store
            .pairs_by_state(PairState::Contradiction, 100)
            .await?,
        // Judge-proposed supersedes awaiting an operator's confirmation. Listed
        // for the same reason Ops renders them: without this a pair the judge
        // ruled on simply disappears from `pairs`, and an API consumer never
        // sees the proposal it left behind.
        "supersede_proposals": st
            .core
            .store
            .pairs_by_state(PairState::Superseded, 100)
            .await?,
        // Merge verdicts recorded while autonomy is off. Their own key for
        // the same reason they are their own state: a consumer counting
        // `contradictions` must not see pairs the model judged complementary.
        "merge_proposals": st
            .core
            .store
            .pairs_by_state(PairState::WouldMerge, 100)
            .await?,
        "pairs": st
            .core
            .store
            .pairs_by_state(PairState::Pending, 100)
            .await?,
    })))
}

#[derive(serde::Deserialize)]
pub struct SearchParams {
    pub q: String,
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
}

async fn search(
    State(st): State<AppState>,
    id: Identity,
    Query(q): Query<SearchParams>,
) -> Result<Json<Vec<crate::core::search::SearchResult>>> {
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
    };
    // Coalescing folds a keystroke into the query it was an early spelling of,
    // and it folds only within one scope — so a box that types has to say who
    // is typing, or two operators' panels fold into each other's queries. A
    // deliberate API call is one event and has nothing to fold with.
    let origin: crate::store::feedback::Origin = if typing {
        door.by(id.subject)
    } else {
        door.into()
    };
    Ok(Json(st.core.search(&query, origin).await?))
}

#[derive(serde::Deserialize)]
pub struct StaleParams {
    pub limit: Option<usize>,
}

/// Active artifacts nobody has confirmed or retrieved in a while — candidates
/// for an operator to review and deprecate. Read-only: nothing here changes
/// an artifact, and nothing here feeds search ranking.
async fn stale(
    State(st): State<AppState>,
    _id: Identity,
    Query(p): Query<StaleParams>,
) -> Result<Json<Vec<crate::core::search::SearchResult>>> {
    Ok(Json(st.core.stale_candidates(p.limit.unwrap_or(20)).await?))
}

async fn ask(
    State(st): State<AppState>,
    _id: Identity,
    Json(req): Json<crate::core::ask::AskRequest>,
) -> Result<Json<crate::core::ask::AskResponse>> {
    Ok(Json(st.core.ask(&req).await?))
}

#[derive(serde::Deserialize)]
pub struct ResurfaceParams {
    pub limit: Option<usize>,
}

async fn resurface(
    State(st): State<AppState>,
    _id: Identity,
    Query(p): Query<ResurfaceParams>,
) -> Result<Json<Vec<crate::core::search::SearchResult>>> {
    Ok(Json(st.core.resurface(p.limit.unwrap_or(5)).await?))
}

async fn get_artifact(
    State(st): State<AppState>,
    _id: Identity,
    Path(cid): Path<String>,
) -> Result<Json<crate::store::artifacts::Chunk>> {
    Ok(Json(st.core.store.get_artifact(&cid).await?))
}

async fn patch_artifact(
    State(st): State<AppState>,
    _id: Identity,
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
    let category = req
        .category
        .map(|c| clean_optional(c, MAX_CATEGORY_LEN, "category"))
        .transpose()?;
    let tags = req.tags.as_deref().map(clean_tags).transpose()?;

    st.core.store.get_artifact(&cid).await?;

    // The embedder is shown the title followed by the body, so either of those
    // invalidates the stored vector. A category or a tag changes only what the
    // payload says about the chunk.
    let revectorize = text.is_some() || title.is_some();

    if let Some(t) = &text {
        st.core.store.update_artifact_text(&cid, t).await?;
    }
    if let Some(t) = &title {
        st.core
            .store
            .update_artifact_title(&cid, t.as_deref())
            .await?;
    }
    if let Some(c) = &category {
        st.core
            .store
            .update_artifact_category(&cid, c.as_deref())
            .await?;
    }
    if let Some(t) = &tags {
        st.core.store.update_artifact_tags(&cid, t).await?;
    }

    let chunk = st.core.store.get_artifact(&cid).await?;
    if revectorize {
        st.core
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
        st.core
            .vectors
            .set_payload(&crate::vector::VectorPayload {
                artifact_id: chunk.id.clone(),
                corpus_id: chunk.corpus_id.clone().unwrap_or_default(),
                provenance: Some(chunk.provenance.as_str().to_string()),
                text: chunk.text.clone(),
                title: chunk.title.clone(),
                category: chunk.category.clone(),
                tags: chunk.tags.clone(),
                created_at: chunk.created_at,
                last_seen_at: None,
                hit_count: None,
                superseded: None,
                status: None,
                last_verified_at: None,
                superseded_by: None,
            })
            .await?;
    }
    Ok(Json(chunk))
}

async fn delete_artifact(
    State(st): State<AppState>,
    _id: Identity,
    Path(cid): Path<String>,
) -> Result<StatusCode> {
    // Both stores, in the order that survives an interruption — see
    // `Core::delete_artifact`, which the UI button posts to as well.
    st.core.delete_artifact(&cid).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn status(State(st): State<AppState>, _id: Identity) -> Result<Json<StatusResponse>> {
    use sqlx::Row;
    let corpus_rows = sqlx::query("SELECT status, COUNT(*) AS n FROM corpora GROUP BY status")
        .fetch_all(&st.core.store.pool)
        .await?;
    let chunks: i64 = sqlx::query("SELECT COUNT(*) AS n FROM artifacts")
        .fetch_one(&st.core.store.pool)
        .await?
        .get("n");

    Ok(Json(StatusResponse {
        sources: corpus_rows
            .iter()
            .map(|r| (r.get("status"), r.get("n")))
            .collect(),
        jobs: st.core.store.job_counts().await?,
        failed: st.core.store.failed_jobs(50).await?,
        oldest_pending_secs: st.core.store.oldest_pending_age().await?,
        chunks,
        // Qdrant being briefly unreachable should not fail the status page,
        // which is exactly where you look when something is wrong.
        vectors: st.core.vectors.count().await.unwrap_or(0),
    }))
}

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
        .route("/corpora/{id}/reprocess", post(reprocess))
        .route("/corpora/{id}/resolve", post(resolve_near_dupe))
        .route("/search", get(search))
        .route("/ask", post(ask))
        .route("/resurface", get(resurface))
        .route("/consolidation", get(consolidation))
        .route("/consolidation/stale", get(stale))
        .route(
            "/artifacts/{id}",
            get(get_artifact)
                .patch(patch_artifact)
                .delete(delete_artifact),
        )
        .route("/status", get(status))
}

#[cfg(test)]
pub(crate) mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    async fn app_and_token() -> (axum::Router, String) {
        let (app, token, _core) = app_token_and_core().await;
        (app, token)
    }

    pub async fn app_token_and_core() -> (axum::Router, String, crate::core::Core) {
        app_from_core(crate::core::test_support::test_core().await).await
    }

    /// Wrap a core a test has already adjusted — feedback switched on, say —
    /// in the real router. Factored out rather than duplicated so there stays
    /// one way to build a test app, and it is the one the binary builds.
    pub async fn app_from_core(
        core: crate::core::Core,
    ) -> (axum::Router, String, crate::core::Core) {
        let (_, token) = crate::auth::tokens::mint(&core.store, "test", "user-1")
            .await
            .unwrap();
        let state_core = core.clone();
        let state = crate::web::state::AppState {
            core,
            auth: std::sync::Arc::new(crate::web::state::AuthContext {
                mode: crate::config::AuthMode::Local,
                local: None,
                oidc: None,
                pending: crate::auth::oidc::PendingStore::new(),
                secure_cookies: false,
            }),
        };
        (crate::web::router(state), token, state_core)
    }

    fn get(uri: &str, token: Option<&str>) -> Request<Body> {
        let mut b = Request::builder().uri(uri).method("GET");
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        b.body(Body::empty()).unwrap()
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

    async fn json_of(res: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    }

    /// A minimal multipart body. Hand-rolled rather than pulling a builder in
    /// for three tests.
    /// `mime` of `None` omits the part's `Content-Type` header entirely, which
    /// is legal and which a client may well do.
    fn post_file(
        uri: &str,
        token: &str,
        filename: &str,
        mime: Option<&str>,
        body: &[u8],
    ) -> Request<Body> {
        const B: &str = "engramtestboundary";
        let mut buf: Vec<u8> = Vec::new();
        let typed = match mime {
            Some(m) => format!("Content-Type: {m}\r\n"),
            None => String::new(),
        };
        buf.extend_from_slice(
            format!(
                "--{B}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n\
                 {typed}\r\n"
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
                format!("--{B}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n")
                    .as_bytes(),
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
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
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
        core.feedback.enabled = true;
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
                    category: Some("note".into()),
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
                    category: Some("note".into()),
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
        core.feedback.enabled = true;
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
    async fn an_uploaded_filename_becomes_the_title_hint() {
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
        let (app, token) = app_and_token().await;
        let res = app
            .oneshot(post_file(
                "/api/v1/corpora/upload",
                &token,
                "notes.pdf",
                Some("application/pdf"),
                b"%PDF-1.7",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let msg = json_of(res).await["error"]
            .as_str()
            .unwrap_or_default()
            .to_string();
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
