//! The trail step in search: a note is found through a query it answered before. The live
//! trails are held in memory; matching is one pass of dot products over them.

use super::Core;
use super::search::SearchResult;
use crate::error::Result;
use crate::store::artifacts::ArtifactStatus;
use crate::store::feedback::Door;
use crate::store::trails::{Tier, Trail};
use crate::vector::{SearchFilter, SearchHit, VectorPayload};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// A replay's trail made from its own query sits this near it.
pub const OWN: f32 = 0.999;

/// Hops followed through `superseded_by`, so a cycle in the data cannot hang a search.
const HOPS: usize = 8;

/// The live trails of one embed model.
pub struct TrailIndex {
    dim: usize,
    /// Normalised rows, `dim` floats each.
    rows: Vec<f32>,
    trails: Vec<Trail>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Matched {
    pub trail_id: String,
    pub artifact_id: String,
    pub query: String,
    pub similarity: f32,
    pub tier: Tier,
}

fn norm(v: &[f32]) -> f32 {
    v.iter().map(|x| x * x).sum::<f32>().sqrt()
}

impl TrailIndex {
    /// Trails of another width than the first are left out: no cosine is defined against them.
    pub fn new(trails: Vec<Trail>) -> Self {
        let dim = trails.first().map_or(0, |t| t.vec.len());
        let trails: Vec<Trail> = trails
            .into_iter()
            .filter(|t| t.vec.len() == dim && norm(&t.vec) > 0.0)
            .collect();
        let mut rows = Vec::with_capacity(trails.len() * dim);
        for t in &trails {
            let n = norm(&t.vec);
            rows.extend(t.vec.iter().map(|x| x / n));
        }
        Self { dim, rows, trails }
    }

    pub fn is_empty(&self) -> bool {
        self.trails.is_empty()
    }

    /// The best trail per note at or above `match_at`, best first. `leave_out_own` drops a trail
    /// within OWN (0.999) of `query_vec`; `with_model` false drops model trails.
    pub fn matches(
        &self,
        query_vec: &[f32],
        match_at: f32,
        leave_out_own: bool,
        with_model: bool,
    ) -> Vec<Matched> {
        let n = norm(query_vec);
        if query_vec.len() != self.dim || n == 0.0 {
            return Vec::new();
        }
        let mut out: Vec<Matched> = self
            .trails
            .iter()
            .zip(self.rows.chunks_exact(self.dim))
            .filter(|(t, _)| with_model || t.tier == Tier::Person)
            .map(|(t, row)| {
                let sim = row.iter().zip(query_vec).map(|(a, b)| a * b).sum::<f32>() / n;
                (t, sim)
            })
            .filter(|(_, sim)| *sim >= match_at && !(leave_out_own && *sim >= OWN))
            .map(|(t, sim)| Matched {
                trail_id: t.id.clone(),
                artifact_id: t.artifact_id.clone(),
                query: t.query.clone(),
                similarity: sim,
                tier: t.tier,
            })
            .collect();
        out.sort_by(|a, b| b.similarity.total_cmp(&a.similarity));
        let mut seen = HashSet::new();
        out.retain(|m| seen.insert(m.artifact_id.clone()));
        out
    }
}

fn tier_weight(t: Tier) -> f32 {
    match t {
        Tier::Person => 1.0,
        Tier::Model => 0.5,
    }
}

/// New order of `n` rows: row `pos` scores `1/(pos+2)`; a matched row at place `tpos` among the
/// matched scores the larger of that and `tier * weight / (tpos+2)`. Stable, so a row that ties
/// one ahead of it stays behind it. Returns the permutation.
pub fn reorder(n: usize, matched: &[(usize, usize, Tier)], weight: f32) -> Vec<usize> {
    let mut score: Vec<f32> = (0..n).map(|pos| 1.0 / (pos as f32 + 2.0)).collect();
    for &(pos, tpos, tier) in matched {
        if let Some(s) = score.get_mut(pos) {
            *s = s.max(tier_weight(tier) * weight / (tpos as f32 + 2.0));
        }
    }
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|a, b| score[*b].total_cmp(&score[*a]));
    order
}

/// Mirrors the vector stores' own filter, for a note loaded by id rather than retrieved.
fn passes(p: &VectorPayload, f: &SearchFilter) -> bool {
    let status = p.status.unwrap_or(ArtifactStatus::Active);
    status != ArtifactStatus::Trashed
        && (f.include_superseded || status != ArtifactStatus::Superseded)
        && (f.include_deprecated || status != ArtifactStatus::Deprecated)
        && f.tags.iter().all(|t| p.tags.contains(t))
        && f.category
            .as_ref()
            .is_none_or(|c| p.category.as_ref() == Some(c))
        && f.corpus_id.as_ref().is_none_or(|c| &p.corpus_id == c)
}

/// Reorders `results` by the matched notes in it and says so on each one.
pub(crate) fn apply(results: &mut Vec<SearchResult>, matched: &[Matched], weight: f32) {
    let rows: Vec<(usize, usize, Tier)> = results
        .iter()
        .enumerate()
        .filter_map(|(pos, r)| {
            matched
                .iter()
                .position(|m| m.artifact_id == r.artifact_id)
                .map(|tpos| (pos, tpos, matched[tpos].tier))
        })
        .collect();
    if rows.is_empty() {
        return;
    }
    let order = reorder(results.len(), &rows, weight);
    let mut old: Vec<Option<SearchResult>> =
        std::mem::take(results).into_iter().map(Some).collect();
    for (to, &from) in order.iter().enumerate() {
        let Some(mut r) = old[from].take() else {
            continue;
        };
        if let (Some(m), Some(e)) = (
            matched.iter().find(|m| m.artifact_id == r.artifact_id),
            r.explanation.as_mut(),
        ) {
            e.trail = Some(crate::core::explain::TrailEffect {
                query: m.query.clone(),
                similarity: m.similarity,
                moved: crate::core::explain::StageEffect {
                    from: Some(from),
                    to: Some(to),
                    delta: None,
                },
            });
        }
        results.push(r);
    }
}

impl Core {
    /// Loaded once, held on `Core` until a trail is made or retired.
    pub async fn trail_index(&self) -> Result<Arc<TrailIndex>> {
        if let Some(i) = self
            .trail_index
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            return Ok(i);
        }
        let i = Arc::new(TrailIndex::new(
            self.store.live_trails(self.embedder.model()).await?,
        ));
        *self
            .trail_index
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(i.clone());
        Ok(i)
    }

    /// After any mint, withdraw, retire or forget; the next search loads the trails again.
    pub fn drop_trail_index(&self) {
        *self
            .trail_index
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// `trails.match_at`, never under the line two unrelated queries sit at.
    pub fn trail_match_at(&self) -> f32 {
        crate::core::gaps::line_above(self.trails_cfg.match_at, self.measured_line())
    }

    /// Cached so a search never computes it; see `refresh_model_trails_count`.
    pub fn model_trails_count(&self) -> bool {
        self.model_trails.load(Ordering::Relaxed)
    }

    /// Called from the Decisions tick. An error keeps the answer in hand.
    pub async fn refresh_model_trails_count(&self) {
        match self.labels_count().await {
            Ok(c) => self.model_trails.store(c, Ordering::Relaxed),
            Err(e) => tracing::warn!(error = %e, "could not read whether model trails count"),
        }
    }

    /// The matched notes of this search, resolved forward and present in the pool once the
    /// returned hits join it. A failure costs the trail step, never the search.
    pub(crate) async fn trail_step(
        &self,
        vector: &[f32],
        weight: f32,
        door: Door,
        filter: &SearchFilter,
        pool: &[SearchHit],
    ) -> (Vec<Matched>, Vec<SearchHit>) {
        if weight == 0.0 || vector.is_empty() {
            return Default::default();
        }
        let index = match self.trail_index().await {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!(error = %e, "could not read the trails; searching without them");
                return Default::default();
            }
        };
        if index.is_empty() {
            return Default::default();
        }
        let found = index.matches(
            vector,
            self.trail_match_at(),
            door == Door::Judge,
            self.model_trails_count(),
        );
        if found.is_empty() {
            return Default::default();
        }
        match self.resolve(found, filter, pool).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "could not resolve the trails' notes; searching without them");
                Default::default()
            }
        }
    }

    async fn resolve(
        &self,
        found: Vec<Matched>,
        filter: &SearchFilter,
        pool: &[SearchHit],
    ) -> Result<(Vec<Matched>, Vec<SearchHit>)> {
        let mut ids: Vec<String> = found.iter().map(|m| m.artifact_id.clone()).collect();
        let mut known = self.vectors.payloads_of(&ids).await?;
        for _ in 0..HOPS {
            let mut moved = false;
            for id in ids.iter_mut() {
                if let Some(next) = known.get(id.as_str()).and_then(|p| p.superseded_by.clone()) {
                    *id = next;
                    moved = true;
                }
            }
            if !moved {
                break;
            }
            let need: Vec<String> = ids
                .iter()
                .filter(|i| !known.contains_key(i.as_str()))
                .cloned()
                .collect();
            if !need.is_empty() {
                known.extend(self.vectors.payloads_of(&need).await?);
            }
        }
        let in_pool: HashSet<&str> = pool
            .iter()
            .map(|h| h.payload.artifact_id.as_str())
            .collect();
        let lowest = pool.iter().map(|h| h.score).reduce(f32::min).unwrap_or(0.0);
        let mut seen = HashSet::new();
        let mut matched = Vec::new();
        let mut joining = Vec::new();
        // Still best first: a note reached by two trails keeps the better.
        for (m, id) in found.into_iter().zip(ids) {
            if !seen.insert(id.clone()) {
                continue;
            }
            if !in_pool.contains(id.as_str()) {
                match known.get(&id) {
                    Some(p) if passes(p, filter) => joining.push(SearchHit {
                        payload: p.clone(),
                        score: lowest,
                        similarity: None,
                    }),
                    _ => continue,
                }
            }
            matched.push(Matched {
                artifact_id: id,
                ..m
            });
        }
        Ok((matched, joining))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place_of(row: usize, order: &[usize]) -> usize {
        order.iter().position(|&r| r == row).unwrap()
    }

    #[test]
    fn a_person_trail_lands_near_third_and_a_model_trail_lower() {
        // 10 rows; the matched row is last (pos 9), the only match (tpos 0), weight 0.5.
        assert_eq!(place_of(9, &reorder(10, &[(9, 0, Tier::Person)], 0.5)), 3);
        assert_eq!(place_of(9, &reorder(10, &[(9, 0, Tier::Model)], 0.5)), 7);
    }

    #[test]
    fn a_first_row_without_a_trail_is_never_passed() {
        let order = reorder(10, &[(9, 0, Tier::Person)], 1.0);
        assert_eq!(order[0], 0);
        assert_eq!(place_of(9, &order), 1);
    }

    #[test]
    fn weight_zero_changes_nothing() {
        assert_eq!(reorder(4, &[(3, 0, Tier::Person)], 0.0), [0, 1, 2, 3]);
    }

    #[test]
    fn a_matched_row_never_moves_down() {
        assert_eq!(place_of(1, &reorder(4, &[(1, 3, Tier::Model)], 0.5)), 1);
    }

    fn trail(vec: Vec<f32>) -> Trail {
        Trail {
            id: "t".into(),
            artifact_id: "a".into(),
            query: "how do I flumph".into(),
            vec,
            tier: Tier::Person,
            matched: 0,
            missed: 0,
        }
    }

    #[test]
    fn a_replay_leaves_out_the_trail_made_from_its_own_query() {
        let own_vec = vec![0.6, 0.8, 0.0];
        let idx = TrailIndex::new(vec![trail(own_vec.clone())]);
        assert!(idx.matches(&own_vec, 0.8, true, true).is_empty());
        assert_eq!(idx.matches(&own_vec, 0.8, false, true).len(), 1);
    }

    #[test]
    fn a_vector_of_another_width_matches_nothing() {
        let idx = TrailIndex::new(vec![trail(vec![1.0, 0.0, 0.0])]);
        assert!(idx.matches(&[1.0, 0.0], 0.0, false, true).is_empty());
    }
}
