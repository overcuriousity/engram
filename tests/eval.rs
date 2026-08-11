use engram::config::Config;
use engram::core::Core;
use engram::core::search::SearchQuery;
use engram::eval::metrics::{mrr, recall_at};
use engram::eval::{EvalPair, FrozenArtifact, eval_dir, load_artifacts, load_pairs};
use engram::store::Store;
use engram::store::artifacts::NewArtifact;
use engram::vector::VectorStore;
use engram::vector::qdrant::QdrantVectors;
use std::sync::Arc;

const LIMIT: usize = 10;

const COLLECTION: &str = "engram_eval";

fn cap_from_env() -> Option<usize> {
    match std::env::var("ENGRAM_EVAL_CAP").ok().as_deref() {
        None => Some(engram::core::search::MAX_PER_CORPUS),
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
    let (artifacts, pairs) = match (load_artifacts(&dir), load_pairs(&dir)) {
        (Ok(c), Ok(p)) => (c, p),
        (c, p) => {
            let why = c.err().map(|e| e.to_string()).unwrap_or_default()
                + &p.err().map(|e| format!(" {e}")).unwrap_or_default();
            eprintln!(
                "no evaluation corpus at {} ({}). Set ENGRAM_EVAL_DIR and run \
                 `cargo run --bin eval-prepare` first.",
                dir.display(),
                why.trim()
            );
            return;
        }
    };
    assert!(!artifacts.is_empty(), "artifacts.json is empty");
    assert!(!pairs.is_empty(), "pairs.json is empty");

    let known: std::collections::HashSet<&str> = artifacts.iter().map(|c| c.id.as_str()).collect();
    for p in &pairs {
        assert!(
            known.contains(p.expect.as_str()),
            "pair {:?} expects artifact {} which is not in artifacts.json; \
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

    index(&core, &artifacts).await;

    let cap = cap_from_env();
    let mut ranks: Vec<Option<usize>> = Vec::with_capacity(pairs.len());
    let mut misses: Vec<(&EvalPair, Option<usize>)> = Vec::new();

    for pair in &pairs {
        let q = SearchQuery {
            q: pair.query.clone(),
            limit: LIMIT,
            tags: vec![],
            category: None,
            mark: false,
            include_deprecated: false,
            include_superseded: false,
        };
        let results = core.search_capped(&q, cap).await.expect("search failed");
        let rank = results.iter().position(|r| r.artifact_id == pair.expect);
        if rank.is_none_or(|i| i >= LIMIT) {
            misses.push((pair, rank));
        }
        ranks.push(rank);
    }

    report(&cfg, &artifacts, &pairs, &ranks, &misses, cap);
    vectors.drop_collection().await.unwrap();
}

async fn index(core: &Core, artifacts: &[FrozenArtifact]) {
    let mut by_corpus: std::collections::BTreeMap<&str, Vec<&FrozenArtifact>> = Default::default();
    for c in artifacts {
        by_corpus.entry(c.source.as_str()).or_default().push(c);
    }

    for (name, group) in by_corpus {
        let src = core
            .store
            .insert_corpus(&format!("eval corpus: {name}"), "eval", Some(name))
            .await
            .unwrap();
        let new: Vec<NewArtifact> = group
            .iter()
            .enumerate()
            .map(|(i, c)| NewArtifact {
                ordinal: i as i64,
                text: c.text.clone(),
                corpus_span: None,
                title: c.title.clone(),
                category: c.category.clone(),
                tags: c.tags.clone(),
                segment_idx: None,
                caveats: vec![],
            })
            .collect();
        core.store.insert_artifacts(&src.id, &new).await.unwrap();
        engram::jobs::embed::run_corpus(core, &src.id)
            .await
            .expect("embedding the corpus failed");
    }
}

fn report(
    cfg: &Config,
    artifacts: &[FrozenArtifact],
    pairs: &[EvalPair],
    ranks: &[Option<usize>],
    misses: &[(&EvalPair, Option<usize>)],
    cap: Option<usize>,
) {
    let found = ranks
        .iter()
        .filter(|r| matches!(r, Some(i) if *i < LIMIT))
        .count();
    println!(
        "\n{} queries over {} artifacts   (embed {}, rerank {}, recency {}, cap {})",
        pairs.len(),
        artifacts.len(),
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
