use super::Core;
use super::search::{SearchQuery, SearchResult};
use crate::error::{Error, Result};
use crate::infer::budget::pack_by_budget;

const ASK_SYSTEM: &str = "You answer questions using only the provided knowledge-base excerpts. \
Quote commands, paths and code exactly as they appear. If the excerpts do not contain the answer, \
say so plainly rather than guessing. Cite excerpts by their number. \
An excerpt may carry lines beginning `Caveat:` — the conditions under which it does not apply. \
Repeat any caveat that bears on your answer rather than dropping it.";

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
    pub citations: Vec<SearchResult>,
    pub dropped: usize,
}

impl Core {
    pub async fn ask(&self, req: &AskRequest) -> Result<AskResponse> {
        if req.q.trim().is_empty() {
            return Err(Error::Validation("question is empty".into()));
        }

        let hits = self
            .search_capped(
                &SearchQuery {
                    q: req.q.clone(),
                    limit: req.limit.unwrap_or(8),
                    tags: req.tags.clone(),
                    category: req.category.clone(),
                    mark: true,
                    include_deprecated: false,
                    include_superseded: false,
                },
                None,
            )
            .await?;

        if hits.is_empty() {
            return Ok(AskResponse {
                answer: "Nothing in the knowledge base matches that question.".into(),
                citations: vec![],
                dropped: 0,
            });
        }

        let mut blocks: Vec<String> = Vec::with_capacity(hits.len());
        for (i, h) in hits.iter().enumerate() {
            let caveats = self
                .store
                .get_artifact(&h.artifact_id)
                .await
                .map(|c| c.caveats)
                .unwrap_or_default();
            let mut block = format!(
                "[{}] {}\n{}",
                i + 1,
                h.title.clone().unwrap_or_default(),
                h.text
            );
            for c in &caveats {
                block.push_str("\nCaveat: ");
                block.push_str(c);
            }
            blocks.push(block);
        }

        let budget = self
            .completer
            .context_tokens()
            .saturating_sub(self.counter.count(ASK_SYSTEM))
            .saturating_sub(self.counter.count(&req.q))
            .saturating_sub(ANSWER_RESERVE_TOKENS);

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

        let user = format!(
            "Question: {}\n\nExcerpts:\n\n{}",
            req.q,
            blocks[..kept].join("\n\n---\n\n")
        );
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
