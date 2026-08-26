use crate::core::Core;
use crate::core::ingest::Capture;
use crate::core::search::{SearchQuery, SearchResult};
use crate::error::Error;
use crate::store::artifacts::ArtifactStatus;
use crate::store::corpora::CorpusStatus;
use crate::web::state::AppState;
use axum::Router;
use base64::Engine;
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
            // Everything the rail says with a badge or a shade of grey, said
            // here in words: an agent reads the meta line and nothing else.
            let mut facts = Vec::new();
            if r.weak {
                facts.push("only loosely related".to_string());
            }
            if r.model_written {
                let verb = if r.synthesized {
                    "synthesized"
                } else {
                    "written"
                };
                facts.push(format!("{verb} by a model from {} sources", r.origin_count));
            }
            match r.status {
                Some(ArtifactStatus::Superseded) => facts.push(match &r.superseded_by {
                    Some(by) => format!("superseded by `{by}`"),
                    None => "superseded".to_string(),
                }),
                Some(ArtifactStatus::Deprecated) => facts.push("deprecated".to_string()),
                Some(ArtifactStatus::Active) | None => {}
            }
            if r.primed {
                facts.push("lifted: reached often".to_string());
            }
            if r.in_sitting {
                facts.push("lifted: open in this sitting".to_string());
            }
            let facts = facts.iter().map(|f| format!(" · {f}")).collect::<String>();
            format!(
                "{heading}\n_{how}{facts}{tags} · corpus: {}_\n\n{}",
                r.corpus_id, r.text
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n---\n\n")
}

/// The channel a tool call arrives through.
const ORIGIN_MCP: &str = "mcp";

/// Store a file an agent handed over as bytes, by what the bytes are: a PDF
/// by its header, an image by its own, and otherwise UTF-8 text with its
/// file facts, as the upload door records them. Anything else is refused by
/// name: a corpus is quoted back verbatim, and bytes stored as mojibake would
/// be a fidelity loss nothing downstream could detect.
async fn ingest_file(
    core: &Core,
    bytes: Vec<u8>,
    filename: Option<String>,
    title: Option<String>,
    note: Option<String>,
) -> crate::error::Result<crate::core::ingest::IngestOutcome> {
    if bytes.starts_with(b"%PDF-") {
        return core
            .ingest_pdf(crate::core::ingest::PdfCapture {
                bytes,
                filename,
                title_hint: title,
                note,
            })
            .await;
    }
    if image::guess_format(&bytes).is_ok() {
        return core
            .ingest_image(crate::core::ingest::ImageCapture {
                bytes,
                filename,
                title_hint: title,
                note,
            })
            .await;
    }
    let size = bytes.len();
    let text = String::from_utf8(bytes).map_err(|_| {
        Error::Validation(
            "that file is neither a PDF, an image nor UTF-8 text — nothing here reads it".into(),
        )
    })?;
    core.ingest_capture(
        Capture::new(text, ORIGIN_MCP)
            .with_title(title)
            .with_note(note)
            .with_file(filename.as_deref(), size, "text/plain"),
    )
    .await
}

/// One line above a document: what it is called and where it came from.
fn corpus_head(c: &crate::store::corpora::Corpus) -> String {
    let title = c
        .title_hint
        .clone()
        .unwrap_or_else(|| crate::web::markdown::stand_in_title(&c.raw_text, 60));
    let mut head = format!("**{title}** · corpus `{}` · via {}", c.id, c.origin);
    if let Some(u) = &c.source_url {
        head.push_str(&format!(" · {u}"));
    }
    head
}

const READ_PAGE_CHARS: usize = 20_000;

/// A page of a document, in characters, ending with where the next page
/// begins when there is one. A 50 MB PDF read whole would be one tool result
/// that fills an agent's context; a page is something it can keep asking for.
fn page_of(text: &str, offset: usize, max_chars: Option<usize>) -> String {
    let max = max_chars.unwrap_or(READ_PAGE_CHARS).max(1);
    let total = text.chars().count();
    if offset >= total {
        return format!("(the document ends at offset {total})");
    }
    let page: String = text.chars().skip(offset).take(max).collect();
    let end = offset + page.chars().count();
    if end < total {
        format!("{page}\n\n[… continues: read again with offset={end} ({total} characters in all)]")
    } else {
        page
    }
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

/// Exactly one of `text`, `url` and `file_base64`. What an agent usually has
/// is text or a link; `file_base64` is for a client that actually holds the
/// bytes — a PDF, an image, a text file — and is bounded by the request body
/// limit, so a book goes in by its link.
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct IngestParams {
    /// The text to store verbatim.
    #[serde(default)]
    pub text: Option<String>,
    /// A link to read instead: a page is extracted to markdown, a PDF or an
    /// image is stored and read in the background. http or https only.
    #[serde(default)]
    pub url: Option<String>,
    /// A file's bytes, base64-encoded: a PDF, an image (PNG, JPEG, WebP) or
    /// a UTF-8 text file. Known by its bytes, not its name.
    #[serde(default)]
    pub file_base64: Option<String>,
    /// The file's name, with `file_base64`. Kept as a fact about the capture.
    #[serde(default)]
    pub filename: Option<String>,
    /// Optional short label for the corpus.
    #[serde(default)]
    pub title: Option<String>,
    /// A sentence of context — where this came from, why it is kept. Stored
    /// beside the text, never inside it.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ReadParams {
    /// A corpus id or an artifact id, as search prints them. An artifact id
    /// reads the document that artifact was cut from.
    pub id: String,
    /// Character offset to read from; the previous page says where it ended.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Characters per page. Default 20 000.
    #[serde(default)]
    pub max_chars: Option<usize>,
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
        description = "Store something in the personal knowledge base: verbatim text, \
                       a link (a page, a PDF or an image — read by this server), or a \
                       file as base64 (PDF, image or UTF-8 text). Exactly one of `text`, \
                       `url`, `file_base64`. Returns immediately; extraction, \
                       segmentation and embedding happen in the background."
    )]
    async fn ingest(&self, Parameters(p): Parameters<IngestParams>) -> String {
        let core = match self.source.core().await {
            Ok(c) => c,
            Err(e) => return format!("Ingest failed: {e}"),
        };
        let supplied = [p.text.is_some(), p.url.is_some(), p.file_base64.is_some()]
            .iter()
            .filter(|x| **x)
            .count();
        if supplied != 1 {
            return "Ingest failed: supply exactly one of `text`, `url` or `file_base64`."
                .to_string();
        }
        let outcome = if let Some(text) = p.text {
            core.ingest_capture(
                Capture::new(text, ORIGIN_MCP)
                    .with_title(p.title)
                    .with_note(p.note),
            )
            .await
        } else if let Some(raw) = p.url {
            match url::Url::parse(&raw) {
                Ok(u) => core.ingest_url(&u, p.title, p.note).await,
                Err(e) => Err(Error::Validation(format!("url: {e}"))),
            }
        } else {
            let encoded = p.file_base64.expect("the one-of check");
            match base64::engine::general_purpose::STANDARD.decode(encoded.trim()) {
                Ok(bytes) => ingest_file(&core, bytes, p.filename, p.title, p.note).await,
                Err(e) => Err(Error::Validation(format!("file_base64 is not base64: {e}"))),
            }
        };
        match outcome {
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
            // What was stored is not yet what search finds; the reply says
            // which reading still stands between the two.
            Ok(o) => match o.status {
                CorpusStatus::Extracting => format!(
                    "Stored as `{}`. Text extraction, then segmentation and embedding, \
                     run in the background.",
                    o.id
                ),
                CorpusStatus::Describing => format!(
                    "Stored as `{}`. The image is read by a vision model, then segmented \
                     and embedded, in the background.",
                    o.id
                ),
                _ => format!(
                    "Stored as `{}`. Segmentation and embedding run in the background.",
                    o.id
                ),
            },
            Err(e) => format!("Ingest failed: {e}"),
        }
    }

    #[tool(
        name = "read",
        description = "Read a stored document verbatim, by the corpus id or artifact id \
                       search printed. The answer is often the paragraph after the one \
                       that matched. Long documents arrive in pages; each says the \
                       offset the next one starts at."
    )]
    async fn read(&self, Parameters(p): Parameters<ReadParams>) -> String {
        let core = match self.source.core().await {
            Ok(c) => c,
            Err(e) => return format!("Read failed: {e}"),
        };
        let (head, body) = match core.store.get_corpus(&p.id).await {
            Ok(c) => (corpus_head(&c), c.raw_text),
            // Not a corpus: perhaps an artifact, whose parent is the document
            // wanted. A merged artifact has no parent, and is its own text.
            Err(Error::NotFound) => match core.store.get_artifact(&p.id).await {
                Ok(a) => match &a.corpus_id {
                    Some(cid) => match core.store.get_corpus(cid).await {
                        Ok(c) => (corpus_head(&c), c.raw_text),
                        Err(e) => return format!("Read failed: {e}"),
                    },
                    None => (
                        format!(
                            "**{}** · merged artifact `{}`",
                            a.title.as_deref().unwrap_or("(untitled)"),
                            a.id
                        ),
                        a.text,
                    ),
                },
                Err(Error::NotFound) => {
                    return format!("Nothing stored under `{}`.", p.id);
                }
                Err(e) => return format!("Read failed: {e}"),
            },
            Err(e) => return format!("Read failed: {e}"),
        };
        format!(
            "{head}\n\n{}",
            page_of(&body, p.offset.unwrap_or(0), p.max_chars)
        )
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
        let state =
            crate::web::test_support::state_over(core, crate::config::AuthMode::Local).await;
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

    /// The rail greys a weak hit; an agent has no grey to see, so the meta
    /// line says it in words.
    #[test]
    fn a_weak_result_says_so() {
        let mut w = hit("thin", None);
        w.weak = true;
        let out = format_search_results(&[w]);
        assert!(out.contains("only loosely related"), "{out}");
        assert!(!format_search_results(&[hit("firm", None)]).contains("loosely"));
    }

    /// Text a model wrote is never silently indistinguishable from captured
    /// text — that is the field's own contract, and it held on every door but
    /// this one.
    #[test]
    fn model_written_text_is_marked_as_such() {
        let mut m = hit("merged", None);
        m.model_written = true;
        m.origin_count = 3;
        let out = format_search_results(&[m]);
        assert!(out.contains("written by a model from 3 sources"), "{out}");

        let mut s = hit("pursued", None);
        s.model_written = true;
        s.synthesized = true;
        s.origin_count = 2;
        let out = format_search_results(&[s]);
        assert!(
            out.contains("synthesized by a model from 2 sources"),
            "{out}"
        );
        assert!(!format_search_results(&[hit("captured", None)]).contains("model"));
    }

    #[test]
    fn a_superseded_or_deprecated_result_names_its_status() {
        use crate::store::artifacts::ArtifactStatus;
        let mut old = hit("old", None);
        old.status = Some(ArtifactStatus::Superseded);
        old.superseded_by = Some("newer".into());
        let out = format_search_results(&[old]);
        assert!(out.contains("superseded by `newer`"), "{out}");

        let mut dep = hit("dep", None);
        dep.status = Some(ArtifactStatus::Deprecated);
        assert!(format_search_results(&[dep]).contains("deprecated"));
    }

    /// Two different reasons to be higher up the list, and neither is
    /// relevance; an agent trusting the order should be told.
    #[test]
    fn a_lifted_result_says_why_it_moved_up() {
        let mut p = hit("often", None);
        p.primed = true;
        assert!(format_search_results(&[p]).contains("lifted: reached often"));
        let mut s = hit("open", None);
        s.in_sitting = true;
        assert!(format_search_results(&[s]).contains("lifted: open in this sitting"));
    }

    /// Search is a dead end without this: it hands back one passage and the
    /// corpus id, and the answer is often the paragraph after the one that
    /// matched. `read` hands back the document, verbatim.
    #[tokio::test]
    async fn read_returns_the_whole_document_by_corpus_id() {
        let core = crate::core::test_support::test_core().await;
        let doc = "# Mounting\n\nRun mount.\n\n# Unmounting\n\nRun umount.";
        let out = core.ingest(doc, "mcp", Some("disks")).await.unwrap();
        let tools = PkdbTools::over(core);
        let text = tools
            .read(Parameters(ReadParams {
                id: out.id.clone(),
                offset: None,
                max_chars: None,
            }))
            .await;
        assert!(text.contains(doc), "{text}");
        assert!(text.contains("disks"), "{text}");
    }

    #[tokio::test]
    async fn read_reaches_the_document_from_one_of_its_artifacts() {
        let core = crate::core::test_support::test_core().await;
        let doc = "## Mounting\nRun `mount /dev/sda1 /mnt`.\n\n## After\nThen check dmesg.";
        let out = core.ingest(doc, "mcp", None).await.unwrap();
        while crate::jobs::run_one(&core).await.unwrap() {}
        let first = core.store.artifacts_for_corpus(&out.id).await.unwrap()[0]
            .id
            .clone();
        let tools = PkdbTools::over(core);
        let text = tools
            .read(Parameters(ReadParams {
                id: first,
                offset: None,
                max_chars: None,
            }))
            .await;
        assert!(text.contains("Then check dmesg"), "{text}");
    }

    #[tokio::test]
    async fn read_of_an_unknown_id_says_so() {
        let tools = PkdbTools::over(crate::core::test_support::test_core().await);
        let text = tools
            .read(Parameters(ReadParams {
                id: "nope".into(),
                offset: None,
                max_chars: None,
            }))
            .await;
        assert!(text.contains("Nothing stored under `nope`"), "{text}");
    }

    /// A 50 MB PDF read whole would fill an agent's context with one call.
    /// The document arrives in pages, each saying where the next begins.
    #[tokio::test]
    async fn read_pages_a_long_document_and_says_where_it_goes_on() {
        let core = crate::core::test_support::test_core().await;
        let doc = "abcdefghij".repeat(10);
        let out = core.ingest(&doc, "mcp", None).await.unwrap();
        let tools = PkdbTools::over(core);
        let page = tools
            .read(Parameters(ReadParams {
                id: out.id.clone(),
                offset: None,
                max_chars: Some(30),
            }))
            .await;
        // Below the head line, which quotes the opening as a stand-in title.
        let body = page.split_once("\n\n").unwrap().1;
        assert!(body.contains(&"abcdefghij".repeat(3)), "{page}");
        assert!(!body.contains(&"abcdefghij".repeat(4)), "{page}");
        assert!(body.contains("offset=30"), "{page}");

        let last = tools
            .read(Parameters(ReadParams {
                id: out.id,
                offset: Some(90),
                max_chars: Some(30),
            }))
            .await;
        assert!(last.ends_with("abcdefghij"), "{last}");
        assert!(!last.contains("offset="), "{last}");
    }

    fn ingest_params() -> IngestParams {
        IngestParams {
            text: None,
            url: None,
            file_base64: None,
            filename: None,
            title: None,
            note: None,
        }
    }

    fn a_pdf_fixture() -> Vec<u8> {
        include_bytes!("../../tests/fixtures/one-heading.pdf").to_vec()
    }

    #[tokio::test]
    async fn ingest_keeps_the_note_an_agent_attaches() {
        let core = crate::core::test_support::test_core().await;
        let tools = PkdbTools::over(core.clone());
        let reply = tools
            .ingest(Parameters(IngestParams {
                text: Some("Run mount.".into()),
                note: Some("from the shell session".into()),
                ..ingest_params()
            }))
            .await;
        let id = reply.split('`').nth(1).expect("an id in backticks");
        let c = core.store.get_corpus(id).await.unwrap();
        assert_eq!(c.metadata["note"], "from the shell session");
    }

    #[tokio::test]
    async fn ingest_takes_exactly_one_source() {
        let tools = PkdbTools::over(crate::core::test_support::test_core().await);
        let both = tools
            .ingest(Parameters(IngestParams {
                text: Some("a".into()),
                url: Some("https://example.test/".into()),
                ..ingest_params()
            }))
            .await;
        assert!(both.contains("exactly one of"), "{both}");
        let none = tools.ingest(Parameters(ingest_params())).await;
        assert!(none.contains("exactly one of"), "{none}");
    }

    /// An agent that holds bytes hands them over as base64; a PDF is known
    /// by its bytes, and stored for extraction under the name it came with.
    #[tokio::test]
    async fn ingest_takes_a_pdf_as_base64_and_says_extraction_is_queued() {
        use base64::Engine;
        let core = crate::core::test_support::test_core().await;
        let tools = PkdbTools::over(core.clone());
        let reply = tools
            .ingest(Parameters(IngestParams {
                file_base64: Some(
                    base64::engine::general_purpose::STANDARD.encode(a_pdf_fixture()),
                ),
                filename: Some("plan.pdf".into()),
                ..ingest_params()
            }))
            .await;
        assert!(reply.contains("extraction"), "{reply}");
        let id = reply.split('`').nth(1).expect("an id in backticks");
        let c = core.store.get_corpus(id).await.unwrap();
        assert_eq!(c.origin, crate::core::ingest::ORIGIN_PDF);
        assert_eq!(c.metadata["file"]["name"], "plan.pdf");
    }

    /// Plain text as a file is a text capture that remembers its file facts,
    /// as the upload door records them; bytes that are neither a document
    /// nor text are refused by name rather than stored as mojibake.
    #[tokio::test]
    async fn ingest_takes_a_text_file_and_refuses_bytes_it_cannot_read() {
        use base64::Engine;
        let core = crate::core::test_support::test_core().await;
        let tools = PkdbTools::over(core.clone());
        let reply = tools
            .ingest(Parameters(IngestParams {
                file_base64: Some(
                    base64::engine::general_purpose::STANDARD.encode("# Notes\n\nRun mount."),
                ),
                filename: Some("notes.md".into()),
                ..ingest_params()
            }))
            .await;
        let id = reply.split('`').nth(1).expect("an id in backticks");
        let c = core.store.get_corpus(id).await.unwrap();
        assert_eq!(c.raw_text, "# Notes\n\nRun mount.");
        assert_eq!(c.metadata["file"]["name"], "notes.md");

        let refused = tools
            .ingest(Parameters(IngestParams {
                file_base64: Some(
                    base64::engine::general_purpose::STANDARD.encode([0u8, 159, 146, 150]),
                ),
                ..ingest_params()
            }))
            .await;
        assert!(refused.contains("Ingest failed"), "{refused}");
        assert!(refused.contains("PDF"), "{refused}");
        let garbage = tools
            .ingest(Parameters(IngestParams {
                file_base64: Some("not base64!".into()),
                ..ingest_params()
            }))
            .await;
        assert!(garbage.contains("base64"), "{garbage}");
    }

    #[tokio::test]
    async fn ingest_reads_a_url_and_keeps_it_as_provenance() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/plan.pdf"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(a_pdf_fixture(), "application/pdf"),
            )
            .mount(&server)
            .await;
        let core = crate::core::test_support::test_core().await;
        let tools = PkdbTools::over(core.clone());
        let url = format!("{}/plan.pdf", server.uri());
        let reply = tools
            .ingest(Parameters(IngestParams {
                url: Some(url.clone()),
                ..ingest_params()
            }))
            .await;
        assert!(reply.contains("extraction"), "{reply}");
        let id = reply.split('`').nth(1).expect("an id in backticks");
        let c = core.store.get_corpus(id).await.unwrap();
        assert_eq!(c.source_url.as_deref(), Some(url.as_str()));
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
