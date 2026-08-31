//! Retrieval evaluation over hand-written query/artifact pairs.
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
//! The ask harness runs the same way and needs the ask endpoint too:
//!   ENGRAM_EVAL_DIR=~/engram-eval cargo test --test eval evaluate_ask -- --ignored --nocapture
//! It measures citation recall, abstention accuracy and faithfulness by
//! literals. With ENGRAM_EVAL_CLAIMS=1 it also asks the synthesize endpoint to
//! trace every claim to an excerpt — one call per answered question.
//!
//! The corpus it reads is whatever the operator actually wants to search, and
//! is not in this repository. Nothing here prints artifact text; a miss is named
//! by the leading characters of its own query.
//!
//! See `docs/evaluation.md` for what the numbers mean, which parameter to sweep
//! for which of them, and what this harness cannot measure — notably anything
//! about a *sequence* of queries, since every pair is scored independently and
//! `Door::Ui` here carries no session.

use engram::config::Config;
use engram::core::Core;
use engram::core::search::SearchQuery;
use engram::eval::metrics::{mrr, recall_at};
use engram::eval::{EvalPair, FrozenArtifact, eval_dir, load_artifacts, load_pairs};
use engram::store::Store;
use engram::store::artifacts::NewArtifact;
use engram::vector::VectorStore;
use engram::vector::qdrant::QdrantVectors;
use std::collections::HashMap;
use std::sync::Arc;

/// Results asked of the search path per query, and the `k` in recall@k.
const LIMIT: usize = 10;

/// Its own collection, dropped before and after, so a run is never polluted by
/// the previous one or by the operator's real index.
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
            // Not a failure: most people running the suite have no corpus, and
            // a benchmark that cannot run has nothing to report either way.
            let why = c.err().map(|e| e.to_string()).unwrap_or_default()
                + &p.err().map(|e| format!(" {e}")).unwrap_or_default();
            eprintln!(
                "no evaluation corpus at {} ({}). Run `engram --export-eval <dir>` \
                 and set ENGRAM_EVAL_DIR to it.",
                dir.display(),
                why.trim()
            );
            return;
        }
    };
    assert!(!artifacts.is_empty(), "artifacts.json is empty");
    assert!(!pairs.is_empty(), "pairs.json is empty");

    let known: std::collections::HashSet<&str> = artifacts.iter().map(|c| c.id.as_str()).collect();
    // A pair naming an id no artifact has is not a hard case, it is a stale pair
    // left behind by an artifact deleted since the export. Scored as a miss it would look
    // like a ranking problem forever.
    for p in &pairs {
        assert!(
            known.contains(p.expect.as_str()),
            "pair {:?} expects artifact {} which is not in artifacts.json; \
             re-export with `engram --export-eval`",
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

    let translated = index(&core, &artifacts).await;

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
            include_deprecated: false,
            include_superseded: false,
            // The harness measures the configured pipeline; with no reranker
            // wired this is inert either way.
            rerank: true,
            explain: false,
        };
        let (results, _) = core
            .search_with(&q, cap, engram::store::feedback::Door::Ui)
            .await
            .expect("search failed");
        // `pair.expect` names a frozen id; the store being searched knows that
        // artifact under one it minted itself.
        let expect = translated
            .get(&pair.expect)
            .expect("every pair was checked against artifacts.json above");
        // Anything that superseded it counts too. A merge moves the knowledge
        // into a new artifact and search correctly returns that one instead, so
        // scoring only the original would report a retrieval regression that is
        // really a bookkeeping change — and the one number that says whether
        // merging helps would be unreadable exactly when it matters.
        let satisfies = engram::eval::satisfied_by(&core, expect).await;
        let rank = results
            .iter()
            .position(|r| satisfies.iter().any(|id| id == &r.artifact_id));
        if rank.is_none_or(|i| i >= LIMIT) {
            misses.push((pair, rank));
        }
        ranks.push(rank);
    }

    report(&cfg, &artifacts, &pairs, &ranks, &misses, cap);
    vectors.drop_collection().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn evaluate_ask() {
    use engram::eval::claims::{parse_claims, supported};
    use engram::eval::metrics::{Abstention, fraction_cited, fully_supported, unsupported_rate};
    use engram::infer::Completer;
    use engram::infer::prompt::{CLAIMS_SYSTEM, ask_excerpt, claims_prompt};

    let dir = eval_dir();
    let (artifacts, questions) = match (load_artifacts(&dir), engram::eval::load_questions(&dir)) {
        (Ok(a), Ok(q)) => (a, q),
        (a, q) => {
            let why = a.err().map(|e| e.to_string()).unwrap_or_default()
                + &q.err().map(|e| format!(" {e}")).unwrap_or_default();
            eprintln!(
                "no judged questions at {} ({}). Ask on /ui/ask with feedback.enabled, judge the \
                 answers, run `engram --export-eval <dir>` and set ENGRAM_EVAL_DIR to it.",
                dir.display(),
                why.trim()
            );
            return;
        }
    };
    assert!(!questions.is_empty(), "questions.json is empty");

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
    let translated = index(&core, &artifacts).await;

    let check_claims = std::env::var("ENGRAM_EVAL_CLAIMS").is_ok_and(|v| v == "1");
    let claim_checker = engram::infer::openai::HttpCompleter::for_claim_checking(
        cfg.infer
            .synthesize
            .as_ref()
            .expect("the eval harness needs [infer.synthesize]"),
    );

    let mut recall: Vec<f64> = Vec::new();
    let mut all_cited = (0usize, 0usize);
    let mut abstention: Vec<(bool, bool)> = Vec::new();
    let mut wrong_abstain: Vec<String> = Vec::new();
    let mut wrong_answer: Vec<String> = Vec::new();
    let mut unsupported_literals: Vec<usize> = Vec::new();
    let mut literal_misses: Vec<(String, Vec<String>)> = Vec::new();
    let mut claims_total = (0usize, 0usize);
    let mut answers_fully = Vec::new();

    for q in &questions {
        let out = core
            .ask(
                &engram::core::ask::AskRequest {
                    q: q.question.clone(),
                    limit: None,
                    tags: vec![],
                    category: None,
                },
                // Never recorded: a benchmark is not someone asking.
                engram::store::feedback::Door::Judge,
            )
            .await
            .expect("ask failed");
        let short: String = q.question.chars().take(48).collect();

        // Abstention.
        let expected = q.verdict == "nothing_here";
        abstention.push((expected, out.abstained));
        if expected && !out.abstained {
            wrong_answer.push(short.clone());
        }
        if !expected && out.abstained {
            wrong_abstain.push(short.clone());
        }

        // Citation recall, over right answers with carriers.
        if q.verdict == "right" && !q.expect.is_empty() {
            let mut carriers = Vec::new();
            for e in &q.expect {
                let stored = translated
                    .get(e)
                    .expect("questions.json names an artifact not in artifacts.json");
                carriers.push(engram::eval::satisfied_by(&core, stored).await);
            }
            let cited: Vec<String> = out
                .citations
                .iter()
                .map(|c| c.artifact_id.clone())
                .collect();
            let f = fraction_cited(&carriers, &cited);
            recall.push(f);
            all_cited.1 += 1;
            if f >= 1.0 {
                all_cited.0 += 1;
            }
        }

        // Faithfulness, over answered questions.
        if !out.abstained && !out.citations.is_empty() {
            let excerpts: Vec<String> = out
                .citations
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    ask_excerpt(i + 1, c.title.as_deref().unwrap_or_default(), &c.text, &[])
                })
                .collect();
            // Read off the response rather than recomputed here. `ask` now runs
            // the check itself against the blocks it actually sent, so counting
            // it a second time from a reconstruction of those blocks would let
            // the harness and the page disagree about the same answer.
            unsupported_literals.push(out.unsupported.len());
            if !out.unsupported.is_empty() {
                literal_misses.push((short.clone(), out.unsupported.clone()));
            }
            if check_claims {
                let reply = claim_checker
                    .complete(CLAIMS_SYSTEM, &claims_prompt(&out.answer, &excerpts))
                    .await
                    .expect("claim check failed");
                match parse_claims(&reply, excerpts.len()) {
                    Ok(claims) => {
                        let (s, t) = supported(&claims);
                        claims_total.0 += s;
                        claims_total.1 += t;
                        answers_fully.push(t - s);
                    }
                    Err(e) => eprintln!("  claim check unreadable for {short:?}: {e}"),
                }
            }
        }
    }

    // The settings line is part of the result, as it is for retrieval.
    println!(
        "\n{} questions over {} artifacts   (ask {}, embed {}, claims {})",
        questions.len(),
        artifacts.len(),
        cfg.infer
            .ask
            .as_ref()
            .expect("the eval harness needs [infer.ask]")
            .model,
        cfg.infer.embed.model,
        if check_claims { "on" } else { "off" }
    );
    if !recall.is_empty() {
        println!(
            "citation recall   {:.2}   (all carriers cited {}/{})",
            recall.iter().sum::<f64>() / recall.len() as f64,
            all_cited.0,
            all_cited.1
        );
    }
    let t = Abstention::tally(&abstention);
    println!(
        "abstained when it should   {}/{}\nanswered when it should    {}/{}",
        t.should_and_did,
        t.should_and_did + t.should_and_did_not,
        t.should_not_did_not,
        t.should_not_did_not + t.should_not_did
    );
    let (clean, answered) = fully_supported(&unsupported_literals);
    println!(
        "answers with no unsupported literal   {clean}/{answered}\nunsupported rate   {:.2}",
        unsupported_rate(&unsupported_literals)
    );
    if check_claims {
        let (fc, fa) = fully_supported(&answers_fully);
        println!(
            "claims supported   {}/{}   (answers fully supported {fc}/{fa})",
            claims_total.0, claims_total.1
        );
    }
    for (label, list) in [
        ("answered when it should have abstained", &wrong_answer),
        ("abstained when it should have answered", &wrong_abstain),
    ] {
        if !list.is_empty() {
            println!("\n{label}:");
            for q in list {
                println!("  {q}");
            }
        }
    }
    if !literal_misses.is_empty() {
        println!("\nunsupported literals:");
        for (q, lits) in &literal_misses {
            println!("  {q:<50} {}", lits.join(" · "));
        }
    }
    println!();
    vectors.drop_collection().await.unwrap();
}

/// Load the frozen artifacts and embed them, reporting the id each one was
/// actually stored under.
///
/// `insert_artifacts` mints a fresh id per artifact, so the ids written in
/// `artifacts.json` do not exist in the store being searched. Without this map
/// every pair scores as a miss and every run reports 0.00 — which is what the
/// harness did for as long as it existed, invisibly, because it is `#[ignore]`d
/// and returns early when there is no corpus.
///
/// One store source per corpus file, because `corpus_id` is what the
/// per-source cap groups by — collapsing the corpus into a single source would
/// silently disable the cap and measure a different program from the one that
/// serves the search page.
async fn index(core: &Core, artifacts: &[FrozenArtifact]) -> HashMap<String, String> {
    let mut by_corpus: std::collections::BTreeMap<&str, Vec<&FrozenArtifact>> = Default::default();
    for c in artifacts {
        by_corpus.entry(c.source.as_str()).or_default().push(c);
    }
    let mut translated = HashMap::new();

    for (name, group) in by_corpus {
        // The raw text has to differ per source: sources are deduplicated by
        // a hash of it.
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
        // Returned in input order, which is what makes the pairing sound.
        let inserted = core.store.insert_artifacts(&src.id, &new).await.unwrap();
        for (frozen, stored) in group.iter().zip(inserted.iter()) {
            translated.insert(frozen.id.clone(), stored.id.clone());
        }
        engram::jobs::embed::run_corpus(core, &src.id)
            .await
            .expect("embedding the corpus failed");
    }
    translated
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
    // The settings line is part of the result. A number recorded without the
    // configuration that produced it cannot be compared against anything.
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

/// The harness scored every pair as a miss for as long as it existed: `index`
/// re-inserts the frozen artifacts, `insert_artifacts` assigns fresh ids, and
/// the scoring loop compared against the ids in `artifacts.json`. Nothing could
/// ever match, so every run would have reported 0.00 — invisible, because the
/// benchmark is `#[ignore]`d and returns early when there is no corpus.
///
/// This is a wiring test, not a quality one, and it runs without infrastructure:
/// the fake embedder is deterministic, so a query equal to an artifact's text
/// embeds identically and must come first.
#[tokio::test]
async fn a_pair_naming_a_frozen_artifact_can_actually_be_found() {
    let artifacts = vec![
        FrozenArtifact {
            id: "frozen-1".into(),
            source: "one.txt".into(),
            text: "the smallest addressable unit is a cluster".into(),
            title: Some("cluster".into()),
            category: None,
            tags: vec![],
        },
        FrozenArtifact {
            id: "frozen-2".into(),
            source: "two.txt".into(),
            text: "a journal records intent before the write".into(),
            title: Some("journal".into()),
            category: None,
            tags: vec![],
        },
    ];

    // Built here rather than through `test_support`, which is `#[cfg(test)]` in
    // the library and so invisible to an integration test.
    let core = Core {
        store: Store::memory().await.unwrap(),
        vectors: Arc::new(engram::vector::memory::MemoryVectors::new()),
        synthesizer: Some(Arc::new(engram::infer::fake::FakeSynthesizer::default())),
        embedder: Arc::new(engram::infer::fake::FakeEmbedder::new(8)),
        reranker: None,
        rerank_apply: vec![],
        completer: Some(Arc::new(engram::infer::fake::FakeCompleter::default())),
        judge: Some(Arc::new(engram::infer::fake::FakeCompleter::default())),
        link_judge: Some(Arc::new(engram::infer::fake::FakeCompleter::default())),
        gap_namer: Some(Arc::new(engram::infer::fake::FakeCompleter::default())),
        reminder: None,
        reaper: Some(Arc::new(engram::infer::fake::FakeCompleter::default())),
        protos: Arc::new(tokio::sync::OnceCell::new()),
        time: engram::config::TimeConfig::default(),
        reap: engram::config::ReapConfig::default(),
        generator: Some(Arc::new(engram::infer::fake::FakeCompleter::default())),
        // The harness measures the shipped default, which is one round.
        planner: None,
        describer: None,
        synthesis: engram::config::SynthesisMode::Eager,
        segment_tokens: engram::config::DEFAULT_SEGMENT_TOKENS,
        chunk_tokens: engram::config::DEFAULT_CHUNK_TOKENS,
        counter: Arc::new(engram::infer::budget::TokenCounter),
        background: Arc::new(engram::core::background::Background::default()),
        recommend: Default::default(),
        ui: Default::default(),
        clock: engram::core::context::Clock::System,
        query_cache: Arc::new(std::sync::Mutex::new(engram::core::QueryCache::new(
            engram::core::QUERY_CACHE_CAPACITY,
        ))),
        consolidate: engram::config::ConsolidateConfig::default(),
        // The harness measures the shipped cap; recency is off because the
        // fake embedder's ordering is the only thing this asserts against.
        ranking: Arc::new(std::sync::RwLock::new(
            engram::core::ranking::RankingParams {
                recency_weight: 0.0,
                per_source_cap: Some(engram::core::search::MAX_PER_CORPUS),
            },
        )),
        tuning: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        weak_below: 0.0,
        recency_half_life_days: 180,
        pinned_boost: 0.15,
        learn: engram::config::LearnConfig {
            enabled: false,
            ..Default::default()
        },
        feedback: engram::config::FeedbackConfig::default(),
        capture: engram::config::CaptureConfig::default(),
        associate: engram::config::AssociateConfig::default(),
        activation: engram::config::ActivationConfig::default(),
        promote: engram::config::PromoteConfig::default(),
        pursuit: engram::config::PursuitConfig::default(),
        schedule: engram::config::ScheduleConfig::default(),
        sitting: engram::config::SittingConfig::default(),
        sittings: std::sync::Arc::new(Default::default()),
        // The benchmark makes no background inference call, so the pacer never
        // has anything to hold back.
        gate: std::sync::Arc::new(engram::infer::gate::InferenceGate::new(
            std::time::Duration::ZERO,
        )),
        corpus_locks: Default::default(),
        lifecycle_lock: Default::default(),
        // The benchmark ingests no images, so nothing ever takes a permit.
        decodes: Arc::new(tokio::sync::Semaphore::new(
            engram::core::image::MAX_CONCURRENT_DECODES,
        )),
    };
    let translated = index(&core, &artifacts).await;

    let q = SearchQuery {
        q: "a journal records intent before the write".into(),
        limit: LIMIT,
        tags: vec![],
        category: None,
        mark: false,
        include_deprecated: false,
        include_superseded: false,
        rerank: true,
        explain: false,
    };
    let (results, _) = core
        .search_with(&q, None, engram::store::feedback::Door::Judge)
        .await
        .unwrap();

    let expect = translated
        .get("frozen-2")
        .expect("index must report the id it gave each frozen artifact");
    assert_eq!(
        results.iter().position(|r| &r.artifact_id == expect),
        Some(0),
        "the frozen id was never translated to the one the store minted"
    );

    // And a grade against an artifact that has since been merged away is still
    // satisfied by the artifact the knowledge moved into. Without this the
    // score collapses for a bookkeeping reason and the one number that says
    // whether merging helps becomes unreadable exactly when it matters.
    let other = translated.get("frozen-1").unwrap();
    let merged = core
        .store
        .insert_merged_artifact(
            &engram::store::artifacts::NewMerged {
                text: "a journal records intent before the write, in clusters".into(),
                title: Some("journal and cluster".into()),
                category: None,
                tags: vec![],
                caveats: vec![],
            },
            &[expect.clone(), other.clone()],
        )
        .await
        .unwrap();
    core.supersede(expect, &merged.id).await.unwrap();

    let satisfies = engram::eval::satisfied_by(&core, expect).await;
    assert!(
        satisfies.contains(&merged.id),
        "a grade against a merged-away artifact no longer resolves: {satisfies:?}"
    );
    assert!(
        satisfies.contains(expect),
        "the original must still satisfy its own grade"
    );
}
