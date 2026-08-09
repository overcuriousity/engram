//! Retrieval evaluation over hand-written query/chunk pairs.
//!
//! Requires a running Qdrant and a real embedding endpoint, which is why it is
//! `#[ignore]`d — the fake embedder produces meaningless vectors, so a
//! benchmark built on it would measure nothing.
//!
//! Run with:
//!   ENGRAM_EVAL_DIR=~/engram-eval cargo test --test eval -- --ignored --nocapture
//!
//! Ranking settings come from configuration, so a sweep is a loop over
//! environment variables rather than a rebuild:
//!   ENGRAM__VECTOR__RECENCY_WEIGHT=0.0 …
//!   ENGRAM_EVAL_CAP=none              (let one document fill the whole list)
//!
//! The corpus it reads is whatever the operator actually wants to search, and
//! is not in this repository. Nothing here prints chunk text; a miss is named
//! by the leading characters of its own query.

use engram::config::Config;
use engram::core::Core;
use engram::core::search::SearchQuery;
use engram::eval::metrics::{mrr, recall_at};
use engram::eval::{EvalPair, FrozenChunk, eval_dir, load_chunks, load_pairs};
use engram::store::Store;
use engram::store::chunks::NewChunk;
use engram::vector::VectorStore;
use engram::vector::qdrant::QdrantVectors;
use std::sync::Arc;

/// Results asked of the search path per query, and the `k` in recall@k.
const LIMIT: usize = 10;

/// Its own collection, dropped before and after, so a run is never polluted by
/// the previous one or by the operator's real index.
const COLLECTION: &str = "engram_eval";

fn cap_from_env() -> Option<usize> {
    match std::env::var("ENGRAM_EVAL_CAP").ok().as_deref() {
        None => Some(engram::core::search::MAX_PER_SOURCE),
        Some("none") => None,
        Some(n) => Some(
            n.parse()
                .expect("ENGRAM_EVAL_CAP must be a number or 'none'"),
        ),
    }
}

#[tokio::test]
#[ignore]
async fn evaluate_retrieval() {
    let dir = eval_dir();
    let (chunks, pairs) = match (load_chunks(&dir), load_pairs(&dir)) {
        (Ok(c), Ok(p)) => (c, p),
        (c, p) => {
            // Not a failure: most people running the suite have no corpus, and
            // a benchmark that cannot run has nothing to report either way.
            let why = c.err().map(|e| e.to_string()).unwrap_or_default()
                + &p.err().map(|e| format!(" {e}")).unwrap_or_default();
            eprintln!(
                "no evaluation corpus at {} ({}). Set ENGRAM_EVAL_DIR and run \
                 `cargo run --bin eval-prepare` first; see \
                 docs/superpowers/specs/2026-08-09-retrieval-evaluation-design.md.",
                dir.display(),
                why.trim()
            );
            return;
        }
    };
    assert!(!chunks.is_empty(), "chunks.json is empty");
    assert!(!pairs.is_empty(), "pairs.json is empty");

    let known: std::collections::HashSet<&str> = chunks.iter().map(|c| c.id.as_str()).collect();
    // A pair naming an id no chunk has is not a hard case, it is a stale pair
    // left behind by a re-run of eval-prepare. Scored as a miss it would look
    // like a ranking problem forever.
    for p in &pairs {
        assert!(
            known.contains(p.expect.as_str()),
            "pair {:?} expects chunk {} which is not in chunks.json; \
             re-check the pairs after re-running eval-prepare",
            p.query,
            p.expect
        );
    }

    let mut cfg = Config::load(None).expect("config.toml");
    cfg.vector.collection = COLLECTION.to_string();

    let vectors = Arc::new(QdrantVectors::connect(&cfg.vector).await.unwrap());
    vectors.drop_collection().await.unwrap();
    vectors
        .ensure_collection(cfg.infer.embed.dim)
        .await
        .unwrap();

    let store = Store::memory().await.unwrap();
    let core = Core::from_config(&cfg, vectors.clone(), store);

    index(&core, &chunks).await;

    let cap = cap_from_env();
    let mut ranks: Vec<Option<usize>> = Vec::with_capacity(pairs.len());
    let mut misses: Vec<(&EvalPair, Option<usize>)> = Vec::new();

    for pair in &pairs {
        let q = SearchQuery {
            q: pair.query.clone(),
            limit: LIMIT,
            tags: vec![],
            category: None,
            // A benchmark must not stamp last_seen_at: resurfacing reads the
            // same field, and a scored run is not someone reading their notes.
            mark: false,
        };
        let results = core.search_capped(&q, cap).await.expect("search failed");
        let rank = results.iter().position(|r| r.chunk_id == pair.expect);
        if rank.is_none_or(|i| i >= LIMIT) {
            misses.push((pair, rank));
        }
        ranks.push(rank);
    }

    report(&cfg, &chunks, &pairs, &ranks, &misses, cap);
    vectors.drop_collection().await.unwrap();
}

/// Load the frozen chunks and embed them.
///
/// One store source per corpus file, because `source_id` is what the
/// per-source cap groups by — collapsing the corpus into a single source would
/// silently disable the cap and measure a different program from the one that
/// serves the search page.
async fn index(core: &Core, chunks: &[FrozenChunk]) {
    let mut by_source: std::collections::BTreeMap<&str, Vec<&FrozenChunk>> = Default::default();
    for c in chunks {
        by_source.entry(c.source.as_str()).or_default().push(c);
    }

    for (name, group) in by_source {
        // The raw text has to differ per source: sources are deduplicated by
        // a hash of it.
        let src = core
            .store
            .insert_source(&format!("eval corpus: {name}"), "eval", Some(name))
            .await
            .unwrap();
        let new: Vec<NewChunk> = group
            .iter()
            .enumerate()
            .map(|(i, c)| NewChunk {
                ordinal: i as i64,
                text: c.text.clone(),
                source_span: None,
                title: c.title.clone(),
                category: c.category.clone(),
                tags: c.tags.clone(),
                window_idx: None,
            })
            .collect();
        core.store.insert_chunks(&src.id, &new).await.unwrap();
        engram::jobs::embed::run_source(core, &src.id)
            .await
            .expect("embedding the corpus failed");
    }
}

fn report(
    cfg: &Config,
    chunks: &[FrozenChunk],
    pairs: &[EvalPair],
    ranks: &[Option<usize>],
    misses: &[(&EvalPair, Option<usize>)],
    cap: Option<usize>,
) {
    let found = ranks
        .iter()
        .filter(|r| matches!(r, Some(i) if *i < LIMIT))
        .count();
    // The settings line is part of the result. A number recorded without the
    // configuration that produced it cannot be compared against anything.
    println!(
        "\n{} queries over {} chunks   (embed {}, rerank {}, recency {}, cap {})",
        pairs.len(),
        chunks.len(),
        cfg.infer.embed.model,
        if cfg.infer.rerank.is_some() {
            "on"
        } else {
            "off"
        },
        cfg.vector.recency_weight,
        cap.map_or("none".to_string(), |c| c.to_string()),
    );
    println!(
        "recall@{LIMIT}   {:.2}   ({}/{})",
        recall_at(ranks, LIMIT),
        found,
        pairs.len()
    );
    println!("MRR         {:.2}\n", mrr(ranks));

    if misses.is_empty() {
        println!("no misses.");
        return;
    }
    // The list that is actually read. An aggregate says something moved; this
    // says what moved, which is what a knob change has to be judged on.
    println!("missed:");
    for (pair, rank) in misses {
        let q: String = pair.query.chars().take(48).collect();
        match rank {
            Some(i) => println!("  {q:<50} rank {}", i + 1),
            None => println!("  {q:<50} not returned"),
        }
    }
    println!();
}
