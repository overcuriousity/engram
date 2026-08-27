//! Why a hit is where it is.
//!
//! A rank is the product of eight stages (see the design record,
//! `docs/superpowers/specs/2026-08-26-ranking-explanation-design.md`, §3).
//! Each used to say what it did in its own way or not at all. This is the one
//! object all three doors read, so that the rail, MCP's meta line and the API
//! cannot disagree about what happened to a result.
//!
//! Nothing here is stored and nothing here reorders anything.

/// What one stage did to one hit. `None` everywhere the stage did not apply.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct StageEffect {
    /// Rank before the stage, where the stage reorders.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<usize>,
    /// Rank after it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<usize>,
    /// Score contribution, where the stage is additive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<f32>,
}

/// What the per-source diversity rule did to this hit.
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapEffect {
    /// No cap configured, or this hit never went through one.
    #[default]
    NotApplied,
    /// Took a place within its corpus's allowance.
    Kept,
    /// Over its cap in one of its corpora, and in the answer anyway because
    /// the refill had nothing else to offer. The case the cap silently fails
    /// in: a pool filled by one corpus leaves nothing to redistribute, so the
    /// displaced hits come straight back and the list is dominated despite a
    /// configured `per_source_cap`.
    ///
    /// Set on every displaced hit as the pool is built, which is honest only
    /// because the truncate at the end of the search cuts the ones the cap did
    /// hold out: a hit still carrying this when a door renders it is one that
    /// reached the caller over its cap.
    Refilled,
}

/// Why one hit is where it is.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct HitExplanation {
    /// Rank as retrieval returned it — fusion *and* the scoring stage, since
    /// Qdrant applies both before anything comes back. Not the RRF rank on its
    /// own: that would need a second query, which the design forbids.
    ///
    /// Read before the cap runs, because the cap reorders: enumerating the
    /// capped list would report where the cap put a hit and call it where
    /// retrieval did.
    ///
    /// `None` for a hit retrieval never returned — an associated one. A zero
    /// here would read as rank #1, which is the one thing a recalled hit must
    /// not claim.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retrieved_rank: Option<usize>,
    /// The recency term's contribution to the score, reconstructed locally.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recency: Option<f32>,
    /// The pinned term's, likewise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rerank: Option<StageEffect>,
    pub cap: CapEffect,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prime: Option<StageEffect>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub past_cliff: bool,
    /// Set for a hit the association stage appended. Every other field is then
    /// absent: it never competed for a place, so there is no ranking story to
    /// tell.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recalled_via: Option<String>,
}

impl HitExplanation {
    /// The explanation of a hit association appended, which is that it was
    /// recalled and nothing more.
    pub fn recalled(via: &str) -> Self {
        Self {
            recalled_via: Some(via.to_string()),
            ..Default::default()
        }
    }
}

/// What cannot belong to a hit: the shape of the pool it was drawn from.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct SearchExplanation {
    /// How wide the fetch was — `limit * CANDIDATE_MULTIPLIER`, or wider when
    /// capture asked for a bigger pool.
    pub candidates_fetched: usize,
    /// Distinct corpora in the pool before the cap ran. One here, with a
    /// `per_source_cap` configured, is the saturation the design is about.
    pub corpora_in_pool: usize,
    /// The cap in force, `None` when uncapped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capped: Option<usize>,
    /// How many hits the cap found over their source's allowance, across the
    /// whole candidate pool.
    pub displaced: usize,
    /// How many of those are in the answer regardless — counted after the
    /// truncate, over the list the caller will see.
    ///
    /// Not counted over the pool, where the refill always takes every
    /// displaced hit back and this could only ever equal `displaced`. Any
    /// number above zero is the cap failing to redistribute: it wanted these
    /// out and had nothing to put in their place.
    pub refilled: usize,
    pub reranked: bool,
}

/// The two score terms Qdrant applied, reconstructed from the payload.
///
/// The recency decay and the pinned boost run inside Qdrant as one sum and
/// only the final score comes back (`src/vector/qdrant.rs`, `scoring_formula`).
/// Both terms are nonetheless computable here from fields the payload already
/// carries, so a full explanation costs no second query.
///
/// `exp_decay` with `midpoint: 0.5` and `scale: s` is `0.5^(|x - target| / s)`
/// — a half-life curve whose half-life is `scale`. A weight of zero omits its
/// term from the formula entirely, so this returns `None` rather than `0.0`: a
/// rendered zero would claim a stage ran and contributed nothing, which is a
/// different statement from the stage not being configured.
///
/// This re-implements another system's semantics, which is the one real risk
/// in the design. It is pinned against real Qdrant in
/// `tests/integration_qdrant.rs`, not against our own belief about the
/// formula.
pub fn scoring_terms(
    payload: &crate::vector::VectorPayload,
    now: i64,
    recency_weight: f32,
    half_life_secs: u64,
    pinned_boost: f32,
    pinned_tag: &str,
) -> (Option<f32>, Option<f32>) {
    let recency = (recency_weight > 0.0).then(|| {
        // Absent means `now`, exactly as the formula's `"defaults"` says.
        let stamp = payload.last_verified_at.unwrap_or(now);
        // Absolute, as `exp_decay`'s `|x - target|` is: a stamp in the future
        // — clock skew between a writer and the query host, or an imported
        // one — decays in Qdrant, and clamping to zero here would report the
        // undecayed weight for a term the store had already cut.
        let age = (now - stamp).abs() as f64;
        let decay = 0.5f64.powf(age / half_life_secs.max(1) as f64);
        recency_weight * decay as f32
    });
    let pinned = (pinned_boost > 0.0 && payload.tags.iter().any(|t| t == pinned_tag))
        .then_some(pinned_boost);
    (recency, pinned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(last_verified_at: Option<i64>, tags: &[&str]) -> crate::vector::VectorPayload {
        crate::vector::VectorPayload {
            artifact_id: "a".into(),
            corpus_id: "c".into(),
            text: String::new(),
            title: None,
            category: None,
            tags: tags.iter().map(|t| t.to_string()).collect(),
            created_at: 0,
            last_seen_at: None,
            hit_count: None,
            status: None,
            last_verified_at,
            superseded_by: None,
            origin_corpora: vec![],
            provenance: None,
        }
    }

    #[test]
    fn one_half_life_of_age_halves_the_recency_term() {
        let (recency, pinned) = scoring_terms(
            &payload(Some(9_000), &[]),
            10_000,
            0.05,
            1_000,
            0.15,
            "pinned",
        );
        let recency = recency.expect("a weighted recency term is present");
        assert!(
            (recency - 0.025).abs() < 1e-6,
            "one half-life old halves the decay: 0.05 * 0.5 = 0.025, got {recency}"
        );
        assert!(pinned.is_none(), "an untagged point earns no pinned term");
    }

    #[test]
    fn a_point_with_no_verification_stamp_decays_not_at_all() {
        let (recency, _) = scoring_terms(&payload(None, &[]), 10_000, 0.05, 1_000, 0.15, "pinned");
        assert_eq!(
            recency,
            Some(0.05),
            "the formula's own default is `now`, which is a decay of 1.0 — \
             reading the absence as maximum age would rank the opposite way"
        );
    }

    #[test]
    fn a_pinned_point_earns_the_whole_boost() {
        let (_, pinned) = scoring_terms(
            &payload(None, &["pinned"]),
            10_000,
            0.0,
            1_000,
            0.15,
            "pinned",
        );
        assert_eq!(pinned, Some(0.15));
    }

    #[test]
    fn a_disabled_term_is_absent_rather_than_zero() {
        let (recency, pinned) = scoring_terms(
            &payload(None, &["pinned"]),
            10_000,
            0.0,
            1_000,
            0.0,
            "pinned",
        );
        assert!(
            recency.is_none() && pinned.is_none(),
            "`scoring_formula` omits a term at weight zero, so the explanation \
             must not claim a stage that never entered the sum"
        );
    }

    #[test]
    fn a_stage_that_did_not_apply_serialises_to_nothing() {
        let e = HitExplanation {
            retrieved_rank: Some(0),
            ..Default::default()
        };
        let json = serde_json::to_string(&e).unwrap();
        assert_eq!(
            json, r#"{"retrieved_rank":0,"cap":"not_applied"}"#,
            "an absent stage must be absent, not null: a door that renders \
             every key would claim a stage ran"
        );
    }

    #[test]
    fn a_recalled_hit_carries_only_that() {
        let e = HitExplanation::recalled("a1");
        assert_eq!(e.recalled_via.as_deref(), Some("a1"));
        assert!(e.rerank.is_none() && e.prime.is_none());
        assert!(
            matches!(e.cap, CapEffect::NotApplied),
            "an associated hit never competed, so no stage may claim it acted"
        );
    }

    #[test]
    fn a_recalled_hit_claims_no_retrieved_rank() {
        let json = serde_json::to_string(&HitExplanation::recalled("a1")).unwrap();
        assert_eq!(
            json, r#"{"cap":"not_applied","recalled_via":"a1"}"#,
            "a rendered `retrieved_rank` of zero reads as #1 to every caller \
             that does not also read `recalled_via` — the rank has to be \
             absent, not zero"
        );
    }

    #[test]
    fn a_stamp_in_the_future_decays_the_same_as_one_that_far_past() {
        let (ahead, _) = scoring_terms(&payload(Some(11_000), &[]), 10_000, 0.05, 1_000, 0.0, "p");
        let (behind, _) = scoring_terms(&payload(Some(9_000), &[]), 10_000, 0.05, 1_000, 0.0, "p");
        assert_eq!(
            ahead, behind,
            "`exp_decay` measures `|x - target|`, so a stamp a half-life ahead \
             of now is decayed exactly as one a half-life behind it; reporting \
             the full weight would contradict the score Qdrant returned"
        );
    }
}
