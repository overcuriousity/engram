use super::Core;
use crate::error::{Error, Result};
use crate::store::artifacts::ArtifactStatus;
use crate::vector::SearchFilter;
use std::collections::HashMap;

pub const DEFAULT_LIMIT: usize = 10;
pub const MAX_LIMIT: usize = 50;
pub const CANDIDATE_MULTIPLIER: usize = 3;
pub const MAX_PER_CORPUS: usize = 3;

#[derive(Debug, Clone, serde::Deserialize)]
pub struct SearchQuery {
    pub q: String,
    #[serde(default)]
    pub limit: usize,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub mark: bool,
    #[serde(default)]
    pub include_deprecated: bool,
    #[serde(default)]
    pub include_superseded: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct SearchTiming {
    pub embed_ms: u128,
    pub total_ms: u128,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchResult {
    pub artifact_id: String,
    pub corpus_id: String,
    pub title: Option<String>,
    pub text: String,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub score: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<ArtifactStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_verified_at: Option<i64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub weak: bool,
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn cap_per_corpus(
    hits: Vec<crate::vector::SearchHit>,
    max: usize,
    target: usize,
) -> Vec<crate::vector::SearchHit> {
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut kept = Vec::with_capacity(hits.len());
    let mut displaced = Vec::new();
    for h in hits {
        let n = seen.entry(h.payload.corpus_id.clone()).or_insert(0);
        *n += 1;
        if *n <= max {
            kept.push(h);
        } else {
            displaced.push(h);
        }
    }
    if kept.len() < target {
        let room = target - kept.len();
        kept.extend(displaced.into_iter().take(room));
    }
    kept
}

fn counts_of(hits: &[crate::vector::SearchHit]) -> HashMap<String, i64> {
    hits.iter()
        .map(|h| {
            (
                h.payload.artifact_id.clone(),
                h.payload.hit_count.unwrap_or(0),
            )
        })
        .collect()
}

pub const FORGOTTEN_AFTER_DAYS: i64 = 30;
const SECONDS_PER_DAY: i64 = 86_400;

impl Core {
    fn mark_seen(
        &self,
        results: &[SearchResult],
        hit_counts: &HashMap<String, i64>,
        counts_as_hit: bool,
    ) {
        if results.is_empty() {
            return;
        }
        let targets: Vec<crate::vector::Touch> = results
            .iter()
            .map(|r| {
                if counts_as_hit {
                    crate::vector::Touch::retrieved(
                        &r.artifact_id,
                        hit_counts.get(&r.artifact_id).copied(),
                    )
                } else {
                    crate::vector::Touch::shown(&r.artifact_id)
                }
            })
            .collect();
        let vectors = self.vectors.clone();
        let now = now_secs();
        self.background.spawn(async move {
            if let Err(e) = vectors.touch(&targets, now).await {
                tracing::warn!(error = %e, "could not record which chunks were shown");
            }
        });
    }

    pub fn mark_artifact_seen(&self, artifact_id: &str) {
        let targets = vec![crate::vector::Touch::shown(artifact_id)];
        let vectors = self.vectors.clone();
        let now = now_secs();
        self.background.spawn(async move {
            if let Err(e) = vectors.touch(&targets, now).await {
                tracing::warn!(error = %e, "could not record that a chunk was opened");
            }
        });
    }

    pub async fn resurface(&self, limit: usize) -> Result<Vec<SearchResult>> {
        let cutoff = now_secs() - FORGOTTEN_AFTER_DAYS * SECONDS_PER_DAY;
        let hits = self
            .vectors
            .resurface(limit.clamp(1, MAX_LIMIT), cutoff, cutoff)
            .await?;
        let hit_counts = counts_of(&hits);
        let results: Vec<SearchResult> = hits
            .into_iter()
            .map(|h| SearchResult {
                artifact_id: h.payload.artifact_id,
                corpus_id: h.payload.corpus_id,
                title: h.payload.title,
                text: h.payload.text,
                category: h.payload.category,
                tags: h.payload.tags,
                score: h.score,
                status: h.payload.status,
                superseded_by: h.payload.superseded_by,
                last_verified_at: h.payload.last_verified_at,
                weak: false,
            })
            .collect();
        self.mark_seen(&results, &hit_counts, false);
        Ok(results)
    }

    pub async fn stale_candidates(&self, limit: usize) -> Result<Vec<SearchResult>> {
        let cutoff = now_secs() - self.consolidate.stale_after_days as i64 * SECONDS_PER_DAY;
        let hits = self
            .vectors
            .stale_candidates(
                cutoff,
                self.consolidate.stale_max_hits,
                limit.clamp(1, MAX_LIMIT),
            )
            .await?;
        Ok(hits
            .into_iter()
            .map(|h| SearchResult {
                artifact_id: h.payload.artifact_id,
                corpus_id: h.payload.corpus_id,
                title: h.payload.title,
                text: h.payload.text,
                category: h.payload.category,
                tags: h.payload.tags,
                score: h.score,
                status: h.payload.status,
                superseded_by: h.payload.superseded_by,
                last_verified_at: h.payload.last_verified_at,
                weak: false,
            })
            .collect())
    }

    pub async fn search(&self, query: &SearchQuery) -> Result<Vec<SearchResult>> {
        Ok(self.search_inner(query, Some(MAX_PER_CORPUS)).await?.0)
    }

    pub async fn search_timed(
        &self,
        query: &SearchQuery,
    ) -> Result<(Vec<SearchResult>, SearchTiming)> {
        self.search_inner(query, Some(MAX_PER_CORPUS)).await
    }

    pub async fn search_capped(
        &self,
        query: &SearchQuery,
        cap: Option<usize>,
    ) -> Result<Vec<SearchResult>> {
        Ok(self.search_inner(query, cap).await?.0)
    }

    async fn search_inner(
        &self,
        query: &SearchQuery,
        cap: Option<usize>,
    ) -> Result<(Vec<SearchResult>, SearchTiming)> {
        if query.q.trim().is_empty() {
            return Err(Error::Validation("query is empty".into()));
        }
        let limit = match query.limit {
            0 => DEFAULT_LIMIT,
            n => n.min(MAX_LIMIT),
        };

        let started = std::time::Instant::now();
        let key = query.q.split_whitespace().collect::<Vec<_>>().join(" ");
        let cached = self.query_cache.lock().ok().and_then(|c| c.get(&key));
        let vector = match cached {
            Some(v) => v,
            None => {
                let v = self
                    .embedder
                    .embed(&[query.q.trim().to_string()])
                    .await?
                    .remove(0);
                if let Ok(mut c) = self.query_cache.lock() {
                    c.put(key, v.clone());
                }
                v
            }
        };
        let embed_ms = started.elapsed().as_millis();

        let filter = SearchFilter {
            tags: query.tags.clone(),
            category: query.category.clone(),
            include_superseded: query.include_superseded,
            include_deprecated: query.include_deprecated,
        };
        let candidates = if cap.is_some() || self.reranker.is_some() {
            limit * CANDIDATE_MULTIPLIER
        } else {
            limit
        };
        let sparse = crate::vector::sparse::encode_query(query.q.trim());
        let hits = self
            .vectors
            .search(&vector, &sparse, candidates, &filter)
            .await?;

        let hits = match cap {
            Some(max) => cap_per_corpus(hits, max, candidates),
            None => hits,
        };
        let hit_counts = counts_of(&hits);

        let mut results: Vec<SearchResult> = hits
            .into_iter()
            .map(|h| SearchResult {
                artifact_id: h.payload.artifact_id,
                corpus_id: h.payload.corpus_id,
                title: h.payload.title,
                text: h.payload.text,
                category: h.payload.category,
                tags: h.payload.tags,
                score: h.score,
                status: h.payload.status,
                superseded_by: h.payload.superseded_by,
                last_verified_at: h.payload.last_verified_at,
                weak: h.similarity.is_some_and(|s| s < self.weak_below),
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
                Err(e) => tracing::warn!(error = %e, "rerank failed; returning vector order"),
            }
        }

        results.truncate(limit);
        if query.mark {
            self.mark_seen(&results, &hit_counts, true);
        }
        tracing::info!(
            q = %query.q,
            results = results.len(),
            embed_ms,
            total_ms = started.elapsed().as_millis(),
            "search"
        );
        Ok((
            results,
            SearchTiming {
                embed_ms,
                total_ms: started.elapsed().as_millis(),
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_support::{test_core, test_core_with_rerank};
    use crate::store::artifacts::NewArtifact;

    async fn seed(core: &crate::core::Core, texts: &[(&str, &str, &[&str])]) -> String {
        seed_from(core, "raw", texts).await
    }

    async fn seed_from(
        core: &crate::core::Core,
        raw: &str,
        texts: &[(&str, &str, &[&str])],
    ) -> String {
        let src = core.store.insert_corpus(raw, "web", None).await.unwrap();
        let new: Vec<NewArtifact> = texts
            .iter()
            .enumerate()
            .map(|(i, (text, cat, tags))| NewArtifact {
                ordinal: i as i64,
                text: text.to_string(),
                corpus_span: None,
                title: Some(format!("t{i}")),
                category: Some(cat.to_string()),
                tags: tags.iter().map(|s| s.to_string()).collect(),
                segment_idx: None,
                caveats: vec![],
            })
            .collect();
        let made = core.store.insert_artifacts(&src.id, &new).await.unwrap();
        for c in &made {
            crate::jobs::embed::run(core, &c.id).await.unwrap();
        }
        src.id
    }

    async fn reembed_all(core: &crate::core::Core) {
        for src in core.store.list_corpora(100, 0).await.unwrap() {
            for c in core.store.artifacts_for_corpus(&src.id).await.unwrap() {
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
            mark: true,
            include_deprecated: false,
            include_superseded: false,
        }
    }

    #[tokio::test]
    async fn an_identical_query_is_embedded_once() {
        let (core, embedder) = crate::core::test_support::test_core_counting_embed_calls().await;
        seed(&core, &[("alpha text", "note", &[])]).await;
        reembed_all(&core).await;

        core.search(&q("dd write iso")).await.unwrap();
        let after_first = embedder.calls();
        core.search(&q("dd write iso")).await.unwrap();
        core.search(&q("  dd write iso  ")).await.unwrap();

        assert_eq!(
            embedder.calls(),
            after_first,
            "the query embedding must be cached"
        );
    }

    #[tokio::test]
    async fn an_unmarked_search_does_not_stamp_last_seen() {
        let core = test_core().await;
        seed_from(&core, "raw", &[("alpha text", "note", &[])]).await;
        reembed_all(&core).await;

        let mut query = q("alpha");
        query.mark = false;
        assert!(!core.search(&query).await.unwrap().is_empty());
        core.background.wait_idle().await;

        let stamped = core
            .vectors
            .resurface(10, i64::MAX, i64::MAX)
            .await
            .unwrap()
            .into_iter()
            .filter(|h| h.payload.last_seen_at.is_some())
            .count();
        assert_eq!(stamped, 0, "typing must not stamp last_seen_at");
    }

    #[tokio::test]
    async fn a_marked_search_records_what_it_showed() {
        let core = test_core().await;
        seed_from(&core, "raw", &[("alpha text", "note", &[])]).await;
        reembed_all(&core).await;

        assert!(!core.search(&q("alpha")).await.unwrap().is_empty());
        core.background.wait_idle().await;

        let stamped = core
            .vectors
            .resurface(10, i64::MAX, i64::MAX)
            .await
            .unwrap()
            .into_iter()
            .filter(|h| h.payload.last_seen_at.is_some())
            .count();
        assert!(stamped > 0, "a deliberate search still counts as seeing");
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

        let hits = core.search(&q("t0\nmounting an E01 image")).await.unwrap();
        assert_eq!(hits[0].text, "mounting an E01 image");
        assert!(hits[0].score > 0.99);
    }

    #[tokio::test]
    async fn results_carry_everything_needed_to_render_without_a_second_lookup() {
        let core = test_core().await;
        let src_id = seed(&core, &[("body text", "concept", &["a", "b"])]).await;
        let hits = core.search(&q("t0\nbody text")).await.unwrap();
        assert_eq!(hits[0].corpus_id, src_id);
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
        let core = test_core_with_rerank().await;
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
    async fn the_per_source_cap_does_not_starve_the_reranker() {
        let (core, reranker) = crate::core::test_support::test_core_counting_reranked_docs().await;
        for name in ["one", "two"] {
            let texts: Vec<String> = (0..20).map(|i| format!("{name} chunk {i}")).collect();
            let refs: Vec<(&str, &str, &[&str])> =
                texts.iter().map(|t| (t.as_str(), "c", &[][..])).collect();
            seed_from(&core, name, &refs).await;
        }

        let query = q("anything");
        let hits = core.search(&query).await.unwrap();
        assert_eq!(
            hits.len(),
            query.limit,
            "the limit still decides the length"
        );
        assert!(
            reranker.docs_seen() > query.limit,
            "the reranker was handed {} candidates for a limit of {}, so it \
             could only reorder the answer it was already given",
            reranker.docs_seen(),
            query.limit
        );
    }

    #[tokio::test]
    async fn one_source_cannot_lead_the_whole_result_list() {
        let core = test_core().await;
        let hog: Vec<String> = (0..12).map(|i| format!("alpha {i}")).collect();
        let refs: Vec<(&str, &str, &[&str])> =
            hog.iter().map(|t| (t.as_str(), "c", &[][..])).collect();
        let big = seed_from(&core, "big", &refs).await;
        let small = seed_from(&core, "small", &[("alpha other", "c", &[])]).await;

        let hits = core.search(&q("t0\nalpha 0")).await.unwrap();
        let leading = &hits[..hits.len().min(MAX_PER_CORPUS + 1)];
        let from_big = leading.iter().filter(|h| h.corpus_id == big).count();
        assert!(
            from_big <= MAX_PER_CORPUS,
            "one source took {from_big} of the leading {} results",
            leading.len()
        );
        assert!(
            leading.iter().any(|h| h.corpus_id == small),
            "the crowded-out source never reached the top of the list"
        );
    }

    #[tokio::test]
    async fn a_single_source_base_still_fills_the_limit() {
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
        let ids: std::collections::HashSet<&str> =
            hits.iter().map(|h| h.artifact_id.as_str()).collect();
        assert_eq!(ids.len(), hits.len(), "a hit appeared twice");
    }

    #[tokio::test]
    async fn ask_reads_in_rank_order_rather_than_diversity_order() {
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
        use crate::vector::{SearchHit, VectorPayload};
        let hit = |chunk: &str, src: &str, score: f32| SearchHit {
            payload: VectorPayload {
                artifact_id: chunk.into(),
                corpus_id: src.into(),
                text: String::new(),
                title: None,
                category: None,
                tags: vec![],
                created_at: 0,
                last_seen_at: None,
                hit_count: None,
                superseded: None,
                status: None,
                last_verified_at: None,
                superseded_by: None,
            },
            score,
            similarity: Some(score),
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
            hits.iter().map(|h| h.payload.artifact_id.clone()).collect()
        };

        assert_eq!(ids(cap_per_corpus(ranked(), 2, 3)), vec!["a1", "a2", "b1"]);
        assert_eq!(
            ids(cap_per_corpus(ranked(), 2, 4)),
            vec!["a1", "a2", "b1", "a3"],
            "a displaced hit must refill an otherwise short list"
        );
    }

    #[tokio::test]
    async fn resurface_returns_only_what_has_been_forgotten() {
        let core = test_core().await;
        seed_from(&core, "old", &[("long forgotten", "c", &[])]).await;
        seed_from(&core, "new", &[("captured just now", "c", &[])]).await;

        let cutoff = now_secs() - FORGOTTEN_AFTER_DAYS * SECONDS_PER_DAY - 1;
        sqlx::query("UPDATE artifacts SET created_at = ? WHERE text = ?")
            .bind(cutoff)
            .bind("long forgotten")
            .execute(&core.store.pool)
            .await
            .unwrap();
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
        let core = test_core().await;
        seed_from(&core, "old", &[("long forgotten", "c", &[])]).await;
        let old = now_secs() - FORGOTTEN_AFTER_DAYS * SECONDS_PER_DAY - 1;
        sqlx::query("UPDATE artifacts SET created_at = ?")
            .bind(old)
            .execute(&core.store.pool)
            .await
            .unwrap();
        reembed_all(&core).await;

        assert_eq!(core.resurface(10).await.unwrap().len(), 1);
        core.background.wait_idle().await;
        assert!(
            core.resurface(10).await.unwrap().is_empty(),
            "a chunk shown a moment ago is not forgotten"
        );
    }

    #[tokio::test]
    async fn an_empty_result_list_is_not_marked_seen() {
        let core = test_core().await;
        assert!(core.search(&q("nothing here")).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn searching_an_empty_base_returns_nothing_rather_than_failing() {
        let core = test_core().await;
        assert!(core.search(&q("anything")).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn include_deprecated_surfaces_a_deprecated_artifact() {
        let core = test_core().await;
        seed(&core, &[("alpha text", "note", &[])]).await;
        reembed_all(&core).await;
        let id = core.search(&q("alpha")).await.unwrap()[0]
            .artifact_id
            .clone();
        core.deprecate(&id).await.unwrap();

        assert!(
            core.search(&q("alpha")).await.unwrap().is_empty(),
            "a deprecated artifact must stay out of an ordinary search"
        );

        let mut opted_in = q("alpha");
        opted_in.include_deprecated = true;
        let hits = core.search(&opted_in).await.unwrap();
        assert_eq!(hits.len(), 1, "include_deprecated returned nothing");
        assert_eq!(hits[0].artifact_id, id);
        assert_eq!(hits[0].status, Some(ArtifactStatus::Deprecated));
    }

    #[tokio::test]
    async fn a_newly_embedded_artifact_is_not_already_stale() {
        let core = test_core().await;
        seed(&core, &[("alpha text", "note", &[])]).await;
        reembed_all(&core).await;

        let hit = &core.search(&q("alpha")).await.unwrap()[0];
        let stamp = hit
            .last_verified_at
            .expect("a fresh artifact carries no last_verified_at");
        assert!(
            stamp > now_secs() - 300,
            "the stamp must be the artifact's own, not epoch: {stamp}"
        );

        assert!(
            core.stale_candidates(10).await.unwrap().is_empty(),
            "an artifact ingested seconds ago is not a deprecation candidate"
        );
    }

    #[tokio::test]
    async fn verifying_restarts_the_retrieval_count() {
        let core = test_core().await;
        seed(&core, &[("alpha text", "note", &[])]).await;
        reembed_all(&core).await;
        let id = core.search(&q("alpha")).await.unwrap()[0]
            .artifact_id
            .clone();
        core.background.wait_idle().await;

        let hits_now = || async {
            core.vectors
                .stale_candidates(i64::MAX, i64::MAX, 10)
                .await
                .unwrap()
                .into_iter()
                .find(|h| h.payload.artifact_id == id)
                .and_then(|h| h.payload.hit_count)
        };
        assert_eq!(
            hits_now().await,
            Some(1),
            "the marked search was not counted"
        );

        core.verify(&id).await.unwrap();
        assert_eq!(hits_now().await, Some(0), "verify must restart the count");
    }

    #[tokio::test]
    async fn opening_a_stale_candidate_does_not_remove_it_from_the_review_list() {
        let core = test_core().await;
        seed(&core, &[("alpha text", "note", &[])]).await;
        reembed_all(&core).await;
        let id = core.store.list_all_artifact_ids().await.unwrap()[0].clone();
        core.vectors
            .set_last_verified_at(&id, 1, false)
            .await
            .unwrap();

        let listed = || async {
            core.stale_candidates(10)
                .await
                .unwrap()
                .iter()
                .any(|r| r.artifact_id == id)
        };
        assert!(listed().await, "the fixture is not a stale candidate");

        core.mark_artifact_seen(&id);
        core.background.wait_idle().await;

        assert!(
            listed().await,
            "reading a candidate took it off the list that offered it"
        );
    }

    #[tokio::test]
    async fn resurfacing_does_not_count_as_a_retrieval() {
        let core = test_core().await;
        seed_from(&core, "old", &[("long forgotten", "c", &[])]).await;
        let old = now_secs() - FORGOTTEN_AFTER_DAYS * SECONDS_PER_DAY - 1;
        sqlx::query("UPDATE artifacts SET created_at = ?")
            .bind(old)
            .execute(&core.store.pool)
            .await
            .unwrap();
        reembed_all(&core).await;
        let id = core.store.list_all_artifact_ids().await.unwrap()[0].clone();
        core.vectors
            .set_last_verified_at(&id, 1, false)
            .await
            .unwrap();

        assert_eq!(core.resurface(10).await.unwrap().len(), 1);
        core.background.wait_idle().await;

        assert!(
            core.stale_candidates(10)
                .await
                .unwrap()
                .iter()
                .any(|r| r.artifact_id == id),
            "being drawn at random counted as a retrieval"
        );
    }

    #[tokio::test]
    async fn a_deprecated_artifact_is_not_offered_as_forgotten() {
        let core = test_core().await;
        seed_from(
            &core,
            "old",
            &[("long forgotten", "c", &[]), ("also forgotten", "c", &[])],
        )
        .await;
        let old = now_secs() - FORGOTTEN_AFTER_DAYS * SECONDS_PER_DAY - 1;
        sqlx::query("UPDATE artifacts SET created_at = ?")
            .bind(old)
            .execute(&core.store.pool)
            .await
            .unwrap();
        reembed_all(&core).await;
        let ids = core.store.list_all_artifact_ids().await.unwrap();

        core.deprecate(&ids[0]).await.unwrap();

        let out = core.resurface(10).await.unwrap();
        assert_eq!(
            out.iter().map(|r| &r.artifact_id).collect::<Vec<_>>(),
            vec![&ids[1]],
            "the forgotten list offered an artifact that was just retired"
        );
    }

    #[tokio::test]
    async fn a_deprecated_artifact_is_not_a_neighbour() {
        let core = test_core().await;
        seed(
            &core,
            &[("alpha text", "note", &[]), ("alpha text too", "note", &[])],
        )
        .await;
        reembed_all(&core).await;
        let ids = core.store.list_all_artifact_ids().await.unwrap();
        assert_eq!(
            core.vectors.neighbours(&ids[0], 10).await.unwrap().len(),
            1,
            "the fixture has no neighbour to lose"
        );

        core.deprecate(&ids[1]).await.unwrap();

        assert!(
            core.vectors
                .neighbours(&ids[0], 10)
                .await
                .unwrap()
                .is_empty(),
            "a retired artifact is still linked from a live one"
        );
    }
}
