//! What to offer under the search box, and why.
//!
//! One vector query against the `ctx` multivector, then the winning cluster
//! re-derived locally — because Qdrant returns the `max_sim` score and not
//! *which* element produced it, and the display needs the element.
//!
//! That places the same arithmetic in two places. It is the price of holding
//! the vectors in the index and the reason outside it, and a test pins that
//! both pick the same artifact: if they drift, the line under the offer
//! explains a hit it is not explaining.

use crate::core::Core;
use crate::core::context::{Bundle, context_score, contributions, encode};
use crate::error::Result;
use crate::vector::cosine;

/// How many artifacts the store is asked for.
///
/// Ten. Every one of them has its clusters reloaded and rescored locally, at
/// most five clusters of 53 dimensions apiece — a few thousand multiplications,
/// which is free next to the round trip that fetched them.
pub const CANDIDATES: usize = 10;

/// How many blocks the line names. Three, sorted by contribution: enough to say
/// what decided it, short enough to stay one line.
pub const NAMED_BLOCKS: usize = 3;

/// Which rung of the ladder the offer rests on.
///
/// Two questions decide it, not one. *How well does the situation match* —
/// which separates `Pattern` from `Similar` — and *how often has it happened*,
/// which separates both from `Tentative`. The second used to be thrown away
/// after the sweep applied it as a cutoff, and throwing it away is what made a
/// thing done twice indistinguishable from a thing done twenty times.
///
/// `Random` is the floor, and it is not a claim. Nothing about the situation
/// produced it, so nothing is printed beside it: it exists because a base that
/// has not learned anything yet should still have something to look at, and
/// saying "here is a card, no reason" is the honest way to do that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rung {
    Pattern,
    Similar,
    /// A real situation, seen once or twice. Held to a higher bar on the match
    /// than `Similar` is, because there is less behind it.
    Tentative,
    Random,
}

impl Rung {
    /// For the recorded row, and for the Ops breakdown. Fixed buckets, unlike
    /// the wording, which varies with the count.
    pub fn as_str(&self) -> &'static str {
        match self {
            Rung::Pattern => "pattern",
            Rung::Similar => "similar",
            Rung::Tentative => "tentative",
            Rung::Random => "random",
        }
    }

    /// Whether this rung explains itself at all. `Random` does not, and the
    /// page prints no line beside it.
    pub fn is_explained(&self) -> bool {
        !matches!(self, Rung::Random)
    }
}

/// What is offered under the search box, and everything the page needs to say
/// why.
#[derive(Debug, Clone)]
pub struct Offer {
    pub artifact_id: String,
    pub title: String,
    pub rung: Rung,
    /// The winning cluster, for the recorded row. `None` on the random card.
    pub slot: Option<i64>,
    /// How many events the winning cluster was built from. Zero on the random
    /// card. What `Tentative` puts into words.
    pub events: i64,
    /// The blocks that decided it, largest contribution first, at most
    /// `NAMED_BLOCKS`. Empty on the random card, which matched nothing.
    pub blocks: Vec<&'static str>,
    /// The representative event's stamp. What "like 08.08., 15:04" prints.
    pub at: Option<i64>,
    /// The zone the representative event happened in, so that stamp is printed
    /// as the device read it. Taking the *current* bundle's zone would misdate
    /// every situation recorded on a trip.
    pub at_tz: Option<String>,
    /// The raw bundle and the contribution numbers, JSON, for the `<details>`.
    pub detail: String,
}

impl Core {
    /// What to offer, from the situation this page view happened in.
    ///
    /// One vector query and no embedding. The winning cluster is re-derived
    /// here because Qdrant's `max_sim` yields the maximum and not which element
    /// produced it — and the display needs the element, both to quote it and to
    /// name the blocks that decided it.
    pub async fn offer(&self, scope: Option<&str>, bundle: &Bundle) -> Result<Option<Offer>> {
        if !self.recommends() {
            return Ok(None);
        }
        let now_at = self.clock.now();
        let now = encode(now_at, scope, bundle, &self.recommend.weights);

        // Superseded and deprecated are out, by `must_not` — the same rule
        // search obeys, and for the same reason a hand-written point carrying
        // no `status` key must still be offered.
        let hits = self
            .vectors
            .context_query(&now, CANDIDATES, &Default::default())
            .await
            .unwrap_or_else(|e| {
                // A vector store that cannot answer must not take the search
                // page down with it: without an offer the page is what it was
                // yesterday, and the ladder still has two rungs below this.
                tracing::warn!(error = %e, "context query unavailable; falling through the ladder");
                Vec::new()
            });

        let ids: Vec<String> = hits.iter().map(|h| h.payload.artifact_id.clone()).collect();
        let clusters = self.store.context_clusters_of(&ids).await?;

        // The argmax, over the **full** vector — that is what reproduces the
        // store's choice, because that is what `max_sim` scored. The rung comes
        // from `context_score`, which slices the `scope` block off: the two
        // numbers answer different questions and live on different scales.
        let mut best: Option<(
            &crate::vector::SearchHit,
            &crate::store::context::StoredCluster,
            f32,
        )> = None;
        for hit in &hits {
            let Some(mine) = clusters.get(&hit.payload.artifact_id) else {
                continue;
            };
            for c in mine {
                // Exact, not probabilistic. The `scope` block keeps a foreign
                // cluster from ranking first in the store, but it is a
                // direction in 53 dimensions and two of them are only ever
                // *near*-orthogonal — and isolation must not be a probability.
                // A cluster belongs to whoever opened the artifact, and this
                // reads only the ones that belong to the caller.
                //
                // This is also why the multivector's own `scope` block cannot
                // be the guarantee: a payload filter acts on the point, not on
                // elements of the set, so Qdrant cannot make this cut. Loading
                // the clusters is what makes it available, and the read path
                // loads them anyway to produce the reason.
                if c.scope.as_deref() != scope {
                    continue;
                }
                // A cluster written under another layout is skipped rather than
                // explained with the wrong blocks. Its centroid may still be in
                // the index — a rebuild copies any set whose width matches —
                // and the next sweep replaces it.
                if c.encoder_version != crate::core::context::ENCODER_VERSION
                    || c.centroid.len() != now.len()
                {
                    continue;
                }
                let full = cosine(&now, &c.centroid);
                if best.is_none_or(|(_, _, b)| full > b) {
                    best = Some((hit, c, full));
                }
            }
        }

        if let Some((hit, cluster, _)) = best {
            let score = context_score(&now, &cluster.centroid);
            // Established, or seen only once or twice. A thin cluster has to
            // match the situation *better* before anything is said, which is
            // what keeps a single accident from being offered as if it meant
            // something.
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
                    blocks: all.iter().take(NAMED_BLOCKS).map(|(l, _)| *l).collect(),
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

        // The floor: something to look at while the base has nothing to say
        // about this situation. Drawn at random and claimed to be nothing —
        // no blocks, no stamp, no line under it.
        //
        // Deliberately not `resurface`. That answers "what has been forgotten",
        // which on a base anyone has just started using is nothing at all, and
        // it stamps what it draws as seen — draining the pool the search page's
        // own resurfacing lives on, one page view at a time.
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
    /// Write down the situation this page view happened in.
    ///
    /// Off the request path: a page view must not get slower, or fail, because
    /// a bookkeeping write did — and a situation dropped at shutdown is a
    /// Friday afternoon the base never learns about, which is why it goes
    /// through `background` rather than a bare `tokio::spawn`.
    ///
    /// The raw string is stored whole, including the fields the encoder does
    /// not read today; the denormalised columns are what the sweep reads on
    /// every row.
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
            local_hour: Some(t.hour as i64),
            weekday: Some(t.weekday as i64),
            tz: bundle.tz.clone(),
        };
        let store = self.store.clone();
        self.background.spawn(async move {
            if let Err(e) = store.record_context(&row).await {
                tracing::warn!(error = %e, "could not record the situation of a page view");
            }
        });
    }

    /// Write down that something was offered, or that the offer was taken.
    ///
    /// Shown against clicked, broken down by rung, is a hit rate — the only
    /// number that can later settle whether the block weights are right. They
    /// are chosen, not measured, and a recommender with no visible hit rate
    /// becomes `[sitting] prime`: a default nobody ever moved because nobody
    /// could see its effect.
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

fn title_of(p: &crate::vector::VectorPayload) -> String {
    p.title
        .clone()
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| first_line(&p.text))
}

/// The opening of the text, for something with no title. Deliberately short:
/// the offer is one line under a search box, not a result card.
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
    use crate::core::context::{Clock, ENCODER_VERSION};
    use crate::core::test_support::test_core;

    /// 2026-08-21T13:52:00Z — a Friday, 15:52 in Berlin.
    const FRIDAY: i64 = 1_787_320_320;

    async fn core_at(now: i64) -> Core {
        let mut core = test_core().await;
        core.recommend.enabled = true;
        core.clock = Clock::Fixed(now);
        core
    }

    fn phone() -> Bundle {
        Bundle {
            tz: Some("Europe/Berlin".into()),
            platform: Some("Android".into()),
            ua_family: Some("Chrome".into()),
            screen_w: Some(390.0),
            screen_h: Some(844.0),
            viewport_w: Some(390.0),
            viewport_h: Some(844.0),
            dpr: Some(3.0),
            cores: Some(8.0),
            memory_gb: Some(4.0),
            language: Some("de-DE".into()),
            color_scheme: Some("dark".into()),
            touch: Some(true),
            orientation: Some("portrait".into()),
            network: Some("cellular".into()),
            ..Default::default()
        }
    }

    /// A desktop, agreeing with `phone()` about nothing.
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

    /// Give `aid` one established situation, at `at`, in both stores.
    async fn learn(core: &Core, aid: &str, scope: &str, at: i64, b: &Bundle) {
        learn_n(core, aid, scope, at, b, 6.0, 6).await;
    }

    /// The same, with the weight and the count said explicitly — so a test can
    /// build a situation that is real but thin.
    async fn learn_n(
        core: &Core,
        aid: &str,
        scope: &str,
        at: i64,
        b: &Bundle,
        weight: f64,
        events: i64,
    ) {
        let v = encode(at, Some(scope), b, &core.recommend.weights);
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
                    encoder_version: ENCODER_VERSION,
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

    /// One artifact with a vector point behind it. `created_at` is 0 and
    /// nothing has been shown, so `resurface` will also return it — which is
    /// what the bottom rung needs.
    async fn seed_artifact(core: &Core, title: &str) -> String {
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let a = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: format!("text of {title}"),
                    corpus_span: None,
                    title: Some(title.into()),
                    category: None,
                    tags: vec![],
                    segment_idx: None,
                    caveats: vec![],
                }],
            )
            .await
            .unwrap()
            .remove(0);
        core.vectors
            .upsert(vec![crate::vector::VectorPoint {
                vector: vec![1.0; 8],
                sparse: Default::default(),
                payload: crate::vector::VectorPayload {
                    artifact_id: a.id.clone(),
                    corpus_id: src.id.clone(),
                    text: a.text.clone(),
                    title: Some(title.into()),
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
            }])
            .await
            .unwrap();
        a.id
    }

    #[tokio::test]
    async fn six_fridays_and_the_seventh_offers_it_before_it_is_asked_for() {
        // The whole feature in one test, and the example it was asked for:
        // seed six Fridays, set the clock to the seventh at 14:52, send the
        // phone bundle, and assert the artifact comes back at rung Pattern with
        // weekday, hour and device named. Nothing here calls a model or an
        // embedder — every step is the production path with a fixed clock in
        // it, and the sweep is the real one.
        let core = core_at(FRIDAY).await;
        let aid = seed_artifact(&core, "recycling centre opening hours").await;
        let noise = seed_artifact(&core, "invoice template").await;

        // Six Fridays at 15:00 on the phone, opening the recycling centre.
        for w in 1..=6 {
            let at = FRIDAY - w * 7 * 86_400;
            seen_and_opened(&core, &aid, at, &phone()).await;
        }
        // And six Tuesday mornings at a desk, opening something else — so the
        // answer is a choice rather than the only candidate.
        for w in 1..=6 {
            let at = FRIDAY - w * 7 * 86_400 - 3 * 86_400 - 7 * 3600;
            seen_and_opened(&core, &noise, at, &desk()).await;
        }

        let learned = crate::jobs::context::run(&core).await.unwrap();
        assert_eq!(learned.events, 12);
        assert_eq!(learned.profiled, 2, "both situations were learned");

        // The seventh Friday, eight minutes before the usual hour.
        let mut seventh = core.clone();
        seventh.clock = Clock::Fixed(FRIDAY - 8 * 60);

        let offer = seventh
            .offer(Some("alice"), &phone())
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
        // And it quotes a Friday that actually happened.
        let at = offer.at.expect("a representative event");
        assert!(
            (1..=6).any(|w| at == FRIDAY - w * 7 * 86_400),
            "quoted {at}, which is not one of the six"
        );
    }

    /// One page view and the open that followed it, at `at`.
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
                local_hour: Some(t.hour as i64),
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
        // If the local recomputation and the store disagree, the line explains a
        // different artifact than the one shown — which is the one dishonesty
        // this feature must not commit.
        let core = core_at(FRIDAY).await;
        let near = seed_artifact(&core, "recycling centre").await;
        let far = seed_artifact(&core, "invoice template").await;
        learn(&core, &near, "alice", FRIDAY - 7 * 86_400, &phone()).await;
        learn(
            &core,
            &far,
            "alice",
            FRIDAY - 10 * 86_400 - 7 * 3600,
            &desk(),
        )
        .await;

        let now = encode(FRIDAY, Some("alice"), &phone(), &core.recommend.weights);
        let from_store = core
            .vectors
            .context_query(&now, CANDIDATES, &Default::default())
            .await
            .unwrap();

        let offer = core.offer(Some("alice"), &phone()).await.unwrap().unwrap();
        assert_eq!(
            offer.artifact_id, from_store[0].payload.artifact_id,
            "the local argmax reproduces the store's"
        );
        assert_eq!(offer.artifact_id, near);
    }

    #[tokio::test]
    async fn a_recurring_situation_is_called_a_pattern_and_names_its_blocks() {
        let core = core_at(FRIDAY).await;
        let aid = seed_artifact(&core, "recycling centre").await;
        learn(&core, &aid, "alice", FRIDAY - 7 * 86_400, &phone()).await;

        let offer = core.offer(Some("alice"), &phone()).await.unwrap().unwrap();
        assert_eq!(offer.rung, Rung::Pattern);
        assert_eq!(offer.slot, Some(0));
        assert_eq!(offer.title, "recycling centre");
        assert_eq!(offer.blocks.len(), NAMED_BLOCKS);
        assert_eq!(offer.at, Some(FRIDAY - 7 * 86_400));
        assert_eq!(offer.at_tz.as_deref(), Some("Europe/Berlin"));
    }

    #[tokio::test]
    async fn a_resemblance_is_not_called_a_pattern() {
        // The wording says what it rests on. The distance between "Fridays
        // around 15:00" and "similar to" is the whole honesty of the feature.
        let core = core_at(FRIDAY).await;
        let aid = seed_artifact(&core, "recycling centre").await;
        // Same Friday and same phone, four hours off, on wifi and in landscape.
        let mut other = phone();
        other.network = Some("wifi".into());
        other.orientation = Some("landscape".into());
        learn(&core, &aid, "alice", FRIDAY - 7 * 86_400 - 4 * 3600, &other).await;

        let offer = core.offer(Some("alice"), &phone()).await.unwrap().unwrap();
        assert_eq!(offer.rung, Rung::Similar, "blocks: {:?}", offer.blocks);
    }

    #[tokio::test]
    async fn one_persons_situations_are_never_offered_to_another() {
        // Until per-user collections exist, the `scope` block is the whole of
        // the isolation, and it needs a test that says so.
        let core = core_at(FRIDAY).await;
        let aid = seed_artifact(&core, "recycling centre").await;
        learn(&core, &aid, "alice", FRIDAY - 7 * 86_400, &phone()).await;

        // Twenty other names, not one. The first version of the `scope` block
        // was one-hot over eight buckets, so any two people had a one-in-eight
        // chance of sharing a slot outright — and a single hand-picked pair
        // that happened not to collide would have passed while the guarantee
        // was worth 87%. Isolation is not a probability, so this does not test
        // it like one.
        for n in 0..20 {
            let who = format!("person-{n}");
            let offer = core.offer(Some(&who), &phone()).await.unwrap();
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
        // The `scope` block keeps a foreign cluster from ranking first in the
        // store, but it is a direction in 53 dimensions and two of them are
        // only ever near-orthogonal. What makes isolation exact is that the
        // read path loads the clusters — which it does anyway, to produce the
        // reason — and reads only the ones belonging to the caller.
        //
        // So: bob has learned nothing, and alice's cluster is the *only* thing
        // in the index. The store therefore returns it however anyone scores,
        // and bob must still not be offered it as a pattern.
        let core = core_at(FRIDAY).await;
        let aid = seed_artifact(&core, "recycling centre").await;
        learn(&core, &aid, "alice", FRIDAY - 7 * 86_400, &phone()).await;

        let now = encode(FRIDAY, Some("bob"), &phone(), &core.recommend.weights);
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

        let offer = core.offer(Some("bob"), &phone()).await.unwrap();
        assert!(
            offer.as_ref().is_none_or(|o| o.slot.is_none()),
            "and the read path cut it anyway: {offer:?}"
        );
    }

    #[tokio::test]
    async fn a_situation_seen_twice_says_twice_rather_than_pattern() {
        // The middle ground. Two occurrences are a real thing that happened and
        // worth offering, and calling them a pattern would be a claim the
        // evidence does not carry. The line says the number instead.
        let core = core_at(FRIDAY).await;
        let aid = seed_artifact(&core, "recycling centre").await;
        // Weight below `firm_at`, which at the default half-life is what two
        // weekly repetitions come to.
        learn_n(&core, &aid, "alice", FRIDAY - 7 * 86_400, &phone(), 1.9, 2).await;

        let offer = core.offer(Some("alice"), &phone()).await.unwrap().unwrap();
        assert_eq!(offer.rung, Rung::Tentative);
        assert_eq!(offer.events, 2, "and it knows how many");
        assert_eq!(offer.slot, Some(0), "still a real cluster");
        assert!(!offer.blocks.is_empty(), "and it still says what matched");
    }

    #[tokio::test]
    async fn a_thin_situation_is_held_to_a_higher_bar_than_an_established_one() {
        // With less behind it, the situation has to match *better* before
        // anything is said at all. Otherwise one accident on a Tuesday would be
        // offered every Tuesday after it.
        let core = core_at(FRIDAY).await;
        let aid = seed_artifact(&core, "recycling centre").await;
        // A middling match — the kind that earns `Similar` when established.
        let mut other = phone();
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

        let offer = core.offer(Some("alice"), &phone()).await.unwrap().unwrap();
        assert_eq!(
            offer.rung,
            Rung::Random,
            "a middling match on one event is not worth a sentence"
        );
    }

    #[tokio::test]
    async fn with_nothing_learned_a_card_is_drawn_and_claims_nothing() {
        // The floor. Something to look at while the base has nothing to say
        // about the situation — and nothing printed beside it, because nothing
        // about the situation produced it.
        let core = core_at(FRIDAY).await;
        let aid = seed_artifact(&core, "recycling centre").await;

        let offer = core.offer(Some("alice"), &phone()).await.unwrap().unwrap();
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
        // The whole reason this is not `resurface`: that one answers "older
        // than thirty days and unshown for thirty days", which on a base
        // somebody started this morning is nothing at all — empty in exactly
        // the moment this rung exists for.
        let core = core_at(FRIDAY).await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        core.store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: "captured five minutes ago".into(),
                    corpus_span: None,
                    title: Some("brand new".into()),
                    category: None,
                    tags: vec![],
                    segment_idx: None,
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();

        let offer = core
            .offer(Some("alice"), &phone())
            .await
            .unwrap()
            .expect("a fresh base still has something to show");
        assert_eq!(offer.title, "brand new");
        assert_eq!(offer.rung, Rung::Random);
    }

    #[tokio::test]
    async fn drawing_a_card_does_not_mark_anything_as_seen() {
        // `resurface` stamps what it draws, which is right for a list somebody
        // asked for and wrong for something that fires on every page view: it
        // would drain the pool the search page's own resurfacing lives on, one
        // page view at a time.
        let core = core_at(FRIDAY).await;
        let aid = seed_artifact(&core, "recycling centre").await;
        for _ in 0..5 {
            core.offer(Some("alice"), &phone()).await.unwrap();
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
        let core = core_at(FRIDAY).await;
        assert!(core.offer(Some("alice"), &phone()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn nothing_is_offered_when_the_faculty_is_off() {
        let mut core = core_at(FRIDAY).await;
        let aid = seed_artifact(&core, "recycling centre").await;
        learn(&core, &aid, "alice", FRIDAY - 7 * 86_400, &phone()).await;
        core.recommend.enabled = false;

        assert!(core.offer(Some("alice"), &phone()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_cluster_from_an_older_encoder_explains_nothing() {
        // Its centroid may still be in the index — a rebuild copies a set whose
        // width matches — but the blocks it was built from are not the blocks
        // this reader knows. Skipped, rather than described with the wrong ones.
        let core = core_at(FRIDAY).await;
        let aid = seed_artifact(&core, "recycling centre").await;
        learn(&core, &aid, "alice", FRIDAY - 7 * 86_400, &phone()).await;
        let mut rows = core
            .store
            .context_clusters_of(std::slice::from_ref(&aid))
            .await
            .unwrap()[&aid]
            .clone();
        rows[0].encoder_version = ENCODER_VERSION + 1;
        core.store
            .replace_context_clusters(&aid, &rows)
            .await
            .unwrap();

        let offer = core.offer(Some("alice"), &phone()).await.unwrap();
        assert!(
            offer.as_ref().is_none_or(|o| o.slot.is_none()),
            "got {offer:?}"
        );
    }

    #[tokio::test]
    async fn a_hidden_artifact_is_never_the_offer() {
        let core = core_at(FRIDAY).await;
        let aid = seed_artifact(&core, "recycling centre").await;
        learn(&core, &aid, "alice", FRIDAY - 7 * 86_400, &phone()).await;
        core.vectors
            .set_lifecycle(
                &aid,
                crate::store::artifacts::ArtifactStatus::Superseded,
                None,
            )
            .await
            .unwrap();

        let offer = core.offer(Some("alice"), &phone()).await.unwrap();
        assert!(
            offer.as_ref().is_none_or(|o| o.rung != Rung::Pattern),
            "got {offer:?}"
        );
    }

    #[tokio::test]
    async fn the_details_carry_the_bundle_and_the_numbers() {
        // "The parameters must be visible" — whoever wants to know exactly,
        // expands it. It is also the answer to what is being collected:
        // inspectable rather than promised.
        let core = core_at(FRIDAY).await;
        let aid = seed_artifact(&core, "recycling centre").await;
        learn(&core, &aid, "alice", FRIDAY - 7 * 86_400, &phone()).await;

        let offer = core.offer(Some("alice"), &phone()).await.unwrap().unwrap();
        let d: serde_json::Value = serde_json::from_str(&offer.detail).unwrap();
        assert_eq!(d["bundle"]["tz"], "Europe/Berlin");
        assert!(d["contributions"].is_object());
        assert!(d["contributions"]["weekday"].is_number());
        assert!(d["score"].is_number());
        assert_eq!(d["representative"]["at"], FRIDAY - 7 * 86_400);
    }

    #[tokio::test]
    async fn the_offer_costs_one_query_and_no_embedding() {
        // The constraint from the top of the roadmap: no model call and no
        // embedding at read time. A counting embedder proves it rather than a
        // comment claiming it.
        let (mut core, embedder) =
            crate::core::test_support::test_core_counting_embed_calls().await;
        core.recommend.enabled = true;
        core.clock = Clock::Fixed(FRIDAY);
        let aid = seed_artifact(&core, "recycling centre").await;
        learn(&core, &aid, "alice", FRIDAY - 7 * 86_400, &phone()).await;
        let before = embedder.calls();

        core.offer(Some("alice"), &phone()).await.unwrap();
        assert_eq!(embedder.calls(), before, "not one embedding call");
    }
}
