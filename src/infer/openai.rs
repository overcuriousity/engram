use super::{
    Completer, Embedder, ProposedArtifact, Reranker, SynthesisBudget, Synthesizer, prompt,
};
use crate::config::{AskRole, EmbedRole, RerankRole, RerankStyle, SynthesizeRole};
use crate::error::{Error, Result};
use async_trait::async_trait;
use serde_json::json;

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

pub struct HttpSynthesizer {
    client: reqwest::Client,
    base_url: String,
    model: String,
    api_key: Option<String>,
    budget: SynthesisBudget,
    max_artifact_tokens: usize,
    reasoning_effort: Option<String>,
    cooldown: std::time::Duration,
}

impl HttpSynthesizer {
    pub fn new(cfg: &SynthesizeRole) -> Self {
        Self {
            client: client(cfg.timeout_secs),
            base_url: cfg.base_url.clone(),
            model: cfg.model.clone(),
            api_key: cfg.api_key.clone(),
            budget: SynthesisBudget {
                context_tokens: cfg.context_tokens,
                max_output_tokens: cfg.max_output_tokens,
                output_ratio: cfg.output_ratio,
            },
            max_artifact_tokens: 1024,
            reasoning_effort: cfg.reasoning_effort.clone(),
            cooldown: std::time::Duration::from_secs(cfg.cooldown_secs),
        }
    }

    pub fn with_max_artifact_tokens(mut self, n: usize) -> Self {
        self.max_artifact_tokens = n;
        self
    }

    async fn chat(&self, messages: serde_json::Value) -> Result<String> {
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "max_tokens": self.budget.max_output_tokens,
            "temperature": 0.2,
        });
        if let Some(effort) = &self.reasoning_effort {
            body["reasoning_effort"] = json!(effort);
        }
        let started = std::time::Instant::now();
        let v = post_json(
            "chunk",
            &self.client,
            url(&self.base_url, "chat/completions"),
            self.api_key.as_deref(),
            body,
        )
        .await?;
        tracing::info!(
            ms = started.elapsed().as_millis(),
            tokens = v["usage"]["completion_tokens"].as_u64(),
            "synthesizer call finished"
        );
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
impl Synthesizer for HttpSynthesizer {
    async fn segment(&self, text: &str) -> Result<Vec<ProposedArtifact>> {
        let user = prompt::user_prompt(text, 1, self.max_artifact_tokens);
        let first = self
            .chat(json!([
                {"role":"system","content": prompt::SYNTHESIZER_SYSTEM},
                {"role":"user","content": user}
            ]))
            .await?;

        match prompt::parse_response(&first) {
            Ok(chunks) => Ok(chunks),
            Err(e) => {
                tracing::warn!(error = %e, "synthesizer returned unparsable output; repairing");
                let repair = prompt::repair_prompt(&first, &e.to_string());
                let second = self
                    .chat(json!([
                        {"role":"system","content": prompt::SYNTHESIZER_SYSTEM},
                        {"role":"user","content": user},
                        {"role":"assistant","content": first},
                        {"role":"user","content": repair}
                    ]))
                    .await?;
                prompt::parse_response(&second)
            }
        }
    }

    fn budget(&self) -> SynthesisBudget {
        self.budget
    }

    fn cooldown(&self) -> std::time::Duration {
        self.cooldown
    }

    async fn title(&self, text: &str, artifact_titles: &[String]) -> Result<Option<String>> {
        let out = self
            .chat(json!([
                {"role":"system","content": prompt::TITLE_SYSTEM},
                {"role":"user","content": prompt::title_prompt(text, artifact_titles)}
            ]))
            .await?;
        let t = out.trim().trim_matches('"').trim();
        Ok((!t.is_empty()).then(|| t.chars().take(120).collect()))
    }
}

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
            client: client(cfg.timeout_secs),
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
            client: client(cfg.timeout_secs),
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
            client: client(cfg.timeout_secs),
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
            cooldown_secs: 0,
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
        let out = c.segment("anything").await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title.as_deref(), Some("A"));
    }

    #[tokio::test]
    async fn synthesizer_retries_once_with_a_repair_prompt() {
        let server = MockServer::start().await;
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
        let out = c.segment("anything").await.unwrap();
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
        let c = HttpSynthesizer::new(&synthesize_cfg(server.uri()));
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
        let e = HttpEmbedder::new(&embed_cfg(server.uri()))
            .embed(&["x".into()])
            .await
            .unwrap_err();
        assert!(e.to_string().contains("dimension"));
    }

    #[tokio::test]
    async fn embedder_rejects_a_short_batch() {
        let server = MockServer::start().await;
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
            timeout_secs: crate::config::DEFAULT_TIMEOUT_SECS,
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
            timeout_secs: crate::config::DEFAULT_TIMEOUT_SECS,
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
            timeout_secs: crate::config::DEFAULT_TIMEOUT_SECS,
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
            timeout_secs: crate::config::DEFAULT_TIMEOUT_SECS,
            reasoning_effort: None,
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
            timeout_secs: crate::config::DEFAULT_TIMEOUT_SECS,
            reasoning_effort: None,
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
