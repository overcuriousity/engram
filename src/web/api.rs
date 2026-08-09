use crate::auth::Identity;
use crate::core::search::SearchQuery;
use crate::error::{Error, Result};
use crate::store::jobs::{FailedJob, Stage};
use crate::web::state::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};

#[derive(serde::Deserialize)]
pub struct IngestRequest {
    pub text: String,
    #[serde(default)]
    pub title: Option<String>,
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

async fn ingest(
    State(st): State<AppState>,
    _id: Identity,
    Json(req): Json<IngestRequest>,
) -> Result<(StatusCode, Json<crate::core::ingest::IngestOutcome>)> {
    let out = st
        .core
        .ingest(&req.text, "web", req.title.as_deref())
        .await?;
    // 201 for a new capture, 200 when the text was already stored.
    let code = if out.duplicate {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((code, Json(out)))
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
) -> Result<Json<Vec<crate::store::corpora::Source>>> {
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
    pub source: crate::store::corpora::Source,
    pub chunks: Vec<crate::store::artifacts::Chunk>,
}

async fn get_corpus(
    State(st): State<AppState>,
    _id: Identity,
    Path(sid): Path<String>,
) -> Result<Json<CorpusDetail>> {
    let source = st.core.store.get_corpus(&sid).await?;
    let chunks = st.core.store.artifacts_for_corpus(&sid).await?;
    Ok(Json(CorpusDetail { source, chunks }))
}

async fn delete_corpus(
    State(st): State<AppState>,
    _id: Identity,
    Path(sid): Path<String>,
) -> Result<StatusCode> {
    st.core.delete_corpus(&sid).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn reprocess(
    State(st): State<AppState>,
    _id: Identity,
    Path(sid): Path<String>,
    Json(req): Json<ReprocessRequest>,
) -> Result<StatusCode> {
    let stage = Stage::parse(&req.stage)
        .ok_or_else(|| Error::Validation(format!("unknown stage `{}`", req.stage)))?;
    st.core.reprocess(&sid, stage).await?;
    Ok(StatusCode::ACCEPTED)
}

#[derive(serde::Deserialize)]
pub struct SearchParams {
    pub q: String,
    pub limit: Option<usize>,
    pub tags: Option<String>,
    pub category: Option<String>,
}

async fn search(
    State(st): State<AppState>,
    _id: Identity,
    Query(q): Query<SearchParams>,
) -> Result<Json<Vec<crate::core::search::SearchResult>>> {
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
        // An API call is one deliberate question; only the typing UI opts out.
        mark: true,
    };
    Ok(Json(st.core.search(&query).await?))
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
        st.core.store.enqueue(Stage::Embed, "chunk", &cid).await?;
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
                corpus_id: chunk.corpus_id.clone(),
                text: chunk.text.clone(),
                title: chunk.title.clone(),
                category: chunk.category.clone(),
                tags: chunk.tags.clone(),
                created_at: chunk.created_at,
                last_seen_at: None,
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
    st.core.store.get_artifact(&cid).await?;
    st.core
        .vectors
        .delete_artifacts(std::slice::from_ref(&cid))
        .await?;
    st.core.store.delete_artifact(&cid).await?;
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

pub fn api_router() -> Router<AppState> {
    Router::new()
        .route("/sources", post(ingest).get(list_corpora))
        .route("/sources/{id}", get(get_corpus).delete(delete_corpus))
        .route("/sources/{id}/reprocess", post(reprocess))
        .route("/search", get(search))
        .route("/ask", post(ask))
        .route("/resurface", get(resurface))
        .route(
            "/chunks/{id}",
            get(get_artifact)
                .patch(patch_artifact)
                .delete(delete_artifact),
        )
        .route("/status", get(status))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    async fn app_and_token() -> (axum::Router, String) {
        let (app, token, _core) = app_token_and_core().await;
        (app, token)
    }

    pub async fn app_token_and_core() -> (axum::Router, String, crate::core::Core) {
        let core = crate::core::test_support::test_core().await;
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

    #[tokio::test]
    async fn every_api_route_rejects_an_unauthenticated_request() {
        // A missing auth check on one route is the failure mode that matters
        // most here, so assert it route by route rather than spot-checking.
        let (app, _) = app_and_token().await;
        for (method, uri) in [
            ("GET", "/api/v1/search?q=x"),
            ("GET", "/api/v1/resurface"),
            ("GET", "/api/v1/sources"),
            ("POST", "/api/v1/sources"),
            ("GET", "/api/v1/sources/abc"),
            ("DELETE", "/api/v1/sources/abc"),
            ("POST", "/api/v1/sources/abc/reprocess"),
            ("POST", "/api/v1/ask"),
            ("GET", "/api/v1/chunks/abc"),
            ("PATCH", "/api/v1/chunks/abc"),
            ("DELETE", "/api/v1/chunks/abc"),
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
                "/api/v1/sources",
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
                .oneshot(post_json("/api/v1/sources", &token, body.clone()))
                .await
                .unwrap(),
        )
        .await;
        let res = app
            .oneshot(post_json("/api/v1/sources", &token, body))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let second = json_of(res).await;
        assert_eq!(first["id"], second["id"]);
        assert_eq!(second["duplicate"], true);
    }

    #[tokio::test]
    async fn empty_ingest_is_a_400() {
        let (app, token) = app_and_token().await;
        let res = app
            .oneshot(post_json(
                "/api/v1/sources",
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
                "/api/v1/sources",
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
            .oneshot(get("/api/v1/sources/nope", Some(&token)))
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
                    "/api/v1/sources",
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
                &format!("/api/v1/sources/{id}/reprocess"),
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
            .oneshot(get("/api/v1/sources/nope", Some(&token)))
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
                "/api/v1/sources",
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
    use axum::http::StatusCode;
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
                &format!("/api/v1/chunks/{cid}"),
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
            &format!("/api/v1/chunks/{cid}"),
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
            &format!("/api/v1/chunks/{cid}"),
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
                &format!("/api/v1/chunks/{cid}"),
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
                &format!("/api/v1/chunks/{cid}"),
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
            &format!("/api/v1/chunks/{cid}"),
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
            &format!("/api/v1/chunks/{cid}"),
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
                &format!("/api/v1/chunks/{cid}"),
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
                &format!("/api/v1/chunks/{cid}"),
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
                &format!("/api/v1/chunks/{cid}"),
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
            &format!("/api/v1/chunks/{cid}"),
            &token,
            serde_json::json!({ "title": "   " }),
        ))
        .await
        .unwrap();
        assert_eq!(core.store.get_artifact(&cid).await.unwrap().title, None);
    }

    #[tokio::test]
    async fn an_empty_text_is_still_rejected() {
        let (app, token, _core, cid) = one_artifact().await;
        let res = app
            .oneshot(patch_json(
                &format!("/api/v1/chunks/{cid}"),
                &token,
                serde_json::json!({ "text": "   " }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }
}
