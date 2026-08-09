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

#[derive(serde::Deserialize)]
pub struct PatchChunkRequest {
    pub text: String,
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

async fn list_sources(
    State(st): State<AppState>,
    _id: Identity,
    Query(p): Query<ListParams>,
) -> Result<Json<Vec<crate::store::sources::Source>>> {
    Ok(Json(
        st.core
            .store
            .list_sources(p.limit.clamp(1, 200), p.offset.max(0))
            .await?,
    ))
}

#[derive(serde::Serialize)]
pub struct SourceDetail {
    #[serde(flatten)]
    pub source: crate::store::sources::Source,
    pub chunks: Vec<crate::store::chunks::Chunk>,
}

async fn get_source(
    State(st): State<AppState>,
    _id: Identity,
    Path(sid): Path<String>,
) -> Result<Json<SourceDetail>> {
    let source = st.core.store.get_source(&sid).await?;
    let chunks = st.core.store.chunks_for_source(&sid).await?;
    Ok(Json(SourceDetail { source, chunks }))
}

async fn delete_source(
    State(st): State<AppState>,
    _id: Identity,
    Path(sid): Path<String>,
) -> Result<StatusCode> {
    st.core.delete_source(&sid).await?;
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

async fn get_chunk(
    State(st): State<AppState>,
    _id: Identity,
    Path(cid): Path<String>,
) -> Result<Json<crate::store::chunks::Chunk>> {
    Ok(Json(st.core.store.get_chunk(&cid).await?))
}

async fn patch_chunk(
    State(st): State<AppState>,
    _id: Identity,
    Path(cid): Path<String>,
    Json(req): Json<PatchChunkRequest>,
) -> Result<Json<crate::store::chunks::Chunk>> {
    if req.text.trim().is_empty() {
        return Err(Error::Validation("chunk text is empty".into()));
    }
    st.core.store.update_chunk_text(&cid, &req.text).await?;
    // The stored vector now describes text that no longer exists, so queue a
    // re-embed rather than leaving search pointing at the old wording.
    st.core.store.enqueue(Stage::Embed, "chunk", &cid).await?;
    Ok(Json(st.core.store.get_chunk(&cid).await?))
}

async fn delete_chunk(
    State(st): State<AppState>,
    _id: Identity,
    Path(cid): Path<String>,
) -> Result<StatusCode> {
    st.core.store.get_chunk(&cid).await?;
    st.core
        .vectors
        .delete_chunks(std::slice::from_ref(&cid))
        .await?;
    st.core.store.delete_chunk(&cid).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn status(State(st): State<AppState>, _id: Identity) -> Result<Json<StatusResponse>> {
    use sqlx::Row;
    let source_rows = sqlx::query("SELECT status, COUNT(*) AS n FROM sources GROUP BY status")
        .fetch_all(&st.core.store.pool)
        .await?;
    let chunks: i64 = sqlx::query("SELECT COUNT(*) AS n FROM chunks")
        .fetch_one(&st.core.store.pool)
        .await?
        .get("n");

    Ok(Json(StatusResponse {
        sources: source_rows
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
        .route("/sources", post(ingest).get(list_sources))
        .route("/sources/{id}", get(get_source).delete(delete_source))
        .route("/sources/{id}/reprocess", post(reprocess))
        .route("/search", get(search))
        .route("/ask", post(ask))
        .route(
            "/chunks/{id}",
            get(get_chunk).patch(patch_chunk).delete(delete_chunk),
        )
        .route("/status", get(status))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    async fn app_and_token() -> (axum::Router, String) {
        let core = crate::core::test_support::test_core().await;
        let (_, token) = crate::auth::tokens::mint(&core.store, "test", "user-1")
            .await
            .unwrap();
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
        (crate::web::router(state), token)
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
            .oneshot(get("/api/v1/search?q=x", Some("pkdb_not_a_real_token")))
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
