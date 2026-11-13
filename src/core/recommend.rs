//! What to offer under the search box, and why. Qdrant's `max_sim` does not say which element won,
//! so the winning cluster is re-derived locally; a test pins that both pick the same artifact.

use crate::core::Core;
use crate::core::context::{Bundle, context_score, contributions, encode};
use crate::error::Result;

/// Each candidate's clusters are rescored locally, which is free next to the round trip.
pub const CANDIDATES: usize = 10;

/// Enough to say what decided it, short enough to stay one line.
pub const NAMED_BLOCKS: usize = 3;

/// Which rung of the ladder the offer rests on: match quality and how often the situation occurred.
/// `Random` is the floor and claims nothing, so nothing is printed beside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rung {
    Pattern,
    Similar,
    /// A real situation seen once or twice, held to a higher match bar than `Similar`.
    Tentative,
    Random,
}

impl Rung {
    /// Fixed buckets for the recorded row and the Ops breakdown.
    pub fn as_str(&self) -> &'static str {
        match self {
            Rung::Pattern => "pattern",
            Rung::Similar => "similar",
            Rung::Tentative => "tentative",
            Rung::Random => "random",
        }
    }

    /// The offer link's marker is viewer-typed and reaches `offer_rates`' `GROUP BY rung`; only
    /// `as_str` words parse.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pattern" => Some(Rung::Pattern),
            "similar" => Some(Rung::Similar),
            "tentative" => Some(Rung::Tentative),
            "random" => Some(Rung::Random),
            _ => None,
        }
    }

    pub fn is_explained(&self) -> bool {
        !matches!(self, Rung::Random)
    }
}

/// What is offered under the search box, and everything the page needs to say why.
#[derive(Debug, Clone)]
pub struct Offer {
    pub artifact_id: String,
    pub title: String,
    pub rung: Rung,
    /// `None` on the random card.
    pub slot: Option<i64>,
    /// Zero on the random card.
    pub events: i64,
    /// Largest contribution first, at most `NAMED_BLOCKS`; empty on the random card.
    pub blocks: Vec<&'static str>,
    pub at: Option<i64>,
    /// The event's own zone; the current bundle's would misdate every situation recorded on a trip.
    pub at_tz: Option<String>,
    /// The raw bundle and the contribution numbers, JSON, for the `<details>`.
    pub detail: String,
}

impl Core {
    /// One vector query and no embedding.
    pub async fn offer(&self, scope: Option<&str>, bundle: &Bundle) -> Result<Option<Offer>> {
        if !self.recommends() {
            return Ok(None);
        }
        let now_at = self.clock.now();
        let now = encode(now_at, bundle, &self.recommend.weights);

        // `must_not`, as in search, so a hand-written point with no `status` key is still offered.
        let hits = self
            .vectors
            .context_query(&now, CANDIDATES, &Default::default())
            .await
            .unwrap_or_else(|e| {
                // A vector store that cannot answer must not take the search page down.
                tracing::warn!(error = %e, "context query unavailable; falling through the ladder");
                Vec::new()
            });

        let ids: Vec<String> = hits.iter().map(|h| h.payload.artifact_id.clone()).collect();
        let clusters = self.store.context_clusters_of(&ids).await?;

        // Ranked by `context_score`, reproducing `max_sim`'s choice; every candidate is tried in
        // order because the rung also turns on `firm_at`, so the closest may fail where the second
        // passes.
        let mut ranked: Vec<(
            &crate::vector::SearchHit,
            &crate::store::context::StoredCluster,
            f32,
        )> = Vec::new();
        for hit in &hits {
            let Some(mine) = clusters.get(&hit.payload.artifact_id) else {
                continue;
            };
            for c in mine {
                // The only isolation cut, and it must be exact: a Qdrant payload filter acts on the
                // point, not on elements of the shared `ctx` multivector.
                if c.scope.as_deref() != scope {
                    continue;
                }
                // A cluster from another encoder layout is skipped rather than explained with the
                // wrong blocks.
                if c.encoder_version
                    != crate::core::context::encoder_version(&self.recommend.weights)
                    || c.centroid.len() != now.len()
                {
                    continue;
                }
                ranked.push((hit, c, context_score(&now, &c.centroid)));
            }
        }
        // Stable, so equal scores keep the store's order between page views.
        ranked.sort_by(|a, b| b.2.total_cmp(&a.2));

        for (hit, cluster, score) in &ranked {
            let score = *score;
            // A thin cluster must match better, so one accident is not offered as a pattern.
            let firm = cluster.weight >= self.recommend.firm_at;
            let rung = match (firm, score) {
                (true, s) if s >= self.recommend.strong_at => Some(Rung::Pattern),
                (true, s) if s >= self.recommend.weak_at => Some(Rung::Similar),
                (false, s) if s >= self.recommend.strong_at => Some(Rung::Tentative),
                _ => None,
            };
            if let Some(rung) = rung {
                let all = contributions(&now, &cluster.centroid, &self.recommend.weights);
                let rep: serde_json::Value =
                    serde_json::from_str(&cluster.representative).unwrap_or_default();
                return Ok(Some(Offer {
                    artifact_id: hit.payload.artifact_id.clone(),
                    title: title_of(&hit.payload),
                    rung,
                    slot: Some(cluster.slot),
                    events: cluster.events,
                    // Only blocks that agreed: `cosine` is zero for an absent block, which sorts
                    // above negative contributions.
                    blocks: all
                        .iter()
                        .filter(|(_, v)| *v > 0.0)
                        .take(NAMED_BLOCKS)
                        .map(|(l, _)| *l)
                        .collect(),
                    at: rep.get("at").and_then(serde_json::Value::as_i64),
                    at_tz: rep
                        .pointer("/bundle/tz")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                    detail: serde_json::json!({
                        "score": score,
                        "events": cluster.events,
                        "weight": cluster.weight,
                        "bundle": bundle,
                        "representative": rep,
                        "contributions": all
                            .iter()
                            .copied()
                            .collect::<std::collections::BTreeMap<_, _>>(),
                    })
                    .to_string(),
                }));
            }
        }

        // Not `resurface`: it is empty on a new base and stamps what it draws, draining the search
        // page's pool.
        Ok(self
            .store
            .random_artifact()
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "could not draw a card");
                None
            })
            .map(|(id, title, text)| Offer {
                artifact_id: id,
                title: title
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| first_line(&text)),
                rung: Rung::Random,
                slot: None,
                events: 0,
                blocks: Vec::new(),
                at: None,
                at_tz: None,
                detail: serde_json::json!({ "bundle": bundle }).to_string(),
            }))
    }
}

impl Core {
    /// Through `background`, off the request path, so a situation is not dropped at shutdown.
    /// The raw string is stored whole; the denormalised columns are for a person using `sqlite3`.
    pub fn record_context_event(&self, raw: &str, bundle: &Bundle, scope: Option<&str>) {
        if !self.recommends() {
            return;
        }
        let at = self.clock.now();
        let t = crate::core::context::local_time(at, bundle.tz.as_deref(), bundle.tz_offset_mins);
        let row = crate::store::context::ContextEvent {
            id: 0,
            scope: scope.map(str::to_string),
            at,
            bundle: raw.to_string(),
            device_key: crate::core::context::device_key(bundle),
            local_hour: Some(t.hour as f64),
            weekday: Some(t.weekday as i64),
            tz: bundle.tz.clone(),
        };
        let store = self.store.clone();
        self.background.spawn(async move {
            if let Err(e) = store.record_context(&row).await {
                tracing::warn!(error = %e, "could not record the situation of a page view");
                return;
            }
            // Arms the sweep's empty-run backoff, which is a full recompute and otherwise waits
            // ever longer.
            if let Err(e) = store
                .arm_now(
                    crate::store::jobs::Stage::Context,
                    "collection",
                    crate::core::background::CONSOLIDATE_TARGET,
                )
                .await
            {
                tracing::warn!(error = %e, "could not bring the context sweep forward");
            }
        });
    }

    /// Shown against clicked, by rung, is the only hit rate that can settle the block weights.
    pub fn record_recommendation(
        &self,
        artifact_id: &str,
        kind: &str,
        rung: &str,
        slot: Option<i64>,
        scope: Option<&str>,
    ) {
        if !self.recommends() {
            return;
        }
        let detail = serde_json::json!({ "rung": rung, "slot": slot }).to_string();
        let (store, id, kind, scope) = (
            self.store.clone(),
            artifact_id.to_string(),
            kind.to_string(),
            scope.map(str::to_string),
        );
        let at = self.clock.now();
        self.background.spawn(async move {
            if let Err(e) = store
                .record_recommendation(&id, &kind, &detail, scope.as_deref(), at)
                .await
            {
                tracing::warn!(error = %e, "could not record what was offered");
            }
        });
    }
}

impl Core {
    /// The centroids live in Qdrant and a SQLite `DELETE` does not reach them. Runs before the rows
    /// go, since they say which points carry a set; a failure does not block the delete.
    pub async fn forget_situations(&self) -> u64 {
        let ids = match self.store.artifacts_with_context_clusters().await {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!(error = %e, "could not list the points carrying situations");
                return 0;
            }
        };
        let mut cleared = 0;
        for id in &ids {
            match self.vectors.set_context_vectors(id, Vec::new()).await {
                Ok(()) => cleared += 1,
                Err(e) => tracing::warn!(
                    error = %e,
                    artifact = %id,
                    "could not clear the situations on a point"
                ),
            }
        }
        cleared
    }
}

/// Empty where nobody named this text; the card shows no heading rather than someone else's.
fn title_of(p: &crate::vector::VectorPayload) -> String {
    let names_itself = p
        .provenance
        .as_deref()
        .map(crate::store::artifacts::Provenance::parse)
        .is_none_or(|p| p.names_its_own_text());
    if !names_itself {
        return String::new();
    }
    p.title
        .clone()
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| first_line(&p.text))
}

/// Deliberately short: the offer is one line under a search box.
fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    match line.char_indices().nth(70) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None => line.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::context::{Clock, encoder_version};
    use crate::core::test_support::{phone_bundle, recommending_core, seed_artifact};

    #[test]
    fn an_offer_over_a_passage_is_made_without_a_name() {
        let payload = |provenance: &str, title: Option<&str>| crate::vector::VectorPayload {
            artifact_id: "a".into(),
            corpus_id: "c".into(),
            text: "Der Vorgang setzt voraus, dass das Journal noch steht.".into(),
            title: title.map(str::to_string),
            provenance: Some(provenance.into()),
            ..Default::default()
        };
        assert_eq!(title_of(&payload("passage", Some("Kapitel 3"))), "");
        assert_eq!(title_of(&payload("note", None)), "");
        assert_eq!(
            title_of(&payload("captured", Some("Wie ein Journal steht"))),
            "Wie ein Journal steht"
        );
    }

    const FRIDAY: i64 = 1_787_320_320;

    fn desk() -> Bundle {
        Bundle {
            tz: Some("Europe/Berlin".into()),
            platform: Some("macOS".into()),
            ua_family: Some("Firefox".into()),
            screen_w: Some(2560.0),
            screen_h: Some(1440.0),
            viewport_w: Some(1920.0),
            viewport_h: Some(1080.0),
            dpr: Some(2.0),
            cores: Some(16.0),
            memory_gb: Some(32.0),
            language: Some("en-GB".into()),
            color_scheme: Some("light".into()),
            touch: Some(false),
            orientation: Some("landscape".into()),
            network: Some("wired".into()),
            ..Default::default()
        }
    }

    async fn learn(core: &Core, aid: &str, scope: &str, at: i64, b: &Bundle) {
        learn_n(core, aid, scope, at, b, 6.0, 6).await;
    }

    async fn learn_n(
        core: &Core,
        aid: &str,
        scope: &str,
        at: i64,
        b: &Bundle,
        weight: f64,
        events: i64,
    ) {
        let v = encode(at, b, &core.recommend.weights);
        core.store
            .replace_context_clusters(
                aid,
                &[crate::store::context::StoredCluster {
                    scope: Some(scope.into()),
                    artifact_id: aid.into(),
                    slot: 0,
                    centroid: v.clone(),
                    weight,
                    events,
                    last_at: at,
                    encoder_version: encoder_version(&core.recommend.weights),
                    representative: serde_json::json!({ "at": at, "bundle": b }).to_string(),
                }],
            )
            .await
            .unwrap();
        core.vectors
            .set_context_vectors(aid, vec![v])
            .await
            .unwrap();
    }

    async fn learn_raw(
        core: &Core,
        aid: &str,
        scope: &str,
        centroid: Vec<f32>,
        weight: f64,
        events: i64,
    ) {
        core.store
            .replace_context_clusters(
                aid,
                &[crate::store::context::StoredCluster {
                    scope: Some(scope.into()),
                    artifact_id: aid.into(),
                    slot: 0,
                    centroid: centroid.clone(),
                    weight,
                    events,
                    last_at: FRIDAY,
                    encoder_version: encoder_version(&core.recommend.weights),
                    representative: serde_json::json!({ "at": FRIDAY, "bundle": {} }).to_string(),
                }],
            )
            .await
            .unwrap();
        core.vectors
            .set_context_vectors(aid, vec![centroid])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn six_fridays_and_the_seventh_offers_it_before_it_is_asked_for() {
        let core = recommending_core(FRIDAY).await;
        let aid = seed_artifact(&core, "recycling centre opening hours").await;
        let noise = seed_artifact(&core, "invoice template").await;

        for w in 1..=6 {
            let at = FRIDAY - w * 7 * 86_400;
            seen_and_opened(&core, &aid, at, &phone_bundle()).await;
        }
        for w in 1..=6 {
            let at = FRIDAY - w * 7 * 86_400 - 3 * 86_400 - 7 * 3600;
            seen_and_opened(&core, &noise, at, &desk()).await;
        }

        let learned = crate::jobs::context::run(&core).await.unwrap();
        assert_eq!(learned.standing.events, 12);
        assert_eq!(learned.standing.profiled, 2, "both situations were learned");

        let mut seventh = core.clone();
        seventh.clock = Clock::Fixed(FRIDAY - 8 * 60);

        let offer = seventh
            .offer(Some("alice"), &phone_bundle())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            offer.artifact_id, aid,
            "the Friday thing, not the Tuesday one"
        );
        assert_eq!(offer.title, "recycling centre opening hours");
        assert_eq!(offer.rung, Rung::Pattern);
        assert!(offer.blocks.contains(&"weekday"), "{:?}", offer.blocks);
        assert!(offer.blocks.contains(&"hour"), "{:?}", offer.blocks);
        assert!(offer.blocks.contains(&"device"), "{:?}", offer.blocks);
        let at = offer.at.expect("a representative event");
        assert!(
            (1..=6).any(|w| at == FRIDAY - w * 7 * 86_400),
            "quoted {at}, which is not one of the six"
        );
    }

    async fn seen_and_opened(core: &Core, aid: &str, at: i64, b: &Bundle) {
        let raw = serde_json::to_string(b).unwrap();
        let t = crate::core::context::local_time(at, b.tz.as_deref(), None);
        core.store
            .record_context(&crate::store::context::ContextEvent {
                id: 0,
                scope: Some("alice".into()),
                at,
                bundle: raw.clone(),
                device_key: crate::core::context::device_key(b),
                local_hour: Some(t.hour as f64),
                weekday: Some(t.weekday as i64),
                tz: b.tz.clone(),
            })
            .await
            .unwrap();
        core.store
            .record_interaction(aid, "opened", None, Some("alice"), at + 5)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn the_reason_explains_the_artifact_that_was_offered() {
        let core = recommending_core(FRIDAY).await;
        let near = seed_artifact(&core, "recycling centre").await;
        let far = seed_artifact(&core, "invoice template").await;
        learn(&core, &near, "alice", FRIDAY - 7 * 86_400, &phone_bundle()).await;
        learn(
            &core,
            &far,
            "alice",
            FRIDAY - 10 * 86_400 - 7 * 3600,
            &desk(),
        )
        .await;

        let now = encode(FRIDAY, &phone_bundle(), &core.recommend.weights);
        let from_store = core
            .vectors
            .context_query(&now, CANDIDATES, &Default::default())
            .await
            .unwrap();

        let offer = core
            .offer(Some("alice"), &phone_bundle())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            offer.artifact_id, from_store[0].payload.artifact_id,
            "the local argmax reproduces the store's"
        );
        assert_eq!(offer.artifact_id, near);
    }

    #[tokio::test]
    async fn a_deleted_artifact_is_not_offered_and_the_next_best_is() {
        let core = recommending_core(FRIDAY).await;
        let near = seed_artifact(&core, "recycling centre").await;
        let far = seed_artifact(&core, "invoice template").await;
        learn(&core, &near, "alice", FRIDAY - 7 * 86_400, &phone_bundle()).await;
        learn(&core, &far, "alice", FRIDAY - 14 * 86_400, &phone_bundle()).await;

        let offered = async || {
            core.offer(Some("alice"), &phone_bundle())
                .await
                .unwrap()
                .expect("an offer")
        };
        assert_eq!(
            offered().await.artifact_id,
            near,
            "it wins while it is there"
        );

        sqlx::query!("DELETE FROM artifacts WHERE id = ?", &near)
            .execute(&core.store.pool)
            .await
            .unwrap();

        let offer = offered().await;
        assert_eq!(offer.artifact_id, far, "the next best that is still there");
        assert_ne!(
            offer.rung,
            Rung::Random,
            "reached through the ladder, not off the floor below it"
        );
    }

    #[tokio::test]
    async fn a_recurring_situation_is_called_a_pattern_and_names_its_blocks() {
        let core = recommending_core(FRIDAY).await;
        let aid = seed_artifact(&core, "recycling centre").await;
        learn(&core, &aid, "alice", FRIDAY - 7 * 86_400, &phone_bundle()).await;

        let offer = core
            .offer(Some("alice"), &phone_bundle())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(offer.rung, Rung::Pattern);
        assert_eq!(offer.slot, Some(0));
        assert_eq!(offer.title, "recycling centre");
        assert_eq!(offer.blocks.len(), NAMED_BLOCKS);
        assert_eq!(offer.at, Some(FRIDAY - 7 * 86_400));
        assert_eq!(offer.at_tz.as_deref(), Some("Europe/Berlin"));
    }

    #[tokio::test]
    async fn a_resemblance_is_not_called_a_pattern() {
        let core = recommending_core(FRIDAY).await;
        let aid = seed_artifact(&core, "recycling centre").await;
        let mut other = phone_bundle();
        other.network = Some("wifi".into());
        other.orientation = Some("landscape".into());
        learn(&core, &aid, "alice", FRIDAY - 7 * 86_400 - 4 * 3600, &other).await;

        let offer = core
            .offer(Some("alice"), &phone_bundle())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(offer.rung, Rung::Similar, "blocks: {:?}", offer.blocks);
    }

    #[tokio::test]
    async fn one_persons_situations_are_never_offered_to_another() {
        let core = recommending_core(FRIDAY).await;
        let aid = seed_artifact(&core, "recycling centre").await;
        learn(&core, &aid, "alice", FRIDAY - 7 * 86_400, &phone_bundle()).await;

        // Twenty names, not one pair: a single pair that happens not to collide proves nothing.
        for n in 0..20 {
            let who = format!("person-{n}");
            let offer = core.offer(Some(&who), &phone_bundle()).await.unwrap();
            assert!(
                offer
                    .as_ref()
                    .is_none_or(|o| o.rung != Rung::Pattern && o.rung != Rung::Similar),
                "alice's Friday was offered to {who}: {offer:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_foreign_scope_is_cut_exactly_rather_than_out_scored() {
        let core = recommending_core(FRIDAY).await;
        let aid = seed_artifact(&core, "recycling centre").await;
        learn(&core, &aid, "alice", FRIDAY - 7 * 86_400, &phone_bundle()).await;

        let now = encode(FRIDAY, &phone_bundle(), &core.recommend.weights);
        let from_store = core
            .vectors
            .context_query(&now, CANDIDATES, &Default::default())
            .await
            .unwrap();
        assert_eq!(
            from_store.len(),
            1,
            "the store has nothing else to return, so it returns alice's"
        );

        let offer = core.offer(Some("bob"), &phone_bundle()).await.unwrap();
        assert!(
            offer.as_ref().is_none_or(|o| o.slot.is_none()),
            "and the read path cut it anyway: {offer:?}"
        );
    }

    #[tokio::test]
    async fn a_situation_seen_twice_says_twice_rather_than_pattern() {
        let core = recommending_core(FRIDAY).await;
        let aid = seed_artifact(&core, "recycling centre").await;
        // Below `firm_at`, which at the default half-life is two weekly repetitions.
        learn_n(
            &core,
            &aid,
            "alice",
            FRIDAY - 7 * 86_400,
            &phone_bundle(),
            1.9,
            2,
        )
        .await;

        let offer = core
            .offer(Some("alice"), &phone_bundle())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(offer.rung, Rung::Tentative);
        assert_eq!(offer.events, 2, "and it knows how many");
        assert_eq!(offer.slot, Some(0), "still a real cluster");
        assert!(!offer.blocks.is_empty(), "and it still says what matched");
    }

    #[tokio::test]
    async fn a_thin_situation_is_held_to_a_higher_bar_than_an_established_one() {
        let core = recommending_core(FRIDAY).await;
        let aid = seed_artifact(&core, "recycling centre").await;
        let mut other = phone_bundle();
        other.network = Some("wifi".into());
        other.orientation = Some("landscape".into());
        learn_n(
            &core,
            &aid,
            "alice",
            FRIDAY - 7 * 86_400 - 4 * 3600,
            &other,
            1.0,
            1,
        )
        .await;

        let offer = core
            .offer(Some("alice"), &phone_bundle())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            offer.rung,
            Rung::Random,
            "a middling match on one event is not worth a sentence"
        );
    }

    #[tokio::test]
    async fn with_nothing_learned_a_card_is_drawn_and_claims_nothing() {
        let core = recommending_core(FRIDAY).await;
        let aid = seed_artifact(&core, "recycling centre").await;

        let offer = core
            .offer(Some("alice"), &phone_bundle())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(offer.rung, Rung::Random);
        assert_eq!(offer.artifact_id, aid);
        assert!(!offer.rung.is_explained(), "no line is printed beside it");
        assert!(offer.blocks.is_empty());
        assert_eq!(offer.slot, None);
        assert_eq!(offer.events, 0);
        assert!(offer.at.is_none(), "nothing to quote");
    }

    #[tokio::test]
    async fn the_card_works_on_a_base_nobody_has_used_yet() {
        let core = recommending_core(FRIDAY).await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        core.store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    text: "captured five minutes ago".into(),
                    title: Some("brand new".into()),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();

        let offer = core
            .offer(Some("alice"), &phone_bundle())
            .await
            .unwrap()
            .expect("a fresh base still has something to show");
        assert_eq!(offer.title, "brand new");
        assert_eq!(offer.rung, Rung::Random);
    }

    #[tokio::test]
    async fn drawing_a_card_does_not_mark_anything_as_seen() {
        let core = recommending_core(FRIDAY).await;
        let aid = seed_artifact(&core, "recycling centre").await;
        for _ in 0..5 {
            core.offer(Some("alice"), &phone_bundle()).await.unwrap();
        }
        core.background.wait_idle().await;

        let payloads = core
            .vectors
            .payloads_of(std::slice::from_ref(&aid))
            .await
            .unwrap();
        assert!(
            payloads[&aid].last_seen_at.is_none(),
            "five page views must not count as having read it"
        );
    }

    #[tokio::test]
    async fn an_empty_base_is_offered_nothing_rather_than_a_lie() {
        let core = recommending_core(FRIDAY).await;
        assert!(
            core.offer(Some("alice"), &phone_bundle())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn nothing_is_offered_when_the_faculty_is_off() {
        let mut core = recommending_core(FRIDAY).await;
        let aid = seed_artifact(&core, "recycling centre").await;
        learn(&core, &aid, "alice", FRIDAY - 7 * 86_400, &phone_bundle()).await;
        core.recommend.enabled = false;

        assert!(
            core.offer(Some("alice"), &phone_bundle())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_cluster_from_an_older_encoder_explains_nothing() {
        let core = recommending_core(FRIDAY).await;
        let aid = seed_artifact(&core, "recycling centre").await;
        learn(&core, &aid, "alice", FRIDAY - 7 * 86_400, &phone_bundle()).await;
        let mut rows = core
            .store
            .context_clusters_of(std::slice::from_ref(&aid))
            .await
            .unwrap()[&aid]
            .clone();
        rows[0].encoder_version = encoder_version(&core.recommend.weights) + 1;
        core.store
            .replace_context_clusters(&aid, &rows)
            .await
            .unwrap();

        let offer = core.offer(Some("alice"), &phone_bundle()).await.unwrap();
        assert!(
            offer.as_ref().is_none_or(|o| o.slot.is_none()),
            "got {offer:?}"
        );
    }

    #[tokio::test]
    async fn a_cluster_built_under_other_weights_is_not_compared_against_new_ones() {
        let mut core = recommending_core(FRIDAY).await;
        let aid = seed_artifact(&core, "recycling centre").await;
        for w in 1..=6 {
            learn(
                &core,
                &aid,
                "alice",
                FRIDAY - w * 7 * 86_400,
                &phone_bundle(),
            )
            .await;
        }
        let offered = core.offer(Some("alice"), &phone_bundle()).await.unwrap();
        assert!(
            offered.as_ref().is_some_and(|o| o.slot.is_some()),
            "this test needs a learned offer to invalidate: {offered:?}"
        );

        core.recommend.weights.network += 0.5;
        let offer = core.offer(Some("alice"), &phone_bundle()).await.unwrap();
        assert!(
            offer.as_ref().is_none_or(|o| o.slot.is_none()),
            "a centroid from the old geometry was still explained: {offer:?}"
        );
    }

    #[tokio::test]
    async fn the_reason_names_only_the_blocks_that_agreed() {
        let core = recommending_core(FRIDAY).await;
        let aid = seed_artifact(&core, "recycling centre").await;
        let bare = Bundle {
            tz: Some("Europe/Berlin".into()),
            ..Default::default()
        };
        for w in 1..=6 {
            learn(&core, &aid, "alice", FRIDAY - w * 7 * 86_400, &bare).await;
        }
        let offer = core.offer(Some("alice"), &bare).await.unwrap().unwrap();
        for absent in ["battery", "month", "screen", "surroundings"] {
            assert!(
                !offer.blocks.contains(&absent),
                "the line claimed {absent} on a bundle that never carried it: {:?}",
                offer.blocks
            );
        }
        assert!(offer.detail.contains("battery"), "{}", offer.detail);
    }

    #[tokio::test]
    async fn a_hidden_artifact_is_never_the_offer() {
        let core = recommending_core(FRIDAY).await;
        let aid = seed_artifact(&core, "recycling centre").await;
        learn(&core, &aid, "alice", FRIDAY - 7 * 86_400, &phone_bundle()).await;
        core.vectors
            .set_lifecycle(
                &aid,
                crate::store::artifacts::ArtifactStatus::Superseded,
                None,
            )
            .await
            .unwrap();

        let offer = core.offer(Some("alice"), &phone_bundle()).await.unwrap();
        assert!(
            offer.as_ref().is_none_or(|o| o.rung != Rung::Pattern),
            "got {offer:?}"
        );
    }

    #[tokio::test]
    async fn the_details_carry_the_bundle_and_the_numbers() {
        let core = recommending_core(FRIDAY).await;
        let aid = seed_artifact(&core, "recycling centre").await;
        learn(&core, &aid, "alice", FRIDAY - 7 * 86_400, &phone_bundle()).await;

        let offer = core
            .offer(Some("alice"), &phone_bundle())
            .await
            .unwrap()
            .unwrap();
        let d: serde_json::Value = serde_json::from_str(&offer.detail).unwrap();
        assert_eq!(d["bundle"]["tz"], "Europe/Berlin");
        assert!(d["contributions"].is_object());
        assert!(d["contributions"]["weekday"].is_number());
        assert!(d["score"].is_number());
        assert_eq!(d["representative"]["at"], FRIDAY - 7 * 86_400);
    }

    #[tokio::test]
    async fn the_offer_costs_one_query_and_no_embedding() {
        let (mut core, embedder) =
            crate::core::test_support::test_core_counting_embed_calls().await;
        core.recommend.enabled = true;
        core.learn.enabled = true;
        core.clock = Clock::Fixed(FRIDAY);
        let aid = seed_artifact(&core, "recycling centre").await;
        learn(&core, &aid, "alice", FRIDAY - 7 * 86_400, &phone_bundle()).await;
        let before = embedder.calls();

        core.offer(Some("alice"), &phone_bundle()).await.unwrap();
        assert_eq!(embedder.calls(), before, "not one embedding call");
    }

    #[tokio::test]
    async fn a_candidate_that_fails_its_rung_does_not_take_the_others_down_with_it() {
        let core = recommending_core(FRIDAY).await;
        let now = encode(FRIDAY, &Bundle::default(), &core.recommend.weights);

        let free = crate::core::context::BLOCKS
            .iter()
            .find(|b| b.name == "viewport")
            .unwrap()
            .at;
        // `cos(t) * now_hat + sin(t) * e` scores exactly `cos(t)` against `now`, as `e` is
        // orthogonal to it.
        let at_score = |target: f32| {
            let n = now.iter().map(|v| v * v).sum::<f32>().sqrt();
            let mut v: Vec<f32> = now.iter().map(|x| x / n * target).collect();
            v[free] += (1.0 - target * target).sqrt();
            v
        };

        let thin = at_score(0.65);
        let firm = at_score(0.55);

        let a = seed_artifact(&core, "a coincidence").await;
        let b = seed_artifact(&core, "recycling centre").await;
        learn_raw(&core, &a, "alice", thin.clone(), 1.0, 1).await;
        learn_raw(&core, &b, "alice", firm.clone(), 6.0, 6).await;

        // Asserted, so a change in the encoder's weights fails this test instead of passing it for
        // another reason.
        let thin_score = context_score(&now, &thin);
        let firm_score = context_score(&now, &firm);
        assert!(
            thin_score > firm_score,
            "the thin cluster no longer wins the ranking"
        );
        assert!(thin_score < core.recommend.strong_at);
        assert!(
            firm_score >= core.recommend.weak_at && firm_score < core.recommend.strong_at,
            "the firm cluster is not a Similar: {firm_score}"
        );

        let offer = core
            .offer(Some("alice"), &Bundle::default())
            .await
            .unwrap()
            .expect("an offer");
        assert_eq!(offer.rung, Rung::Similar, "fell through to the floor");
        assert_eq!(offer.artifact_id, b, "offered the candidate that failed");
    }
}
