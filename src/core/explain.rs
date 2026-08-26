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
    /// Over its cap in one of its corpora, and present only because the
    /// refill had nothing else to offer. The case the cap silently fails in:
    /// a pool filled by one corpus leaves nothing to redistribute, so the
    /// displaced hits come straight back and the list is dominated despite a
    /// configured `per_source_cap`.
    Refilled,
}

/// Why one hit is where it is.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct HitExplanation {
    /// Rank as retrieval returned it — fusion *and* the scoring stage, since
    /// Qdrant applies both before anything comes back. Not the RRF rank on its
    /// own: that would need a second query, which the design forbids.
    pub retrieved_rank: usize,
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
    pub displaced: usize,
    /// How many of the displaced came straight back. Equal to `displaced`
    /// means the cap redistributed nothing at all.
    pub refilled: usize,
    pub reranked: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stage_that_did_not_apply_serialises_to_nothing() {
        let e = HitExplanation {
            retrieved_rank: 0,
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
}
