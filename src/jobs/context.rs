//! What situations recur for which artifact.
//!
//! One pass over the log, no randomness, no model call. It joins three sources
//! on `scope` and `at` — never on a stored id — agglomerates the situations an
//! artifact was opened in, decays them, and writes the surviving centroids to
//! the vector store as that artifact's `ctx` set.
//!
//! A full rebuild every run, from the raw bundles. That is what makes a change
//! to the encoder a sweep rather than a migration, and it is why the sweep
//! never reads what a previous run concluded.

use crate::core::Core;
use crate::error::Result;
use crate::vector::cosine;

/// How often this runs.
///
/// A constant rather than a setting, for the reason `REPAIR_INTERVAL_HOURS`
/// gives: how often a faculty learns is not a preference, and `[recommend]` is
/// one gate and a table of weights. Six hours means a situation recorded this
/// morning is offered this evening, which is as fast as anything here needs to
/// be — the patterns being learned are weekly.
pub const INTERVAL_HOURS: u64 = 6;

/// How long after a search an open still counts as that search's.
///
/// The bridge: where an open followed a search, the event inherits the search's
/// identity, so a recurring search resolves to the artifact it led to rather
/// than to a rerun of the query. Fifteen minutes, which is the same order as
/// `pursuit.idle_secs` and deliberately not that key — pursuits may be off, and
/// this must still work.
pub const BRIDGE_SECS: i64 = 900;

/// How far from an open a recorded situation may sit and still be that open's.
///
/// Half an hour either way. A page view records a situation; the opens that
/// follow it belong to it. Wider than `BRIDGE_SECS` because a person can read
/// for a while after arriving, and a situation is a slower thing than a query.
pub const MATCH_SECS: i64 = 1800;

/// Centroids one artifact may carry, across every scope.
///
/// `max_clusters` bounds the situations per person per artifact; this bounds
/// the array itself, which is shared by every scope that ever opened the
/// artifact. On a base with one person it never binds. On one with many it is
/// what keeps a popular artifact's multivector from growing with the user
/// count — the heaviest survive, which is the same rule `min_weight` applies
/// one level down.
pub const MAX_SLOTS: usize = 16;

/// One situation an artifact was opened in, ready to cluster.
#[derive(Debug, Clone)]
pub struct Member {
    pub vec: Vec<f32>,
    /// Already decayed, and already multiplied by `self_weight` where this was
    /// an open of something this feature offered.
    pub weight: f64,
    pub at: i64,
    /// The raw bundle, carried so the winner can be quoted.
    pub bundle: String,
}

/// One learned situation.
#[derive(Debug, Clone)]
pub struct Cluster {
    pub centroid: Vec<f32>,
    pub weight: f64,
    /// How many events went into it, undecayed. `weight` says how much this
    /// still counts; this says how often it happened, which is what the line
    /// under the offer can put in words a person reads.
    pub events: usize,
    pub last_at: i64,
    /// `{"at": <unix>, "bundle": {…}}` for the member nearest the centroid.
    pub representative: String,
}

/// What one run did.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Report {
    pub standing: Standing,
    /// Artifacts whose every situation had decayed away.
    ///
    /// The one count here that is work: a profile that existed before this run
    /// and does not after it. Everything else this run touched, it rewrote to
    /// what was already there.
    pub cleared: usize,
}

/// What the pass saw and re-derived, as opposed to what it changed.
///
/// Nested, and the nesting is what makes the empty-run backoff reachable for
/// this sweep: `jobs::did_work` calls any non-zero flat number work, and all
/// three of these are non-zero on every run over unchanged data. `run` is a
/// full recompute — it reads every in-window event, re-clusters it and writes
/// the same rows back — so `events`, `profiled` and `clusters` describe the
/// window rather than the run, and at the top level they held the sweep at its
/// configured period for the life of a dormant base.
///
/// Backing off is safe here only because a new open cancels the wait:
/// `Core::record_context_event` arms this unit, which is what stops a
/// situation written during a long backoff from waiting the rest of it out.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Standing {
    /// Opens in the window that had a situation to encode from.
    pub events: usize,
    /// Artifacts left with at least one situation after this run.
    pub profiled: usize,
    /// Clusters written across those artifacts.
    pub clusters: usize,
}

/// One deterministic pass, in event order.
///
/// An event joins the nearest cluster when cosine exceeds `merge_at`, otherwise
/// it opens its own; when the count exceeds `max_clusters` the two nearest are
/// merged. One pass and no randomness, because otherwise it is not testable —
/// and because a recommendation whose reason changed between two sweeps over
/// identical data could not be accounted for by anyone.
///
/// Members arrive already decayed. A cluster whose total weight falls below
/// `min_weight` is dropped, which is what protects against the single accident:
/// one event never reaches the threshold.
pub fn agglomerate(
    members: &[Member],
    merge_at: f32,
    max_clusters: usize,
    min_weight: f64,
) -> Vec<Cluster> {
    struct Building {
        centroid: Vec<f32>,
        weight: f64,
        last_at: i64,
        /// Indices into `members`, so the representative can be chosen against
        /// the *final* centroid rather than against whatever it was when each
        /// member arrived.
        members: Vec<usize>,
    }

    let cap = max_clusters.max(1);
    let mut built: Vec<Building> = Vec::new();

    for (i, m) in members.iter().enumerate() {
        if m.weight <= 0.0 {
            continue;
        }
        let nearest = built
            .iter()
            .enumerate()
            .map(|(k, c)| (k, cosine(&m.vec, &c.centroid)))
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        match nearest {
            Some((k, sim)) if sim > merge_at => {
                let c = &mut built[k];
                blend(&mut c.centroid, c.weight, &m.vec, m.weight);
                c.weight += m.weight;
                c.last_at = c.last_at.max(m.at);
                c.members.push(i);
            }
            _ => built.push(Building {
                centroid: m.vec.clone(),
                weight: m.weight,
                last_at: m.at,
                members: vec![i],
            }),
        }

        while built.len() > cap {
            let centroids: Vec<&[f32]> = built.iter().map(|c| &c.centroid[..]).collect();
            let Some((a, b)) = closest_pair(&centroids) else {
                break;
            };
            let victim = built.remove(b);
            let host = &mut built[a];
            blend(
                &mut host.centroid,
                host.weight,
                &victim.centroid,
                victim.weight,
            );
            host.weight += victim.weight;
            host.last_at = host.last_at.max(victim.last_at);
            host.members.extend(victim.members);
        }
    }

    let mut out: Vec<Cluster> = built
        .into_iter()
        .filter(|c| c.weight >= min_weight)
        .map(|c| {
            // The representative is the member nearest the *finished* centroid.
            // Ties break on the index, so which of two equally central events
            // is quoted does not depend on a float comparison going either way.
            let rep = c
                .members
                .iter()
                .map(|&i| (i, cosine(&members[i].vec, &c.centroid)))
                .max_by(|x, y| {
                    x.1.partial_cmp(&y.1)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| y.0.cmp(&x.0))
                })
                .map(|(i, _)| i);
            let representative = rep
                .map(|i| {
                    // The bundle plus its stamp, because a bundle carries no
                    // time of its own and the line says "like 08.08., 15:04".
                    // Always valid JSON: a bundle that will not re-parse becomes
                    // an empty object rather than corrupting the row.
                    let bundle: serde_json::Value = serde_json::from_str(&members[i].bundle)
                        .unwrap_or_else(|_| serde_json::json!({}));
                    serde_json::json!({ "at": members[i].at, "bundle": bundle }).to_string()
                })
                .unwrap_or_else(|| r#"{"at":0,"bundle":{}}"#.to_string());
            Cluster {
                centroid: c.centroid,
                events: c.members.len(),
                weight: c.weight,
                last_at: c.last_at,
                representative,
            }
        })
        .collect();
    // Heaviest first: this is the order slots are allocated in, and `MAX_SLOTS`
    // keeps the front of it.
    out.sort_by(|a, b| {
        b.weight
            .partial_cmp(&a.weight)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.last_at.cmp(&a.last_at))
    });
    out
}

/// Weighted mean, in place.
fn blend(centroid: &mut [f32], have: f64, add: &[f32], w: f64) {
    let total = have + w;
    if total <= 0.0 {
        return;
    }
    for (i, c) in centroid.iter_mut().enumerate() {
        let other = add.get(i).copied().unwrap_or(0.0);
        *c = ((*c as f64 * have + other as f64 * w) / total) as f32;
    }
}

/// The two nearest centroids, as `(keep, drop)` with `keep < drop`.
///
/// Quadratic, over at most `max_clusters + 1` vectors of 45 dimensions. That is
/// a few hundred multiplications on a path that runs every six hours.
fn closest_pair(centroids: &[&[f32]]) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize, f32)> = None;
    for a in 0..centroids.len() {
        for b in (a + 1)..centroids.len() {
            let sim = cosine(centroids[a], centroids[b]);
            if best.is_none_or(|(_, _, s)| sim > s) {
                best = Some((a, b, sim));
            }
        }
    }
    best.map(|(a, b, _)| (a, b))
}

/// The slice of an `at`-ordered log that falls in `[lo, hi]`.
///
/// The three sources come back ordered by time — `ORDER BY at, id` in each of
/// the three queries — and the sweep asks each of them the same question once
/// per interaction: what is near this moment. Scanning the whole vector for
/// that made the pass quadratic, which on a base with a year of page views and
/// a log that is never pruned is a background job doing hundreds of millions of
/// string comparisons every six hours. Two `partition_point` calls answer it
/// against the order the rows already arrive in.
fn window<T>(v: &[T], at: impl Fn(&T) -> i64, lo: i64, hi: i64) -> &[T] {
    let start = v.partition_point(|x| at(x) < lo);
    let end = v.partition_point(|x| at(x) <= hi);
    &v[start..end.max(start)]
}

/// One pass: read the log, encode, cluster, write.
pub async fn run(core: &Core) -> Result<Report> {
    let mut report = Report::default();
    if !core.recommends() {
        return Ok(report);
    }
    let cfg = &core.recommend;
    let now = core.clock.now();
    let since = now - crate::store::context::RETAIN_DAYS * 86_400;

    // Three sources, read whole. Bounded by the retention window rather than
    // paged: the sweep rebuilds every profile from scratch, and a rebuild is
    // only correct when it sees the whole window.
    let interactions = core.store.interactions_between(since, now).await?;
    let contexts = core.store.context_events_since(since).await?;
    // The bridge. `events_between` excludes the judge door already, which is
    // right here too: a benchmark run is not a situation anybody was in.
    let searches = core.store.events_between(since, now).await?;

    // (scope, artifact) -> the situations it was opened in.
    let mut by_pair: std::collections::BTreeMap<(String, String), Vec<Member>> =
        std::collections::BTreeMap::new();

    for i in &interactions {
        // `dwell` is not an open, and `recommended_shown` is not an
        // interaction at all — it is this feature's own offer, and counting it
        // would profile every artifact it ever guessed at.
        let self_made = match i.kind.as_str() {
            "opened" | "pivoted" => false,
            "recommended_open" => true,
            _ => continue,
        };
        let artifact_id = i.artifact_id.clone();
        let scope = i.scope.clone().unwrap_or_default();

        // Where the open followed a search, the situation is the search's, not
        // the open's: a recurring search resolves to the artifact it led to
        // rather than to a rerun of the query.
        let anchor = window(&searches, |s| s.created_at, i.at - BRIDGE_SECS, i.at)
            .iter()
            .filter(|s| {
                s.scope.as_deref().unwrap_or_default() == scope
                    && s.shown.iter().any(|(id, _)| id == &artifact_id)
            })
            .map(|s| s.created_at)
            .max()
            .unwrap_or(i.at);

        // The nearest recorded situation, within half an hour either way.
        let matched = window(
            &contexts,
            |c| c.at,
            anchor - MATCH_SECS,
            anchor + MATCH_SECS,
        )
        .iter()
        .filter(|c| c.scope.as_deref().unwrap_or_default() == scope)
        .min_by_key(|c| ((c.at - anchor).abs(), c.id));

        // No bundle is not no event. `at` and `scope` were recorded from the
        // beginning, and because an absent block is zeroed rather than
        // defaulted, an old row feeds in with no special handling — weekday and
        // hour contribute, device and network contribute nothing.
        let (raw, at) = match matched {
            Some(c) => (c.bundle.clone(), c.at),
            None => ("{}".to_string(), anchor),
        };
        let bundle = crate::core::context::parse_bundle(&raw);

        // Age decay, and the self-reinforcement guard as a multiplier. At
        // `self_weight = 0` a `recommended_open` produces a weightless member,
        // which the clusterer skips — so the first lucky guess cannot grow into
        // a habit the system taught itself.
        let age_days = ((now - at).max(0) as f64) / 86_400.0;
        let decayed = 0.5f64.powf(age_days / cfg.half_life_days.max(0.1));
        let weight = decayed * if self_made { cfg.self_weight } else { 1.0 };
        if weight <= 0.0 {
            continue;
        }

        by_pair
            .entry((scope.clone(), artifact_id))
            .or_default()
            .push(Member {
                vec: crate::core::context::encode(at, &bundle, &cfg.weights),
                weight,
                at,
                bundle: raw,
            });
        report.standing.events += 1;
    }

    // Slots are numbered per *artifact*, not per (scope, artifact): the
    // multivector is one array on one point, shared by every scope that has
    // opened this artifact, so a slot numbered per scope would have two owners
    // writing index 0.
    let mut per_artifact: std::collections::BTreeMap<String, Vec<(Option<String>, Cluster)>> =
        std::collections::BTreeMap::new();
    for ((scope, artifact_id), members) in by_pair {
        let scope = (!scope.is_empty()).then_some(scope);
        for c in agglomerate(
            &members,
            cfg.cluster_merge_at,
            cfg.max_clusters,
            cfg.min_weight,
        ) {
            per_artifact
                .entry(artifact_id.clone())
                .or_default()
                .push((scope.clone(), c));
        }
    }

    // Artifacts that had a profile and produced none this run. Their centroids
    // must go, or a pattern that stopped months ago would still be offered.
    let mut stale: std::collections::BTreeSet<String> = core
        .store
        .artifacts_with_context_clusters()
        .await?
        .into_iter()
        .collect();

    for (artifact_id, mut clusters) in per_artifact {
        stale.remove(&artifact_id);
        clusters.sort_by(|a, b| {
            b.1.weight
                .partial_cmp(&a.1.weight)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.1.last_at.cmp(&a.1.last_at))
        });
        clusters.truncate(MAX_SLOTS);

        let rows: Vec<crate::store::context::StoredCluster> = clusters
            .iter()
            .enumerate()
            .map(|(slot, (scope, c))| crate::store::context::StoredCluster {
                scope: scope.clone(),
                artifact_id: artifact_id.clone(),
                slot: slot as i64,
                centroid: c.centroid.clone(),
                weight: c.weight,
                events: c.events as i64,
                last_at: c.last_at,
                encoder_version: crate::core::context::encoder_version(&cfg.weights),
                representative: c.representative.clone(),
            })
            .collect();

        // SQLite first, then the vector store. If the second write fails the
        // table names clusters the index does not carry — which offers nothing
        // and explains nothing, and the next run repairs it. The other order
        // would offer an artifact with no reason to show for it, which is the
        // one failure this must not have.
        core.store
            .replace_context_clusters(&artifact_id, &rows)
            .await?;
        let vectors: Vec<Vec<f32>> = rows.iter().map(|r| r.centroid.clone()).collect();
        core.vectors
            .set_context_vectors(&artifact_id, vectors)
            .await?;

        report.standing.profiled += 1;
        report.standing.clusters += rows.len();
    }

    for artifact_id in stale {
        core.store
            .replace_context_clusters(&artifact_id, &[])
            .await?;
        core.vectors
            .set_context_vectors(&artifact_id, vec![])
            .await?;
        report.cleared += 1;
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(v: Vec<f32>, weight: f64, at: i64) -> Member {
        Member {
            vec: v,
            weight,
            at,
            bundle: format!(r#"{{"n":{at}}}"#),
        }
    }

    use crate::core::context::{Bundle, CTX_DIM, device_key, encoder_version, parse_bundle};
    use crate::core::test_support::test_core;

    /// 2026-08-21T13:52:00Z — a Friday, 15:52 in Berlin.
    const FRIDAY_SEVENTH: i64 = 1_787_320_320;

    /// A Friday at ~15:00 Berlin time, `weeks_back` weeks before the seventh.
    fn friday(weeks_back: i64) -> i64 {
        FRIDAY_SEVENTH - weeks_back * 7 * 86_400 - 52 * 60
    }

    /// A core with the recommender on and a clock that does not move.
    async fn recommending_core(now: i64) -> Core {
        let mut core = test_core().await;
        core.recommend.enabled = true;
        core.learn.enabled = true;
        core.clock = crate::core::context::Clock::Fixed(now);
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

    /// One artifact with a vector point behind it, because `context_query`
    /// renders from the payload and an artifact whose embedding never ran has
    /// none.
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

    /// One page view and the open that followed it.
    async fn seen_and_opened(core: &Core, aid: &str, at: i64, bundle: &Bundle, kind: &str) {
        let raw = serde_json::to_string(bundle).unwrap();
        let t = crate::core::context::local_time(at, bundle.tz.as_deref(), None);
        core.store
            .record_context(&crate::store::context::ContextEvent {
                id: 0,
                scope: Some("alice".into()),
                at,
                bundle: raw.clone(),
                device_key: device_key(&parse_bundle(&raw)),
                local_hour: Some(t.hour as f64),
                weekday: Some(t.weekday as i64),
                tz: bundle.tz.clone(),
            })
            .await
            .unwrap();
        core.store
            .record_interaction(aid, kind, None, Some("alice"), at + 5)
            .await
            .unwrap();
    }

    async fn seed_six_fridays(core: &Core, aid: &str) {
        for w in 1..=6 {
            seen_and_opened(core, aid, friday(w), &phone(), "opened").await;
        }
    }

    #[tokio::test]
    async fn six_fridays_become_one_stored_situation() {
        let core = recommending_core(FRIDAY_SEVENTH).await;
        let aid = seed_artifact(&core, "recycling centre").await;
        seed_six_fridays(&core, &aid).await;

        let r = run(&core).await.unwrap();
        assert_eq!(r.standing.events, 6);
        assert_eq!(r.standing.profiled, 1);
        assert_eq!(r.standing.clusters, 1);

        let stored = core
            .store
            .context_clusters_of(std::slice::from_ref(&aid))
            .await
            .unwrap();
        let c = &stored[&aid][0];
        assert_eq!(c.slot, 0);
        assert_eq!(c.centroid.len(), CTX_DIM);
        assert_eq!(c.encoder_version, encoder_version(&core.recommend.weights));
        assert_eq!(c.scope.as_deref(), Some("alice"));

        // And the vector store agrees, which is what the read path queries.
        let hits = core
            .vectors
            .context_query(&c.centroid, 5, &Default::default())
            .await
            .unwrap();
        assert_eq!(hits[0].payload.artifact_id, aid);
    }

    #[tokio::test]
    async fn an_open_of_something_this_offered_raises_no_weight() {
        // The named test the guard needs — the sitting has one saying it writes
        // no activation, for the same reason. Without this, the first lucky
        // guess grows into a habit the system taught itself.
        let core = recommending_core(FRIDAY_SEVENTH).await;
        let aid = seed_artifact(&core, "recycling centre").await;
        seed_six_fridays(&core, &aid).await;
        let before = run(&core).await.unwrap();
        let weight_before = core
            .store
            .context_clusters_of(std::slice::from_ref(&aid))
            .await
            .unwrap()[&aid][0]
            .weight;

        // Six more Fridays, every one of them an open of the offer.
        for w in 1..=6 {
            seen_and_opened(&core, &aid, friday(w) + 60, &phone(), "recommended_open").await;
        }

        let after = run(&core).await.unwrap();
        assert_eq!(
            after.standing.events, before.standing.events,
            "not one of them counted"
        );
        let weight_after = core
            .store
            .context_clusters_of(std::slice::from_ref(&aid))
            .await
            .unwrap()[&aid][0]
            .weight;
        assert!(
            (weight_after - weight_before).abs() < 1e-6,
            "weight moved from {weight_before} to {weight_after}"
        );
    }

    #[tokio::test]
    async fn a_pattern_that_stopped_is_cleared_rather_than_left_standing() {
        // A year of silence at a 45-day half-life is 2^-8 of the weight it had.
        let core = recommending_core(FRIDAY_SEVENTH).await;
        let aid = seed_artifact(&core, "recycling centre").await;
        seed_six_fridays(&core, &aid).await;
        run(&core).await.unwrap();
        assert!(
            !core
                .store
                .context_clusters_of(std::slice::from_ref(&aid))
                .await
                .unwrap()
                .is_empty()
        );

        // The same base, a year later.
        let mut later = core.clone();
        later.clock = crate::core::context::Clock::Fixed(FRIDAY_SEVENTH + 365 * 86_400);
        let r = run(&later).await.unwrap();
        assert_eq!(r.cleared, 1);
        assert_eq!(r.standing.profiled, 0);
        assert!(
            core.store
                .context_clusters_of(std::slice::from_ref(&aid))
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            later
                .vectors
                .context_query(&vec![0.1; CTX_DIM], 5, &Default::default())
                .await
                .unwrap()
                .is_empty(),
            "and the vector store was cleared too"
        );
    }

    #[tokio::test]
    async fn an_old_event_with_no_bundle_still_carries_a_weekday_and_an_hour() {
        // The cold start, which is not a backfill path: it is the ordinary
        // sweep reading older rows. Device and network contribute nothing
        // because an absent block is zeroed rather than defaulted.
        let core = recommending_core(FRIDAY_SEVENTH).await;
        let aid = seed_artifact(&core, "recycling centre").await;
        for w in 1..=6 {
            core.store
                .record_interaction(&aid, "opened", None, Some("alice"), friday(w))
                .await
                .unwrap();
        }
        let r = run(&core).await.unwrap();
        assert_eq!(
            r.standing.events, 6,
            "no context event, and they still count"
        );
        assert_eq!(r.standing.clusters, 1);
    }

    #[tokio::test]
    async fn two_people_get_two_profiles_on_one_artifact() {
        // Slots are numbered per artifact and not per (scope, artifact): the
        // multivector is one array on one point, so a slot numbered per scope
        // would have two owners writing index 0.
        let core = recommending_core(FRIDAY_SEVENTH).await;
        let aid = seed_artifact(&core, "shared").await;
        seed_six_fridays(&core, &aid).await;
        for w in 1..=6 {
            let at = friday(w) - 3 * 86_400;
            core.store
                .record_interaction(&aid, "opened", None, Some("bob"), at)
                .await
                .unwrap();
        }

        run(&core).await.unwrap();
        let stored = core
            .store
            .context_clusters_of(std::slice::from_ref(&aid))
            .await
            .unwrap();
        let mine = &stored[&aid];
        assert_eq!(mine.len(), 2, "one situation each");
        let slots: Vec<i64> = mine.iter().map(|c| c.slot).collect();
        assert_eq!(
            slots,
            vec![0, 1],
            "numbered per artifact, without collision"
        );
        let mut scopes: Vec<&str> = mine.iter().filter_map(|c| c.scope.as_deref()).collect();
        scopes.sort();
        assert_eq!(scopes, vec!["alice", "bob"]);
    }

    #[tokio::test]
    async fn the_sweep_does_not_run_when_the_faculty_is_off() {
        let mut core = recommending_core(FRIDAY_SEVENTH).await;
        core.recommend.enabled = false;
        let aid = seed_artifact(&core, "recycling centre").await;
        seed_six_fridays(&core, &aid).await;

        let r = run(&core).await.unwrap();
        assert_eq!(r.standing.events, 0);
        assert!(
            core.store
                .context_clusters_of(std::slice::from_ref(&aid))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn six_fridays_and_one_monday_leave_one_situation() {
        // The shape the whole feature is for. The outlier is real and
        // recorded; it is simply not yet a pattern, and `min_weight` is what
        // says so. Without it, one accident is a habit.
        let mut members: Vec<Member> = (0..6)
            .map(|i| member(vec![1.0, 0.02 * i as f32], 1.0, 1_000 + i))
            .collect();
        members.push(member(vec![0.0, 1.0], 1.0, 2_000));

        let out = agglomerate(&members, 0.82, 5, 2.0);
        assert_eq!(out.len(), 1, "the Monday is below the threshold");
        assert!((out[0].weight - 6.0).abs() < 1e-6);
        assert_eq!(out[0].last_at, 1_005, "the most recent member's stamp");
    }

    #[test]
    fn two_real_situations_are_two_clusters_not_their_mean() {
        // The recycling centre looked up on Friday afternoons *and*
        // occasionally on Monday mornings. A mean of them is a situation that
        // never happened, and it would match neither.
        let mut members: Vec<Member> = (0..4)
            .map(|i| member(vec![1.0, 0.0], 1.0, 1_000 + i))
            .collect();
        members.extend((0..4).map(|i| member(vec![0.0, 1.0], 1.0, 2_000 + i)));

        let out = agglomerate(&members, 0.82, 5, 2.0);
        assert_eq!(out.len(), 2);
        assert!(out.iter().any(|c| c.centroid[0] > 0.9));
        assert!(out.iter().any(|c| c.centroid[1] > 0.9));
    }

    #[test]
    fn the_same_input_clusters_the_same_way_twice() {
        // One pass and no randomness, because otherwise it is not testable —
        // and because a recommendation that changed its reason between two
        // sweeps over identical data would be unaccountable.
        let members: Vec<Member> = (0..12)
            .map(|i| {
                member(
                    vec![(i % 3) as f32, ((i + 1) % 3) as f32, 1.0],
                    1.0,
                    1_000 + i,
                )
            })
            .collect();
        let a = agglomerate(&members, 0.82, 4, 0.0);
        let b = agglomerate(&members, 0.82, 4, 0.0);
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(&b) {
            assert_eq!(x.centroid, y.centroid);
            assert_eq!(x.representative, y.representative);
        }
    }

    #[test]
    fn the_count_never_exceeds_the_cap() {
        let members: Vec<Member> = (0..20)
            .map(|i| {
                let mut v = vec![0.0; 20];
                v[i] = 1.0;
                member(v, 1.0, 1_000 + i as i64)
            })
            .collect();
        let out = agglomerate(&members, 0.99, 5, 0.0);
        assert!(out.len() <= 5, "got {}", out.len());
    }

    #[test]
    fn a_cluster_quotes_the_member_nearest_its_centre() {
        // What the display shows is a real event that happened, not a
        // reconstruction of the average of several.
        let members = vec![
            member(vec![1.0, 0.0], 1.0, 1_000),
            member(vec![0.98, 0.2], 1.0, 1_001),
            member(vec![0.99, 0.1], 1.0, 1_002),
        ];
        let out = agglomerate(&members, 0.5, 5, 0.0);
        assert_eq!(out.len(), 1);
        let rep: serde_json::Value = serde_json::from_str(&out[0].representative).unwrap();
        assert!(rep["at"].is_i64(), "the stamp travels with the bundle");
        assert!(rep["bundle"].is_object());
        // And it is one of the members, not something assembled.
        let at = rep["at"].as_i64().unwrap();
        assert!(members.iter().any(|m| m.at == at));
    }

    #[test]
    fn a_bundle_that_will_not_reparse_becomes_an_empty_one() {
        // The row must always be valid JSON: the display reads it back, and a
        // half-written bundle must not be able to break the page it explains.
        let mut m = member(vec![1.0, 0.0], 3.0, 1_000);
        m.bundle = "}{ not json".into();
        let out = agglomerate(&[m], 0.5, 5, 0.0);
        let rep: serde_json::Value = serde_json::from_str(&out[0].representative).unwrap();
        assert_eq!(rep["bundle"], serde_json::json!({}));
        assert_eq!(rep["at"], 1_000);
    }

    #[test]
    fn heavier_members_pull_the_centroid_further() {
        let members = vec![
            member(vec![1.0, 0.0], 1.0, 1_000),
            member(vec![0.9, 0.44], 9.0, 1_001),
        ];
        let out = agglomerate(&members, 0.5, 5, 0.0);
        assert_eq!(out.len(), 1);
        assert!(out[0].centroid[1] > 0.3, "the heavy one dominates");
    }

    #[test]
    fn a_weightless_member_is_skipped_entirely() {
        // What the self-reinforcement guard rides on: at `self_weight = 0` a
        // `recommended_open` arrives weighing nothing and must not open a
        // cluster of its own either.
        let members = vec![
            member(vec![1.0, 0.0], 3.0, 1_000),
            member(vec![0.0, 1.0], 0.0, 1_001),
        ];
        let out = agglomerate(&members, 0.82, 5, 0.0);
        assert_eq!(out.len(), 1);
        assert!(out[0].centroid[0] > 0.9);
    }

    #[test]
    fn nothing_in_means_nothing_out() {
        assert!(agglomerate(&[], 0.82, 5, 2.0).is_empty());
    }

    #[test]
    fn the_window_is_the_slice_and_nothing_either_side_of_it() {
        // The sweep asks each of three ordered logs the same question once per
        // interaction — what is near this moment — and scanning them whole made
        // the pass quadratic over a 400-day window. The answer has to be the
        // same slice a filter would have produced.
        let ats: Vec<i64> = vec![10, 20, 20, 30, 40, 50];
        let got = |lo, hi| -> Vec<i64> { window(&ats, |a| *a, lo, hi).to_vec() };
        assert_eq!(got(20, 40), vec![20, 20, 30, 40], "both bounds inclusive");
        assert_eq!(got(21, 39), vec![30]);
        assert_eq!(got(0, 100), ats);
        assert_eq!(got(51, 100), Vec::<i64>::new(), "past the end");
        assert_eq!(got(0, 9), Vec::<i64>::new(), "before the start");
        assert_eq!(got(41, 41), Vec::<i64>::new(), "a gap");
        // An empty log, and a window whose bounds have crossed — neither may
        // panic on a slice index.
        assert_eq!(window::<i64>(&[], |a| *a, 0, 10).len(), 0);
        assert_eq!(got(40, 20).len(), 0);
    }
}
