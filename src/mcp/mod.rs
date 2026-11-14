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

/// Markdown, since results go straight into an agent's context.
pub fn format_search_results(
    results: &[SearchResult],
    explanation: Option<&crate::core::explain::SearchExplanation>,
) -> String {
    if results.is_empty() {
        return "No matches in the knowledge base.".to_string();
    }
    // Only the pool can say how many corpora it drew from: one source dominating versus being all there was.
    let head = match explanation {
        Some(s) => format!(
            "_Pool: {} candidates, {} corpus{}{}{}._\n\n",
            s.candidates_fetched,
            s.corpora_in_pool,
            if s.corpora_in_pool == 1 { "" } else { "es" },
            match s.capped {
                Some(n) => format!(" · cap {n} per source"),
                None => " · uncapped".to_string(),
            },
            // Refilled counts the answer: an over-cap hit still in it had no replacement.
            match (s.displaced, s.refilled) {
                (0, _) => String::new(),
                (d, 0) => format!(" · {d} displaced"),
                (d, r) => format!(
                    " · {d} displaced, {r} still in the answer — \
                     the cap had nothing to redistribute to"
                ),
            },
        ),
        None => String::new(),
    };
    let mut rank = 0;
    let body = results
        .iter()
        .map(|r| {
            // Never "Untitled": the passage opening says what it is.
            let title = r
                .title
                .clone()
                .unwrap_or_else(|| crate::web::markdown::stand_in_title(&r.text, 60));
            // `stand_in_title` can leave nothing; the id is a poor name but a working one.
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
            // A bare ordinal reads as rank, and an associated hit did not compete, so it gets no number.
            let heading = if r.via.is_none() {
                rank += 1;
                format!("### {rank}. {title}")
            } else {
                format!("### {title}")
            };
            // An agent has no grey to see, so a hit past the cliff says so in words.
            let how = match (&r.via, &r.reason) {
                (Some(_), Some(why)) => format!("recalled beside the answer — {why}"),
                (Some(_), None) => "recalled beside the answer".to_string(),
                (None, _) if r.past_cliff => {
                    format!("score {:.3} · below the relevance cliff", r.score)
                }
                (None, _) => format!("score {:.3}", r.score),
            };
            // An agent reads the meta line and nothing else, so badges become words.
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
                Some(ArtifactStatus::Trashed) => facts.push("in trash".to_string()),
                Some(ArtifactStatus::Active) | None => {}
            }
            if r.primed {
                facts.push(
                    "lifted: opened, confirmed or cited more than what it passed".to_string(),
                );
            }
            if let Some(d) = &r.due_in {
                facts.push(format!("due {d}"));
            }
            if r.in_sitting {
                facts.push("lifted: open in this sitting".to_string());
            }
            let facts = facts.iter().map(|f| format!(" · {f}")).collect::<String>();
            let why = match (explanation.is_some(), r.explanation.as_ref()) {
                (true, Some(e)) => why_line(e),
                _ => String::new(),
            };
            // An agent reading one side of a contradiction as settled repeats it as fact.
            let disagrees = r
                .disagrees_with
                .iter()
                .map(|d| format!("\n{}", disagreement_line(d)))
                .collect::<String>();
            format!(
                "{heading}\n_{how}{facts}{tags} · corpus: {}_{why}{disagrees}\n\n{}",
                r.corpus_id, r.text
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n---\n\n");
    format!("{head}{body}")
}

/// Neither side is called right.
fn disagreement_line(d: &crate::store::pairs::Disagreement) -> String {
    format!(
        "Disagrees with \"{}\" ({}){}",
        d.other_title.as_deref().unwrap_or("another note"),
        crate::fmt::fmt_day(d.other_created_at),
        d.detail
            .as_deref()
            .map(|x| format!(": {x}"))
            .unwrap_or_default(),
    )
}

/// Clamped before the cast: a `u64` past `i64::MAX` goes negative, and a large one overflows the multiply.
pub(crate) fn due_horizon(now: i64, hours: u64) -> i64 {
    const MAX_HORIZON_HOURS: u64 = 100 * 365 * 24;
    now.saturating_add(hours.min(MAX_HORIZON_HOURS) as i64 * 3_600)
}

pub(crate) fn format_due(rows: &[crate::store::moments::DueRow], now: i64) -> String {
    if rows.is_empty() {
        return "_Nothing is due._".to_string();
    }
    let mut overdue = vec![];
    let mut due = vec![];
    let mut undated = vec![];
    for r in rows {
        let line = match r.moment.at {
            None => format!("- **{}** (id: {})", r.title, r.moment.id),
            Some(at) => format!(
                "- **{}** — {} (id: {})",
                r.title,
                crate::web::due::when_words(
                    at,
                    now,
                    crate::core::moments::zone(Some(&r.moment.tz))
                ),
                r.moment.id
            ),
        };
        match r.moment.at {
            None => undated.push(line),
            Some(at) if at < now => overdue.push(line),
            Some(_) => due.push(line),
        }
    }
    let mut out = String::new();
    for (head, lines) in [
        ("## Overdue", overdue),
        ("## Due", due),
        ("## Undated", undated),
    ] {
        if lines.is_empty() {
            continue;
        }
        out.push_str(head);
        out.push('\n');
        out.push_str(&lines.join("\n"));
        out.push_str("\n\n");
    }
    out.trim_end().to_string()
}

/// Named rather than counted: a number an agent cannot attribute to a stage is worse than none.
fn why_line(e: &crate::core::explain::HitExplanation) -> String {
    if let Some(via) = &e.recalled_via {
        return format!("\n_why it is here: recalled beside `{via}`; never ranked._");
    }
    // Absent rather than zero for a hit retrieval never returned: there is no "#1" for a rank that does not exist.
    let mut parts: Vec<String> = e
        .retrieved_rank
        .map(|r| format!("retrieved at #{}", r + 1))
        .into_iter()
        .collect();
    if let Some(v) = e.recency {
        parts.push(format!("recency +{v:.3}"));
    }
    if let Some(v) = e.pinned {
        parts.push(format!("pinned +{v:.3}"));
    }
    if let Some(s) = &e.rerank {
        parts.push(format!(
            "reranked {} → {}",
            s.from.unwrap_or(0) + 1,
            s.to.unwrap_or(0) + 1
        ));
    }
    // Kept is the ordinary case; reporting every no-op trains an agent to skip the line.
    if matches!(e.cap, crate::core::explain::CapEffect::Refilled) {
        parts.push("over its source's cap, kept anyway".to_string());
    }
    if let Some(s) = &e.prime {
        parts.push(format!(
            "primed {} → {}",
            s.from.unwrap_or(0) + 1,
            s.to.unwrap_or(0) + 1
        ));
    }
    if e.past_cliff {
        parts.push("below the cliff".to_string());
    }
    if parts.is_empty() {
        return String::new();
    }
    format!("\n_why it is here: {}._", parts.join(" · "))
}

const ORIGIN_MCP: &str = "mcp";

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

/// Paged so a 50 MB PDF does not fill an agent's context in one tool result.
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

/// The production variant deliberately holds no core. See `PkdbTools`.
#[derive(Clone)]
enum CoreSource {
    /// Single-base tests only. Boxed: larger and rarer.
    Fixed(Box<Core>),
    Tenant(std::sync::Arc<crate::tenants::Tenants>, String),
}

impl CoreSource {
    async fn core(&self) -> crate::error::Result<Core> {
        match self {
            CoreSource::Fixed(c) => Ok((**c).clone()),
            CoreSource::Tenant(tenants, subject) => Ok(tenants.get(subject).await?.core),
        }
    }

    /// The account setting alone: MCP sends no `Accept-Language`. A failing control database falls back to English.
    async fn lang(&self, core: &Core) -> crate::infer::lang::Lang {
        match self {
            CoreSource::Fixed(_) => Default::default(),
            CoreSource::Tenant(_, subject) => core
                .store
                .control
                .lang(subject)
                .await
                .ok()
                .flatten()
                .unwrap_or_default(),
        }
    }
}

#[derive(Clone)]
pub struct PkdbTools {
    source: CoreSource,
    /// Carried because `routes` is synchronous; `Core::asks` is instance-wide, the same for every tenant.
    asks: bool,
}

impl PkdbTools {
    pub fn over(core: Core) -> PkdbTools {
        PkdbTools {
            asks: core.asks(),
            source: CoreSource::Fixed(Box::new(core)),
        }
    }

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

/// Exactly one of `text`, `url` and `file_base64`. What an agent usually has is text or a link;
/// `file_base64` is for a client that holds the bytes — a PDF, an image, a text file — and is
/// bounded by the request body limit, so a book goes in by its link.
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
    /// The IANA time zone the text was written in (e.g. `Europe/Berlin`), so
    /// a date in it is read there.
    #[serde(default)]
    pub tz: Option<String>,
    /// `remind` to store `text` as a reminder: it is dated at capture and
    /// comes back from `due` when its time approaches.
    #[serde(default)]
    pub intent: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct DueParams {
    /// How far ahead counts as due. The server's `time.horizon_hours` (48)
    /// when absent.
    #[serde(default)]
    pub horizon_hours: Option<u64>,
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
    /// Ask why each hit is where it is: the ranking stages that moved it, and
    /// the shape of the pool it was drawn from. Off by default — the stages
    /// are noise to a caller that only wants the artifacts.
    #[serde(default)]
    pub explain: Option<bool>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct AskParams {
    /// A question to answer from the knowledge base.
    pub q: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct DecideOption {
    /// The option's name, as it will appear in the answer.
    pub name: String,
    /// What choosing it means. Optional.
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct DecideParams {
    /// The question to decide from the knowledge base.
    pub q: String,
    /// Two to eight options to choose between. Leave out, with `levels`, for yes or no.
    #[serde(default)]
    pub options: Option<Vec<DecideOption>>,
    /// Two to ten levels of a scale, lowest first.
    #[serde(default)]
    pub levels: Option<Vec<String>>,
    /// Facts about the present situation that the notes cannot know.
    #[serde(default)]
    pub situation: Option<String>,
}

#[tool_router]
impl PkdbTools {
    /// `ask` is absent rather than "not configured" when nothing can answer.
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
        // Checked before `capture_time` takes it: `intent` reaches only the text branch. See `refuse_time_fields`.
        if p.text.is_none()
            && let Err(e) = crate::web::api::refuse_time_fields(p.intent.as_deref())
        {
            return format!("Ingest failed: {e}");
        }
        let time = match crate::web::api::capture_time(p.tz, p.intent) {
            Ok(t) => t,
            Err(e) => return format!("Ingest failed: {e}"),
        };
        let lang = self.source.lang(&core).await;
        let outcome = if let Some(text) = p.text {
            core.ingest_capture(
                Capture::new(text, ORIGIN_MCP)
                    .with_lang(lang)
                    .with_title(p.title)
                    .with_note(p.note)
                    .with_tz(time.tz)
                    .with_intent(time.intent),
            )
            .await
        } else if let Some(raw) = p.url {
            match url::Url::parse(&raw) {
                Ok(u) => core.ingest_url(&u, p.title, p.note, lang).await,
                Err(e) => Err(Error::Validation(format!("url: {e}"))),
            }
        } else {
            let encoded = p.file_base64.unwrap_or_default();
            match base64::engine::general_purpose::STANDARD.decode(encoded.trim()) {
                Ok(bytes) => {
                    core.ingest_file(bytes, p.filename, p.title, p.note, ORIGIN_MCP, lang)
                        .await
                }
                Err(e) => Err(Error::Validation(format!("file_base64 is not base64: {e}"))),
            }
        };
        match outcome {
            Ok(o) if o.duplicate => format!("Already stored as `{}`.", o.id),
            // The flag is for a person who may want to keep only one of the two.
            Ok(o) if o.link_unread.is_some() => format!(
                "Stored as `{}`, but the page could not be read ({}). The link and any \
                 note are stored and searchable; the page is fetched again in the background.",
                o.id,
                o.link_unread.as_deref().unwrap_or("")
            ),
            Ok(o) if o.near_duplicate.is_some() => {
                let n = o.near_duplicate.as_ref();
                format!(
                    "Stored as `{}` and being indexed. It is {:.0}% similar to `{}`; \
                     the base merges or hides what the two repeat on its own, with an \
                     undo on Insights.",
                    o.id,
                    n.map_or(0.0, |n| n.similarity * 100.0),
                    n.map_or("", |n| n.corpus_id.as_str())
                )
            }
            // Stored is not yet searchable; the reply says which reading still stands between.
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
            // Perhaps an artifact, whose parent document is wanted; a merged artifact is its own text.
            Err(Error::NotFound) => match core.store.get_artifact(&p.id).await {
                Ok(a) => match &a.corpus_id {
                    Some(cid) => match core.store.get_corpus(cid).await {
                        Ok(c) => (corpus_head(&c), c.raw_text),
                        Err(e) => return format!("Read failed: {e}"),
                    },
                    None => (
                        format!(
                            "**{}** · merged artifact `{}`",
                            a.title.clone().unwrap_or_else(
                                || crate::web::markdown::stand_in_title(&a.text, 60)
                            ),
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
        name = "due",
        description = "What the operator has asked to be reminded of: overdue, due \
                       within the horizon (default 48h), and reminders still waiting \
                       for a date. Read-only; `done` and `snooze` are on the web UI \
                       and the REST API."
    )]
    async fn due(&self, Parameters(p): Parameters<DueParams>) -> String {
        let core = match self.source.core().await {
            Ok(c) => c,
            Err(e) => return format!("Due failed: {e}"),
        };
        let now = core.clock.now();
        let horizon = due_horizon(now, p.horizon_hours.unwrap_or(core.time.horizon_hours));
        match core.store.open_due(now, horizon).await {
            Ok(rows) => format_due(&rows, now),
            Err(e) => format!("Due failed: {e}"),
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
            explain: p.explain.unwrap_or(false),
        };
        let core = match self.source.core().await {
            Ok(c) => c,
            Err(e) => return format!("Search failed: {e}"),
        };
        // The cap `Core::search` applies, so an explanation cannot change what the door returns.
        let cap = core
            .ranking
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .per_source_cap;
        let explain = query.explain;
        match core
            .search_with(&query, cap, crate::store::feedback::Door::Mcp)
            .await
        {
            Ok((r, outcome)) => format_search_results(&r, explain.then_some(&outcome.explanation)),
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
    #[tool(
        name = "decide",
        description = "Decide a typed question from the notes that bear on it: yes or no, one of several options, or a level on a scale. Returns weights, how firmly the notes hold it, and each note's own reading. It answers what the notes support, not what is advisable."
    )]
    pub(crate) async fn decide(&self, Parameters(p): Parameters<DecideParams>) -> String {
        use crate::infer::decide::{Kind, Question};
        let kind = match (p.options, p.levels) {
            (Some(_), Some(_)) => return "Decide failed: give options or levels, not both".into(),
            (Some(options), None) => Some(Kind::Choice {
                options: options
                    .into_iter()
                    .map(|o| (o.name, o.description.unwrap_or_default()))
                    .collect(),
            }),
            (None, Some(levels)) => Some(Kind::Score { levels }),
            (None, None) => None,
        };
        let req = crate::core::decide::DecideRequest {
            q: p.q,
            // Empty instructions ask `q`.
            questions: kind
                .map(|kind| Question {
                    key: "verdict".into(),
                    kind,
                    instructions: String::new(),
                })
                .into_iter()
                .collect(),
            situation: p.situation,
            ..Default::default()
        };
        let core = match self.source.core().await {
            Ok(c) => c,
            Err(e) => return format!("Decide failed: {e}"),
        };
        match core.decide(&req, crate::store::feedback::Door::Mcp).await {
            Ok(d) => format_decision(&d),
            Err(e) => format!("Decide failed: {e}"),
        }
    }
}

#[rmcp::tool_handler(router = self.routes())]
impl rmcp::ServerHandler for PkdbTools {}

/// MCP has no page, so this string is all the caller knows. Ordered by consequence: truncation, then the
/// unsupported literals by name, since an agent is the caller most likely to run a fabricated command.
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
    // Both sides were cited; neither is picked.
    for d in &a.disagreements {
        let this = a
            .citations
            .iter()
            .find(|c| c.artifact_id == d.artifact_id)
            .and_then(|c| c.title.as_deref())
            .unwrap_or("a note");
        out.push_str(&format!(
            "\n\n_Your notes disagree: \"{this}\" and \"{}\" ({}){}._",
            d.other_title.as_deref().unwrap_or("another note"),
            crate::fmt::fmt_day(d.other_created_at),
            d.detail
                .as_deref()
                .map(|x| format!(": {x}"))
                .unwrap_or_default(),
        ));
    }
    if !a.citations.is_empty() {
        out.push_str("\n\n---\n\n**Sources**\n\n");
        out.push_str(&format_search_results(&a.citations, None));
    }
    if a.dropped > 0 {
        out.push_str(&format!(
            "\n\n_{} further excerpt(s) omitted for context budget._",
            a.dropped
        ));
    }
    out
}

/// A title an agent's markdown cannot break: one line, no table cell, emphasis or code.
fn md_title(e: &crate::core::decide::Evidence) -> String {
    let t = e
        .title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| crate::web::markdown::stand_in_title(&e.text, 60));
    md_line(&t)
}

/// One line an agent's markdown cannot break: no table cell, emphasis or code.
fn md_line(t: &str) -> String {
    t.replace(['\r', '\n'], " ")
        .chars()
        .fold(String::new(), |mut o, c| {
            if matches!(c, '|' | '*' | '_' | '`') {
                o.push('\\');
            }
            o.push(c);
            o
        })
}

/// The first question only: every door asks one, keyed `verdict`.
pub fn format_decision(d: &crate::core::decide::Decision) -> String {
    use crate::web::decide::{bars, pick_label, standing_copy};
    let mut out = format!("**{}**", md_line(&d.question));
    let (Some(q), Some((_, prior))) = (d.questions.first(), d.prior.first()) else {
        return out;
    };
    // Nothing bears: no verdict from the model's own prior either.
    let Some((_, joint)) = d.answers.as_ref().and_then(|a| a.first()) else {
        out.push_str("\n\nNothing here: the base does not hold this.");
        return out;
    };
    let without = bars(q, prior, prior)
        .iter()
        .map(|b| format!("{} {}", md_line(&b.label), b.weight))
        .collect::<Vec<_>>()
        .join(", ");
    out.push_str("\n\n");
    for b in bars(q, &joint.answer, prior) {
        out.push_str(&format!("- {}: {}\n", md_line(&b.label), b.weight));
    }
    let standing = d.standing.first().map_or("", |(_, s)| standing_copy(*s));
    out.push_str(&format!("\n{standing} Without notes: {without}."));
    if !d.evidence.is_empty() {
        let verdict = pick_label(q, &joint.answer);
        out.push_str("\n\n---\n\n**Evidence**\n");
        for e in &d.evidence {
            out.push_str(&format!("\n{}. {}", e.n, md_title(e)));
            if let Some((_, a)) = e.read.iter().find(|(k, _)| *k == q.key) {
                let own = pick_label(q, a);
                let against = if own == verdict { "" } else { " (against)" };
                out.push_str(&format!(" — {}{against}", md_line(&own)));
            }
        }
    }
    let rest = d.read.saturating_sub(d.evidence.len());
    if rest > 0 {
        let s = if rest == 1 { "" } else { "s" };
        out.push_str(&format!("\n\n_{rest} more passage{s} did not bear on it._"));
    }
    if d.dropped > 0 {
        out.push_str(&format!(
            "\n\n_{} bearing passage(s) did not fit the joint read._",
            d.dropped
        ));
    }
    out
}

/// Unauthenticated requests are rejected before any tool runs.
async fn mcp_guard(
    _id: crate::auth::Identity,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    next.run(req).await
}

/// rmcp's DNS-rebinding guard allows loopback only; behind a proxy the Host is `server.public_url`'s host.
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
    // Stateless: legacy session mode answers a bare `tools/list` probe (no `initialize`) with 422.
    StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_allowed_hosts(hosts)
}

/// One service per tenant, capped like the registry. Tools hold a subject, never a `Core`: a pinned core doubles the
/// open pools and blocks `Working::is_idle`. Eviction ends a client's MCP session; it reconnects as usual.
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

/// rmcp reaps idle sessions after five minutes and SSE pings never reach its timer; a person's pause runs longer.
const SESSION_KEEP_ALIVE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

fn session_manager() -> rmcp::transport::streamable_http_server::session::local::LocalSessionManager
{
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    let mut manager = LocalSessionManager::default();
    manager.session_config.keep_alive = Some(SESSION_KEEP_ALIVE);
    manager
}

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
    use rmcp::transport::streamable_http_server::StreamableHttpService;
    let subject = tenant.user.subject.clone();
    // Read off the handed core only because `routes` is synchronous; the same for every tenant.
    let asks = tenant.core.asks();
    let mut g = services
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
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
                std::sync::Arc::new(session_manager()),
                service_config(public_host),
            )
        })
        .clone();
    order.retain(|s| *s != subject);
    order.push(subject);
    // The caller's entry was just moved to the back, so it is never the one dropped.
    while map.len() > cap.max(1) {
        let Some(oldest) = order.first().cloned() else {
            break;
        };
        order.remove(0);
        map.remove(&oldest);
    }
    svc
}

/// RFC 9728: where an MCP client finds the authorization server. Only with OIDC.
async fn protected_resource(
    axum::extract::State(st): axum::extract::State<AppState>,
) -> crate::error::Result<axum::Json<serde_json::Value>> {
    let oidc = st.auth.oidc.as_ref().ok_or(crate::error::Error::NotFound)?;
    Ok(axum::Json(serde_json::json!({
        "resource": oidc.resource(),
        "authorization_servers": [oidc.issuer()],
    })))
}

async fn bearer_challenge(
    axum::extract::State(st): axum::extract::State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let mut res = next.run(req).await;
    if res.status() == axum::http::StatusCode::UNAUTHORIZED
        && st.auth.oidc.is_some()
        && let Ok(v) = axum::http::HeaderValue::from_str(&format!(
            "Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource\"",
            st.auth.deployment.base()
        ))
    {
        res.headers_mut()
            .insert(axum::http::header::WWW_AUTHENTICATE, v);
    }
    res
}

pub fn mcp_router(state: AppState) -> Router<AppState> {
    let public_host = state
        .auth
        .deployment
        .public_url
        .as_ref()
        .and_then(|u| u.host_str().map(str::to_string));
    let cap = state.config.store.max_open_tenants;
    let tenants = state.tenants.clone();
    let services: std::sync::Arc<TenantServices> = Default::default();

    let router = Router::new()
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
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            mcp_guard,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            bearer_challenge,
        ))
        .layer(crate::web::security::rate_limit(
            crate::web::API_PER_MINUTE,
            crate::web::API_BURST,
        ));
    if state.auth.oidc.is_some() {
        router.route(
            "/.well-known/oauth-protected-resource",
            axum::routing::get(protected_resource),
        )
    } else {
        router
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn the_service_map_holds_no_more_tenants_than_the_registry_does() {
        let (tenants, a, b, _dir) = crate::tenants::test_support::two_tenants().await;
        let services: TenantServices = Default::default();
        service_for(&services, &tenants, &a, None, 1);
        service_for(&services, &tenants, &b, None, 1);

        let g = services
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(g.0.len(), 1, "the map grew past the cap");
        assert!(
            g.0.contains_key(&b.user.subject),
            "the tenant that just asked was the one dropped"
        );
    }

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
            score: 0.5,
            ..Default::default()
        }
    }

    #[test]
    fn the_mcp_door_names_a_passage_by_its_opening() {
        let out = format_search_results(
            &[search_hit(
                None,
                "Die digitale Forensik unterscheidet sich zusätzlich",
            )],
            None,
        );
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
            retired_only: false,
            disagreements: vec![],
            event_id: None,
        }
    }

    #[test]
    fn the_mcp_answer_names_the_literals_the_base_does_not_hold() {
        let out = format_answer(&answer(&["wipefs --all /dev/sdX"]));
        assert!(out.contains("wipefs --all /dev/sdX"), "{out}");
        assert!(out.contains("no cited excerpt"), "{out}");
    }

    #[test]
    fn an_answer_drawn_from_its_excerpts_carries_no_warning() {
        let out = format_answer(&answer(&[]));
        assert!(!out.contains("no cited excerpt"), "{out}");
    }

    fn disagreement(on: &str, other: &str) -> crate::store::pairs::Disagreement {
        crate::store::pairs::Disagreement {
            artifact_id: on.into(),
            created_at: 1_772_280_000,
            other_id: other.into(),
            other_title: Some("Backup schedule (2025)".into()),
            // Midday UTC, so the day reads the same in any zone.
            other_created_at: 1_772_366_400,
            detail: Some("one says nightly, the other weekly".into()),
        }
    }

    #[test]
    fn a_hit_says_in_words_which_note_disagrees_with_it() {
        let mut h = search_hit(Some("Backup schedule"), "Backups run nightly.");
        h.disagrees_with = vec![disagreement("a", "b")];
        let out = format_search_results(&[h], None);
        let day = crate::fmt::fmt_day(1_772_366_400);
        assert!(
            out.contains(&format!(
                "Disagrees with \"Backup schedule (2025)\" ({day}): one says nightly, the other weekly"
            )),
            "{out}"
        );

        let mut h = search_hit(Some("Backup schedule"), "Backups run nightly.");
        let mut d = disagreement("a", "b");
        d.other_title = None;
        d.detail = None;
        h.disagrees_with = vec![d];
        let out = format_search_results(&[h], None);
        assert!(
            out.contains(&format!("Disagrees with \"another note\" ({day})\n")),
            "{out}"
        );

        let out = format_search_results(&[search_hit(Some("t"), "x")], None);
        assert!(!out.contains("Disagrees with"), "{out}");
    }

    #[test]
    fn an_answer_drawn_from_a_disagreement_says_both_notes_disagree() {
        let mut a = answer(&[]);
        a.citations = vec![search_hit(Some("Backup schedule"), "Backups run nightly.")];
        a.disagreements = vec![disagreement("a", "b")];
        let out = format_answer(&a);
        assert!(
            out.contains("Your notes disagree: \"Backup schedule\" and \"Backup schedule (2025)\""),
            "{out}"
        );
        assert!(out.contains("one says nightly, the other weekly"), "{out}");
        assert!(!format_answer(&answer(&[])).contains("disagree"));
    }

    #[test]
    fn the_mcp_door_is_stateless_so_a_handshakeless_probe_is_served() {
        assert!(!service_config(None).legacy_session_mode);
    }

    #[tokio::test]
    async fn the_protected_resource_document_points_at_the_issuer() {
        let core = crate::core::test_support::test_core().await;
        let mut state =
            crate::web::test_support::state_over(core, crate::config::AuthMode::Oidc).await;
        let base = state.auth.deployment.base();
        let (_idp, client) = crate::auth::oidc::fake_client(&base).await;
        let issuer = client.issuer().to_string();
        state.auth = std::sync::Arc::new(crate::web::state::AuthContext {
            mode: crate::config::AuthMode::Oidc,
            local: None,
            oidc: Some(client),
            pending: crate::auth::oidc::PendingStore::new(),
            deployment: state.auth.deployment.clone(),
        });
        let app = crate::web::router(state);
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/.well-known/oauth-protected-resource")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 1 << 16)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["resource"], format!("{base}/mcp"));
        assert_eq!(v["authorization_servers"][0], issuer);

        let unauth = app
            .oneshot(
                Request::builder()
                    .uri("/mcp")
                    .method("POST")
                    .header("authorization", "Bearer not.a.jwt")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);
        assert!(
            unauth.headers()["www-authenticate"]
                .to_str()
                .unwrap()
                .contains("/.well-known/oauth-protected-resource")
        );
    }

    #[tokio::test]
    async fn there_is_no_well_known_route_without_oidc() {
        let core = crate::core::test_support::test_core().await;
        let state =
            crate::web::test_support::state_over(core, crate::config::AuthMode::Local).await;
        let res = crate::web::router(state)
            .oneshot(
                Request::builder()
                    .uri("/.well-known/oauth-protected-resource")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 1 << 16)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("authorization_servers"));
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
                        explain: false,
                    },
                    crate::store::feedback::Door::Ui,
                )
                .await
                .unwrap(),
            None,
        );

        assert!(text.contains("mount /dev/sda1"), "{text}");
        assert!(text.contains("corpus:"), "{text}");
    }

    #[test]
    fn empty_results_produce_a_clear_message_not_an_empty_string() {
        let text = format_search_results(&[], None);
        assert!(!text.trim().is_empty());
        assert!(text.to_lowercase().contains("no match"));
    }

    fn hit(id: &str, via: Option<&str>) -> SearchResult {
        SearchResult {
            artifact_id: id.into(),
            corpus_id: "c".into(),
            title: Some(id.into()),
            text: "body".into(),
            score: 0.5,
            via: via.map(str::to_string),
            ..Default::default()
        }
    }

    fn explained(id: &str) -> SearchResult {
        let mut r = hit(id, None);
        r.explanation = Some(crate::core::explain::HitExplanation {
            retrieved_rank: Some(3),
            recency: Some(0.021),
            cap: crate::core::explain::CapEffect::Refilled,
            ..Default::default()
        });
        r
    }

    #[test]
    fn a_horizon_off_the_wire_is_clamped_before_it_is_cast() {
        let now = 1_000_000;
        assert_eq!(
            due_horizon(now, 48),
            now + 48 * 3_600,
            "the ordinary case is untouched"
        );
        assert_eq!(
            due_horizon(now, 0),
            now,
            "and so is asking for nothing ahead"
        );
        let century = 100 * 365 * 24 * 3_600;
        assert_eq!(due_horizon(now, u64::MAX), now + century);
        assert_eq!(due_horizon(now, i64::MAX as u64 / 3_600 + 1), now + century);
        assert!(due_horizon(now, u64::MAX) > now, "never behind the caller");
    }

    #[test]
    fn the_meta_line_stays_silent_unless_an_explanation_was_asked_for() {
        let out = format_search_results(&[explained("a1")], None);
        assert!(
            !out.contains("why it is here"),
            "an agent that did not ask for the stages must not be handed them"
        );
        assert!(
            out.contains("corpus: c"),
            "the existing meta line is untouched"
        );
    }

    #[test]
    fn an_explained_result_names_the_stage_that_failed_to_redistribute() {
        let summary = crate::core::explain::SearchExplanation {
            candidates_fetched: 30,
            corpora_in_pool: 1,
            capped: Some(3),
            displaced: 4,
            refilled: 4,
            reranked: false,
            keyword_only: false,
        };
        let out = format_search_results(&[explained("a1")], Some(&summary));

        assert!(
            out.contains("retrieved at #4"),
            "ranks are 1-based at every door: {out}"
        );
        assert!(
            out.contains("over its source's cap, kept anyway"),
            "the case the whole object exists for has to be readable rather \
             than inferred: {out}"
        );
        assert!(
            out.contains("1 corpus") && out.contains("30 candidates"),
            "the pool's shape belongs above the list, not on a hit: {out}"
        );
        assert!(
            out.contains("4 displaced, 4 still in the answer")
                && out.contains("nothing to redistribute to"),
            "over-cap hits still in the answer is the failure, and it has to \
             be named: {out}"
        );
    }

    #[test]
    fn a_cap_that_kept_its_hits_out_of_the_answer_names_no_failure() {
        let summary = crate::core::explain::SearchExplanation {
            candidates_fetched: 30,
            corpora_in_pool: 4,
            capped: Some(3),
            displaced: 4,
            refilled: 0,
            reranked: false,
            keyword_only: false,
        };
        let out = format_search_results(&[hit("a1", None)], Some(&summary));
        assert!(
            out.contains("4 displaced") && !out.contains("still in the answer"),
            "nothing over its cap survived the truncate, so there is no \
             failure to name: {out}"
        );
    }

    #[test]
    fn a_result_past_the_cliff_says_so_and_keeps_its_number() {
        let mut past = hit("tail", None);
        past.past_cliff = true;
        let out = format_search_results(&[hit("head", None), past], None);
        let head = out.find("### 1. head").unwrap();
        let tail = out.find("### 2. tail").unwrap();
        let note = out.find("below the relevance cliff").unwrap();
        assert!(head < tail && tail < note, "{out}");
        assert_eq!(out.matches("below the relevance cliff").count(), 1, "{out}");
    }

    #[test]
    fn an_associated_result_says_it_was_recalled_rather_than_ranked() {
        let out = format_search_results(
            &[hit("ranked", None), hit("recalled", Some("ranked"))],
            None,
        );
        assert!(out.contains("recalled beside"), "{out}");

        assert!(out.contains("### 1. ranked"), "{out}");
        assert!(out.contains("### recalled"), "{out}");
        assert!(!out.contains("### 2"), "{out}");
    }

    #[test]
    fn a_weak_result_says_so() {
        let mut w = hit("thin", None);
        w.weak = true;
        let out = format_search_results(&[w], None);
        assert!(out.contains("only loosely related"), "{out}");
        assert!(!format_search_results(&[hit("firm", None)], None).contains("loosely"));
    }

    #[test]
    fn model_written_text_is_marked_as_such() {
        let mut m = hit("merged", None);
        m.model_written = true;
        m.origin_count = 3;
        let out = format_search_results(&[m], None);
        assert!(out.contains("written by a model from 3 sources"), "{out}");

        let mut s = hit("pursued", None);
        s.model_written = true;
        s.synthesized = true;
        s.origin_count = 2;
        let out = format_search_results(&[s], None);
        assert!(
            out.contains("synthesized by a model from 2 sources"),
            "{out}"
        );
        assert!(!format_search_results(&[hit("captured", None)], None).contains("model"));
    }

    #[test]
    fn a_superseded_or_deprecated_result_names_its_status() {
        use crate::store::artifacts::ArtifactStatus;
        let mut old = hit("old", None);
        old.status = Some(ArtifactStatus::Superseded);
        old.superseded_by = Some("newer".into());
        let out = format_search_results(&[old], None);
        assert!(out.contains("superseded by `newer`"), "{out}");

        let mut dep = hit("dep", None);
        dep.status = Some(ArtifactStatus::Deprecated);
        assert!(format_search_results(&[dep], None).contains("deprecated"));
    }

    #[test]
    fn a_lifted_result_says_why_it_moved_up() {
        let mut p = hit("often", None);
        p.primed = true;
        assert!(
            format_search_results(&[p], None)
                .contains("lifted: opened, confirmed or cited more than what it passed")
        );
        let mut s = hit("open", None);
        s.in_sitting = true;
        assert!(format_search_results(&[s], None).contains("lifted: open in this sitting"));
    }

    #[test]
    fn a_result_with_a_reminder_due_says_when() {
        let mut d = hit("rent", None);
        d.due_in = Some("in 2 h".into());
        assert!(format_search_results(&[d], None).contains("due in 2 h"));
    }

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
            tz: None,
            intent: None,
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
        let config = service_config(Some("engram.example".to_string()));
        assert!(
            config.allowed_hosts.iter().any(|h| h == "engram.example"),
            "{:?}",
            config.allowed_hosts
        );
        assert!(
            config.allowed_hosts.iter().any(|h| h == "127.0.0.1"),
            "{:?}",
            config.allowed_hosts
        );
    }

    #[test]
    fn an_mcp_session_outlives_the_pause_between_tool_calls() {
        // rmcp's five-minute default expires while the person is still reading the last answer.
        let configured = session_manager()
            .session_config
            .keep_alive
            .expect("an idle session is still reclaimed eventually");
        assert!(
            configured
                > rmcp::transport::streamable_http_server::session::local::SessionConfig::DEFAULT_KEEP_ALIVE,
            "{configured:?} is no better than the default rmcp reaps at"
        );
        assert_eq!(configured, std::time::Duration::from_secs(60 * 60));
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

        let rejected = call(StreamableHttpServerConfig::default()).await;
        assert_eq!(rejected.status(), StatusCode::FORBIDDEN);
        // Past the host check, the protocol layer complains about the missing session instead.
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

    fn due_row(title: &str, at: Option<i64>) -> crate::store::moments::DueRow {
        crate::store::moments::DueRow {
            named: true,
            moment: crate::store::moments::Moment {
                id: format!("m-{title}"),
                artifact_id: "a".into(),
                kind: crate::store::moments::Kind::Due,
                at,
                until: None,
                tz: "UTC".into(),
                rule: None,
                source: crate::store::moments::Source::Set,
                span: None,
                done_at: None,
                snoozed_until: None,
                notified_at: None,
                moved_from: None,
                moved_at: None,
                series_id: None,
                origin_corpus_id: None,
                created_at: 0,
            },
            title: title.into(),
            opening: title.into(),
        }
    }

    #[test]
    fn the_due_tool_answers_in_three_sections_or_one_line() {
        let now = 1_000_000;
        let out = format_due(
            &[
                due_row("late", Some(now - 60)),
                due_row("soon", Some(now + 3_600)),
                due_row("someday", None),
            ],
            now,
        );
        let (o, d, u) = (
            out.find("## Overdue").unwrap(),
            out.find("## Due").unwrap(),
            out.find("## Undated").unwrap(),
        );
        assert!(o < d && d < u, "{out}");
        assert!(
            out.contains("**late**") && out.contains("id: m-late"),
            "{out}"
        );
        assert_eq!(format_due(&[], now), "_Nothing is due._");
    }

    fn keep_or_switch() -> crate::infer::decide::Question {
        crate::infer::decide::Question {
            key: "verdict".into(),
            kind: crate::infer::decide::Kind::Choice {
                options: vec![
                    ("keep".into(), String::new()),
                    ("switch to gemma".into(), String::new()),
                ],
            },
            instructions: "Should I keep the bge-m3 embedder?".into(),
        }
    }

    fn chose(pick: &str, p: [f64; 2]) -> crate::infer::decide::Answer {
        crate::infer::decide::Answer::Choice {
            pick: pick.into(),
            probabilities: p.to_vec(),
            confidence: p[0].max(p[1]),
        }
    }

    fn evidence_row(n: usize, title: Option<&str>, pick: &str) -> crate::core::decide::Evidence {
        let p = if pick == "keep" {
            [0.9, 0.1]
        } else {
            [0.2, 0.8]
        };
        crate::core::decide::Evidence {
            n,
            artifact_id: format!("A{n}"),
            title: title.map(str::to_string),
            text: format!("Passage {n} about the embedder."),
            bears: 0.9,
            joint: true,
            read: vec![("verdict".into(), chose(pick, p))],
        }
    }

    /// The spec's example: keep 81 / switch 19, prior 55 / 45, settled, two of four passages bearing.
    fn decision() -> crate::core::decide::Decision {
        crate::core::decide::Decision {
            question: "Should I keep the bge-m3 embedder?".into(),
            questions: vec![keep_or_switch()],
            answers: Some(vec![(
                "verdict".into(),
                crate::core::decide::Verdict {
                    answer: chose("keep", [0.81, 0.19]),
                    swapped_agree: Some(true),
                },
            )]),
            prior: vec![("verdict".into(), chose("keep", [0.55, 0.45]))],
            standing: vec![("verdict".into(), crate::core::decide::Standing::Settled)],
            evidence: vec![
                evidence_row(1, Some("Reindex notes 09/14"), "keep"),
                evidence_row(2, Some("GPU budget"), "switch to gemma"),
            ],
            read: 4,
            dropped: 0,
            model: "rule".into(),
            event_id: None,
            state: None,
            reads: vec![],
        }
    }

    fn not_held() -> crate::core::decide::Decision {
        let mut d = decision();
        d.answers = None;
        d.standing = vec![("verdict".into(), crate::core::decide::Standing::NotHeld)];
        d.evidence = vec![];
        d
    }

    #[test]
    fn the_mcp_decision_reads_as_weights_standing_and_evidence() {
        assert_eq!(
            format_decision(&decision()),
            "**Should I keep the bge-m3 embedder?**\n\
             \n\
             - keep: 81\n\
             - switch to gemma: 19\n\
             \n\
             Settled. Without notes: keep 55, switch to gemma 45.\n\
             \n\
             ---\n\
             \n\
             **Evidence**\n\
             \n\
             1. Reindex notes 09/14 — keep\n\
             2. GPU budget — switch to gemma (against)\n\
             \n\
             _2 more passages did not bear on it._"
        );
    }

    #[test]
    fn a_decision_the_base_does_not_hold_says_so_and_lists_nothing() {
        let out = format_decision(&not_held());
        assert!(
            out.starts_with("**Should I keep the bge-m3 embedder?**"),
            "{out}"
        );
        assert!(
            out.contains("Nothing here: the base does not hold this."),
            "{out}"
        );
        assert!(!out.contains("Without notes"), "{out}");
        assert_eq!(
            out,
            "**Should I keep the bge-m3 embedder?**\n\nNothing here: the base does not hold this."
        );
        assert!(!out.contains("**Evidence**"), "{out}");
        assert!(!out.contains("- keep:"), "{out}");
    }

    #[test]
    fn no_percent_sign_and_no_likelihood() {
        let mut dropped = decision();
        dropped.dropped = 1;
        for out in [
            format_decision(&decision()),
            format_decision(&not_held()),
            format_decision(&dropped),
        ] {
            assert!(!out.contains('%'), "{out}");
            assert!(!out.to_lowercase().contains("likely"), "{out}");
        }
        assert!(
            format_decision(&dropped)
                .contains("_1 bearing passage(s) did not fit the joint read._")
        );
    }

    #[test]
    fn an_awkward_title_stays_one_list_item() {
        let mut d = decision();
        d.evidence[0].title = Some("a|b *c*\nd".into());
        d.evidence[1].title = None;
        let out = format_decision(&d);
        let line = out.lines().find(|l| l.starts_with("1. ")).unwrap();
        assert_eq!(line, r"1. a\|b \*c\* d — keep", "{out}");
        assert!(
            out.contains("2. Passage 2 about the embedder. — switch to gemma"),
            "{out}"
        );
    }

    #[test]
    fn an_awkward_question_or_label_stays_on_its_line() {
        let awkward = "a|b *c*\nd";
        let mut d = decision();
        d.question = "Keep *it*\nor | not?".into();
        d.questions[0].kind = crate::infer::decide::Kind::Choice {
            options: vec![
                (awkward.into(), String::new()),
                ("_e_".into(), String::new()),
            ],
        };
        d.answers.as_mut().unwrap()[0].1.answer = chose(awkward, [0.81, 0.19]);
        d.prior = vec![("verdict".into(), chose(awkward, [0.55, 0.45]))];
        for e in &mut d.evidence {
            e.read = vec![("verdict".into(), chose(awkward, [0.9, 0.1]))];
        }
        let out = format_decision(&d);
        assert!(out.starts_with(r"**Keep \*it\* or \| not?**"), "{out}");
        assert!(out.contains(r"- a\|b \*c\* d: 81"), "{out}");
        assert!(out.contains(r"- \_e\_: 19"), "{out}");
        assert!(
            out.contains(r"Without notes: a\|b \*c\* d 55, \_e\_ 45."),
            "{out}"
        );
        assert!(
            out.contains(r"1. Reindex notes 09/14 — a\|b \*c\* d"),
            "{out}"
        );
    }

    #[test]
    fn yes_or_no_lists_yes_and_no() {
        use crate::infer::decide::{Answer, Question};
        let mut d = decision();
        d.questions = vec![Question::noul("verdict", "Keep it?")];
        d.answers = Some(vec![(
            "verdict".into(),
            crate::core::decide::Verdict {
                answer: Answer::Noul { yes: 0.7 },
                swapped_agree: None,
            },
        )]);
        d.prior = vec![("verdict".into(), Answer::Noul { yes: 0.4 })];
        for e in &mut d.evidence {
            e.read = vec![("verdict".into(), Answer::Noul { yes: 0.9 })];
        }
        let out = format_decision(&d);
        assert!(out.contains("- yes: 70\n- no: 30"), "{out}");
        assert!(out.contains("Without notes: yes 40, no 60."), "{out}");
        assert!(out.contains("1. Reindex notes 09/14 — yes\n"), "{out}");
    }

    #[tokio::test]
    async fn the_decide_tool_is_always_offered() {
        let core = crate::core::test_support::test_core().await;
        assert!(PkdbTools::over(core.clone()).routes().has_route("decide"));
        let mut core = core;
        core.completer = None;
        assert!(PkdbTools::over(core).routes().has_route("decide"));
    }

    fn params(q: &str, options: Option<&[&str]>, levels: Option<&[&str]>) -> DecideParams {
        DecideParams {
            q: q.into(),
            options: options.map(|o| {
                o.iter()
                    .map(|n| DecideOption {
                        name: n.to_string(),
                        description: None,
                    })
                    .collect()
            }),
            levels: levels.map(|l| l.iter().map(|s| s.to_string()).collect()),
            situation: None,
        }
    }

    #[tokio::test]
    async fn the_tool_decides_yes_or_no_by_default_and_from_options_when_given() {
        use crate::core::decide::tests::{deciding, marked, three_base};
        use crate::infer::decide::Kind;
        let mut core = three_base().await;
        let d = deciding(&mut core, marked(0.9));
        let tools = PkdbTools::over(core);

        let out = tools
            .decide(Parameters(params("Keep bge-m3?", None, None)))
            .await;
        assert!(out.contains("- yes:"), "{out}");

        let out = tools
            .decide(Parameters(params(
                "Which embedder?",
                Some(&["switch", "keep"]),
                None,
            )))
            .await;
        let (s, k) = (out.find("- switch:").unwrap(), out.find("- keep:").unwrap());
        assert!(s < k, "{out}");
        assert!(d.requests().iter().any(|(_, qs)| qs.iter().any(|q| matches!(
            &q.kind,
            Kind::Choice { options } if options.iter().map(|(n, _)| n.as_str()).eq(["switch", "keep"])
        ))));
    }

    #[tokio::test]
    async fn options_and_levels_together_are_refused_without_a_call() {
        use crate::core::decide::tests::{deciding, marked, three_base};
        let mut core = three_base().await;
        let d = deciding(&mut core, marked(0.9));
        let tools = PkdbTools::over(core);
        let out = tools
            .decide(Parameters(params(
                "Q?",
                Some(&["a", "b"]),
                Some(&["low", "high"]),
            )))
            .await;
        assert_eq!(out, "Decide failed: give options or levels, not both");
        assert_eq!(d.calls(), 0);
    }

    #[tokio::test]
    async fn one_option_is_refused_in_one_sentence() {
        use crate::core::decide::tests::{deciding, marked, three_base};
        let mut core = three_base().await;
        let d = deciding(&mut core, marked(0.9));
        let tools = PkdbTools::over(core);
        let out = tools
            .decide(Parameters(params("Q?", Some(&["only"]), None)))
            .await;
        assert!(out.starts_with("Decide failed:"), "{out}");
        assert!(!out.contains('\n'), "{out}");
        assert_eq!(d.calls(), 0);
    }

    #[tokio::test]
    async fn an_mcp_decision_is_not_recorded() {
        use crate::core::decide::tests::{deciding, marked, three_base};
        let mut core = three_base().await;
        core.learn.enabled = true;
        deciding(&mut core, marked(0.9));
        let pool = core.store.pool.clone();
        let tools = PkdbTools::over(core);
        let out = tools
            .decide(Parameters(params("Keep bge-m3?", None, None)))
            .await;
        assert!(!out.starts_with("Decide failed"), "{out}");
        let n: i64 = sqlx::query_scalar!("SELECT count(*) FROM decide_events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 0);
    }
}
