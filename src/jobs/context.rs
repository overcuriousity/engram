//! What situations recur for which artifact: one deterministic pass over the log, no model call.
//! A full rebuild from raw bundles every run, so an encoder change is a sweep, not a migration.

use crate::core::Core;
use crate::error::Result;
use crate::vector::cosine;

/// A constant, like `REPAIR_INTERVAL_HOURS`: the patterns learned are weekly, so six hours is fast
/// enough.
pub const INTERVAL_HOURS: u64 = 6;

/// An open this soon after a search inherits its identity. Deliberately not `pursuit.idle_secs`:
/// pursuits may be off.
pub const BRIDGE_SECS: i64 = 900;

/// Wider than `BRIDGE_SECS`: a person reads for a while after arriving.
pub const MATCH_SECS: i64 = 1800;

/// Bounds the one array every scope shares on an artifact's point; the heaviest survive.
pub const MAX_SLOTS: usize = 16;

#[derive(Debug, Clone)]
pub struct Member {
    pub vec: Vec<f32>,
    /// Already decayed, and multiplied by `self_weight` for an open of this feature's own offer.
    pub weight: f64,
    pub at: i64,
    pub bundle: String,
}

#[derive(Debug, Clone)]
pub struct Cluster {
    pub centroid: Vec<f32>,
    pub weight: f64,
    /// Undecayed: how often it happened, for the line under the offer.
    pub events: usize,
    pub last_at: i64,
    pub representative: String,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Report {
    pub standing: Standing,
    /// The one count here that is work: a profile that existed before this run and not after.
    pub cleared: usize,
}

/// Nested so `jobs::did_work` ignores them: a full recompute re-derives all three on every run.
/// Backing off is safe because `Core::record_context_event` arms this unit.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Standing {
    pub events: usize,
    pub profiled: usize,
    pub clusters: usize,
}

/// One pass, no randomness, so a recommendation's reason is reproducible. A cluster below
/// `min_weight` is dropped, so one accident never becomes a habit.
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
        /// Indices, so the representative is chosen against the final centroid.
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
            // Ties break on the index, so the choice never hangs on a float comparison.
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
                    // A bundle carries no time of its own. A bundle that will not re-parse becomes
                    // an empty object.
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
    // Heaviest first: the order slots are allocated in, and `MAX_SLOTS` keeps the front.
    out.sort_by(|a, b| {
        b.weight
            .partial_cmp(&a.weight)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.last_at.cmp(&a.last_at))
    });
    out
}

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

/// Quadratic over at most `max_clusters + 1` vectors of 45 dimensions: cheap.
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

/// The sources arrive `ORDER BY at, id`; a linear scan per interaction made the pass quadratic over
/// a year of logs.
fn window<T>(v: &[T], at: impl Fn(&T) -> i64, lo: i64, hi: i64) -> &[T] {
    let start = v.partition_point(|x| at(x) < lo);
    let end = v.partition_point(|x| at(x) <= hi);
    &v[start..end.max(start)]
}

pub async fn run(core: &Core) -> Result<Report> {
    let mut report = Report::default();
    if !core.recommends() {
        return Ok(report);
    }
    let cfg = &core.recommend;
    let now = core.clock.now();
    let since = now - crate::store::context::RETAIN_DAYS * 86_400;

    // Read whole within the retention window: a rebuild is only correct over the whole window.
    let interactions = core.store.interactions_between(since, now).await?;
    let contexts = core.store.context_events_since(since).await?;
    // `events_between` excludes the judge door: a benchmark run is no one's situation.
    let searches = core.store.events_between(since, now).await?;

    let mut by_pair: std::collections::BTreeMap<(String, String), Vec<Member>> =
        std::collections::BTreeMap::new();

    for i in &interactions {
        // `recommended_shown` is this feature's own offer; counting it would profile every guess.
        let self_made = match i.kind.as_str() {
            "opened" | "pivoted" => false,
            "recommended_open" => true,
            _ => continue,
        };
        let artifact_id = i.artifact_id.clone();
        let scope = i.scope.clone().unwrap_or_default();

        let anchor = window(&searches, |s| s.created_at, i.at - BRIDGE_SECS, i.at)
            .iter()
            .filter(|s| {
                s.scope.as_deref().unwrap_or_default() == scope
                    && s.shown.iter().any(|(id, _)| id == &artifact_id)
            })
            .map(|s| s.created_at)
            .max()
            .unwrap_or(i.at);

        let matched = window(
            &contexts,
            |c| c.at,
            anchor - MATCH_SECS,
            anchor + MATCH_SECS,
        )
        .iter()
        .filter(|c| c.scope.as_deref().unwrap_or_default() == scope)
        .min_by_key(|c| ((c.at - anchor).abs(), c.id));

        // No bundle is not no event: an absent block is zeroed, so old rows feed in with weekday
        // and hour only.
        let (raw, at) = match matched {
            Some(c) => (c.bundle.clone(), c.at),
            None => ("{}".to_string(), anchor),
        };
        let bundle = crate::core::context::parse_bundle(&raw);

        // At `self_weight = 0` an open of the offer weighs nothing, so a lucky guess cannot grow
        // into a habit.
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

    // Numbered per artifact: the multivector is one array on one point shared by every scope.
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

    // A pattern that stopped months ago must not still be offered.
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

        // SQLite first: a failed vector write then offers nothing and the next run repairs it; the
        // other order offers an artifact with no reason to show.
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
    use crate::core::test_support::{phone_bundle, recommending_core, seed_artifact};

    const FRIDAY_SEVENTH: i64 = 1_787_320_320;

    fn friday(weeks_back: i64) -> i64 {
        FRIDAY_SEVENTH - weeks_back * 7 * 86_400 - 52 * 60
    }

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
            seen_and_opened(core, aid, friday(w), &phone_bundle(), "opened").await;
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

        let hits = core
            .vectors
            .context_query(&c.centroid, 5, &Default::default())
            .await
            .unwrap();
        assert_eq!(hits[0].payload.artifact_id, aid);
    }

    #[tokio::test]
    async fn an_open_of_something_this_offered_raises_no_weight() {
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

        for w in 1..=6 {
            seen_and_opened(
                &core,
                &aid,
                friday(w) + 60,
                &phone_bundle(),
                "recommended_open",
            )
            .await;
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
                .context_query(&[0.1; CTX_DIM], 5, &Default::default())
                .await
                .unwrap()
                .is_empty(),
            "and the vector store was cleared too"
        );
    }

    #[tokio::test]
    async fn an_old_event_with_no_bundle_still_carries_a_weekday_and_an_hour() {
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
        // A mean of the two situations would match neither.
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
        let at = rep["at"].as_i64().unwrap();
        assert!(members.iter().any(|m| m.at == at));
    }

    #[test]
    fn a_bundle_that_will_not_reparse_becomes_an_empty_one() {
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
        let ats: Vec<i64> = vec![10, 20, 20, 30, 40, 50];
        let got = |lo, hi| -> Vec<i64> { window(&ats, |a| *a, lo, hi).to_vec() };
        assert_eq!(got(20, 40), vec![20, 20, 30, 40], "both bounds inclusive");
        assert_eq!(got(21, 39), vec![30]);
        assert_eq!(got(0, 100), ats);
        assert_eq!(got(51, 100), Vec::<i64>::new(), "past the end");
        assert_eq!(got(0, 9), Vec::<i64>::new(), "before the start");
        assert_eq!(got(41, 41), Vec::<i64>::new(), "a gap");
        assert_eq!(window::<i64>(&[], |a| *a, 0, 10).len(), 0);
        assert_eq!(got(40, 20).len(), 0);
    }
}
