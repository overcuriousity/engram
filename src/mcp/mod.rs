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
    let mut rank = 0;
    results
        .iter()
        .map(|r| {
            // Never "Untitled": a verbatim passage has no title by design, and
            // an agent reading a list of them has the same problem the search
            // rail has — a column of a word that says nothing where a name
            // would say something. The opening of the passage says what it is.
            let title = r
                .title
                .clone()
                .unwrap_or_else(|| crate::web::markdown::stand_in_title(&r.text, 60));
            // `stand_in_title` takes markup and leading punctuation off the
            // front, so a passage that is only those leaves nothing — and a
            // heading with no text after it is a numbered entry an agent
            // cannot refer back to. The id is a poor name and a working one.
            let title = if title.is_empty() {
                r.artifact_id.clone()
            } else {
                title
            };
            let tags = if r.tags.is_empty() {
                String::new()
            } else {
                format!(" · {}", r.tags.join(", "))
            };
            // An agent reads this as a ranked list unless it is told
            // otherwise, and a bare ordinal is the strongest ranking signal
            // there is. An associated hit did not compete for a place, so it
            // gets no number at all rather than one that continues the count.
            let heading = if r.via.is_none() {
                rank += 1;
                format!("### {rank}. {title}")
            } else {
                format!("### {title}")
            };
            // An agent reading a numbered list has no grey to see, so a hit
            // past the cliff says so in words.
            let how = match (&r.via, &r.reason) {
                (Some(_), Some(why)) => format!("recalled beside the answer — {why}"),
                (Some(_), None) => "recalled beside the answer".to_string(),
                (None, _) if r.past_cliff => {
                    format!("score {:.3} · below the relevance cliff", r.score)
                }
                (None, _) => format!("score {:.3}", r.score),
            };
            format!(
                "{heading}\n_{how}{tags} · corpus: {}_\n\n{}",
                r.corpus_id, r.text
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n---\n\n")
}

/// Where a tool call's `Core` comes from.
///
/// The whole point of the enum is that the production variant does *not* hold
/// one. See `PkdbTools`.
#[derive(Clone)]
enum CoreSource {
    /// One fixed core, held for the life of the tools. The single-base tests,
    /// and nothing that serves a request. Boxed because it is by far the larger
    /// of the two and by far the rarer.
    Fixed(Box<Core>),
    /// Resolved from the registry on every call, by subject.
    Tenant(std::sync::Arc<crate::tenants::Tenants>, String),
}

impl CoreSource {
    async fn core(&self) -> crate::error::Result<Core> {
        match self {
            CoreSource::Fixed(c) => Ok((**c).clone()),
            CoreSource::Tenant(tenants, subject) => Ok(tenants.get(subject).await?.core),
        }
    }
}

#[derive(Clone)]
pub struct PkdbTools {
    source: CoreSource,
    /// Whether `ask` is a tool at all.
    ///
    /// Carried rather than asked of the core, because `routes` is synchronous —
    /// the handler macro calls it — and resolving a tenant is not. It costs
    /// nothing to carry: `Core::asks` is `infer.complete` being configured,
    /// which is instance-wide and identical for every tenant on it.
    asks: bool,
}

impl PkdbTools {
    /// Tools over one fixed core.
    pub fn over(core: Core) -> PkdbTools {
        PkdbTools {
            asks: core.asks(),
            source: CoreSource::Fixed(Box::new(core)),
        }
    }

    /// Tools that look their tenant up when a call arrives.
    fn for_subject(
        tenants: std::sync::Arc<crate::tenants::Tenants>,
        subject: String,
        asks: bool,
    ) -> PkdbTools {
        PkdbTools {
            source: CoreSource::Tenant(tenants, subject),
            asks,
        }
    }
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

#[tool_router]
impl PkdbTools {
    /// The tools this core can actually serve. `ask` is not a tool that says
    /// "not configured" — it is not a tool.
    pub(crate) fn routes(&self) -> rmcp::handler::server::router::tool::ToolRouter<Self> {
        let mut r = Self::tool_router();
        if !self.asks {
            r.remove_route("ask");
        }
        r
    }

    #[tool(
        name = "ingest",
        description = "Store text in the personal knowledge base. Returns immediately; \
                       segmentation and embedding happen in the background."
    )]
    async fn ingest(&self, Parameters(p): Parameters<IngestParams>) -> String {
        let core = match self.source.core().await {
            Ok(c) => c,
            Err(e) => return format!("Ingest failed: {e}"),
        };
        match core.ingest(&p.text, "mcp", p.title.as_deref()).await {
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
            rerank: true,
        };
        let core = match self.source.core().await {
            Ok(c) => c,
            Err(e) => return format!("Search failed: {e}"),
        };
        match core.search(&query, crate::store::feedback::Door::Mcp).await {
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
        let core = match self.source.core().await {
            Ok(c) => c,
            Err(e) => return format!("Ask failed: {e}"),
        };
        match core
            .ask(
                &crate::core::ask::AskRequest {
                    q: p.q,
                    limit: None,
                    tags: vec![],
                    category: None,
                },
                crate::store::feedback::Door::Mcp,
            )
            .await
        {
            Ok(a) => format_answer(&a),
            Err(e) => format!("Ask failed: {e}"),
        }
    }
}

#[rmcp::tool_handler(router = self.routes())]
impl rmcp::ServerHandler for PkdbTools {}

/// An answer, with everything the page would have shown around it.
///
/// A function rather than prose inside the tool, because MCP is the door with
/// no page: an agent gets this string and nothing else, so what it does or does
/// not say is the whole of what the caller knows — and that is worth being able
/// to test directly.
///
/// Order is by consequence. Truncation first, because an agent reading a
/// cut-off answer as a complete one acts on half a procedure. Then the literals
/// the base does not hold, because the page marks those inline and badges them
/// and an agent sees neither — and an agent is the caller most likely to *run*
/// a fabricated command. Named rather than counted: "one unsupported literal"
/// is not actionable, the literal is.
fn format_answer(a: &crate::core::ask::AskResponse) -> String {
    let mut out = a.answer.clone();
    if a.truncated {
        out.push_str(
            "\n\n_This answer was cut off at the configured answer length limit \
             (ask.max_output_tokens) and is incomplete._",
        );
    }
    if !a.unsupported.is_empty() {
        let named = a
            .unsupported
            .iter()
            .map(|l| format!("`{l}`"))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!(
            "\n\n_Not from the knowledge base — these appear in no cited excerpt, \
             and the model wrote them: {named}._"
        ));
    }
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

/// Guard in front of the MCP service. Extracting `Identity` here means an
/// unauthenticated request is rejected before any tool can run.
async fn mcp_guard(
    _id: crate::auth::Identity,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    next.run(req).await
}

/// rmcp's DNS-rebinding guard accepts loopback hosts only, which is right for
/// a laptop and wrong behind a reverse proxy: there the Host header is the
/// deployment's public name, and a guard that does not know it answers 403 to
/// every remote client. The public host comes from the OIDC redirect URL, the
/// one place the configuration already states its own address.
fn service_config(
    public_host: Option<String>,
) -> rmcp::transport::streamable_http_server::StreamableHttpServerConfig {
    use rmcp::transport::streamable_http_server::StreamableHttpServerConfig;

    let mut hosts = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ];
    if let Some(host) = public_host {
        hosts.push(host);
    }
    StreamableHttpServerConfig::default().with_allowed_hosts(hosts)
}

/// One MCP service per tenant, built on first use and kept — up to the same
/// cap the tenant registry holds cores to.
///
/// `StreamableHttpService` is constructed with the tools it will serve, so a
/// single service is a single user's tools, and the door has to pick the right
/// one before the request reaches it. Building one per tenant also gives each
/// user their own `LocalSessionManager`, which is what an MCP session ought to
/// be: one client talking to one base.
///
/// What the tools carry is a subject and the registry, never a `Core`. Holding
/// the core the door happened to be given would pin it here for as long as the
/// service stayed in this map — a second lifetime beside the registry's own, so
/// an instance would hold up to `2 × max_open_tenants` SQLite pools and vector
/// clients with one cap in charge of each half and neither in charge of the
/// total. It also kept `Working::is_idle` false forever for anybody who had
/// ever used `/mcp`, since a pinned core is a live `Arc` on their sittings, so
/// the registry could never reap their working memory. Resolving per call puts
/// `store.max_open_tenants` back in sole charge of what is open.
///
/// The map still cannot only grow — an entry keyed by subject with nothing that
/// removes it is a `LocalSessionManager` per person who has ever opened `/mcp`
/// — so: a recency list beside it, and the same cap.
///
/// Evicting a service ends the MCP sessions it was tracking; a client that
/// comes back initializes a new one, which is the ordinary reconnect path.
type TenantServices = std::sync::Mutex<(
    std::collections::HashMap<
        String,
        rmcp::transport::streamable_http_server::StreamableHttpService<
            PkdbTools,
            rmcp::transport::streamable_http_server::session::local::LocalSessionManager,
        >,
    >,
    Vec<String>,
)>;

fn service_for(
    services: &TenantServices,
    tenants: &std::sync::Arc<crate::tenants::Tenants>,
    tenant: &crate::tenants::Tenant,
    public_host: Option<String>,
    cap: usize,
) -> rmcp::transport::streamable_http_server::StreamableHttpService<
    PkdbTools,
    rmcp::transport::streamable_http_server::session::local::LocalSessionManager,
> {
    use rmcp::transport::streamable_http_server::{
        StreamableHttpService, session::local::LocalSessionManager,
    };
    let subject = tenant.user.subject.clone();
    // The one thing read off the core the door was handed, and only because
    // `routes` is synchronous. It is the same answer for every tenant.
    let asks = tenant.core.asks();
    let mut g = services.lock().expect("mcp services");
    let (map, order) = &mut *g;
    let svc = map
        .entry(subject.clone())
        .or_insert_with(|| {
            let tenants = tenants.clone();
            let subject = subject.clone();
            StreamableHttpService::new(
                move || {
                    Ok(PkdbTools::for_subject(
                        tenants.clone(),
                        subject.clone(),
                        asks,
                    ))
                },
                std::sync::Arc::new(LocalSessionManager::default()),
                service_config(public_host),
            )
        })
        .clone();
    order.retain(|s| *s != subject);
    order.push(subject);
    // The caller's own entry was just moved to the back, so it is never the
    // one dropped here.
    while map.len() > cap.max(1) {
        let Some(oldest) = order.first().cloned() else {
            break;
        };
        order.remove(0);
        map.remove(&oldest);
    }
    svc
}

pub fn mcp_router(state: AppState) -> Router<AppState> {
    let public_host = state.auth.oidc.as_ref().and_then(|o| o.public_host());
    let cap = state.config.store.max_open_tenants;
    let tenants = state.tenants.clone();
    let services: std::sync::Arc<TenantServices> = Default::default();

    Router::new()
        .route(
            "/mcp",
            axum::routing::any(
                move |tenant: crate::tenants::Tenant, req: axum::extract::Request| {
                    let services = services.clone();
                    let tenants = tenants.clone();
                    let public_host = public_host.clone();
                    async move {
                        use tower::Service;
                        let mut svc = service_for(&services, &tenants, &tenant, public_host, cap);
                        svc.call(req).await
                    }
                },
            ),
        )
        .layer(axum::middleware::from_fn_with_state(state, mcp_guard))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    /// A session manager per entry, so a map that only ever grows holds one for
    /// every person who has ever opened `/mcp`, whatever
    /// `store.max_open_tenants` says an instance holds open.
    #[tokio::test]
    async fn the_service_map_holds_no_more_tenants_than_the_registry_does() {
        let (tenants, a, b, _dir) = crate::tenants::test_support::two_tenants().await;
        let services: TenantServices = Default::default();
        service_for(&services, &tenants, &a, None, 1);
        service_for(&services, &tenants, &b, None, 1);

        let g = services.lock().unwrap();
        assert_eq!(g.0.len(), 1, "the map grew past the cap");
        assert!(
            g.0.contains_key(&b.user.subject),
            "the tenant that just asked was the one dropped"
        );
    }

    /// The tools carry a subject, not a core, so a call goes to whoever the
    /// service was built for — resolved when the call arrives, through the one
    /// registry whose cap decides what an instance holds open.
    #[tokio::test]
    async fn a_tool_call_reaches_the_subjects_own_base() {
        let (tenants, a, b, _dir) = crate::tenants::test_support::two_tenants().await;
        let tools = PkdbTools::for_subject(tenants.clone(), a.user.subject.clone(), false);
        let out = tools
            .source
            .core()
            .await
            .unwrap()
            .ingest("only a's", "mcp", None)
            .await
            .unwrap();

        assert!(
            a.core.store.get_corpus(&out.id).await.is_ok(),
            "the call did not land in the subject's own base"
        );
        assert!(
            b.core.store.get_corpus(&out.id).await.is_err(),
            "the call landed in another tenant's base"
        );
    }

    fn search_hit(title: Option<&str>, text: &str) -> SearchResult {
        SearchResult {
            artifact_id: "a".into(),
            corpus_id: "s".into(),
            title: title.map(str::to_string),
            text: text.into(),
            category: None,
            tags: vec![],
            score: 0.5,
            status: None,
            superseded_by: None,
            last_verified_at: None,
            weak: false,
            primed: false,
            in_sitting: false,
            past_cliff: false,
            via: None,
            reason: None,
            model_written: false,
            synthesized: false,
            origin_count: 0,
        }
    }

    #[test]
    fn the_mcp_door_names_a_passage_by_its_opening() {
        // Claude Code reads this list. A verbatim passage has no title by
        // design, and three headings reading "Untitled" is a list of a word
        // that says nothing where a name would say something.
        let out = format_search_results(&[search_hit(
            None,
            "Die digitale Forensik unterscheidet sich zusätzlich",
        )]);
        assert!(!out.contains("Untitled"), "{out}");
        assert!(out.contains("Die digitale Forensik"), "{out}");
    }

    fn answer(unsupported: &[&str]) -> crate::core::ask::AskResponse {
        crate::core::ask::AskResponse {
            answer: "Run `engram reindex`, then `wipefs --all /dev/sdX`.".into(),
            citations: vec![],
            dropped: 0,
            truncated: false,
            abstained: false,
            unsupported: unsupported.iter().map(|s| s.to_string()).collect(),
            event_id: None,
        }
    }

    /// MCP is the door with no page. An agent gets this string and nothing
    /// else, so a literal the base does not hold has to be named in it — and
    /// named, not counted, because an agent may run what it reads.
    #[test]
    fn the_mcp_answer_names_the_literals_the_base_does_not_hold() {
        let out = format_answer(&answer(&["wipefs --all /dev/sdX"]));
        assert!(out.contains("wipefs --all /dev/sdX"), "{out}");
        assert!(out.contains("no cited excerpt"), "{out}");
    }

    /// And says nothing when there is nothing to say: a caveat on every answer
    /// is a caveat nobody reads.
    #[test]
    fn an_answer_drawn_from_its_excerpts_carries_no_warning() {
        let out = format_answer(&answer(&[]));
        assert!(!out.contains("no cited excerpt"), "{out}");
    }

    #[tokio::test]
    async fn mcp_requires_a_bearer_token() {
        let core = crate::core::test_support::test_core().await;
        let state = crate::web::test_support::state_over(core, crate::config::AuthMode::Local).await;
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
                        rerank: true,
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
            in_sitting: false,
            past_cliff: false,
            via: via.map(str::to_string),
            reason: None,
            model_written: false,
            synthesized: false,
            origin_count: 0,
        }
    }

    /// An agent reading a numbered list has no grey to see; a hit past the
    /// cliff has to say so in words, and one above it must not.
    #[test]
    fn a_result_past_the_cliff_says_so_and_keeps_its_number() {
        let mut past = hit("tail", None);
        past.past_cliff = true;
        let out = format_search_results(&[hit("head", None), past]);
        let head = out.find("### 1. head").unwrap();
        let tail = out.find("### 2. tail").unwrap();
        let note = out.find("below the relevance cliff").unwrap();
        assert!(head < tail && tail < note, "{out}");
        assert_eq!(out.matches("below the relevance cliff").count(), 1, "{out}");
    }

    #[test]
    fn an_associated_result_says_it_was_recalled_rather_than_ranked() {
        // Straight into an agent's context: without this the extra result reads
        // as the fourth-best match for the query, which it is not.
        let out = format_search_results(&[hit("ranked", None), hit("recalled", Some("ranked"))]);
        assert!(out.contains("recalled beside"), "{out}");

        // A bare ordinal is the strongest ranking signal there is: an agent
        // reads a numbered list as ranked unless told otherwise. The ranked
        // hit keeps its number; the associated one gets none, rather than a
        // number that continues the count as if it had competed for one.
        assert!(out.contains("### 1. ranked"), "{out}");
        assert!(out.contains("### recalled"), "{out}");
        assert!(!out.contains("### 2"), "{out}");
    }

    #[test]
    fn the_mcp_door_trusts_the_deployments_own_host() {
        // rmcp's DNS-rebinding guard accepts loopback hosts only; behind the
        // reverse proxy the Host header is the public name, and without this
        // the door answers 403 to every remote client.
        let config = service_config(Some("engram.example".to_string()));
        assert!(
            config.allowed_hosts.iter().any(|h| h == "engram.example"),
            "{:?}",
            config.allowed_hosts
        );
        // Loopback stays: local development talks to the same door directly.
        assert!(
            config.allowed_hosts.iter().any(|h| h == "127.0.0.1"),
            "{:?}",
            config.allowed_hosts
        );
    }

    #[test]
    fn the_mcp_door_keeps_loopback_only_without_a_public_host() {
        let config = service_config(None);
        assert_eq!(config.allowed_hosts, ["localhost", "127.0.0.1", "::1"]);
    }

    #[tokio::test]
    async fn the_mcp_service_answers_under_the_deployments_public_host() {
        use rmcp::transport::streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
        };

        let call = |config: StreamableHttpServerConfig| async {
            let core = crate::core::test_support::test_core().await;
            let service = StreamableHttpService::new(
                move || Ok(PkdbTools::over(core.clone())),
                std::sync::Arc::new(LocalSessionManager::default()),
                config,
            );
            service
                .oneshot(
                    Request::builder()
                        .uri("/mcp")
                        .method("POST")
                        .header("host", "engram.example")
                        .header("content-type", "application/json")
                        .header("accept", "application/json, text/event-stream")
                        .body(Body::from(
                            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
                        ))
                        .unwrap(),
                )
                .await
                .unwrap()
        };

        // What production answered before the fix: a loopback-only guard
        // rejects the public name before any tool can run.
        let rejected = call(StreamableHttpServerConfig::default()).await;
        assert_eq!(rejected.status(), StatusCode::FORBIDDEN);
        // With the deployment's host named, the same request gets through to
        // the protocol layer — which complains about the missing session, not
        // the host.
        let accepted = call(service_config(Some("engram.example".to_string()))).await;
        assert_ne!(accepted.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn the_ask_tool_is_offered_only_with_an_ask_model() {
        let core = crate::core::test_support::test_core().await;
        let tools = PkdbTools::over(core.clone());
        assert!(tools.routes().has_route("ask"));
        let mut core = core;
        core.completer = None;
        let tools = PkdbTools::over(core);
        assert!(!tools.routes().has_route("ask"));
        assert!(tools.routes().has_route("search"));
    }
}
