use super::Core;
use crate::error::{Error, Result};
use crate::store::artifacts::ArtifactStatus;
use crate::store::feedback::{Door, Origin};
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
    /// A model wrote this text (merged, or synthesized from a pursuit). Never
    /// silently indistinguishable from captured text.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub model_written: bool,
    /// Written from a pursuit. What the stopping rule reads.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub synthesized: bool,
    /// How many corpora it draws from — the badge's source count.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub origin_count: usize,
    /// This hit moved up because it is more accessible than the ones it passed
    /// — recently and often reached. Bounded by `associate.prime_lift`, never
    /// past rank 1, and said out loud wherever it happened: nothing about the
    /// order is silent.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub primed: bool,
    /// This sitting has already been in this artifact. Said beside `primed`,
    /// because "you were just reading this" and "this is reached often" are
    /// two different reasons to be higher up the list and the page should not
    /// pass one off as the other.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub in_sitting: bool,
    /// This hit sits past the cliff: the one step in this list's scores that
    /// accounts for more of the fall than the rest of the list together. See
    /// `cliff`. It still competed and still placed — nothing is reordered or
    /// dropped — but the page stops claiming it is an answer.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub past_cliff: bool,
    /// The ranked hit that recalled this one. `None` for a ranked hit — which
    /// is every hit inside `limit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    /// What the judge said the relation is, where a link has been judged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
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
            past_cliff: false,
            via: None,
            reason: None,
        }
    }
}

/// Visible to the rest of `core` so `ask` can decay link weights to the same
/// clock the results rail does, rather than keeping a second reading of it.
pub(super) fn now_secs() -> i64 {
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
        // A merge counts against every corpus it drew from; a passage or a
        // captured artifact against its one. The payload carries the
        // projection; a point written before it existed falls back to
        // `corpus_id`.
        let keys: Vec<String> = if h.payload.origin_corpora.is_empty() {
            vec![h.payload.corpus_id.clone()]
        } else {
            h.payload.origin_corpora.clone()
        };
        let over = keys
            .iter()
            .any(|k| seen.get(k).copied().unwrap_or(0) >= max);
        if !over {
            // Only a hit that took a place counts against one. A displaced hit
            // is over its cap in *one* of its corpora, and charging it to the
            // others as well let a five-corpus merge that never made the list
            // evict unrelated hits from the four that had room for it.
            for k in &keys {
                *seen.entry(k.clone()).or_insert(0) += 1;
            }
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

/// The largest gap must exceed this many times the mean of the other gaps to
/// count as a cliff. A constant rather than configuration: a default that
/// changes what the page claims moves after the harness has run, and until
/// then a number with its reasoning beside it is more honest than a knob.
pub const CLIFF_FACTOR: f32 = 3.0;

/// And it must be at least this share of the top score.
///
/// Without a floor, a plateau made any difference at all a cliff: when every
/// other gap is exactly zero — which is what a saturated reranker produces once
/// several hits round to the same f32 — their mean is zero, and every positive
/// number exceeds three times zero. The page then drew its line through a pair
/// of hits that were numerically tied. A share of the top score rather than a
/// fixed gap keeps the rule scale-free, which is the same reason `CLIFF_FACTOR`
/// measures a step against the list's other steps instead of against a number.
pub const CLIFF_MIN_SHARE: f32 = 0.01;

/// Where the relevance falls off in a ranked list, as the number of hits above
/// the fall — or `None` when no single step stands out.
///
/// Hybrid scores are fused ranks and mean nothing across queries; reranker
/// scores and the recency stage live on other scales again. A step compared
/// against its own list's other steps is scale-free: the cliff is the one gap
/// that is larger than `CLIFF_FACTOR` times the mean of all the others, and at
/// least `CLIFF_MIN_SHARE` of the top score. Below three hits there is one gap
/// and nothing to compare it to. Gaps are read in list order and a negative one
/// — a primed near-tie lifted past its neighbour — is a near-tie, not a fall.
///
/// This is also where `ask` will stop packing excerpts, which is why it is a
/// function over scores rather than a step inside `search_with`.
pub fn cliff(scores: &[f32]) -> Option<usize> {
    if scores.len() < 3 {
        return None;
    }
    let gaps: Vec<f32> = scores.windows(2).map(|w| (w[0] - w[1]).max(0.0)).collect();
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
    // A tie is not a fall, whatever the comparison below makes of it.
    if largest <= CLIFF_MIN_SHARE * scores[0].abs() {
        return None;
    }
    let others = (gaps.iter().sum::<f32>() - largest) / (gaps.len() - 1) as f32;
    (largest > CLIFF_FACTOR * others).then_some(at + 1)
}

/// Flag every hit from the cliff on. The list is left in its order and at its
/// length; nothing about the cliff is silent and nothing about it is a change.
fn mark_past_cliff(results: &mut [SearchResult]) {
    let scores: Vec<f32> = results.iter().map(|r| r.score).collect();
    if let Some(above) = cliff(&scores) {
        for r in results.iter_mut().skip(above) {
            r.past_cliff = true;
        }
    }
}

/// Move hits up on activation, within hard bounds.
///
/// Rank-based rather than score-based: hybrid scores are fused ranks and mean
/// nothing across queries, while "moved up two places" means the same thing
/// every time. The activation is normalised within this one list, so `margin` is
/// a fraction of the most accessible hit here rather than an absolute weight —
/// which is what makes one default work for a list of ones and a list of
/// hundreds.
///
/// Index 0 is untouchable and index 1 cannot move, because moving it would
/// displace rank 1. An exact match is never buried.
///
/// Every row's destination is decided against the ORIGINAL ordering in one
/// pass, then the list is reordered once. An earlier attempt did this by
/// repeatedly swapping adjacent rows in place, insertion-sort style — but
/// once a row swaps, the array no longer records which row already spent its
/// budget: a later position, reached by a *different* row after an earlier
/// row moved through it, got a fresh budget for free. On a five-element list
/// with only the last row activated and `lift = 2`, that let it climb three
/// places instead of two, and on a longer list the overrun only grows. Here
/// every comparison reads from an activation snapshot untouched by any move,
/// so no row can ever borrow a gap another row's move opened.
fn prime(
    mut results: Vec<SearchResult>,
    activation: &HashMap<String, f64>,
    margin: f64,
    lift: usize,
    sitting: &std::collections::HashSet<String>,
) -> Vec<SearchResult> {
    // Marked before anything can return. `in_sitting` is a fact about the row —
    // this sitting has been in it — and not a consequence of the reordering. A
    // list of two is a list nothing can move in, not a list where the badge
    // stops being true, and a badge that disappears on short result lists reads
    // as the page forgetting rather than as a rule.
    for r in &mut results {
        r.in_sitting = sitting.contains(&r.artifact_id);
    }
    if lift == 0 || results.len() < 3 {
        return results;
    }
    let max = activation.values().copied().fold(0.0f64, f64::max);
    if max <= 0.0 && sitting.is_empty() {
        return results;
    }
    let n = results.len();
    // Normalised once, against the original list, and never touched again.
    //
    // The sitting enters here, as a value in the same scale, rather than as a
    // second pass: one budget, one walk, one `lift`. A hit cannot be lifted
    // `prime_lift` places for being accessible and then `prime_lift` again for
    // having been read ten minutes ago — which is what two passes would do, and
    // the second lift would be the one nobody bounded.
    let acts: Vec<f64> = results
        .iter()
        .map(|r| {
            let act = match max > 0.0 {
                true => activation.get(&r.artifact_id).copied().unwrap_or(0.0) / max,
                false => 0.0,
            };
            match sitting.contains(&r.artifact_id) {
                // The top of the same normalised scale: what this sitting has
                // been in is as accessible as anything in the base gets.
                true => act.max(1.0),
                false => act,
            }
        })
        .collect();

    // For each row, how many of its original predecessors — walked nearest
    // first — it beats by strictly more than `margin`, stopping at the first
    // it does not beat, capped at `lift`, and floored so the target index
    // can never fall below 1: rank 1 is never displaced. Rows 0 and 1 never
    // move, so the walk only starts at index 2.
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

    // One stable sort by destination decides the final order. The secondary
    // key is load-bearing: without it, a climbing row sorts behind the row
    // it was meant to pass, because its original index is larger.
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
        .map(|(pos, i)| {
            let mut r = slots[i]
                .take()
                .expect("each original index used exactly once");
            // Only a climb is priming: this is exactly the row that ended up
            // at a lower index than it started at. A hit that was merely
            // displaced by another row's climb did not move up, and
            // labelling it would say something untrue about it.
            r.primed = pos < i;
            r
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

        // Only a retrieval raises accessibility. A list nobody asked for —
        // `resurface`, an association — is shown, not reached, and the row in
        // §5.4 that reads zero is the one-way guard this implements.
        //
        // Also gated on `associating()`: activation only exists to feed
        // priming, and an install that never opted into `feedback` must see
        // no artifact's activation move, or the ranked order could start
        // changing under a feature it never turned on.
        // Never `maybe_promote` here: exposure is not engagement. The only
        // thing exposure can trigger is the opposite — an eager artifact shown
        // and shown and never confirmed is re-read — and that ships disabled.
        if counts_as_hit && self.associating() {
            let ids: Vec<String> = results.iter().map(|r| r.artifact_id.clone()).collect();
            let shown: Vec<(String, i64)> = if self.promote.resynthesize_after_unconfirmed > 0 {
                results
                    .iter()
                    .map(|r| {
                        (
                            r.artifact_id.clone(),
                            hit_counts.get(&r.artifact_id).copied().unwrap_or(0) + 1,
                        )
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let core = self.clone();
            let (delta, half_life) = (self.activation.retrieved, self.activation.half_life_days);
            let at = now_secs();
            self.background.spawn(async move {
                if let Err(e) = core.store.bump_activation(&ids, delta, half_life, at).await {
                    tracing::warn!(error = %e, "could not raise activation for a search");
                }
                if !shown.is_empty()
                    && let Err(e) = crate::jobs::promote::maybe_resynthesize(&core, &shown).await
                {
                    tracing::warn!(error = %e, "could not check the re-synthesis threshold");
                }
            });
        }
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

        // An open is a deliberate act and counts for less than a retrieval:
        // clicking a candidate says you looked, not that it answered. Gated
        // on `associating()` for the same reason as `mark_seen`: activation
        // exists only to feed priming, so it must not move at all while the
        // layer is off.
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
                // An open is an engagement: the one kind of bump that can
                // promote. See `jobs::promote::maybe_promote`.
                if let Err(e) = crate::jobs::promote::maybe_promote(&core, &ids, at).await {
                    tracing::warn!(error = %e, "could not check the promotion threshold");
                }
            });
        }
    }

    /// What happened after the list rendered: this artifact was opened, or
    /// reached from `via` — a neighbour, an association, a continuation. The
    /// pursuit sweep attaches it to a search by time and scope; nothing here
    /// names one. Off the request path, and a no-op unless pursuits are on
    /// and searches are being recorded.
    pub fn record_interaction(&self, artifact_id: &str, via: Option<&str>, scope: Option<&str>) {
        if !self.pursuit.enabled || !self.feedback.enabled {
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

    /// How long an artifact stayed open. Capped — a tab left open overnight is
    /// not a day of reading — and a no-op unless pursuits are on.
    pub fn record_dwell(&self, artifact_id: &str, secs: i64, scope: Option<&str>) {
        if !self.pursuit.enabled || !self.feedback.enabled || secs <= 0 {
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

    /// Each artifact's activation, already decayed to now.
    ///
    /// The one SQLite read the query path takes. It can only add: a failure is
    /// one warning and an empty map, and everything downstream then behaves
    /// exactly as it did before any of this existed.
    async fn activation_now(&self, ids: &[String]) -> HashMap<String, f64> {
        let at = now_secs();
        match self.store.activation_of(ids).await {
            Ok(rows) => rows
                .into_iter()
                .map(|(id, (value, stamp))| {
                    (
                        id,
                        crate::store::links::decayed(
                            value,
                            stamp,
                            at,
                            self.activation.half_life_days,
                        ),
                    )
                })
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "could not read activation; results are unprimed");
                HashMap::new()
            }
        }
    }

    /// Artifacts linked to the top of the answer, appended beside it.
    ///
    /// One hop only. Spreading further is what a graph view would be for, and
    /// there is none. Everything here is additive: it never removes or reorders
    /// a ranked hit, and a store that will not answer produces an empty list and
    /// one warning.
    ///
    /// `filter` is the caller's own narrowing, and it applies here too. A
    /// search for `category=runbook` is a statement about what the searcher
    /// will accept, not a hint to the ranker, and an association that walks
    /// past it hands back exactly the rows they asked not to see. `links_from`
    /// already excludes anything not active and not live, so what is left to
    /// re-check here is what the caller typed.
    async fn associated(
        &self,
        results: &[SearchResult],
        filter: &SearchFilter,
    ) -> Vec<SearchResult> {
        let anchors: Vec<String> = results
            .iter()
            .take(self.associate.spread_from)
            .map(|r| r.artifact_id.clone())
            .collect();
        if anchors.is_empty() || self.associate.spread_max == 0 {
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
                // Not `spread_max`: `links_from` returns one row per (anchor,
                // link) with no dedup across anchors, and it truncates before
                // the filtering below runs. The rows most likely to be
                // discarded here are anchor-to-anchor links — both ends
                // already in `results` — and those are exactly the links
                // co-retrieval makes most likely to exist. A limit of just
                // `spread_max` lets such rows consume the whole budget and
                // leave nothing for genuinely new artifacts. This bound
                // leaves room for every anchor-to-anchor pair among the
                // ranked anchors to be discarded and still fill the budget;
                // the real cap is the `out.len() >= spread_max` check below.
                //
                // Saturating, and `try_from` rather than `as`: both operands
                // are operator-typed. `Config::normalize` clamps them to
                // something a person could mean, so this cannot overflow in
                // practice — but `as` on a `usize` that did would wrap to a
                // negative `i64` and switch association silently off, which is
                // the one failure nobody would report as a bug.
                i64::try_from(
                    self.associate
                        .spread_max
                        .saturating_mul(self.associate.spread_from.saturating_add(1)),
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
            if out.len() >= self.associate.spread_max {
                break;
            }
            if !have.insert(l.other.clone()) {
                continue;
            }
            // Read from SQLite rather than the vector store: the row is already
            // one connection away, and the payload adds nothing this needs.
            let Ok(c) = self.store.get_artifact(&l.other).await else {
                continue;
            };
            // The same predicate the vector store applies, on the same two
            // fields: every listed tag present, and the category exact.
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
                // Not a rank and not a similarity: this hit did not compete for
                // a place in the list, it was recalled beside it.
                score: 0.0,
                status: Some(c.status),
                superseded_by: c.superseded_by,
                last_verified_at: c.last_verified_at,
                weak: false,
                primed: false,
                in_sitting: false,
                past_cliff: false,
                via: Some(l.via),
                reason: l.reason,
                model_written: false,
                synthesized: false,
                origin_count: 0,
            });
        }
        out
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

    /// The embedding of `q`, if a search just made it. `search_with` caches the
    /// query vector under the whitespace-normalised query; a caller that ran a
    /// search a moment ago and wants to store the vector it used reads it here
    /// rather than paying for the embedding twice. `None` only if the cache
    /// evicted it in between, which a caller must tolerate.
    pub fn cached_query_vector(&self, q: &str) -> Option<Vec<f32>> {
        let key = q.split_whitespace().collect::<Vec<_>>().join(" ");
        self.query_cache.lock().ok().and_then(|c| c.get(&key))
    }

    /// `search`, with the per-source cap chosen by the caller and what the
    /// search cost. `cap` of `None` lets a single source supply every result:
    /// `ask` wants that, since a question is often answered by one document.
    /// The timing is reported as a `server-timing` header, so a sluggish box
    /// points at the embedder or the vector store without anyone opening a log.
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
                let v = self.embedder.embed_query(query.q.trim()).await?;
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
            // Ordinary search asks the whole base. Narrowing to one document is
            // the coverage check's question, not a searcher's.
            corpus_id: None,
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

        // Before capture and before the truncate, so the pool that is recorded
        // is the order the searcher was actually shown — a judged rank has to
        // be a rank that happened. Bounded by `prime_lift` and never past rank
        // 1, so this can reorder near-ties and nothing else.
        //
        // Held off the same two doors as association below, for the same
        // reason in a different shape. `Ask` does not show this list to
        // anybody: it turns the head of it into excerpts and, when they do not
        // all fit the context window, keeps a prefix and drops the tail on the
        // stated grounds that the tail matched the question least
        // (`src/core/ask.rs`). Reordering by accessibility makes that untrue,
        // and the excerpt lost is then the one that answered best — a silent
        // change to the answer, on a path where nobody can see what was cut.
        // `Judge` needs the pool it labels to be the pool the ranking produced.
        if self.associating()
            && self.associate.prime_lift > 0
            && !matches!(door, Door::Ask | Door::Judge)
        {
            let ids: Vec<String> = results.iter().map(|r| r.artifact_id.clone()).collect();
            let activation = self.activation_now(&ids).await;
            // Off by default and empty when off: this is the only part of the
            // sitting that moves an order, and the same query ranking
            // differently in two sittings is what is disorienting about it.
            // Held off the doors priming is already held off, for the same
            // reasons, and off every door with no session for the reason in
            // `Origin::session`.
            let sitting: std::collections::HashSet<String> = match self.sitting.prime {
                true => origin
                    .session
                    .as_deref()
                    .map(|s| {
                        self.sittings
                            .read(s, now_secs(), self.pursuit.idle_secs as i64)
                            .touched
                            .into_iter()
                            .collect()
                    })
                    .unwrap_or_default(),
                false => Default::default(),
            };
            results = prime(
                results,
                &activation,
                self.associate.prime_margin,
                self.associate.prime_lift,
                &sitting,
            );
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
                // The stopping rule for pursuits: a synthesized artifact at
                // final rank 1, at or above `weak_below`, means the base
                // answered. A fused rank says where a hit placed and not how
                // good it was, which is why `weak` is read beside it.
                answered: results.first().is_some_and(|r| r.synthesized && !r.weak),
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
        // On the list the caller will see, in its final order: after priming,
        // after the truncate, and before association appends hits that never
        // competed for a place. Marks, never reorders or drops.
        mark_past_cliff(&mut results);
        if query.mark {
            // A query answered these, so they count as retrievals.
            self.mark_seen(&results, &hit_counts, true);
        }
        // After the truncate and after capture, so an association can only ever
        // add: it is outside `limit`, outside the recorded pool, and outside the
        // retrieval count. See `Touch::shown`.
        //
        // Gated on the door, not on `captured()` — that predicate means
        // "recorded for relevance feedback", an unrelated idea that happens
        // to select the same four doors today. `Ask` is excluded because it
        // synthesises an answer from `results` as excerpts; text that never
        // matched the question must not become source material. `Judge` is
        // excluded because its query is composed in full knowledge of the
        // answer and needs a clean pool to label, not a widened one.
        if self.associating() && !matches!(door, Door::Ask | Door::Judge) {
            let recalled = self.associated(&results, &filter).await;
            if !recalled.is_empty() {
                self.mark_seen(&recalled, &HashMap::new(), false);
                results.extend(recalled);
            }
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
    async fn a_deliberate_search_makes_what_it_returned_more_accessible() {
        let mut core = test_core().await;
        core.feedback.enabled = true;
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
        assert!(after > before, "a retrieval raised nothing");
    }

    #[tokio::test]
    async fn typing_does_not_make_what_it_happened_to_match_more_accessible() {
        // The same rule as `last_seen_at`: an incremental request is not a
        // retrieval, and letting every keystroke raise activation would make
        // accessibility a function of how slowly someone types.
        let mut core = test_core().await;
        core.feedback.enabled = true;
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
        // `resurface` shows what has been forgotten. Counting that as a reason
        // to be more accessible is the loop this whole design is built to close.
        let mut core = test_core().await;
        core.feedback.enabled = true;
        seed_from(&core, "old", &[("long forgotten", "c", &[])]).await;
        sqlx::query("UPDATE artifacts SET created_at = ?")
            .bind(now_secs() - FORGOTTEN_AFTER_DAYS * SECONDS_PER_DAY - 1)
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
    async fn opening_an_artifact_makes_it_more_accessible_by_less_than_a_retrieval() {
        let mut core = test_core().await;
        core.feedback.enabled = true;
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
        // Loose on purpose: `before` is the raw stored activation, but the bump
        // re-reads it through decay, so the error grows with wall-clock time
        // between the artifact's creation and the bump (5.7e-7 after one
        // second, 1.15e-6 after two) and a loaded machine can straddle 1e-6.
        // 1e-3 still separates `opened` (0.5) from `retrieved` (1.0) by three
        // orders of magnitude, so it cannot mask a real regression — don't
        // tighten it back without addressing the decay re-read instead.
        assert!((after - before - core.activation.opened).abs() < 1e-3);
        assert!(core.activation.opened < core.activation.retrieved);
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
                status: None,
                last_verified_at: None,
                superseded_by: None,
                origin_corpora: vec![],
                provenance: None,
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
        // A merge of `a` and `b` counts against both: with `a` already full it
        // is displaced even though `b` has room.
        let mut m = hit("m", "", 0.85);
        m.payload.origin_corpora = vec!["a".into(), "b".into()];
        let with_merge = vec![
            hit("a1", "a", 0.9),
            hit("a2", "a", 0.8),
            m,
            hit("b1", "b", 0.7),
        ];
        assert_eq!(
            ids(cap_per_corpus(with_merge, 2, 3)),
            vec!["a1", "a2", "b1"]
        );
        // And a merge that was displaced took no place in the corpora it never
        // made the list for. Charging it to all of them let one merge spanning
        // `a` and `b` — dropped because `a` was full — evict `b2` from a corpus
        // with room to spare.
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
            ids(cap_per_corpus(with_merge, 2, 4)),
            vec!["a1", "a2", "b1", "b2"],
            "a displaced hit must not spend a slot in a corpus it did not enter"
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
        // The opt-in exists to let an operator look at what was retired, so it
        // must reach a deprecation without also reaching a supersession.
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

    fn ranked(ids: &[&str]) -> Vec<SearchResult> {
        ids.iter()
            .map(|id| SearchResult {
                artifact_id: (*id).into(),
                corpus_id: "c".into(),
                title: None,
                text: String::new(),
                category: None,
                tags: vec![],
                score: 0.5,
                status: None,
                superseded_by: None,
                last_verified_at: None,
                weak: false,
                primed: false,
                in_sitting: false,
                past_cliff: false,
                via: None,
                reason: None,
                model_written: false,
                synthesized: false,
                origin_count: 0,
            })
            .collect()
    }

    fn order(rs: &[SearchResult]) -> Vec<&str> {
        rs.iter().map(|r| r.artifact_id.as_str()).collect()
    }

    #[test]
    fn a_hit_climbs_at_most_two_places_and_never_past_the_first() {
        // Rank-based rather than score-based on purpose: hybrid scores are
        // fused ranks and mean nothing across queries, while "moved up two
        // places" means the same thing every time — and can be tested here.
        let act = HashMap::from([("d".to_string(), 4.0)]);
        let out = prime(
            ranked(&["a", "b", "c", "d"]),
            &act,
            0.5,
            2,
            &Default::default(),
        );
        assert_eq!(order(&out), vec!["a", "d", "b", "c"]);
        assert!(out[1].primed, "the hit that moved must say so");
        assert!(!out[2].primed, "the hit it passed did not move up");
    }

    #[test]
    fn the_most_active_hit_cannot_displace_an_exact_match() {
        let act = HashMap::from([("b".to_string(), 9.0)]);
        let out = prime(ranked(&["a", "b", "c"]), &act, 0.5, 2, &Default::default());
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
        );
        assert_eq!(order(&out), vec!["a", "d", "b", "c"]);
        assert!(out[1].primed);
        assert!(out[1].in_sitting, "the page cannot say why it moved");
    }

    #[test]
    fn a_list_too_short_to_reorder_still_says_what_the_sitting_read() {
        // `in_sitting` is a fact about the row, not a consequence of a move. A
        // list of two is a list nothing can be lifted in — but a badge that
        // vanishes on short lists reads as the page forgetting, not as a rule.
        let sitting = std::collections::HashSet::from(["b".to_string()]);
        let out = prime(ranked(&["a", "b"]), &HashMap::new(), 0.5, 2, &sitting);
        assert_eq!(order(&out), vec!["a", "b"], "nothing can move on two rows");
        assert!(out[1].in_sitting);
        assert!(!out[0].in_sitting);
        assert!(out.iter().all(|r| !r.primed), "and nothing was primed");
    }

    #[test]
    fn the_sitting_and_activation_share_one_budget() {
        // Two passes would give a hit `prime_lift` places for being accessible
        // and `prime_lift` again for having been read ten minutes ago — and the
        // second lift would be the one nobody bounded. One walk, one `lift`.
        let act = HashMap::from([("e".to_string(), 9.0)]);
        let sitting = std::collections::HashSet::from(["e".to_string()]);
        let out = prime(ranked(&["a", "b", "c", "d", "e"]), &act, 0.5, 2, &sitting);
        assert_eq!(
            order(&out),
            vec!["a", "b", "e", "c", "d"],
            "a hit in both moved further than one lift"
        );
    }

    #[test]
    fn the_sitting_cannot_displace_the_first_hit_either() {
        // Rank 0 never moves, whatever the reason for moving would have been.
        let sitting = std::collections::HashSet::from(["b".to_string(), "c".to_string()]);
        let out = prime(ranked(&["a", "b", "c"]), &HashMap::new(), 0.5, 2, &sitting);
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
        );
        assert_eq!(order(&out), vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn activation_that_is_merely_higher_does_not_move_anything() {
        // The margin is what keeps this from reshuffling every list: two hits
        // that are both somewhat active stay in the order the ranking gave.
        let act = HashMap::from([("c".to_string(), 4.0), ("d".to_string(), 3.6)]);
        let out = prime(
            ranked(&["a", "b", "c", "d"]),
            &act,
            0.5,
            2,
            &Default::default(),
        );
        assert_eq!(
            order(&out),
            vec!["a", "c", "b", "d"],
            "only c clears the margin"
        );
    }

    #[test]
    fn a_hit_climbs_exactly_the_lift_and_no_further_however_long_the_list_is() {
        // An insertion-sort-style implementation can let a row that already
        // spent its climb budget be mistaken for a fresh row once something
        // else moves through the position it landed on. Five elements is
        // enough to expose that: `e` should climb exactly two, not three.
        let act = HashMap::from([("e".to_string(), 1.0)]);
        let out = prime(
            ranked(&["a", "b", "c", "d", "e"]),
            &act,
            0.5,
            2,
            &Default::default(),
        );
        assert_eq!(order(&out), vec!["a", "b", "e", "c", "d"]);
        let moved = order(&out).iter().position(|id| *id == "e").unwrap();
        assert_eq!(moved, 4 - 2, "e must climb exactly prime_lift positions");
        assert!(out[moved].primed);
    }

    #[test]
    fn the_lift_bound_holds_on_a_longer_list_too() {
        // Same shape, stretched to seven: the last row must still land
        // exactly `lift` positions up, at index 4 rather than index 1.
        let act = HashMap::from([("g".to_string(), 1.0)]);
        let out = prime(
            ranked(&["a", "b", "c", "d", "e", "f", "g"]),
            &act,
            0.5,
            2,
            &Default::default(),
        );
        let moved = order(&out).iter().position(|id| *id == "g").unwrap();
        assert_eq!(moved, 4, "g must land at index 4, not be pulled to index 1");
        assert_eq!(6 - moved, 2, "the climb must equal prime_lift exactly");
    }

    #[tokio::test]
    async fn priming_changes_the_order_a_search_returns_and_says_which_hit_moved() {
        let mut core = test_core().await;
        core.feedback.enabled = true;
        core.associate.prime_lift = 2;
        let texts: Vec<(&str, &str, &[&str])> = (0..6)
            .map(|_| ("alpha text about it", "note", &[][..]))
            .collect();
        seed(&core, &texts).await;
        reembed_all(&core).await;

        let plain = {
            let mut off = core.clone();
            off.associate.prime_lift = 0;
            off.search(&q("alpha text about it"), Door::Ui)
                .await
                .unwrap()
        };
        assert!(plain.len() >= 4, "this test needs a list to reorder");
        assert!(plain.iter().all(|r| !r.primed));

        // The one at the bottom is the one people actually keep confirming.
        let bottom = plain.last().unwrap().artifact_id.clone();
        sqlx::query("UPDATE artifacts SET activation = 100.0, activated_at = ? WHERE id = ?")
            .bind(now_secs())
            .bind(&bottom)
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
    async fn a_marked_search_does_not_touch_activation_while_feedback_is_off() {
        // Shipped defaults: `feedback.enabled = false`, `associate.enabled =
        // true`. Links are learned from recorded searches, and none are being
        // recorded, so the whole associative layer — including activation,
        // which exists only to feed priming — must stay dark. `test_core()`
        // is exactly this combination; nothing here turns `feedback` on.
        let core = test_core().await;
        assert!(!core.feedback.enabled && core.associate.enabled);
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
            "a marked search raised activation with feedback.enabled false"
        );
    }

    #[tokio::test]
    async fn priming_never_reaches_the_order_while_feedback_is_off() {
        // The spec promise (§11): "existing installs see nothing change until
        // they opt in." `associate.enabled` alone must not let a large
        // activation move the ranked order — the order must be byte-identical
        // to `prime_lift = 0`, not merely bounded.
        let core = test_core().await;
        assert!(!core.feedback.enabled && core.associate.enabled);
        assert_eq!(core.associate.prime_lift, 2, "the shipped default");
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

        // The one at the bottom is given a huge activation directly — exactly
        // what moved the order in the "feature on" test above.
        let bottom = plain.last().unwrap().artifact_id.clone();
        sqlx::query("UPDATE artifacts SET activation = 100.0, activated_at = ? WHERE id = ?")
            .bind(now_secs())
            .bind(&bottom)
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
            "the order changed even though feedback.enabled is false"
        );
        assert!(with_huge_activation.iter().all(|r| !r.primed));
    }

    #[tokio::test]
    async fn priming_never_reaches_the_list_ask_and_judge_are_given() {
        // `ask` does not show this list to anyone: it turns the head of it into
        // excerpts and, when they do not all fit the context window, keeps a
        // prefix and drops the tail — on the stated grounds that the tail
        // matched the question least. Reorder by accessibility and the excerpt
        // dropped is the one that answered best, invisibly. `judge` needs the
        // pool it labels to be the pool the ranking produced.
        let mut core = test_core().await;
        core.feedback.enabled = true;
        core.associate.prime_lift = 2;
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
        sqlx::query("UPDATE artifacts SET activation = 100.0, activated_at = ? WHERE id = ?")
            .bind(now_secs())
            .bind(&bottom)
            .execute(&core.store.pool)
            .await
            .unwrap();

        // The same activation that does move the order on the UI door.
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
        // A category is a statement about what the searcher will accept, not a
        // hint to the ranker. An artifact recalled beside a hit still has to
        // satisfy it, or the search hands back exactly the rows they narrowed
        // away.
        let mut core = test_core().await;
        core.feedback.enabled = true;
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

        // Unfiltered, the linked artifact is recalled beside the ranked hit.
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

    /// The id of the artifact whose text is exactly this. Ordering from
    /// `list_all_artifact_ids` is not promised, and a test that assumed one
    /// would pass or fail on which row SQLite happened to return first.
    async fn id_of(core: &crate::core::Core, text: &str) -> String {
        sqlx::query_scalar("SELECT id FROM artifacts WHERE text = ?")
            .bind(text)
            .fetch_one(&core.store.pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_linked_artifact_is_recalled_beside_the_answer_and_says_what_recalled_it() {
        let mut core = test_core().await;
        core.feedback.enabled = true;
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
    }

    #[tokio::test]
    async fn ask_and_judge_never_receive_an_association_but_ui_does() {
        // `ask` feeds `results` to the model as excerpts to synthesise an
        // answer from; text that never matched the question must not become
        // source material. `judge` composes its query in full knowledge of
        // the answer and needs a clean pool to label. Both are excluded by
        // door, not by `captured()` — that predicate means something
        // unrelated (recorded for relevance feedback) that happens to select
        // the same four doors today. One door alone would not prove the
        // gate is door-shaped rather than coincidental, so this checks both
        // an excluded door and an included one against the same link.
        let mut core = test_core().await;
        core.feedback.enabled = true;
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
        core.feedback.enabled = true;
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

    /// The captured pool, as `(role, shown)` ordered by rank — `role` is `"a"`
    /// or `"b"` depending on which of the two seeded ids the row names, so two
    /// separately-seeded cores (whose artifact ids are fresh UUIDs and so never
    /// equal) can still be compared for shape.
    async fn captured_pool(core: &crate::core::Core, a: &str, b: &str) -> Vec<(&'static str, i64)> {
        let rows: Vec<(String, i64)> =
            sqlx::query_as("SELECT artifact_id, shown FROM search_candidates ORDER BY rank")
                .fetch_all(&core.store.pool)
                .await
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
        // The failure mode of any Hebbian system: a link recalls an artifact, is
        // strengthened by having done so, and recalls it harder next time. Both
        // loops have to be closed by construction: the recalled hit must not be
        // written as a candidate, and must not count as a retrieval.
        //
        // This cannot be checked by asking "is `b` absent from
        // `search_candidates`" directly: on a base this small, `b` is a
        // legitimate low-ranked candidate of the plain hybrid search with or
        // without any link — `feedback.candidates` over-fetches wider than the
        // two-artifact corpus, so the ordinary capture path records it anyway,
        // unshown. That is correct and expected, and unrelated to association.
        // What has to be proven instead is a control comparison: the captured
        // pool is identical whether or not the link exists, which is what shows
        // the link changed nothing about what was recorded — plus a check that
        // the treatment side effect (the link) actually took hold, or the
        // comparison would pass vacuously on a run where nothing associated at
        // all. A later reader tempted to simplify this back to a direct
        // absence check would reintroduce a test that fails on an artifact of
        // the fixture rather than a bug.
        async fn seeded() -> (crate::core::Core, String, String) {
            let mut core = test_core().await;
            core.feedback.enabled = true;
            seed_from(&core, "one", &[("alpha text", "note", &[])]).await;
            seed_from(&core, "two", &[("something else entirely", "note", &[])]).await;
            reembed_all(&core).await;
            let a = id_of(&core, "alpha text").await;
            let b = id_of(&core, "something else entirely").await;
            (core, a, b)
        }

        let mut query = q("t0\nalpha text");
        query.limit = 1;

        // Unlinked control.
        let (unlinked, a_u, b_u) = seeded().await;
        let unlinked_out = unlinked.search(&query, Door::Ui).await.unwrap();
        unlinked.background.wait_idle().await;
        let unlinked_pool = captured_pool(&unlinked, &a_u, &b_u).await;

        // Linked treatment.
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

        // 1. The captured pool — id and shown, in rank order — is identical
        // either way: the link changed nothing about what the search recorded.
        assert_eq!(
            unlinked_pool, linked_pool,
            "the link changed what the search recorded as a candidate"
        );

        // 2. Being recalled did not count as a retrieval.
        assert!(
            (after - before).abs() < 1e-9,
            "being recalled raised activation"
        );
    }

    #[tokio::test]
    async fn a_hidden_artifact_is_never_recalled_by_association() {
        let mut core = test_core().await;
        core.feedback.enabled = true;
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
    async fn a_weak_link_is_not_strong_enough_to_recall_anything() {
        let mut core = test_core().await;
        core.feedback.enabled = true;
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
        // `links_from` returns one row per (anchor, link) with no dedup
        // across anchors, and truncates at its own limit before `associated`
        // filters out rows whose other end is already ranked. A link between
        // two of the top `spread_from` hits is therefore fetched twice — once
        // per anchor — and those are exactly the rows co-retrieval makes most
        // likely to exist. If the store limit were just `spread_max`, three
        // such rows could fill the entire budget and then all be discarded,
        // leaving nothing for an artifact that is linked to the ranked hits
        // but did not itself rank. This calls `associated` directly, on
        // hand-built anchors, so the outcome does not depend on the fake
        // embedder's incidental ranking of a fifth candidate.
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

        // Every pair among the three ranked hits (a, b, c) is linked more
        // strongly than the single link from a ranked hit to the one
        // artifact that never ranks (d) — so a limit that keeps the
        // strongest rows first keeps the redundant, discardable ones and
        // drops the useful one, which is exactly the bug.
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
            corpus_id: String::new(),
            title: None,
            text: String::new(),
            category: None,
            tags: vec![],
            score: 1.0,
            status: None,
            superseded_by: None,
            last_verified_at: None,
            weak: false,
            primed: false,
            in_sitting: false,
            past_cliff: false,
            via: None,
            reason: None,
            model_written: false,
            synthesized: false,
            origin_count: 0,
        };
        let results = vec![dummy(a.clone()), dummy(b.clone()), dummy(c.clone())];

        let out = core.associated(&results, &SearchFilter::default()).await;
        let ids: Vec<&str> = out.iter().map(|r| r.artifact_id.as_str()).collect();
        assert!(
            ids.contains(&d.as_str()),
            "an artifact linked only to the ranked hits, not among them, was not recalled: {ids:?}"
        );
    }

    // ── The cliff ────────────────────────────────────────────────────────────

    /// Hybrid RRF as Qdrant fuses it: two hits both branches agreed on, then
    /// hits only the dense branch found. The step between them is the cliff.
    #[test]
    fn a_hybrid_list_falls_off_where_only_one_branch_still_matches() {
        let both = |r: f32| 1.0 / (60.0 + r) + 1.0 / (60.0 + r);
        let one = |r: f32| 1.0 / (60.0 + r);
        let scores = [both(1.0), both(2.0), one(3.0), one(4.0), one(5.0), one(6.0)];
        assert_eq!(cliff(&scores), Some(2));
    }

    /// Dense-only RRF falls evenly, `1/(60+r)`: no step stands out, so the
    /// list makes no claim about where its answers stop.
    #[test]
    fn an_evenly_falling_list_has_no_cliff() {
        let scores: Vec<f32> = (1..=10).map(|r| 1.0 / (60.0 + r as f32)).collect();
        assert_eq!(cliff(&scores), None);
    }

    #[test]
    fn reranker_scores_fall_off_after_the_close_matches() {
        assert_eq!(cliff(&[0.95, 0.90, 0.30, 0.28, 0.10]), Some(2));
    }

    /// Two comparable drops: the list is ambiguous and says nothing, rather
    /// than picking one of them and calling the rest noise.
    #[test]
    fn an_ambiguous_list_says_nothing() {
        assert_eq!(cliff(&[0.9, 0.5, 0.1, 0.05]), None);
    }

    /// One gap has nothing to be compared against.
    #[test]
    fn fewer_than_three_hits_have_no_cliff() {
        assert_eq!(cliff(&[]), None);
        assert_eq!(cliff(&[1.0]), None);
        assert_eq!(cliff(&[1.0, 0.1]), None);
    }

    /// A plateau and then a drop is the clearest cliff there is; the mean of
    /// the other gaps being zero must not turn it into a division that says no.
    #[test]
    fn a_plateau_followed_by_a_drop_is_a_cliff() {
        assert_eq!(cliff(&[1.0, 1.0, 1.0, 0.2, 0.2]), Some(3));
        // And an entirely flat list has no fall at all.
        assert_eq!(cliff(&[0.5, 0.5, 0.5, 0.5]), None);
    }

    /// The same plateau, with a drop too small to mean anything: three hits a
    /// reranker scored alike and a fourth a rounding error below them. The mean
    /// of the other gaps is zero here too, so without a floor every positive
    /// number cleared three times zero and the page drew its line between two
    /// hits that were tied.
    #[test]
    fn a_tie_at_the_foot_of_a_plateau_is_not_a_cliff() {
        assert_eq!(cliff(&[1.0, 1.0, 1.0, 0.9999]), None);
        // The floor is a share of the top score rather than a fixed gap, so the
        // same shape in fused-rank space — where the scores, and every real gap
        // between them, are two orders of magnitude smaller — is read the same
        // way, and a genuine fall there is still a cliff.
        assert_eq!(cliff(&[0.016, 0.016, 0.016, 0.0159]), None);
        assert_eq!(cliff(&[0.016, 0.016, 0.016, 0.004]), Some(3));
    }

    /// Priming may lift a near-tie past its neighbour, which reads as a
    /// negative gap. That is a near-tie, not a fall.
    #[test]
    fn a_primed_inversion_is_not_a_cliff() {
        assert_eq!(cliff(&[0.90, 0.88, 0.89, 0.87, 0.30]), Some(4));
        assert_eq!(cliff(&[0.50, 0.48, 0.49, 0.47, 0.46]), None);
    }

    #[test]
    fn hits_past_the_cliff_are_marked_and_the_rest_are_not() {
        let dummy = |id: &str, score: f32| SearchResult {
            artifact_id: id.into(),
            corpus_id: "c".into(),
            title: None,
            text: String::new(),
            category: None,
            tags: vec![],
            score,
            status: None,
            superseded_by: None,
            last_verified_at: None,
            weak: false,
            primed: false,
            in_sitting: false,
            past_cliff: false,
            via: None,
            reason: None,
            model_written: false,
            synthesized: false,
            origin_count: 0,
        };
        let mut results = vec![
            dummy("a", 0.95),
            dummy("b", 0.90),
            dummy("c", 0.30),
            dummy("d", 0.28),
        ];
        mark_past_cliff(&mut results);
        assert_eq!(
            results.iter().map(|r| r.past_cliff).collect::<Vec<_>>(),
            vec![false, false, true, true]
        );
        // The list is neither reordered nor shortened.
        assert_eq!(
            results
                .iter()
                .map(|r| r.artifact_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b", "c", "d"]
        );

        let mut flat = vec![dummy("a", 0.5), dummy("b", 0.5), dummy("c", 0.5)];
        mark_past_cliff(&mut flat);
        assert!(flat.iter().all(|r| !r.past_cliff));
    }

    #[tokio::test]
    async fn a_synthesized_artifact_leading_the_list_marks_the_search_answered() {
        let mut core = test_core().await;
        core.feedback.enabled = true;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let captured = core
            .store
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    ordinal: 0,
                    text: "captured text".into(),
                    corpus_span: None,
                    title: Some("c".into()),
                    category: None,
                    tags: vec![],
                    segment_idx: None,
                    caveats: vec![],
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

        // The fake embedder is symmetric: the query that is the generated
        // artifact's rendered text lands it first.
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
        // And the rail knows what a model wrote.
        let hits = core
            .search(&q("g\ngenerated text"), Door::Ui)
            .await
            .unwrap();
        assert!(hits[0].synthesized && hits[0].model_written);
        assert_eq!(hits[0].origin_count, 1);
    }

    #[tokio::test]
    async fn an_interaction_is_recorded_only_with_pursuits_on() {
        let mut core = test_core().await;
        core.feedback.enabled = true;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let a = core
            .store
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    ordinal: 0,
                    text: "a".into(),
                    corpus_span: None,
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: None,
                    caveats: vec![],
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
                .is_empty()
        );
        core.pursuit.enabled = true;
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
