//! Why a hit is where it is: the one object the rail, MCP and the API all read.
//! Nothing here is stored and nothing here reorders anything.

/// What one stage did to one hit. `None` everywhere the stage did not apply.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StageEffect {
    /// Rank before the stage, where the stage reorders.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<usize>,
    /// Score contribution, where the stage is additive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<f32>,
}

/// What the per-source diversity rule did to this hit.
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapEffect {
    /// No cap configured, or this hit never went through one.
    #[default]
    NotApplied,
    Kept,
    /// Over its cap but in the answer because the refill had nothing else to offer.
    /// Set on every displaced hit; the final truncate removes the ones the cap did hold out.
    Refilled,
}

/// Why one hit is where it is.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HitExplanation {
    /// Rank as Qdrant returned it (fusion and scoring), read before the cap reorders.
    /// `None` for an associated hit, which must not read as rank #1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retrieved_rank: Option<usize>,
    /// The recency term's contribution to the score, reconstructed locally.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recency: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rerank: Option<StageEffect>,
    pub cap: CapEffect,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prime: Option<StageEffect>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub past_cliff: bool,
    /// Found by its words in SQLite, not by the index; see `Store::keyword_matches`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub keyword: bool,
    /// The one row an exploring search lent the window; see `Core::explore_swap`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explored: Option<StageEffect>,
    /// Set for a hit the association stage appended; every other field is then absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recalled_via: Option<String>,
}

impl HitExplanation {
    pub fn recalled(via: &str) -> Self {
        Self {
            recalled_via: Some(via.to_string()),
            ..Default::default()
        }
    }
}

/// What cannot belong to a hit: the shape of the pool it was drawn from.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SearchExplanation {
    /// `limit * CANDIDATE_MULTIPLIER`, or wider when capture asked for a bigger pool.
    pub candidates_fetched: usize,
    /// Before the cap ran; one here with a `per_source_cap` configured is saturation.
    pub corpora_in_pool: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capped: Option<usize>,
    /// Hits over their source's allowance across the whole candidate pool.
    pub displaced: usize,
    /// Displaced hits in the answer regardless, counted after the truncate; above zero the cap
    /// failed.
    pub refilled: usize,
    pub reranked: bool,
    /// The query could not be embedded and the answer is keyword matches only.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub keyword_only: bool,
}

/// The recency and pinned terms Qdrant summed, reconstructed from the payload without a second
/// query. `None` for a zero weight, which Qdrant omits; pinned against real Qdrant in
/// `tests/integration_qdrant.rs`.
pub fn scoring_terms(
    payload: &crate::vector::VectorPayload,
    now: i64,
    recency_weight: f32,
    half_life_secs: u64,
    pinned_boost: f32,
    pinned_tag: &str,
) -> (Option<f32>, Option<f32>) {
    let recency = (recency_weight > 0.0).then(|| {
        // Absent means `now`, as the formula's `"defaults"` says.
        let stamp = payload.last_verified_at.unwrap_or(now);
        // Absolute, as `exp_decay`'s `|x - target|` is: a future stamp from clock skew still decays
        // in Qdrant.
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
            tags: tags.iter().map(|t| t.to_string()).collect(),
            last_verified_at,
            ..Default::default()
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
