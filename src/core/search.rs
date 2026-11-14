use super::Core;
use crate::error::{Error, Result};
use crate::store::artifacts::ArtifactStatus;
use crate::store::feedback::{Door, Origin};
use crate::vector::{SearchFilter, SearchHit};
use std::collections::HashMap;

pub const DEFAULT_LIMIT: usize = 10;
pub const MAX_LIMIT: usize = 50;
/// Reranking and grouping only narrow what they are given.
pub const CANDIDATE_MULTIPLIER: usize = 3;
/// A forty-chunk document would otherwise fill the whole answer.
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
    /// Incremental UI requests pass false: `last_seen_at` feeds `resurface`, and typing must not
    /// drain it.
    #[serde(default)]
    pub mark: bool,
    /// Include deprecated artifacts, excluded by default.
    #[serde(default)]
    pub include_deprecated: bool,
    /// Include superseded artifacts, excluded by default.
    #[serde(default)]
    pub include_superseded: bool,
    /// Ask for the reranker, where `Core::rerank_apply` allows it; the UI's typing path passes
    /// false.
    #[serde(default = "default_rerank")]
    pub rerank: bool,
    /// Render the ranking explanation, which is computed on every search either way.
    #[serde(default)]
    pub explain: bool,
}

fn default_rerank() -> bool {
    true
}

/// What priming read at one search, recorded so a replay at another lift sees what the searcher
/// saw.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Priming {
    pub activation: HashMap<String, f64>,
    pub sitting: std::collections::HashSet<String>,
    pub due: std::collections::HashSet<String>,
}

/// The gate's claim for one search: a lease others yield to, or a permit that waits; held, never
/// read.
enum Lane<'a> {
    Interactive(#[allow(dead_code)] crate::infer::gate::InteractiveLease),
    Background(#[allow(dead_code)] crate::infer::gate::BackgroundPermit<'a>),
}

/// What a search cost, for the faint line under the rail.
#[derive(Debug, Clone, Copy)]
pub struct SearchTiming {
    pub embed_ms: u128,
    pub total_ms: u128,
    /// Whether a rerank actually ran and came back; the UI's "confirmed" claim reads this, not the
    /// request.
    pub reranked: bool,
}

/// Everything a search says about itself beside the results: its timing and its explanation.
#[derive(Debug, Clone)]
pub struct SearchOutcome {
    pub timing: SearchTiming,
    pub explanation: crate::core::explain::SearchExplanation,
    /// The capture this search wrote, where the door waited for it (UI only); `None` elsewhere.
    pub event: Option<String>,
    /// What `q` was embedded as; empty when the search fell back to keywords.
    pub query_vec: Vec<f32>,
}

/// A stage of the search pipeline, in order; named by the server, which decides whether reranking
/// runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchStage {
    Embed,
    Retrieve,
    Rerank,
}

/// What one search emits while it happens; produced by `search_inner`, as the blocking door is.
#[derive(Debug, Clone)]
pub enum SearchEvent {
    /// Always first, always once.
    Stages(Vec<SearchStage>),
    /// This stage has started.
    Stage(SearchStage),
    /// Terminal, and exactly what the blocking door returns.
    Results(Vec<SearchResult>),
}

/// Deserialised too: the terminal client reads this very JSON.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SearchResult {
    pub artifact_id: String,
    pub corpus_id: String,
    pub title: Option<String>,
    pub text: String,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub score: f32,
    /// Absent means active; see `VectorPayload::status`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<ArtifactStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_verified_at: Option<i64>,
    /// Only loosely related, read from the cosine similarity, which means the same across queries.
    /// False when nothing can be said: a lexical hit or a store with no similarity. Weakness is
    /// demonstrated.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub weak: bool,
    /// A model wrote this text; never silently indistinguishable from captured text.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub model_written: bool,
    /// Written from a pursuit; the stopping rule reads it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub synthesized: bool,
    /// How many corpora it draws from — the badge's source count.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub origin_count: usize,
    /// Moved up for engagement, bounded by `associate.prime_lift`, never past rank 1, and always
    /// said.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub primed: bool,
    /// Said beside `primed`: "you were just reading this" is a different reason from "used often".
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub in_sitting: bool,
    /// An open reminder falls due inside `time.horizon_hours`; a fact whether or not `time.lift`
    /// moves it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_at: Option<i64>,
    /// In words ("in 2 h"), so every door prints the same without owning a clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_in: Option<String>,
    /// Past the cliff (see `cliff`); not reordered or dropped, but no longer claimed as an answer.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub past_cliff: bool,
    /// A done reminder's note: moved to the tail, never dropped.
    #[serde(default)]
    pub retired: bool,
    /// Comparable across queries where `score` is not; `None` for a lexical-only hit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub similarity: Option<f32>,
    /// `title` is not this text's own name: a passage's section heading, or the note's via
    /// `fill_titles`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub borrowed_name: bool,
    /// The ranked hit that recalled this one; `None` for a ranked hit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    /// What the judge said the relation is, where a link has been judged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// `None` outside a ranking; a `Default` would claim a retrieved rank of zero.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explanation: Option<crate::core::explain::HitExplanation>,
    /// Notes the dedupe judge found disagreeing; read by `finish_hits`, so the ranking never sees
    /// it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disagrees_with: Vec<crate::store::pairs::Disagreement>,
}

/// Test-only: in production every field is a decision, and a new one must break every call site.
#[cfg(test)]
impl Default for SearchResult {
    fn default() -> Self {
        Self {
            artifact_id: String::new(),
            corpus_id: String::new(),
            title: None,
            text: String::new(),
            category: None,
            tags: vec![],
            score: 0.0,
            status: None,
            superseded_by: None,
            last_verified_at: None,
            weak: false,
            model_written: false,
            synthesized: false,
            origin_count: 0,
            primed: false,
            in_sitting: false,
            due_at: None,
            due_in: None,
            past_cliff: false,
            retired: false,
            similarity: None,
            borrowed_name: false,
            via: None,
            reason: None,
            explanation: None,
            disagrees_with: Vec::new(),
        }
    }
}

/// Whitespace-split, trimmed of punctuation, at least two characters, deduped, at most eight; all
/// must match.
fn keyword_terms(q: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for w in q.split_whitespace() {
        let w = w.trim_matches(|c: char| !c.is_alphanumeric());
        if w.chars().count() < 2 || out.iter().any(|o| o.eq_ignore_ascii_case(w)) {
            continue;
        }
        out.push(w.to_string());
        if out.len() == 8 {
            break;
        }
    }
    out
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

impl From<SearchHit> for SearchResult {
    fn from(h: SearchHit) -> Self {
        let provenance = h
            .payload
            .provenance
            .as_deref()
            .map(crate::store::artifacts::Provenance::parse);
        SearchResult {
            model_written: provenance.is_some_and(|p| p.is_model_written()),
            synthesized: provenance == Some(crate::store::artifacts::Provenance::Synthesized),
            // Set here, so `fill_titles` is the only other writer and every door reads one flag.
            borrowed_name: provenance.is_some_and(|p| !p.names_its_own_text()),
            origin_count: h.payload.origin_corpora.len(),
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
            primed: false,
            in_sitting: false,
            due_at: None,
            due_in: None,
            past_cliff: false,
            retired: false,
            similarity: h.similarity,
            via: None,
            reason: None,
            disagrees_with: Vec::new(),
            explanation: None,
        }
    }
}

/// Visible to `core` so `ask` decays link weights to the same clock as the results rail.
pub(super) fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Each hit's current position, for `note_reorder` to compare against.
fn positions(results: &[SearchResult]) -> HashMap<String, usize> {
    results
        .iter()
        .enumerate()
        .map(|(i, r)| (r.artifact_id.clone(), i))
        .collect()
}

/// A stage that left a hit alone writes nothing: "kept" and "did not run" must differ.
fn note_reorder(
    results: &mut [SearchResult],
    before: &HashMap<String, usize>,
    field: impl Fn(
        &mut crate::core::explain::HitExplanation,
    ) -> &mut Option<crate::core::explain::StageEffect>,
) {
    for (to, r) in results.iter_mut().enumerate() {
        let Some(&from) = before.get(&r.artifact_id) else {
            continue;
        };
        if from == to {
            continue;
        }
        if let Some(e) = r.explanation.as_mut() {
            *field(e) = Some(crate::core::explain::StageEffect {
                from: Some(from),
                to: Some(to),
                delta: None,
            });
        }
    }
}

/// Kept beside the list: "displaced and came back" is not recoverable from the order.
#[derive(Debug, Default)]
struct CapReport {
    corpora_in_pool: usize,
    /// How many hits were over their cap in one of their corpora.
    displaced: usize,
    /// Always every displaced hit, since the refill targets the pool; `search_inner` counts after
    /// the truncate.
    refilled: std::collections::HashSet<String>,
}

/// At most `max` hits per source lead, then the displaced refill in rank order up to `target`.
fn cap_per_corpus(
    hits: Vec<crate::vector::SearchHit>,
    max: usize,
    target: usize,
) -> (Vec<crate::vector::SearchHit>, CapReport) {
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut all_corpora: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut kept = Vec::with_capacity(hits.len());
    let mut displaced = Vec::new();
    for h in hits {
        // A merge counts against every corpus it drew from; old points fall back to `corpus_id`.
        let keys: Vec<String> = if h.payload.origin_corpora.is_empty() {
            vec![h.payload.corpus_id.clone()]
        } else {
            h.payload.origin_corpora.clone()
        };
        all_corpora.extend(keys.iter().cloned());
        let over = keys
            .iter()
            .any(|k| seen.get(k).copied().unwrap_or(0) >= max);
        if !over {
            // Only a hit that took a place counts against its corpora.
            for k in &keys {
                *seen.entry(k.clone()).or_insert(0) += 1;
            }
            kept.push(h);
        } else {
            displaced.push(h);
        }
    }
    let mut report = CapReport {
        corpora_in_pool: all_corpora.len(),
        displaced: displaced.len(),
        refilled: Default::default(),
    };
    if kept.len() < target {
        let room = target - kept.len();
        for h in displaced.into_iter().take(room) {
            report.refilled.insert(h.payload.artifact_id.clone());
            kept.push(h);
        }
    }
    (kept, report)
}

/// Absent `hit_count` reads as zero.
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

/// The largest gap must exceed this many times the mean of the other gaps.
pub const CLIFF_FACTOR: f32 = 3.0;

/// Without a floor, a plateau of tied scores (mean gap zero) makes any difference a cliff.
pub const CLIFF_MIN_SHARE: f32 = 0.01;

/// Hits above the relevance fall, or `None`: the one gap over `CLIFF_FACTOR` × the mean of the
/// others. `scores` must be sorted descending, or a clamped negative gap manufactures a cliff.
pub fn cliff(scores: &[f32]) -> Option<usize> {
    if scores.len() < 3 {
        return None;
    }
    debug_assert!(
        scores.windows(2).all(|w| w[0] >= w[1]),
        "cliff needs its scores sorted descending: {scores:?}"
    );
    let gaps: Vec<f32> = scores.windows(2).map(|w| w[0] - w[1]).collect();
    let (at, largest) = gaps
        .iter()
        .copied()
        .enumerate()
        .fold(
            (0usize, 0.0f32),
            |best, (i, g)| if g > best.1 { (i, g) } else { best },
        );
    if largest <= 0.0 {
        return None;
    }
    // A tie is not a fall.
    if largest <= CLIFF_MIN_SHARE * scores[0].abs() {
        return None;
    }
    let others = (gaps.iter().sum::<f32>() - largest) / (gaps.len() - 1) as f32;
    (largest > CLIFF_FACTOR * others).then_some(at + 1)
}

/// Reranker scores, or else cosine similarity; never the fused rank, whose first gap is always
/// largest. A hit without a similarity is skipped, not an end to the reading.
fn cliff_scores(results: &[SearchResult], reranked: bool) -> Vec<Option<f32>> {
    if reranked {
        return results.iter().map(|r| Some(r.score)).collect();
    }
    results.iter().map(|r| r.similarity).collect()
}

/// Flag every hit from the cliff on, without reordering: gaps are measured over sorted scores and
/// the cut is carried back as a score, so what is marked stays a tail (`ask` truncates at the first
/// mark).
fn mark_past_cliff(results: &mut [SearchResult], reranked: bool) {
    let scored = cliff_scores(results, reranked);
    let mut sorted: Vec<f32> = scored.iter().flatten().copied().collect();
    sorted.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    let Some(above) = cliff(&sorted) else {
        return;
    };
    // A hit with no score is kept where it sits above the cut.
    let cut = sorted[above - 1];
    let from = scored
        .iter()
        .rposition(|s| s.is_some_and(|s| s >= cut))
        .map_or(0, |i| i + 1);
    for r in results.iter_mut().skip(from) {
        r.past_cliff = true;
        if let Some(e) = r.explanation.as_mut() {
            e.past_cliff = true;
        }
    }
}

/// Retired notes to the tail, then the cliff read over the live prefix, then the tail marked with
/// it. The marks must stay a suffix; a high-scoring retired row read with the rest would switch the
/// cliff off.
fn sink_retired_and_mark_the_cliff(results: &mut [SearchResult], reranked: bool) {
    results.sort_by_key(|r| r.retired);
    let live = results
        .iter()
        .position(|r| r.retired)
        .unwrap_or(results.len());
    mark_past_cliff(&mut results[..live], reranked);
    for r in results.iter_mut().filter(|r| r.retired) {
        r.past_cliff = true;
        if let Some(e) = r.explanation.as_mut() {
            e.past_cliff = true;
        }
    }
}

/// Activation above the capture baseline (`links::engagement_at`), so a never-opened artifact has
/// none.
fn engagement(
    rows: HashMap<String, (f64, i64, i64)>,
    at: i64,
    half_life_days: f64,
) -> HashMap<String, f64> {
    rows.into_iter()
        .map(|(id, (value, stamp, created_at))| {
            (
                id,
                crate::store::links::engagement_at(value, stamp, created_at, at, half_life_days),
            )
        })
        .collect()
}

/// Rank-based, normalised within this list. Rows 0 and 1 never move, so rank 1 is never displaced.
/// Every destination is decided against the original order in one pass, so no row borrows another's
/// budget.
fn prime(
    mut results: Vec<SearchResult>,
    activation: &HashMap<String, f64>,
    margin: f64,
    lift: usize,
    sitting: &std::collections::HashSet<String>,
    sitting_prime: bool,
    due: &std::collections::HashSet<String>,
) -> Vec<SearchResult> {
    // Gates the use of the sitting, not its recording, so the idle pass can replay with it on.
    let empty = std::collections::HashSet::new();
    let sitting = match sitting_prime {
        true => sitting,
        false => &empty,
    };
    // Marked before anything can return: `in_sitting` is a fact about the row, not the reordering.
    for r in &mut results {
        r.in_sitting = sitting.contains(&r.artifact_id);
    }
    if lift == 0 || results.len() < 3 {
        return results;
    }
    let max = activation.values().copied().fold(0.0f64, f64::max);
    if max <= 0.0 && sitting.is_empty() && due.is_empty() {
        return results;
    }
    let n = results.len();
    // The sitting is a value on the same scale, so there is one budget and one bounded lift.
    let acts: Vec<f64> = results
        .iter()
        .map(|r| {
            let act = match max > 0.0 {
                true => activation.get(&r.artifact_id).copied().unwrap_or(0.0) / max,
                false => 0.0,
            };
            match sitting.contains(&r.artifact_id) || due.contains(&r.artifact_id) {
                true => act.max(1.0),
                false => act,
            }
        })
        .collect();

    // Walked nearest first, stopping at the first predecessor not beaten by more than `margin`.
    let mut climb = vec![0usize; n];
    for (i, slot) in climb.iter_mut().enumerate().skip(2) {
        let mut c = 0usize;
        while c < lift {
            let predecessor = i - c - 1;
            if predecessor < 1 || acts[i] - acts[predecessor] <= margin {
                break;
            }
            c += 1;
        }
        *slot = c;
    }

    // The secondary key is load-bearing: a climbing row would otherwise sort behind the row it
    // passes.
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by_key(|&i| {
        let target = i - climb[i];
        let climbed = climb[i] > 0;
        (target, u8::from(!climbed), i)
    });

    let mut slots: Vec<Option<SearchResult>> = results.into_iter().map(Some).collect();
    order
        .into_iter()
        .enumerate()
        .filter_map(|(pos, i)| {
            let mut r = slots[i].take()?;
            // Only a climb is priming; a row merely displaced did not move up.
            r.primed = pos < i;
            Some(r)
        })
        .collect()
}

/// Chunks older than this are candidates for resurfacing, and a chunk shown
/// within this window counts as still remembered.
pub const FORGOTTEN_AFTER_DAYS: i64 = 30;
const SECONDS_PER_DAY: i64 = 86_400;

impl Core {
    /// Off the request path but tracked, so shutdown drains it. `hit_counts` saves rereading each
    /// hit. `counts_as_hit` is false for an unrequested list such as `resurface`; see
    /// `vector::Touch`.
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

        // Only a retrieval raises accessibility, and only with the associative layer live. Never
        // promotes.
        if counts_as_hit && self.associating() {
            let ids: Vec<String> = results.iter().map(|r| r.artifact_id.clone()).collect();
            let core = self.clone();
            let (delta, half_life) = (self.activation.retrieved, self.activation.half_life_days);
            let at = now_secs();
            self.background.spawn(async move {
                if let Err(e) = core.store.bump_activation(&ids, delta, half_life, at).await {
                    tracing::warn!(error = %e, "could not raise activation for a search");
                }
            });
        }
    }

    /// Give a heading-less passage its corpus's title on the way out, flagged with `borrowed_name`.
    /// Best-effort: a failed read costs the titles, never the results.
    pub(crate) async fn fill_titles(&self, results: &mut [SearchResult]) {
        let mut wanted: Vec<String> = results
            .iter()
            .filter(|r| r.title.is_none())
            .map(|r| r.corpus_id.clone())
            .collect();
        wanted.sort();
        wanted.dedup();
        if wanted.is_empty() {
            return;
        }
        let titles = match self.store.corpus_titles(&wanted).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(error = %e, "could not read corpus titles for untitled hits");
                return;
            }
        };
        for r in results.iter_mut().filter(|r| r.title.is_none()) {
            if let Some(t) = titles.get(&r.corpus_id) {
                r.title = Some(t.clone());
                r.borrowed_name = true;
            }
        }
    }

    /// A borrowed name and known disagreements, added once on the way out. Best-effort.
    pub(crate) async fn finish_hits(&self, results: &mut [SearchResult]) {
        self.fill_titles(results).await;
        let ids: Vec<String> = results.iter().map(|r| r.artifact_id.clone()).collect();
        match self.store.open_contradictions(&ids).await {
            Ok(rows) => {
                for r in results.iter_mut() {
                    r.disagrees_with = rows
                        .iter()
                        .filter(|d| d.artifact_id == r.artifact_id)
                        .cloned()
                        .collect();
                }
            }
            Err(e) => tracing::warn!(error = %e, "could not read disagreements; hits carry none"),
        }
    }

    /// Stamps `last_seen_at` only: counting the open as a hit would drop it from the stale list
    /// that offered it.
    pub fn mark_artifact_seen(&self, artifact_id: &str) {
        let targets = vec![crate::vector::Touch::shown(artifact_id)];
        let vectors = self.vectors.clone();
        let now = now_secs();
        self.background.spawn(async move {
            if let Err(e) = vectors.touch(&targets, now).await {
                tracing::warn!(error = %e, "could not record that a chunk was opened");
            }
        });

        // An open counts for less than a retrieval, and only with the associative layer live.
        if self.associating() {
            let ids = vec![artifact_id.to_string()];
            let core = self.clone();
            let (delta, half_life) = (self.activation.opened, self.activation.half_life_days);
            let at = now_secs();
            self.background.spawn(async move {
                if let Err(e) = core.store.bump_activation(&ids, delta, half_life, at).await {
                    tracing::warn!(error = %e, "could not raise activation for opening");
                    return;
                }
                // An open is an engagement: the one kind of bump that can promote.
                if let Err(e) = crate::jobs::promote::maybe_promote(&core, &ids, at).await {
                    tracing::warn!(error = %e, "could not check the promotion threshold");
                }
            });
        }
    }

    /// Not `mark_artifact_seen`: a model citing it is not a person having looked at it.
    pub fn mark_artifacts_cited(&self, ids: Vec<String>) {
        if ids.is_empty() || !self.associating() {
            return;
        }
        let core = self.clone();
        let (delta, half_life) = (self.activation.cited, self.activation.half_life_days);
        let at = now_secs();
        self.background.spawn(async move {
            if let Err(e) = core.store.bump_activation(&ids, delta, half_life, at).await {
                tracing::warn!(error = %e, "could not raise activation for a citation");
                return;
            }
            if let Err(e) = crate::jobs::promote::maybe_promote(&core, &ids, at).await {
                tracing::warn!(error = %e, "could not check the promotion threshold");
            }
        });
    }

    /// The interaction log every downstream faculty reads; a no-op while `[learn]` is off.
    pub fn record_interaction(&self, artifact_id: &str, via: Option<&str>, scope: Option<&str>) {
        if !self.learn.enabled {
            return;
        }
        let store = self.store.clone();
        let (id, via, scope) = (
            artifact_id.to_string(),
            via.map(str::to_string),
            scope.map(str::to_string),
        );
        let at = now_secs();
        self.background.spawn(async move {
            let kind = if via.is_some() { "pivoted" } else { "opened" };
            if let Err(e) = store
                .record_interaction(&id, kind, via.as_deref(), scope.as_deref(), at)
                .await
            {
                tracing::warn!(error = %e, "could not record that an artifact was opened");
            }
        });
    }

    /// Capped, since a tab left open overnight is not a day of reading; a no-op while `[learn]` is
    /// off.
    pub fn record_dwell(&self, artifact_id: &str, secs: i64, scope: Option<&str>) {
        if !self.learn.enabled || secs <= 0 {
            return;
        }
        let secs = secs.min(600);
        let store = self.store.clone();
        let (id, scope) = (artifact_id.to_string(), scope.map(str::to_string));
        let at = now_secs();
        self.background.spawn(async move {
            if let Err(e) = store.record_dwell(&id, secs, scope.as_deref(), at).await {
                tracing::warn!(error = %e, "could not record how long an artifact was open");
            }
        });
    }

    /// Activation less the capture baseline, decayed to now. A failure is a warning and an empty
    /// map.
    async fn engagement_now(&self, ids: &[String]) -> HashMap<String, f64> {
        let at = now_secs();
        match self.store.activation_of(ids).await {
            Ok(rows) => engagement(rows, at, self.activation.half_life_days),
            Err(e) => {
                tracing::warn!(error = %e, "could not read activation; results are unprimed");
                HashMap::new()
            }
        }
    }

    /// Lift the top candidate below the window into the last visible row, on `feedback.explore` of
    /// recorded searches. Returns it and its prior rank, so an open is charged to that rank. Chosen
    /// by hashing the event, not per keystroke.
    fn explore_swap(
        &self,
        results: &mut [SearchResult],
        q: &str,
        limit: usize,
        door: crate::store::feedback::Door,
        origin: &Origin,
    ) -> Option<(String, usize)> {
        if !self.learn.enabled
            || !door.records_opens()
            || self.feedback.explore <= 0.0
            || limit < 2
            || results.len() <= limit
            // The lifted row must be inside the recorded pool, or an open on it is refused.
            || limit >= self.feedback.candidates
        {
            return None;
        }
        // API and MCP name no event: the query and the hour, so a fixed seed does not explore the
        // same questions forever.
        let hourly;
        let seed: &str = match origin.fold_onto.as_deref() {
            Some(ev) => ev,
            None => {
                hourly = format!("{q}:{}", crate::store::now() / 3_600);
                &hourly
            }
        };
        // FNV-1a: reproducible from the journal, and no randomness dependency.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in seed.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        let share = (self.feedback.explore.clamp(0.0, 1.0) * 1000.0) as u64;
        if h % 1000 >= share {
            return None;
        }
        // The displaced row keeps its place in the pool and is merely not shown.
        let natural = limit;
        let lifted = results[natural].artifact_id.clone();
        results.swap(limit - 1, natural);
        if let Some(e) = results[limit - 1].explanation.as_mut() {
            e.explored = Some(crate::core::explain::StageEffect {
                from: Some(natural),
                to: Some(limit - 1),
                delta: None,
            });
        }
        tracing::debug!(
            artifact = %lifted,
            from = natural,
            "an exploring search lifted a hidden candidate into the last row"
        );
        Some((lifted, natural))
    }

    async fn associated(
        &self,
        results: &[SearchResult],
        filter: &SearchFilter,
        spread_max: usize,
    ) -> Vec<SearchResult> {
        let anchors: Vec<String> = results
            .iter()
            .take(self.associate.spread_from)
            .map(|r| r.artifact_id.clone())
            .collect();
        if anchors.is_empty() || spread_max == 0 {
            return Vec::new();
        }
        let links = match self
            .store
            .links_from(
                &anchors,
                &[
                    crate::store::links::LinkState::Learning,
                    crate::store::links::LinkState::Related,
                ],
                self.associate.half_life_days,
                now_secs(),
                self.associate.show_min,
                // Not `spread_max`: `links_from` truncates before anchor-to-anchor rows are
                // filtered out. Saturating `try_from`: an `as` wrap would go negative and silently
                // switch association off.
                i64::try_from(
                    spread_max.saturating_mul(self.associate.spread_from.saturating_add(1)),
                )
                .unwrap_or(i64::MAX),
            )
            .await
        {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(error = %e, "could not read links; results are unassociated");
                return Vec::new();
            }
        };

        let mut have: std::collections::HashSet<String> =
            results.iter().map(|r| r.artifact_id.clone()).collect();
        let mut out = Vec::new();
        for l in links {
            if out.len() >= spread_max {
                break;
            }
            if !have.insert(l.other.clone()) {
                continue;
            }
            // From SQLite: the row is one connection away.
            let Ok(c) = self.store.get_artifact(&l.other).await else {
                continue;
            };
            // The vector store's predicate on the same two fields.
            if !filter.tags.iter().all(|t| c.tags.contains(t))
                || filter
                    .category
                    .as_ref()
                    .is_some_and(|want| c.category.as_ref() != Some(want))
            {
                continue;
            }
            out.push(SearchResult {
                artifact_id: c.id,
                corpus_id: c.corpus_id.unwrap_or_default(),
                title: c.title,
                text: c.text,
                category: c.category,
                tags: c.tags,
                // Not a rank or similarity: it was recalled beside the list.
                score: 0.0,
                status: Some(c.status),
                superseded_by: c.superseded_by,
                last_verified_at: c.last_verified_at,
                weak: false,
                primed: false,
                in_sitting: false,
                due_at: None,
                due_in: None,
                past_cliff: false,
                retired: false,
                similarity: None,
                // From the row, as the ranked path reads it from the payload.
                borrowed_name: !c.provenance.names_its_own_text(),
                // Never ranked, so every other stage stays absent.
                disagrees_with: Vec::new(),
                explanation: Some(crate::core::explain::HitExplanation::recalled(&l.via)),
                via: Some(l.via),
                reason: l.reason,
                // From the row: `model_written` stops a paraphrase being handed back as its own
                // root.
                model_written: c.provenance.is_model_written(),
                synthesized: c.provenance == crate::store::artifacts::Provenance::Synthesized,
                origin_count: 0,
            });
        }
        out
    }

    /// See `Store::keyword_matches`; `unembedded_only` narrows to what has no vector. A failed read
    /// answers nothing.
    async fn keyword_hits(
        &self,
        q: &str,
        unembedded_only: bool,
        limit: usize,
        category: &Option<String>,
        tags: &[String],
    ) -> Vec<SearchResult> {
        let terms = keyword_terms(q);
        let found = match self
            .store
            .keyword_matches(&terms, unembedded_only, limit * 2)
            .await
        {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, "keyword search failed");
                return Vec::new();
            }
        };
        found
            .into_iter()
            .filter(|c| {
                tags.iter().all(|t| c.tags.contains(t))
                    && category
                        .as_ref()
                        .is_none_or(|want| c.category.as_ref() == Some(want))
            })
            .take(limit)
            .map(|c| SearchResult {
                artifact_id: c.id,
                corpus_id: c.corpus_id.unwrap_or_default(),
                title: c.title,
                text: c.text,
                category: c.category,
                tags: c.tags,
                // Not a rank: the caller places these.
                score: 0.0,
                status: Some(c.status),
                superseded_by: c.superseded_by,
                last_verified_at: c.last_verified_at,
                // It holds every word of the query; nothing here can demonstrate weakness.
                weak: false,
                primed: false,
                in_sitting: false,
                due_at: None,
                due_in: None,
                past_cliff: false,
                retired: false,
                similarity: None,
                borrowed_name: !c.provenance.names_its_own_text(),
                disagrees_with: Vec::new(),
                explanation: Some(crate::core::explain::HitExplanation {
                    keyword: true,
                    ..Default::default()
                }),
                via: None,
                reason: None,
                model_written: c.provenance.is_model_written(),
                synthesized: c.provenance == crate::store::artifacts::Provenance::Synthesized,
                origin_count: 0,
            })
            .collect()
    }

    /// Random rather than ranked: there is no query, and ranking would return the same answer.
    pub async fn resurface(&self, limit: usize) -> Result<Vec<SearchResult>> {
        let cutoff = now_secs() - FORGOTTEN_AFTER_DAYS * SECONDS_PER_DAY;
        let hits = self
            .vectors
            .resurface(limit.clamp(1, MAX_LIMIT), cutoff, cutoff)
            .await?;
        let hit_counts = counts_of(&hits);
        let results: Vec<SearchResult> = hits
            .into_iter()
            // Drawn, not matched: nothing here is weak.
            .map(SearchResult::from)
            .collect();
        // Counts as seeing, so they do not return tomorrow, but not as a retrieval.
        self.mark_seen(&results, &hit_counts, false);
        Ok(results)
    }

    /// Candidates for an operator to review; a one-way signal, so it creates no popularity loop.
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
            // Drawn, not matched: nothing here is weak.
            .map(SearchResult::from)
            .collect())
    }

    /// The hot path: one embedding, one vector search, optionally one rerank. No completion, ever.
    pub async fn search(
        &self,
        query: &SearchQuery,
        origin: impl Into<Origin>,
    ) -> Result<Vec<SearchResult>> {
        let cap = self
            .ranking
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .per_source_cap;
        Ok(self.search_with(query, cap, origin.into()).await?.0)
    }

    /// The vector a search just embedded `q` with; `None` if evicted meanwhile.
    pub fn cached_query_vector(&self, q: &str) -> Option<Vec<f32>> {
        let key = q.split_whitespace().collect::<Vec<_>>().join(" ");
        self.query_cache.lock().ok().and_then(|c| c.get(&key))
    }

    /// Seed the cache so a replay of stored observations spends no inference.
    pub fn remember_query_vector(&self, q: &str, vector: Vec<f32>) {
        // An empty vector is an absence spelled `unwrap_or_default()`; cached, it would break every
        // later search of that wording, since `cosine` answers 0.0 across widths.
        if vector.is_empty() {
            return;
        }
        let key = q.split_whitespace().collect::<Vec<_>>().join(" ");
        if let Ok(mut c) = self.query_cache.lock()
            && c.get(&key).is_none()
        {
            c.put(key, vector);
        }
    }

    /// `cap` of `None` lets one source supply every result, which `ask` wants. Timing goes to
    /// `server-timing`.
    pub async fn search_with(
        &self,
        query: &SearchQuery,
        cap: Option<usize>,
        origin: impl Into<Origin>,
    ) -> Result<(Vec<SearchResult>, SearchOutcome)> {
        let params = crate::core::ranking::RankingParams {
            per_source_cap: cap,
            ..*self
                .ranking
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        };
        self.search_inner(query, params, origin.into(), true, None)
            .await
    }

    /// The sweep's door, so measurement and serving are one pipeline; it takes the background lane.
    pub async fn search_with_ranking(
        &self,
        query: &SearchQuery,
        params: crate::core::ranking::RankingParams,
        origin: impl Into<Origin>,
    ) -> Result<(Vec<SearchResult>, SearchOutcome)> {
        self.search_inner(query, params, origin.into(), false, None)
            .await
    }

    /// `search_with`, reporting each stage as it starts; the pipeline is the producer.
    pub fn search_events(
        &self,
        query: SearchQuery,
        cap: Option<usize>,
        origin: Origin,
    ) -> impl tokio_stream::Stream<Item = Result<SearchEvent>> + 'static {
        let core = self.clone();
        let params = crate::core::ranking::RankingParams {
            per_source_cap: cap,
            ..*self
                .ranking
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        };
        async_stream::try_stream! {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let task = tokio::spawn(async move {
                core.search_inner(&query, params, origin, true, Some(tx)).await
            });
            // The sender drops when the search returns, so this ends after every reported stage.
            while let Some(ev) = rx.recv().await {
                yield ev;
            }
            let joined = task
                .await
                .map_err(|e| Error::Internal(format!("the search task: {e}")))?;
            yield SearchEvent::Results(joined?.0);
        }
    }

    /// `waited_on` chooses the lane and nothing else.
    async fn search_inner(
        &self,
        query: &SearchQuery,
        params: crate::core::ranking::RankingParams,
        origin: Origin,
        waited_on: bool,
        stages: Option<tokio::sync::mpsc::UnboundedSender<SearchEvent>>,
    ) -> Result<(Vec<SearchResult>, SearchOutcome)> {
        let door = origin.door;
        let cap = params.per_source_cap;
        let recency_weight = params.recency_weight;
        // A client that hung up must not fail a running search.
        let say = |ev: SearchEvent| {
            if let Some(tx) = &stages {
                let _ = tx.send(ev);
            }
        };
        // Always computed; `query.explain` decides only what is rendered.
        let mut explanation = crate::core::explain::SearchExplanation {
            capped: cap,
            ..Default::default()
        };
        let q = query.q.trim();
        if q.is_empty() {
            return Err(Error::Validation("query is empty".into()));
        }
        // The embedder's own batch ceiling: too long a query is a 400 here rather than a 502
        // downstream.
        let rendered = self.embedder.templates().render_query(q);
        let safe_limit = (self.embedder.max_input_tokens() as f32 * 0.8) as usize;
        let tokens = self.counter.count(&rendered);
        if tokens > safe_limit {
            return Err(Error::Validation(format!(
                "search text is too long ({tokens} tokens, limit is {safe_limit}) — paste less, or use Capture to store it instead"
            )));
        }
        let limit = match query.limit {
            0 => DEFAULT_LIMIT,
            n => n.min(MAX_LIMIT),
        };

        // Decided before the stage list, which promises the work that will happen.
        let reranking = query.rerank
            && params.rerank
            && match door {
                Door::Ask => self.reranks_ask(),
                // The same predicate that arms the UI's refining pass.
                _ => self.reranks_search(),
            };
        let mut will = vec![SearchStage::Embed, SearchStage::Retrieve];
        if reranking && self.reranker.is_some() {
            will.push(SearchStage::Rerank);
        }
        say(SearchEvent::Stages(will));

        // Embedding and reranking are model calls, so a person waiting takes the interactive lane;
        // a sweep takes the background turn, or it would wall off every worker.
        let _lane = match waited_on {
            true => Lane::Interactive(self.gate.interactive()),
            false => Lane::Background(self.gate.background_light().await),
        };

        say(SearchEvent::Stage(SearchStage::Embed));
        let started = std::time::Instant::now();
        // Whitespace-normalised: prefixes repeat within a search and queries across sessions.
        let key = q.split_whitespace().collect::<Vec<_>>().join(" ");
        let cached = self.query_cache.lock().ok().and_then(|c| c.get(&key));
        let vector = match cached {
            Some(v) => v,
            None => {
                // Retried only where someone waits: typing bursts get shed with 429s. The job layer
                // backs off for sweeps.
                let embedded = match waited_on {
                    true => {
                        crate::infer::retry::transiently(
                            crate::infer::retry::INTERACTIVE_BUDGET,
                            || self.embedder.embed_query(q),
                        )
                        .await
                    }
                    false => self.embedder.embed_query(q).await,
                };
                let v = match embedded {
                    Ok(v) => v,
                    // Embedder down: a person waiting gets keyword hits; a sweep does not, or it
                    // learns a different ranking.
                    Err(e)
                        if waited_on
                            && door != Door::Judge
                            && !matches!(e, Error::Validation(_)) =>
                    {
                        tracing::warn!(error = %e, "the query could not be embedded; answering on keywords");
                        let mut results = self
                            .keyword_hits(q, false, limit, &query.category, &query.tags)
                            .await;
                        if results.is_empty() {
                            return Err(e);
                        }
                        self.finish_hits(&mut results).await;
                        // Every promised stage is entered, so a client following them is not left
                        // waiting.
                        say(SearchEvent::Stage(SearchStage::Retrieve));
                        if reranking && self.reranker.is_some() {
                            say(SearchEvent::Stage(SearchStage::Rerank));
                        }
                        return Ok((
                            results,
                            SearchOutcome {
                                timing: SearchTiming {
                                    embed_ms: started.elapsed().as_millis(),
                                    total_ms: started.elapsed().as_millis(),
                                    reranked: false,
                                },
                                explanation: crate::core::explain::SearchExplanation {
                                    keyword_only: true,
                                    ..explanation
                                },
                                event: None,
                                query_vec: Vec::new(),
                            },
                        ));
                    }
                    Err(e) => return Err(e),
                };
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
            // Retired artifacts stay out unless opted in; they stay readable by id.
            include_superseded: query.include_superseded,
            include_deprecated: query.include_deprecated,
            // Narrowing to one document is the coverage check's question, not a searcher's.
            corpus_id: None,
        };
        // The cap and the reranker can only discard what they are given.
        let candidates = if cap.is_some() || reranking {
            limit * params.candidate_multiplier
        } else {
            limit
        };
        // Capture needs a pool wider than the answer; `Config::normalize` caps `candidates` at the
        // widest fetch.
        let candidates = if self.learn.enabled && door.fetches_the_capture_pool() {
            candidates.max(self.feedback.candidates)
        } else {
            candidates
        };
        explanation.candidates_fetched = candidates;
        // Computed locally and for free.
        let sparse = crate::vector::sparse::encode_query(query.q.trim());
        say(SearchEvent::Stage(SearchStage::Retrieve));
        let hits = self
            .vectors
            .search_weighted(
                &vector,
                &sparse,
                candidates,
                &filter,
                crate::vector::Recency {
                    weight: recency_weight,
                    half_life_days: params.recency_half_life_days,
                },
            )
            .await?;

        // Read before anything reorders: the cap moves displaced hits to the tail.
        let retrieved_rank: HashMap<String, usize> = hits
            .iter()
            .enumerate()
            .map(|(rank, h)| (h.payload.artifact_id.clone(), rank))
            .collect();

        // Cap before reranking, refilling to the pool so the reranker still gets more than `limit`.
        let (hits, cap_report) = match cap {
            Some(max) => cap_per_corpus(hits, max, candidates),
            None => {
                let corpora: std::collections::HashSet<&str> = hits
                    .iter()
                    .flat_map(|h| match h.payload.origin_corpora.is_empty() {
                        true => vec![h.payload.corpus_id.as_str()],
                        false => h
                            .payload
                            .origin_corpora
                            .iter()
                            .map(String::as_str)
                            .collect(),
                    })
                    .collect();
                let report = CapReport {
                    corpora_in_pool: corpora.len(),
                    ..Default::default()
                };
                (hits, report)
            }
        };
        explanation.corpora_in_pool = cap_report.corpora_in_pool;
        explanation.displaced = cap_report.displaced;
        let hit_counts = counts_of(&hits);
        let sims: HashMap<String, Option<f32>> = if self.learn.enabled && door.captured() {
            hits.iter()
                .map(|h| (h.payload.artifact_id.clone(), h.similarity))
                .collect()
        } else {
            HashMap::new()
        };

        // From the store's own configuration; recency comes off `params`, which
        // `search_with_ranking` overrides.
        let half_life_secs = params.recency_half_life_days as u64 * 86_400;
        let pinned_boost = self.pinned_boost;
        // Asked of the store: the default `search_weighted` ignores the weights, so the terms would
        // not apply.
        let scored = self.vectors.applies_scoring_formula();
        let scored_at = now_secs();
        let mut results: Vec<SearchResult> = hits
            .into_iter()
            .map(|h| {
                // A hit with no similarity was matched verbatim by the lexical half.
                let weak = h.similarity.is_some_and(|s| s < self.weak_below());
                // A refilled hit is over its cap and present only because nothing else was on
                // offer.
                let cap = match (
                    cap.is_some(),
                    cap_report.refilled.contains(&h.payload.artifact_id),
                ) {
                    (false, _) => crate::core::explain::CapEffect::NotApplied,
                    (true, true) => crate::core::explain::CapEffect::Refilled,
                    (true, false) => crate::core::explain::CapEffect::Kept,
                };
                let (recency, pinned) = match scored {
                    true => crate::core::explain::scoring_terms(
                        &h.payload,
                        scored_at,
                        recency_weight,
                        half_life_secs,
                        pinned_boost,
                        crate::vector::qdrant::PINNED_TAG,
                    ),
                    false => (None, None),
                };
                SearchResult {
                    weak,
                    explanation: Some(crate::core::explain::HitExplanation {
                        retrieved_rank: retrieved_rank.get(&h.payload.artifact_id).copied(),
                        recency,
                        pinned,
                        cap,
                        ..Default::default()
                    }),
                    ..SearchResult::from(h)
                }
            })
            .collect();

        // Unembedded text found by its words goes at the head, a few only; not on the judging door.
        if door != Door::Judge {
            let have: std::collections::HashSet<String> =
                results.iter().map(|r| r.artifact_id.clone()).collect();
            let room = (limit / 3).max(1);
            let top = results.first().map(|r| r.score).unwrap_or(1.0);
            let fresh: Vec<SearchResult> = self
                .keyword_hits(
                    q,
                    true,
                    room + have.len().min(room),
                    &query.category,
                    &query.tags,
                )
                .await
                .into_iter()
                .filter(|r| !have.contains(&r.artifact_id))
                .take(room)
                .map(|r| SearchResult { score: top, ..r })
                .collect();
            if !fresh.is_empty() {
                results.splice(0..0, fresh);
            }
        }

        self.finish_hits(&mut results).await;

        let mut reranked = false;
        // On the stage list's condition and before the emptiness check, so a named stage is always
        // entered.
        if reranking && self.reranker.is_some() {
            say(SearchEvent::Stage(SearchStage::Rerank));
        }
        if let Some(reranker) = &self.reranker
            && reranking
            && !results.is_empty()
        {
            let before = positions(&results);
            let docs: Vec<String> = results.iter().map(|r| r.text.clone()).collect();
            // Reranked wider when recorded, at no extra cost, so a buried hit stays confirmable.
            let top_n = if self.learn.enabled && door.captured() {
                limit.max(self.feedback.candidates)
            } else {
                limit
            };
            match reranker.rerank(&query.q, &docs, top_n).await {
                Ok(order) => {
                    reranked = true;
                    results = order
                        .into_iter()
                        .filter_map(|(idx, score)| {
                            results
                                .get(idx)
                                .map(|r| SearchResult { score, ..r.clone() })
                        })
                        .collect();
                    note_reorder(&mut results, &before, |e| &mut e.rerank);
                }
                // A rerank failure degrades ordering, not availability.
                Err(e) => tracing::warn!(error = %e, "rerank failed; returning vector order"),
            }
        }

        // Primed before capture and truncate, so the recorded pool is what the searcher saw; not on
        // `Ask` (its excerpt cut assumes rank order) or `Judge`. Due rows are read regardless of
        // `time.lift`.
        let due_map = {
            let ids: Vec<String> = results.iter().map(|r| r.artifact_id.clone()).collect();
            let now = now_secs();
            self.store
                .due_for(&ids, now, now + self.time.horizon_hours as i64 * 3_600)
                .await
                .unwrap_or_default()
        };
        for r in &mut results {
            r.due_at = due_map.get(&r.artifact_id).copied();
            r.due_in = r.due_at.map(crate::fmt::ago_or_ahead);
        }
        {
            let cids: Vec<String> = results.iter().map(|r| r.corpus_id.clone()).collect();
            // Logged rather than silent: an empty set is indistinguishable from "nothing is
            // retired".
            let retired = match self.store.retired_among(&cids).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = %e, "could not read retirement for this page; nothing is marked or sunk");
                    Default::default()
                }
            };
            for r in &mut results {
                r.retired = retired.contains(&r.corpus_id);
            }
        }
        let due: std::collections::HashSet<String> = match self.time.lift {
            true => due_map.keys().cloned().collect(),
            false => Default::default(),
        };
        // What a replay needs to see this list as the searcher did.
        let mut primed_with: Option<Priming> = None;
        let primes = self.associating() && !matches!(door, Door::Ask | Door::Judge);
        if primes || origin.replay.is_some() {
            let before = positions(&results);
            let ids: Vec<String> = results.iter().map(|r| r.artifact_id.clone()).collect();
            // Collected whatever the knob says, so the idle pass has evidence to move it; used only
            // where it says.
            let sitting: std::collections::HashSet<String> = origin
                .session
                .as_deref()
                .map(|s| {
                    self.sittings
                        .read(s, now_secs(), self.pursuit.idle_secs as i64)
                        .touched
                        .into_iter()
                        .collect()
                })
                .unwrap_or_default();
            // A replay is primed with what the recorded search read, not the base as it is now.
            let priming = match origin.replay.clone() {
                Some(p) => p,
                None => Priming {
                    activation: self.engagement_now(&ids).await,
                    sitting,
                    due: due.clone(),
                },
            };
            results = prime(
                results,
                &priming.activation,
                self.associate.prime_margin,
                params.prime_lift,
                &priming.sitting,
                params.sitting_prime,
                &priming.due,
            );
            note_reorder(&mut results, &before, |e| &mut e.prime);
            // Only a real search records what it read.
            if primes {
                primed_with = Some(priming);
            }
        }

        // Sunk before the capture: the recorded `rank` must be an order a searcher saw.

        // Before the window is fixed and the capture, so the lifted row is sunk, marked and
        // recorded as shown.
        let explored = self.explore_swap(&mut results, q, limit, door, &origin);

        let visible = limit.min(results.len());
        sink_retired_and_mark_the_cliff(&mut results[..visible], reranked);

        // Recorded while the list is wider than the answer, off the request path except at the UI
        // door. The band is read first so it is in the pool; it is appended after the truncate,
        // outside `limit`.
        let recalled = if self.associating() && !matches!(door, Door::Ask | Door::Judge) {
            self.associated(&results[..visible], &filter, params.spread_max)
                .await
        } else {
            Vec::new()
        };

        let mut captured_event = None;
        if self.learn.enabled && door.captured() {
            let mut candidates: Vec<crate::store::feedback::NewCandidate> = results
                .iter()
                .take(self.feedback.candidates)
                .enumerate()
                .map(|(i, r)| crate::store::feedback::NewCandidate {
                    artifact_id: r.artifact_id.clone(),
                    score: r.score,
                    similarity: sims.get(&r.artifact_id).copied().flatten(),
                    shown: i < limit,
                    band: false,
                    explored_from: explored
                        .as_ref()
                        .filter(|(id, _)| *id == r.artifact_id)
                        .map(|(_, natural)| *natural as i64),
                })
                .collect();
            candidates.extend(
                recalled
                    .iter()
                    .map(|r| crate::store::feedback::NewCandidate {
                        artifact_id: r.artifact_id.clone(),
                        score: r.score,
                        similarity: None,
                        shown: true,
                        band: true,
                        explored_from: None,
                    }),
            );
            let event = crate::store::feedback::NewEvent {
                query: query.q.trim().to_string(),
                door,
                scope: origin.scope.clone(),
                // Keeps a second tab's keystroke from folding into the first tab's search. See
                // `record_search`.
                fold_onto: origin.fold_onto.clone(),
                context: primed_with.clone(),
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
                // The pursuit stopping rule: a synthesized artifact at rank 1, not weak.
                answered: results.first().is_some_and(|r| r.synthesized && !r.weak),
            };
            let window = self.feedback.coalesce_secs;
            match door {
                // The UI waits for its capture so every row names its search; a failure is logged,
                // the results go out.
                Door::Ui | Door::App => match self.store.record_search(event, window).await {
                    Ok(id) => captured_event = Some(id),
                    Err(e) => tracing::warn!(error = %e, "could not record the search"),
                },
                _ => {
                    let store = self.store.clone();
                    self.background.spawn(async move {
                        if let Err(e) = store.record_search(event, window).await {
                            tracing::warn!(error = %e, "could not record the search");
                        }
                    });
                }
            }
        }

        results.truncate(limit);
        // Counted after the truncate, where the cap either removes something or does not.
        explanation.refilled = results
            .iter()
            .filter(|r| {
                r.explanation
                    .as_ref()
                    .is_some_and(|e| matches!(e.cap, crate::core::explain::CapEffect::Refilled))
            })
            .count();
        if query.mark {
            // A query answered these, so they count as retrievals.
            self.mark_seen(&results, &hit_counts, true);
        }
        // After the truncate, so an association only adds. Not on `Ask` (it would become source
        // material) or `Judge`.
        if !recalled.is_empty() {
            self.mark_seen(&recalled, &HashMap::new(), false);
            let from = results.len();
            results.extend(recalled);
            // The earlier pass ran before these existed.
            self.finish_hits(&mut results[from..]).await;
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
            SearchOutcome {
                timing: SearchTiming {
                    embed_ms,
                    total_ms: started.elapsed().as_millis(),
                    reranked,
                },
                explanation: crate::core::explain::SearchExplanation {
                    reranked,
                    ..explanation
                },
                event: captured_event,
                query_vec: vector,
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

    #[tokio::test]
    async fn an_empty_query_vector_is_never_remembered() {
        let core = test_core().await;
        core.remember_query_vector("wie mounte ich das image", vec![]);
        assert!(
            core.cached_query_vector("wie mounte ich das image")
                .is_none(),
            "the absence of a vector was cached as a vector"
        );
        // A real one still lands, and still wins over a later empty.
        core.remember_query_vector("wie mounte ich das image", vec![0.1, 0.2]);
        assert_eq!(
            core.cached_query_vector("wie mounte ich das image"),
            Some(vec![0.1, 0.2])
        );
        core.remember_query_vector("wie mounte ich das image", vec![]);
        assert_eq!(
            core.cached_query_vector("wie mounte ich das image"),
            Some(vec![0.1, 0.2]),
            "and cannot be displaced by one"
        );
    }

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
                title: Some(format!("t{i}")),
                category: Some(cat.to_string()),
                tags: tags.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            })
            .collect();
        let made = core.store.insert_artifacts(&src.id, &new).await.unwrap();
        for c in &made {
            crate::jobs::embed::run(core, &c.id).await.unwrap();
        }
        src.id
    }

    async fn events_of(core: &crate::core::Core, text: &str, origin: Origin) -> Vec<SearchEvent> {
        use tokio_stream::StreamExt as _;
        let s = core.search_events(q(text), None, origin);
        tokio::pin!(s);
        let mut seen = Vec::new();
        while let Some(ev) = s.next().await {
            seen.push(ev.expect("a search that was going to succeed"));
        }
        seen
    }

    #[tokio::test]
    async fn a_search_names_the_stages_it_will_run_before_it_runs_them() {
        let core = test_core().await;
        seed(&core, &[("the journal is a ring buffer", "linux", &[])]).await;
        let seen = events_of(&core, "journal", Door::Cli.into()).await;
        match &seen[0] {
            SearchEvent::Stages(v) => assert_eq!(
                v,
                &vec![SearchStage::Embed, SearchStage::Retrieve],
                "no reranker is configured, so nothing may promise a rerank"
            ),
            other => panic!("the stage list is the first frame, not {other:?}"),
        }
        assert!(matches!(seen.last(), Some(SearchEvent::Results(_))));
    }

    #[tokio::test]
    async fn a_configured_reranker_is_named_and_a_query_that_opts_out_is_not() {
        use tokio_stream::StreamExt as _;
        let (core, _calls) = test_core_counting_reranked_docs().await;
        seed(&core, &[("the journal is a ring buffer", "linux", &[])]).await;
        let seen = events_of(&core, "journal", Door::Cli.into()).await;
        assert!(
            matches!(&seen[0], SearchEvent::Stages(v) if v.contains(&SearchStage::Rerank)),
            "{seen:?}"
        );

        let opted_out = SearchQuery {
            rerank: false,
            ..q("journal")
        };
        let s = core.search_events(opted_out, None, Door::Cli.into());
        tokio::pin!(s);
        let first = s.next().await.unwrap().unwrap();
        assert!(
            matches!(&first, SearchEvent::Stages(v) if !v.contains(&SearchStage::Rerank)),
            "a query that opted out was promised a rerank: {first:?}"
        );
    }

    #[tokio::test]
    async fn every_named_stage_is_also_announced_as_it_starts() {
        let (core, _calls) = test_core_counting_reranked_docs().await;
        seed(&core, &[("the journal is a ring buffer", "linux", &[])]).await;
        let seen = events_of(&core, "journal", Door::Cli.into()).await;
        let mut named: Vec<SearchStage> = Vec::new();
        let mut started: Vec<SearchStage> = Vec::new();
        for ev in &seen {
            match ev {
                SearchEvent::Stages(v) => named = v.clone(),
                SearchEvent::Stage(s) => started.push(*s),
                SearchEvent::Results(_) => {}
            }
        }
        assert_eq!(named, started, "{seen:?}");
    }

    #[tokio::test]
    async fn a_search_that_retrieves_nothing_still_runs_every_stage_it_named() {
        let (core, _calls) = test_core_counting_reranked_docs().await;
        let seen = events_of(&core, "journal", Door::Cli.into()).await;
        let mut named: Vec<SearchStage> = Vec::new();
        let mut started: Vec<SearchStage> = Vec::new();
        let mut hits = 1;
        for ev in &seen {
            match ev {
                SearchEvent::Stages(v) => named = v.clone(),
                SearchEvent::Stage(s) => started.push(*s),
                SearchEvent::Results(r) => hits = r.len(),
            }
        }
        assert_eq!(hits, 0, "the empty base returned hits: {seen:?}");
        assert!(
            named.contains(&SearchStage::Rerank),
            "the fixture stopped configuring a reranker: {named:?}"
        );
        assert_eq!(named, started, "{seen:?}");
    }

    #[tokio::test]
    async fn a_retired_note_sinks_below_the_cliff_and_says_why() {
        let core = test_core().await;
        seed(
            &core,
            &[
                ("the invoice for august", "admin", &[]),
                ("invoice terms and conditions", "admin", &[]),
                ("invoice numbering scheme", "admin", &[]),
            ],
        )
        .await;
        let cid = seed_from(
            &core,
            "reminder",
            &[("remind me friday to send the invoice", "admin", &[])],
        )
        .await;

        let before = core.search(&q("invoice"), Door::Cli).await.unwrap();
        let pos = before
            .iter()
            .position(|r| r.corpus_id == cid)
            .expect("it places while open");

        core.store.retire_corpus(&cid, now_secs()).await.unwrap();
        let after = core.search(&q("invoice"), Door::Cli).await.unwrap();
        let row = after
            .iter()
            .find(|r| r.corpus_id == cid)
            .expect("still findable — nothing was deleted");
        assert!(row.retired, "the row says what it is");
        assert!(row.past_cliff, "and it is below the line");
        assert_eq!(
            after[after.len() - 1].corpus_id,
            cid,
            "demoted to the tail, so `ask` still truncates a tail"
        );
        assert!(after.iter().position(|r| r.corpus_id == cid).unwrap() >= pos);
    }

    #[tokio::test]
    async fn the_recorded_ranks_are_the_order_the_searcher_was_handed() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        seed(
            &core,
            &[
                ("the invoice for august", "admin", &[]),
                ("invoice terms and conditions", "admin", &[]),
                ("invoice numbering scheme", "admin", &[]),
            ],
        )
        .await;
        let cid = seed_from(
            &core,
            "reminder",
            &[("remind me friday to send the invoice", "admin", &[])],
        )
        .await;
        core.store.retire_corpus(&cid, now_secs()).await.unwrap();

        // `Door::Ui` captures inline, so the row is there when the search returns.
        let got = core.search(&q("invoice"), Door::Ui).await.unwrap();
        assert!(
            got.iter().any(|r| r.retired),
            "the fixture must actually put a retired hit in the list"
        );
        let recorded: Vec<String> = sqlx::query_scalar!(
            "SELECT artifact_id FROM search_candidates ORDER BY event_id, rank"
        )
        .fetch_all(&core.store.pool)
        .await
        .unwrap();
        let shown: Vec<String> = got.iter().map(|r| r.artifact_id.clone()).collect();
        assert_eq!(
            recorded[..shown.len()],
            shown[..],
            "the ranking that was trained on is the ranking that was read"
        );
    }

    #[tokio::test]
    async fn a_hit_says_it_is_due_inside_the_horizon_and_not_outside() {
        let mut core = test_core().await;
        seed(&core, &[("send the invoice to the client", "admin", &[])]).await;
        let first = core.search(&q("invoice"), Door::Cli).await.unwrap();
        let aid = first[0].artifact_id.clone();
        assert!(first[0].due_at.is_none());
        let now = now_secs();
        let far = core
            .store
            .insert_moment(&crate::store::moments::NewMoment {
                artifact_id: aid.clone(),
                kind: crate::store::moments::Kind::Due,
                at: Some(now + 10 * 86_400),
                tz: "UTC".into(),
                rule: None,
                source: crate::store::moments::Source::Set,
                span: None,
                series_id: None,
            })
            .await
            .unwrap();
        let out = core.search(&q("invoice"), Door::Cli).await.unwrap();
        assert!(
            out[0].due_at.is_none(),
            "ten days out is beyond the horizon"
        );
        core.store.mark_done(&far, now).await.unwrap();
        core.store
            .insert_moment(&crate::store::moments::NewMoment {
                artifact_id: aid.clone(),
                kind: crate::store::moments::Kind::Due,
                // Half an hour of slack: the badge rounds down, and a slow runner may pass a
                // second.
                at: Some(now + 2 * 3_600 + 1_800),
                tz: "UTC".into(),
                rule: None,
                source: crate::store::moments::Source::Set,
                span: None,
                series_id: None,
            })
            .await
            .unwrap();
        core.time.lift = false;
        let out = core.search(&q("invoice"), Door::Cli).await.unwrap();
        assert_eq!(out[0].due_at, Some(now + 2 * 3_600 + 1_800));
        assert_eq!(
            out[0].due_in.as_deref(),
            Some("in 2 h"),
            "the badge is a fact whatever the lift says"
        );
    }

    #[tokio::test]
    async fn the_streamed_search_returns_what_the_blocking_one_returns() {
        let core = test_core().await;
        seed(
            &core,
            &[
                ("the journal is a ring buffer", "linux", &[]),
                ("btrfs subvolume quotas", "linux", &[]),
            ],
        )
        .await;
        let blocking = core.search(&q("journal"), Door::Cli).await.unwrap();
        let streamed = events_of(&core, "journal", Door::Cli.into())
            .await
            .into_iter()
            .find_map(|ev| match ev {
                SearchEvent::Results(hits) => Some(hits),
                _ => None,
            })
            .expect("a terminal frame");
        assert_eq!(
            blocking.iter().map(|h| &h.artifact_id).collect::<Vec<_>>(),
            streamed.iter().map(|h| &h.artifact_id).collect::<Vec<_>>()
        );
    }

    async fn reembed_all(core: &crate::core::Core) {
        for src in core.store.list_corpora(100, 0).await.unwrap() {
            for c in core.store.artifacts_for_corpus(&src.id).await.unwrap() {
                crate::jobs::embed::run(core, &c.id).await.unwrap();
            }
        }
    }

    struct BlockingEmbedder {
        inner: crate::infer::fake::FakeEmbedder,
        started: std::sync::Arc<tokio::sync::Notify>,
        release: std::sync::Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl crate::infer::Embedder for BlockingEmbedder {
        async fn embed_raw(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            self.started.notify_one();
            self.release.notified().await;
            self.inner.embed_raw(texts).await
        }
        fn templates(&self) -> &crate::config::EmbedTemplates {
            self.inner.templates()
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

    // Real time, not `start_paused`: a paused clock auto-advances the SQLite pool's timeouts.
    #[tokio::test]
    async fn a_search_holds_the_interactive_lane_while_it_runs() {
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

    #[tokio::test]
    async fn text_with_no_vector_yet_is_found_by_its_words() {
        let core = test_core().await;
        seed(
            &core,
            &[("mounting an image with losetup", "procedure", &[])],
        )
        .await;
        let src = core
            .store
            .insert_corpus("raw-unembedded", "web", None)
            .await
            .unwrap();
        core.store
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    text: "Erinnere mich am Freitag an die Bereinigung der Signal-Gruppe".into(),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();

        let (got, _) = core
            .search_with(&q("Signal-Gruppe Freitag"), None, Door::Ui)
            .await
            .unwrap();
        let hit = got
            .iter()
            .find(|r| r.text.contains("Signal-Gruppe"))
            .expect("the unembedded passage was not found");
        assert!(
            hit.explanation.as_ref().is_some_and(|e| e.keyword),
            "and it says how it was found"
        );
        let (got, _) = core
            .search_with(&q("Signal-Gruppe Montag"), None, Door::Ui)
            .await
            .unwrap();
        assert!(!got.iter().any(|r| r.text.contains("Signal-Gruppe")));
    }

    #[tokio::test]
    async fn with_the_embedder_down_a_person_still_gets_what_the_words_find() {
        let mut core = test_core().await;
        seed(
            &core,
            &[("mounting an image with losetup", "procedure", &[])],
        )
        .await;
        core.embedder = std::sync::Arc::new(crate::infer::fake::FakeEmbedder::rejecting(
            "the endpoint is down",
        ));

        let (got, out) = core
            .search_with(&q("losetup"), None, Door::Ui)
            .await
            .expect("keyword hits, not an error");
        assert!(got.iter().any(|r| r.text.contains("losetup")));
        assert!(out.explanation.keyword_only);

        assert!(core.search(&q("zzzqqq"), Door::Ui).await.is_err());
        let params = *core
            .ranking
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            core.search_with_ranking(&q("losetup"), params, Door::Judge)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_rate_limited_keystroke_is_asked_again() {
        let mut core = test_core().await;
        let embedder = std::sync::Arc::new(
            crate::infer::fake::FakeEmbedder::new(core.embedder.dim()).busy_for_the_first(1),
        );
        core.embedder = embedder.clone();

        core.search(&q("mounting an image"), Door::Api)
            .await
            .expect("a search met one 429 and gave up");
        assert_eq!(embedder.calls(), 2, "the keystroke was not asked again");
    }

    #[tokio::test]
    async fn a_sweep_does_not_wait_out_a_limiter() {
        let mut core = test_core().await;
        let embedder = std::sync::Arc::new(
            crate::infer::fake::FakeEmbedder::new(core.embedder.dim()).busy_for_the_first(1),
        );
        core.embedder = embedder.clone();

        let params = *core
            .ranking
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let e = core
            .search_with_ranking(&q("mounting an image"), params, Door::Judge)
            .await
            .expect_err("the sweep waited the limiter out");
        assert!(matches!(e, Error::InferenceBusy { .. }), "{e}");
        assert_eq!(embedder.calls(), 1, "the sweep retried");
    }

    fn q(text: &str) -> SearchQuery {
        SearchQuery {
            q: text.into(),
            limit: 10,
            tags: vec![],
            category: None,
            // The deliberate search the API and MCP make; the incremental case is tested
            // explicitly.
            mark: true,
            rerank: true,
            explain: false,
            include_deprecated: false,
            include_superseded: false,
        }
    }

    #[tokio::test]
    async fn the_pool_is_as_deep_as_the_multiplier_says() {
        let core = test_core().await;
        seed(&core, &[("mounting an image", "procedure", &[])]).await;
        let mut query = q("mount");
        query.limit = 4;
        let base = *core
            .ranking
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for multiplier in [1usize, 2, 5] {
            let params = crate::core::ranking::RankingParams {
                candidate_multiplier: multiplier,
                per_source_cap: Some(2),
                ..base
            };
            let (_, outcome) = core
                .search_with_ranking(&query, params, Door::Judge)
                .await
                .unwrap();
            assert_eq!(
                outcome.explanation.candidates_fetched,
                4 * multiplier,
                "multiplier {multiplier}"
            );
        }
    }

    #[tokio::test]
    async fn the_replay_door_fetches_the_pool_the_captured_doors_do() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        seed(&core, &[("mounting an image", "procedure", &[])]).await;
        let mut query = q("mount");
        query.limit = 4;
        let base = *core
            .ranking
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let mut fetched = vec![];
        for multiplier in [1usize, 2] {
            let params = crate::core::ranking::RankingParams {
                candidate_multiplier: multiplier,
                per_source_cap: Some(2),
                ..base
            };
            let (_, judged) = core
                .search_with_ranking(&query, params, Door::Judge)
                .await
                .unwrap();
            let (_, served) = core
                .search_with_ranking(&query, params, Door::Ui)
                .await
                .unwrap();
            assert_eq!(
                judged.explanation.candidates_fetched, served.explanation.candidates_fetched,
                "the replay measured a pool the live door does not fetch (multiplier {multiplier})"
            );
            fetched.push(judged.explanation.candidates_fetched);
        }
        assert_eq!(
            fetched[0], fetched[1],
            "two rungs the floor makes identical were measured as different"
        );
        assert_eq!(fetched[0], core.feedback.candidates);
    }

    #[tokio::test]
    async fn a_search_reports_the_shape_of_its_own_pool() {
        let core = test_core().await;
        seed(&core, &[("mounting an image", "procedure", &[])]).await;

        let (_, outcome) = core
            .search_with(&q("mount"), Some(3), Door::Ui)
            .await
            .unwrap();

        assert_eq!(
            outcome.explanation.capped,
            Some(3),
            "the cap in force belongs in the explanation: a reader cannot tell \
             whether one corpus dominating is the rule failing or no rule at all"
        );
        assert!(
            outcome.explanation.candidates_fetched >= 3,
            "the pool is wider than the answer whenever something narrows it"
        );
    }

    #[tokio::test]
    async fn an_identical_query_is_embedded_once() {
        let (core, embedder) = crate::core::test_support::test_core_counting_embed_calls().await;
        seed(&core, &[("alpha text", "note", &[])]).await;
        reembed_all(&core).await;

        core.search(&q("dd write iso"), Door::Ui).await.unwrap();
        let after_first = embedder.calls();
        core.search(&q("dd write iso"), Door::Ui).await.unwrap();
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
    async fn a_deliberate_search_leaves_what_it_returned_no_more_accessible() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        seed_from(&core, "one", &[("alpha text", "note", &[])]).await;
        reembed_all(&core).await;
        let id = core.store.list_all_artifact_ids().await.unwrap()[0].clone();
        let before = core
            .store
            .activation_of(std::slice::from_ref(&id))
            .await
            .unwrap()[&id]
            .0;

        core.search(&q("alpha"), Door::Ui).await.unwrap();
        core.background.wait_idle().await;

        let after = core
            .store
            .activation_of(std::slice::from_ref(&id))
            .await
            .unwrap()[&id]
            .0;
        assert_eq!(after, before, "a retrieval raised activation");
        assert_eq!(core.activation.retrieved, 0.0, "the shipped weight");
    }

    #[tokio::test]
    async fn typing_does_not_make_what_it_happened_to_match_more_accessible() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        seed_from(&core, "one", &[("alpha text", "note", &[])]).await;
        reembed_all(&core).await;
        let id = core.store.list_all_artifact_ids().await.unwrap()[0].clone();
        let before = core
            .store
            .activation_of(std::slice::from_ref(&id))
            .await
            .unwrap()[&id]
            .0;

        let mut query = q("alpha");
        query.mark = false;
        core.search(&query, Door::Ui).await.unwrap();
        core.background.wait_idle().await;

        assert_eq!(
            core.store
                .activation_of(std::slice::from_ref(&id))
                .await
                .unwrap()[&id]
                .0,
            before
        );
    }

    #[tokio::test]
    async fn being_drawn_at_random_raises_nothing() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        seed_from(&core, "old", &[("long forgotten", "c", &[])]).await;
        sqlx::query!(
            "UPDATE artifacts SET created_at = ?",
            now_secs() - FORGOTTEN_AFTER_DAYS * SECONDS_PER_DAY - 1
        )
        .execute(&core.store.pool)
        .await
        .unwrap();
        reembed_all(&core).await;
        let id = core.store.list_all_artifact_ids().await.unwrap()[0].clone();
        let before = core
            .store
            .activation_of(std::slice::from_ref(&id))
            .await
            .unwrap()[&id]
            .0;

        core.resurface(10).await.unwrap();
        core.background.wait_idle().await;

        assert_eq!(
            core.store
                .activation_of(std::slice::from_ref(&id))
                .await
                .unwrap()[&id]
                .0,
            before
        );
    }

    #[tokio::test]
    async fn opening_an_artifact_makes_it_more_accessible_by_the_unit_the_rest_are_measured_in() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        seed_from(&core, "one", &[("alpha text", "note", &[])]).await;
        let id = core.store.list_all_artifact_ids().await.unwrap()[0].clone();
        let before = core
            .store
            .activation_of(std::slice::from_ref(&id))
            .await
            .unwrap()[&id]
            .0;

        core.mark_artifact_seen(&id);
        core.background.wait_idle().await;

        let after = core
            .store
            .activation_of(std::slice::from_ref(&id))
            .await
            .unwrap()[&id]
            .0;
        // Loose on purpose: the bump re-reads activation through decay, so the error grows with
        // wall-clock time.
        assert!((after - before - core.activation.opened).abs() < 1e-3);
        assert_eq!(core.activation.opened, 1.0);
        assert!(core.activation.retrieved < core.activation.opened);
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

        // FakeEmbedder hashes text, so query the exact embedded string for a deterministic top hit.
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
    async fn a_generation_with_rerank_off_never_calls_the_reranker() {
        let (core, reranker) = test_core_counting_reranked_docs().await;
        seed(
            &core,
            &[("alpha", "c", &[]), ("beta", "c", &[]), ("gamma", "c", &[])],
        )
        .await;
        core.ranking
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .rerank = false;
        core.search(&q("t0\nalpha"), Door::Ui).await.unwrap();
        assert_eq!(
            reranker.calls(),
            0,
            "rerank off in the generation must mean no call"
        );
        core.ranking
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .rerank = true;
        core.search(&q("t0\nalpha"), Door::Ui).await.unwrap();
        assert_eq!(reranker.calls(), 1);
    }

    #[tokio::test]
    async fn rerank_over_fetches_candidates_before_narrowing() {
        let core = test_core_counting_reranked_docs().await.0;
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
        let (core, reranker) = crate::core::test_support::test_core_counting_reranked_docs().await;
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
    async fn a_scope_of_ask_keeps_the_reranker_out_of_search() {
        let (mut core, reranker) =
            crate::core::test_support::test_core_counting_reranked_docs().await;
        core.rerank_apply = vec![crate::config::RerankApply::Ask];
        seed_from(
            &core,
            "one",
            &[("alpha", "c", &[][..]), ("beta", "c", &[][..])],
        )
        .await;

        core.search(&q("anything"), Door::Ui).await.unwrap();
        assert_eq!(
            reranker.docs_seen(),
            0,
            "the reranker was consulted on a search it is scoped out of"
        );
    }

    #[tokio::test]
    async fn an_incremental_query_skips_the_reranker() {
        let (core, reranker) = crate::core::test_support::test_core_counting_reranked_docs().await;
        seed_from(
            &core,
            "one",
            &[("alpha", "c", &[][..]), ("beta", "c", &[][..])],
        )
        .await;

        let mut query = q("anything");
        query.rerank = false;
        core.search(&query, Door::Ui).await.unwrap();
        assert_eq!(
            reranker.docs_seen(),
            0,
            "a rerank:false query still reached the reranker"
        );

        query.rerank = true;
        core.search(&query, Door::Ui).await.unwrap();
        assert!(
            reranker.docs_seen() > 0,
            "the refining pass must consult the reranker"
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

    fn hit(chunk: &str, src: &str, score: f32) -> crate::vector::SearchHit {
        crate::vector::SearchHit {
            payload: crate::vector::VectorPayload {
                artifact_id: chunk.into(),
                corpus_id: src.into(),
                ..Default::default()
            },
            score,
            similarity: Some(score),
        }
    }

    fn ids(hits: Vec<crate::vector::SearchHit>) -> Vec<String> {
        hits.iter().map(|h| h.payload.artifact_id.clone()).collect()
    }

    #[test]
    fn the_cap_leads_with_the_highest_ranked_chunk_of_each_source() {
        let ranked = || {
            vec![
                hit("a1", "a", 0.9),
                hit("a2", "a", 0.8),
                hit("b1", "b", 0.7),
                hit("a3", "a", 0.6),
            ]
        };
        assert_eq!(
            ids(cap_per_corpus(ranked(), 2, 3).0),
            vec!["a1", "a2", "b1"]
        );
        assert_eq!(
            ids(cap_per_corpus(ranked(), 2, 4).0),
            vec!["a1", "a2", "b1", "a3"],
            "a displaced hit must refill an otherwise short list"
        );
        let mut m = hit("m", "", 0.85);
        m.payload.origin_corpora = vec!["a".into(), "b".into()];
        let with_merge = vec![
            hit("a1", "a", 0.9),
            hit("a2", "a", 0.8),
            m,
            hit("b1", "b", 0.7),
        ];
        assert_eq!(
            ids(cap_per_corpus(with_merge, 2, 3).0),
            vec!["a1", "a2", "b1"]
        );
        let mut m = hit("m", "", 0.85);
        m.payload.origin_corpora = vec!["a".into(), "b".into()];
        let with_merge = vec![
            hit("a1", "a", 0.9),
            hit("a2", "a", 0.8),
            m,
            hit("b1", "b", 0.7),
            hit("b2", "b", 0.6),
        ];
        assert_eq!(
            ids(cap_per_corpus(with_merge, 2, 4).0),
            vec!["a1", "a2", "b1", "b2"],
            "a displaced hit must not spend a slot in a corpus it did not enter"
        );
    }

    #[tokio::test]
    async fn asking_for_an_explanation_does_not_change_the_order() {
        let core = test_core().await;
        seed(
            &core,
            &[
                ("mounting an image", "procedure", &[]),
                ("mounting a share", "procedure", &[]),
                ("unmounting cleanly", "procedure", &[]),
            ],
        )
        .await;

        let plain = core.search(&q("mount"), Door::Ui).await.unwrap();
        let explained = core
            .search(
                &SearchQuery {
                    explain: true,
                    ..q("mount")
                },
                Door::Ui,
            )
            .await
            .unwrap();

        let ids = |v: &[SearchResult]| -> Vec<String> {
            v.iter().map(|r| r.artifact_id.clone()).collect()
        };
        assert_eq!(
            ids(&plain),
            ids(&explained),
            "an explanation is an observation, never a stage"
        );
    }

    #[tokio::test]
    async fn a_rerank_that_moved_a_hit_says_where_it_moved_it_from() {
        // `FakeReranker` reverses its input, so every hit in a list of three moves.
        let core = test_core_counting_reranked_docs().await.0;
        seed(
            &core,
            &[("alpha", "c", &[]), ("beta", "c", &[]), ("gamma", "c", &[])],
        )
        .await;

        let hits = core.search(&q("t0\nalpha"), Door::Ui).await.unwrap();
        let moved: Vec<_> = hits
            .iter()
            .filter_map(|h| h.explanation.as_ref()?.rerank.as_ref())
            .collect();

        assert!(
            !moved.is_empty(),
            "a reranker that reordered must say so on the hits it moved"
        );
        for e in moved {
            assert_ne!(
                e.from, e.to,
                "only a hit that actually moved carries the stage: \
                 considered-and-kept and did-not-run must not render the same"
            );
        }
    }

    #[tokio::test]
    async fn every_ranked_hit_explains_its_retrieved_rank_and_the_cap() {
        let core = test_core().await;
        seed(&core, &[("mounting an image", "procedure", &[])]).await;

        let (hits, _) = core
            .search_with(&q("mount"), Some(3), Door::Ui)
            .await
            .unwrap();
        let e = hits[0]
            .explanation
            .as_ref()
            .expect("a ranked hit explains itself");

        assert_eq!(e.retrieved_rank, Some(0));
        assert!(
            e.recency.is_none() && e.pinned.is_none(),
            "`MemoryVectors` takes the default `search_weighted`, which drops \
             the weight and delegates to a plain `search`: reporting a \
             recency term here would explain a stage that never ran, on the \
             very search it claims to explain"
        );
        assert!(
            matches!(e.cap, crate::core::explain::CapEffect::Kept),
            "a hit inside its corpus's allowance was kept, not refilled"
        );
        assert!(
            e.recalled_via.is_none(),
            "a ranked hit was not recalled by anything"
        );
    }

    #[test]
    fn a_pool_one_corpus_filled_reports_a_cap_that_redistributed_nothing() {
        let only_a = vec![
            hit("a1", "a", 0.9),
            hit("a2", "a", 0.8),
            hit("a3", "a", 0.7),
            hit("a4", "a", 0.6),
        ];
        let (kept, report) = cap_per_corpus(only_a, 2, 4);

        assert_eq!(kept.len(), 4, "the refill still fills the list");
        assert_eq!(report.corpora_in_pool, 1);
        assert_eq!(report.displaced, 2);
        assert_eq!(
            report.refilled.len(),
            2,
            "every displaced hit came back: the cap redistributed nothing"
        );
        assert!(report.refilled.contains("a3") && report.refilled.contains("a4"));
    }

    #[tokio::test]
    async fn a_list_one_corpus_filled_says_the_cap_removed_nothing() {
        let core = test_core().await;
        let texts: Vec<(&str, &str, &[&str])> = (0..6)
            .map(|_| ("mounting an image", "note", &[][..]))
            .collect();
        seed(&core, &texts).await;

        let mut query = q("mounting an image");
        query.limit = 3;
        let (hits, outcome) = core.search_with(&query, Some(1), Door::Ui).await.unwrap();

        assert!(outcome.explanation.displaced > 0, "the cap had work to do");
        assert!(
            outcome.explanation.refilled > 0,
            "hits over the cap are in the answer, which is the cap failing: \
             {:?}",
            outcome.explanation
        );
        assert!(
            hits.iter()
                .any(|h| h.explanation.as_ref().is_some_and(|e| {
                    matches!(e.cap, crate::core::explain::CapEffect::Refilled)
                })),
            "the summary and the rows have to agree about which hits those are"
        );
    }

    #[tokio::test]
    async fn a_cap_the_answer_never_needed_to_break_reports_no_refill() {
        let core = test_core().await;
        let texts: Vec<(&str, &str, &[&str])> = (0..4)
            .map(|_| ("mounting an image", "note", &[][..]))
            .collect();
        seed_from(&core, "a", &texts).await;
        seed_from(&core, "b", &texts).await;

        let mut query = q("mounting an image");
        query.limit = 4;
        let (hits, outcome) = core.search_with(&query, Some(2), Door::Ui).await.unwrap();

        assert_eq!(outcome.explanation.corpora_in_pool, 2);
        assert!(outcome.explanation.displaced > 0, "the cap had work to do");
        assert_eq!(
            outcome.explanation.refilled, 0,
            "the answer is filled from inside the cap, so the rule held: {:?}",
            outcome.explanation
        );
        assert!(
            hits.iter().all(|h| h
                .explanation
                .as_ref()
                .is_some_and(|e| { matches!(e.cap, crate::core::explain::CapEffect::Kept) })),
            "and no row may claim otherwise"
        );
    }

    #[test]
    fn the_cap_moves_its_displaced_hits_to_the_tail_rather_than_dropping_them() {
        let mixed = vec![
            hit("a1", "a", 0.9),
            hit("a2", "a", 0.8),
            hit("a3", "a", 0.7),
            hit("b1", "b", 0.6),
        ];
        let (kept, report) = cap_per_corpus(mixed, 2, 4);

        assert_eq!(
            kept.iter()
                .map(|h| h.payload.artifact_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a1", "a2", "b1", "a3"],
            "the over-cap hit goes behind everything within its cap, and \
             `b1` is lifted past it — that reordering is the whole effect"
        );
        assert_eq!(report.displaced, 1);
        assert_eq!(
            report.refilled.len(),
            1,
            "with room for the whole pool the refill takes it all back; \
             reading this as `the cap redistributed nothing` would call every \
             search a failure"
        );
    }

    #[tokio::test]
    async fn resurface_returns_only_what_has_been_forgotten() {
        let core = test_core().await;
        seed_from(&core, "old", &[("long forgotten", "c", &[])]).await;
        seed_from(&core, "new", &[("captured just now", "c", &[])]).await;

        // `created_at` is set by the store, so age the one that should surface.
        let cutoff = now_secs() - FORGOTTEN_AFTER_DAYS * SECONDS_PER_DAY - 1;
        sqlx::query!(
            "UPDATE artifacts SET created_at = ? WHERE text = ?",
            cutoff,
            "long forgotten"
        )
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
        let core = test_core().await;
        seed_from(&core, "old", &[("long forgotten", "c", &[])]).await;
        let old = now_secs() - FORGOTTEN_AFTER_DAYS * SECONDS_PER_DAY - 1;
        sqlx::query!("UPDATE artifacts SET created_at = ?", old)
            .execute(&core.store.pool)
            .await
            .unwrap();
        reembed_all(&core).await;

        assert_eq!(core.resurface(10).await.unwrap().len(), 1);
        // `mark_seen` runs off the request path; wait rather than sleep.
        core.background.wait_idle().await;
        assert!(
            core.resurface(10).await.unwrap().is_empty(),
            "a chunk shown a moment ago is not forgotten"
        );
    }

    #[tokio::test]
    async fn an_empty_result_list_is_not_marked_seen() {
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
    async fn a_query_too_long_to_embed_is_rejected_before_it_reaches_the_endpoint() {
        let core = test_core().await;
        let pasted = "lorem ".repeat(6000);
        let err = core.search(&q(&pasted), Door::Ui).await.unwrap_err();
        assert!(
            matches!(err, Error::Validation(ref m) if m.contains("too long")),
            "expected a validation error about length, got {err:?}"
        );
    }

    #[tokio::test]
    async fn include_deprecated_surfaces_a_deprecated_artifact() {
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
        let core = test_core().await;
        seed_from(&core, "old", &[("long forgotten", "c", &[])]).await;
        let old = now_secs() - FORGOTTEN_AFTER_DAYS * SECONDS_PER_DAY - 1;
        sqlx::query!("UPDATE artifacts SET created_at = ?", old)
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

    async fn two_live_artifacts() -> (crate::core::Core, String, String) {
        let core = test_core().await;
        seed_from(
            &core,
            "raw",
            &[
                ("mounting a loop image", "c", &[]),
                ("mounting an iso image", "c", &[]),
            ],
        )
        .await;
        reembed_all(&core).await;
        let ids = core.store.list_all_artifact_ids().await.unwrap();
        (core, ids[0].clone(), ids[1].clone())
    }

    #[tokio::test]
    async fn a_trashed_artifact_is_not_found_by_search() {
        let (core, gone, kept) = two_live_artifacts().await;
        core.trash_artifact(&gone).await.unwrap();
        let hits = core
            .search(&q("mounting an image"), Door::Judge)
            .await
            .unwrap();
        let ids: Vec<_> = hits.iter().map(|h| h.artifact_id.as_str()).collect();
        assert!(!ids.contains(&gone.as_str()), "{ids:?}");
        assert!(ids.contains(&kept.as_str()), "{ids:?}");
        let mut wide = q("mounting an image");
        wide.include_deprecated = true;
        wide.include_superseded = true;
        let hits = core.search(&wide, Door::Judge).await.unwrap();
        assert!(hits.iter().all(|h| h.artifact_id != gone));
    }

    #[tokio::test]
    async fn a_trashed_artifact_is_absent_from_due_and_day() {
        let (core, gone, _) = two_live_artifacts().await;
        let at = crate::store::now() + 3600;
        core.edit_reminder(&gone, at, "UTC", None).await.unwrap();
        let horizon = crate::store::now() + 86_400;
        assert_eq!(core.store.open_due(0, horizon).await.unwrap().len(), 1);
        assert_eq!(
            core.store.moments_between(0, horizon).await.unwrap().len(),
            1
        );
        core.trash_artifact(&gone).await.unwrap();
        assert!(core.store.open_due(0, horizon).await.unwrap().is_empty());
        assert!(
            core.store
                .moments_between(0, horizon)
                .await
                .unwrap()
                .is_empty()
        );
        core.restore_artifact(&gone).await.unwrap();
        assert_eq!(core.store.open_due(0, horizon).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_restored_artifact_has_its_status_and_vectors_back() {
        let (core, id, _) = two_live_artifacts().await;
        core.deprecate(&id).await.unwrap();
        core.trash_artifact(&id).await.unwrap();
        assert_eq!(
            core.store.get_artifact(&id).await.unwrap().status,
            ArtifactStatus::Trashed
        );
        core.restore_artifact(&id).await.unwrap();
        assert_eq!(
            core.store.get_artifact(&id).await.unwrap().status,
            ArtifactStatus::Deprecated
        );
        let payload = core
            .vectors
            .lifecycle_of(std::slice::from_ref(&id))
            .await
            .unwrap();
        assert_eq!(payload[&id].status, ArtifactStatus::Deprecated);
        assert!(matches!(
            core.restore_artifact(&id).await,
            Err(Error::NotFound)
        ));
    }

    #[tokio::test]
    async fn trash_older_than_30_days_is_purged_and_younger_is_kept() {
        let (core, old, young) = two_live_artifacts().await;
        core.trash_artifact(&old).await.unwrap();
        core.trash_artifact(&young).await.unwrap();
        sqlx::query!(
            "UPDATE artifacts SET trashed_at = trashed_at - ? WHERE id = ?",
            crate::core::TRASH_RETENTION_SECS + 1,
            &old
        )
        .execute(&core.store.pool)
        .await
        .unwrap();
        assert_eq!(
            core.purge_trash(crate::core::TRASH_RETENTION_SECS)
                .await
                .unwrap(),
            1
        );
        assert!(core.store.get_artifact(&old).await.is_err());
        assert_eq!(
            core.store.get_artifact(&young).await.unwrap().status,
            ArtifactStatus::Trashed
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
        sqlx::query!("UPDATE artifacts SET created_at = ?", old)
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

    fn ranked(ids: &[&str]) -> Vec<SearchResult> {
        ids.iter()
            .map(|id| SearchResult {
                artifact_id: (*id).into(),
                corpus_id: "c".into(),
                score: 0.5,
                ..Default::default()
            })
            .collect()
    }

    fn order(rs: &[SearchResult]) -> Vec<&str> {
        rs.iter().map(|r| r.artifact_id.as_str()).collect()
    }

    #[test]
    fn the_sitting_moves_nothing_and_badges_nothing_while_the_knob_is_off() {
        let sitting = std::collections::HashSet::from(["d".to_string()]);
        let out = prime(
            ranked(&["a", "b", "c", "d"]),
            &HashMap::new(),
            0.5,
            2,
            &sitting,
            false,
            &Default::default(),
        );
        assert_eq!(order(&out), vec!["a", "b", "c", "d"], "nothing moved");
        assert!(
            out.iter().all(|r| !r.in_sitting),
            "and nothing is badged either"
        );
    }

    #[test]
    fn the_sitting_lifts_and_badges_once_the_knob_is_on() {
        let sitting = std::collections::HashSet::from(["d".to_string()]);
        let out = prime(
            ranked(&["a", "b", "c", "d"]),
            &HashMap::new(),
            0.5,
            2,
            &sitting,
            true,
            &Default::default(),
        );
        assert_eq!(order(&out), vec!["a", "d", "b", "c"]);
        assert!(out[1].primed && out[1].in_sitting);
    }

    #[test]
    fn a_hit_climbs_at_most_two_places_and_never_past_the_first() {
        let act = HashMap::from([("d".to_string(), 4.0)]);
        let out = prime(
            ranked(&["a", "b", "c", "d"]),
            &act,
            0.5,
            2,
            &Default::default(),
            false,
            &Default::default(),
        );
        assert_eq!(order(&out), vec!["a", "d", "b", "c"]);
        assert!(out[1].primed, "the hit that moved must say so");
        assert!(!out[2].primed, "the hit it passed did not move up");
    }

    #[test]
    fn the_most_active_hit_cannot_displace_an_exact_match() {
        let act = HashMap::from([("b".to_string(), 9.0)]);
        let out = prime(
            ranked(&["a", "b", "c"]),
            &act,
            0.5,
            2,
            &Default::default(),
            false,
            &Default::default(),
        );
        assert_eq!(order(&out), vec!["a", "b", "c"]);
        assert!(out.iter().all(|r| !r.primed));
    }

    #[test]
    fn what_this_sitting_read_can_lift_a_hit() {
        let sitting = std::collections::HashSet::from(["d".to_string()]);
        let out = prime(
            ranked(&["a", "b", "c", "d"]),
            &HashMap::new(),
            0.5,
            2,
            &sitting,
            true,
            &Default::default(),
        );
        assert_eq!(order(&out), vec!["a", "d", "b", "c"]);
        assert!(out[1].primed);
        assert!(out[1].in_sitting, "the page cannot say why it moved");
    }

    #[test]
    fn a_due_hit_is_lifted_like_a_sitting_hit_and_says_so() {
        let due = std::collections::HashSet::from(["d".to_string()]);
        let out = prime(
            ranked(&["a", "b", "c", "d"]),
            &HashMap::new(),
            0.5,
            2,
            &Default::default(),
            false,
            &due,
        );
        assert_eq!(order(&out), vec!["a", "d", "b", "c"]);
        assert!(out[1].primed);
        assert!(!out[1].in_sitting, "due is not the sitting");
    }

    #[test]
    fn a_due_hit_never_displaces_an_exact_match() {
        let due = std::collections::HashSet::from(["b".to_string()]);
        let out = prime(
            ranked(&["a", "b", "c"]),
            &HashMap::new(),
            0.5,
            2,
            &Default::default(),
            false,
            &due,
        );
        assert_eq!(
            order(&out),
            vec!["a", "b", "c"],
            "rank 0 never moves and b is already at 1"
        );
    }

    #[test]
    fn a_list_too_short_to_reorder_still_says_what_the_sitting_read() {
        let sitting = std::collections::HashSet::from(["b".to_string()]);
        let out = prime(
            ranked(&["a", "b"]),
            &HashMap::new(),
            0.5,
            2,
            &sitting,
            true,
            &Default::default(),
        );
        assert_eq!(order(&out), vec!["a", "b"], "nothing can move on two rows");
        assert!(out[1].in_sitting);
        assert!(!out[0].in_sitting);
        assert!(out.iter().all(|r| !r.primed), "and nothing was primed");
    }

    #[test]
    fn the_sitting_and_activation_share_one_budget() {
        let act = HashMap::from([("e".to_string(), 9.0)]);
        let sitting = std::collections::HashSet::from(["e".to_string()]);
        let out = prime(
            ranked(&["a", "b", "c", "d", "e"]),
            &act,
            0.5,
            2,
            &sitting,
            true,
            &Default::default(),
        );
        assert_eq!(
            order(&out),
            vec!["a", "b", "e", "c", "d"],
            "a hit in both moved further than one lift"
        );
    }

    #[test]
    fn the_sitting_cannot_displace_the_first_hit_either() {
        let sitting = std::collections::HashSet::from(["b".to_string(), "c".to_string()]);
        let out = prime(
            ranked(&["a", "b", "c"]),
            &HashMap::new(),
            0.5,
            2,
            &sitting,
            true,
            &Default::default(),
        );
        assert_eq!(order(&out)[0], "a");
    }

    #[test]
    fn a_lift_of_zero_turns_priming_off_entirely() {
        let act = HashMap::from([("d".to_string(), 4.0)]);
        let out = prime(
            ranked(&["a", "b", "c", "d"]),
            &act,
            0.5,
            0,
            &Default::default(),
            false,
            &Default::default(),
        );
        assert_eq!(order(&out), vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn activation_that_is_merely_higher_does_not_move_anything() {
        let act = HashMap::from([("c".to_string(), 4.0), ("d".to_string(), 3.6)]);
        let out = prime(
            ranked(&["a", "b", "c", "d"]),
            &act,
            0.5,
            2,
            &Default::default(),
            false,
            &Default::default(),
        );
        assert_eq!(
            order(&out),
            vec!["a", "c", "b", "d"],
            "only c clears the margin"
        );
    }

    #[test]
    fn a_never_opened_artifact_has_no_engagement_however_fresh_it_is() {
        // `(value, activated_at, created_at)` at day 0 and day 28, two half-lives at a 14-day
        // half-life.
        const DAY: i64 = 86_400;
        let now = 28 * DAY;
        let acts = HashMap::from([
            ("fresh".to_string(), (1.0, now, now)),
            ("old".to_string(), (1.0, 0, 0)),
            ("opened".to_string(), (1.8, now, now)),
        ]);
        let e = engagement(acts, now, 14.0);
        assert_eq!(e["fresh"], 0.0);
        assert_eq!(e["old"], 0.0);
        assert!((e["opened"] - 0.8).abs() < 1e-9);
        let out = prime(
            ranked(&["a", "fresh", "old"]),
            &engagement(
                HashMap::from([
                    ("fresh".to_string(), (1.0, now, now)),
                    ("old".to_string(), (1.0, 0, 0)),
                ]),
                now,
                14.0,
            ),
            0.5,
            2,
            &Default::default(),
            false,
            &Default::default(),
        );
        assert!(
            out.iter().all(|r| !r.primed),
            "primed without ever being opened"
        );
    }

    #[test]
    fn engagement_is_the_same_size_however_old_the_artifact_is() {
        const DAY: i64 = 86_400;
        let now = 42 * DAY;
        let e = engagement(
            HashMap::from([
                // Captured today, opened three times today: 1.0 + 3 × 1.0.
                ("fresh".to_string(), (4.0, now, now)),
                // Captured 42 days ago (three half-lives) and opened three times that day.
                ("old".to_string(), (4.0, 0, 0)),
                // Captured 42 days ago, opened once today: the decayed baseline plus a whole open.
                ("old_open_today".to_string(), (0.125 + 1.0, now, 0)),
            ]),
            now,
            14.0,
        );
        assert!((e["fresh"] - 3.0).abs() < 1e-9);
        assert!((e["old"] - 0.375).abs() < 1e-9, "got {}", e["old"]);
        assert!(
            (e["old_open_today"] - 1.0).abs() < 1e-9,
            "got {}",
            e["old_open_today"]
        );
    }

    #[test]
    fn a_hit_climbs_exactly_the_lift_and_no_further_however_long_the_list_is() {
        let act = HashMap::from([("e".to_string(), 1.0)]);
        let out = prime(
            ranked(&["a", "b", "c", "d", "e"]),
            &act,
            0.5,
            2,
            &Default::default(),
            false,
            &Default::default(),
        );
        assert_eq!(order(&out), vec!["a", "b", "e", "c", "d"]);
        let moved = order(&out).iter().position(|id| *id == "e").unwrap();
        assert_eq!(moved, 4 - 2, "e must climb exactly prime_lift positions");
        assert!(out[moved].primed);
    }

    #[test]
    fn the_lift_bound_holds_on_a_longer_list_too() {
        let act = HashMap::from([("g".to_string(), 1.0)]);
        let out = prime(
            ranked(&["a", "b", "c", "d", "e", "f", "g"]),
            &act,
            0.5,
            2,
            &Default::default(),
            false,
            &Default::default(),
        );
        let moved = order(&out).iter().position(|id| *id == "g").unwrap();
        assert_eq!(moved, 4, "g must land at index 4, not be pulled to index 1");
        assert_eq!(6 - moved, 2, "the climb must equal prime_lift exactly");
    }

    #[tokio::test]
    async fn priming_changes_the_order_a_search_returns_and_says_which_hit_moved() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        let texts: Vec<(&str, &str, &[&str])> = (0..6)
            .map(|_| ("alpha text about it", "note", &[][..]))
            .collect();
        seed(&core, &texts).await;
        reembed_all(&core).await;

        let plain = core
            .search(&q("alpha text about it"), Door::Ui)
            .await
            .unwrap();
        assert!(plain.len() >= 4, "this test needs a list to reorder");
        assert!(plain.iter().all(|r| !r.primed));
        core.ranking
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .prime_lift = 2;

        let bottom = plain.last().unwrap().artifact_id.clone();
        sqlx::query!(
            "UPDATE artifacts SET activation = 100.0, activated_at = ? WHERE id = ?",
            now_secs(),
            &bottom
        )
        .execute(&core.store.pool)
        .await
        .unwrap();

        let primed = core
            .search(&q("alpha text about it"), Door::Ui)
            .await
            .unwrap();
        let moved = primed.iter().position(|r| r.artifact_id == bottom).unwrap();
        assert!(
            moved < plain.len() - 1,
            "activation did not reach the ranked list at all"
        );
        assert!(primed[moved].primed, "a hit that moved must say so");
        assert_ne!(primed[0].artifact_id, bottom, "rank 1 was displaced");
    }

    #[tokio::test]
    async fn a_marked_search_does_not_touch_activation_while_learning_is_off() {
        let core = test_core().await;
        assert!(!core.learn.enabled);
        seed_from(&core, "one", &[("alpha text", "note", &[])]).await;
        reembed_all(&core).await;
        let id = core.store.list_all_artifact_ids().await.unwrap()[0].clone();
        let before = core
            .store
            .activation_of(std::slice::from_ref(&id))
            .await
            .unwrap()[&id]
            .0;

        core.search(&q("alpha text about it"), Door::Ui)
            .await
            .unwrap();
        core.background.wait_idle().await;

        let after = core
            .store
            .activation_of(std::slice::from_ref(&id))
            .await
            .unwrap()[&id]
            .0;
        assert_eq!(
            after, before,
            "a marked search raised activation with `[learn]` off"
        );
    }

    #[tokio::test]
    async fn a_search_records_the_sitting_even_though_the_knob_is_off() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        seed(&core, &[("alpha text about it", "note", &[])]).await;
        reembed_all(&core).await;
        let a = id_of(&core, "alpha text about it").await;

        assert!(
            !core
                .ranking
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .sitting_prime,
            "the fixture must run with the knob off, or this proves nothing"
        );
        core.sittings
            .touched("sess", &a, now_secs(), core.pursuit.idle_secs as i64);

        let (_, outcome) = core
            .search_with(
                &q("alpha text about it"),
                None,
                crate::store::feedback::Origin::from(Door::Ui).in_sitting(Some("sess".to_string())),
            )
            .await
            .unwrap();
        core.background.wait_idle().await;

        let event = outcome.event.expect("the UI door waits for its capture");
        let ctx = core
            .store
            .search_context(&event)
            .await
            .unwrap()
            .expect("a priming search records what priming read");
        assert!(
            ctx.sitting.contains(&a),
            "the touched artifact must be in the recorded sitting: {:?}",
            ctx.sitting
        );
    }

    #[tokio::test]
    async fn a_door_with_no_session_records_an_empty_sitting() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        seed(&core, &[("alpha text about it", "note", &[])]).await;
        reembed_all(&core).await;
        let a = id_of(&core, "alpha text about it").await;
        core.sittings
            .touched("sess", &a, now_secs(), core.pursuit.idle_secs as i64);

        let (_, outcome) = core
            .search_with(&q("alpha text about it"), None, Door::Ui)
            .await
            .unwrap();
        core.background.wait_idle().await;

        let event = outcome.event.expect("the UI door waits for its capture");
        let ctx = core.store.search_context(&event).await.unwrap().unwrap();
        assert!(
            ctx.sitting.is_empty(),
            "a search with no session names no sitting: {:?}",
            ctx.sitting
        );
    }

    #[tokio::test]
    async fn priming_never_reaches_the_order_while_learning_is_off() {
        let core = test_core().await;
        assert!(!core.learn.enabled);
        core.ranking
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .prime_lift = 2;
        let texts: Vec<(&str, &str, &[&str])> = (0..6)
            .map(|_| ("alpha text about it", "note", &[][..]))
            .collect();
        seed(&core, &texts).await;
        reembed_all(&core).await;

        let plain = core
            .search(&q("alpha text about it"), Door::Ui)
            .await
            .unwrap();
        assert!(plain.len() >= 4, "this test needs a list to reorder");

        let bottom = plain.last().unwrap().artifact_id.clone();
        sqlx::query!(
            "UPDATE artifacts SET activation = 100.0, activated_at = ? WHERE id = ?",
            now_secs(),
            &bottom
        )
        .execute(&core.store.pool)
        .await
        .unwrap();

        let with_huge_activation = core
            .search(&q("alpha text about it"), Door::Ui)
            .await
            .unwrap();
        let plain_ids: Vec<&str> = plain.iter().map(|r| r.artifact_id.as_str()).collect();
        let after_ids: Vec<&str> = with_huge_activation
            .iter()
            .map(|r| r.artifact_id.as_str())
            .collect();
        assert_eq!(
            plain_ids, after_ids,
            "the order changed even though `[learn]` is off"
        );
        assert!(with_huge_activation.iter().all(|r| !r.primed));
    }

    #[tokio::test]
    async fn priming_never_reaches_the_list_ask_and_judge_are_given() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        core.ranking
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .prime_lift = 2;
        let texts: Vec<(&str, &str, &[&str])> = (0..6)
            .map(|_| ("alpha text about it", "note", &[][..]))
            .collect();
        seed(&core, &texts).await;
        reembed_all(&core).await;

        let plain = core
            .search(&q("alpha text about it"), Door::Ui)
            .await
            .unwrap();
        assert!(plain.len() >= 4, "this test needs a list to reorder");
        let bottom = plain.last().unwrap().artifact_id.clone();
        sqlx::query!(
            "UPDATE artifacts SET activation = 100.0, activated_at = ? WHERE id = ?",
            now_secs(),
            &bottom
        )
        .execute(&core.store.pool)
        .await
        .unwrap();

        let moved = core
            .search(&q("alpha text about it"), Door::Ui)
            .await
            .unwrap();
        assert_ne!(
            moved.last().unwrap().artifact_id,
            bottom,
            "this test proves nothing unless priming is working on some door"
        );

        for door in [Door::Ask, Door::Judge] {
            let held = core.search(&q("alpha text about it"), door).await.unwrap();
            let ids: Vec<&str> = held.iter().map(|r| r.artifact_id.as_str()).collect();
            let plain_ids: Vec<&str> = plain.iter().map(|r| r.artifact_id.as_str()).collect();
            assert_eq!(ids, plain_ids, "{door:?} was handed a primed order");
            assert!(held.iter().all(|r| !r.primed));
        }
    }

    #[tokio::test]
    async fn an_association_obeys_the_filters_the_searcher_typed() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        core.associate.show_min = 1.0;
        seed(
            &core,
            &[
                ("alpha text about it", "runbook", &["ops"][..]),
                ("something else entirely", "note", &[][..]),
            ],
        )
        .await;
        reembed_all(&core).await;
        let ids = core.store.list_all_artifact_ids().await.unwrap();
        let (hit, other) = {
            let a = core.store.get_artifact(&ids[0]).await.unwrap();
            if a.category.as_deref() == Some("runbook") {
                (ids[0].clone(), ids[1].clone())
            } else {
                (ids[1].clone(), ids[0].clone())
            }
        };
        core.store
            .bump_link(&hit, &other, 9.0, Some("q"), 30.0, now_secs())
            .await
            .unwrap();

        let wide = core
            .search(&q("alpha text about it"), Door::Ui)
            .await
            .unwrap();
        assert!(
            wide.iter().any(|r| r.artifact_id == other),
            "the association never fired, so the filter proves nothing"
        );

        let mut narrowed = q("alpha text about it");
        narrowed.category = Some("runbook".into());
        let narrow = core.search(&narrowed, Door::Ui).await.unwrap();
        assert!(
            !narrow.iter().any(|r| r.artifact_id == other),
            "an association walked past the category the searcher typed"
        );

        let mut tagged = q("alpha text about it");
        tagged.tags = vec!["ops".into()];
        let by_tag = core.search(&tagged, Door::Ui).await.unwrap();
        assert!(
            !by_tag.iter().any(|r| r.artifact_id == other),
            "an association walked past the tags the searcher typed"
        );
    }

    async fn captured_events(core: &crate::core::Core) -> i64 {
        core.background.wait_idle().await;
        sqlx::query_scalar!("SELECT count(*) FROM search_events")
            .fetch_one(&core.store.pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_captured_search_stores_the_pool_it_could_have_shown() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        let texts: Vec<(&str, &str, &[&str])> = (0..12)
            .map(|_| ("alpha text about it", "note", &[][..]))
            .collect();
        seed(&core, &texts).await;
        reembed_all(&core).await;

        let mut query = q("alpha text about it");
        query.limit = 3;
        core.search(&query, Door::Ui).await.unwrap();
        core.background.wait_idle().await;

        let rows = sqlx::query!("SELECT rank, shown FROM search_candidates ORDER BY rank")
            .fetch_all(&core.store.pool)
            .await
            .unwrap();
        assert!(
            rows.len() > 3,
            "the pool must be wider than the three results shown, got {}",
            rows.len()
        );
        let shown: i64 = rows.into_iter().map(|r| r.shown).sum();
        assert_eq!(
            shown, 3,
            "exactly the answer the searcher saw is flagged shown"
        );
    }

    #[tokio::test]
    async fn the_stored_pool_is_as_wide_as_it_was_configured_to_be() {
        let mut core = test_core().await;
        core.learn.enabled = true;
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

        let pool: i64 = sqlx::query_scalar!("SELECT count(*) FROM search_candidates")
            .fetch_one(&core.store.pool)
            .await
            .unwrap();
        assert_eq!(
            pool, 12,
            "the configured pool was cut back to the over-fetch"
        );
    }

    #[tokio::test]
    async fn the_live_cap_is_read_per_search_rather_than_compiled_in() {
        let core = test_core().await;
        // Identical untitled chunks tie, so the assertions read only whether a source holds two
        // places.
        for (raw, text) in [
            ("raw one", "the image will not mount"),
            ("raw two", "unrelated words"),
        ] {
            let src = core.store.insert_corpus(raw, "web", None).await.unwrap();
            let new: Vec<NewArtifact> = (0..3)
                .map(|i| NewArtifact {
                    ordinal: i,
                    text: text.to_string(),
                    ..Default::default()
                })
                .collect();
            for c in core.store.insert_artifacts(&src.id, &new).await.unwrap() {
                crate::jobs::embed::run(&core, &c.id).await.unwrap();
            }
        }

        let mut query = q("the image will not mount");
        query.limit = 6;

        core.ranking
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .per_source_cap = None;
        let uncapped = core.search(&query, Door::Judge).await.unwrap();
        assert_eq!(
            uncapped[0].text, uncapped[1].text,
            "uncapped, one document supplies the head of the list"
        );

        core.ranking
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .per_source_cap = Some(1);
        let capped = core.search(&query, Door::Judge).await.unwrap();
        assert_ne!(
            capped[0].text, capped[1].text,
            "a cap of one must let the other source take the second place"
        );
    }

    #[tokio::test]
    async fn a_passage_carrying_its_sections_heading_says_the_name_is_borrowed() {
        let core = test_core().await;
        let src = core
            .store
            .insert_corpus("feeding schedule", "web", Some("Sourdough"))
            .await
            .unwrap();
        let new = vec![NewArtifact {
            text: "feeding schedule that finally worked".to_string(),
            title: Some("Kapitel 3".to_string()),
            ..Default::default()
        }];
        for c in core
            .store
            .insert_artifacts_with_provenance(
                &src.id,
                &new,
                crate::store::artifacts::Provenance::Passage,
            )
            .await
            .unwrap()
        {
            crate::jobs::embed::run(&core, &c.id).await.unwrap();
        }
        let hits = core
            .search(&q("feeding schedule"), Door::Judge)
            .await
            .unwrap();
        assert_eq!(hits[0].title.as_deref(), Some("Kapitel 3"));
        assert!(
            hits[0].borrowed_name,
            "the heading names the section, not this passage"
        );
    }

    #[tokio::test]
    async fn a_passage_with_no_heading_is_shown_under_its_notes_title() {
        let core = test_core().await;
        let named = core
            .store
            .insert_corpus("feeding schedule", "web", Some("Sourdough"))
            .await
            .unwrap();
        let unnamed = core.store.insert_corpus("raw", "web", None).await.unwrap();
        for (src, text) in [
            (&named, "feeding schedule that finally worked"),
            (&unnamed, "words"),
        ] {
            let new = vec![NewArtifact {
                text: text.to_string(),
                ..Default::default()
            }];
            for c in core.store.insert_artifacts(&src.id, &new).await.unwrap() {
                crate::jobs::embed::run(&core, &c.id).await.unwrap();
            }
        }
        let hits = core
            .search(&q("feeding schedule"), Door::Judge)
            .await
            .unwrap();
        let of = |src: &crate::store::corpora::Corpus| {
            hits.iter().find(|h| h.corpus_id == src.id).unwrap()
        };
        assert_eq!(of(&named).title.as_deref(), Some("Sourdough"));
        assert!(
            of(&named).borrowed_name,
            "the title must say it is not this passage's own"
        );
        assert_eq!(of(&unnamed).title, None);
        assert!(!of(&unnamed).borrowed_name);
    }

    #[tokio::test]
    async fn a_reranked_search_stores_a_pool_wider_than_its_answer() {
        let (mut core, _r) = crate::core::test_support::test_core_counting_reranked_docs().await;
        core.learn.enabled = true;
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

        let rows = sqlx::query!("SELECT rank, shown FROM search_candidates ORDER BY rank")
            .fetch_all(&core.store.pool)
            .await
            .unwrap();
        assert!(
            rows.len() > 3,
            "the reranked pool collapsed to the answer, got {}",
            rows.len()
        );
        let shown: i64 = rows.into_iter().map(|r| r.shown).sum();
        assert_eq!(
            shown, 3,
            "only the answer the searcher saw is flagged shown"
        );
    }

    #[tokio::test]
    async fn the_captured_filters_record_every_narrowing_the_search_used() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        seed(&core, &[("alpha text", "note", &[])]).await;
        reembed_all(&core).await;

        let mut query = q("alpha text");
        query.include_deprecated = true;
        core.search(&query, Door::Ui).await.unwrap();
        core.background.wait_idle().await;

        let filters: String = sqlx::query_scalar!("SELECT filters FROM search_events")
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
        let core = test_core().await; // `[learn]` is off in tests
        seed(&core, &[("alpha text", "note", &[])]).await;
        reembed_all(&core).await;
        core.search(&q("alpha text"), Door::Ui).await.unwrap();
        assert_eq!(captured_events(&core).await, 0);
    }

    #[tokio::test]
    async fn a_search_that_found_nothing_is_still_captured() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        core.search(&q("nothing is indexed yet"), Door::Ui)
            .await
            .unwrap();
        assert_eq!(captured_events(&core).await, 1);
    }

    #[tokio::test]
    async fn the_doors_that_know_the_answer_are_never_captured() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        seed(&core, &[("alpha text", "note", &[])]).await;
        reembed_all(&core).await;

        core.search(&q("alpha text"), Door::Judge).await.unwrap();
        core.search(&q("alpha text"), Door::Ask).await.unwrap();
        assert_eq!(captured_events(&core).await, 0);
    }

    async fn explorable(explore: f32, n: usize) -> crate::core::Core {
        let mut core = test_core().await;
        core.learn.enabled = true;
        core.feedback.explore = explore;
        for i in 0..n {
            let raw = format!("src{i}");
            let text = format!("mount the loop device image {i}");
            seed_from(&core, &raw, &[(text.as_str(), "note", &[])]).await;
        }
        reembed_all(&core).await;
        core
    }

    async fn pool_with_exploration(core: &crate::core::Core) -> Vec<(String, i64, Option<i64>)> {
        sqlx::query!(
            "SELECT artifact_id, shown, explored_from
               FROM search_candidates WHERE band = 0 ORDER BY rank"
        )
        .fetch_all(&core.store.pool)
        .await
        .map(|v| {
            v.into_iter()
                .map(|r| (r.artifact_id, r.shown, r.explored_from))
                .collect::<Vec<_>>()
        })
        .unwrap()
    }

    #[tokio::test]
    async fn an_exploring_search_shows_one_row_the_ranking_hid() {
        let core = explorable(1.0, 8).await;
        let mut query = q("mount the loop device image");
        query.limit = 3;
        let out = core.search(&query, Door::Ui).await.unwrap();
        core.background.wait_idle().await;

        assert_eq!(out.len(), 3, "the window is still the window");
        let pool = pool_with_exploration(&core).await;
        let lifted: Vec<_> = pool.iter().filter(|(_, _, e)| e.is_some()).collect();
        assert_eq!(lifted.len(), 1, "exactly one row is lent, not a shuffle");
        let (id, shown, from) = lifted[0];
        assert_eq!(*shown, 1, "a lifted row nobody sees is worth nothing");
        assert_eq!(
            *from,
            Some(3),
            "charged to the rank the ranking gave it, which is the first one it cut"
        );
        assert!(
            out.iter().any(|r| &r.artifact_id == id),
            "the lifted artifact is in the answer the searcher was handed"
        );
        assert_eq!(
            pool.iter().filter(|(_, shown, _)| *shown == 1).count(),
            3,
            "the window did not grow"
        );
    }

    #[tokio::test]
    async fn nothing_explores_when_the_knob_is_off_or_there_is_no_room() {
        let core = explorable(0.0, 8).await;
        let mut query = q("mount the loop device image");
        query.limit = 3;
        core.search(&query, Door::Ui).await.unwrap();
        core.background.wait_idle().await;
        assert!(
            pool_with_exploration(&core)
                .await
                .iter()
                .all(|(_, _, e)| e.is_none()),
            "explore = 0 explores"
        );

        let core = explorable(1.0, 2).await;
        let mut query = q("mount the loop device image");
        query.limit = 10;
        core.search(&query, Door::Ui).await.unwrap();
        core.background.wait_idle().await;
        assert!(
            pool_with_exploration(&core)
                .await
                .iter()
                .all(|(_, _, e)| e.is_none()),
            "a window wider than the base explored anyway"
        );
    }

    async fn id_of(core: &crate::core::Core, text: &str) -> String {
        sqlx::query_scalar!(r#"SELECT id AS "id!" FROM artifacts WHERE text = ?"#, text)
            .fetch_one(&core.store.pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_linked_artifact_is_recalled_beside_the_answer_and_says_what_recalled_it() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        seed_from(&core, "one", &[("alpha text", "note", &[])]).await;
        seed_from(&core, "two", &[("something else entirely", "note", &[])]).await;
        reembed_all(&core).await;
        let a = id_of(&core, "alpha text").await;
        let b = id_of(&core, "something else entirely").await;
        core.store
            .bump_link(&a, &b, 5.0, Some("both of these"), 30.0, now_secs())
            .await
            .unwrap();

        let mut query = q("t0\nalpha text");
        query.limit = 1;
        let out = core.search(&query, Door::Ui).await.unwrap();

        assert_eq!(out.len(), 2, "the association was not appended: {out:?}");
        assert_eq!(out[0].artifact_id, a);
        assert_eq!(
            out[0].via, None,
            "a ranked hit was not recalled by anything"
        );
        assert_eq!(out[1].artifact_id, b);
        assert_eq!(out[1].via.as_deref(), Some(a.as_str()));

        core.ranking
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .spread_max = 0;
        let out = core.search(&query, Door::Ui).await.unwrap();
        assert_eq!(out.len(), 1, "spread zero appends nothing: {out:?}");
    }

    #[tokio::test]
    async fn an_appended_hit_is_captured_in_the_band_and_an_open_on_it_is_an_observation() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        seed_from(&core, "one", &[("alpha text", "note", &[])]).await;
        seed_from(&core, "two", &[("something else entirely", "note", &[])]).await;
        reembed_all(&core).await;
        let a = id_of(&core, "alpha text").await;
        let b = id_of(&core, "something else entirely").await;
        core.store
            .bump_link(&a, &b, 5.0, Some("both of these"), 30.0, now_secs())
            .await
            .unwrap();
        let params = *core
            .ranking
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let generation = core
            .store
            .record_generation(&crate::store::generations::NewGeneration {
                params: params.into(),
                embed_recipe: "recipe-a".into(),
                chat_model: "qwen".into(),
                ..Default::default()
            })
            .await
            .unwrap();

        let mut query = q("t0\nalpha text");
        query.limit = 1;
        let (out, outcome) = core
            .search_with_ranking(&query, params, Door::Ui)
            .await
            .unwrap();
        assert_eq!(out.len(), 2);
        let event = outcome.event.expect("the UI door waits for its capture");
        let rows =
            sqlx::query!("SELECT artifact_id, band, shown FROM search_candidates ORDER BY rank")
                .fetch_all(&core.store.pool)
                .await
                .unwrap();
        let band: Vec<_> = rows.iter().filter(|r| r.band == 1).collect();
        assert_eq!(band.len(), 1, "the appended hit is in the pool, flagged");
        assert_eq!(band[0].artifact_id, b);
        assert_eq!(band[0].shown, 1);

        assert!(core.store.open_event(&event, &b).await.unwrap());
        let obs = core
            .store
            .observations_for_generation(&generation, 5)
            .await
            .unwrap();
        assert_eq!(obs.len(), 1);
        assert_eq!(obs[0].artifact_id.as_deref(), Some(b.as_str()));
        // The place on screen, not in the pool: `eval::sweep` and `jobs::retract` read `rank - 1`
        // as served.
        assert_eq!(
            obs[0].rank,
            Some(2),
            "the place it was shown at, under the one ranked hit: {rows:?}"
        );
    }

    #[tokio::test]
    async fn ask_and_judge_never_receive_an_association_but_ui_does() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        seed_from(&core, "one", &[("alpha text", "note", &[])]).await;
        seed_from(&core, "two", &[("something else entirely", "note", &[])]).await;
        reembed_all(&core).await;
        let a = id_of(&core, "alpha text").await;
        let b = id_of(&core, "something else entirely").await;
        core.store
            .bump_link(&a, &b, 5.0, Some("both of these"), 30.0, now_secs())
            .await
            .unwrap();

        let mut query = q("t0\nalpha text");
        query.limit = 1;

        let ask_out = core.search(&query, Door::Ask).await.unwrap();
        assert_eq!(ask_out.len(), 1, "ask received an association: {ask_out:?}");

        let judge_out = core.search(&query, Door::Judge).await.unwrap();
        assert_eq!(
            judge_out.len(),
            1,
            "judge received an association: {judge_out:?}"
        );

        let ui_out = core.search(&query, Door::Ui).await.unwrap();
        assert_eq!(
            ui_out.len(),
            2,
            "ui did not receive the association: {ui_out:?}"
        );
        assert_eq!(ui_out[1].via.as_deref(), Some(a.as_str()));
    }

    #[tokio::test]
    async fn an_artifact_already_in_the_answer_is_not_recalled_again() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        seed_from(
            &core,
            "one",
            &[("alpha text", "note", &[]), ("alpha other", "note", &[])],
        )
        .await;
        reembed_all(&core).await;
        let a = id_of(&core, "alpha text").await;
        let b = id_of(&core, "alpha other").await;
        core.store
            .bump_link(&a, &b, 5.0, Some("q"), 30.0, now_secs())
            .await
            .unwrap();

        let out = core.search(&q("alpha"), Door::Ui).await.unwrap();
        let seen: std::collections::HashSet<&str> =
            out.iter().map(|r| r.artifact_id.as_str()).collect();
        assert_eq!(seen.len(), out.len(), "an artifact was returned twice");
    }

    async fn captured_pool(core: &crate::core::Core, a: &str, b: &str) -> Vec<(&'static str, i64)> {
        let rows: Vec<(String, i64)> = sqlx::query!(
            "SELECT artifact_id, shown FROM search_candidates WHERE band = 0 ORDER BY rank"
        )
        .fetch_all(&core.store.pool)
        .await
        .map(|v| {
            v.into_iter()
                .map(|r| (r.artifact_id, r.shown))
                .collect::<Vec<_>>()
        })
        .unwrap();
        rows.into_iter()
            .map(|(id, shown)| {
                let role = if id == a {
                    "a"
                } else if id == b {
                    "b"
                } else {
                    "?"
                };
                (role, shown)
            })
            .collect()
    }

    #[tokio::test]
    async fn a_recalled_artifact_does_not_feed_the_learning_that_produced_it() {
        // `b` is a legitimate low-ranked candidate on so small a base, so this compares a linked
        // and an unlinked pool rather than checking `b`'s absence.
        async fn seeded() -> (crate::core::Core, String, String) {
            let mut core = test_core().await;
            core.learn.enabled = true;
            seed_from(&core, "one", &[("alpha text", "note", &[])]).await;
            seed_from(&core, "two", &[("something else entirely", "note", &[])]).await;
            reembed_all(&core).await;
            let a = id_of(&core, "alpha text").await;
            let b = id_of(&core, "something else entirely").await;
            (core, a, b)
        }

        let mut query = q("t0\nalpha text");
        query.limit = 1;

        let (unlinked, a_u, b_u) = seeded().await;
        let unlinked_out = unlinked.search(&query, Door::Ui).await.unwrap();
        unlinked.background.wait_idle().await;
        let unlinked_pool = captured_pool(&unlinked, &a_u, &b_u).await;

        let (linked, a, b) = seeded().await;
        linked
            .store
            .bump_link(&a, &b, 5.0, Some("q"), 30.0, now_secs())
            .await
            .unwrap();
        let before = linked
            .store
            .activation_of(std::slice::from_ref(&b))
            .await
            .unwrap()[&b]
            .0;
        let linked_out = linked.search(&query, Door::Ui).await.unwrap();
        linked.background.wait_idle().await;
        let linked_pool = captured_pool(&linked, &a, &b).await;
        let after = linked
            .store
            .activation_of(std::slice::from_ref(&b))
            .await
            .unwrap()[&b]
            .0;

        // 3. The treatment actually took effect, or the comparison below is
        // vacuous.
        assert_eq!(unlinked_out.len(), 1, "got: {unlinked_out:?}");
        assert_eq!(linked_out.len(), 2, "got: {linked_out:?}");
        assert_eq!(linked_out[1].artifact_id, b);
        assert_eq!(linked_out[1].via.as_deref(), Some(a.as_str()));

        // 1. The captured pool, id and shown in rank order, is identical either way.
        assert_eq!(
            unlinked_pool, linked_pool,
            "the link changed what the search recorded as a candidate"
        );

        // 2. Being recalled did not count as a retrieval.
        assert!(
            (after - before).abs() < 1e-9,
            "being recalled raised activation"
        );

        // 4. The band is in the pool, flagged, and the associate job's co-appearance read ignores
        // it.
        let band: Vec<String> =
            sqlx::query_scalar!("SELECT artifact_id FROM search_candidates WHERE band = 1")
                .fetch_all(&linked.store.pool)
                .await
                .unwrap();
        assert_eq!(
            band,
            vec![b.clone()],
            "the recalled hit is recorded as the band"
        );
        let event: String = sqlx::query_scalar!(r#"SELECT id AS "id!" FROM search_events"#)
            .fetch_one(&linked.store.pool)
            .await
            .unwrap();
        let shown = linked
            .store
            .events_between(0, i64::MAX)
            .await
            .unwrap()
            .into_iter()
            .find(|e| e.id == event)
            .expect("the event")
            .shown;
        assert!(
            shown.iter().all(|(id, _)| *id != b),
            "the pursuit's shown list read the band: {shown:?}"
        );
    }

    #[tokio::test]
    async fn a_hidden_artifact_is_never_recalled_by_association() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        seed_from(&core, "one", &[("alpha text", "note", &[])]).await;
        seed_from(&core, "two", &[("something else entirely", "note", &[])]).await;
        reembed_all(&core).await;
        let a = id_of(&core, "alpha text").await;
        let b = id_of(&core, "something else entirely").await;
        core.store
            .bump_link(&a, &b, 5.0, Some("q"), 30.0, now_secs())
            .await
            .unwrap();
        core.deprecate(&b).await.unwrap();

        let mut query = q("t0\nalpha text");
        query.limit = 1;
        assert_eq!(core.search(&query, Door::Ui).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_hit_says_which_note_disagrees_with_it_and_keeps_its_rank() {
        use crate::store::pairs::{DecidedBy, PairState};
        let core = test_core().await;
        seed_from(&core, "one", &[("alpha text", "note", &[])]).await;
        seed_from(&core, "two", &[("alpha text again", "note", &[])]).await;
        reembed_all(&core).await;
        let a = id_of(&core, "alpha text").await;
        let b = id_of(&core, "alpha text again").await;

        let ranked = |hits: &[SearchResult]| -> Vec<String> {
            hits.iter().map(|h| h.artifact_id.clone()).collect()
        };
        let before = core.search(&q("t0\nalpha text"), Door::Ui).await.unwrap();
        assert_eq!(before.len(), 2);
        assert!(before.iter().all(|h| h.disagrees_with.is_empty()));

        core.store.record_pair(&a, &b, 0.9).await.unwrap();
        let id = core.store.pair_between(&a, &b).await.unwrap().unwrap().id;
        core.store
            .set_pair_state(
                id,
                PairState::Contradiction,
                Some("port 80 here, 8080 there"),
                DecidedBy::Model,
            )
            .await
            .unwrap();

        let after = core.search(&q("t0\nalpha text"), Door::Ui).await.unwrap();
        assert_eq!(
            ranked(&after),
            ranked(&before),
            "a disagreement moves nothing"
        );
        for h in &after {
            let other = if h.artifact_id == a { &b } else { &a };
            assert_eq!(h.disagrees_with.len(), 1, "{:?}", h.artifact_id);
            assert_eq!(&h.disagrees_with[0].other_id, other);
            assert_eq!(
                h.disagrees_with[0].detail.as_deref(),
                Some("port 80 here, 8080 there")
            );
        }
    }

    #[tokio::test]
    async fn a_recalled_hit_is_named_and_attributed_by_its_own_provenance() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        seed_from(&core, "one", &[("alpha text", "note", &[])]).await;
        seed_from(&core, "two", &[("something else entirely", "note", &[])]).await;
        reembed_all(&core).await;
        let a = id_of(&core, "alpha text").await;
        let b = id_of(&core, "something else entirely").await;
        core.store
            .bump_link(&a, &b, 5.0, Some("q"), 30.0, now_secs())
            .await
            .unwrap();

        async fn recalled(core: &crate::core::Core) -> SearchResult {
            let mut query = q("t0\nalpha text");
            query.limit = 1;
            core.search(&query, Door::Ui)
                .await
                .unwrap()
                .into_iter()
                .find(|h| h.via.is_some())
                .expect("the associated hit")
        }

        // A passage carrying its section's heading, `t1`, which names the corpus and not the
        // passage.
        sqlx::query!(
            "UPDATE artifacts SET provenance = 'passage' WHERE id = ?",
            &b
        )
        .execute(&core.store.pool)
        .await
        .unwrap();
        let hit = recalled(&core).await;
        assert!(
            hit.borrowed_name,
            "a recalled passage carries a heading that is not its own: {:?}",
            hit.title
        );
        assert!(!hit.model_written, "a passage is source text");

        sqlx::query!(
            "UPDATE artifacts SET provenance = 'merged' WHERE id = ?",
            &b
        )
        .execute(&core.store.pool)
        .await
        .unwrap();
        let hit = recalled(&core).await;
        assert!(!hit.borrowed_name, "a merge was named by what wrote it");
        assert!(
            hit.model_written,
            "a merge reached by association is a merge"
        );
    }

    #[tokio::test]
    async fn a_weak_link_is_not_strong_enough_to_recall_anything() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        seed_from(&core, "one", &[("alpha text", "note", &[])]).await;
        seed_from(&core, "two", &[("something else entirely", "note", &[])]).await;
        reembed_all(&core).await;
        let a = id_of(&core, "alpha text").await;
        let b = id_of(&core, "something else entirely").await;
        // One co-appearance, against a `show_min` of 2.0.
        core.store
            .bump_link(&a, &b, 1.0, Some("q"), 30.0, now_secs())
            .await
            .unwrap();

        let mut query = q("t0\nalpha text");
        query.limit = 1;
        assert_eq!(core.search(&query, Door::Ui).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn links_between_ranked_hits_do_not_starve_the_association_budget() {
        // Calls `associated` directly so the outcome does not depend on the fake embedder's
        // ranking.
        let core = test_core().await;
        seed_from(
            &core,
            "one",
            &[
                ("alpha one", "note", &[]),
                ("alpha two", "note", &[]),
                ("alpha three", "note", &[]),
                ("alpha four", "note", &[]),
            ],
        )
        .await;
        reembed_all(&core).await;
        let a = id_of(&core, "alpha one").await;
        let b = id_of(&core, "alpha two").await;
        let c = id_of(&core, "alpha three").await;
        let d = id_of(&core, "alpha four").await;

        let now = now_secs();
        core.store
            .bump_link(&a, &b, 10.0, Some("q"), 30.0, now)
            .await
            .unwrap();
        core.store
            .bump_link(&a, &c, 10.0, Some("q"), 30.0, now)
            .await
            .unwrap();
        core.store
            .bump_link(&b, &c, 10.0, Some("q"), 30.0, now)
            .await
            .unwrap();
        core.store
            .bump_link(&a, &d, 5.0, Some("q"), 30.0, now)
            .await
            .unwrap();

        let dummy = |id: String| SearchResult {
            artifact_id: id,
            score: 1.0,
            ..Default::default()
        };
        let results = vec![dummy(a.clone()), dummy(b.clone()), dummy(c.clone())];

        let out = core
            .associated(
                &results,
                &SearchFilter::default(),
                core.associate.spread_max,
            )
            .await;
        let ids: Vec<&str> = out.iter().map(|r| r.artifact_id.as_str()).collect();
        assert!(
            ids.contains(&d.as_str()),
            "an artifact linked only to the ranked hits, not among them, was not recalled: {ids:?}"
        );
    }

    #[test]
    fn a_hybrid_list_falls_off_where_only_one_branch_still_matches() {
        let both = |r: f32| 1.0 / (60.0 + r) + 1.0 / (60.0 + r);
        let one = |r: f32| 1.0 / (60.0 + r);
        let scores = [both(1.0), both(2.0), one(3.0), one(4.0), one(5.0), one(6.0)];
        assert_eq!(cliff(&scores), Some(2));
    }

    #[test]
    fn an_evenly_falling_list_has_no_cliff() {
        let scores: Vec<f32> = (1..=10).map(|r| 1.0 / (60.0 + r as f32)).collect();
        assert_eq!(cliff(&scores), None);
    }

    #[test]
    fn reranker_scores_fall_off_after_the_close_matches() {
        assert_eq!(cliff(&[0.95, 0.90, 0.30, 0.28, 0.10]), Some(2));
    }

    #[test]
    fn an_ambiguous_list_says_nothing() {
        assert_eq!(cliff(&[0.9, 0.5, 0.1, 0.05]), None);
    }

    #[test]
    fn fewer_than_three_hits_have_no_cliff() {
        assert_eq!(cliff(&[]), None);
        assert_eq!(cliff(&[1.0]), None);
        assert_eq!(cliff(&[1.0, 0.1]), None);
    }

    #[test]
    fn a_plateau_followed_by_a_drop_is_a_cliff() {
        assert_eq!(cliff(&[1.0, 1.0, 1.0, 0.2, 0.2]), Some(3));
        assert_eq!(cliff(&[0.5, 0.5, 0.5, 0.5]), None);
    }

    #[test]
    fn a_tie_at_the_foot_of_a_plateau_is_not_a_cliff() {
        assert_eq!(cliff(&[1.0, 1.0, 1.0, 0.9999]), None);
        // The floor is a share of the top score, so the same shape in fused-rank space reads the
        // same.
        assert_eq!(cliff(&[0.016, 0.016, 0.016, 0.0159]), None);
        assert_eq!(cliff(&[0.016, 0.016, 0.016, 0.004]), Some(3));
    }

    #[test]
    fn a_primed_inversion_is_not_a_cliff() {
        assert_eq!(cliff(&[0.90, 0.89, 0.88, 0.87, 0.30]), Some(4));
        assert_eq!(cliff(&[0.50, 0.49, 0.48, 0.47, 0.46]), None);
    }

    #[test]
    fn hits_past_the_cliff_are_marked_and_the_rest_are_not() {
        let dummy = |id: &str, score: f32| SearchResult {
            artifact_id: id.into(),
            corpus_id: "c".into(),
            score,
            ..Default::default()
        };
        let mut results = vec![
            dummy("a", 0.95),
            dummy("b", 0.90),
            dummy("c", 0.30),
            dummy("d", 0.28),
        ];
        mark_past_cliff(&mut results, true);
        assert_eq!(
            results.iter().map(|r| r.past_cliff).collect::<Vec<_>>(),
            vec![false, false, true, true]
        );
        assert_eq!(
            results
                .iter()
                .map(|r| r.artifact_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b", "c", "d"]
        );

        let mut flat = vec![dummy("a", 0.5), dummy("b", 0.5), dummy("c", 0.5)];
        mark_past_cliff(&mut flat, true);
        assert!(flat.iter().all(|r| !r.past_cliff));
    }

    #[test]
    fn a_retired_hit_at_the_tail_does_not_take_the_cliff_with_it() {
        let dummy = |id: &str, score: f32, retired: bool| SearchResult {
            artifact_id: id.into(),
            corpus_id: "c".into(),
            score,
            retired,
            ..Default::default()
        };
        let mut results = vec![
            dummy("retired", 0.90, true),
            dummy("a", 0.88, false),
            dummy("b", 0.87, false),
            dummy("c", 0.30, false),
            dummy("d", 0.29, false),
        ];
        sink_retired_and_mark_the_cliff(&mut results, true);
        assert_eq!(
            results
                .iter()
                .map(|r| r.artifact_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b", "c", "d", "retired"],
            "the retired row sinks to the tail"
        );
        assert_eq!(
            results.iter().map(|r| r.past_cliff).collect::<Vec<_>>(),
            vec![false, false, true, true, true],
            "the fall between 0.87 and 0.30 is still where the cliff is"
        );
        let first = results.iter().position(|r| r.past_cliff).unwrap();
        assert!(results[first..].iter().all(|r| r.past_cliff));
    }

    #[test]
    fn without_a_reranker_the_cliff_is_read_from_similarity_and_never_from_the_fused_rank() {
        let dummy = |id: &str, score: f32, similarity: Option<f32>| SearchResult {
            artifact_id: id.into(),
            corpus_id: "c".into(),
            score,
            similarity,
            ..Default::default()
        };
        let fused = [1.050, 0.633, 0.383, 0.250, 0.217];
        // Sanity: on the fused scores alone the rule draws its line after #1.
        assert_eq!(cliff(&fused), Some(1));

        let sims = [0.82, 0.80, 0.78, 0.76, 0.74];
        let mut even: Vec<SearchResult> = fused
            .iter()
            .zip(sims)
            .enumerate()
            .map(|(i, (f, s))| dummy(&i.to_string(), *f, Some(s)))
            .collect();
        mark_past_cliff(&mut even, false);
        assert!(
            even.iter().all(|r| !r.past_cliff),
            "a cliff was drawn on a list whose similarity falls evenly"
        );

        let sims = [0.82, 0.80, 0.78, 0.31, 0.29];
        let mut three: Vec<SearchResult> = fused
            .iter()
            .zip(sims)
            .enumerate()
            .map(|(i, (f, s))| dummy(&i.to_string(), *f, Some(s)))
            .collect();
        mark_past_cliff(&mut three, false);
        assert_eq!(
            three.iter().map(|r| r.past_cliff).collect::<Vec<_>>(),
            vec![false, false, false, true, true]
        );

        let mut tail = three.clone();
        tail[4].similarity = None;
        tail.iter_mut().for_each(|r| r.past_cliff = false);
        mark_past_cliff(&mut tail, false);
        assert_eq!(
            tail.iter().map(|r| r.past_cliff).collect::<Vec<_>>(),
            vec![false, false, false, true, true]
        );
        let mut mixed = three.clone();
        mixed[1].similarity = None;
        mixed.iter_mut().for_each(|r| r.past_cliff = false);
        mark_past_cliff(&mut mixed, false);
        assert_eq!(
            mixed.iter().map(|r| r.past_cliff).collect::<Vec<_>>(),
            vec![false, false, false, true, true],
            "a hit with no similarity ended the reading instead of being skipped"
        );
        let mut leading = three.clone();
        leading[0].similarity = None;
        leading.iter_mut().for_each(|r| r.past_cliff = false);
        mark_past_cliff(&mut leading, false);
        assert_eq!(
            leading.iter().map(|r| r.past_cliff).collect::<Vec<_>>(),
            vec![false, false, false, true, true],
            "a lexical-only hit at rank 1 switched the cliff off"
        );
        let mut none = three.clone();
        none.iter_mut().for_each(|r| {
            r.similarity = None;
            r.past_cliff = false;
        });
        mark_past_cliff(&mut none, false);
        assert!(none.iter().all(|r| !r.past_cliff));

        let mut inverted: Vec<SearchResult> = [0.80, 0.40, 0.78]
            .iter()
            .enumerate()
            .map(|(i, s)| dummy(&i.to_string(), 1.0 - i as f32 * 0.1, Some(*s)))
            .collect();
        mark_past_cliff(&mut inverted, false);
        assert!(
            inverted.iter().all(|r| !r.past_cliff),
            "an out-of-order hit manufactured a cliff above itself"
        );

        // Sorted `0.90 0.85 0.20 0.10`: the fall is after the second, so only the last hit is past
        // it.
        let mut still: Vec<SearchResult> = [0.90, 0.20, 0.85, 0.10]
            .iter()
            .enumerate()
            .map(|(i, s)| dummy(&i.to_string(), 1.0 - i as f32 * 0.1, Some(*s)))
            .collect();
        mark_past_cliff(&mut still, false);
        assert_eq!(
            still.iter().map(|r| r.past_cliff).collect::<Vec<_>>(),
            vec![false, false, false, true]
        );
    }

    #[tokio::test]
    async fn a_synthesized_artifact_leading_the_list_marks_the_search_answered() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        // With the coalescing window on, the second search would fold into the first.
        core.feedback.coalesce_secs = 0;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let captured = core
            .store
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    text: "captured text".into(),
                    title: Some("c".into()),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();
        let made_gen = core
            .store
            .insert_synthesized_artifact(
                &crate::store::artifacts::NewSynthesized {
                    text: "generated text".into(),
                    title: Some("g".into()),
                    category: None,
                    tags: vec![],
                    caveats: vec![],
                    cues: vec![],
                },
                &[captured[0].id.clone()],
            )
            .await
            .unwrap();
        crate::jobs::embed::run(&core, &captured[0].id)
            .await
            .unwrap();
        crate::jobs::embed::run(&core, &made_gen.id).await.unwrap();

        // The fake embedder is symmetric: querying the artifact's rendered text lands it first.
        core.search(&q("g\ngenerated text"), Door::Ui)
            .await
            .unwrap();
        core.search(&q("c\ncaptured text"), Door::Ui).await.unwrap();
        core.background.wait_idle().await;
        let now = crate::store::now();
        let events = core.store.events_between(0, now + 1).await.unwrap();
        assert_eq!(events.len(), 2, "{events:?}");
        let by_q: std::collections::HashMap<&str, bool> = events
            .iter()
            .map(|e| (e.query.as_str(), e.answered))
            .collect();
        assert!(by_q["g\ngenerated text"], "{events:?}");
        assert!(!by_q["c\ncaptured text"], "{events:?}");
        let hits = core
            .search(&q("g\ngenerated text"), Door::Ui)
            .await
            .unwrap();
        assert!(hits[0].synthesized && hits[0].model_written);
        assert_eq!(hits[0].origin_count, 1);
    }

    #[tokio::test]
    async fn an_interaction_is_recorded_whenever_the_layer_is_learning() {
        let mut core = test_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let a = core
            .store
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    text: "a".into(),
                    ..Default::default()
                }],
            )
            .await
            .unwrap()[0]
            .id
            .clone();
        core.record_interaction(&a, None, Some("me"));
        core.background.wait_idle().await;
        let now = crate::store::now();
        assert!(
            core.store
                .interactions_between(0, now + 1)
                .await
                .unwrap()
                .is_empty(),
            "an open was written down with `[learn]` off"
        );
        core.learn.enabled = true;
        core.record_interaction(&a, None, Some("me"));
        core.record_interaction(&a, Some("other"), Some("me"));
        core.background.wait_idle().await;
        let got = core.store.interactions_between(0, now + 1).await.unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].kind, "opened");
        assert_eq!(got[1].kind, "pivoted");
        assert_eq!(got[1].via.as_deref(), Some("other"));
    }
}
