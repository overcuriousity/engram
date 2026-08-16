use crate::core::Core;
use crate::core::search::{SearchQuery, SearchResult};
use crate::web::state::AppState;
use axum::Router;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};

/// Results go straight into an agent's context, so they stay markdown: the
/// artifact text is already markdown, and the surrounding structure has to be
/// readable rather than JSON-shaped.
pub fn format_search_results(results: &[SearchResult]) -> String {
    if results.is_empty() {
        return "No matches in the knowledge base.".to_string();
    }
    results
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let title = r.title.clone().unwrap_or_else(|| "Untitled".into());
            let tags = if r.tags.is_empty() {
                String::new()
            } else {
                format!(" · {}", r.tags.join(", "))
            };
            // An agent reads this as a ranked list unless it is told otherwise,
            // and an associated hit did not compete for its place.
            let how = match (&r.via, &r.reason) {
                (Some(_), Some(why)) => format!("recalled beside the answer — {why}"),
                (Some(_), None) => "recalled beside the answer".to_string(),
                (None, _) => format!("score {:.3}", r.score),
            };
            format!(
                "### {}. {title}\n_{how}{tags} · corpus: {}_\n\n{}",
                i + 1,
                r.corpus_id,
                r.text
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n---\n\n")
}

#[derive(Clone)]
pub struct PkdbTools {
    pub core: Core,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct IngestParams {
    /// The text to store verbatim.
    pub text: String,
    /// Optional short label for the corpus.
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SearchParams {
    /// The situation to find something for, in natural language. A sentence or
    /// a paragraph of context ranks better than keywords: the query is embedded
    /// whole, so pass what the user actually said or what you are looking at.
    pub q: String,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub category: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct AskParams {
    /// A question to answer from the knowledge base.
    pub q: String,
}

#[tool_router(server_handler)]
impl PkdbTools {
    #[tool(
        name = "ingest",
        description = "Store text in the personal knowledge base. Returns immediately; \
                       segmentation and embedding happen in the background."
    )]
    async fn ingest(&self, Parameters(p): Parameters<IngestParams>) -> String {
        match self.core.ingest(&p.text, "mcp", p.title.as_deref()).await {
            Ok(o) if o.duplicate => format!("Already stored as `{}`.", o.id),
            // A parked capture is stored and nothing more: no segmentation, no
            // embedding, and nothing searchable until a person decides. Saying
            // "runs in the background" here would have the agent report a
            // success that never happens.
            Ok(o) if o.near_duplicate.is_some() => {
                let n = o.near_duplicate.expect("just checked");
                format!(
                    "Stored as `{}`, but held for review: it is {:.0}% similar to `{}`, \
                     so it is not segmented or indexed until someone decides between \
                     them in the web UI.",
                    o.id,
                    n.similarity * 100.0,
                    n.corpus_id
                )
            }
            Ok(o) => format!(
                "Stored as `{}`. Segmentation and embedding run in the background.",
                o.id
            ),
            Err(e) => format!("Ingest failed: {e}"),
        }
    }

    #[tool(
        name = "search",
        description = "Search the personal knowledge base by meaning. Returns ranked \
                       markdown artifacts, not a generated answer."
    )]
    async fn search(&self, Parameters(p): Parameters<SearchParams>) -> String {
        let query = SearchQuery {
            q: p.q,
            limit: p.limit.unwrap_or(0) as usize,
            tags: p.tags.unwrap_or_default(),
            category: p.category,
            // A tool call is one deliberate question, not a keystroke.
            mark: true,
            include_deprecated: false,
            include_superseded: false,
        };
        match self
            .core
            .search(&query, crate::store::feedback::Door::Mcp)
            .await
        {
            Ok(r) => format_search_results(&r),
            Err(e) => format!("Search failed: {e}"),
        }
    }

    #[tool(
        name = "ask",
        description = "Answer a question by synthesising across knowledge-base artifacts. \
                       Slower than search; prefer search unless synthesis is needed."
    )]
    async fn ask(&self, Parameters(p): Parameters<AskParams>) -> String {
        match self
            .core
            .ask(&crate::core::ask::AskRequest {
                q: p.q,
                limit: None,
                tags: vec![],
                category: None,
            })
            .await
        {
            Ok(a) => {
                let mut out = a.answer;
                if !a.citations.is_empty() {
                    out.push_str("\n\n---\n\n**Sources**\n\n");
                    out.push_str(&format_search_results(&a.citations));
                }
                if a.dropped > 0 {
                    out.push_str(&format!(
                        "\n\n_{} further excerpt(s) omitted for context budget._",
                        a.dropped
                    ));
                }
                out
            }
            Err(e) => format!("Ask failed: {e}"),
        }
    }
}

/// Guard in front of the MCP service. Extracting `Identity` here means an
/// unauthenticated request is rejected before any tool can run.
async fn mcp_guard(
    _id: crate::auth::Identity,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    next.run(req).await
}

pub fn mcp_router(state: AppState) -> Router<AppState> {
    use rmcp::transport::streamable_http_server::{
        StreamableHttpService, session::local::LocalSessionManager,
    };

    let core = state.core.clone();
    let service = StreamableHttpService::new(
        move || Ok(PkdbTools { core: core.clone() }),
        std::sync::Arc::new(LocalSessionManager::default()),
        Default::default(),
    );

    Router::new()
        .route_service("/mcp", service)
        .layer(axum::middleware::from_fn_with_state(state, mcp_guard))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn mcp_requires_a_bearer_token() {
        let core = crate::core::test_support::test_core().await;
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
        let res = crate::web::router(state)
            .oneshot(
                Request::builder()
                    .uri("/mcp")
                    .method("POST")
                    .header("content-type", "application/json")
                    .header("accept", "application/json, text/event-stream")
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn search_results_are_formatted_as_markdown_for_an_agent() {
        let core = crate::core::test_support::test_core().await;
        core.ingest("## Mounting\nRun `mount /dev/sda1 /mnt`.", "mcp", None)
            .await
            .unwrap();
        while crate::jobs::run_one(&core).await.unwrap() {}

        let text = format_search_results(
            &core
                .search(
                    &SearchQuery {
                        q: "mounting".into(),
                        limit: 5,
                        tags: vec![],
                        category: None,
                        mark: true,
                        include_deprecated: false,
                        include_superseded: false,
                    },
                    crate::store::feedback::Door::Ui,
                )
                .await
                .unwrap(),
        );

        // An agent consumes this directly, so it must stay markdown and keep
        // the corpus id for follow-up lookups.
        assert!(text.contains("mount /dev/sda1"), "{text}");
        assert!(text.contains("corpus:"), "{text}");
    }

    #[test]
    fn empty_results_produce_a_clear_message_not_an_empty_string() {
        let text = format_search_results(&[]);
        assert!(!text.trim().is_empty());
        assert!(text.to_lowercase().contains("no match"));
    }

    fn hit(id: &str, via: Option<&str>) -> SearchResult {
        SearchResult {
            artifact_id: id.into(),
            corpus_id: "c".into(),
            title: Some(id.into()),
            text: "body".into(),
            category: None,
            tags: vec![],
            score: 0.5,
            status: None,
            superseded_by: None,
            last_verified_at: None,
            weak: false,
            primed: false,
            via: via.map(str::to_string),
            reason: None,
        }
    }

    #[test]
    fn an_associated_result_says_it_was_recalled_rather_than_ranked() {
        // Straight into an agent's context: without this the extra result reads
        // as the fourth-best match for the query, which it is not.
        let out = format_search_results(&[hit("ranked", None), hit("recalled", Some("ranked"))]);
        assert!(out.contains("recalled beside"), "{out}");
    }
}
