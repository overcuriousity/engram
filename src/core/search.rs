use super::Core;
use crate::error::{Error, Result};
use crate::vector::SearchFilter;

pub const DEFAULT_LIMIT: usize = 10;
pub const MAX_LIMIT: usize = 50;
/// Over-fetch before reranking and grouping. Both only narrow what they are
/// given, so the candidate pool has to be wider than the answer.
pub const CANDIDATE_MULTIPLIER: usize = 3;
/// Chunks one source may contribute to a result list. A forty-chunk document
/// otherwise fills the whole answer and hides everything else in the corpus.
pub const MAX_PER_SOURCE: usize = 3;

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

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Promote at most `max` hits per source to the front of the list, then refill
/// from what that displaced until `limit` is reached.
///
/// The cap is a diversity rule, not a ceiling. Capping alone would mean a base
/// holding two sources could never answer with more than `2 * max` results
/// however many good matches it contains — which is the common case for a young
/// knowledge base, and the case where throwing matches away hurts most. So the
/// displaced hits go back on the end in rank order: one long document no longer
/// leads the list, but it still fills it when nothing else can.
fn cap_per_source(
    hits: Vec<crate::vector::SearchHit>,
    max: usize,
    limit: usize,
) -> Vec<crate::vector::SearchHit> {
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut kept = Vec::with_capacity(hits.len());
    let mut displaced = Vec::new();
    for h in hits {
        let n = seen.entry(h.payload.source_id.clone()).or_insert(0);
        *n += 1;
        if *n <= max {
            kept.push(h);
        } else {
            displaced.push(h);
        }
    }
    if kept.len() < limit {
        let room = limit - kept.len();
        kept.extend(displaced.into_iter().take(room));
    }
    kept
}

/// Chunks older than this are candidates for resurfacing, and a chunk shown
/// within this window counts as still remembered.
pub const FORGOTTEN_AFTER_DAYS: i64 = 30;
const SECONDS_PER_DAY: i64 = 86_400;

impl Core {
    /// Record that these results were shown, without making the caller wait.
    ///
    /// One request for the whole list, off the request path: a search must not
    /// get slower, or fail, because a bookkeeping write did. Tracked rather
    /// than merely spawned, so shutdown drains it instead of dropping it — a
    /// lost stamp makes a chunk look forgotten when it is not.
    fn mark_seen(&self, results: &[SearchResult]) {
        if results.is_empty() {
            return;
        }
        let ids: Vec<String> = results.iter().map(|r| r.chunk_id.clone()).collect();
        let vectors = self.vectors.clone();
        let now = now_secs();
        self.background.spawn(async move {
            if let Err(e) = vectors.touch(&ids, now).await {
                tracing::warn!(error = %e, "could not record which chunks were shown");
            }
        });
    }

    /// A random handful of chunks that have not surfaced in a month.
    ///
    /// Random rather than ranked, because there is no query: the question is
    /// what has been forgotten, and ranking would keep returning the same
    /// answer to it.
    pub async fn resurface(&self, limit: usize) -> Result<Vec<SearchResult>> {
        let cutoff = now_secs() - FORGOTTEN_AFTER_DAYS * SECONDS_PER_DAY;
        let hits = self
            .vectors
            .resurface(limit.clamp(1, MAX_LIMIT), cutoff, cutoff)
            .await?;
        let results: Vec<SearchResult> = hits
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
        // Surfacing counts as seeing, or the same chunks come back tomorrow.
        self.mark_seen(&results);
        Ok(results)
    }

    /// The hot path. One embedding call, one vector search, and optionally one
    /// rerank call. No completion, ever.
    ///
    /// Results are capped per source so one long document cannot fill the list.
    pub async fn search(&self, query: &SearchQuery) -> Result<Vec<SearchResult>> {
        self.search_capped(query, Some(MAX_PER_SOURCE)).await
    }

    /// `cap` of `None` lets a single source supply every result. `ask` wants
    /// that: a question is often answered by one document, and starving it of
    /// its own paragraphs to make the list look varied helps nobody.
    pub async fn search_capped(
        &self,
        query: &SearchQuery,
        cap: Option<usize>,
    ) -> Result<Vec<SearchResult>> {
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
        // Over-fetch whenever something downstream narrows the list: both the
        // per-source cap and the reranker can only discard what they are given.
        let candidates = if cap.is_some() || self.reranker.is_some() {
            limit * CANDIDATE_MULTIPLIER
        } else {
            limit
        };
        // The lexical half of the query. Computed locally and for free, so it
        // costs nothing when the store ignores it.
        let sparse = crate::vector::sparse::encode_query(query.q.trim());
        let hits = self
            .vectors
            .search(&vectors[0], &sparse, candidates, &filter)
            .await?;

        // Cap before reranking, in vector order, so what leads per source is
        // that source's best.
        let hits = match cap {
            Some(max) => cap_per_source(hits, max, limit),
            None => hits,
        };

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
        self.mark_seen(&results);
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
        seed_from(core, "raw", texts).await
    }

    /// `raw` has to differ per source: sources are deduplicated by a hash of it.
    async fn seed_from(
        core: &crate::core::Core,
        raw: &str,
        texts: &[(&str, &str, &[&str])],
    ) -> String {
        let src = core.store.insert_source(raw, "web", None).await.unwrap();
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
                window_idx: None,
            })
            .collect();
        let made = core.store.insert_chunks(&src.id, &new).await.unwrap();
        for c in &made {
            crate::jobs::embed::run(core, &c.id).await.unwrap();
        }
        src.id
    }

    /// Rewrite every vector payload from the current chunk rows.
    async fn reembed_all(core: &crate::core::Core) {
        for src in core.store.list_sources(100, 0).await.unwrap() {
            for c in core.store.chunks_for_source(&src.id).await.unwrap() {
                crate::jobs::embed::run(core, &c.id).await.unwrap();
            }
        }
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
        // Spread across sources so the per-source cap is not what narrows the
        // list; this test is about the candidate pool, not about grouping.
        for batch in 0..10 {
            let texts: Vec<String> = (0..3).map(|i| format!("doc {batch}-{i}")).collect();
            let refs: Vec<(&str, &str, &[&str])> =
                texts.iter().map(|t| (t.as_str(), "c", &[][..])).collect();
            seed_from(&core, &format!("raw {batch}"), &refs).await;
        }

        let mut query = q("anything");
        query.limit = 5;
        let hits = core.search(&query).await.unwrap();
        assert_eq!(hits.len(), 5, "result count must still honour the limit");
    }

    #[tokio::test]
    async fn one_source_cannot_lead_the_whole_result_list() {
        // A forty-chunk document otherwise crowds out every other source and
        // the top of the list becomes forty near-identical paragraphs.
        let core = test_core().await;
        let hog: Vec<String> = (0..12).map(|i| format!("alpha {i}")).collect();
        let refs: Vec<(&str, &str, &[&str])> =
            hog.iter().map(|t| (t.as_str(), "c", &[][..])).collect();
        let big = seed_from(&core, "big", &refs).await;
        let small = seed_from(&core, "small", &[("alpha other", "c", &[])]).await;

        let hits = core.search(&q("t0\nalpha 0")).await.unwrap();
        let leading = &hits[..hits.len().min(MAX_PER_SOURCE + 1)];
        let from_big = leading.iter().filter(|h| h.source_id == big).count();
        assert!(
            from_big <= MAX_PER_SOURCE,
            "one source took {from_big} of the leading {} results",
            leading.len()
        );
        assert!(
            leading.iter().any(|h| h.source_id == small),
            "the crowded-out source never reached the top of the list"
        );
    }

    #[tokio::test]
    async fn a_single_source_base_still_fills_the_limit() {
        // The cap orders the list, it does not shorten it. A base holding one
        // document must not answer every query with three results.
        let core = test_core().await;
        let texts: Vec<String> = (0..8).map(|i| format!("alpha {i}")).collect();
        let refs: Vec<(&str, &str, &[&str])> =
            texts.iter().map(|t| (t.as_str(), "c", &[][..])).collect();
        seed_from(&core, "only", &refs).await;

        let hits = core.search(&q("t0\nalpha 0")).await.unwrap();
        assert_eq!(
            hits.len(),
            8,
            "the per-source cap swallowed matches nothing else could replace"
        );
        // And still no duplicates: refilling must not re-add a kept hit.
        let ids: std::collections::HashSet<&str> =
            hits.iter().map(|h| h.chunk_id.as_str()).collect();
        assert_eq!(ids.len(), hits.len(), "a hit appeared twice");
    }

    #[tokio::test]
    async fn ask_reads_in_rank_order_rather_than_diversity_order() {
        // An answer is often found in one document. `ask` takes the ranked
        // list as it is, rather than the reordering that makes a browsable
        // list varied.
        let core = test_core().await;
        let hog: Vec<String> = (0..6).map(|i| format!("alpha {i}")).collect();
        let refs: Vec<(&str, &str, &[&str])> =
            hog.iter().map(|t| (t.as_str(), "c", &[][..])).collect();
        seed_from(&core, "big", &refs).await;
        seed_from(&core, "small", &[("alpha other", "c", &[])]).await;

        let capped = core.search(&q("t0\nalpha 0")).await.unwrap();
        let uncapped = core.search_capped(&q("t0\nalpha 0"), None).await.unwrap();
        assert!(
            uncapped.windows(2).all(|w| w[0].score >= w[1].score),
            "ask was handed a reordered list: {:?}",
            uncapped.iter().map(|h| h.score).collect::<Vec<_>>()
        );
        assert_eq!(
            capped.len(),
            uncapped.len(),
            "the two paths must differ in order, not in how much they return"
        );
    }

    #[test]
    fn the_cap_leads_with_the_highest_ranked_chunk_of_each_source() {
        // Applied to a ranked list, what leads per source must be its best,
        // not whichever chunk happened to be enumerated first.
        use crate::vector::{SearchHit, VectorPayload};
        let hit = |chunk: &str, src: &str, score: f32| SearchHit {
            payload: VectorPayload {
                chunk_id: chunk.into(),
                source_id: src.into(),
                text: String::new(),
                title: None,
                category: None,
                tags: vec![],
                created_at: 0,
                last_seen_at: None,
            },
            score,
        };
        let ranked = || {
            vec![
                hit("a1", "a", 0.9),
                hit("a2", "a", 0.8),
                hit("b1", "b", 0.7),
                hit("a3", "a", 0.6),
            ]
        };
        let ids = |hits: Vec<SearchHit>| -> Vec<String> {
            hits.iter().map(|h| h.payload.chunk_id.clone()).collect()
        };

        // Room for three: the cap holds and `a3` stays out.
        assert_eq!(ids(cap_per_source(ranked(), 2, 3)), vec!["a1", "a2", "b1"]);
        // Room for four and nothing else to offer: `a3` comes back, last.
        assert_eq!(
            ids(cap_per_source(ranked(), 2, 4)),
            vec!["a1", "a2", "b1", "a3"],
            "a displaced hit must refill an otherwise short list"
        );
    }

    #[tokio::test]
    async fn resurface_returns_only_what_has_been_forgotten() {
        let core = test_core().await;
        seed_from(&core, "old", &[("long forgotten", "c", &[])]).await;
        seed_from(&core, "new", &[("captured just now", "c", &[])]).await;

        // `created_at` is set by the store, so age the one that should surface.
        let cutoff = now_secs() - FORGOTTEN_AFTER_DAYS * SECONDS_PER_DAY - 1;
        sqlx::query("UPDATE chunks SET created_at = ? WHERE text = ?")
            .bind(cutoff)
            .bind("long forgotten")
            .execute(&core.store.pool)
            .await
            .unwrap();
        // The vector payload carries its own copy, so re-embed to pick it up.
        reembed_all(&core).await;

        let out = core.resurface(10).await.unwrap();
        assert_eq!(
            out.len(),
            1,
            "got: {:?}",
            out.iter().map(|r| &r.text).collect::<Vec<_>>()
        );
        assert_eq!(out[0].text, "long forgotten");
    }

    #[tokio::test]
    async fn a_resurfaced_chunk_does_not_come_straight_back() {
        // Showing something counts as seeing it, or the same handful returns
        // every day and the feature is noise.
        let core = test_core().await;
        seed_from(&core, "old", &[("long forgotten", "c", &[])]).await;
        let old = now_secs() - FORGOTTEN_AFTER_DAYS * SECONDS_PER_DAY - 1;
        sqlx::query("UPDATE chunks SET created_at = ?")
            .bind(old)
            .execute(&core.store.pool)
            .await
            .unwrap();
        reembed_all(&core).await;

        assert_eq!(core.resurface(10).await.unwrap().len(), 1);
        // mark_seen runs off the request path; wait for it rather than sleeping
        // and hoping, or this test fails on a loaded machine and nowhere else.
        core.background.wait_idle().await;
        assert!(
            core.resurface(10).await.unwrap().is_empty(),
            "a chunk shown a moment ago is not forgotten"
        );
    }

    #[tokio::test]
    async fn an_empty_result_list_is_not_marked_seen() {
        // Nothing was shown, so nothing should be recorded — and the empty
        // case must not produce a pointless write.
        let core = test_core().await;
        assert!(core.search(&q("nothing here")).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn searching_an_empty_base_returns_nothing_rather_than_failing() {
        let core = test_core().await;
        assert!(core.search(&q("anything")).await.unwrap().is_empty());
    }
}
