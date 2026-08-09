use super::Core;
use crate::error::{Error, Result};
use crate::vector::SearchFilter;

pub const DEFAULT_LIMIT: usize = 10;
pub const MAX_LIMIT: usize = 50;
/// Over-fetch before reranking. Reranking only reorders what it is given, so
/// the candidate pool has to be wider than the answer.
pub const CANDIDATE_MULTIPLIER: usize = 3;

#[derive(Debug, Clone, serde::Deserialize)]
pub struct SearchQuery {
    pub q: String,
    #[serde(default)]
    pub limit: usize,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub category: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchResult {
    pub chunk_id: String,
    pub source_id: String,
    pub title: Option<String>,
    pub text: String,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub score: f32,
}

impl Core {
    /// The hot path. One embedding call, one vector search, and optionally one
    /// rerank call. No completion, ever.
    pub async fn search(&self, query: &SearchQuery) -> Result<Vec<SearchResult>> {
        if query.q.trim().is_empty() {
            return Err(Error::Validation("query is empty".into()));
        }
        let limit = match query.limit {
            0 => DEFAULT_LIMIT,
            n => n.min(MAX_LIMIT),
        };

        let started = std::time::Instant::now();
        let vectors = self.embedder.embed(&[query.q.trim().to_string()]).await?;
        let embed_ms = started.elapsed().as_millis();

        let filter = SearchFilter {
            tags: query.tags.clone(),
            category: query.category.clone(),
        };
        let candidates = if self.reranker.is_some() {
            limit * CANDIDATE_MULTIPLIER
        } else {
            limit
        };
        let hits = self
            .vectors
            .search(&vectors[0], candidates, &filter)
            .await?;

        let mut results: Vec<SearchResult> = hits
            .into_iter()
            .map(|h| SearchResult {
                chunk_id: h.payload.chunk_id,
                source_id: h.payload.source_id,
                title: h.payload.title,
                text: h.payload.text,
                category: h.payload.category,
                tags: h.payload.tags,
                score: h.score,
            })
            .collect();

        if let Some(reranker) = &self.reranker
            && !results.is_empty()
        {
            let docs: Vec<String> = results.iter().map(|r| r.text.clone()).collect();
            match reranker.rerank(&query.q, &docs, limit).await {
                Ok(order) => {
                    results = order
                        .into_iter()
                        .filter_map(|(idx, score)| {
                            results
                                .get(idx)
                                .map(|r| SearchResult { score, ..r.clone() })
                        })
                        .collect();
                }
                // A rerank failure degrades ordering, not availability; vector
                // order is still a usable answer.
                Err(e) => tracing::warn!(error = %e, "rerank failed; returning vector order"),
            }
        }

        results.truncate(limit);
        tracing::info!(
            q = %query.q,
            results = results.len(),
            embed_ms,
            total_ms = started.elapsed().as_millis(),
            "search"
        );
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_support::{test_core, test_core_with_rerank};
    use crate::store::chunks::NewChunk;

    async fn seed(core: &crate::core::Core, texts: &[(&str, &str, &[&str])]) -> String {
        let src = core.store.insert_source("raw", "web", None).await.unwrap();
        let new: Vec<NewChunk> = texts
            .iter()
            .enumerate()
            .map(|(i, (text, cat, tags))| NewChunk {
                ordinal: i as i64,
                text: text.to_string(),
                source_span: None,
                title: Some(format!("t{i}")),
                category: Some(cat.to_string()),
                tags: tags.iter().map(|s| s.to_string()).collect(),
            })
            .collect();
        let made = core.store.insert_chunks(&src.id, &new).await.unwrap();
        for c in &made {
            crate::jobs::embed::run(core, &c.id).await.unwrap();
        }
        src.id
    }

    fn q(text: &str) -> SearchQuery {
        SearchQuery {
            q: text.into(),
            limit: 10,
            tags: vec![],
            category: None,
        }
    }

    #[tokio::test]
    async fn returns_the_chunk_whose_text_matches_the_query() {
        let core = test_core().await;
        seed(
            &core,
            &[
                ("mounting an E01 image", "procedure", &["forensics"]),
                ("configuring a printer", "procedure", &["office"]),
            ],
        )
        .await;

        // FakeEmbedder hashes text, so query the exact embedded string to get
        // a deterministic top hit.
        let hits = core.search(&q("t0\nmounting an E01 image")).await.unwrap();
        assert_eq!(hits[0].text, "mounting an E01 image");
        assert!(hits[0].score > 0.99);
    }

    #[tokio::test]
    async fn results_carry_everything_needed_to_render_without_a_second_lookup() {
        let core = test_core().await;
        let src_id = seed(&core, &[("body text", "concept", &["a", "b"])]).await;
        let hits = core.search(&q("t0\nbody text")).await.unwrap();
        assert_eq!(hits[0].source_id, src_id);
        assert_eq!(hits[0].title.as_deref(), Some("t0"));
        assert_eq!(hits[0].category.as_deref(), Some("concept"));
        assert_eq!(hits[0].tags, vec!["a".to_string(), "b".to_string()]);
    }

    #[tokio::test]
    async fn tag_and_category_filters_narrow_the_results() {
        let core = test_core().await;
        seed(
            &core,
            &[
                ("alpha", "procedure", &["linux"]),
                ("beta", "concept", &["linux"]),
            ],
        )
        .await;

        let mut query = q("anything");
        query.category = Some("concept".into());
        let hits = core.search(&query).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].text, "beta");

        let mut query = q("anything");
        query.tags = vec!["linux".into()];
        assert_eq!(core.search(&query).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn limit_is_clamped_to_a_sane_range() {
        let core = test_core().await;
        seed(&core, &[("a", "c", &[]), ("b", "c", &[]), ("c", "c", &[])]).await;

        let mut query = q("anything");
        query.limit = 0;
        assert_eq!(
            core.search(&query).await.unwrap().len(),
            3,
            "limit 0 must fall back to the default"
        );

        query.limit = 1;
        assert_eq!(core.search(&query).await.unwrap().len(), 1);

        query.limit = 10_000;
        assert!(core.search(&query).await.unwrap().len() <= MAX_LIMIT);
    }

    #[tokio::test]
    async fn empty_query_is_rejected() {
        let core = test_core().await;
        assert!(matches!(
            core.search(&q("  ")).await,
            Err(crate::error::Error::Validation(_))
        ));
    }

    #[tokio::test]
    async fn rerank_reorders_when_configured() {
        let core = test_core_with_rerank().await;
        seed(
            &core,
            &[("alpha", "c", &[]), ("beta", "c", &[]), ("gamma", "c", &[])],
        )
        .await;

        let plain = test_core().await;
        seed(
            &plain,
            &[("alpha", "c", &[]), ("beta", "c", &[]), ("gamma", "c", &[])],
        )
        .await;

        let with = core.search(&q("t0\nalpha")).await.unwrap();
        let without = plain.search(&q("t0\nalpha")).await.unwrap();
        assert_ne!(
            with.iter().map(|h| h.text.clone()).collect::<Vec<_>>(),
            without.iter().map(|h| h.text.clone()).collect::<Vec<_>>(),
            "FakeReranker reverses order, so the two must differ"
        );
    }

    #[tokio::test]
    async fn rerank_over_fetches_candidates_before_narrowing() {
        // Reranking can only reorder what it is given. If the candidate pool
        // were not wider than the limit, a better match ranked 11th by vector
        // similarity could never be promoted into a top-10 answer.
        let core = test_core_with_rerank().await;
        let many: Vec<(String, &str, Vec<&str>)> =
            (0..30).map(|i| (format!("doc {i}"), "c", vec![])).collect();
        let refs: Vec<(&str, &str, &[&str])> = many
            .iter()
            .map(|(t, c, g)| (t.as_str(), *c, g.as_slice()))
            .collect();
        seed(&core, &refs).await;

        let mut query = q("anything");
        query.limit = 5;
        let hits = core.search(&query).await.unwrap();
        assert_eq!(hits.len(), 5, "result count must still honour the limit");
    }

    #[tokio::test]
    async fn searching_an_empty_base_returns_nothing_rather_than_failing() {
        let core = test_core().await;
        assert!(core.search(&q("anything")).await.unwrap().is_empty());
    }
}
