use super::{
    Completer, Completion, Delta, Describer, Embedder, ProposedArtifact, Reranker, SegmentInput,
    SynthesisBudget, Synthesizer, Transcriber, prompt,
};
use crate::config::{
    AskRole, CeilingParam, EmbedRole, RerankRole, RerankStyle, SynthesizeRole, TierConfig,
    TranscribeRole,
};
use crate::error::{Error, Result};
use async_trait::async_trait;
use serde_json::json;

/// Per role, so a slow model widens only its own timeout; a local reasoning model can take minutes per window.
fn client(timeout_secs: u64) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

fn url(base: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

/// A 4xx meaning the request itself is wrong. 408, 429, 401, 403 and 404 describe the endpoint and heal, so stay retryable.
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

/// "Not now": 429 is a limiter, 503 a model still loading. Decides the status shown and whether search retries.
pub fn busy_upstream_status(status: reqwest::StatusCode) -> bool {
    use reqwest::StatusCode as S;
    matches!(status, S::TOO_MANY_REQUESTS | S::SERVICE_UNAVAILABLE)
}

pub(crate) struct Endpoint {
    client: reqwest::Client,
    base_url: String,
    model: String,
    api_key: Option<String>,
    role: &'static str,
    /// Corrected in place on the first rejection and shared per server ([`LEARNED_CEILING`]): one 400 per server per process.
    ceiling_param: std::sync::Arc<AtomicCeilingParam>,
}

/// `explicit`: the operator named it, so it outranks an inferred name. See [`learned_ceiling`].
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

    /// For a name the operator gave or a 400 taught, which an inferred name cannot unseat.
    fn set_authoritative(&self, p: CeilingParam) {
        self.set(p);
        self.explicit
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// `truncated` is `finish_reason == "length"`; only the caller knows what a cut-off reply costs.
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
            // Unshared until a role that sends a ceiling claims the server's cell; embed and rerank send none.
            ceiling_param: std::sync::Arc::new(AtomicCeilingParam::new(CeilingParam::MaxTokens)),
        }
    }

    /// Roles are built in a fixed order, so the shared cell is not simply overwritten: an explicit name outranks
    /// an inferred one, and between equals the first wins with a warning. See [`learned_ceiling`].
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

    /// Status already judged, so a streaming caller need not buffer the body.
    async fn post(&self, path: &str, body: serde_json::Value) -> Result<reqwest::Response> {
        self.send(self.client.post(url(&self.base_url, path)).json(&body))
            .await
    }

    /// For transcription, routed here so speech failures are judged and classified like every other role's.
    async fn post_multipart(
        &self,
        path: &str,
        form: reqwest::multipart::Form,
    ) -> Result<reqwest::Response> {
        self.send(self.client.post(url(&self.base_url, path)).multipart(form))
            .await
    }

    async fn send(&self, mut req: reqwest::RequestBuilder) -> Result<reqwest::Response> {
        let role = self.role;
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
            // Truncate: an error page can be megabytes, and this lands in a job's last_error column.
            let detail: String = body.chars().take(400).collect();
            let detail = format!("HTTP {status}: {detail}");
            return Err(if permanent_upstream_status(status) {
                Error::InferenceRejected { role, detail }
            } else if busy_upstream_status(status) {
                Error::InferenceBusy { role, detail }
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

    /// The ceiling's parameter name is learned per endpoint: a 400 naming it is retried once under the other name.
    /// Only the name is retried; any other 400 stays a 400.
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

    /// Separate because the streaming path needs it for an endpoint that answers a stream request with a plain completion.
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

    /// An unmarked cut-off reply would be read as complete. `empty` describes a reply with no content that was not cut off.
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
            // An empty reply at `length` looks like a transport fault: a reasoning model can spend the ceiling thinking.
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

/// Keyed by address and model; `ask`, the judges, `vision` and `synthesize` may all be one server.
type CeilingRegistry =
    std::collections::HashMap<(String, String), std::sync::Arc<AtomicCeilingParam>>;

static LEARNED_CEILING: std::sync::LazyLock<std::sync::Mutex<CeilingRegistry>> =
    std::sync::LazyLock::new(Default::default);

/// An explicit name replaces an inferred one; otherwise the first stays and a disagreement is reported for the operator.
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
        // Agreement promotes an inferred name to explicit, so a later inferring role cannot unseat it.
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

/// The two names share no substring, so naming the other one is unambiguous (OpenAI's own message does).
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

fn error_detail(e: &Error) -> &str {
    match e {
        Error::Inference { detail, .. } | Error::InferenceRejected { detail, .. } => detail,
        _ => "",
    }
}

pub struct HttpSynthesizer {
    ep: Endpoint,
    budget: SynthesisBudget,
    max_artifact_tokens: usize,
    reasoning_effort: Option<String>,
    structured_output: bool,
    /// The real tokenizer, not the estimator: on Cyrillic the estimate undercounted windows by ~40 % and drew 400s.
    counter: std::sync::Arc<crate::infer::budget::TokenCounter>,
}

/// The text of every `content`: everything the endpoint counts that this side controls.
fn message_tokens(
    messages: &serde_json::Value,
    counter: &crate::infer::budget::TokenCounter,
) -> usize {
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
                    neighbors: cfg.context_neighbor_tokens,
                },
            },
            max_artifact_tokens: 1024,
            reasoning_effort: cfg.reasoning_effort.clone(),
            structured_output: cfg.structured_output,
            counter: Default::default(),
        }
    }

    /// Without it this side uses the estimator.
    pub fn with_counter(mut self, c: std::sync::Arc<crate::infer::budget::TokenCounter>) -> Self {
        self.counter = c;
        self
    }

    /// Set from `embed.max_input_tokens * 0.8` so the embedder never receives an oversized chunk.
    pub fn with_max_artifact_tokens(mut self, n: usize) -> Self {
        self.max_artifact_tokens = n;
        self
    }

    /// `schema` goes as a `json_schema` response format: a decoding constraint is the only reliable fix for small-model JSON.
    async fn chat(
        &self,
        messages: serde_json::Value,
        schema: Option<(&str, serde_json::Value)>,
    ) -> Result<String> {
        let spent = message_tokens(&messages, &self.counter);
        let mut body = json!({
            "messages": messages,
            "temperature": 0.2,
        });
        if let Some(effort) = &self.reasoning_effort {
            body["reasoning_effort"] = json!(effort);
        }
        if let Some((name, value)) = schema.filter(|_| self.structured_output) {
            body["response_format"] = response_format(name, value);
        }
        // The repair call carries the first reply too, so the full ceiling again would be a permanent 400.
        // Truncation is not an error: `parse_response` salvages what a cut-off list got right.
        let ceiling = crate::infer::budget::ceiling_for_prompt(
            self.budget.context_tokens,
            spent,
            self.budget.max_output_tokens,
        );
        Ok(self.ep.chat(body, Some(ceiling)).await?.text)
    }
}

impl HttpSynthesizer {
    async fn run_segment(&self, input: SegmentInput<'_>) -> Result<crate::infer::SegmentReply> {
        let judged = input.judge.is_some();
        // Fixed for the repair too: a different language would be a second system prompt over the first one's reply.
        let system = prompt::synthesizer_system(input.lang);
        let user = prompt::user_prompt(
            input.core,
            1,
            self.max_artifact_tokens,
            input.context,
            input.judge,
        );
        let schema = || {
            if judged {
                ("judged_artifacts", prompt::judged_artifacts_schema())
            } else {
                ("artifacts", prompt::artifacts_schema())
            }
        };
        let parse = |body: &str| -> Result<crate::infer::SegmentReply> {
            if judged {
                prompt::parse_judged_response(body)
            } else {
                Ok(crate::infer::SegmentReply {
                    artifacts: prompt::parse_response(body)?,
                    judgement: None,
                })
            }
        };
        let first = self
            .chat(
                json!([
                    {"role":"system","content": system},
                    {"role":"user","content": user}
                ]),
                Some(schema()),
            )
            .await?;

        match parse(&first) {
            Ok(reply) => Ok(reply),
            Err(e) => {
                // One repair attempt; beyond that the caller falls back to a structural split.
                tracing::warn!(error = %e, "synthesizer returned unparsable output; repairing");
                let repair = prompt::repair_prompt(&first, &e.to_string());
                let messages = json!([
                    {"role":"system","content": system},
                    {"role":"user","content": user},
                    {"role":"assistant","content": first},
                    {"role":"user","content": repair}
                ]);

                // The repair prompt includes the whole reply, so it may not fit; skipping it falls back as a failed repair would.
                if crate::infer::budget::checked_ceiling_for_prompt(
                    self.budget.context_tokens,
                    message_tokens(&messages, &self.counter),
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

                let second = self.chat(messages, Some(schema())).await?;
                parse(&second)
            }
        }
    }
}

#[async_trait]
impl Synthesizer for HttpSynthesizer {
    async fn segment(&self, input: SegmentInput<'_>) -> Result<Vec<ProposedArtifact>> {
        Ok(self.run_segment(input).await?.artifacts)
    }

    async fn segment_judged(&self, input: SegmentInput<'_>) -> Result<crate::infer::SegmentReply> {
        self.run_segment(input).await
    }

    fn budget(&self) -> SynthesisBudget {
        self.budget
    }

    async fn title(
        &self,
        text: &str,
        artifact_titles: &[String],
        lang: crate::infer::lang::Lang,
    ) -> Result<Option<String>> {
        let out = self
            .chat(
                json!([
                    {"role":"system","content": prompt::title_system(lang)},
                    {"role":"user","content": prompt::title_prompt(text, artifact_titles)}
                ]),
                None,
            )
            .await?;
        let t = out.trim().trim_matches('"').trim();
        Ok((!t.is_empty()).then(|| t.chars().take(120).collect()))
    }
}

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
        // Proxies before llama.cpp-style servers forward an absent `encoding_format` as null, which the backend rejects.
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
        // There is no standard rerank endpoint, so the style is configured rather than guessed.
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

/// `strict` makes the endpoint compile the schema into a grammar rather than treat it as a hint.
fn response_format(name: &str, schema: serde_json::Value) -> serde_json::Value {
    json!({
        "type": "json_schema",
        "json_schema": {"name": name, "strict": true, "schema": schema}
    })
}

fn content_type(res: &reqwest::Response) -> &str {
    res.headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

/// Only a reply calling itself JSON (by media type) is taken as one document; sloppy content-types stay streams.
fn is_json_document(res: &reqwest::Response) -> bool {
    content_type(res)
        .split(';')
        .next()
        .is_some_and(|t| t.trim().eq_ignore_ascii_case("application/json"))
}

#[derive(Debug, Default)]
pub(crate) struct SseChunk {
    pub content: Option<String>,
    pub reasoning: Option<String>,
    pub finish_reason: Option<String>,
    /// Sent as a frame once the 200 has gone out (llama.cpp, vLLM, proxies); it has no `choices`.
    pub error: Option<String>,
}

/// `None` for blank lines, comments and the `[DONE]` sentinel.
pub(crate) fn parse_sse_line(line: &str) -> Option<SseChunk> {
    let payload = line.strip_prefix("data:")?.trim();
    if payload.is_empty() || payload == "[DONE]" {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    if let Some(err) = v.get("error") {
        // `{"error":{"message":..}}` is OpenAI's shape, `{"error":"..."}` some servers'; else the whole object.
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
        // llama.cpp and vLLM say `reasoning_content`, others `reasoning`.
        reasoning: s(delta, "reasoning_content").or_else(|| s(delta, "reasoning")),
        finish_reason: choice
            .get("finish_reason")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        error: None,
    })
}

/// A read can split a JSON object or a UTF-8 character, so bytes accumulate and only whole lines leave.
#[derive(Default)]
pub(crate) struct SseBuffer {
    buf: Vec<u8>,
}

impl SseBuffer {
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

    /// A truncated connection is where the tokens already received are worth keeping.
    pub(crate) fn finish(&mut self) -> Vec<SseChunk> {
        let rest = std::mem::take(&mut self.buf);
        parse_sse_line(String::from_utf8_lossy(&rest).trim_end_matches('\r'))
            .into_iter()
            .collect()
    }
}

pub struct HttpCompleter {
    ep: Endpoint,
    context_tokens: usize,
    /// Inside a schema-constrained JSON string EOS is masked, so the model cannot stop; one judge call ran to ~190 KB.
    max_output_tokens: usize,
    reasoning_effort: Option<String>,
    /// `None` for ask, whose answer is prose for a person.
    response_schema: Option<(&'static str, serde_json::Value)>,
    /// See `HttpSynthesizer::counter`: whole artifacts go into judge calls.
    counter: std::sync::Arc<crate::infer::budget::TokenCounter>,
}

impl HttpCompleter {
    pub fn with_counter(mut self, c: std::sync::Arc<crate::infer::budget::TokenCounter>) -> Self {
        self.counter = c;
        self
    }

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
            counter: Default::default(),
        }
    }

    /// One body for buffered and streamed calls, so they cannot differ in ceiling name, effort or schema.
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

    /// A caller may ask for less than configured, never more.
    fn clamped(&self, ceiling: usize) -> usize {
        ceiling.min(self.max_output_tokens).max(1)
    }

    /// On the synthesize endpoint: background work like segmentation, kept off ask's interactive endpoint.
    pub fn for_judging(cfg: &SynthesizeRole) -> Self {
        Self::judging(cfg, ("verdict", prompt::dedupe_schema()))
    }

    /// Separate from `for_judging` because the struct carries the response format; the dedupe grammar cannot say `related`.
    pub fn for_link_judging(cfg: &SynthesizeRole) -> Self {
        Self::judging(cfg, ("link", prompt::link_schema()))
    }

    pub fn for_gap_naming(cfg: &SynthesizeRole) -> Self {
        Self::judging(cfg, ("gap_label", prompt::gap_label_schema()))
    }

    /// Separate because the dedupe grammar cannot express a reap verdict.
    pub fn for_reaping(cfg: &SynthesizeRole) -> Self {
        Self::judging(cfg, ("reap", prompt::reap_schema()))
    }

    pub fn for_generating(cfg: &SynthesizeRole) -> Self {
        Self::judging(cfg, ("artifact", prompt::generate_schema()))
    }

    /// Separate: it asks for a merged artifact, while the dedupe grammar would demand a `relation` a person already decided.
    pub fn for_pair_synthesis(cfg: &SynthesizeRole) -> Self {
        Self::judging(cfg, ("synthesis", prompt::synthesize_schema()))
    }

    /// A `TierConfig`, not a role: it runs on whichever endpoint `ask.plan_tier` names, resolved by the config layer.
    pub fn for_plan(cfg: &TierConfig) -> Self {
        Self {
            ep: Endpoint::new(
                &cfg.base_url,
                &cfg.model,
                cfg.api_key.as_deref(),
                cfg.timeout_secs,
                "plan",
            )
            .with_ceiling_param(cfg.ceiling_param, cfg.reasoning_effort.as_deref()),
            context_tokens: cfg.context_tokens,
            max_output_tokens: cfg.max_output_tokens,
            reasoning_effort: cfg.reasoning_effort.clone(),
            response_schema: cfg
                .structured_output
                .then_some(("need", prompt::plan_schema())),
            counter: Default::default(),
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
            counter: Default::default(),
        }
    }
}

#[async_trait]
impl Completer for HttpCompleter {
    /// The ceiling is measured against the prompt, since dedupe prompts carry whole artifacts.
    /// No room for a reply is `InferenceRejected`: a ceiling of 1 yields an empty 200 that the sweep retries forever.
    async fn complete(&self, system: &str, user: &str) -> Result<String> {
        let spent = self.counter.count(system) + self.counter.count(user);
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

    /// The body `answer` sends with `stream` added, so the reply is the same.
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
        // An endpoint that ignores `stream` sends the plain completion; that is the reply, delivered as one token.
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
                    // Surfaced as-is: the server's reason, without which this reads as a transport fault.
                    return Err(Error::Inference {
                        role,
                        detail: format!("the stream carried an error: {message}"),
                    });
                }
                if let Some(r) = c.reasoning {
                    // Send errors are ignored: the body is still read to its end so the connection is reusable and the GPU free.
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

        // A stream has no whole-response object, so truncation comes from the last frame with a reason.
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

pub struct HttpDescriber {
    ep: Endpoint,
    /// What comes back is stored and segmented, so an unbounded reply is an unbounded document.
    max_output_tokens: usize,
}

impl HttpDescriber {
    /// Without its own `base_url` vision borrows the synthesize endpoint's address, key and ceiling name.
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
            // Inherits synthesize's `reasoning_effort` too, or the two roles guess different names for one server.
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
        // A truncated description is still stored, with a log line; an empty one is an error from `chat`.
        Ok(self.ep.chat(body, Some(self.max_output_tokens)).await?.text)
    }
}

pub struct HttpTranscriber {
    ep: Endpoint,
    language: Option<String>,
}

impl HttpTranscriber {
    pub fn new(cfg: &TranscribeRole) -> Self {
        Self {
            ep: Endpoint::new(
                &cfg.base_url,
                &cfg.model,
                cfg.api_key.as_deref(),
                cfg.timeout_secs,
                "transcribe",
            ),
            language: cfg.language.clone(),
        }
    }
}

/// whisper.cpp's server and faster-whisper-server choose the demuxer by filename; `audio/mpeg` is why this is a map.
fn audio_extension(mime: &str) -> &str {
    let subtype = mime
        .split(';')
        .next()
        .unwrap_or("")
        .rsplit('/')
        .next()
        .unwrap_or("")
        .trim();
    match subtype {
        "mpeg" => "mp3",
        "x-wav" | "wave" => "wav",
        // No type, or `octet-stream` (an untyped multipart part), is WebM: the one format browsers send unlabelled.
        "" | "octet-stream" => "webm",
        other => other,
    }
}

#[async_trait]
impl Transcriber for HttpTranscriber {
    async fn transcribe(&self, audio: &[u8], mime: &str) -> Result<String> {
        let role = "transcribe";
        let part = reqwest::multipart::Part::bytes(audio.to_vec())
            .file_name(format!("recording.{}", audio_extension(mime)))
            // An empty content-type must not reach `mime_str("")`, which errors.
            .mime_str(
                mime.split(';')
                    .next()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("application/octet-stream"),
            )
            .map_err(|e| Error::Inference {
                role,
                detail: e.to_string(),
            })?;
        let mut form = reqwest::multipart::Form::new()
            .part("file", part)
            .text("model", self.ep.model.clone())
            // `json` is the format every one of these servers agrees on.
            .text("response_format", "json");
        if let Some(l) = &self.language {
            form = form.text("language", l.clone());
        }
        let body: serde_json::Value = self
            .ep
            .post_multipart("audio/transcriptions", form)
            .await?
            .json()
            .await
            .map_err(|e| Error::Inference {
                role,
                detail: e.to_string(),
            })?;
        // Silence is a legitimate empty answer; a reply without `text` is not this client's endpoint.
        body.get("text")
            .and_then(|t| t.as_str())
            .map(|t| t.trim().to_string())
            .ok_or_else(|| Error::Inference {
                role,
                detail: "the reply carried no `text` field".into(),
            })
    }
}

/// Failure is a warning: ingest survives a dead inference endpoint.
pub async fn probe(role: &str, base_url: &str, api_key: Option<&str>) -> bool {
    probe_path(role, base_url, "models", api_key).await
}

/// For a server whose reachability is not checked under `models`.
pub async fn probe_path(role: &str, base_url: &str, path: &str, api_key: Option<&str>) -> bool {
    let c = client(crate::config::DEFAULT_TIMEOUT_SECS);
    let mut req = c.get(url(base_url, path));
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
        // An expired key and a restarting proxy both answer like this, and both heal.
        assert!(!permanent_upstream_status(S::UNAUTHORIZED));
        assert!(!permanent_upstream_status(S::FORBIDDEN));
        assert!(!permanent_upstream_status(S::NOT_FOUND));
        assert!(!permanent_upstream_status(S::INTERNAL_SERVER_ERROR));
        assert!(!permanent_upstream_status(S::BAD_GATEWAY));
    }

    static EMPTY_CONTEXT: crate::infer::context::WindowContext =
        crate::infer::context::WindowContext {
            opening: None,
            before: None,
            after: None,
        };

    fn window(text: &str) -> SegmentInput<'_> {
        SegmentInput {
            lang: crate::infer::lang::Lang::En,
            core: text,
            context: &EMPTY_CONTEXT,
            judge: None,
        }
    }
    use crate::config::{AskRole, EmbedRole, RerankRole, RerankStyle, SynthesizeRole};
    use crate::infer::{Completer, Embedder, Reranker, Synthesizer};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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
            context_neighbor_tokens: 0,
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
            plan: false,
            structured_output: true,
            plan_endpoint: None,
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
        // Only the schema makes a malformed reply ungeneratable for a 9B model.
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
        // A schema leaking onto this call would constrain a one-line title into a JSON object.
        let server = echoing_server("A Good Title").await;
        let t = HttpSynthesizer::new(&synthesize_cfg(server.uri()))
            .title("some text", &[], crate::infer::lang::Lang::En)
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
            plan: false,
            structured_output: true,
            plan_endpoint: None,
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
        // `up_to_n_times` retires the first mock after one hit.
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

    #[tokio::test]
    async fn the_repair_call_still_fits_the_window_it_is_repairing() {
        let server = MockServer::start().await;
        // The repair prompt carries the reply twice, leaving less room than the ceiling.
        // Sized well over the line so the test does not measure the system prompt's length.
        let long_prose = "sorry, here is prose ".repeat(700);
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

        // Half-window ceiling, as in the example config; doubled so the scaffolding itself fits.
        let mut cfg = synthesize_cfg(server.uri());
        cfg.context_tokens *= 2;
        cfg.max_output_tokens = cfg.context_tokens / 2;
        let context = cfg.context_tokens;
        HttpSynthesizer::new(&cfg)
            .segment(window(&"a sentence to segment. ".repeat(60)))
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2, "the repair call was not made");
        let repair: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
        let prompt = message_tokens(&repair["messages"], &Default::default());
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
            .respond_with(ResponseTemplate::new(500))
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
    async fn a_limiter_and_a_loading_model_are_busy_rather_than_broken() {
        for status in [429u16, 503] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/embeddings"))
                .respond_with(ResponseTemplate::new(status))
                .mount(&server)
                .await;
            let e = HttpEmbedder::new(&embed_cfg(server.uri()))
                .embed_raw(&["x".into()])
                .await
                .unwrap_err();
            assert!(
                matches!(e, crate::error::Error::InferenceBusy { role: "embed", .. }),
                "{status} came back as {e}"
            );
            assert!(e.retryable());
            assert_eq!(
                e.status(),
                reqwest::StatusCode::SERVICE_UNAVAILABLE,
                "{status} reached the browser as {}",
                e.status()
            );
        }
    }

    #[test]
    fn busy_is_narrower_than_merely_transient() {
        use reqwest::StatusCode as S;
        assert!(busy_upstream_status(S::TOO_MANY_REQUESTS));
        assert!(busy_upstream_status(S::SERVICE_UNAVAILABLE));
        // Retryable, but waiting is not promised to fix it.
        assert!(!busy_upstream_status(S::INTERNAL_SERVER_ERROR));
        assert!(!busy_upstream_status(S::BAD_GATEWAY));
        assert!(!busy_upstream_status(S::GATEWAY_TIMEOUT));
        assert!(!busy_upstream_status(S::BAD_REQUEST));
    }

    #[tokio::test]
    async fn embedder_sends_float_encoding_and_orders_results_by_index() {
        // A litellm proxy forwards an absent `encoding_format` as null; `index` beats array position.
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
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .and(body_partial_json(serde_json::json!({
                "input": ["title: Recovering | text: run fsck", "title: none | text: bare"]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(two))
            .expect(1)
            .mount(&server)
            .await;
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
        // Writing a mismatched dimension to Qdrant would corrupt the collection.
        let e = HttpEmbedder::new(&embed_cfg(server.uri()))
            .embed_raw(&["x".into()])
            .await
            .unwrap_err();
        assert!(e.to_string().contains("dimension"));
    }

    #[tokio::test]
    async fn embedder_rejects_a_short_batch() {
        let server = MockServer::start().await;
        // Accepting this would pair the wrong vector with the wrong chunk.
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
        // An index outside the batch is dropped rather than panicking.
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
                apply: vec![
                    crate::config::RerankApply::Ask,
                    crate::config::RerankApply::Search,
                ],
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
            plan: false,
            structured_output: true,
            plan_endpoint: None,
        };
        assert_eq!(
            HttpCompleter::new(&cfg).complete("s", "u").await.unwrap(),
            "the answer"
        );
    }

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
                    plan: false,
                    structured_output: true,
                    plan_endpoint: None,
                }),
                _ => HttpCompleter::for_judging(&synthesize_cfg(server.uri())),
            };
            completer.complete("s", "u").await.unwrap();

            let want = if label == "ask" { 1024 } else { 2048 };
            assert_eq!(
                sent_body(&server).await["max_tokens"].as_u64(),
                Some(want),
                "{label} did not send its output ceiling"
            );
        }
    }

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

    #[tokio::test]
    async fn a_judge_prompt_that_fills_the_window_shrinks_its_own_ceiling() {
        let server = echoing_server(r#"{"verdict":{"relation":"distinct"}}"#).await;
        let cfg = synthesize_cfg(server.uri());
        // About 7000 estimated tokens: 2048 of reply does not fit the 8192 window.
        let user = "x".repeat(7000 * 7 / 2);
        HttpCompleter::for_judging(&cfg)
            .complete("s", &user)
            .await
            .unwrap();

        let body = sent_body(&server).await;
        let sent = body["max_tokens"].as_u64().unwrap() as usize;
        let counter = crate::infer::budget::TokenCounter::default();
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

        // No `reasoning_effort`, so the guess is `max_tokens`, and wrong.
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

        completer.complete("s", "u").await.unwrap();
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 3, "the second call was retried again");
        let second: serde_json::Value = serde_json::from_slice(&requests[2].body).unwrap();
        assert_eq!(second["max_completion_tokens"].as_u64(), Some(2048));
    }

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

    #[tokio::test]
    async fn an_explicit_ceiling_name_is_not_overwritten_by_another_role_s_guess() {
        for ask_first in [true, false] {
            let server = echoing_server("a prose answer").await;

            let mut ask = ask_cfg(server.uri());
            ask.ceiling_param = Some(CeilingParam::MaxTokens);
            let mut synth = synthesize_cfg(server.uri());
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

    #[tokio::test]
    async fn a_prompt_that_cannot_fit_its_window_is_refused_rather_than_clamped() {
        let server = echoing_server(r#"{"verdict":{"relation":"distinct"}}"#).await;
        let cfg = synthesize_cfg(server.uri());
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
            .respond_with(ResponseTemplate::new(500).set_body_string("broke"))
            .mount(&server)
            .await;
        let mut cfg = vision_cfg(Some(server.uri()));
        cfg.api_key = None;
        let d = HttpDescriber::new(&cfg, Some(&synthesize_cfg("http://unused".into())));
        let e = d.describe(b"x", "").await.unwrap_err();
        assert!(matches!(e, Error::Inference { role: "vision", .. }), "{e}");
        assert!(e.retryable());
    }

    fn completer_against(uri: &str) -> HttpCompleter {
        HttpCompleter::new(&ask_cfg(uri.to_string()))
    }

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

    #[test]
    fn a_delta_carries_reasoning_under_either_field_name() {
        let a =
            parse_sse_line(r#"data: {"choices":[{"delta":{"reasoning_content":"hm"}}]}"#).unwrap();
        let b = parse_sse_line(r#"data: {"choices":[{"delta":{"reasoning":"hm"}}]}"#).unwrap();
        assert_eq!(a.reasoning.as_deref(), Some("hm"));
        assert_eq!(b.reasoning.as_deref(), Some("hm"));
    }

    #[test]
    fn the_done_sentinel_is_not_a_chunk() {
        assert!(parse_sse_line("data: [DONE]").is_none());
        assert!(parse_sse_line("").is_none());
        assert!(parse_sse_line(": keep-alive").is_none());
    }

    #[test]
    fn a_finish_reason_of_length_is_read_from_the_last_chunk() {
        let c =
            parse_sse_line(r#"data: {"choices":[{"delta":{},"finish_reason":"length"}]}"#).unwrap();
        assert_eq!(c.finish_reason.as_deref(), Some("length"));
    }

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
        assert!(matches!(rx.recv().await, Some(Delta::Token(t)) if t == "whole"));
    }

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

    #[test]
    fn an_error_frame_parses_under_either_shape() {
        let a = parse_sse_line(r#"data: {"error":{"message":"boom"}}"#).unwrap();
        assert_eq!(a.error.as_deref(), Some("boom"));
        let b = parse_sse_line(r#"data: {"error":"boom"}"#).unwrap();
        assert_eq!(b.error.as_deref(), Some("boom"));
        let c = parse_sse_line(r#"data: {"choices":[{"delta":{"content":"x"}}]}"#).unwrap();
        assert!(c.error.is_none());
    }

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

    fn transcribe_cfg(base: String) -> TranscribeRole {
        TranscribeRole {
            base_url: base,
            model: "whisper-1".into(),
            api_key: Some("secret".into()),
            timeout_secs: 60,
            language: None,
        }
    }

    #[tokio::test]
    async fn the_transcriber_posts_the_recording_as_a_file_part() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/audio/transcriptions"))
            .and(header("authorization", "Bearer secret"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"text": " hello  "})),
            )
            .mount(&server)
            .await;

        let out = HttpTranscriber::new(&transcribe_cfg(server.uri()))
            .transcribe(b"\x1a\x45\xdf\xa3fake webm", "audio/webm;codecs=opus")
            .await
            .unwrap();
        // whisper returns a leading space that would land in the search box.
        assert_eq!(out, "hello");

        let reqs = server.received_requests().await.unwrap();
        let body = String::from_utf8_lossy(&reqs[0].body);
        assert!(body.contains("name=\"file\""), "{body}");
        assert!(body.contains("filename=\"recording.webm\""), "{body}");
        assert!(body.contains("whisper-1"), "{body}");
        assert!(
            !body.contains("name=\"language\""),
            "no language is a language to detect, not an empty field: {body}"
        );
    }

    #[tokio::test]
    async fn a_configured_language_is_sent_with_the_recording() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/audio/transcriptions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"text": "ja"})),
            )
            .mount(&server)
            .await;

        let mut cfg = transcribe_cfg(server.uri());
        cfg.language = Some("de".into());
        HttpTranscriber::new(&cfg)
            .transcribe(b"audio", "audio/webm")
            .await
            .unwrap();

        let reqs = server.received_requests().await.unwrap();
        let body = String::from_utf8_lossy(&reqs[0].body);
        assert!(body.contains("name=\"language\""), "{body}");
        assert!(body.contains("de"), "{body}");
    }

    #[tokio::test]
    async fn silence_is_an_answer_and_a_foreign_reply_is_not() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/audio/transcriptions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"text": ""})))
            .mount(&server)
            .await;
        assert_eq!(
            HttpTranscriber::new(&transcribe_cfg(server.uri()))
                .transcribe(b"a", "audio/webm")
                .await
                .unwrap(),
            ""
        );

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/audio/transcriptions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"error": "nope"})),
            )
            .mount(&server)
            .await;
        assert!(
            HttpTranscriber::new(&transcribe_cfg(server.uri()))
                .transcribe(b"a", "audio/webm")
                .await
                .is_err()
        );
    }

    #[test]
    fn the_recording_is_named_after_the_container_it_is_in() {
        assert_eq!(audio_extension("audio/webm;codecs=opus"), "webm");
        assert_eq!(audio_extension("audio/mp4"), "mp4");
        assert_eq!(audio_extension("audio/ogg"), "ogg");
        assert_eq!(audio_extension("audio/mpeg"), "mp3");
        assert_eq!(audio_extension("audio/x-wav"), "wav");
        assert_eq!(audio_extension(""), "webm");
        assert_eq!(audio_extension("application/octet-stream"), "webm");
    }
}
