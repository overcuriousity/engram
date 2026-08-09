use super::{ChunkBudget, Chunker, Completer, Embedder, ProposedChunk, Reranker, prompt};
use crate::config::{AskRole, ChunkRole, EmbedRole, RerankRole, RerankStyle};
use crate::error::{Error, Result};
use async_trait::async_trait;
use serde_json::json;

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(180))
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

async fn post_json(
    role: &'static str,
    c: &reqwest::Client,
    url: String,
    api_key: Option<&str>,
    body: serde_json::Value,
) -> Result<serde_json::Value> {
    let mut req = c.post(&url).json(&body);
    if let Some(k) = api_key {
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
        return Err(Error::Inference {
            role,
            detail: format!("HTTP {status}: {detail}"),
        });
    }
    res.json().await.map_err(|e| Error::Inference {
        role,
        detail: e.to_string(),
    })
}

// ── Chunker ──────────────────────────────────────────────────────────────────

pub struct HttpChunker {
    client: reqwest::Client,
    base_url: String,
    model: String,
    api_key: Option<String>,
    budget: ChunkBudget,
    max_chunk_tokens: usize,
}

impl HttpChunker {
    pub fn new(cfg: &ChunkRole) -> Self {
        Self {
            client: client(),
            base_url: cfg.base_url.clone(),
            model: cfg.model.clone(),
            api_key: cfg.api_key.clone(),
            budget: ChunkBudget {
                context_tokens: cfg.context_tokens,
                max_output_tokens: cfg.max_output_tokens,
                output_ratio: cfg.output_ratio,
            },
            max_chunk_tokens: 1024,
        }
    }

    /// Caps chunk size so the embedder never receives an oversized chunk.
    /// Set from `embed.max_input_tokens * 0.8` during wiring.
    pub fn with_max_chunk_tokens(mut self, n: usize) -> Self {
        self.max_chunk_tokens = n;
        self
    }

    async fn chat(&self, messages: serde_json::Value) -> Result<String> {
        let body = json!({
            "model": self.model,
            "messages": messages,
            "max_tokens": self.budget.max_output_tokens,
            "temperature": 0.2,
        });
        let v = post_json(
            "chunk",
            &self.client,
            url(&self.base_url, "chat/completions"),
            self.api_key.as_deref(),
            body,
        )
        .await?;
        v["choices"][0]["message"]["content"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| Error::Inference {
                role: "chunk",
                detail: "no message content".into(),
            })
    }
}

#[async_trait]
impl Chunker for HttpChunker {
    async fn segment(&self, text: &str) -> Result<Vec<ProposedChunk>> {
        let user = prompt::user_prompt(text, 1, self.max_chunk_tokens);
        let first = self
            .chat(json!([
                {"role":"system","content": prompt::CHUNKER_SYSTEM},
                {"role":"user","content": user}
            ]))
            .await?;

        match prompt::parse_response(&first) {
            Ok(chunks) => Ok(chunks),
            Err(e) => {
                // One repair attempt with the parser error fed back. Beyond
                // that the caller falls back to a structural split.
                tracing::warn!(error = %e, "chunker returned unparsable output; repairing");
                let repair = prompt::repair_prompt(&first, &e.to_string());
                let second = self
                    .chat(json!([
                        {"role":"system","content": prompt::CHUNKER_SYSTEM},
                        {"role":"user","content": user},
                        {"role":"assistant","content": first},
                        {"role":"user","content": repair}
                    ]))
                    .await?;
                prompt::parse_response(&second)
            }
        }
    }

    fn budget(&self) -> ChunkBudget {
        self.budget
    }
}

// ── Embedder ─────────────────────────────────────────────────────────────────

pub struct HttpEmbedder {
    client: reqwest::Client,
    base_url: String,
    model: String,
    api_key: Option<String>,
    dim: usize,
    max_input_tokens: usize,
}

impl HttpEmbedder {
    pub fn new(cfg: &EmbedRole) -> Self {
        Self {
            client: client(),
            base_url: cfg.base_url.clone(),
            model: cfg.model.clone(),
            api_key: cfg.api_key.clone(),
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
            "model": self.model,
            "input": texts,
            "encoding_format": "float",
        });
        let v = post_json(
            "embed",
            &self.client,
            url(&self.base_url, "embeddings"),
            self.api_key.as_deref(),
            body,
        )
        .await?;

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
        &self.model
    }
    fn max_input_tokens(&self) -> usize {
        self.max_input_tokens
    }
}

// ── Reranker ─────────────────────────────────────────────────────────────────

pub struct HttpReranker {
    client: reqwest::Client,
    base_url: String,
    model: String,
    api_key: Option<String>,
    style: RerankStyle,
}

impl HttpReranker {
    pub fn new(cfg: &RerankRole) -> Self {
        Self {
            client: client(),
            base_url: cfg.base_url.clone(),
            model: cfg.model.clone(),
            api_key: cfg.api_key.clone(),
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
                json!({ "model": self.model, "query": query, "documents": docs, "top_n": top_n }),
            ),
            RerankStyle::Vllm => (
                "v1/rerank",
                json!({ "model": self.model, "query": query, "documents": docs, "top_n": top_n }),
            ),
        };
        let v = post_json(
            "rerank",
            &self.client,
            url(&self.base_url, path),
            self.api_key.as_deref(),
            body,
        )
        .await?;

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

// ── Completer ────────────────────────────────────────────────────────────────

pub struct HttpCompleter {
    client: reqwest::Client,
    base_url: String,
    model: String,
    api_key: Option<String>,
    context_tokens: usize,
}

impl HttpCompleter {
    pub fn new(cfg: &AskRole) -> Self {
        Self {
            client: client(),
            base_url: cfg.base_url.clone(),
            model: cfg.model.clone(),
            api_key: cfg.api_key.clone(),
            context_tokens: cfg.context_tokens,
        }
    }
}

#[async_trait]
impl Completer for HttpCompleter {
    async fn complete(&self, system: &str, user: &str) -> Result<String> {
        let body = json!({
            "model": self.model,
            "messages": [
                {"role":"system","content": system},
                {"role":"user","content": user}
            ],
            "temperature": 0.3,
        });
        let v = post_json(
            "ask",
            &self.client,
            url(&self.base_url, "chat/completions"),
            self.api_key.as_deref(),
            body,
        )
        .await?;
        v["choices"][0]["message"]["content"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| Error::Inference {
                role: "ask",
                detail: "no message content".into(),
            })
    }

    fn context_tokens(&self) -> usize {
        self.context_tokens
    }
}

/// One cheap reachability check per role at startup. Failure is a warning, not
/// a fatal error: ingest is designed to survive a dead inference endpoint.
pub async fn probe(role: &str, base_url: &str, api_key: Option<&str>) -> bool {
    let c = client();
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
    use crate::config::{AskRole, ChunkRole, EmbedRole, RerankRole, RerankStyle};
    use crate::infer::{Chunker, Completer, Embedder, Reranker};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn chunk_cfg(base: String) -> ChunkRole {
        ChunkRole {
            base_url: base,
            model: "m".into(),
            api_key: Some("secret".into()),
            context_tokens: 8192,
            max_output_tokens: 2048,
            output_ratio: 1.4,
            tokenizer_path: None,
        }
    }
    fn embed_cfg(base: String) -> EmbedRole {
        EmbedRole {
            base_url: base,
            model: "e".into(),
            api_key: None,
            dim: 4,
            max_input_tokens: 512,
        }
    }

    #[tokio::test]
    async fn chunker_posts_chat_completions_and_parses_chunks() {
        let server = MockServer::start().await;
        let reply = serde_json::json!({
            // r###: the payload contains `"##` (a quoted markdown H2), which
            // terminates both r#"..."# and r##"..."## literals.
            "choices":[{"message":{"content":
                r###"{"chunks":[{"text":"## A\nbody","title":"A","category":"note","tags":["t"]}]}"###}}]
        });
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("authorization", "Bearer secret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(reply))
            .mount(&server)
            .await;

        let c = HttpChunker::new(&chunk_cfg(server.uri()));
        let out = c.segment("anything").await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title.as_deref(), Some("A"));
    }

    #[tokio::test]
    async fn chunker_retries_once_with_a_repair_prompt() {
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
                "choices":[{"message":{"content":r#"{"chunks":[{"text":"ok"}]}"#}}]
            })))
            .mount(&server)
            .await;

        let c = HttpChunker::new(&chunk_cfg(server.uri()));
        let out = c.segment("anything").await.unwrap();
        assert_eq!(out[0].text, "ok");
    }

    #[tokio::test]
    async fn chunker_gives_up_after_the_repair_attempt() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices":[{"message":{"content":"still prose"}}]
            })))
            .mount(&server)
            .await;

        let c = HttpChunker::new(&chunk_cfg(server.uri()));
        assert!(matches!(
            c.segment("x").await,
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
        let c = HttpChunker::new(&chunk_cfg(server.uri()));
        let e = c.segment("x").await.unwrap_err();
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
    async fn embedder_asks_for_float_encoding_explicitly() {
        // A litellm proxy in front of a llama.cpp-style server forwards the
        // absent field as null, and the backend answers 500 with
        // "type must be string, but is null". Every embed call fails against
        // such an endpoint unless the field is sent.
        use wiremock::matchers::body_partial_json;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .and(body_partial_json(
                serde_json::json!({"encoding_format": "float"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"data":[{"index":0,"embedding":[1.0,0.0,0.0,0.0]}]}),
            ))
            .mount(&server)
            .await;

        let out = HttpEmbedder::new(&embed_cfg(server.uri()))
            .embed(&["x".into()])
            .await
            .unwrap();
        assert_eq!(out[0], vec![1.0, 0.0, 0.0, 0.0]);
    }

    #[tokio::test]
    async fn embedder_sends_a_batch_and_orders_results_by_index() {
        let server = MockServer::start().await;
        // Deliberately out of order: the API contract is that `index` is
        // authoritative, not array position.
        let reply = serde_json::json!({"data":[
            {"index":1,"embedding":[1.0,0.0,0.0,0.0]},
            {"index":0,"embedding":[0.0,1.0,0.0,0.0]}
        ]});
        Mock::given(method("POST"))
            .and(path("/embeddings"))
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
    async fn reranker_tei_style_returns_sorted_index_score_pairs() {
        let server = MockServer::start().await;
        let reply = serde_json::json!([{"index":2,"score":0.9},{"index":0,"score":0.4}]);
        Mock::given(method("POST"))
            .and(path("/rerank"))
            .respond_with(ResponseTemplate::new(200).set_body_json(reply))
            .mount(&server)
            .await;

        let cfg = RerankRole {
            base_url: server.uri(),
            model: "r".into(),
            api_key: None,
            style: RerankStyle::Tei,
        };
        let out = HttpReranker::new(&cfg)
            .rerank("q", &["a".into(), "b".into(), "c".into()], 2)
            .await
            .unwrap();
        assert_eq!(out[0].0, 2);
        assert_eq!(out.len(), 2);
    }

    #[tokio::test]
    async fn reranker_cohere_style_reads_relevance_score_and_results_wrapper() {
        let server = MockServer::start().await;
        let reply = serde_json::json!({"results":[
            {"index":1,"relevance_score":0.8},
            {"index":0,"relevance_score":0.95}
        ]});
        Mock::given(method("POST"))
            .and(path("/rerank"))
            .respond_with(ResponseTemplate::new(200).set_body_json(reply))
            .mount(&server)
            .await;

        let cfg = RerankRole {
            base_url: server.uri(),
            model: "r".into(),
            api_key: None,
            style: RerankStyle::Cohere,
        };
        let out = HttpReranker::new(&cfg)
            .rerank("q", &["a".into(), "b".into()], 5)
            .await
            .unwrap();
        assert_eq!(out[0].0, 0, "highest relevance_score must come first");
        assert_eq!(out.len(), 2);
    }

    #[tokio::test]
    async fn reranker_drops_out_of_range_indexes() {
        let server = MockServer::start().await;
        // A malformed index must not panic on the caller's `results.get(idx)`.
        let reply = serde_json::json!([{"index":99,"score":0.9},{"index":0,"score":0.4}]);
        Mock::given(method("POST"))
            .and(path("/rerank"))
            .respond_with(ResponseTemplate::new(200).set_body_json(reply))
            .mount(&server)
            .await;

        let cfg = RerankRole {
            base_url: server.uri(),
            model: "r".into(),
            api_key: None,
            style: RerankStyle::Tei,
        };
        let out = HttpReranker::new(&cfg)
            .rerank("q", &["a".into()], 5)
            .await
            .unwrap();
        assert_eq!(out, vec![(0, 0.4)]);
    }

    #[tokio::test]
    async fn completer_returns_message_content() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices":[{"message":{"content":"the answer"}}]
            })))
            .mount(&server)
            .await;
        let cfg = AskRole {
            base_url: server.uri(),
            model: "m".into(),
            api_key: None,
            context_tokens: 4096,
        };
        assert_eq!(
            HttpCompleter::new(&cfg).complete("s", "u").await.unwrap(),
            "the answer"
        );
    }

    #[tokio::test]
    async fn base_url_with_a_trailing_slash_does_not_double_up() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices":[{"message":{"content":"ok"}}]
            })))
            .mount(&server)
            .await;
        let cfg = AskRole {
            base_url: format!("{}/", server.uri()),
            model: "m".into(),
            api_key: None,
            context_tokens: 4096,
        };
        assert_eq!(
            HttpCompleter::new(&cfg).complete("s", "u").await.unwrap(),
            "ok"
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
}
