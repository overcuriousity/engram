use super::{
    Completer, Describer, Embedder, ProposedArtifact, Reranker, SegmentInput, SynthesisBudget,
    Synthesizer, prompt,
};
use crate::config::{AskRole, EmbedRole, RerankRole, RerankStyle, SynthesizeRole};
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
        }
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

    /// One chat completion; `body` carries everything but `model`. Logs the
    /// cost of every call — on local hardware a window takes minutes, and the
    /// log is what tells a long wait from a hang. `finish_reason` is what tells
    /// a truncated reply from a model that wrote bad JSON of its own accord.
    async fn chat(&self, mut body: serde_json::Value) -> Result<String> {
        body["model"] = json!(self.model);
        let started = std::time::Instant::now();
        let v = self.post_json("chat/completions", body).await?;
        tracing::info!(
            role = self.role,
            ms = started.elapsed().as_millis(),
            tokens = v["usage"]["completion_tokens"].as_u64(),
            finish_reason = v["choices"][0]["finish_reason"].as_str(),
            "completion finished"
        );
        v["choices"][0]["message"]["content"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| Error::Inference {
                role: self.role,
                detail: "no message content".into(),
            })
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
            ),
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
            "max_tokens": self.budget.max_output_tokens,
            "temperature": 0.2,
        });
        if let Some(effort) = &self.reasoning_effort {
            body["reasoning_effort"] = json!(effort);
        }
        if let Some(name) = schema.filter(|_| self.structured_output) {
            body["response_format"] = response_format(name, prompt::artifacts_schema());
        }
        self.ep.chat(body).await
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
            ),
            context_tokens: cfg.context_tokens,
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
        Self {
            ep: Endpoint::new(
                &cfg.base_url,
                &cfg.model,
                cfg.api_key.as_deref(),
                cfg.timeout_secs,
                "judge",
            ),
            context_tokens: cfg.context_tokens,
            reasoning_effort: cfg.reasoning_effort.clone(),
            response_schema: cfg
                .structured_output
                .then(|| ("verdict", prompt::dedupe_schema())),
        }
    }
}

#[async_trait]
impl Completer for HttpCompleter {
    async fn complete(&self, system: &str, user: &str) -> Result<String> {
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
        self.ep.chat(body).await
    }

    fn context_tokens(&self) -> usize {
        self.context_tokens
    }
}

// ── Describer ────────────────────────────────────────────────────────────────

pub struct HttpDescriber {
    ep: Endpoint,
}

impl HttpDescriber {
    pub fn new(model: &str, base_url: &str, api_key: Option<&str>, timeout_secs: u64) -> Self {
        Self {
            ep: Endpoint::new(base_url, model, api_key, timeout_secs, "vision"),
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
        self.ep.chat(body).await
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
            structured_output: true,
            context_opening_tokens: 200,
            context_overlap_tokens: 150,
            cooldown_secs: None,
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
            timeout_secs: crate::config::DEFAULT_TIMEOUT_SECS,
            reasoning_effort: None,
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
        let cases: [(RerankStyle, serde_json::Value, usize, Vec<(usize, f32)>); 3] = [
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
            timeout_secs: crate::config::DEFAULT_TIMEOUT_SECS,
            reasoning_effort: None,
        };
        assert_eq!(
            HttpCompleter::new(&cfg).complete("s", "u").await.unwrap(),
            "the answer"
        );
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
        let d = HttpDescriber::new("vl", &format!("{}/v1", server.uri()), Some("k"), 30);
        let out = d
            .describe(b"\xFF\xD8jpegbytes", "Photo taken 2026-08-09")
            .await
            .unwrap();
        assert_eq!(out, "# Whiteboard\n\n- item");

        let req = &server.received_requests().await.unwrap()[0];
        assert_eq!(req.headers.get("authorization").unwrap(), "Bearer k");
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(body["model"], "vl");
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
        let d = HttpDescriber::new("vl", &server.uri(), None, 30);
        let e = d.describe(b"x", "").await.unwrap_err();
        assert!(matches!(e, Error::Inference { role: "vision", .. }), "{e}");
        assert!(e.retryable());
    }
}
