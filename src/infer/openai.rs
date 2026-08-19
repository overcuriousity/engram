use super::{
    Completer, Completion, Delta, Describer, Embedder, ProposedArtifact, Reranker, SegmentInput,
    SynthesisBudget, Synthesizer, prompt,
};
use crate::config::{
    AskRole, CeilingParam, EmbedRole, RerankRole, RerankStyle, SynthesizeRole, TierConfig,
};
use crate::error::{Error, Result};
use async_trait::async_trait;
use serde_json::json;

/// One client per role, so a slow model only widens its own patience.
///
/// A timeout here looks exactly like a dead endpoint to the job runner: the
/// call fails, the job retries, and it fails again at the same wall. A local
/// reasoning model can spend well over three minutes on one segmentation
/// window, so the ceiling is configurable per role.
fn client(timeout_secs: u64) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build()
        .expect("http client")
}

fn url(base: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

/// A 4xx that says the request itself is wrong. 408 and 429 are 4xx by number
/// but "come back later" by meaning, and stay retryable.
///
/// So do 401, 403 and 404. Those three describe the endpoint rather than the
/// request: a key expires, a token is rotated, a proxy answers 404 for as long
/// as the backend behind it is restarting — and in every one of them the same
/// request succeeds once the wall is fixed. Calling them permanent meant one
/// expired key was enough to mark chunks `embed_failed` and settle their
/// corpora to `partial`, which coming back up does not undo.
pub fn permanent_upstream_status(status: reqwest::StatusCode) -> bool {
    use reqwest::StatusCode as S;
    status.is_client_error()
        && !matches!(
            status,
            S::REQUEST_TIMEOUT
                | S::TOO_MANY_REQUESTS
                | S::UNAUTHORIZED
                | S::FORBIDDEN
                | S::NOT_FOUND
        )
}

/// One configured endpoint: where to post, as whom, and which role a failure
/// is reported under.
pub(crate) struct Endpoint {
    client: reqwest::Client,
    base_url: String,
    model: String,
    api_key: Option<String>,
    role: &'static str,
    /// Which name this endpoint takes the output ceiling under, corrected in
    /// place the first time the endpoint rejects the name we guessed.
    ///
    /// Shared mutable state on a struct that is otherwise plain configuration,
    /// because the correction is worth nothing if every later call re-learns it.
    /// Shared *between* endpoints too — see [`LEARNED_CEILING`] — so that is one
    /// 400 per server per process, and not one per role pointed at it.
    ceiling_param: std::sync::Arc<AtomicCeilingParam>,
}

/// `CeilingParam` behind an atomic, so a `&self` call can record what it
/// learned.
///
/// `explicit` records whether the name currently held was named by the operator
/// or merely inferred, so a role that was told the name outranks one that
/// guessed it — see [`learned_ceiling`].
struct AtomicCeilingParam {
    param: std::sync::atomic::AtomicBool,
    explicit: std::sync::atomic::AtomicBool,
}

impl AtomicCeilingParam {
    fn new(p: CeilingParam) -> Self {
        Self::seeded(p, false)
    }

    fn seeded(p: CeilingParam, explicit: bool) -> Self {
        Self {
            param: std::sync::atomic::AtomicBool::new(p == CeilingParam::MaxCompletionTokens),
            explicit: std::sync::atomic::AtomicBool::new(explicit),
        }
    }

    fn is_explicit(&self) -> bool {
        self.explicit.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn get(&self) -> CeilingParam {
        if self.param.load(std::sync::atomic::Ordering::Relaxed) {
            CeilingParam::MaxCompletionTokens
        } else {
            CeilingParam::MaxTokens
        }
    }

    fn set(&self, p: CeilingParam) {
        self.param.store(
            p == CeilingParam::MaxCompletionTokens,
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// Set a name that is not a guess — one the operator gave, or one a 400
    /// taught — so that a role which only inferred a name cannot unseat it.
    fn set_authoritative(&self, p: CeilingParam) {
        self.set(p);
        self.explicit
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// A completion, and how it ended.
///
/// `truncated` is `finish_reason == "length"`: the model did not stop, the
/// ceiling stopped it. Carried out of the transport rather than only logged
/// because the caller is the only one that knows what a cut-off reply costs —
/// synthesis salvages what parsed, ask has to say so on the answer.
pub(crate) struct ChatReply {
    text: String,
    truncated: bool,
}

impl Endpoint {
    fn new(
        base_url: &str,
        model: &str,
        api_key: Option<&str>,
        timeout_secs: u64,
        role: &'static str,
    ) -> Self {
        Self {
            client: client(timeout_secs),
            base_url: base_url.to_string(),
            model: model.to_string(),
            api_key: api_key.map(str::to_string),
            role,
            // Unshared until a role that actually sends a ceiling claims the
            // server's cell below. `embed` and `rerank` send none, and have no
            // business seeding a name for a model they are not the one calling.
            ceiling_param: std::sync::Arc::new(AtomicCeilingParam::new(CeilingParam::MaxTokens)),
        }
    }

    /// The name to send the output ceiling under, configured or inferred, in the
    /// cell this server's other roles read and write.
    ///
    /// The cell is shared per server, so what this role resolves must not simply
    /// overwrite what another role resolved for the same server: the roles are
    /// constructed in a fixed order and the last one would otherwise win. An
    /// operator who sets `ask.ceiling_param` explicitly while `synthesize` — the
    /// same URL and model in the shipped example — only sets `reasoning_effort`
    /// would have their explicit setting replaced by synthesize's guess, and
    /// against an endpoint that ignores unknown fields that leaves `ask` with no
    /// ceiling at all, which is the failure this is all here to prevent.
    ///
    /// So: a name the operator gave outranks one a role inferred, and between
    /// two of equal standing the first wins and the disagreement is reported.
    /// See [`learned_ceiling`].
    fn with_ceiling_param(
        mut self,
        configured: Option<CeilingParam>,
        effort: Option<&str>,
    ) -> Self {
        let resolved = configured.unwrap_or_else(|| CeilingParam::inferred_from(effort));
        self.ceiling_param = learned_ceiling(
            &self.base_url,
            &self.model,
            resolved,
            configured.is_some(),
            self.role,
        );
        self
    }

    /// Post and hand back the response with its status already judged, so a
    /// caller that wants the body as a stream is not forced to buffer it into
    /// JSON first.
    async fn post(&self, path: &str, body: serde_json::Value) -> Result<reqwest::Response> {
        let role = self.role;
        let mut req = self.client.post(url(&self.base_url, path)).json(&body);
        if let Some(k) = &self.api_key {
            req = req.bearer_auth(k);
        }
        let started = std::time::Instant::now();
        let res = req.send().await.map_err(|e| Error::Inference {
            role,
            detail: e.to_string(),
        })?;
        let status = res.status();
        tracing::debug!(role, %status, ms = started.elapsed().as_millis(), "inference call");

        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            // Truncate: an upstream error page can be megabytes, and this string
            // ends up in a job's last_error column.
            let detail: String = body.chars().take(400).collect();
            let detail = format!("HTTP {status}: {detail}");
            return Err(if permanent_upstream_status(status) {
                Error::InferenceRejected { role, detail }
            } else {
                Error::Inference { role, detail }
            });
        }
        Ok(res)
    }

    async fn post_json(&self, path: &str, body: serde_json::Value) -> Result<serde_json::Value> {
        let role = self.role;
        self.post(path, body)
            .await?
            .json()
            .await
            .map_err(|e| Error::Inference {
                role,
                detail: e.to_string(),
            })
    }

    /// One chat completion; `body` carries everything but `model` and the
    /// output ceiling. Logs the cost of every call — on local hardware a window
    /// takes minutes, and the log is what tells a long wait from a hang.
    ///
    /// `ceiling` is applied here rather than by the caller because the name it
    /// goes out under is a property of the endpoint, and one that has to be
    /// learned: a request rejected for naming the wrong one is retried under the
    /// other, once, and the answer is remembered for every later call. Only the
    /// name is retried — a 400 about anything else stays a 400.
    async fn send_with_ceiling(
        &self,
        mut body: serde_json::Value,
        ceiling: Option<usize>,
    ) -> Result<reqwest::Response> {
        body["model"] = json!(self.model);
        let mut sent = self.ceiling_param.get();
        let mut may_retry = ceiling.is_some();
        loop {
            if let Some(max) = ceiling {
                if let Some(o) = body.as_object_mut() {
                    o.remove(sent.flipped().as_str());
                }
                body[sent.as_str()] = json!(max);
            }
            match self.post("chat/completions", body.clone()).await {
                Ok(v) => return Ok(v),
                Err(e) if may_retry && rejects_ceiling_name(error_detail(&e), sent) => {
                    may_retry = false;
                    sent = sent.flipped();
                    tracing::warn!(
                        role = self.role,
                        rejected = sent.flipped().as_str(),
                        retrying_as = sent.as_str(),
                        "the endpoint refused the output ceiling's name; using the other one \
                         from now on. Set ceiling_param to skip this."
                    );
                    self.ceiling_param.set_authoritative(sent);
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn chat(&self, body: serde_json::Value, ceiling: Option<usize>) -> Result<ChatReply> {
        let started = std::time::Instant::now();
        let res = self.send_with_ceiling(body, ceiling).await?;
        self.buffered_reply(res, ceiling, started).await
    }

    /// The whole-response half of `chat`: one JSON object, read to its end.
    ///
    /// Its own method because the streaming path needs it too — for the
    /// endpoint that was asked to stream and answered with a plain completion
    /// instead, which is a valid reply and not a broken stream.
    async fn buffered_reply(
        &self,
        res: reqwest::Response,
        ceiling: Option<usize>,
        started: std::time::Instant,
    ) -> Result<ChatReply> {
        let role = self.role;
        let v: serde_json::Value = res.json().await.map_err(|e| Error::Inference {
            role,
            detail: e.to_string(),
        })?;
        let finish_reason = v["choices"][0]["finish_reason"].as_str();
        tracing::info!(
            role,
            ms = started.elapsed().as_millis(),
            tokens = v["usage"]["completion_tokens"].as_u64(),
            finish_reason,
            "completion finished"
        );
        let text = v["choices"][0]["message"]["content"]
            .as_str()
            .map(str::to_string);
        self.reply(text, finish_reason, ceiling, "no message content")
    }

    /// How a completion ended, whichever way it arrived.
    ///
    /// `length` means the ceiling stopped the model rather than the model
    /// stopping itself, and a reply nothing marks as cut off is read as a
    /// complete one — by the parser, and by whoever asked. `empty` is the
    /// detail for a reply with no content at all that was *not* cut off; the
    /// buffered and streamed paths describe that differently.
    fn reply(
        &self,
        text: Option<String>,
        finish_reason: Option<&str>,
        ceiling: Option<usize>,
        empty: &str,
    ) -> Result<ChatReply> {
        let truncated = finish_reason == Some("length");
        if truncated {
            tracing::warn!(
                role = self.role,
                ceiling,
                "the reply hit its output ceiling and is cut off"
            );
        }
        let text = text.ok_or_else(|| Error::Inference {
            role: self.role,
            // An empty reply at `length` is the one failure that reads as a
            // transport fault and is not one: a reasoning model bills its
            // thinking against this ceiling, and can spend all of it before
            // the message content starts.
            detail: if truncated {
                "the output ceiling was spent before any message content was written; \
                 raise max_output_tokens or lower reasoning_effort"
                    .into()
            } else {
                empty.into()
            },
        })?;
        Ok(ChatReply { text, truncated })
    }
}

/// The ceiling name learned per server, shared by every role that calls it.
///
/// What a 400 buys is worth one call and only if it is not re-bought: `ask`,
/// `judge`, `link_judge`, `vision` and `synthesize` are five [`Endpoint`]s that
/// may all be one server, and the two judges always are. Keyed by address and
/// model, because that pair is what decides how a request gets read.
type CeilingRegistry =
    std::collections::HashMap<(String, String), std::sync::Arc<AtomicCeilingParam>>;

static LEARNED_CEILING: std::sync::LazyLock<std::sync::Mutex<CeilingRegistry>> =
    std::sync::LazyLock::new(Default::default);

/// The shared cell for one server, seeded with `resolved` if this is the first
/// role to ask for it.
///
/// When a later role resolves a different name for the same server, the cell is
/// *not* simply overwritten — see [`Endpoint::with_ceiling_param`] for what that
/// costs. `explicit` says whether the operator named this one or the role
/// inferred it, and an explicit name replaces an inferred one. Anything else
/// keeps what is already there and says so, because the two roles disagreeing
/// about one server is a configuration mistake only the operator can settle.
fn learned_ceiling(
    base_url: &str,
    model: &str,
    resolved: CeilingParam,
    explicit: bool,
    role: &'static str,
) -> std::sync::Arc<AtomicCeilingParam> {
    let mut map = LEARNED_CEILING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let cell = map
        .entry((base_url.to_string(), model.to_string()))
        .or_insert_with(|| std::sync::Arc::new(AtomicCeilingParam::seeded(resolved, explicit)))
        .clone();

    let held = cell.get();
    if held == resolved {
        // Agreement still promotes an inferred name to an explicit one: the
        // operator has now said it, so a later inferring role cannot unseat it.
        if explicit {
            cell.set_authoritative(resolved);
        }
        return cell;
    }

    if explicit && !cell.is_explicit() {
        cell.set_authoritative(resolved);
        return cell;
    }

    tracing::warn!(
        role,
        base_url,
        model,
        wanted = resolved.as_str(),
        using = held.as_str(),
        "two roles share this endpoint and resolved different names for the output ceiling; \
         keeping the one already resolved. Set ceiling_param to the same value on every role \
         pointed at this server."
    );
    cell
}

/// Whether this rejection is the endpoint saying the ceiling went out under the
/// wrong name — as opposed to any of the other things a 400 can mean.
///
/// The two names share no substring, so naming the other one is unambiguous;
/// OpenAI's own message does exactly that ("Use 'max_completion_tokens'
/// instead"). An endpoint that only names the field it refused is read as such
/// when it also says it did not understand it.
fn rejects_ceiling_name(detail: &str, sent: CeilingParam) -> bool {
    if detail.contains(sent.flipped().as_str()) {
        return true;
    }
    let lower = detail.to_ascii_lowercase();
    detail.contains(sent.as_str())
        && [
            "unsupported",
            "not supported",
            "unknown",
            "unrecognized",
            "unrecognised",
        ]
        .iter()
        .any(|w| lower.contains(w))
}

/// The message an upstream failure carries, whichever way it was classified.
fn error_detail(e: &Error) -> &str {
    match e {
        Error::Inference { detail, .. } | Error::InferenceRejected { detail, .. } => detail,
        _ => "",
    }
}

// ── Synthesizer ──────────────────────────────────────────────────────────────────

pub struct HttpSynthesizer {
    ep: Endpoint,
    budget: SynthesisBudget,
    max_artifact_tokens: usize,
    reasoning_effort: Option<String>,
    structured_output: bool,
}

/// What a chat message list costs as a prompt: the text of every `content`,
/// which is everything an endpoint counts that this side controls.
fn message_tokens(messages: &serde_json::Value) -> usize {
    let counter = crate::infer::budget::TokenCounter;
    messages
        .as_array()
        .map(|ms| {
            ms.iter()
                .filter_map(|m| m["content"].as_str())
                .map(|c| counter.count(c))
                .sum()
        })
        .unwrap_or(0)
}

impl HttpSynthesizer {
    pub fn new(cfg: &SynthesizeRole) -> Self {
        Self {
            ep: Endpoint::new(
                &cfg.base_url,
                &cfg.model,
                cfg.api_key.as_deref(),
                cfg.timeout_secs,
                "chunk",
            )
            .with_ceiling_param(cfg.ceiling_param, cfg.reasoning_effort.as_deref()),
            budget: SynthesisBudget {
                context_tokens: cfg.context_tokens,
                max_output_tokens: cfg.max_output_tokens,
                output_ratio: cfg.output_ratio,
                context: crate::infer::context::ContextBudget {
                    opening: cfg.context_opening_tokens,
                    overlap: cfg.context_overlap_tokens,
                },
            },
            max_artifact_tokens: 1024,
            reasoning_effort: cfg.reasoning_effort.clone(),
            structured_output: cfg.structured_output,
        }
    }

    /// Caps chunk size so the embedder never receives an oversized chunk.
    /// Set from `embed.max_input_tokens * 0.8` during wiring.
    pub fn with_max_artifact_tokens(mut self, n: usize) -> Self {
        self.max_artifact_tokens = n;
        self
    }

    /// `schema`, when given, is sent as an OpenAI `json_schema` response format.
    /// An endpoint that honours it compiles the schema into a decoding
    /// constraint, which is the only thing that reliably stops a small local
    /// model closing an array with a brace or dropping a required field. The
    /// calls that want prose rather than JSON — titles — pass `None`.
    async fn chat(&self, messages: serde_json::Value, schema: Option<&str>) -> Result<String> {
        let spent = message_tokens(&messages);
        let mut body = json!({
            "messages": messages,
            "temperature": 0.2,
        });
        if let Some(effort) = &self.reasoning_effort {
            body["reasoning_effort"] = json!(effort);
        }
        if let Some(name) = schema.filter(|_| self.structured_output) {
            body["response_format"] = response_format(name, prompt::artifacts_schema());
        }
        // The same invariant every other caller keeps: prompt plus ceiling has
        // to fit the window as the server counts both. `segment`'s first call
        // is budgeted against the window by construction, but the repair call
        // carries the whole first reply as well — up to `max_output_tokens` of
        // it — so asking for the full ceiling again is a permanent 400 on
        // precisely the windows that needed repairing.
        //
        // Truncation is not an error here: `parse_response` salvages the
        // artifacts a cut-off list still got right, and losing nine good ones to
        // the tenth is the worst trade in the write path.
        let ceiling = crate::infer::budget::ceiling_for_prompt(
            self.budget.context_tokens,
            spent,
            self.budget.max_output_tokens,
        );
        Ok(self.ep.chat(body, Some(ceiling)).await?.text)
    }
}

#[async_trait]
impl Synthesizer for HttpSynthesizer {
    async fn segment(&self, input: SegmentInput<'_>) -> Result<Vec<ProposedArtifact>> {
        let user = prompt::user_prompt(input.core, 1, self.max_artifact_tokens, input.context);
        let first = self
            .chat(
                json!([
                    {"role":"system","content": prompt::SYNTHESIZER_SYSTEM},
                    {"role":"user","content": user}
                ]),
                Some("artifacts"),
            )
            .await?;

        match prompt::parse_response(&first) {
            Ok(chunks) => Ok(chunks),
            Err(e) => {
                // One repair attempt with the parser error fed back. Beyond
                // that the caller falls back to a structural split.
                tracing::warn!(error = %e, "synthesizer returned unparsable output; repairing");
                let repair = prompt::repair_prompt(&first, &e.to_string());
                let messages = json!([
                    {"role":"system","content": prompt::SYNTHESIZER_SYSTEM},
                    {"role":"user","content": user},
                    {"role":"assistant","content": first},
                    {"role":"user","content": repair}
                ]);

                // The repair prompt is the first one plus the whole reply it is
                // repairing, so on a window the first call nearly filled there
                // is no room left to answer in. Sending it anyway spends a call
                // to be refused; not sending it leaves the parse error to stand,
                // and the caller falls back to a structural split — which is
                // what happens when the repair fails in any case.
                if crate::infer::budget::checked_ceiling_for_prompt(
                    self.budget.context_tokens,
                    message_tokens(&messages),
                    self.budget.max_output_tokens,
                )
                .is_none()
                {
                    tracing::warn!(
                        context_tokens = self.budget.context_tokens,
                        "the repair prompt does not fit the window; splitting structurally \
                         instead of spending a call that cannot be answered"
                    );
                    return Err(e);
                }

                let second = self.chat(messages, Some("artifacts")).await?;
                prompt::parse_response(&second)
            }
        }
    }

    fn budget(&self) -> SynthesisBudget {
        self.budget
    }

    async fn title(&self, text: &str, artifact_titles: &[String]) -> Result<Option<String>> {
        let out = self
            .chat(
                json!([
                    {"role":"system","content": prompt::TITLE_SYSTEM},
                    {"role":"user","content": prompt::title_prompt(text, artifact_titles)}
                ]),
                None,
            )
            .await?;
        // A model that ignores "no quotes" should not put them on the screen,
        // and a model that answers with an essay should not become a title.
        let t = out.trim().trim_matches('"').trim();
        Ok((!t.is_empty()).then(|| t.chars().take(120).collect()))
    }
}

// ── Embedder ─────────────────────────────────────────────────────────────────

pub struct HttpEmbedder {
    ep: Endpoint,
    dim: usize,
    max_input_tokens: usize,
    templates: crate::config::EmbedTemplates,
}

impl HttpEmbedder {
    pub fn new(cfg: &EmbedRole) -> Self {
        Self {
            ep: Endpoint::new(
                &cfg.base_url,
                &cfg.model,
                cfg.api_key.as_deref(),
                cfg.timeout_secs,
                "embed",
            ),
            dim: cfg.dim,
            max_input_tokens: cfg.max_input_tokens,
            templates: cfg.templates(),
        }
    }
}

#[async_trait]
impl Embedder for HttpEmbedder {
    async fn embed_raw(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        // `encoding_format` is optional in OpenAI's own API and defaults to
        // float there, but proxies in front of llama.cpp-style servers pass the
        // absent field through as null and the backend rejects it. Sending it
        // explicitly costs nothing and keeps those endpoints usable.
        let body = json!({
            "model": self.ep.model,
            "input": texts,
            "encoding_format": "float",
        });
        let v = self.ep.post_json("embeddings", body).await?;

        let data = v["data"].as_array().ok_or_else(|| Error::Inference {
            role: "embed",
            detail: "response had no data array".into(),
        })?;

        let mut out = vec![Vec::new(); texts.len()];
        for item in data {
            let idx = item["index"].as_u64().unwrap_or(0) as usize;
            let vec_f: Vec<f32> = item["embedding"]
                .as_array()
                .ok_or_else(|| Error::Inference {
                    role: "embed",
                    detail: "missing embedding".into(),
                })?
                .iter()
                .map(|x| x.as_f64().unwrap_or(0.0) as f32)
                .collect();

            if vec_f.len() != self.dim {
                return Err(Error::Inference {
                    role: "embed",
                    detail: format!(
                        "dimension mismatch: config says {}, endpoint returned {}",
                        self.dim,
                        vec_f.len()
                    ),
                });
            }
            if idx >= out.len() {
                return Err(Error::Inference {
                    role: "embed",
                    detail: format!("index {idx} out of range"),
                });
            }
            out[idx] = vec_f;
        }
        if out.iter().any(Vec::is_empty) {
            return Err(Error::Inference {
                role: "embed",
                detail: "endpoint skipped an input".into(),
            });
        }
        Ok(out)
    }

    fn dim(&self) -> usize {
        self.dim
    }
    fn model(&self) -> &str {
        &self.ep.model
    }
    fn max_input_tokens(&self) -> usize {
        self.max_input_tokens
    }
    fn templates(&self) -> &crate::config::EmbedTemplates {
        &self.templates
    }
}

// ── Reranker ─────────────────────────────────────────────────────────────────

pub struct HttpReranker {
    ep: Endpoint,
    style: RerankStyle,
}

impl HttpReranker {
    pub fn new(cfg: &RerankRole) -> Self {
        Self {
            ep: Endpoint::new(
                &cfg.base_url,
                &cfg.model,
                cfg.api_key.as_deref(),
                cfg.timeout_secs,
                "rerank",
            ),
            style: cfg.style,
        }
    }
}

#[async_trait]
impl Reranker for HttpReranker {
    async fn rerank(
        &self,
        query: &str,
        docs: &[String],
        top_n: usize,
    ) -> Result<Vec<(usize, f32)>> {
        // There is no OpenAI-standard rerank endpoint; each server shapes it
        // differently, so the style is configured rather than guessed.
        let (path, body) = match self.style {
            RerankStyle::Tei => ("rerank", json!({ "query": query, "texts": docs })),
            RerankStyle::Cohere => (
                "rerank",
                json!({ "model": self.ep.model, "query": query, "documents": docs, "top_n": top_n }),
            ),
            RerankStyle::Vllm => (
                "v1/rerank",
                json!({ "model": self.ep.model, "query": query, "documents": docs, "top_n": top_n }),
            ),
        };
        let v = self.ep.post_json(path, body).await?;

        // TEI replies with a bare array; Cohere and vLLM wrap it in `results`.
        let arr = v
            .as_array()
            .cloned()
            .or_else(|| v["results"].as_array().cloned())
            .ok_or_else(|| Error::Inference {
                role: "rerank",
                detail: "unrecognised response shape".into(),
            })?;

        let mut out: Vec<(usize, f32)> = arr
            .iter()
            .filter_map(|item| {
                let idx = item["index"].as_u64()? as usize;
                let score = item["score"]
                    .as_f64()
                    .or_else(|| item["relevance_score"].as_f64())?
                    as f32;
                Some((idx, score))
            })
            .filter(|(i, _)| *i < docs.len())
            .collect();

        out.sort_by(|a, b| b.1.total_cmp(&a.1));
        out.truncate(top_n);
        Ok(out)
    }
}

/// An OpenAI `json_schema` response format.
///
/// `strict` is what makes the difference between a schema the endpoint treats
/// as a hint and one it compiles into a grammar the decoder cannot leave.
fn response_format(name: &str, schema: serde_json::Value) -> serde_json::Value {
    json!({
        "type": "json_schema",
        "json_schema": {"name": name, "strict": true, "schema": schema}
    })
}

// ── Server-sent events ───────────────────────────────────────────────────────

/// The response's `content-type`, or `""` when it sent none.
fn content_type(res: &reqwest::Response) -> &str {
    res.headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

/// Whether a reply is one JSON document rather than a stream of frames.
///
/// Asked the narrow way round on purpose: a server that streams under a sloppy
/// content-type must go on being read as a stream, so only a reply that calls
/// itself JSON is taken for the whole completion object. Compared on the media
/// type alone, so a `; charset=utf-8` suffix or a stray capital changes nothing.
fn is_json_document(res: &reqwest::Response) -> bool {
    content_type(res)
        .split(';')
        .next()
        .is_some_and(|t| t.trim().eq_ignore_ascii_case("application/json"))
}

/// One `data:` frame of an OpenAI-shaped stream.
#[derive(Debug, Default)]
pub(crate) struct SseChunk {
    pub content: Option<String>,
    pub reasoning: Option<String>,
    pub finish_reason: Option<String>,
    /// The server's own account of a failure, sent as a frame because the
    /// headers — and the 200 — had already gone out when it happened. llama.cpp
    /// does this for a prompt that outgrows the context; vLLM and most proxies
    /// do it too. A frame like that has no `choices`, and a parser that only
    /// reads `choices` drops the one line that says why the stream ended.
    pub error: Option<String>,
}

/// Parse one line. `None` for anything that is not a chunk: blank lines,
/// comment lines, and the `[DONE]` sentinel.
pub(crate) fn parse_sse_line(line: &str) -> Option<SseChunk> {
    let payload = line.strip_prefix("data:")?.trim();
    if payload.is_empty() || payload == "[DONE]" {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    if let Some(err) = v.get("error") {
        // `{"error":{"message":..}}` is the OpenAI shape; `{"error":"..."}`
        // is what some servers send. Either way the message is what matters,
        // and the whole object is a fair fallback for a shape neither of these.
        let message = err
            .get("message")
            .and_then(|m| m.as_str())
            .map(str::to_string)
            .or_else(|| err.as_str().map(str::to_string))
            .unwrap_or_else(|| err.to_string());
        return Some(SseChunk {
            error: Some(message),
            ..SseChunk::default()
        });
    }
    let choice = v.get("choices")?.get(0)?;
    let delta = choice.get("delta");
    let s = |o: Option<&serde_json::Value>, k: &str| {
        o.and_then(|d| d.get(k))
            .and_then(|x| x.as_str())
            .filter(|x| !x.is_empty())
            .map(str::to_string)
    };
    Some(SseChunk {
        content: s(delta, "content"),
        // Endpoints disagree on the name — llama.cpp and vLLM say
        // `reasoning_content`, others say `reasoning` — and reading one spelling
        // drops the thinking entirely on the servers that use the other.
        reasoning: s(delta, "reasoning_content").or_else(|| s(delta, "reasoning")),
        finish_reason: choice
            .get("finish_reason")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        error: None,
    })
}

/// Reassembles frames out of a byte stream that is cut wherever the network
/// cut it.
///
/// One read is not one line: a JSON object — and a single UTF-8 character —
/// can be split across two of them. A parser that decodes and parses each read
/// on its own loses tokens on exactly the long answers streaming exists for,
/// and does it intermittently. So bytes accumulate here, and only whole lines
/// leave; whatever follows the last newline stays for the next read.
#[derive(Default)]
pub(crate) struct SseBuffer {
    buf: Vec<u8>,
}

impl SseBuffer {
    /// The chunks the newly arrived bytes completed, in order.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<SseChunk> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(nl) = self.buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=nl).collect();
            if let Some(c) = parse_sse_line(
                String::from_utf8_lossy(&line[..line.len() - 1]).trim_end_matches('\r'),
            ) {
                out.push(c);
            }
        }
        out
    }

    /// What a stream that ended without a final newline still owes. Endpoints
    /// terminate their last frame properly, but a truncated connection is
    /// exactly the case where the tokens already received are worth keeping.
    pub(crate) fn finish(&mut self) -> Vec<SseChunk> {
        let rest = std::mem::take(&mut self.buf);
        parse_sse_line(String::from_utf8_lossy(&rest).trim_end_matches('\r'))
            .into_iter()
            .collect()
    }
}

// ── Completer ────────────────────────────────────────────────────────────────

pub struct HttpCompleter {
    ep: Endpoint,
    context_tokens: usize,
    /// Hard ceiling on output tokens, sent on every call.
    ///
    /// Not optional, and not merely a cost control. `response_schema` compiles
    /// into a decoding constraint, and while the model is inside a JSON string
    /// the end-of-sequence token is not a valid continuation — so it is masked
    /// out and the model *cannot* stop there. A small model that wanders into a
    /// long or repeating `merged.text` therefore has nothing of its own to stop
    /// it: without this ceiling the only limit is the server's, which one judge
    /// call reached after ~190KB and a quarter hour, to be thrown away whole
    /// because a reply cut off mid-string does not parse.
    max_output_tokens: usize,
    reasoning_effort: Option<String>,
    /// The JSON Schema this role's replies must satisfy, sent as a response
    /// format so the endpoint constrains decoding. `None` for the ask role,
    /// whose answer is prose for a person to read.
    response_schema: Option<(&'static str, serde_json::Value)>,
}

impl HttpCompleter {
    pub fn new(cfg: &AskRole) -> Self {
        Self {
            ep: Endpoint::new(
                &cfg.base_url,
                &cfg.model,
                cfg.api_key.as_deref(),
                cfg.timeout_secs,
                "ask",
            )
            .with_ceiling_param(cfg.ceiling_param, cfg.reasoning_effort.as_deref()),
            context_tokens: cfg.context_tokens,
            max_output_tokens: cfg.max_output_tokens,
            reasoning_effort: cfg.reasoning_effort.clone(),
            response_schema: None,
        }
    }

    /// The request body both the buffered and the streamed call send. One
    /// place, because a difference between the two would be a difference in the
    /// answer: which name the ceiling goes out under, how hard the model is
    /// told to think, and whether the reply is schema-constrained all decide
    /// what comes back.
    fn body(&self, system: &str, user: &str) -> serde_json::Value {
        let mut body = json!({
            "messages": [
                {"role":"system","content": system},
                {"role":"user","content": user}
            ],
            "temperature": 0.3,
        });
        if let Some(effort) = &self.reasoning_effort {
            body["reasoning_effort"] = json!(effort);
        }
        if let Some((name, schema)) = &self.response_schema {
            body["response_format"] = response_format(name, schema.clone());
        }
        body
    }

    /// Never above what the role was configured to allow: a caller asking for
    /// room it measured against the context window is asking for a smaller
    /// ceiling, never a larger one.
    fn clamped(&self, ceiling: usize) -> usize {
        ceiling.min(self.max_output_tokens).max(1)
    }

    /// The judge that rules on duplicate pairs, on the synthesize endpoint.
    ///
    /// Judging is the same kind of work as segmentation — read a passage,
    /// decide something about it, answer in a fixed shape — and nothing like
    /// answering a user's question. It also runs in the background, where a
    /// slow careful model costs nobody a wait, while sharing the ask endpoint
    /// puts sweep traffic in front of an interactive request.
    pub fn for_judging(cfg: &SynthesizeRole) -> Self {
        Self::judging(cfg, ("verdict", prompt::dedupe_schema()))
    }

    /// The judge that rules on associative links, on the same endpoint.
    ///
    /// A separate completer rather than a second caller of `for_judging`,
    /// because the response format is carried by the struct and the two judges
    /// answer different questions. Sharing one meant every link went out under
    /// the dedupe grammar, which cannot express `related` or `unrelated` at all.
    pub fn for_link_judging(cfg: &SynthesizeRole) -> Self {
        Self::judging(cfg, ("link", prompt::link_schema()))
    }

    /// The claim check behind the ask harness: same endpoint and settings as
    /// the judges, its own response shape.
    pub fn for_claim_checking(cfg: &SynthesizeRole) -> Self {
        Self::judging(cfg, ("claims", prompt::claims_schema()))
    }

    /// The model that names a knowledge gap, on the judges' endpoint.
    pub fn for_gap_naming(cfg: &SynthesizeRole) -> Self {
        Self::judging(cfg, ("gap_label", prompt::gap_label_schema()))
    }

    /// The model that writes an artifact from a pursuit, on the judges'
    /// endpoint: background work in a fixed shape, like every other judge.
    pub fn for_generating(cfg: &SynthesizeRole) -> Self {
        Self::judging(cfg, ("artifact", prompt::generate_schema()))
    }

    /// The model that says, once, what one answer still needs.
    ///
    /// Takes a `TierConfig` rather than a role because that is honestly what it
    /// is handed: this call has no role of its own, it runs on whichever
    /// endpoint the operator pointed `ask.follow_up_tier` at — the efficient
    /// one, typically, while the answer it feeds runs on the deep one. Falling
    /// back to the ask role's own endpoint is the config layer's job, so by the
    /// time this is called there is exactly one endpoint to build.
    pub fn for_follow_up(cfg: &TierConfig) -> Self {
        Self {
            ep: Endpoint::new(
                &cfg.base_url,
                &cfg.model,
                cfg.api_key.as_deref(),
                cfg.timeout_secs,
                "follow_up",
            )
            .with_ceiling_param(cfg.ceiling_param, cfg.reasoning_effort.as_deref()),
            context_tokens: cfg.context_tokens,
            max_output_tokens: cfg.max_output_tokens,
            reasoning_effort: cfg.reasoning_effort.clone(),
            response_schema: cfg
                .structured_output
                .then_some(("need", prompt::follow_up_schema())),
        }
    }

    fn judging(cfg: &SynthesizeRole, schema: (&'static str, serde_json::Value)) -> Self {
        Self {
            ep: Endpoint::new(
                &cfg.base_url,
                &cfg.model,
                cfg.api_key.as_deref(),
                cfg.timeout_secs,
                "judge",
            )
            .with_ceiling_param(cfg.ceiling_param, cfg.reasoning_effort.as_deref()),
            context_tokens: cfg.context_tokens,
            max_output_tokens: cfg.max_output_tokens,
            reasoning_effort: cfg.reasoning_effort.clone(),
            response_schema: cfg.structured_output.then_some(schema),
        }
    }
}

#[async_trait]
impl Completer for HttpCompleter {
    /// The judges come through here, and neither asks for a ceiling that fits
    /// what it sent: `dedupe_prompt` carries two whole artifacts and, behind a
    /// merged one, the captured sources it was written from — and asking for the
    /// full configured ceiling beside that is a request the endpoint refuses. So
    /// the ceiling is measured against what the prompt costs, the way `ask`
    /// measures its own.
    ///
    /// A prompt that leaves no room for a reply is refused here rather than
    /// sent under a ceiling of 1. The clamped call does not fail cleanly: it
    /// returns 200 with an empty message and `finish_reason = "length"`, which
    /// surfaces as a retryable `Error::Inference`, so the sweep re-sends the
    /// same structurally impossible request forever. `InferenceRejected` is
    /// permanent, which is what a prompt too large for its window is.
    async fn complete(&self, system: &str, user: &str) -> Result<String> {
        let counter = crate::infer::budget::TokenCounter;
        let spent = counter.count(system) + counter.count(user);
        let Some(ceiling) = crate::infer::budget::checked_ceiling_for_prompt(
            self.context_tokens,
            spent,
            self.max_output_tokens,
        ) else {
            return Err(Error::InferenceRejected {
                role: self.ep.role,
                detail: format!(
                    "the prompt is {spent} tokens against a {} token context window, which \
                     leaves no room for a reply. Raise this role's context_tokens, or lower \
                     what the caller packs into one call.",
                    self.context_tokens
                ),
            });
        };
        if ceiling < self.max_output_tokens {
            tracing::debug!(
                ceiling,
                configured = self.max_output_tokens,
                prompt = spent,
                context = self.context_tokens,
                "the prompt left less window than the configured output ceiling"
            );
        }
        Ok(self.answer(system, user, ceiling).await?.text)
    }

    async fn answer(&self, system: &str, user: &str, ceiling: usize) -> Result<Completion> {
        let reply = self
            .ep
            .chat(self.body(system, user), Some(self.clamped(ceiling)))
            .await?;
        Ok(Completion {
            text: reply.text,
            truncated: reply.truncated,
        })
    }

    /// The same call as `answer`, read a frame at a time.
    ///
    /// The body is the one `answer` sends with `stream` added, so the reply is
    /// the same reply — what changes is only when the caller sees it.
    async fn answer_streaming(
        &self,
        system: &str,
        user: &str,
        ceiling: usize,
        sink: tokio::sync::mpsc::Sender<Delta>,
    ) -> Result<Completion> {
        let role = self.ep.role;
        let started = std::time::Instant::now();
        let ceiling = self.clamped(ceiling);
        let mut body = self.body(system, user);
        body["stream"] = json!(true);

        let mut res = self.ep.send_with_ceiling(body, Some(ceiling)).await?;
        // An endpoint or proxy that ignores `stream` answers with the plain
        // completion object. That is not a broken stream, it is the reply —
        // and every door reaches the model through here now, including the
        // two that never asked for a stream. So the buffered parse takes it,
        // and the sink sees the whole answer as one token rather than none.
        if is_json_document(&res) {
            tracing::debug!(
                role,
                content_type = content_type(&res),
                "asked to stream, answered whole; reading it as a completion"
            );
            let reply = self.ep.buffered_reply(res, Some(ceiling), started).await?;
            let _ = sink.send(Delta::Token(reply.text.clone())).await;
            return Ok(Completion {
                text: reply.text,
                truncated: reply.truncated,
            });
        }
        let mut frames = SseBuffer::default();
        let mut text = String::new();
        let mut finish_reason: Option<String> = None;
        loop {
            let read = res.chunk().await.map_err(|e| Error::Inference {
                role,
                detail: e.to_string(),
            })?;
            let chunks = match &read {
                Some(bytes) => frames.push(bytes),
                None => frames.finish(),
            };
            for c in chunks {
                if let Some(message) = c.error {
                    // Surfaced as-is: this is the server's reason, and without
                    // it the failure below reads as a transport fault with an
                    // empty body — which is what it looked like before.
                    return Err(Error::Inference {
                        role,
                        detail: format!("the stream carried an error: {message}"),
                    });
                }
                if let Some(r) = c.reasoning {
                    // Send errors are ignored throughout: the receiver is a
                    // reader that may stop reading, and the response is still
                    // read to its end rather than abandoned mid-body, so the
                    // connection is reusable and the call ends when the GPU is
                    // actually free. Nothing records what comes back once the
                    // receiver is gone: the caller that dropped it was the only
                    // recorder.
                    let _ = sink.send(Delta::Reasoning(r)).await;
                }
                if let Some(t) = c.content {
                    text.push_str(&t);
                    let _ = sink.send(Delta::Token(t)).await;
                }
                if c.finish_reason.is_some() {
                    finish_reason = c.finish_reason;
                }
            }
            if read.is_none() {
                break;
            }
        }

        // Truncation is read from the last frame that carried a reason, because
        // a stream has no whole-response object to read it from — and an answer
        // cut off mid-sentence that nothing marks is read as a finished one.
        tracing::info!(
            role,
            ms = started.elapsed().as_millis(),
            finish_reason,
            "streamed completion finished"
        );
        let reply = self.ep.reply(
            Some(text).filter(|t| !t.is_empty()),
            finish_reason.as_deref(),
            Some(ceiling),
            "the stream carried no message content",
        )?;
        Ok(Completion {
            text: reply.text,
            truncated: reply.truncated,
        })
    }

    fn context_tokens(&self) -> usize {
        self.context_tokens
    }

    fn max_output_tokens(&self) -> usize {
        self.max_output_tokens
    }
}

// ── Describer ────────────────────────────────────────────────────────────────

pub struct HttpDescriber {
    ep: Endpoint,
    /// Hard ceiling on output tokens, sent on every call — the same rule the
    /// other roles follow, and for a stronger reason: what comes back is stored
    /// as a corpus and segmented, so a reply nothing bounds is a document
    /// nothing bounds.
    max_output_tokens: usize,
}

impl HttpDescriber {
    /// Takes both roles because a vision role without its own `base_url` is the
    /// synthesize endpoint: it borrows that endpoint's address, key and — since
    /// it is the same server reading the request — the name it takes the output
    /// ceiling under.
    pub fn new(cfg: &crate::config::VisionRole, synth: Option<&SynthesizeRole>) -> Self {
        let (base_url, api_key) = cfg.resolve(synth);
        Self {
            ep: Endpoint::new(
                &base_url,
                &cfg.model,
                api_key.as_deref(),
                cfg.timeout_secs,
                "vision",
            )
            // This role has no `reasoning_effort` of its own. Where it borrows
            // the synthesize endpoint it inherits that role's — not just its
            // explicit `ceiling_param` — because otherwise the two roles guess
            // different names for one server, and against a silent endpoint the
            // describer's ceiling is the one that gets dropped.
            .with_ceiling_param(
                cfg.ceiling_param(synth),
                cfg.inherited_reasoning_effort(synth),
            ),
            max_output_tokens: cfg.max_output_tokens,
        }
    }
}

#[async_trait]
impl Describer for HttpDescriber {
    async fn describe(&self, image_jpeg: &[u8], context: &str) -> Result<String> {
        use base64::Engine;
        let data_url = format!(
            "data:image/jpeg;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(image_jpeg)
        );
        let body = json!({
            "messages": [
                {"role": "system", "content": prompt::DESCRIBE_SYSTEM},
                {"role": "user", "content": [
                    {"type": "text", "text": context},
                    {"type": "image_url", "image_url": {"url": data_url}}
                ]}
            ],
            "temperature": 0.2,
        });
        // A description cut off at the ceiling is still worth storing — it is
        // prose, and the part that arrived describes what it describes — so the
        // truncation is logged rather than raised. An empty one is not, and
        // `chat` says so in terms the operator can act on.
        Ok(self.ep.chat(body, Some(self.max_output_tokens)).await?.text)
    }
}

/// One cheap reachability check per role at startup. Failure is a warning, not
/// a fatal error: ingest is designed to survive a dead inference endpoint.
pub async fn probe(role: &str, base_url: &str, api_key: Option<&str>) -> bool {
    let c = client(crate::config::DEFAULT_TIMEOUT_SECS);
    let mut req = c.get(url(base_url, "models"));
    if let Some(k) = api_key {
        req = req.bearer_auth(k);
    }
    match req.timeout(std::time::Duration::from_secs(5)).send().await {
        Ok(r) if r.status().is_success() => {
            tracing::info!(role, "inference endpoint reachable");
            true
        }
        Ok(r) => {
            tracing::warn!(role, status = %r.status(), "inference endpoint responded with an error");
            false
        }
        Err(e) => {
            tracing::warn!(role, error = %e, "inference endpoint unreachable");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_4xx_is_permanent_except_the_two_that_mean_try_again() {
        use reqwest::StatusCode as S;
        assert!(permanent_upstream_status(S::BAD_REQUEST));
        assert!(permanent_upstream_status(S::PAYLOAD_TOO_LARGE));
        assert!(permanent_upstream_status(S::UNSUPPORTED_MEDIA_TYPE));
        assert!(permanent_upstream_status(S::UNPROCESSABLE_ENTITY));
        assert!(!permanent_upstream_status(S::REQUEST_TIMEOUT));
        assert!(!permanent_upstream_status(S::TOO_MANY_REQUESTS));
        // The endpoint, not the request: an expired key and a restarting proxy
        // both answer like this, and both heal on their own.
        assert!(!permanent_upstream_status(S::UNAUTHORIZED));
        assert!(!permanent_upstream_status(S::FORBIDDEN));
        assert!(!permanent_upstream_status(S::NOT_FOUND));
        assert!(!permanent_upstream_status(S::INTERNAL_SERVER_ERROR));
        assert!(!permanent_upstream_status(S::BAD_GATEWAY));
    }

    /// The empty context every one of these tests wants: they exercise the
    /// transport and the parser, not the windowing.
    static EMPTY_CONTEXT: crate::infer::context::WindowContext =
        crate::infer::context::WindowContext {
            opening: None,
            before: None,
            after: None,
        };

    fn window(text: &str) -> SegmentInput<'_> {
        SegmentInput {
            core: text,
            context: &EMPTY_CONTEXT,
        }
    }
    use crate::config::{AskRole, EmbedRole, RerankRole, RerankStyle, SynthesizeRole};
    use crate::infer::{Completer, Embedder, Reranker, Synthesizer};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A model name no other test shares.
    ///
    /// [`LEARNED_CEILING`] is keyed by `(base_url, model)` and outlives every
    /// test in the process, while a `MockServer`'s ephemeral port does not: a
    /// later test can bind the port a finished one used, and would then inherit
    /// what that test's endpoint had resolved or learned. Varying the model
    /// keeps each test's cell its own.
    fn test_model() -> String {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        format!("m{}", N.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }

    fn synthesize_cfg(base: String) -> SynthesizeRole {
        SynthesizeRole {
            base_url: base,
            model: test_model(),
            api_key: Some("secret".into()),
            context_tokens: 8192,
            max_output_tokens: 2048,
            output_ratio: 1.4,
            timeout_secs: crate::config::DEFAULT_TIMEOUT_SECS,
            reasoning_effort: None,
            ceiling_param: None,
            structured_output: true,
            context_opening_tokens: 200,
            context_overlap_tokens: 150,
        }
    }
    fn ask_cfg(base: String) -> AskRole {
        AskRole {
            base_url: base,
            model: test_model(),
            api_key: None,
            context_tokens: 4096,
            max_output_tokens: 1024,
            timeout_secs: crate::config::DEFAULT_TIMEOUT_SECS,
            reasoning_effort: None,
            ceiling_param: None,
            follow_up: false,
            structured_output: true,
            follow_up_endpoint: None,
        }
    }
    fn vision_cfg(base: Option<String>) -> crate::config::VisionRole {
        crate::config::VisionRole {
            model: test_model(),
            base_url: base,
            api_key: Some("k".into()),
            timeout_secs: 30,
            max_output_tokens: 2048,
            ceiling_param: None,
        }
    }
    fn embed_cfg(base: String) -> EmbedRole {
        let t = crate::config::EmbedTemplates::default();
        EmbedRole {
            base_url: base,
            model: "e".into(),
            api_key: None,
            dim: 4,
            max_input_tokens: 512,
            timeout_secs: crate::config::DEFAULT_TIMEOUT_SECS,
            query_template: t.query_template,
            document_template: t.document_template,
            document_template_untitled: t.document_template_untitled,
            chunk_tokens: crate::config::DEFAULT_CHUNK_TOKENS,
        }
    }

    #[tokio::test]
    async fn synthesizer_posts_chat_completions_and_parses_artifacts() {
        let server = MockServer::start().await;
        let reply = serde_json::json!({
            // r###: the payload contains `"##` (a quoted markdown H2), which
            // terminates both r#"..."# and r##"..."## literals.
            "choices":[{"message":{"content":
                r###"{"artifacts":[{"text":"## A\nbody","title":"A","category":"note","tags":["t"]}]}"###}}]
        });
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("authorization", "Bearer secret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(reply))
            .mount(&server)
            .await;

        let c = HttpSynthesizer::new(&synthesize_cfg(server.uri()));
        let out = c.segment(window("anything")).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title.as_deref(), Some("A"));
    }

    /// The captured request body of a single call, so a test can assert on what
    /// was sent rather than only on what came back.
    async fn sent_body(server: &MockServer) -> serde_json::Value {
        let reqs = server.received_requests().await.expect("recording is on");
        assert_eq!(reqs.len(), 1, "expected exactly one call");
        serde_json::from_slice(&reqs[0].body).expect("the request body is JSON")
    }

    async fn echoing_server(content: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices":[{"message":{"content": content}}]
            })))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn segmentation_constrains_the_reply_to_the_schema_the_parser_wants() {
        // A 9B model asked for JSON closes an array with a brace and drops
        // required fields, and the parse error that follows is indistinguishable
        // from a truncated reply. The schema is the only thing that makes the
        // malformed reply ungeneratable rather than merely unwanted.
        let server = echoing_server(
            r#"{"artifacts":[{"text":"body","title":"A","category":"n","tags":[]}]}"#,
        )
        .await;
        HttpSynthesizer::new(&synthesize_cfg(server.uri()))
            .segment(window("anything"))
            .await
            .unwrap();

        let body = sent_body(&server).await;
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(
            body["response_format"]["json_schema"]["strict"], true,
            "without strict the schema is a hint, not a decoding constraint"
        );
        assert_eq!(
            body["response_format"]["json_schema"]["schema"],
            prompt::artifacts_schema(),
            "the schema sent is not the one the parser was written against"
        );
    }

    #[tokio::test]
    async fn a_title_is_asked_for_as_prose_not_as_json() {
        // `chat` is shared with segmentation, and a schema leaking onto this
        // call would constrain a one-line title into a JSON object.
        let server = echoing_server("A Good Title").await;
        let t = HttpSynthesizer::new(&synthesize_cfg(server.uri()))
            .title("some text", &[])
            .await
            .unwrap();

        assert_eq!(t.as_deref(), Some("A Good Title"));
        assert!(
            sent_body(&server).await.get("response_format").is_none(),
            "a title was asked for as JSON"
        );
    }

    #[tokio::test]
    async fn structured_output_can_be_switched_off_for_an_endpoint_that_rejects_it() {
        let mut cfg = synthesize_cfg(String::new());
        cfg.structured_output = false;
        let server = echoing_server(
            r#"{"artifacts":[{"text":"body","title":"A","category":"n","tags":[]}]}"#,
        )
        .await;
        cfg.base_url = server.uri();

        HttpSynthesizer::new(&cfg)
            .segment(window("x"))
            .await
            .unwrap();

        assert!(
            sent_body(&server).await.get("response_format").is_none(),
            "the opt-out did not reach the request"
        );
    }

    #[tokio::test]
    async fn the_judge_constrains_its_verdict_and_the_ask_path_does_not() {
        // Same struct, two roles: the judge's reply is parsed as a verdict and
        // must be constrained, while the ask reply is prose for a person and
        // would be ruined by a schema.
        let judge_server = echoing_server(r#"{"relation":"distinct"}"#).await;
        let mut cfg = synthesize_cfg(judge_server.uri());
        cfg.api_key = None;
        HttpCompleter::for_judging(&cfg)
            .complete("s", "u")
            .await
            .unwrap();
        let body = sent_body(&judge_server).await;
        assert_eq!(
            body["response_format"]["json_schema"]["schema"],
            prompt::dedupe_schema()
        );

        let ask_server = echoing_server("a prose answer").await;
        HttpCompleter::new(&AskRole {
            base_url: ask_server.uri(),
            model: test_model(),
            api_key: None,
            context_tokens: 4096,
            max_output_tokens: 1024,
            timeout_secs: crate::config::DEFAULT_TIMEOUT_SECS,
            reasoning_effort: None,
            ceiling_param: None,
            follow_up: false,
            structured_output: true,
            follow_up_endpoint: None,
        })
        .complete("s", "u")
        .await
        .unwrap();
        assert!(
            sent_body(&ask_server)
                .await
                .get("response_format")
                .is_none(),
            "the answer to a question was constrained to a verdict schema"
        );
    }

    #[tokio::test]
    async fn synthesizer_retries_once_with_a_repair_prompt() {
        let server = MockServer::start().await;
        // First call garbage, second valid. `up_to_n_times` makes the first
        // mock retire after one hit so the second takes over.
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices":[{"message":{"content":"sorry, here is prose"}}]
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices":[{"message":{"content":r#"{"artifacts":[{"text":"ok"}]}"#}}]
            })))
            .mount(&server)
            .await;

        let c = HttpSynthesizer::new(&synthesize_cfg(server.uri()));
        let out = c.segment(window("anything")).await.unwrap();
        assert_eq!(out[0].text, "ok");
    }

    /// The repair call carries the whole first reply on top of the prompt that
    /// produced it, so asking for the full configured ceiling again breaks the
    /// one invariant the endpoint enforces — prompt plus ceiling fits the window
    /// — and it breaks it on precisely the windows that needed repairing.
    #[tokio::test]
    async fn the_repair_call_still_fits_the_window_it_is_repairing() {
        let server = MockServer::start().await;
        // A long unparsable reply. The repair prompt carries it twice — once as
        // the assistant turn, once quoted back in the repair instruction — so
        // this leaves the window with less room than the configured ceiling.
        let long_prose = "sorry, here is prose ".repeat(533);
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices":[{"message":{"content": long_prose}}]
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices":[{"message":{"content":r#"{"artifacts":[{"text":"ok"}]}"#}}]
            })))
            .mount(&server)
            .await;

        // A ceiling that is half its window, which is the shape the shipped
        // example config uses and the one where this fails.
        let mut cfg = synthesize_cfg(server.uri());
        cfg.max_output_tokens = cfg.context_tokens / 2;
        let context = cfg.context_tokens;
        HttpSynthesizer::new(&cfg)
            .segment(window(&"a sentence to segment. ".repeat(60)))
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2, "the repair call was not made");
        let repair: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
        let prompt = message_tokens(&repair["messages"]);
        let ceiling = repair["max_tokens"].as_u64().expect("a ceiling is sent") as usize;
        assert!(
            prompt > cfg.max_output_tokens,
            "the repair prompt did not carry the first reply, so this proves nothing: {prompt}"
        );
        assert!(
            prompt + ceiling <= context,
            "the repair asked for {ceiling} tokens beside a {prompt} token prompt, \
             which is {} over the {context} token window",
            prompt + ceiling - context
        );
    }

    /// And where the repair prompt cannot fit at all, the call is not made:
    /// there is no ceiling that answers it, so spending the call only delays
    /// the structural split the caller falls back to anyway.
    #[tokio::test]
    async fn a_repair_that_cannot_fit_the_window_is_not_attempted() {
        let server = echoing_server(&"sorry, here is prose ".repeat(2000)).await;

        let cfg = synthesize_cfg(server.uri());
        let out = HttpSynthesizer::new(&cfg)
            .segment(window(&"a sentence to segment. ".repeat(60)))
            .await;

        assert!(
            matches!(out, Err(crate::error::Error::MalformedLlmOutput(_))),
            "the parse error did not stand, so the structural fallback never runs: {out:?}"
        );
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "a repair call went out with no room in the window to answer it"
        );
    }

    #[tokio::test]
    async fn synthesizer_gives_up_after_the_repair_attempt() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices":[{"message":{"content":"still prose"}}]
            })))
            .mount(&server)
            .await;

        let c = HttpSynthesizer::new(&synthesize_cfg(server.uri()));
        assert!(matches!(
            c.segment(window("x")).await,
            Err(crate::error::Error::MalformedLlmOutput(_))
        ));
    }

    #[tokio::test]
    async fn upstream_5xx_is_a_retryable_inference_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let c = HttpSynthesizer::new(&synthesize_cfg(server.uri()));
        let e = c.segment(window("x")).await.unwrap_err();
        assert!(matches!(
            e,
            crate::error::Error::Inference { role: "chunk", .. }
        ));
        assert!(e.retryable());
    }

    #[tokio::test]
    async fn rate_limit_is_retryable() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;
        let e = HttpEmbedder::new(&embed_cfg(server.uri()))
            .embed_raw(&["x".into()])
            .await
            .unwrap_err();
        assert!(e.retryable());
    }

    #[tokio::test]
    async fn embedder_sends_float_encoding_and_orders_results_by_index() {
        // `encoding_format` is sent explicitly: a litellm proxy in front of a
        // llama.cpp-style server forwards the absent field as null and the
        // backend answers 500. And `index` is authoritative over array position.
        use wiremock::matchers::body_partial_json;
        let server = MockServer::start().await;
        let reply = serde_json::json!({"data":[
            {"index":1,"embedding":[1.0,0.0,0.0,0.0]},
            {"index":0,"embedding":[0.0,1.0,0.0,0.0]}
        ]});
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .and(body_partial_json(
                serde_json::json!({"encoding_format": "float"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(reply))
            .mount(&server)
            .await;

        let e = HttpEmbedder::new(&embed_cfg(server.uri()));
        let out = e
            .embed_raw(&["first".into(), "second".into()])
            .await
            .unwrap();
        assert_eq!(out[0], vec![0.0, 1.0, 0.0, 0.0]);
        assert_eq!(out[1], vec![1.0, 0.0, 0.0, 0.0]);
    }

    #[tokio::test]
    async fn documents_and_queries_are_rendered_through_the_templates_before_the_post() {
        use wiremock::matchers::body_partial_json;
        let server = MockServer::start().await;
        let one = serde_json::json!({"data":[{"index":0,"embedding":[1.0,0.0,0.0,0.0]}]});
        let two = serde_json::json!({"data":[
            {"index":0,"embedding":[1.0,0.0,0.0,0.0]},
            {"index":1,"embedding":[0.0,1.0,0.0,0.0]}
        ]});
        // The document side: titled and untitled take different templates.
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .and(body_partial_json(serde_json::json!({
                "input": ["title: Recovering | text: run fsck", "title: none | text: bare"]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(two))
            .expect(1)
            .mount(&server)
            .await;
        // The query side: the retrieval task prefix, nothing else.
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .and(body_partial_json(serde_json::json!({
                "input": ["task: search result | query: fsck"]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(one))
            .expect(1)
            .mount(&server)
            .await;

        let e = HttpEmbedder::new(&embed_cfg(server.uri()));
        let docs = vec![
            crate::infer::EmbedDoc {
                title: Some("Recovering".into()),
                text: "run fsck".into(),
            },
            crate::infer::EmbedDoc {
                title: None,
                text: "bare".into(),
            },
        ];
        let out = e.embed_documents(&docs).await.unwrap();
        assert_eq!(out.len(), 2);
        let q = e.embed_query("fsck").await.unwrap();
        assert_eq!(q, vec![1.0, 0.0, 0.0, 0.0]);
        // `.expect(1)` on both mocks is verified when `server` drops.
    }

    #[tokio::test]
    async fn embedder_rejects_a_dimension_mismatch() {
        let server = MockServer::start().await;
        let reply = serde_json::json!({"data":[{"index":0,"embedding":[1.0,2.0]}]});
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(reply))
            .mount(&server)
            .await;
        // Config says dim 4, server returned 2. Writing this to Qdrant would
        // corrupt the collection.
        let e = HttpEmbedder::new(&embed_cfg(server.uri()))
            .embed_raw(&["x".into()])
            .await
            .unwrap_err();
        assert!(e.to_string().contains("dimension"));
    }

    #[tokio::test]
    async fn embedder_rejects_a_short_batch() {
        let server = MockServer::start().await;
        // Two inputs, one embedding back. Accepting this would silently pair
        // the wrong vector with the wrong chunk.
        let reply = serde_json::json!({"data":[{"index":0,"embedding":[1.0,0.0,0.0,0.0]}]});
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(reply))
            .mount(&server)
            .await;
        let e = HttpEmbedder::new(&embed_cfg(server.uri()))
            .embed_raw(&["a".into(), "b".into()])
            .await
            .unwrap_err();
        assert!(e.to_string().contains("skipped"), "got: {e}");
    }

    #[tokio::test]
    async fn the_reranker_reads_both_wire_shapes_and_drops_bad_indexes() {
        // TEI answers a bare list of {index, score}; Cohere wraps
        // {index, relevance_score} in `results`. Either way the caller gets
        // (index, score) best first, and an index outside the batch is dropped
        // rather than panicking on `results.get(idx)`.
        type Case = (RerankStyle, serde_json::Value, usize, Vec<(usize, f32)>);
        let cases: [Case; 3] = [
            (
                RerankStyle::Tei,
                serde_json::json!([{"index":2,"score":0.9},{"index":0,"score":0.4}]),
                3,
                vec![(2, 0.9), (0, 0.4)],
            ),
            (
                RerankStyle::Cohere,
                serde_json::json!({"results":[
                    {"index":1,"relevance_score":0.8},
                    {"index":0,"relevance_score":0.95}
                ]}),
                2,
                vec![(0, 0.95), (1, 0.8)],
            ),
            (
                RerankStyle::Tei,
                serde_json::json!([{"index":99,"score":0.9},{"index":0,"score":0.4}]),
                1,
                vec![(0, 0.4)],
            ),
        ];
        for (style, reply, docs, want) in cases {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/rerank"))
                .respond_with(ResponseTemplate::new(200).set_body_json(reply))
                .mount(&server)
                .await;
            let cfg = RerankRole {
                base_url: server.uri(),
                model: "r".into(),
                api_key: None,
                style,
                timeout_secs: crate::config::DEFAULT_TIMEOUT_SECS,
            };
            let batch: Vec<String> = (0..docs).map(|i| format!("d{i}")).collect();
            let out = HttpReranker::new(&cfg)
                .rerank("q", &batch, 5)
                .await
                .unwrap();
            assert_eq!(out, want);
        }
    }

    #[tokio::test]
    async fn completer_returns_message_content_and_tolerates_a_trailing_slash() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices":[{"message":{"content":"the answer"}}]
            })))
            .mount(&server)
            .await;
        let cfg = AskRole {
            base_url: format!("{}/", server.uri()),
            model: test_model(),
            api_key: None,
            context_tokens: 4096,
            max_output_tokens: 1024,
            timeout_secs: crate::config::DEFAULT_TIMEOUT_SECS,
            reasoning_effort: None,
            ceiling_param: None,
            follow_up: false,
            structured_output: true,
            follow_up_endpoint: None,
        };
        assert_eq!(
            HttpCompleter::new(&cfg).complete("s", "u").await.unwrap(),
            "the answer"
        );
    }

    /// A judge reply is grammar-constrained, and a grammar masks out the
    /// end-of-sequence token everywhere it would not be valid JSON — inside a
    /// string most of all. The model therefore cannot end a reply it has
    /// wandered into, and the only ceiling left is whatever the endpoint applies
    /// when asked for none. One such call ran to ~190KB and was thrown away
    /// whole, so this asserts the ceiling is on the wire rather than merely
    /// configured.
    #[tokio::test]
    async fn every_completion_carries_its_output_ceiling() {
        for label in ["ask", "judge"] {
            let server = echoing_server(r#"{"relation":"distinct"}"#).await;
            let completer = match label {
                "ask" => HttpCompleter::new(&AskRole {
                    base_url: server.uri(),
                    model: test_model(),
                    api_key: None,
                    context_tokens: 4096,
                    max_output_tokens: 1024,
                    timeout_secs: crate::config::DEFAULT_TIMEOUT_SECS,
                    reasoning_effort: None,
                    ceiling_param: None,
                    follow_up: false,
                    structured_output: true,
                    follow_up_endpoint: None,
                }),
                _ => HttpCompleter::for_judging(&synthesize_cfg(server.uri())),
            };
            completer.complete("s", "u").await.unwrap();

            // 1024 from the ask role, 2048 from the synthesize role the judge
            // borrows: each carries its own ceiling, not a shared constant.
            let want = if label == "ask" { 1024 } else { 2048 };
            assert_eq!(
                sent_body(&server).await["max_tokens"].as_u64(),
                Some(want),
                "{label} did not send its output ceiling"
            );
        }
    }

    /// A reasoning model answers a `max_tokens` with a 400 naming
    /// `max_completion_tokens`, so sending the ceiling under the wrong name
    /// against one is not a loose constraint — it is every call failing.
    #[tokio::test]
    async fn a_reasoning_endpoint_gets_the_ceiling_under_the_name_it_accepts() {
        let server = echoing_server(r#"{"verdict":{"relation":"distinct"}}"#).await;
        let mut cfg = synthesize_cfg(server.uri());
        cfg.reasoning_effort = Some("low".into());
        HttpCompleter::for_judging(&cfg)
            .complete("s", "u")
            .await
            .unwrap();

        let body = sent_body(&server).await;
        assert_eq!(body["max_completion_tokens"].as_u64(), Some(2048));
        assert!(
            body.get("max_tokens").is_none(),
            "a reasoning endpoint was sent max_tokens, which it rejects: {body}"
        );
        assert_eq!(body["reasoning_effort"].as_str(), Some("low"));
    }

    /// `reasoning_effort` is a guess at which name the endpoint takes, and the
    /// configuration this project recommends for a local model — suppressing
    /// thinking with `reasoning_effort = "none"` — is exactly where the guess is
    /// wrong. A llama.cpp build reads `max_tokens` and ignores what it does not
    /// know, so guessing wrong there is not a 400 to learn from: it is no
    /// ceiling at all, which is the unbounded reply this all exists to prevent.
    #[tokio::test]
    async fn the_configured_ceiling_name_beats_the_guess() {
        let server = echoing_server(r#"{"verdict":{"relation":"distinct"}}"#).await;
        let mut cfg = synthesize_cfg(server.uri());
        cfg.reasoning_effort = Some("none".into());
        cfg.ceiling_param = Some(CeilingParam::MaxTokens);
        HttpCompleter::for_judging(&cfg)
            .complete("s", "u")
            .await
            .unwrap();

        let body = sent_body(&server).await;
        assert_eq!(body["max_tokens"].as_u64(), Some(2048));
        assert!(
            body.get("max_completion_tokens").is_none(),
            "the configured name lost to the guess: {body}"
        );
        // Still sent: suppressing the thinking is why it was set.
        assert_eq!(body["reasoning_effort"].as_str(), Some("none"));
    }

    /// The other half of the same argument, without the operator having said
    /// anything. `reasoning_effort = "none"` is what this project's own example
    /// config recommends for suppressing a local model's thinking, so reading it
    /// as evidence of a hosted reasoning endpoint sends `max_completion_tokens`
    /// to a llama.cpp build — which ignores the field it does not know, applies
    /// no ceiling, and returns no error saying so. The guess has to fall the
    /// safe way, because only the other direction has a 400 to learn from.
    #[tokio::test]
    async fn suppressed_thinking_is_not_mistaken_for_a_reasoning_endpoint() {
        let server = echoing_server(r#"{"verdict":{"relation":"distinct"}}"#).await;
        let mut cfg = synthesize_cfg(server.uri());
        cfg.reasoning_effort = Some("none".into());
        assert!(cfg.ceiling_param.is_none(), "the operator has not said");
        HttpCompleter::for_judging(&cfg)
            .complete("s", "u")
            .await
            .unwrap();

        let body = sent_body(&server).await;
        assert_eq!(body["max_tokens"].as_u64(), Some(2048));
        assert!(
            body.get("max_completion_tokens").is_none(),
            "a local endpoint was sent the name it silently ignores: {body}"
        );
    }

    /// Neither judge's prompt is small — `dedupe_prompt` carries two whole
    /// artifacts and the captured sources behind a merged one — so asking for
    /// the configured ceiling regardless is a request the endpoint refuses
    /// outright. That 400 is permanent, the pair is re-armed at the same size on
    /// every later sweep, and it never gets judged at all.
    #[tokio::test]
    async fn a_judge_prompt_that_fills_the_window_shrinks_its_own_ceiling() {
        let server = echoing_server(r#"{"verdict":{"relation":"distinct"}}"#).await;
        let cfg = synthesize_cfg(server.uri());
        // Roughly 7000 tokens by the character estimate: 2048 of reply on top of
        // it does not fit the role's 8192-token window.
        let user = "x".repeat(7000 * 7 / 2);
        HttpCompleter::for_judging(&cfg)
            .complete("s", &user)
            .await
            .unwrap();

        let body = sent_body(&server).await;
        let sent = body["max_tokens"].as_u64().unwrap() as usize;
        let counter = crate::infer::budget::TokenCounter;
        let prompt = counter.count("s") + counter.count(&user);
        assert!(
            sent < cfg.max_output_tokens,
            "the full configured ceiling went out beside a {prompt}-token prompt"
        );
        assert!(
            prompt + sent < cfg.context_tokens,
            "prompt ({prompt}) plus ceiling ({sent}) does not fit {}",
            cfg.context_tokens
        );
    }

    /// The half of the guess the endpoint can correct. An endpoint that names
    /// the other parameter in its refusal is telling us which one it takes, and
    /// re-learning that on every call would mean one wasted 400 per judge call
    /// forever.
    #[tokio::test]
    async fn a_ceiling_refused_by_name_is_retried_under_the_other_and_remembered() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                "Unsupported parameter: 'max_tokens' is not supported with this model. \
                 Use 'max_completion_tokens' instead.",
            ))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices":[{"message":{"content":"the answer"}}]
            })))
            .mount(&server)
            .await;

        // No `reasoning_effort`, so the guess is `max_tokens` — and wrong.
        let completer = HttpCompleter::for_judging(&synthesize_cfg(server.uri()));
        assert_eq!(completer.complete("s", "u").await.unwrap(), "the answer");

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2, "the refusal was not retried");
        let retry: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert_eq!(retry["max_completion_tokens"].as_u64(), Some(2048));
        assert!(
            retry.get("max_tokens").is_none(),
            "the retry carried both names: {retry}"
        );

        // And the next call starts where the first one left off.
        completer.complete("s", "u").await.unwrap();
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 3, "the second call was retried again");
        let second: serde_json::Value = serde_json::from_slice(&requests[2].body).unwrap();
        assert_eq!(second["max_completion_tokens"].as_u64(), Some(2048));
    }

    /// What the 400 taught belongs to the server, not to the role that paid for
    /// it. `ask`, `judge`, `link_judge`, `vision` and `synthesize` are five
    /// endpoints that may all be one address, and the two judges always are — so
    /// a name learned per endpoint means the same refusal bought over and over.
    #[tokio::test]
    async fn the_name_a_400_teaches_is_shared_by_every_role_on_that_server() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                "Unsupported parameter: 'max_tokens' is not supported with this model. \
                 Use 'max_completion_tokens' instead.",
            ))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices":[{"message":{"content":"the answer"}}]
            })))
            .mount(&server)
            .await;

        // Both built up front, the way the core builds them at startup.
        let cfg = synthesize_cfg(server.uri());
        let dedupe = HttpCompleter::for_judging(&cfg);
        let link = HttpCompleter::for_link_judging(&cfg);

        dedupe.complete("s", "u").await.unwrap();
        link.complete("s", "u").await.unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests.len(),
            3,
            "the link judge re-bought a refusal the dedupe judge had already paid for"
        );
        let last: serde_json::Value = serde_json::from_slice(&requests[2].body).unwrap();
        assert_eq!(last["max_completion_tokens"].as_u64(), Some(2048));
        assert!(last.get("max_tokens").is_none(), "{last}");
    }

    /// The cell is shared per server, so a role that only *guessed* a name must
    /// not overwrite one the operator gave. The shipped example config puts
    /// `infer.ask` and `infer.synthesize` on one URL and one model, and the core
    /// builds ask before the judges — so an explicit `ask.ceiling_param` used to
    /// be replaced by synthesize's guess. Against an endpoint that ignores the
    /// field it does not know, that leaves ask with no ceiling at all.
    ///
    /// Both orders, because "whichever was built last" is exactly the bug.
    #[tokio::test]
    async fn an_explicit_ceiling_name_is_not_overwritten_by_another_role_s_guess() {
        for ask_first in [true, false] {
            let server = echoing_server("a prose answer").await;

            let mut ask = ask_cfg(server.uri());
            ask.ceiling_param = Some(CeilingParam::MaxTokens);
            let mut synth = synthesize_cfg(server.uri());
            // One server, one model: the same endpoint by every measure that
            // decides how a request is read.
            synth.model = ask.model.clone();
            synth.reasoning_effort = Some("high".into());
            assert!(synth.ceiling_param.is_none(), "synthesize only guesses");

            let asker = if ask_first {
                let a = HttpCompleter::new(&ask);
                let _ = HttpCompleter::for_judging(&synth);
                a
            } else {
                let _ = HttpCompleter::for_judging(&synth);
                HttpCompleter::new(&ask)
            };
            asker.answer("s", "u", 100).await.unwrap();

            let body = sent_body(&server).await;
            assert_eq!(
                body["max_tokens"].as_u64(),
                Some(100),
                "ask built {}: the configured name lost to synthesize's guess: {body}",
                if ask_first { "first" } else { "second" }
            );
            assert!(body.get("max_completion_tokens").is_none(), "{body}");
        }
    }

    /// A prompt too large for its window is a permanent failure and has to
    /// settle as one. Clamping the ceiling to 1 instead turns it into a 200
    /// carrying an empty reply — indistinguishable from a transient failure, so
    /// the sweep re-sends the same impossible request forever.
    #[tokio::test]
    async fn a_prompt_that_cannot_fit_its_window_is_refused_rather_than_clamped() {
        let server = echoing_server(r#"{"verdict":{"relation":"distinct"}}"#).await;
        let cfg = synthesize_cfg(server.uri());
        // 8192 tokens of window; 3.5 chars to the token, so this is well past it.
        let huge = "x".repeat(cfg.context_tokens * 4);

        let e = HttpCompleter::for_judging(&cfg)
            .complete("s", &huge)
            .await
            .expect_err("an unanswerable prompt was sent anyway");

        assert!(
            matches!(e, Error::InferenceRejected { .. }),
            "a permanent failure was reported as something to retry: {e}"
        );
        assert!(!e.retryable(), "{e}");
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "the call went out despite having no room for a reply"
        );
    }

    /// A 400 about anything else is not a ceiling-name problem, and retrying it
    /// under the other name would turn one permanent rejection into two.
    #[tokio::test]
    async fn a_refusal_that_is_not_about_the_ceiling_is_not_retried() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_string("context length exceeded"))
            .mount(&server)
            .await;

        let e = HttpCompleter::for_judging(&synthesize_cfg(server.uri()))
            .complete("s", "u")
            .await
            .unwrap_err();
        assert!(matches!(e, Error::InferenceRejected { .. }), "{e}");
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "an unrelated 400 was retried under the other ceiling name"
        );
    }

    /// A reasoning model bills its thinking against the ceiling, so a low
    /// ceiling and a high effort can be spent before the message content starts.
    /// That comes back as an empty reply, which read as a transport fault and
    /// sent the operator looking at the wrong thing.
    #[tokio::test]
    async fn an_empty_reply_at_the_ceiling_says_the_ceiling_was_spent() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices":[{"message":{}, "finish_reason":"length"}]
            })))
            .mount(&server)
            .await;

        let e = HttpCompleter::for_judging(&synthesize_cfg(server.uri()))
            .complete("s", "u")
            .await
            .unwrap_err();
        assert!(
            e.to_string().contains("max_output_tokens"),
            "the error names nothing an operator can act on: {e}"
        );
    }

    /// What `finish_reason` is for: a reply the ceiling stopped is not a reply
    /// the model finished, and only the caller knows what that costs.
    #[tokio::test]
    async fn a_reply_stopped_by_the_ceiling_is_reported_as_truncated() {
        for (reason, want) in [("length", true), ("stop", false)] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/chat/completions"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices":[{"message":{"content":"half an ans"}, "finish_reason": reason}]
                })))
                .mount(&server)
                .await;

            let out = HttpCompleter::new(&ask_cfg(server.uri()))
                .answer("s", "u", 512)
                .await
                .unwrap();
            assert_eq!(out.truncated, want, "finish_reason {reason:?} read wrong");
        }
    }

    /// The ceiling a caller asks for is a maximum, not an instruction: `ask`
    /// derives it from what its prompt actually cost, and the role's own
    /// configured ceiling still bounds it.
    #[tokio::test]
    async fn a_call_may_ask_for_less_room_than_the_role_allows_but_not_more() {
        for (asked, want) in [(256_usize, 256_u64), (99_999, 1024)] {
            let server = echoing_server("an answer").await;
            HttpCompleter::new(&ask_cfg(server.uri()))
                .answer("s", "u", asked)
                .await
                .unwrap();
            assert_eq!(
                sent_body(&server).await["max_tokens"].as_u64(),
                Some(want),
                "a call asking for {asked} sent the wrong ceiling"
            );
        }
    }

    /// The link judge shares an endpoint with the duplicate judge and used to
    /// share its response format too, which no link verdict can satisfy.
    #[tokio::test]
    async fn each_judge_sends_the_schema_of_the_question_it_is_asking() {
        for (label, schema_name) in [("dedupe", "verdict"), ("link", "link")] {
            let server = echoing_server(r#"{"verdict":{"relation":"distinct"}}"#).await;
            let cfg = synthesize_cfg(server.uri());
            let judge = match label {
                "dedupe" => HttpCompleter::for_judging(&cfg),
                _ => HttpCompleter::for_link_judging(&cfg),
            };
            judge.complete("s", "u").await.unwrap();

            let sent = sent_body(&server).await;
            assert_eq!(
                sent["response_format"]["json_schema"]["name"].as_str(),
                Some(schema_name),
                "the {label} judge sent another judge's schema"
            );
            let relations =
                &sent["response_format"]["json_schema"]["schema"]["properties"]["verdict"];
            let allows_related = relations.to_string().contains("unrelated");
            assert_eq!(
                allows_related,
                label == "link",
                "the {label} judge's grammar covers the wrong set of relations"
            );
        }
    }

    #[tokio::test]
    async fn upstream_error_bodies_are_truncated_before_they_reach_a_job_row() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .respond_with(ResponseTemplate::new(500).set_body_string("x".repeat(100_000)))
            .mount(&server)
            .await;
        let e = HttpEmbedder::new(&embed_cfg(server.uri()))
            .embed_raw(&["x".into()])
            .await
            .unwrap_err();
        assert!(
            e.to_string().len() < 600,
            "error string was {} chars",
            e.to_string().len()
        );
    }

    #[tokio::test]
    async fn the_describer_sends_the_image_as_a_data_url_beside_the_context() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "# Whiteboard\n\n- item"}}]
            })))
            .mount(&server)
            .await;
        let cfg = vision_cfg(Some(format!("{}/v1", server.uri())));
        let model = cfg.model.clone();
        let d = HttpDescriber::new(&cfg, Some(&synthesize_cfg("http://unused".into())));
        let out = d
            .describe(b"\xFF\xD8jpegbytes", "Photo taken 2026-08-09")
            .await
            .unwrap();
        assert_eq!(out, "# Whiteboard\n\n- item");

        let req = &server.received_requests().await.unwrap()[0];
        assert_eq!(req.headers.get("authorization").unwrap(), "Bearer k");
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(body["model"], model);
        // Every completion carries a ceiling, this one most of all: what comes
        // back is stored as a corpus and segmented from there.
        assert_eq!(body["max_tokens"].as_u64(), Some(2048));
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], prompt::DESCRIBE_SYSTEM);
        let parts = body["messages"][1]["content"].as_array().unwrap();
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "Photo taken 2026-08-09");
        assert_eq!(parts[1]["type"], "image_url");
        let url = parts[1]["image_url"]["url"].as_str().unwrap();
        assert!(url.starts_with("data:image/jpeg;base64,"), "{url}");
        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(url.trim_start_matches("data:image/jpeg;base64,"))
            .unwrap();
        assert_eq!(decoded, b"\xFF\xD8jpegbytes");
    }

    /// A vision role without a `base_url` *is* the synthesize endpoint, so it has
    /// to guess the ceiling's name the same way that role does. Inheriting only
    /// the explicit `ceiling_param` left the two disagreeing whenever synthesize
    /// set `reasoning_effort` and nothing else — and against the silent endpoint
    /// that matters on, the describer's ceiling is the one that gets dropped.
    #[tokio::test]
    async fn a_borrowed_vision_endpoint_inherits_the_guess_and_not_only_the_setting() {
        let server = echoing_server("# Whiteboard").await;
        let mut synth = synthesize_cfg(server.uri());
        synth.reasoning_effort = Some("high".into());
        assert!(synth.ceiling_param.is_none(), "nothing explicit to inherit");

        HttpDescriber::new(&vision_cfg(None), Some(&synth))
            .describe(b"x", "")
            .await
            .unwrap();

        let body = sent_body(&server).await;
        assert_eq!(body["max_completion_tokens"].as_u64(), Some(2048));
        assert!(
            body.get("max_tokens").is_none(),
            "the describer guessed a different name than the synthesizer sharing its server: {body}"
        );
    }

    /// The condition on that inheritance: a role with its own address is a
    /// different server, and how one server reads a request says nothing about
    /// how another does.
    #[tokio::test]
    async fn a_vision_role_with_its_own_endpoint_inherits_neither() {
        let server = echoing_server("# Whiteboard").await;
        let mut synth = synthesize_cfg("http://unused".into());
        synth.reasoning_effort = Some("high".into());

        HttpDescriber::new(&vision_cfg(Some(server.uri())), Some(&synth))
            .describe(b"x", "")
            .await
            .unwrap();

        let body = sent_body(&server).await;
        assert_eq!(body["max_tokens"].as_u64(), Some(2048));
        assert!(body.get("max_completion_tokens").is_none(), "{body}");
    }

    #[tokio::test]
    async fn a_describer_error_is_an_inference_error_for_the_vision_role() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503).set_body_string("busy"))
            .mount(&server)
            .await;
        let mut cfg = vision_cfg(Some(server.uri()));
        cfg.api_key = None;
        let d = HttpDescriber::new(&cfg, Some(&synthesize_cfg("http://unused".into())));
        let e = d.describe(b"x", "").await.unwrap_err();
        assert!(matches!(e, Error::Inference { role: "vision", .. }), "{e}");
        assert!(e.retryable());
    }

    // ── streaming ────────────────────────────────────────────────────────────

    fn completer_against(uri: &str) -> HttpCompleter {
        HttpCompleter::new(&ask_cfg(uri.to_string()))
    }

    /// An SSE server that hands the whole body over at once. The frames are
    /// what is under test here; [`SseBuffer`] owns the split-read case.
    async fn streaming_server(body: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;
        server
    }

    /// Endpoints disagree on the field name for reasoning tokens: llama.cpp and
    /// vLLM send `reasoning_content`, others send `reasoning`. Both must land in
    /// the same place or the thinking silently vanishes on half of them.
    #[test]
    fn a_delta_carries_reasoning_under_either_field_name() {
        let a =
            parse_sse_line(r#"data: {"choices":[{"delta":{"reasoning_content":"hm"}}]}"#).unwrap();
        let b = parse_sse_line(r#"data: {"choices":[{"delta":{"reasoning":"hm"}}]}"#).unwrap();
        assert_eq!(a.reasoning.as_deref(), Some("hm"));
        assert_eq!(b.reasoning.as_deref(), Some("hm"));
    }

    /// The sentinel ends the stream and is not a chunk.
    #[test]
    fn the_done_sentinel_is_not_a_chunk() {
        assert!(parse_sse_line("data: [DONE]").is_none());
        assert!(parse_sse_line("").is_none());
        assert!(parse_sse_line(": keep-alive").is_none());
    }

    /// Truncation is still detectable, now from the final chunk rather than the
    /// whole response. Without it an answer cut off mid-sentence is
    /// indistinguishable from a complete one.
    #[test]
    fn a_finish_reason_of_length_is_read_from_the_last_chunk() {
        let c =
            parse_sse_line(r#"data: {"choices":[{"delta":{},"finish_reason":"length"}]}"#).unwrap();
        assert_eq!(c.finish_reason.as_deref(), Some("length"));
    }

    /// A JSON object can be split across two TCP reads. A parser that assumes
    /// one read is one line loses tokens on exactly the long answers streaming
    /// exists for, and does it intermittently. The split is placed mid-object
    /// so a per-read parser cannot recover it.
    #[test]
    fn a_data_line_split_across_two_reads_is_reassembled() {
        let mut b = SseBuffer::default();
        assert!(
            b.push(br#"data: {"choices":[{"delta":{"con"#).is_empty(),
            "half an object is not a frame yet"
        );
        let got = b.push("tent\":\"hello\"}}]}\n\ndata: [DONE]\n\n".as_bytes());
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].content.as_deref(), Some("hello"));
    }

    /// The buffer holds bytes rather than text because a multi-byte character
    /// splits across reads too, and decoding each read on its own turns it into
    /// replacement characters — silent corruption of every non-English answer.
    #[test]
    fn a_character_split_across_two_reads_survives_whole() {
        let mut b = SseBuffer::default();
        let frame = "data: {\"choices\":[{\"delta\":{\"content\":\"Größe\"}}]}\n".as_bytes();
        let cut = frame.iter().position(|c| *c == 0xC3).unwrap() + 1;
        assert!(b.push(&frame[..cut]).is_empty());
        let got = b.push(&frame[cut..]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].content.as_deref(), Some("Größe"));
    }

    /// The whole point: the caller sees the answer in pieces, and the pieces
    /// are the answer.
    #[tokio::test]
    async fn a_streamed_answer_arrives_as_deltas_and_as_a_completion() {
        let server = streaming_server(
            "data: {\"choices\":[{\"delta\":{\"content\":\"he\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"content\":\"llo\"}}]}\n\n\
             data: [DONE]\n\n",
        )
        .await;

        let c = completer_against(&server.uri());
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let done = c.answer_streaming("s", "u", 64, tx).await.unwrap();
        let mut got = String::new();
        while let Some(Delta::Token(t)) = rx.recv().await {
            got.push_str(&t);
        }
        assert_eq!(got, "hello");
        assert_eq!(done.text, "hello");
        assert!(!done.truncated);
        assert_eq!(sent_body(&server).await["stream"], serde_json::json!(true));
    }

    /// Reasoning is delivered apart from the answer and is no part of it: the
    /// page dims it, and nothing downstream reads it as text the model wrote.
    #[tokio::test]
    async fn streamed_reasoning_is_delivered_apart_from_the_answer() {
        let server = streaming_server(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"content\":\"said\"}}]}\n\n\
             data: [DONE]\n\n",
        )
        .await;

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let done = completer_against(&server.uri())
            .answer_streaming("s", "u", 64, tx)
            .await
            .unwrap();
        let mut deltas = Vec::new();
        while let Some(d) = rx.recv().await {
            deltas.push(d);
        }
        assert!(
            matches!(&deltas[..], [Delta::Reasoning(r), Delta::Token(t)] if r == "think" && t == "said"),
            "{deltas:?}"
        );
        assert_eq!(done.text, "said");
    }

    /// The ceiling still stops the model, and the caller still has to be told:
    /// on a stream the only place that says so is the last frame.
    #[tokio::test]
    async fn a_streamed_reply_stopped_by_the_ceiling_is_reported_as_truncated() {
        for (reason, want) in [("length", true), ("stop", false)] {
            let server = streaming_server(&format!(
                "data: {{\"choices\":[{{\"delta\":{{\"content\":\"half\"}}}}]}}\n\n\
                 data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"{reason}\"}}]}}\n\n\
                 data: [DONE]\n\n"
            ))
            .await;
            let (tx, _rx) = tokio::sync::mpsc::channel(8);
            let done = completer_against(&server.uri())
                .answer_streaming("s", "u", 64, tx)
                .await
                .unwrap();
            assert_eq!(done.truncated, want, "finish_reason {reason:?} read wrong");
        }
    }

    /// A reader that closed its tab must not fail a call whose answer is still
    /// being recorded, and must not shorten it either.
    #[tokio::test]
    async fn a_receiver_dropped_mid_stream_neither_fails_nor_shortens_the_call() {
        let server = streaming_server(
            "data: {\"choices\":[{\"delta\":{\"content\":\"he\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"content\":\"llo\"}}]}\n\n\
             data: [DONE]\n\n",
        )
        .await;
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        drop(rx);
        let done = completer_against(&server.uri())
            .answer_streaming("s", "u", 64, tx)
            .await
            .unwrap();
        assert_eq!(done.text, "hello");
    }

    /// A reasoning model can spend the whole ceiling before the answer starts.
    /// An empty stream is a failed call, and the message has to say which of
    /// the two it was — the buffered path draws the same distinction.
    #[tokio::test]
    async fn a_stream_that_carried_no_content_is_an_error_that_says_why() {
        let server = streaming_server(
            "data: {\"choices\":[{\"delta\":{\"reasoning\":\"...\"},\"finish_reason\":\"length\"}]}\n\n\
             data: [DONE]\n\n",
        )
        .await;
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let Err(e) = completer_against(&server.uri())
            .answer_streaming("s", "u", 64, tx)
            .await
        else {
            panic!("a stream with no content answered nothing and reported success");
        };
        assert!(format!("{e}").contains("output ceiling was spent"), "{e}");
    }

    /// An endpoint or proxy that ignores `stream: true` answers with the plain
    /// completion object. Every door goes through the streaming call now, so
    /// treating that as a broken stream would break the API and MCP doors on
    /// exactly the servers they used to work against.
    #[tokio::test]
    async fn a_whole_completion_sent_back_to_a_streaming_call_is_still_the_answer() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "whole"}, "finish_reason": "stop"}]
            })))
            .mount(&server)
            .await;
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let done = completer_against(&server.uri())
            .answer_streaming("s", "u", 64, tx)
            .await
            .unwrap();
        assert_eq!(done.text, "whole");
        assert!(!done.truncated);
        // The reader still gets the answer, all at once.
        assert!(matches!(rx.recv().await, Some(Delta::Token(t)) if t == "whole"));
    }

    /// A server that fails after the headers went out says so in a frame of
    /// its own — llama.cpp on a prompt past the context, most proxies on an
    /// upstream fault. That frame has no `choices`; dropping it leaves the
    /// operator with "no message content" and the log with no reason.
    #[tokio::test]
    async fn an_error_frame_mid_stream_is_the_reason_the_call_reports() {
        let server = streaming_server(
            "data: {\"error\":{\"message\":\"context size exceeded\",\"code\":400}}\n\n",
        )
        .await;
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let Err(e) = completer_against(&server.uri())
            .answer_streaming("s", "u", 64, tx)
            .await
        else {
            panic!("a stream that carried only an error reported success");
        };
        assert!(format!("{e}").contains("context size exceeded"), "{e}");
    }

    /// The two shapes an error frame comes in.
    #[test]
    fn an_error_frame_parses_under_either_shape() {
        let a = parse_sse_line(r#"data: {"error":{"message":"boom"}}"#).unwrap();
        assert_eq!(a.error.as_deref(), Some("boom"));
        let b = parse_sse_line(r#"data: {"error":"boom"}"#).unwrap();
        assert_eq!(b.error.as_deref(), Some("boom"));
        let c = parse_sse_line(r#"data: {"choices":[{"delta":{"content":"x"}}]}"#).unwrap();
        assert!(c.error.is_none());
    }

    /// The streamed body is the buffered one plus `stream`: the ceiling under
    /// the name this endpoint takes it under, and nothing else changed.
    #[tokio::test]
    async fn a_streaming_call_sends_the_same_body_the_buffered_one_does() {
        let server =
            streaming_server("data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n").await;
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        completer_against(&server.uri())
            .answer_streaming("s", "u", 99_999, tx)
            .await
            .unwrap();
        let body = sent_body(&server).await;
        assert_eq!(body["max_tokens"].as_u64(), Some(1024), "{body}");
        assert_eq!(body["messages"][0]["content"], serde_json::json!("s"));
        assert_eq!(body["temperature"], serde_json::json!(0.3));
    }
}
