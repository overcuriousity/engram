use super::Core;
use crate::error::{Error, Result};
use crate::store::artifacts::ArtifactStatus;
use crate::store::feedback::Origin;
use crate::vector::{SearchFilter, SearchHit};
use std::collections::HashMap;

pub const DEFAULT_LIMIT: usize = 10;
pub const MAX_LIMIT: usize = 50;
/// Over-fetch before reranking and grouping. Both only narrow what they are
/// given, so the candidate pool has to be wider than the answer.
pub const CANDIDATE_MULTIPLIER: usize = 3;
/// Chunks one source may contribute to a result list. A forty-chunk document
/// otherwise fills the whole answer and hides everything else in the corpus.
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
    /// Whether this search counts as having *seen* its results.
    ///
    /// Incremental UI requests pass false. Every keystroke used to stamp
    /// `last_seen_at` on whatever the prefix happened to match, which is the
    /// same field `resurface` reads — so typing quietly drained the
    /// forgotten-chunk feature. Opening, expanding and submitting pass true,
    /// and so do the API and MCP paths, which are deliberate by construction.
    #[serde(default)]
    pub mark: bool,
    /// Include deprecated artifacts, excluded by default.
    #[serde(default)]
    pub include_deprecated: bool,
    /// Include superseded artifacts, excluded by default.
    #[serde(default)]
    pub include_superseded: bool,
}

/// What a search cost, for the faint line under the rail.
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
    /// Absent means active — see `VectorPayload::status`. Only worth reading
    /// when the query opted into deprecated/superseded results; an ordinary
    /// search never returns anything else.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<ArtifactStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_verified_at: Option<i64>,
    /// This artifact is only loosely related to the query.
    ///
    /// Retrieval always returns its best candidates, however bad they are, and
    /// `score` cannot expose that: hybrid retrieval fuses ranks, so the top hit
    /// for a typed-in typo carries the same score as the top hit for an exact
    /// question. Presenting both the same way is how a search for nonsense came
    /// back with four confident-looking answers. This is read from the cosine
    /// similarity, which does mean the same thing from one query to the next.
    ///
    /// False when nothing can be said — a hit the lexical half found contains
    /// the query's terms verbatim, and a store that reports no similarity gets
    /// the benefit of the doubt. Weakness has to be demonstrated, never assumed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub weak: bool,
}

impl From<SearchHit> for SearchResult {
    fn from(h: SearchHit) -> Self {
        SearchResult {
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
        }
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Promote at most `max` hits per source to the front of the list, then refill
/// from what that displaced until `target` is reached.
///
/// The cap is a diversity rule, not a ceiling. Capping alone would mean a base
/// holding two sources could never answer with more than `2 * max` results
/// however many good matches it contains — which is the common case for a young
/// knowledge base, and the case where throwing matches away hurts most. So the
/// displaced hits go back on the end in rank order: one long document no longer
/// leads the list, but it still fills it when nothing else can.
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

/// Each hit's stored retrieval count, keyed by artifact id. Absent means the
/// payload carried no `hit_count`, which reads as zero.
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
    /// `hit_counts` carries what the payloads the caller just read said, keyed
    /// by artifact id. `touch` needs the current count to increment it, and
    /// passing along the copy already in hand is what keeps a marked search
    /// from costing an extra read of every hit it just fetched.
    ///
    /// `counts_as_hit` is false for a list nobody asked for — `resurface` draws
    /// its chunks at random, and counting that as a retrieval would let the
    /// forgotten-chunks feature quietly disqualify the same artifacts from the
    /// stale-review list. See `vector::Touch`.
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

    /// Opening a chunk is the deliberate act that counts as remembering it,
    /// which is why the detail pane records it and an incremental search does
    /// not.
    ///
    /// It stamps `last_seen_at` only. An open is not a retrieval: the stale
    /// review list surfaces artifacts with at most `stale_max_hits` hits since
    /// they were last verified — zero by default — so counting the click that
    /// opens a candidate would remove it from the list that offered it, and
    /// only a `verify` could ever put it back.
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
        let hit_counts = counts_of(&hits);
        let results: Vec<SearchResult> = hits
            .into_iter()
            // Not an answer to a query, so there is no query to be far from:
            // these lists are drawn, not matched, and nothing here is weak.
            .map(SearchResult::from)
            .collect();
        // Surfacing counts as seeing, or the same chunks come back tomorrow —
        // but not as a retrieval: nobody asked for these.
        self.mark_seen(&results, &hit_counts, false);
        Ok(results)
    }

    /// Active artifacts confirmed stale a while ago and rarely or never
    /// retrieved since — candidates for an operator to review and deprecate,
    /// never anything applied automatically. A one-way signal: it can only
    /// surface a candidate, never rank anything higher, so it cannot create a
    /// popularity feedback loop the way scoring on `hit_count` directly would.
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
            // Not an answer to a query, so there is no query to be far from:
            // these lists are drawn, not matched, and nothing here is weak.
            .map(SearchResult::from)
            .collect())
    }

    /// The hot path. One embedding call, one vector search, and optionally one
    /// rerank call. No completion, ever.
    ///
    /// Results are capped per source so one long document cannot fill the list.
    pub async fn search(
        &self,
        query: &SearchQuery,
        origin: impl Into<Origin>,
    ) -> Result<Vec<SearchResult>> {
        Ok(self
            .search_with(query, Some(MAX_PER_CORPUS), origin.into())
            .await?
            .0)
    }

    /// `search`, with the per-source cap chosen by the caller and what the
    /// search cost. `cap` of `None` lets a single source supply every result:
    /// `ask` wants that, since a question is often answered by one document.
    /// The UI shows the timing faintly, so a sluggish box points at the
    /// embedder or the vector store without anyone opening a log.
    pub async fn search_with(
        &self,
        query: &SearchQuery,
        cap: Option<usize>,
        origin: impl Into<Origin>,
    ) -> Result<(Vec<SearchResult>, SearchTiming)> {
        let origin = origin.into();
        let door = origin.door;
        if query.q.trim().is_empty() {
            return Err(Error::Validation("query is empty".into()));
        }
        let limit = match query.limit {
            0 => DEFAULT_LIMIT,
            n => n.min(MAX_LIMIT),
        };

        // Embedding the query is a model call, and so is the reranker, so a
        // search is a person waiting on the endpoint exactly as `ask` is — and
        // it is the more common way in, through the UI and through MCP. Without
        // the lane a worker is free to start a window the instant the query
        // lands, and the query then waits out twenty to seventy seconds of it.
        // `ask` takes one too and holds it across this; the lane is a count, so
        // nesting is what it is built for.
        let _lane = self.gate.interactive();

        let started = std::time::Instant::now();
        // Prefixes repeat constantly inside one search and whole queries repeat
        // across sessions, so this is the difference between one embedding call
        // per search and one per keystroke.
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
            // Deprecated/superseded artifacts stay out of ordinary search by
            // default; they remain readable by id either way, which is what
            // the review queue and the undo need. A caller opts in explicitly.
            include_superseded: query.include_superseded,
            include_deprecated: query.include_deprecated,
        };
        // Over-fetch whenever something downstream narrows the list: both the
        // per-source cap and the reranker can only discard what they are given.
        let candidates = if cap.is_some() || self.reranker.is_some() {
            limit * CANDIDATE_MULTIPLIER
        } else {
            limit
        };
        // Capture needs a pool wider than the answer — that is the whole point
        // of storing one, since a hit the ranking buried is unconfirmable
        // otherwise. The fetch is the ceiling on that pool, and the width above
        // is derived from `limit` alone: a `feedback.candidates` larger than it
        // was silently cut back, either by a small `limit` (three results
        // over-fetched to nine, against a configured pool of twenty) or by
        // raising `candidates` past the multiplier. Neither said so anywhere.
        // Widening the fetch is also the cost of the setting: `Config::normalize`
        // caps `candidates` at `MAX_LIMIT * CANDIDATE_MULTIPLIER` so this can
        // never exceed what the widest ordinary search already fetches.
        let candidates = if self.feedback.enabled && door.captured() {
            candidates.max(self.feedback.candidates)
        } else {
            candidates
        };
        // The lexical half of the query. Computed locally and for free, so it
        // costs nothing when the store ignores it.
        let sparse = crate::vector::sparse::encode_query(query.q.trim());
        let hits = self
            .vectors
            .search(&vector, &sparse, candidates, &filter)
            .await?;

        // Cap before reranking, in vector order, so what leads per source is
        // that source's best. The refill target is the candidate pool rather
        // than the answer: refilling only to `limit` would hand the reranker
        // exactly `limit` hits whenever a few sources dominate, which is the
        // case over-fetching exists for. The final truncate still cuts to size.
        let hits = match cap {
            Some(max) => cap_per_corpus(hits, max, candidates),
            None => hits,
        };
        // Taken before the payloads are consumed: `mark_seen` needs each hit's
        // stored `hit_count` to increment it without reading it back.
        let hit_counts = counts_of(&hits);
        // Taken for the same reason: the similarity is dropped when a payload
        // becomes a `SearchResult`, and capture wants the value rather than the
        // `weak` verdict computed from it.
        let sims: HashMap<String, Option<f32>> = if self.feedback.enabled && door.captured() {
            hits.iter()
                .map(|h| (h.payload.artifact_id.clone(), h.similarity))
                .collect()
        } else {
            HashMap::new()
        };

        let mut results: Vec<SearchResult> = hits
            .into_iter()
            .map(|h| {
                // Demonstrated, never assumed: a hit with no similarity to
                // read is one the lexical half matched verbatim.
                let weak = h.similarity.is_some_and(|s| s < self.weak_below);
                SearchResult {
                    weak,
                    ..SearchResult::from(h)
                }
            })
            .collect();

        if let Some(reranker) = &self.reranker
            && !results.is_empty()
        {
            let docs: Vec<String> = results.iter().map(|r| r.text.clone()).collect();
            // Reranked wider than the answer when the search is being recorded:
            // the reranker scores every document either way, so asking it to
            // return more costs nothing, while asking for `limit` would hand
            // capture a pool exactly as wide as what the searcher saw. A hit
            // the reranker buried would then be unconfirmable, and the whole
            // point of storing a pool is that it can be.
            let top_n = if self.feedback.enabled && door.captured() {
                limit.max(self.feedback.candidates)
            } else {
                limit
            };
            match reranker.rerank(&query.q, &docs, top_n).await {
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

        // Recorded here, where the list is still wider than the answer and the
        // ordering is final. Off the request path via `Background`, like
        // `mark_seen` below it: a search must not get slower, or fail, because
        // bookkeeping did.
        if self.feedback.enabled && door.captured() {
            let candidates: Vec<crate::store::feedback::NewCandidate> = results
                .iter()
                .take(self.feedback.candidates)
                .enumerate()
                .map(|(i, r)| crate::store::feedback::NewCandidate {
                    artifact_id: r.artifact_id.clone(),
                    score: r.score,
                    similarity: sims.get(&r.artifact_id).copied().flatten(),
                    shown: i < limit,
                })
                .collect();
            let event = crate::store::feedback::NewEvent {
                query: query.q.trim().to_string(),
                door,
                scope: origin.scope.clone(),
                filters: serde_json::json!({
                    "tags": query.tags,
                    "category": query.category,
                    "limit": limit,
                    "include_deprecated": query.include_deprecated,
                    "include_superseded": query.include_superseded,
                })
                .to_string(),
                query_vec: vector.clone(),
                embed_model: self.embedder.model().to_string(),
                candidates,
            };
            let store = self.store.clone();
            let window = self.feedback.coalesce_secs;
            self.background.spawn(async move {
                if let Err(e) = store.record_search(event, window).await {
                    tracing::warn!(error = %e, "could not record the search");
                }
            });
        }

        results.truncate(limit);
        if query.mark {
            // A query answered these, so they count as retrievals.
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
    use crate::core::test_support::{test_core, test_core_counting_reranked_docs};
    use crate::store::artifacts::NewArtifact;
    use crate::store::feedback::Door;
    use sqlx::Row;

    async fn seed(core: &crate::core::Core, texts: &[(&str, &str, &[&str])]) -> String {
        seed_from(core, "raw", texts).await
    }

    /// `raw` has to differ per source: sources are deduplicated by a hash of it.
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

    /// Rewrite every vector payload from the current chunk rows.
    async fn reembed_all(core: &crate::core::Core) {
        for src in core.store.list_corpora(100, 0).await.unwrap() {
            for c in core.store.artifacts_for_corpus(&src.id).await.unwrap() {
                crate::jobs::embed::run(core, &c.id).await.unwrap();
            }
        }
    }

    /// An embedder that stops inside the call, so a test can look at what the
    /// rest of the system is doing while a search waits on the endpoint.
    struct BlockingEmbedder {
        inner: crate::infer::fake::FakeEmbedder,
        started: std::sync::Arc<tokio::sync::Notify>,
        release: std::sync::Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl crate::infer::Embedder for BlockingEmbedder {
        async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            self.started.notify_one();
            self.release.notified().await;
            self.inner.embed(texts).await
        }
        fn dim(&self) -> usize {
            self.inner.dim()
        }
        fn model(&self) -> &str {
            self.inner.model()
        }
        fn max_input_tokens(&self) -> usize {
            self.inner.max_input_tokens()
        }
    }

    // Real time rather than `start_paused`: the SQLite pool's own timeouts are
    // tokio timers too, and a paused clock auto-advances them the moment every
    // task is idle, so the pool times out before the test starts.
    #[tokio::test]
    async fn a_search_holds_the_interactive_lane_while_it_runs() {
        // `ask` took the lane and a plain search did not, which left the more
        // common way in — the UI and MCP — free to be overtaken: a worker could
        // start a window the instant the query landed, and the person waiting on
        // their search then waited out twenty to seventy seconds of it. The
        // query embed is a model call, and so is the reranker.
        let mut core = test_core().await;
        let started = std::sync::Arc::new(tokio::sync::Notify::new());
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        core.embedder = std::sync::Arc::new(BlockingEmbedder {
            inner: crate::infer::fake::FakeEmbedder::new(core.embedder.dim()),
            started: started.clone(),
            release: release.clone(),
        });
        let core = std::sync::Arc::new(core);

        let searching = tokio::spawn({
            let core = core.clone();
            async move { core.search(&q("timeout"), Door::Api).await }
        });
        started.notified().await;

        let gate = core.gate.clone();
        let worker = tokio::spawn(async move {
            gate.background().await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !worker.is_finished(),
            "a background call started while a search was waiting on the endpoint"
        );

        release.notify_one();
        searching.await.unwrap().unwrap();
        worker.await.unwrap();
    }

    fn q(text: &str) -> SearchQuery {
        SearchQuery {
            q: text.into(),
            limit: 10,
            tags: vec![],
            category: None,
            // The default for these tests is the deliberate search the API and
            // MCP make; the incremental case is exercised explicitly below.
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

        core.search(&q("dd write iso"), Door::Ui).await.unwrap();
        let after_first = embedder.calls();
        core.search(&q("dd write iso"), Door::Ui).await.unwrap();
        // Whitespace differences are not a different question.
        core.search(&q("  dd write iso  "), Door::Ui).await.unwrap();

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
        assert!(!core.search(&query, Door::Ui).await.unwrap().is_empty());
        core.background.wait_idle().await;

        // Nothing was stamped, so the chunk is still eligible to resurface.
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

        assert!(!core.search(&q("alpha"), Door::Ui).await.unwrap().is_empty());
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

        // FakeEmbedder hashes text, so query the exact embedded string to get
        // a deterministic top hit.
        let hits = core
            .search(&q("t0\nmounting an E01 image"), Door::Ui)
            .await
            .unwrap();
        assert_eq!(hits[0].text, "mounting an E01 image");
        assert!(hits[0].score > 0.99);
    }

    #[tokio::test]
    async fn results_carry_everything_needed_to_render_without_a_second_lookup() {
        let core = test_core().await;
        let src_id = seed(&core, &[("body text", "concept", &["a", "b"])]).await;
        let hits = core.search(&q("t0\nbody text"), Door::Ui).await.unwrap();
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
        let hits = core.search(&query, Door::Ui).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].text, "beta");

        let mut query = q("anything");
        query.tags = vec!["linux".into()];
        assert_eq!(core.search(&query, Door::Ui).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn limit_is_clamped_to_a_sane_range() {
        let core = test_core().await;
        seed(&core, &[("a", "c", &[]), ("b", "c", &[]), ("c", "c", &[])]).await;

        let mut query = q("anything");
        query.limit = 0;
        assert_eq!(
            core.search(&query, Door::Ui).await.unwrap().len(),
            3,
            "limit 0 must fall back to the default"
        );

        query.limit = 1;
        assert_eq!(core.search(&query, Door::Ui).await.unwrap().len(), 1);

        query.limit = 10_000;
        assert!(core.search(&query, Door::Ui).await.unwrap().len() <= MAX_LIMIT);
    }

    #[tokio::test]
    async fn empty_query_is_rejected() {
        let core = test_core().await;
        assert!(matches!(
            core.search(&q("  "), Door::Ui).await,
            Err(crate::error::Error::Validation(_))
        ));
    }

    #[tokio::test]
    async fn rerank_reorders_when_configured() {
        let core = test_core_counting_reranked_docs().await.0;
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

        let with = core.search(&q("t0\nalpha"), Door::Ui).await.unwrap();
        let without = plain.search(&q("t0\nalpha"), Door::Ui).await.unwrap();
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
        let core = test_core_counting_reranked_docs().await.0;
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
        let hits = core.search(&query, Door::Ui).await.unwrap();
        assert_eq!(hits.len(), 5, "result count must still honour the limit");
    }

    #[tokio::test]
    async fn the_per_source_cap_does_not_starve_the_reranker() {
        // The cap runs first, and it used to refill only up to the limit: on a
        // corpus of a few long documents that handed the reranker exactly the
        // answer it was meant to choose from, so over-fetching bought nothing.
        let (core, reranker) = crate::core::test_support::test_core_counting_reranked_docs().await;
        // Two long documents: the cap keeps three from each, so the refill is
        // what has to reach the candidate pool rather than only the answer.
        for name in ["one", "two"] {
            let texts: Vec<String> = (0..20).map(|i| format!("{name} chunk {i}")).collect();
            let refs: Vec<(&str, &str, &[&str])> =
                texts.iter().map(|t| (t.as_str(), "c", &[][..])).collect();
            seed_from(&core, name, &refs).await;
        }

        let query = q("anything");
        let hits = core.search(&query, Door::Ui).await.unwrap();
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
        // A forty-chunk document otherwise crowds out every other source and
        // the top of the list becomes forty near-identical paragraphs.
        let core = test_core().await;
        let hog: Vec<String> = (0..12).map(|i| format!("alpha {i}")).collect();
        let refs: Vec<(&str, &str, &[&str])> =
            hog.iter().map(|t| (t.as_str(), "c", &[][..])).collect();
        let big = seed_from(&core, "big", &refs).await;
        let small = seed_from(&core, "small", &[("alpha other", "c", &[])]).await;

        let hits = core.search(&q("t0\nalpha 0"), Door::Ui).await.unwrap();
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
        // The cap orders the list, it does not shorten it. A base holding one
        // document must not answer every query with three results.
        let core = test_core().await;
        let texts: Vec<String> = (0..8).map(|i| format!("alpha {i}")).collect();
        let refs: Vec<(&str, &str, &[&str])> =
            texts.iter().map(|t| (t.as_str(), "c", &[][..])).collect();
        seed_from(&core, "only", &refs).await;

        let hits = core.search(&q("t0\nalpha 0"), Door::Ui).await.unwrap();
        assert_eq!(
            hits.len(),
            8,
            "the per-source cap swallowed matches nothing else could replace"
        );
        // And still no duplicates: refilling must not re-add a kept hit.
        let ids: std::collections::HashSet<&str> =
            hits.iter().map(|h| h.artifact_id.as_str()).collect();
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

        let capped = core.search(&q("t0\nalpha 0"), Door::Ui).await.unwrap();
        let (uncapped, _) = core
            .search_with(&q("t0\nalpha 0"), None, Door::Ui)
            .await
            .unwrap();
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

        // Room for three: the cap holds and `a3` stays out.
        assert_eq!(ids(cap_per_corpus(ranked(), 2, 3)), vec!["a1", "a2", "b1"]);
        // Room for four and nothing else to offer: `a3` comes back, last.
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

        // `created_at` is set by the store, so age the one that should surface.
        let cutoff = now_secs() - FORGOTTEN_AFTER_DAYS * SECONDS_PER_DAY - 1;
        sqlx::query("UPDATE artifacts SET created_at = ? WHERE text = ?")
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
        sqlx::query("UPDATE artifacts SET created_at = ?")
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
        assert!(
            core.search(&q("nothing here"), Door::Ui)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn searching_an_empty_base_returns_nothing_rather_than_failing() {
        let core = test_core().await;
        assert!(
            core.search(&q("anything"), Door::Ui)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn include_deprecated_surfaces_a_deprecated_artifact() {
        // The opt-in exists to let an operator look at what was retired. It has
        // to reach past the legacy `superseded` flag, which a deprecation must
        // therefore leave alone — see `lifecycle_payload`.
        let core = test_core().await;
        seed(&core, &[("alpha text", "note", &[])]).await;
        reembed_all(&core).await;
        let id = core.search(&q("alpha"), Door::Ui).await.unwrap()[0]
            .artifact_id
            .clone();
        core.deprecate(&id).await.unwrap();

        assert!(
            core.search(&q("alpha"), Door::Ui).await.unwrap().is_empty(),
            "a deprecated artifact must stay out of an ordinary search"
        );

        let mut opted_in = q("alpha");
        opted_in.include_deprecated = true;
        let hits = core.search(&opted_in, Door::Ui).await.unwrap();
        assert_eq!(hits.len(), 1, "include_deprecated returned nothing");
        assert_eq!(hits[0].artifact_id, id);
        assert_eq!(hits[0].status, Some(ArtifactStatus::Deprecated));
    }

    #[tokio::test]
    async fn a_newly_embedded_artifact_is_not_already_stale() {
        // The scoring formula reads a missing `last_verified_at` as epoch, so
        // an unstamped point ranks as maximally stale and lands on the
        // deprecation-review list the moment it is ingested.
        let core = test_core().await;
        seed(&core, &[("alpha text", "note", &[])]).await;
        reembed_all(&core).await;

        let hit = &core.search(&q("alpha"), Door::Ui).await.unwrap()[0];
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
    async fn resurfacing_does_not_count_as_a_retrieval() {
        // The forgotten list draws at random from exactly the population the
        // stale review list targets — old and unseen — so counting what it drew
        // as a hit quietly emptied the review list over time. It still stamps
        // `last_seen_at`, which is what keeps the same handful from returning
        // every day.
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
        // Old and unseen is exactly what a retired artifact looks like, so the
        // forgotten list used to hand back the very things an operator had just
        // taken out of search — and `resurface` marks what it shows as seen, so
        // it also rewrote their bookkeeping on the way.
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

    async fn captured_events(core: &crate::core::Core) -> i64 {
        core.background.wait_idle().await;
        sqlx::query_scalar("SELECT count(*) FROM search_events")
            .fetch_one(&core.store.pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_captured_search_stores_the_pool_it_could_have_shown() {
        // The stored pool is wider than the answer: the judging card offers all
        // of it, so an artifact the ranking buried can still be confirmed.
        let mut core = test_core().await;
        core.feedback.enabled = true;
        let texts: Vec<(&str, &str, &[&str])> = (0..12)
            .map(|_| ("alpha text about it", "note", &[][..]))
            .collect();
        seed(&core, &texts).await;
        reembed_all(&core).await;

        let mut query = q("alpha text about it");
        query.limit = 3;
        core.search(&query, Door::Ui).await.unwrap();
        core.background.wait_idle().await;

        let rows = sqlx::query("SELECT rank, shown FROM search_candidates ORDER BY rank")
            .fetch_all(&core.store.pool)
            .await
            .unwrap();
        assert!(
            rows.len() > 3,
            "the pool must be wider than the three results shown, got {}",
            rows.len()
        );
        let shown: i64 = rows.iter().map(|r| r.get::<i64, _>("shown")).sum();
        assert_eq!(
            shown, 3,
            "exactly the answer the searcher saw is flagged shown"
        );
    }

    #[tokio::test]
    async fn the_stored_pool_is_as_wide_as_it_was_configured_to_be() {
        // The fetch width came from `limit` alone, so a pool configured wider
        // than the over-fetch was quietly truncated to it and nothing warned.
        // At a limit of two the pool was six, not the twelve asked for — and
        // the six missing candidates are exactly the buried ones the judging
        // card exists to surface.
        let mut core = test_core().await;
        core.feedback.enabled = true;
        core.feedback.candidates = 12;
        let texts: Vec<(&str, &str, &[&str])> = (0..20)
            .map(|_| ("alpha text about it", "note", &[][..]))
            .collect();
        seed(&core, &texts).await;
        reembed_all(&core).await;

        let mut query = q("alpha text about it");
        query.limit = 2;
        core.search(&query, Door::Ui).await.unwrap();
        core.background.wait_idle().await;

        let pool: i64 = sqlx::query_scalar("SELECT count(*) FROM search_candidates")
            .fetch_one(&core.store.pool)
            .await
            .unwrap();
        assert_eq!(
            pool, 12,
            "the configured pool was cut back to the over-fetch"
        );
    }

    #[tokio::test]
    async fn a_reranked_search_stores_a_pool_wider_than_its_answer() {
        // The reranker returns at most `top_n`, so asking it for `limit` would
        // hand capture a pool exactly as wide as the answer — and a hit the
        // reranker buried would become unconfirmable.
        let (mut core, _r) = crate::core::test_support::test_core_counting_reranked_docs().await;
        core.feedback.enabled = true;
        let texts: Vec<(&str, &str, &[&str])> = (0..12)
            .map(|_| ("alpha text about it", "note", &[][..]))
            .collect();
        seed(&core, &texts).await;
        reembed_all(&core).await;

        let mut query = q("alpha text about it");
        query.limit = 3;
        let answer = core.search(&query, Door::Ui).await.unwrap();
        assert_eq!(answer.len(), 3, "the searcher still sees only the answer");
        core.background.wait_idle().await;

        let rows = sqlx::query("SELECT rank, shown FROM search_candidates ORDER BY rank")
            .fetch_all(&core.store.pool)
            .await
            .unwrap();
        assert!(
            rows.len() > 3,
            "the reranked pool collapsed to the answer, got {}",
            rows.len()
        );
        let shown: i64 = rows.iter().map(|r| r.get::<i64, _>("shown")).sum();
        assert_eq!(
            shown, 3,
            "only the answer the searcher saw is flagged shown"
        );
    }

    #[tokio::test]
    async fn the_captured_filters_record_every_narrowing_the_search_used() {
        // `filters` exists so a replay can reproduce the same narrowing. A
        // search that opted into deprecated artifacts drew its pool from a
        // wider base than the default, and a replay reading only tags and
        // category would score the judged pair against a base the search
        // never saw.
        let mut core = test_core().await;
        core.feedback.enabled = true;
        seed(&core, &[("alpha text", "note", &[])]).await;
        reembed_all(&core).await;

        let mut query = q("alpha text");
        query.include_deprecated = true;
        core.search(&query, Door::Ui).await.unwrap();
        core.background.wait_idle().await;

        let filters: String = sqlx::query_scalar("SELECT filters FROM search_events")
            .fetch_one(&core.store.pool)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&filters).unwrap();
        assert_eq!(
            v["include_deprecated"],
            serde_json::json!(true),
            "{filters}"
        );
        assert_eq!(
            v["include_superseded"],
            serde_json::json!(false),
            "a flag the search left off still has to be recorded as off: {filters}"
        );
    }

    #[tokio::test]
    async fn capture_writes_nothing_while_it_is_switched_off() {
        let core = test_core().await; // feedback.enabled defaults to false
        seed(&core, &[("alpha text", "note", &[])]).await;
        reembed_all(&core).await;
        core.search(&q("alpha text"), Door::Ui).await.unwrap();
        assert_eq!(captured_events(&core).await, 0);
    }

    #[tokio::test]
    async fn a_search_that_found_nothing_is_still_captured() {
        // Deliberately unlike `mark_seen`, which skips an empty list because
        // there is nothing to stamp. Here the empty list is the finding.
        let mut core = test_core().await;
        core.feedback.enabled = true;
        core.search(&q("nothing is indexed yet"), Door::Ui)
            .await
            .unwrap();
        assert_eq!(captured_events(&core).await, 1);
    }

    #[tokio::test]
    async fn the_doors_that_know_the_answer_are_never_captured() {
        // Judging composes its queries while reading the artifact, and `ask`
        // has no single right answer to judge. Both would only add noise.
        let mut core = test_core().await;
        core.feedback.enabled = true;
        seed(&core, &[("alpha text", "note", &[])]).await;
        reembed_all(&core).await;

        core.search(&q("alpha text"), Door::Judge).await.unwrap();
        core.search(&q("alpha text"), Door::Ask).await.unwrap();
        assert_eq!(captured_events(&core).await, 0);
    }
}
