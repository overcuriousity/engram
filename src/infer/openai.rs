use super::{
    Completer, Completion, Describer, Embedder, ProposedArtifact, Reranker, SegmentInput,
    SynthesisBudget, Synthesizer, prompt,
};
use crate::config::{AskRole, CeilingParam, EmbedRole, RerankRole, RerankStyle, SynthesizeRole};
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
    /// because the correction is worth nothing if every later call re-learns it:
    /// one 400 per process, not one per judge call.
    ceiling_param: AtomicCeilingParam,
}

/// `CeilingParam` behind an atomic, so a `&self` call can record what it
/// learned.
struct AtomicCeilingParam(std::sync::atomic::AtomicBool);

impl AtomicCeilingParam {
    fn new(p: CeilingParam) -> Self {
        Self(std::sync::atomic::AtomicBool::new(
            p == CeilingParam::MaxCompletionTokens,
        ))
    }

    fn get(&self) -> CeilingParam {
        if self.0.load(std::sync::atomic::Ordering::Relaxed) {
            CeilingParam::MaxCompletionTokens
        } else {
            CeilingParam::MaxTokens
        }
    }

    fn set(&self, p: CeilingParam) {
        self.0.store(
            p == CeilingParam::MaxCompletionTokens,
            std::sync::atomic::Ordering::Relaxed,
        );
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
            ceiling_param: AtomicCeilingParam::new(CeilingParam::MaxTokens),
        }
    }

    /// The name to send the output ceiling under, configured or inferred.
    fn with_ceiling_param(self, configured: Option<CeilingParam>, effort: Option<&str>) -> Self {
        self.ceiling_param
            .set(configured.unwrap_or_else(|| inferred_ceiling_param(effort)));
        self
    }

    async fn post_json(&self, path: &str, body: serde_json::Value) -> Result<serde_json::Value> {
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
        res.json().await.map_err(|e| Error::Inference {
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
    async fn chat(&self, mut body: serde_json::Value, ceiling: Option<usize>) -> Result<ChatReply> {
        body["model"] = json!(self.model);
        let started = std::time::Instant::now();

        let mut sent = self.ceiling_param.get();
        let mut may_retry = ceiling.is_some();
        let v = loop {
            if let Some(max) = ceiling {
                if let Some(o) = body.as_object_mut() {
                    o.remove(sent.flipped().as_str());
                }
                body[sent.as_str()] = json!(max);
            }
            match self.post_json("chat/completions", body.clone()).await {
                Ok(v) => break v,
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
                    self.ceiling_param.set(sent);
                }
                Err(e) => return Err(e),
            }
        };

        let finish_reason = v["choices"][0]["finish_reason"].as_str();
        tracing::info!(
            role = self.role,
            ms = started.elapsed().as_millis(),
            tokens = v["usage"]["completion_tokens"].as_u64(),
            finish_reason,
            "completion finished"
        );
        // `length` means the ceiling stopped the model rather than the model
        // stopping itself, and a reply nothing marks as cut off is read as a
        // complete one — by the parser, and by whoever asked.
        let truncated = finish_reason == Some("length");
        if truncated {
            tracing::warn!(
                role = self.role,
                ceiling,
                "the reply hit its output ceiling and is cut off"
            );
        }

        let text = v["choices"][0]["message"]["content"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| Error::Inference {
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
                    "no message content".into()
                },
            })?;
        Ok(ChatReply { text, truncated })
    }
}

/// The name to send the output ceiling under when the operator has not said.
///
/// A reasoning model refuses `max_tokens` outright — a 400 naming
/// `max_completion_tokens` — and `reasoning_effort` is the only signal at this
/// layer that one is on the other end. It is a guess, not a fact: the field is
/// also what suppresses thinking on a local model that understands it, and that
/// endpoint wants `max_tokens`. `ceiling_param` overrides it, and a 400
/// corrects it.
fn inferred_ceiling_param(effort: Option<&str>) -> CeilingParam {
    match effort {
        Some(_) => CeilingParam::MaxCompletionTokens,
        None => CeilingParam::MaxTokens,
    }
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
        // Truncation is not an error here: `parse_response` salvages the
        // artifacts a cut-off list still got right, and losing nine good ones to
        // the tenth is the worst trade in the write path.
        Ok(self
            .ep
            .chat(body, Some(self.budget.max_output_tokens))
            .await?
            .text)
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
                let second = self
                    .chat(
                        json!([
                            {"role":"system","content": prompt::SYNTHESIZER_SYSTEM},
                            {"role":"user","content": user},
                            {"role":"assistant","content": first},
                            {"role":"user","content": repair}
                        ]),
                        Some("artifacts"),
                    )
                    .await?;
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
        }
    }
}

#[async_trait]
impl Embedder for HttpEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
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
    async fn complete(&self, system: &str, user: &str) -> Result<String> {
        Ok(self
            .answer(system, user, self.max_output_tokens)
            .await?
            .text)
    }

    async fn answer(&self, system: &str, user: &str, ceiling: usize) -> Result<Completion> {
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
        // Never above what the role was configured to allow: a caller asking
        // for room it measured against the context window is asking for a
        // smaller ceiling, never a larger one.
        let ceiling = ceiling.min(self.max_output_tokens).max(1);
        let reply = self.ep.chat(body, Some(ceiling)).await?;
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
    pub fn new(cfg: &crate::config::VisionRole, synth: &SynthesizeRole) -> Self {
        let (base_url, api_key) = cfg.resolve(synth);
        Self {
            ep: Endpoint::new(
                &base_url,
                &cfg.model,
                api_key.as_deref(),
                cfg.timeout_secs,
                "vision",
            )
            // No `reasoning_effort` on this role, so the guess is `max_tokens`
            // unless the synthesize endpoint it may be borrowing says otherwise;
            // a hosted reasoning model's 400 corrects it on the first call.
            .with_ceiling_param(cfg.ceiling_param(synth), None),
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

    fn synthesize_cfg(base: String) -> SynthesizeRole {
        SynthesizeRole {
            base_url: base,
            model: "m".into(),
            api_key: Some("secret".into()),
            context_tokens: 8192,
            max_output_tokens: 2048,
            output_ratio: 1.4,
            tokenizer_path: None,
            timeout_secs: crate::config::DEFAULT_TIMEOUT_SECS,
            reasoning_effort: None,
            ceiling_param: None,
            structured_output: true,
            context_opening_tokens: 200,
            context_overlap_tokens: 150,
            cooldown_secs: None,
        }
    }
    fn ask_cfg(base: String) -> AskRole {
        AskRole {
            base_url: base,
            model: "m".into(),
            api_key: None,
            context_tokens: 4096,
            max_output_tokens: 1024,
            timeout_secs: crate::config::DEFAULT_TIMEOUT_SECS,
            reasoning_effort: None,
            ceiling_param: None,
        }
    }
    fn vision_cfg(base: Option<String>) -> crate::config::VisionRole {
        crate::config::VisionRole {
            model: "vl".into(),
            base_url: base,
            api_key: Some("k".into()),
            timeout_secs: 30,
            max_output_tokens: 2048,
            ceiling_param: None,
        }
    }
    fn embed_cfg(base: String) -> EmbedRole {
        EmbedRole {
            base_url: base,
            model: "e".into(),
            api_key: None,
            dim: 4,
            max_input_tokens: 512,
            timeout_secs: crate::config::DEFAULT_TIMEOUT_SECS,
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
            model: "m".into(),
            api_key: None,
            context_tokens: 4096,
            max_output_tokens: 1024,
            timeout_secs: crate::config::DEFAULT_TIMEOUT_SECS,
            reasoning_effort: None,
            ceiling_param: None,
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
            .embed(&["x".into()])
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
        let out = e.embed(&["first".into(), "second".into()]).await.unwrap();
        assert_eq!(out[0], vec![0.0, 1.0, 0.0, 0.0]);
        assert_eq!(out[1], vec![1.0, 0.0, 0.0, 0.0]);
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
            .embed(&["x".into()])
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
            .embed(&["a".into(), "b".into()])
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
            model: "m".into(),
            api_key: None,
            context_tokens: 4096,
            max_output_tokens: 1024,
            timeout_secs: crate::config::DEFAULT_TIMEOUT_SECS,
            reasoning_effort: None,
            ceiling_param: None,
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
                    model: "m".into(),
                    api_key: None,
                    context_tokens: 4096,
                    max_output_tokens: 1024,
                    timeout_secs: crate::config::DEFAULT_TIMEOUT_SECS,
                    reasoning_effort: None,
                    ceiling_param: None,
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
            .embed(&["x".into()])
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
        let d = HttpDescriber::new(
            &vision_cfg(Some(format!("{}/v1", server.uri()))),
            &synthesize_cfg("http://unused".into()),
        );
        let out = d
            .describe(b"\xFF\xD8jpegbytes", "Photo taken 2026-08-09")
            .await
            .unwrap();
        assert_eq!(out, "# Whiteboard\n\n- item");

        let req = &server.received_requests().await.unwrap()[0];
        assert_eq!(req.headers.get("authorization").unwrap(), "Bearer k");
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(body["model"], "vl");
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

    #[tokio::test]
    async fn a_describer_error_is_an_inference_error_for_the_vision_role() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503).set_body_string("busy"))
            .mount(&server)
            .await;
        let mut cfg = vision_cfg(Some(server.uri()));
        cfg.api_key = None;
        let d = HttpDescriber::new(&cfg, &synthesize_cfg("http://unused".into()));
        let e = d.describe(b"x", "").await.unwrap_err();
        assert!(matches!(e, Error::Inference { role: "vision", .. }), "{e}");
        assert!(e.retryable());
    }
}
