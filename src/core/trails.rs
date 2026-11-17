//! The trail step in search: a note is found through a query it answered before. The live
//! trails are held in memory; matching is one pass of dot products over them. Below, where trails
//! come from and what a search's outcome says to the ones it showed.

use super::Core;
use super::search::SearchResult;
use crate::core::judging::catalogue::answers_query;
use crate::error::Result;
use crate::store::artifacts::ArtifactStatus;
use crate::store::feedback::{Door, blob_to_vec};
use crate::store::trails::{NewTrail, Tier, Trail, TrailSource};
use crate::vector::{SearchFilter, SearchHit, VectorPayload};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// A replay's trail made from its own query sits this near it.
pub const OWN: f32 = 0.999;

/// Hops followed through `superseded_by`, so a cycle in the data cannot hang a search.
const HOPS: usize = 8;

/// Trails under an old embed model embedded again per quiet Decisions tick.
pub const STALE_PER_PASS: i64 = 50;

/// A trail write rides on a verdict, an open or a label already stored; its failure costs the
/// trail, never that.
pub(crate) fn shrug(what: &'static str, r: Result<()>) {
    if let Err(e) = r {
        tracing::warn!(error = %e, step = what, "could not update the trails");
    }
}

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
        let generation = self.trail_gen.load(Ordering::SeqCst);
        let i = Arc::new(TrailIndex::new(
            self.store.live_trails(self.embedder.model()).await?,
        ));
        self.keep_trail_index(generation, i.clone());
        Ok(i)
    }

    /// Kept only if no drop came between the read of `generation` and now: that load may predate
    /// the change.
    fn keep_trail_index(&self, generation: u64, i: Arc<TrailIndex>) {
        let mut held = self
            .trail_index
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.trail_gen.load(Ordering::SeqCst) == generation {
            *held = Some(i);
        }
    }

    /// After any mint, withdraw, retire or forget; the next search loads the trails again.
    pub fn drop_trail_index(&self) {
        let mut held = self
            .trail_index
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.trail_gen.fetch_add(1, Ordering::SeqCst);
        *held = None;
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

    /// No-op unless `learn.enabled`, the vector is under the live embed model and non-empty.
    pub async fn mint_trail(
        &self,
        artifact_id: &str,
        query: &str,
        query_vec: &[f32],
        embed_model: &str,
        source: TrailSource,
        source_id: &str,
    ) -> Result<()> {
        if !self.learn.enabled || query_vec.is_empty() || embed_model != self.embedder.model() {
            return Ok(());
        }
        let t = NewTrail {
            artifact_id,
            query,
            query_vec,
            embed_model,
            source,
            source_id,
        };
        self.store
            .mint_trail(&t, self.trails_cfg.per_note, self.trails_cfg.max)
            .await?;
        self.drop_trail_index();
        Ok(())
    }

    /// One trail per note, from the event's own words and vector.
    pub async fn mint_trails_from(
        &self,
        source: TrailSource,
        source_id: &str,
        artifact_ids: &[String],
    ) -> Result<()> {
        if !self.learn.enabled || artifact_ids.is_empty() {
            return Ok(());
        }
        let Some(p) = self.store.trail_pairing(source, source_id).await? else {
            return Ok(());
        };
        let vec = blob_to_vec(&p.vec);
        for id in artifact_ids {
            self.mint_trail(id, &p.query, &vec, &p.embed_model, source, source_id)
                .await?;
        }
        Ok(())
    }

    /// An undone verdict takes its pairing back.
    pub async fn withdraw_trails(&self, source: TrailSource, source_id: &str) -> Result<()> {
        if self.store.withdraw_trail_source(source, source_id).await? > 0 {
            self.drop_trail_index();
        }
        Ok(())
    }

    /// Use confirms the trail that brought or lifted the opened note.
    pub async fn trails_after_open(&self, event_id: &str, artifact_id: &str) -> Result<()> {
        for (a, trail, _, _) in self.store.event_trails(event_id).await? {
            if a == artifact_id {
                self.store.trail_confirmed(&trail).await?;
            }
        }
        Ok(())
    }

    /// A hit confirms as an open does, unless the search's open already did; the pairing is a
    /// person's from here either way.
    pub async fn trails_after_hit(&self, event_id: &str, artifact_id: &str) -> Result<()> {
        if !self.store.event_opened(event_id).await? {
            self.trails_after_open(event_id, artifact_id).await?;
        }
        self.mint_trails_from(
            TrailSource::Verdict,
            event_id,
            std::slice::from_ref(&artifact_id.to_string()),
        )
        .await
    }

    /// "Nothing here" counts against every trail the search showed, a person's too.
    pub async fn trails_after_gap(&self, event_id: &str) -> Result<()> {
        let mut retired = false;
        for (_, trail, shown, _) in self.store.event_trails(event_id).await? {
            if shown {
                retired |= self.store.trail_missed(&trail).await?;
            }
        }
        if retired {
            self.drop_trail_index();
        }
        Ok(())
    }

    /// The model may withdraw its own trails, never confirm one or touch a person's: a model trail
    /// on a shown candidate labelled under `ask_at` is missed. Then the labeller's find, if any,
    /// leaves a model trail; `model_expected` leaves out what a model trail brought.
    pub async fn trails_after_labels(
        &self,
        event_id: &str,
        labels: &[(String, f64)],
    ) -> Result<()> {
        let asked = answers_query("");
        let t = self.thresholds(&asked).await?;
        let mut retired = false;
        for (a, trail, shown, tier) in self.store.event_trails(event_id).await? {
            let unhelpful = labels.iter().any(|(id, yes)| *id == a && *yes < t.ask_at);
            if shown && tier == Tier::Model && unhelpful {
                retired |= self.store.trail_missed(&trail).await?;
            }
        }
        if retired {
            self.drop_trail_index();
        }
        let found = self
            .store
            .model_expected(event_id, t.act_at, asked.version, self.decider.model())
            .await?;
        match found {
            Some(found) => {
                self.mint_trails_from(TrailSource::Label, event_id, &[found])
                    .await
            }
            None => Ok(()),
        }
    }

    /// While the base is quiet: trails under an old embed model, embedded again under the live
    /// one, a bounded number a pass. Returns how many.
    pub async fn reembed_stale_trails(&self) -> Result<usize> {
        let model = self.embedder.model().to_string();
        let mut done = 0;
        let mut failed = None;
        for (id, query) in self.store.stale_trails(&model, STALE_PER_PASS).await? {
            let permit = self.gate.background_light().await;
            let got = self.embedder.embed_query(&query).await;
            permit.finished();
            match got {
                Ok(v) => match self.store.reembed_trail(&id, &v, &model).await {
                    Ok(()) => done += 1,
                    Err(e) => failed = Some(e),
                },
                Err(e) => failed = Some(e),
            }
            if failed.is_some() {
                break;
            }
        }
        if done > 0 {
            self.drop_trail_index();
        }
        failed.map_or(Ok(done), Err)
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

    #[tokio::test]
    async fn a_load_a_drop_overtook_is_not_kept() {
        let core = crate::core::test_support::test_core().await;
        let generation = core.trail_gen.load(Ordering::SeqCst);
        core.drop_trail_index();
        core.keep_trail_index(generation, Arc::new(TrailIndex::new(Vec::new())));
        assert!(core.trail_index.read().unwrap().is_none());
        core.keep_trail_index(generation + 1, Arc::new(TrailIndex::new(Vec::new())));
        assert!(core.trail_index.read().unwrap().is_some());
    }

    #[test]
    fn a_vector_of_another_width_matches_nothing() {
        let idx = TrailIndex::new(vec![trail(vec![1.0, 0.0, 0.0])]);
        assert!(idx.matches(&[1.0, 0.0], 0.0, false, true).is_empty());
    }

    use crate::core::test_support::{seed_artifact, test_core};
    use crate::store::feedback::{Labeller, NewCandidate, NewEvent};

    async fn learning() -> Core {
        let mut core = test_core().await;
        core.learn.enabled = true;
        core
    }

    /// A search under the live embed model showing each note, with the trail that brought it.
    async fn searched(core: &Core, query: &str, shown: &[(&str, Option<&str>)]) -> String {
        let candidates = shown
            .iter()
            .enumerate()
            .map(|(i, (a, t))| NewCandidate {
                artifact_id: a.to_string(),
                score: 1.0 - i as f32 / 10.0,
                similarity: Some(0.5),
                shown: true,
                trail_id: t.map(str::to_string),
                ..Default::default()
            })
            .collect();
        let ev = NewEvent {
            query: query.into(),
            door: Door::Ui,
            scope: None,
            filters: "{}".into(),
            query_vec: core.embedder.embed_query(query).await.unwrap(),
            embed_model: core.embedder.model().into(),
            candidates,
            answered: false,
            fold_onto: None,
            context: None,
        };
        core.store.record_search(ev, 0).await.unwrap()
    }

    async fn trail_on(core: &Core, note: &str, query: &str, source: TrailSource) -> String {
        let vec = core.embedder.embed_query(query).await.unwrap();
        let model = core.embedder.model();
        core.mint_trail(note, query, &vec, model, source, "e0")
            .await
            .unwrap();
        live(core).await.last().unwrap().id.clone()
    }

    async fn live(core: &Core) -> Vec<Trail> {
        core.store.live_trails(core.embedder.model()).await.unwrap()
    }

    async fn labelled(core: &Core, event: &str, rows: &[(String, f64)]) {
        let (v, m) = (answers_query("").version, core.decider.model());
        core.store
            .put_search_labels(event, rows, v, m)
            .await
            .unwrap();
        core.trails_after_labels(event, rows).await.unwrap();
    }

    #[tokio::test]
    async fn a_search_judged_a_hit_leaves_a_person_trail_and_undo_retires_it() {
        let core = learning().await;
        let n = seed_artifact(&core, "mount").await;
        let ev = searched(&core, "mount the image", &[(&n, None)]).await;
        core.store
            .judge_hit(&ev, &n, Labeller::Confirm)
            .await
            .unwrap();
        core.trails_after_hit(&ev, &n).await.unwrap();
        let t = live(&core).await;
        assert_eq!((t.len(), t[0].tier), (1, Tier::Person));
        assert_eq!(t[0].artifact_id, n);

        core.store.unjudge(&ev, Labeller::Confirm).await.unwrap();
        core.withdraw_trails(TrailSource::Verdict, &ev)
            .await
            .unwrap();
        assert!(live(&core).await.is_empty());
    }

    #[tokio::test]
    async fn the_labellers_find_in_an_unjudged_search_is_a_model_trail() {
        let core = learning().await;
        let n = seed_artifact(&core, "mount").await;
        let ev = searched(&core, "mount the image", &[(&n, None)]).await;
        labelled(&core, &ev, &[(n.clone(), 1.0)]).await;
        let t = live(&core).await;
        assert_eq!((t.len(), t[0].tier), (1, Tier::Model));
    }

    #[tokio::test]
    async fn a_candidate_a_model_trail_brought_makes_no_trail_and_no_sweep_pair_when_labelled_relevant()
     {
        let core = learning().await;
        let generation = crate::store::generations::ensure_generation(
            &core.store,
            Default::default(),
            "recipe",
            "chat",
        )
        .await
        .unwrap()
        .id;
        let (brought, plain) = (
            seed_artifact(&core, "a").await,
            seed_artifact(&core, "b").await,
        );
        let original = trail_on(&core, &brought, "older words", TrailSource::Label).await;
        let ev = searched(&core, "mount the image", &[(&brought, Some(&original))]).await;
        labelled(&core, &ev, &[(brought.clone(), 1.0)]).await;
        // The same, with no trail behind the candidate: it is the sweep's pair.
        let other = searched(&core, "loop device", &[(&plain, None)]).await;
        labelled(&core, &other, &[(plain.clone(), 1.0)]).await;

        let on_brought: Vec<String> = live(&core)
            .await
            .into_iter()
            .filter(|t| t.artifact_id == brought)
            .map(|t| t.id)
            .collect();
        assert_eq!(on_brought, [original]);
        let (at, v) = (
            crate::jobs::label::relevant_at(&core).await.unwrap(),
            answers_query("").version,
        );
        let pairs = core
            .store
            .model_judged_pairs(&generation, at, v, core.decider.model(), 10)
            .await
            .unwrap();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].event_id, other);
    }

    #[tokio::test]
    async fn a_model_trail_labelled_unhelpful_three_times_is_retired_and_a_person_trail_is_not() {
        let core = learning().await;
        let (m, p) = (
            seed_artifact(&core, "a").await,
            seed_artifact(&core, "b").await,
        );
        let model = trail_on(&core, &m, "older words", TrailSource::Label).await;
        let person = trail_on(&core, &p, "other words", TrailSource::Verdict).await;
        for i in 0..3 {
            let q = format!("search {i}");
            let ev = searched(&core, &q, &[(&m, Some(&model)), (&p, Some(&person))]).await;
            labelled(&core, &ev, &[(m.clone(), 0.0), (p.clone(), 0.0)]).await;
        }
        let ids: Vec<String> = live(&core).await.into_iter().map(|t| t.id).collect();
        assert_eq!(ids, [person]);
    }

    #[tokio::test]
    async fn opening_the_note_a_trail_brought_confirms_the_trail() {
        let core = learning().await;
        let n = seed_artifact(&core, "mount").await;
        let t = trail_on(&core, &n, "older words", TrailSource::Label).await;
        let ev = searched(&core, "mount the image", &[(&n, Some(&t))]).await;
        assert!(core.store.open_event(&ev, &n).await.unwrap());
        core.trails_after_open(&ev, &n).await.unwrap();
        assert_eq!(live(&core).await[0].matched, 1);
    }

    #[tokio::test]
    async fn a_search_confirms_a_trail_once_however_often_its_note_is_opened() {
        let core = learning().await;
        let n = seed_artifact(&core, "mount").await;
        let t = trail_on(&core, &n, "older words", TrailSource::Label).await;
        let ev = searched(&core, "mount the image", &[(&n, Some(&t))]).await;
        // As the open routes do it.
        for _ in 0..2 {
            let (stamped, first) = core.store.open_event_first(&ev, &n).await.unwrap();
            assert!(stamped);
            if first {
                core.trails_after_open(&ev, &n).await.unwrap();
            }
        }
        assert_eq!(live(&core).await[0].matched, 1);
        core.store
            .judge_hit(&ev, &n, Labeller::Confirm)
            .await
            .unwrap();
        core.trails_after_hit(&ev, &n).await.unwrap();
        assert_eq!(live(&core).await[0].matched, 1);
    }

    #[tokio::test]
    async fn nothing_here_counts_against_every_trail_shown() {
        let core = learning().await;
        let (a, b) = (
            seed_artifact(&core, "a").await,
            seed_artifact(&core, "b").await,
        );
        let ta = trail_on(&core, &a, "older words", TrailSource::Label).await;
        let tb = trail_on(&core, &b, "other words", TrailSource::Verdict).await;
        let ev = searched(
            &core,
            "mount the image",
            &[(&a, Some(&ta)), (&b, Some(&tb))],
        )
        .await;
        assert!(core.store.gap_event(&ev, "mount the image").await.unwrap());
        core.trails_after_gap(&ev).await.unwrap();
        assert!(live(&core).await.iter().all(|t| t.missed == 1));
    }

    #[tokio::test]
    async fn deleting_captured_searches_deletes_every_trail() {
        let core = learning().await;
        let n = seed_artifact(&core, "mount").await;
        trail_on(&core, &n, "older words", TrailSource::Verdict).await;
        core.forget_situations().await;
        let left = sqlx::query_scalar!("SELECT COUNT(*) FROM trails")
            .fetch_one(&core.store.pool)
            .await
            .unwrap();
        assert_eq!(left, 0);
    }

    #[tokio::test]
    async fn with_learning_off_no_trail_is_made() {
        let mut core = learning().await;
        core.learn.enabled = false;
        let n = seed_artifact(&core, "mount").await;
        let ev = searched(&core, "mount the image", &[(&n, None)]).await;
        core.trails_after_hit(&ev, &n).await.unwrap();
        assert!(live(&core).await.is_empty());
    }

    #[tokio::test]
    async fn a_trail_under_the_old_embed_model_is_embedded_again() {
        let core = learning().await;
        let n = seed_artifact(&core, "mount").await;
        let t = crate::store::trails::NewTrail {
            artifact_id: &n,
            query: "older words",
            query_vec: &[1.0, 0.0],
            embed_model: "old",
            source: TrailSource::Verdict,
            source_id: "e0",
        };
        core.store.mint_trail(&t, 8, 20_000).await.unwrap();
        assert_eq!(core.reembed_stale_trails().await.unwrap(), 1);
        assert_eq!(live(&core).await.len(), 1);
    }
}
