use super::Core;
use super::search::{SearchQuery, SearchResult};
use crate::error::{Error, Result};
use crate::infer::budget::pack_by_budget;
use crate::infer::prompt::{ASK_SYSTEM, ask_excerpt, ask_prompt};

/// Reserve part of the context for the answer itself.
const ANSWER_RESERVE_TOKENS: usize = 1024;

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
}

impl Core {
    pub async fn ask(&self, req: &AskRequest) -> Result<AskResponse> {
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
        let hits = self
            .search_capped(
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
            // "nothing found" is pure latency.
            return Ok(AskResponse {
                answer: "Nothing in the knowledge base matches that question.".into(),
                citations: vec![],
                dropped: 0,
            });
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

        let budget = self
            .completer
            .context_tokens()
            .saturating_sub(self.counter.count(ASK_SYSTEM))
            .saturating_sub(self.counter.count(&req.q))
            .saturating_sub(ANSWER_RESERVE_TOKENS);

        // Highest score first, so what gets cut is what mattered least.
        let kept = pack_by_budget(&blocks, &self.counter, budget);
        let dropped = blocks.len() - kept;
        if dropped > 0 {
            tracing::info!(dropped, kept, "ask: excerpts trimmed to fit the context");
        }

        if kept == 0 {
            return Ok(AskResponse {
                answer: "The best matching excerpt is too large for the configured context window."
                    .into(),
                citations: vec![],
                dropped,
            });
        }

        let user = ask_prompt(&req.q, &blocks[..kept]);
        let answer = self.completer.complete(ASK_SYSTEM, &user).await?;

        Ok(AskResponse {
            answer,
            citations: hits.into_iter().take(kept).collect(),
            dropped,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_support::test_core;
    use crate::store::artifacts::NewArtifact;

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

        core.ask(&AskRequest {
            q: "chunk".into(),
            limit: Some(3),
            tags: vec![],
            category: None,
        })
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

        core.ask(&AskRequest {
            q: "chunk".into(),
            limit: Some(3),
            tags: vec![],
            category: None,
        })
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
                category: Some("note".into()),
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
        let out = core.ask(&req("how do I do the thing")).await.unwrap();
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
        core.completer = std::sync::Arc::new(crate::infer::fake::EchoCompleter);
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

        let out = core.ask(&req("how do I format a device")).await.unwrap();
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
        let out = core.ask(&req("anything")).await.unwrap();
        assert!(
            out.dropped > 0,
            "a silently dropped citation is worse than a reported one"
        );
        assert!(out.citations.len() < 20);
    }

    #[tokio::test]
    async fn citations_match_exactly_what_the_model_was_shown() {
        let core = test_core().await;
        seed(&core, 20, 400).await;
        let out = core.ask(&req("anything")).await.unwrap();
        assert_eq!(
            out.citations.len() + out.dropped,
            8,
            "citations plus dropped must account for every retrieved excerpt"
        );
    }

    #[tokio::test]
    async fn ask_with_no_matches_says_so_without_calling_the_model() {
        let core = test_core().await;
        let out = core.ask(&req("nothing is stored")).await.unwrap();
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
            core.ask(&req("  ")).await,
            Err(crate::error::Error::Validation(_))
        ));
    }
}
