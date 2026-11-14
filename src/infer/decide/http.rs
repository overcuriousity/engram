//! `Decider` over System One's `POST /systemone`.

use super::{Answers, Decider, Question, check, wire};
use crate::config::DecideRole;
use crate::error::{Error, Result};
use crate::infer::budget::TokenCounter;
use crate::infer::openai::Endpoint;
use crate::infer::retry::transiently;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;

/// A decision is awaited by someone; a short retry covers a blip, not an outage.
pub const DECIDE_RETRY_BUDGET: Duration = Duration::from_secs(3);

pub struct HttpDecider {
    ep: Endpoint,
    model: String,
    context_tokens: usize,
    counter: Arc<TokenCounter>,
    slots: tokio::sync::Semaphore,
}

impl HttpDecider {
    pub fn new(cfg: &DecideRole) -> Self {
        // An empty key from a templated config must not become `Bearer `.
        let key = cfg.api_key.as_deref().filter(|k| !k.trim().is_empty());
        Self {
            ep: Endpoint::new(&cfg.base_url, &cfg.model, key, cfg.timeout_secs, "decide"),
            model: cfg.model.clone(),
            context_tokens: cfg.context_tokens,
            counter: Arc::new(TokenCounter::default()),
            slots: tokio::sync::Semaphore::new(cfg.parallel.max(1)),
        }
    }

    pub fn with_counter(mut self, counter: Arc<TokenCounter>) -> Self {
        self.counter = counter;
        self
    }
}

#[async_trait]
impl Decider for HttpDecider {
    async fn decide(&self, state: &str, questions: &[Question]) -> Result<Answers> {
        check(&self.counter, self.context_tokens, state, questions)?;
        let body = wire::request_json(&self.model, state, questions);
        let _slot = self.slots.acquire().await.map_err(|_| Error::Inference {
            role: "decide",
            detail: "the decider is closed".into(),
        })?;
        let text = transiently(DECIDE_RETRY_BUDGET, || {
            self.ep.post_text("systemone", body.clone())
        })
        .await?;
        wire::parse_response(&text, questions)
    }

    fn context_tokens(&self) -> usize {
        self.context_tokens
    }

    fn model(&self) -> &str {
        &self.model
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infer::decide::Answer;
    use serde_json::Value;
    use wiremock::matchers::{body_string, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn fixture() -> Value {
        let f = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/systemone/response-noul.json"
        );
        serde_json::from_str(&std::fs::read_to_string(f).unwrap()).unwrap()
    }
    fn noul_ok() -> String {
        fixture()["body"].to_string()
    }
    fn wet() -> Question {
        Question::noul("wet", "Is water wet?")
    }
    fn role(server: &MockServer) -> DecideRole {
        DecideRole {
            base_url: server.uri(),
            model: "m".into(),
            api_key: None,
            context_tokens: 8192,
            timeout_secs: 5,
            parallel: 1,
        }
    }
    async fn serve(server: &MockServer, r: ResponseTemplate, times: Option<u64>) {
        let m = Mock::given(method("POST"))
            .and(path("/systemone"))
            .respond_with(r);
        match times {
            Some(n) => m.expect(n).mount(server).await,
            None => m.mount(server).await,
        }
    }
    fn ok() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_string(noul_ok())
    }
    fn assert_noul(a: &Answers) {
        // The fixture goes through serde_json's default float parse, which is off by an ulp.
        let Answer::Noul { yes } = a.answers["wet"] else {
            panic!("{:?}", a.answers["wet"])
        };
        assert!((yes - 0.9784768909885947).abs() < 1e-12, "{yes}");
        assert_eq!(a.input_tokens, 140);
    }

    #[tokio::test]
    async fn it_posts_the_request_to_systemone() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/systemone"))
            .and(body_string(wire::request_json("m", "", &[wet()])))
            .respond_with(ok())
            .expect(1)
            .mount(&s)
            .await;
        let a = HttpDecider::new(&role(&s))
            .decide("", &[wet()])
            .await
            .unwrap();
        assert_noul(&a);
    }
    #[tokio::test]
    async fn a_trailing_slash_on_the_base_url_is_harmless() {
        let s = MockServer::start().await;
        serve(&s, ok(), Some(1)).await;
        let mut r = role(&s);
        r.base_url = format!("{}/", s.uri());
        assert_noul(&HttpDecider::new(&r).decide("", &[wet()]).await.unwrap());
    }
    #[tokio::test]
    async fn a_request_that_cannot_fit_never_leaves() {
        let s = MockServer::start().await;
        serve(&s, ResponseTemplate::new(200), Some(0)).await;
        let mut r = role(&s);
        r.context_tokens = 1024;
        let e = HttpDecider::new(&r)
            .decide(&"word ".repeat(4000), &[wet()])
            .await;
        assert!(matches!(e, Err(Error::Validation(_))));
    }
    #[tokio::test]
    async fn invalid_questions_never_leave() {
        let s = MockServer::start().await;
        serve(&s, ResponseTemplate::new(200), Some(0)).await;
        let e = HttpDecider::new(&role(&s)).decide("", &[]).await;
        assert!(matches!(e, Err(Error::Validation(_))));
    }
    #[tokio::test]
    async fn a_429_is_asked_again() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429))
            .up_to_n_times(1)
            .mount(&s)
            .await;
        serve(&s, ok(), None).await;
        assert_noul(
            &HttpDecider::new(&role(&s))
                .decide("", &[wet()])
                .await
                .unwrap(),
        );
        assert_eq!(s.received_requests().await.unwrap().len(), 2);
    }
    #[tokio::test(start_paused = true)]
    async fn a_500_is_an_inference_error() {
        let s = MockServer::start().await;
        serve(&s, ResponseTemplate::new(500), None).await;
        let e = HttpDecider::new(&role(&s)).decide("", &[wet()]).await;
        assert!(
            matches!(e, Err(Error::Inference { role: "decide", .. })),
            "{e:?}"
        );
    }
    #[tokio::test]
    async fn a_reply_that_does_not_parse_is_refused_once() {
        let s = MockServer::start().await;
        serve(
            &s,
            ResponseTemplate::new(200).set_body_string("not json"),
            Some(1),
        )
        .await;
        let e = HttpDecider::new(&role(&s)).decide("", &[wet()]).await;
        assert!(
            matches!(e, Err(Error::InferenceRejected { role: "decide", .. })),
            "{e:?}"
        );
    }
    #[tokio::test]
    async fn the_key_is_sent_as_a_bearer() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("authorization", "Bearer k"))
            .respond_with(ok())
            .expect(1)
            .mount(&s)
            .await;
        let mut r = role(&s);
        r.api_key = Some("k".into());
        assert_noul(&HttpDecider::new(&r).decide("", &[wet()]).await.unwrap());
    }
    #[tokio::test]
    async fn an_empty_key_sends_no_authorization() {
        let s = MockServer::start().await;
        serve(&s, ok(), None).await;
        let mut r = role(&s);
        r.api_key = Some("".into());
        HttpDecider::new(&r).decide("", &[wet()]).await.unwrap();
        assert!(
            s.received_requests().await.unwrap()[0]
                .headers
                .get("authorization")
                .is_none()
        );
    }
    #[tokio::test]
    async fn one_slot_means_one_request_at_a_time() {
        let s = MockServer::start().await;
        serve(&s, ok().set_delay(Duration::from_millis(150)), None).await;
        let pair = |parallel: usize| {
            let mut r = role(&s);
            r.parallel = parallel;
            let d = HttpDecider::new(&r);
            async move {
                let t = std::time::Instant::now();
                let qs = [wet()];
                let (a, b) = tokio::join!(d.decide("", &qs), d.decide("", &qs));
                a.unwrap();
                b.unwrap();
                t.elapsed()
            }
        };
        assert!(pair(1).await >= Duration::from_millis(300));
        assert!(pair(2).await < Duration::from_millis(290));
    }
}
