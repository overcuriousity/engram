use super::Core;
use super::search::{SearchQuery, SearchResult};
use crate::error::{Error, Result};
use crate::infer::budget::pack_by_budget;
use crate::infer::prompt::{ABSTAIN_PREFIX, ASK_SYSTEM, abstained, ask_excerpt, ask_prompt};
use crate::store::asks::{NewAsk, NewAskCitation};
use crate::store::feedback::{Door, Origin};

#[derive(Debug, Clone, serde::Deserialize)]
pub struct AskRequest {
    pub q: String,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub category: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AskResponse {
    pub answer: String,
    /// Exactly the excerpts the model saw.
    pub citations: Vec<SearchResult>,
    /// Retrieved but left out for budget. Reported so a missing citation is
    /// visible rather than silent.
    pub dropped: usize,
    /// The answer stops where its output ceiling did, not where the model meant
    /// to. Reported for the same reason `dropped` is: an answer cut off
    /// mid-sentence is otherwise indistinguishable from a complete one, and the
    /// fix — a higher `ask.max_output_tokens` — belongs to whoever is reading
    /// it.
    pub truncated: bool,
    /// The answer opened with `prompt::ABSTAIN_PREFIX`, or there was nothing
    /// to show the model. What the harness counts as "said nothing here".
    pub abstained: bool,
    /// The recorded question, when this door records — the UI, with feedback
    /// on. The page shows a verdict bar only when this is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
}

impl Core {
    pub async fn ask(&self, req: &AskRequest, origin: impl Into<Origin>) -> Result<AskResponse> {
        let origin = origin.into();
        if req.q.trim().is_empty() {
            return Err(Error::Validation("question is empty".into()));
        }

        // Held for the whole answer rather than around the completion, because
        // a search embeds the query and that is a model call too. A gap between
        // them is a gap the worker would fill with a window, and a window is
        // twenty to seventy seconds of somebody waiting.
        //
        // Taking the lane does not make an in-flight call stop; nothing here
        // cancels. It keeps the worker from putting anything new in front of
        // this one.
        let _lane = self.gate.interactive();

        // No per-source cap: an answer often lives in one document, and
        // withholding its paragraphs to keep the citation list varied would
        // make the answer worse, not fairer.
        let (hits, _) = self
            .search_with(
                &SearchQuery {
                    q: req.q.clone(),
                    limit: req.limit.unwrap_or(8),
                    tags: req.tags.clone(),
                    category: req.category.clone(),
                    // Asking a question is as deliberate as a search gets.
                    mark: true,
                    include_deprecated: false,
                    include_superseded: false,
                },
                None,
                // Deliberately not captured: the right answer to a question is
                // a synthesis across several artifacts, so "which one was it"
                // has no well-defined meaning for someone judging it later.
                crate::store::feedback::Door::Ask,
            )
            .await?;

        if hits.is_empty() {
            // No retrieval, no completion: spending a model call to say
            // "nothing found" is pure latency. Opens with the sentinel so the
            // page and the harness read it as the abstention it is.
            let response = AskResponse {
                answer: format!("{ABSTAIN_PREFIX}. Nothing matches that question."),
                citations: vec![],
                dropped: 0,
                truncated: false,
                abstained: true,
                event_id: None,
            };
            return self.record_ask(req, &origin, response).await;
        }

        // Caveats are the conditions under which an excerpt does not apply, and
        // an answer that quotes "run `mkfs` on the device" without "destroys
        // everything already on it" is worse than no answer. They are not in
        // the vector payload — what gets embedded is a separate decision — so
        // they are read from the store, which costs one cheap SQLite lookup per
        // hit and no inference. An excerpt whose row has since been deleted
        // simply carries none.
        let mut blocks: Vec<String> = Vec::with_capacity(hits.len());
        for (i, h) in hits.iter().enumerate() {
            let caveats = self
                .store
                .get_artifact(&h.artifact_id)
                .await
                .map(|c| c.caveats)
                .unwrap_or_default();
            blocks.push(ask_excerpt(
                i + 1,
                h.title.as_deref().unwrap_or_default(),
                &h.text,
                &caveats,
            ));
        }

        // Reserve what the completer will ask for, not a constant. The endpoint
        // counts the prompt and the requested ceiling against one window, so
        // packing excerpts up to `context - 1024` while the call goes out
        // demanding `max_output_tokens` of reply is a request the server refuses
        // outright — and it refuses it on precisely the queries that retrieved
        // enough to be worth answering.
        //
        // The margin comes off the top too. `ceiling_for_prompt` holds back
        // headroom for the estimate being an estimate, and it holds it back out
        // of the *reply*: packing up to `max_output_tokens` exactly and then
        // being charged that margin is how a 32k window with a 2k ceiling ends
        // up asking for one token of answer. Reserving it here is what makes
        // the two halves agree.
        //
        // Never more than half the window, though. The reserve is configuration
        // and the window is configuration, and nothing makes the two agree: a
        // role whose ceiling is its whole context (4096 and 4096, which is an
        // ordinary shape for a local model) reserves everything, packs nothing,
        // and answers "too large for the context window" to every question ever
        // asked without once calling the model. Half a window of excerpts and
        // half a window of answer is a worse answer than the operator asked for;
        // no answer is not an answer.
        let context = self.completer.context_tokens();
        let reserve = self
            .completer
            .max_output_tokens()
            .saturating_add(crate::infer::budget::MAX_HEADROOM_TOKENS)
            .min(context / 2);
        let budget = context
            .saturating_sub(self.counter.count(ASK_SYSTEM))
            .saturating_sub(self.counter.count(&req.q))
            .saturating_sub(reserve);

        // Highest score first, so what gets cut is what mattered least.
        let kept = pack_by_budget(&blocks, &self.counter, budget);
        let dropped = blocks.len() - kept;
        if dropped > 0 {
            tracing::info!(dropped, kept, "ask: excerpts trimmed to fit the context");
        }

        if kept == 0 {
            let response = AskResponse {
                answer: "The best matching excerpt is too large for the configured context window."
                    .into(),
                citations: vec![],
                dropped,
                truncated: false,
                // A configuration failure, not a statement about the base.
                abstained: false,
                event_id: None,
            };
            return self.record_ask(req, &origin, response).await;
        }

        let user = ask_prompt(&req.q, &blocks[..kept]);

        // The ceiling comes from what the prompt actually cost, not from what
        // was reserved for it: packing is an estimate made before the excerpts
        // were chosen, and whatever the window has left over after them is room
        // the answer may have. Bounded by the configured ceiling, so this only
        // ever gives back what the reserve above took away — and it keeps the
        // one invariant the endpoint enforces, that prompt plus ceiling fits the
        // window, with the margin `ceiling_for_prompt` leaves for the estimate
        // being an estimate.
        let spent = self.counter.count(ASK_SYSTEM) + self.counter.count(&user);
        let ceiling = crate::infer::budget::ceiling_for_prompt(
            context,
            spent,
            self.completer.max_output_tokens(),
        );
        let answer = self.completer.answer(ASK_SYSTEM, &user, ceiling).await?;
        if answer.truncated {
            tracing::warn!(
                ceiling,
                "ask: the answer hit its output ceiling and is cut off"
            );
        }

        let response = AskResponse {
            abstained: abstained(&answer.text),
            answer: answer.text,
            citations: hits.into_iter().take(kept).collect(),
            dropped,
            truncated: answer.truncated,
            event_id: None,
        };
        self.record_ask(req, &origin, response).await
    }

    /// Record the question when this door records. Only the UI, and only with
    /// feedback on: a question is personal data of the same kind as a query,
    /// and API and MCP callers asked for the smallest footprint. Recorded
    /// synchronously — the id goes back to the page — and after the answer,
    /// which has already taken seconds; one insert costs nothing beside it.
    /// A failure to record must not cost the answer: it is logged and the
    /// response goes out without an id.
    async fn record_ask(
        &self,
        req: &AskRequest,
        origin: &Origin,
        mut response: AskResponse,
    ) -> Result<AskResponse> {
        if !(self.feedback.enabled && origin.door == Door::Ui) {
            return Ok(response);
        }
        let ask = NewAsk {
            question: req.q.trim().to_string(),
            scope: origin.scope.clone(),
            filters: serde_json::json!({
                "tags": req.tags,
                "category": req.category,
                "limit": req.limit.unwrap_or(8),
            })
            .to_string(),
            query_vec: self.cached_query_vector(&req.q).unwrap_or_default(),
            embed_model: self.embedder.model().to_string(),
            answer: response.answer.clone(),
            abstained: response.abstained,
            dropped: response.dropped,
            truncated: response.truncated,
            citations: response
                .citations
                .iter()
                .map(|c| NewAskCitation {
                    artifact_id: c.artifact_id.clone(),
                    score: c.score,
                })
                .collect(),
        };
        match self.store.record_ask(ask).await {
            Ok(id) => response.event_id = Some(id),
            Err(e) => tracing::warn!(error = %e, "could not record the question"),
        }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_support::test_core;
    use crate::store::artifacts::NewArtifact;
    use crate::store::feedback::Door;

    /// A completer that asks, from inside the model call, whether background
    /// work could start right now. That is the question the lane exists to
    /// answer, and the only place it can be asked honestly.
    struct LaneProbe {
        gate: std::sync::Arc<crate::infer::gate::InferenceGate>,
        background_was_held: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl crate::infer::Completer for LaneProbe {
        async fn complete(&self, _system: &str, _user: &str) -> Result<String> {
            let held =
                tokio::time::timeout(std::time::Duration::from_millis(50), self.gate.background())
                    .await
                    .is_err();
            self.background_was_held
                .store(held, std::sync::atomic::Ordering::SeqCst);
            Ok("an answer".into())
        }
        fn context_tokens(&self) -> usize {
            4096
        }
        fn max_output_tokens(&self) -> usize {
            1024
        }
    }

    #[tokio::test]
    async fn a_question_holds_the_interactive_lane_for_its_whole_answer() {
        // Priority, not preemption: the worker must not put a new window in
        // front of someone who is waiting. What it cannot do is stop the window
        // already running, which is the ~73s ceiling this deliberately accepts.
        let mut core = test_core().await;
        let probe = std::sync::Arc::new(LaneProbe {
            gate: std::sync::Arc::clone(&core.gate),
            background_was_held: std::sync::atomic::AtomicBool::new(false),
        });
        core.completer = probe.clone();
        seed(&core, 3, 4).await;

        core.ask(
            &AskRequest {
                q: "chunk".into(),
                limit: Some(3),
                tags: vec![],
                category: None,
            },
            Door::Api,
        )
        .await
        .unwrap();

        assert!(
            probe
                .background_was_held
                .load(std::sync::atomic::Ordering::SeqCst),
            "a window could have started while the question was still being answered"
        );
    }

    #[tokio::test]
    async fn asking_a_question_leaves_the_lane_free_afterwards() {
        // The lease is RAII, so the interesting failure is one that leaks: a
        // single question would then hold background work off forever.
        let core = test_core().await;
        seed(&core, 3, 4).await;

        core.ask(
            &AskRequest {
                q: "chunk".into(),
                limit: Some(3),
                tags: vec![],
                category: None,
            },
            Door::Api,
        )
        .await
        .unwrap();

        // Returns immediately if the lane was released; hangs the test if not.
        tokio::time::timeout(std::time::Duration::from_secs(5), core.gate.background())
            .await
            .expect("the interactive lane was never released");
    }

    async fn seed(core: &crate::core::Core, n: usize, size: usize) {
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let new: Vec<NewArtifact> = (0..n)
            .map(|i| NewArtifact {
                ordinal: i as i64,
                text: format!("chunk {i} ") + &"filler ".repeat(size),
                corpus_span: None,
                title: Some(format!("t{i}")),
                category: Some("reference".into()),
                tags: vec![],
                segment_idx: None,
                caveats: vec![],
            })
            .collect();
        let made = core.store.insert_artifacts(&src.id, &new).await.unwrap();
        for c in &made {
            crate::jobs::embed::run(core, &c.id).await.unwrap();
        }
    }

    fn req(q: &str) -> AskRequest {
        AskRequest {
            q: q.into(),
            limit: None,
            tags: vec![],
            category: None,
        }
    }

    #[tokio::test]
    async fn ask_returns_an_answer_with_the_chunks_it_used() {
        let core = test_core().await;
        seed(&core, 2, 2).await;
        let out = core
            .ask(&req("how do I do the thing"), Door::Api)
            .await
            .unwrap();
        assert_eq!(out.answer, "fake answer");
        assert!(
            !out.citations.is_empty(),
            "an answer with no citations is unverifiable"
        );
    }

    #[tokio::test]
    async fn the_model_is_shown_the_caveats_of_every_excerpt() {
        // A caveat is the condition under which an artifact does not apply, and
        // an answer that quotes a destructive command without it is worse than
        // no answer. Caveats are not in the vector payload, so this asserts the
        // store lookup that puts them back.
        let mut core = test_core().await;
        core.completer = std::sync::Arc::new(crate::infer::fake::FakeCompleter { reply: None });
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    ordinal: 0,
                    text: "Format the device with mkfs.".into(),
                    corpus_span: None,
                    title: Some("Format a device".into()),
                    category: None,
                    tags: vec![],
                    segment_idx: None,
                    caveats: vec!["Destroys every existing file on the device.".into()],
                }],
            )
            .await
            .unwrap();
        crate::jobs::embed::run(&core, &made[0].id).await.unwrap();

        let out = core
            .ask(&req("how do I format a device"), Door::Api)
            .await
            .unwrap();
        assert!(
            out.answer
                .contains("Caveat: Destroys every existing file on the device."),
            "the caveat never reached the model: {}",
            out.answer
        );
    }

    #[tokio::test]
    async fn ask_reports_chunks_dropped_for_budget() {
        let core = test_core().await;
        // FakeCompleter reports a 4096-token context; oversized excerpts force
        // some to be left out.
        seed(&core, 20, 400).await;
        let out = core.ask(&req("anything"), Door::Api).await.unwrap();
        assert!(
            out.dropped > 0,
            "a silently dropped citation is worse than a reported one"
        );
        assert!(out.citations.len() < 20);
    }

    /// Echoes its prompt back and records the ceiling it was handed, which
    /// together are the whole request the endpoint would have measured.
    struct Ceilinged {
        context: usize,
        max_output: usize,
        asked_for: std::sync::atomic::AtomicUsize,
    }

    impl Ceilinged {
        fn new(context: usize, max_output: usize) -> Self {
            Self {
                context,
                max_output,
                asked_for: std::sync::atomic::AtomicUsize::new(0),
            }
        }
        fn ceiling(&self) -> usize {
            self.asked_for.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl crate::infer::Completer for Ceilinged {
        async fn complete(&self, _system: &str, user: &str) -> Result<String> {
            Ok(user.to_string())
        }
        async fn answer(
            &self,
            _system: &str,
            user: &str,
            ceiling: usize,
        ) -> Result<crate::infer::Completion> {
            self.asked_for
                .store(ceiling, std::sync::atomic::Ordering::SeqCst);
            Ok(crate::infer::Completion {
                text: user.to_string(),
                truncated: false,
            })
        }
        fn context_tokens(&self) -> usize {
            self.context
        }
        fn max_output_tokens(&self) -> usize {
            self.max_output
        }
    }

    /// The prompt and the reply are counted against one window by the endpoint,
    /// so packing excerpts up to a constant reserve while the call demands
    /// `max_output_tokens` of reply is a request the server refuses — and it
    /// refuses it on exactly the questions that retrieved enough to be worth
    /// answering. What is packed has to leave room for what is asked for.
    ///
    /// Both directions, in the two shapes an operator actually configures: a
    /// ceiling that fits comfortably inside its window, and one that eats most
    /// of it.
    #[tokio::test]
    async fn what_is_packed_leaves_room_for_the_reply_that_was_asked_for() {
        use crate::infer::budget::MAX_HEADROOM_TOKENS;

        for (context, max_output) in [(4096, 3072), (8192, 1024), (4096, 4096), (32768, 2048)] {
            let completer = std::sync::Arc::new(Ceilinged::new(context, max_output));
            let mut core = test_core().await;
            core.completer = completer.clone();
            // Excerpts sized against the window, so packing actually fills it
            // in every case — a prompt that leaves the window half empty tests
            // nothing about what happens when it does not.
            seed(&core, 20, context / 20).await;
            let mut r = req("anything");
            r.limit = Some(20);
            let out = core.ask(&r, Door::Api).await.unwrap();

            // The echoed prompt is what actually went to the endpoint, and the
            // recorded ceiling is what the call asked for on top of it.
            let prompt = core.counter.count(&out.answer) + core.counter.count(ASK_SYSTEM);
            let ceiling = completer.ceiling();
            assert!(
                prompt + ceiling <= context,
                "at context {context} / ceiling {max_output}: the prompt was {prompt} tokens \
                 and the call asked for {ceiling} more, which is {} over the window",
                prompt + ceiling - context
            );
            assert!(
                ceiling > 0 && ceiling <= max_output,
                "at context {context} / ceiling {max_output}: the call asked for {ceiling}, \
                 which is not a ceiling the role allows"
            );

            // And it is a *usable* ceiling, not merely a positive one. The
            // reserve is what packing set aside for the answer; the answer must
            // get it back, less the margin held for the estimate. Asserting
            // only `> 0` let a ceiling of 1 — an empty reply, every time —
            // through at the very ordinary 32k/2k shape.
            let floor = max_output.min(context / 2) - MAX_HEADROOM_TOKENS;
            assert!(
                ceiling >= floor,
                "at context {context} / ceiling {max_output}: the call asked for {ceiling}, \
                 but packing reserved room for at least {floor}"
            );
        }
    }

    /// The reserve is configuration and the window is configuration, and nothing
    /// makes the two agree. A role whose ceiling is its whole window — 4096 and
    /// 4096, an ordinary local shape, and what every default lands on — reserved
    /// everything and packed nothing, so every question ever asked was answered
    /// "too large for the configured context window" without the model being
    /// called at all.
    #[tokio::test]
    async fn a_ceiling_as_wide_as_its_window_still_answers() {
        let mut core = test_core().await;
        core.completer = std::sync::Arc::new(Ceilinged::new(4096, 4096));
        seed(&core, 3, 4).await;
        let out = core.ask(&req("chunk"), Door::Api).await.unwrap();
        assert!(
            !out.citations.is_empty(),
            "nothing was packed, so nothing was answered: {}",
            out.answer
        );
    }

    /// An answer that stops because it ran out of room reads exactly like one
    /// that stopped because it was finished, and the difference is the reader's
    /// to act on.
    #[tokio::test]
    async fn an_answer_cut_off_by_its_ceiling_says_so() {
        struct Truncating;
        #[async_trait::async_trait]
        impl crate::infer::Completer for Truncating {
            async fn complete(&self, _system: &str, _user: &str) -> Result<String> {
                Ok("half an ans".into())
            }
            async fn answer(
                &self,
                _system: &str,
                _user: &str,
                _ceiling: usize,
            ) -> Result<crate::infer::Completion> {
                Ok(crate::infer::Completion {
                    text: "half an ans".into(),
                    truncated: true,
                })
            }
            fn context_tokens(&self) -> usize {
                4096
            }
            fn max_output_tokens(&self) -> usize {
                1024
            }
        }

        let mut core = test_core().await;
        core.completer = std::sync::Arc::new(Truncating);
        seed(&core, 3, 4).await;
        let out = core.ask(&req("chunk"), Door::Api).await.unwrap();
        assert!(
            out.truncated,
            "a cut-off answer was reported as a whole one"
        );
    }

    #[tokio::test]
    async fn citations_match_exactly_what_the_model_was_shown() {
        let core = test_core().await;
        seed(&core, 20, 400).await;
        let out = core.ask(&req("anything"), Door::Api).await.unwrap();
        assert_eq!(
            out.citations.len() + out.dropped,
            8,
            "citations plus dropped must account for every retrieved excerpt"
        );
    }

    #[tokio::test]
    async fn ask_with_no_matches_says_so_without_calling_the_model() {
        let core = test_core().await;
        let out = core
            .ask(&req("nothing is stored"), Door::Api)
            .await
            .unwrap();
        assert!(out.citations.is_empty());
        assert!(
            out.answer.to_lowercase().contains("nothing"),
            "got: {}",
            out.answer
        );
    }

    #[tokio::test]
    async fn empty_question_is_rejected() {
        let core = test_core().await;
        assert!(matches!(
            core.ask(&req("  "), Door::Api).await,
            Err(crate::error::Error::Validation(_))
        ));
    }

    #[tokio::test]
    async fn a_ui_ask_is_recorded_with_its_citations_when_feedback_is_on() {
        let mut core = test_core().await;
        core.feedback.enabled = true;
        seed(&core, 3, 4).await;
        let out = core.ask(&req("chunk"), Door::Ui.by("me")).await.unwrap();
        let id = out.event_id.expect("a UI ask is recorded");
        let ev = core.store.ask_event(&id).await.unwrap().expect("stored");
        assert_eq!(ev.question, "chunk");
        assert_eq!(ev.answer, out.answer);
        assert_eq!(
            ev.citations
                .iter()
                .map(|c| c.artifact_id.as_str())
                .collect::<Vec<_>>(),
            out.citations
                .iter()
                .map(|c| c.artifact_id.as_str())
                .collect::<Vec<_>>(),
            "the stored citations must be exactly the excerpts the model saw, in order"
        );
        assert!(!out.abstained);
        // The vector it retrieved with travels with it, so a gap can be
        // clustered later without re-embedding.
        let dim: i64 = sqlx::query_scalar("SELECT vec_dim FROM ask_events WHERE id = ?")
            .bind(&id)
            .fetch_one(&core.store.pool)
            .await
            .unwrap();
        assert!(dim > 0);
    }

    #[tokio::test]
    async fn an_api_or_mcp_ask_is_never_recorded() {
        let mut core = test_core().await;
        core.feedback.enabled = true;
        seed(&core, 3, 4).await;
        for door in [Door::Api, Door::Mcp] {
            let out = core.ask(&req("chunk"), door).await.unwrap();
            assert!(out.event_id.is_none(), "{door:?} recorded a question");
        }
        assert_eq!(core.store.ask_stats().await.unwrap().asked, 0);
    }

    #[tokio::test]
    async fn a_ui_ask_is_not_recorded_when_feedback_is_off() {
        let core = test_core().await;
        seed(&core, 3, 4).await;
        let out = core.ask(&req("chunk"), Door::Ui.by("me")).await.unwrap();
        assert!(out.event_id.is_none());
    }

    #[tokio::test]
    async fn an_ask_with_no_hits_is_recorded_as_an_abstention_without_a_model_call() {
        let mut core = test_core().await;
        core.feedback.enabled = true;
        let out = core
            .ask(&req("nothing is stored"), Door::Ui.by("me"))
            .await
            .unwrap();
        assert!(out.abstained);
        assert!(
            crate::infer::prompt::abstained(&out.answer),
            "{}",
            out.answer
        );
        let ev = core
            .store
            .ask_event(out.event_id.as_deref().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert!(ev.abstained && ev.citations.is_empty());
    }

    #[tokio::test]
    async fn an_answer_that_opens_with_the_sentinel_is_flagged_abstained() {
        let mut core = test_core().await;
        core.completer = std::sync::Arc::new(crate::infer::fake::FakeCompleter {
            reply: Some("Not in the knowledge base. The excerpts cover chunks only.".into()),
        });
        seed(&core, 3, 4).await;
        let out = core.ask(&req("chunk"), Door::Api).await.unwrap();
        assert!(out.abstained);
        assert!(
            !out.citations.is_empty(),
            "abstaining does not hide what was shown"
        );
    }
}
