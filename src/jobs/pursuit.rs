//! Pursuits: a coherent run of searches, what was engaged with, and the one artifact an unmet need
//! earns.
//! Local decides, the model only writes: `run` groups and scores, `generate` makes the one call.

use crate::core::Core;
use crate::error::Result;
use crate::infer::prompt;
use crate::store::artifacts::NewSynthesized;
use crate::store::jobs::Stage;

/// One call, superseding nothing. Idempotent: a retry after a crash between insert and update must
/// not write twice.
pub async fn generate(core: &Core, pursuit_id: &str) -> Result<()> {
    let p = core.store.get_pursuit(pursuit_id).await?;
    if p.state != "open" || p.artifact_id.is_some() {
        return Ok(());
    }
    let now = crate::store::now();
    // Closed with a reason: a unit queued before the generator was removed must not leave an `open`
    // row nobody can act on.
    let Some(generator) = core.generator.clone() else {
        core.store
            .close_pursuit(pursuit_id, "unsatisfied", "no generator configured", now)
            .await?;
        return Ok(());
    };

    // Engaged artifacts resolved to their captured roots: consolidating consolidations drifts
    // off-subject while every literal checks out.
    let roots = core.store.roots_of(&p.sources).await?;
    let mut ids: Vec<String> = Vec::new();
    for id in &p.sources {
        for r in roots.get(id).into_iter().flatten() {
            if !ids.contains(r) {
                ids.push(r.clone());
            }
        }
    }
    let rows = core.store.artifacts_by_ids(&ids).await?;
    let mut sources: Vec<crate::store::artifacts::Chunk> = ids
        .iter()
        .filter_map(|id| rows.iter().find(|c| &c.id == id).cloned())
        .filter(|c| c.in_results())
        .collect();
    if sources.len() < core.pursuit.min_sources {
        core.store
            .close_pursuit(
                pursuit_id,
                "unsatisfied",
                "sources gone before generation",
                now,
            )
            .await?;
        return Ok(());
    }

    // The operator often answers the question themselves before the sweep runs, so ask the base
    // once more with the stored vector, excluding the engaged sources. A failed probe is left to
    // the generation below.
    if let Some(found) = answered_since(core, &p).await? {
        core.store
            .close_pursuit(
                pursuit_id,
                "satisfied",
                &format!("the base gained {found} after the searching stopped"),
                now,
            )
            .await?;
        return Ok(());
    }

    // Packed like the dedupe judge: questions and system prompt always go; sources drop from the
    // tail.
    let window = generator.context_tokens();
    let ceiling = generator.max_output_tokens();
    let system = core.counter.count(prompt::GENERATE_SYSTEM);
    let user = loop {
        let excerpts: Vec<(String, String)> = sources
            .iter()
            .map(|c| (c.title.clone().unwrap_or_default(), c.text.clone()))
            .collect();
        let user = prompt::generate_prompt(&p.queries, &excerpts);
        let spent = system + core.counter.count(&user);
        if spent + ceiling.min(window / 2) <= window {
            break user;
        }
        // At `min_sources` and still over budget, retrying only spends `MAX_ATTEMPTS` calls on
        // identical content, so close with the reason.
        if sources.len() <= core.pursuit.min_sources {
            core.store
                .close_pursuit(
                    pursuit_id,
                    "unsatisfied",
                    "the fewest sources worth generating from do not fit one call",
                    now,
                )
                .await?;
            return Ok(());
        }
        sources.pop();
    };
    let source_text: String = sources
        .iter()
        .map(|c| c.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");

    let permit = core.gate.background().await;
    let reply = generator.complete(prompt::GENERATE_SYSTEM, &user).await;
    permit.finished();
    let g = prompt::parse_generation(&reply?)?;

    // An abstention is not an artifact: stored, it would be handed back to the next search for
    // those words. The pursuit closes unsatisfied, a gap.
    if prompt::abstained(&g.text) {
        core.store
            .close_pursuit(
                pursuit_id,
                "unsatisfied",
                "the excerpts did not bear on the questions",
                now,
            )
            .await?;
        tracing::info!(
            pursuit = pursuit_id,
            "the generation abstained; nothing written up"
        );
        return Ok(());
    }

    let ids: Vec<String> = sources.iter().map(|c| c.id.clone()).collect();
    let made = core
        .store
        .insert_synthesized_artifact(
            &NewSynthesized {
                text: g.text,
                title: Some(g.title),
                category: g.category,
                tags: g.tags,
                caveats: g.caveats,
                cues: p.queries.clone(),
            },
            &ids,
        )
        .await?;
    // Drift is flagged, not prevented: a literal no source carries is marked for the reader.
    let missing = crate::infer::verify::missing_literals(&made.text, &made.caveats, &source_text);
    if let Some(first) = missing.first() {
        core.store
            .set_artifact_flags(
                &made.id,
                &[crate::infer::verify::FLAG_LITERALS.to_string()],
                Some(&format!("missing literal: {first}")),
            )
            .await?;
    }
    core.store
        .enqueue(Stage::Embed, "artifact", &made.id)
        .await?;
    core.store
        .set_pursuit_artifact(pursuit_id, &made.id, now)
        .await?;
    tracing::info!(pursuit = pursuit_id, artifact_id = %made.id, sources = ids.len(), "generated an artifact from a pursuit");
    Ok(())
}

/// The engaged sources can lead the list and are all skipped, so a probe of `sources.len()` could
/// leave nothing to judge.
const ANSWER_PROBE_EXTRA: usize = 3;

/// An unseen artifact created after the pursuit opened that now answers it at or above
/// `weak_below`. `None` across embedding models: their cosines are not distances.
async fn answered_since(
    core: &Core,
    p: &crate::store::pursuits::Pursuit,
) -> Result<Option<String>> {
    let Some((vec, model)) = core.store.pursuit_vec(&p.id).await? else {
        return Ok(None);
    };
    if model != core.embedder.model() {
        return Ok(None);
    }
    let k = p.sources.len() + ANSWER_PROBE_EXTRA;
    let hits = core
        .vectors
        .search(&vec, &Default::default(), k, &Default::default())
        .await?;
    let fresh: Vec<String> = hits
        .into_iter()
        // `None` is "no opinion", not a low value; closing is a claim about distance, as in
        // `gaps::cover`.
        .filter(|h| h.similarity.is_some_and(|s| s >= core.weak_below()))
        .map(|h| h.payload.artifact_id)
        .filter(|id| !p.sources.contains(id))
        .collect();
    if fresh.is_empty() {
        return Ok(None);
    }
    Ok(core
        .store
        .artifacts_by_ids(&fresh)
        .await?
        .into_iter()
        .find(|c| c.created_at > p.opened_at && c.in_results())
        .map(|c| c.id))
}

pub const PURSUIT_AFTER: &str = "pursuit.events_after";

/// Stops the first sweep after `[learn]` is enabled, with no cursor, from reading every search
/// since install. A day, because a pursuit is a sitting.
const MAX_LOOKBACK_SECS: i64 = 24 * 60 * 60;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Need {
    pub queries: Vec<String>,
    pub opened_at: i64,
    pub last_at: i64,
    pub answered: bool,
    pub abstained: bool,
    pub engagement: Vec<(String, f64)>,
    /// Dwell weight per artifact, at most 0.5 each: a tiebreak for prompt order, never part of the
    /// decision.
    pub dwell: Vec<(String, f64)>,
    pub pivots: usize,
    pub returns: usize,
    pub refined: usize,
    pub abandoned: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Satisfied(String),
    Unsatisfied(String),
    Generate,
}

/// Everything in `engagement` was reached from a hit at or above the weak line, so "engaged" means
/// "engaged something strong".
pub fn unsatisfied(n: &Need) -> bool {
    n.engagement.is_empty() || n.refined >= 2 || n.abandoned || n.abstained
}

/// The analysis pass, in the spec's order; every input is in `Need`.
pub fn decide(n: &Need, min_sources: usize, min_engagement: f64) -> Decision {
    if n.answered {
        return Decision::Satisfied("a synthesized artifact led the list".into());
    }
    let total: f64 = n.engagement.iter().map(|(_, w)| *w).sum();
    let unsatisfied = unsatisfied(n);
    if n.engagement.len() < min_sources {
        return if unsatisfied {
            // Reachable with artifacts engaged above the shipped `min_sources = 2`.
            Decision::Unsatisfied(if n.engagement.is_empty() {
                "nothing engaged".into()
            } else {
                format!("{} engaged, below min_sources", n.engagement.len())
            })
        } else {
            Decision::Satisfied("nothing to assemble".into())
        };
    }
    let assembled = total >= min_engagement;
    let wanted = n.pivots + n.returns > 0;
    if assembled && (unsatisfied || wanted) {
        return Decision::Generate;
    }
    if unsatisfied {
        Decision::Unsatisfied(format!("engaged {total:.1} below min_engagement"))
    } else {
        Decision::Satisfied("a strong hit was opened and searching stopped".into())
    }
}

struct Ev {
    /// Search only: a capture's link names this row; read by `cover_typed_from`.
    id: Option<String>,
    at: i64,
    scope: Option<String>,
    query: String,
    vec: Vec<f32>,
    answered: bool,
    confirmed: Option<String>,
    shown: Vec<(String, Option<f32>)>,
    is_ask: bool,
    abstained: bool,
    /// What the answer referenced, never everything the ask packed (`RecordedAsk::cited`).
    cited: Vec<String>,
}

/// Runs only once everything unprocessed has been idle for `idle_secs`: a pursuit is something that
/// ended.
pub async fn run(core: &Core) -> Result<usize> {
    if !core.associating() {
        return Ok(0);
    }
    let now = crate::store::now();
    let idle = core.pursuit.idle_secs as i64;
    let after: i64 = core
        .store
        .meta_get(PURSUIT_AFTER)
        .await?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let after = after.max(now - MAX_LOOKBACK_SECS);
    if let Some(newest) = core.store.newest_event_at().await?
        && newest > after
        && now - newest < idle
    {
        return Ok(0);
    }
    let searches = core.store.events_between(after, now).await?;
    let asks = core.store.asks_between(after, now).await?;
    if searches.is_empty() && asks.is_empty() {
        return Ok(0);
    }
    let interactions = core.store.interactions_between(after, now).await?;

    let mut evs: Vec<Ev> = searches
        .into_iter()
        .map(|e| Ev {
            id: Some(e.id),
            at: e.created_at,
            scope: e.scope,
            query: e.query,
            vec: e.query_vec,
            answered: e.answered,
            confirmed: e.confirmed,
            shown: e.shown,
            is_ask: false,
            abstained: false,
            cited: vec![],
        })
        .chain(asks.into_iter().map(|a| Ev {
            id: None,
            at: a.created_at,
            scope: a.scope,
            query: a.question,
            vec: a.query_vec,
            answered: false,
            confirmed: None,
            shown: vec![],
            is_ask: true,
            abstained: a.abstained,
            cited: a.cited,
        }))
        .filter(|e| !e.vec.is_empty())
        .collect();
    evs.sort_by_key(|e| e.at);

    // An interaction belongs to the latest prior event in scope, within the idle window, that
    // reached its artifact; never to an answered search, never by the clock alone.
    let weak = core.weak_below();
    let mut reach: Vec<std::collections::HashSet<String>> = evs
        .iter()
        .map(|e| {
            let mut r: std::collections::HashSet<String> = e
                .shown
                .iter()
                .filter(|(_, sim)| sim.is_none_or(|s| s >= weak))
                .map(|(id, _)| id.clone())
                .collect();
            r.extend(e.confirmed.iter().cloned());
            r.extend(e.cited.iter().cloned());
            r
        })
        .collect();
    let mut attached: Vec<Vec<usize>> = vec![Vec::new(); evs.len()];
    for (k, i) in interactions.iter().enumerate() {
        let owner = evs
            .iter()
            .enumerate()
            .filter(|(_, e)| e.at <= i.at && i.at - e.at <= idle && e.scope == i.scope)
            .filter(|(idx, _)| {
                reach[*idx].contains(&i.artifact_id)
                    || i.via.as_ref().is_some_and(|v| reach[*idx].contains(v))
            })
            .max_by_key(|(_, e)| e.at)
            .map(|(idx, _)| idx);
        if let Some(idx) = owner {
            reach[idx].insert(i.artifact_id.clone());
            attached[idx].push(k);
        }
    }

    // Anchored on the lead query, not linked transitively, so a bridge cannot pull in an unrelated
    // topic; the idle gap makes each group a sitting.
    let line = core.link_at();
    let mut clusters: Vec<Vec<usize>> = Vec::new();
    for m in 0..evs.len() {
        let best = clusters
            .iter_mut()
            .filter(|c| c.last().is_some_and(|l| evs[m].at - evs[*l].at <= idle))
            .map(|c| (crate::core::gaps::cosine(&evs[c[0]].vec, &evs[m].vec), c))
            .filter(|(sim, _)| *sim >= line)
            .max_by(|a, b| a.0.total_cmp(&b.0));
        match best {
            Some((_, c)) => c.push(m),
            None => clusters.push(vec![m]),
        }
    }

    let mut written = 0;
    for members in clusters {
        let need = need_of(&evs, &members, &attached, &interactions);
        // Engagement first, dwell breaking ties.
        let mut ranked: Vec<(String, f64)> = need
            .engagement
            .iter()
            .map(|(id, w)| {
                let d = need
                    .dwell
                    .iter()
                    .find(|(x, _)| x == id)
                    .map(|(_, v)| *v)
                    .unwrap_or(0.0);
                (id.clone(), w + d)
            })
            .collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        let sources: Vec<String> = ranked.into_iter().map(|(id, _)| id).collect();
        let decision = decide(&need, core.pursuit.min_sources, core.pursuit.min_engagement);
        // The lead event's vector goes on the row: a gap is a question plus the vector it was found
        // by.
        let lead = members.first().map(|&m| &evs[m]);
        let pid = core
            .store
            .insert_pursuit(
                need.opened_at,
                &need.queries,
                &sources,
                lead.map(|e| (e.vec.as_slice(), core.embedder.model())),
            )
            .await?;
        // Keyed on the cluster: only an `open` pursuit is undecided. Deciding a closed one twice
        // would arm a second generation or reopen a dismissal.
        if core.store.get_pursuit(&pid).await?.state != "open" {
            continue;
        }
        written += 1;
        match decision {
            Decision::Satisfied(why) => {
                core.store
                    .close_pursuit(&pid, "satisfied", &why, now)
                    .await?;
            }
            Decision::Unsatisfied(why) => {
                core.store
                    .close_pursuit(&pid, "unsatisfied", &why, now)
                    .await?;
                cover_typed_from(core, &evs, &members).await;
            }
            Decision::Generate => {
                if let Some(covering) = covered_by_existing(core, &sources).await? {
                    core.store
                        .close_pursuit(&pid, "satisfied", &format!("covered by {covering}"), now)
                        .await?;
                } else {
                    core.store
                        .rearm_idle_seq(Stage::Generate, "pursuit", &pid, 0)
                        .await?;
                }
            }
        }
    }
    core.store.meta_set(PURSUIT_AFTER, &now.to_string()).await?;
    tracing::info!(pursuits = written, line, "pursuit sweep");
    Ok(written)
}

/// `gaps::cover` runs at capture, before the sweep makes this pursuit a gap, so this is its second
/// call for captures typed from these searches. Best-effort: the pursuit is already closed, and a
/// failed check must not leave other clusters undecided.
async fn cover_typed_from(core: &Core, evs: &[Ev], members: &[usize]) {
    let ids: Vec<String> = members.iter().filter_map(|&m| evs[m].id.clone()).collect();
    let typed = match core.store.captures_typed_from(&ids).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "could not look for a capture typed from this pursuit");
            return;
        }
    };
    for (corpus_id, event_id) in typed {
        match crate::jobs::gaps::cover_answering(core, &corpus_id, Some(event_id)).await {
            Ok(n) if n > 0 => {
                tracing::info!(
                    corpus_id,
                    closed = n,
                    "a capture answered the pursuit it was typed from"
                );
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(corpus_id, error = %e, "could not record that a capture answered a pursuit");
            }
        }
    }
}

fn need_of(
    evs: &[Ev],
    members: &[usize],
    attached: &[Vec<usize>],
    interactions: &[crate::store::pursuits::Interaction],
) -> Need {
    use crate::store::links::normalize_query;
    let mut n = Need {
        opened_at: evs[members[0]].at,
        last_at: members.last().map_or(evs[members[0]].at, |l| evs[*l].at),
        ..Default::default()
    };
    let mut seen_q: std::collections::HashSet<String> = Default::default();
    let mut order: Vec<String> = Vec::new();
    let mut weight: std::collections::HashMap<String, f64> = Default::default();
    let mut bump = |id: &str, w: f64| {
        if !weight.contains_key(id) {
            order.push(id.to_string());
        }
        *weight.entry(id.to_string()).or_insert(0.0) += w;
    };
    let mut touched: Vec<String> = Vec::new();
    for (pos, &m) in members.iter().enumerate() {
        let e = &evs[m];
        if seen_q.insert(normalize_query(&e.query)) {
            n.queries.push(e.query.clone());
        }
        n.answered |= e.answered;
        n.abstained |= e.abstained;
        if e.is_ask {
            // An abstention arrives with no citations, so it is not both the evidence of a need and
            // its engagement.
            for id in &e.cited {
                bump(id, 1.0);
            }
            continue;
        }
        if let Some(id) = &e.confirmed {
            bump(id, 3.0);
        }
        let mine: Vec<&crate::store::pursuits::Interaction> = attached[m]
            .iter()
            .map(|&k| &interactions[k])
            // What the base offered is not something a person did; every other kind weighs 1.0.
            .filter(|i| i.kind != "recommended_shown")
            .collect();
        if !e.answered {
            for i in &mine {
                if i.kind == "dwell" {
                    // At most 0.5 per artifact: a tenth per minute, capped.
                    let secs: f64 = i
                        .detail
                        .as_deref()
                        .and_then(|d| d.parse().ok())
                        .unwrap_or(0.0);
                    let w = (secs / 600.0).min(0.5);
                    match n.dwell.iter_mut().find(|(x, _)| x == &i.artifact_id) {
                        Some(d) => d.1 = (d.1 + w).min(0.5),
                        None => n.dwell.push((i.artifact_id.clone(), w)),
                    }
                    continue;
                }
                let w = if i.kind == "pivoted" {
                    n.pivots += 1;
                    1.5
                } else {
                    1.0
                };
                bump(&i.artifact_id, w);
                // Came back to it after something else: hard to fake.
                if touched.last().is_some_and(|t| t != &i.artifact_id)
                    && touched.contains(&i.artifact_id)
                {
                    bump(&i.artifact_id, 2.0);
                    n.returns += 1;
                }
                touched.push(i.artifact_id.clone());
            }
        }
        let followed = pos + 1 < members.len();
        // Dwell alone is not engagement.
        let engaged = mine.iter().any(|i| i.kind != "dwell");
        if !engaged && e.confirmed.is_none() && !e.answered {
            if followed {
                n.refined += 1;
            } else {
                n.abandoned = true;
            }
        }
    }
    n.engagement = order
        .into_iter()
        .map(|id| (weight[&id], id))
        .map(|(w, id)| (id, w))
        .collect();
    n
}

/// Are these sources' roots all inside an existing active generation's? Then the need is already
/// written up.
async fn covered_by_existing(core: &Core, sources: &[String]) -> Result<Option<String>> {
    let roots = core.store.roots_of(sources).await?;
    let mine: std::collections::BTreeSet<String> = roots.values().flatten().cloned().collect();
    if mine.is_empty() {
        return Ok(None);
    }
    // One lineage read for the whole set rather than one per candidate.
    let generated = core.store.synthesized_artifacts(200).await?;
    let ids: Vec<String> = generated.iter().map(|g| g.id.clone()).collect();
    let theirs = core.store.roots_of(&ids).await?;
    for g in &generated {
        let set: std::collections::BTreeSet<&String> =
            theirs.get(&g.id).into_iter().flatten().collect();
        if mine.iter().all(|r| set.contains(r)) {
            return Ok(Some(g.id.clone()));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_support::test_core;
    use crate::store::artifacts::{NewArtifact, Provenance};

    #[test]
    fn a_generation_reply_parses_and_an_empty_one_does_not() {
        let g = prompt::parse_generation(
            r#"{"artifact":{"title":"T","text":"run `mount -o ro`","category":"procedure","tags":["x"],"caveats":["read-only"]}}"#,
        )
        .unwrap();
        assert_eq!(g.title, "T");
        assert_eq!(g.caveats, vec!["read-only".to_string()]);
        assert!(prompt::parse_generation(r#"{"artifact":{"title":"T","text":"  "}}"#).is_err());
        assert!(prompt::parse_generation("not json").is_err());
    }

    async fn two_sources(core: &crate::core::Core) -> Vec<String> {
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let na = |o: i64, t: &str| NewArtifact {
            ordinal: o,
            text: t.into(),
            title: Some(format!("S{o}")),
            ..Default::default()
        };
        core.store
            .insert_artifacts(
                &src.id,
                &[
                    na(0, "mount the image with `mount -o ro,loop`"),
                    na(1, "then read the journal at /var/log/journal"),
                ],
            )
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect()
    }

    #[tokio::test]
    async fn a_pursuit_is_written_up_once_with_cues_lineage_and_an_embed() {
        let mut core = test_core().await;
        core.generator = Some(std::sync::Arc::new(
            crate::infer::fake::ScriptedCompleter::new(vec![
                r#"{"artifact":{"title":"Reading a journal","text":"Mount with `mount -o ro,loop`, then read /var/log/journal.","category":"procedure","tags":[],"caveats":[]}}"#.into(),
            ]),
        ));
        let ids = two_sources(&core).await;
        let pid = core
            .store
            .insert_pursuit(100, &["how do I read the journal".into()], &ids, None)
            .await
            .unwrap();

        generate(&core, &pid).await.unwrap();
        generate(&core, &pid).await.unwrap();

        let made = core.store.synthesized_artifacts(10).await.unwrap();
        assert_eq!(made.len(), 1, "generated twice");
        let g = &made[0];
        assert_eq!(g.provenance, Provenance::Synthesized);
        assert_eq!(g.cues, vec!["how do I read the journal".to_string()]);
        assert!(g.flags.is_empty(), "{:?}", g.flags);
        let roots = core
            .store
            .roots_of(std::slice::from_ref(&g.id))
            .await
            .unwrap();
        let mut got = roots[&g.id].clone();
        got.sort();
        let mut want = ids.clone();
        want.sort();
        assert_eq!(got, want);
        assert!(core.store.live_job(Stage::Embed, &g.id).await.unwrap());
        let p = core.store.get_pursuit(&pid).await.unwrap();
        assert_eq!(p.state, "generated");
        assert_eq!(p.artifact_id.as_deref(), Some(g.id.as_str()));
        for id in &ids {
            assert!(core.store.get_artifact(id).await.unwrap().in_results());
        }
    }

    #[tokio::test]
    async fn a_pursuit_with_no_generator_closes_unsatisfied_rather_than_sitting_open() {
        let mut core = test_core().await;
        // `[learn]` on, `[infer.generate]` absent: a working config with the model taken out.
        core.generator = None;
        let ids = two_sources(&core).await;
        let pid = core
            .store
            .insert_pursuit(100, &["how do I read the journal".into()], &ids, None)
            .await
            .unwrap();

        generate(&core, &pid).await.unwrap();

        let p = core.store.get_pursuit(&pid).await.unwrap();
        assert_eq!(p.state, "unsatisfied", "closed, and not left open");
        assert_eq!(p.reason.as_deref(), Some("no generator configured"));
        assert!(p.artifact_id.is_none());
        assert!(
            core.store
                .synthesized_artifacts(10)
                .await
                .unwrap()
                .is_empty(),
            "and nothing was written"
        );
    }

    #[tokio::test]
    async fn a_synthesized_source_contributes_its_roots_not_its_text() {
        let mut core = test_core().await;
        let scripted = std::sync::Arc::new(crate::infer::fake::ScriptedCompleter::new(vec![
            r#"{"artifact":{"title":"T","text":"Mount with `mount -o ro,loop`.","category":"procedure","tags":[],"caveats":[]}}"#.into(),
        ]));
        core.generator = Some(scripted.clone());
        let ids = two_sources(&core).await;
        let earlier = core
            .store
            .insert_synthesized_artifact(
                &NewSynthesized {
                    text: "an earlier write-up".into(),
                    title: Some("Earlier".into()),
                    category: None,
                    tags: vec![],
                    caveats: vec![],
                    cues: vec![],
                },
                &ids,
            )
            .await
            .unwrap();
        let pid = core
            .store
            .insert_pursuit(100, &["q".into()], std::slice::from_ref(&earlier.id), None)
            .await
            .unwrap();
        generate(&core, &pid).await.unwrap();
        let made = core.store.synthesized_artifacts(10).await.unwrap();
        let g = made.iter().find(|g| g.id != earlier.id).expect("generated");
        let roots = core
            .store
            .roots_of(std::slice::from_ref(&g.id))
            .await
            .unwrap();
        let mut got = roots[&g.id].clone();
        got.sort();
        let mut want = ids.clone();
        want.sort();
        assert_eq!(got, want, "the roots, not the synthesis");
        assert!(!got.contains(&earlier.id));
        let prompt = scripted.prompts().remove(0);
        assert!(!prompt.contains("an earlier write-up"), "{prompt}");
        assert!(prompt.contains("S0") && prompt.contains("S1"), "{prompt}");
    }

    #[tokio::test]
    async fn a_generation_that_abstains_writes_nothing_and_closes_unsatisfied() {
        let mut core = test_core().await;
        core.generator = Some(std::sync::Arc::new(
            crate::infer::fake::ScriptedCompleter::new(vec![format!(
                r#"{{"artifact":{{"title":"Mann-Whitney U Test","text":"{}. The excerpts are about lead isotopes.","category":"concept","tags":[],"caveats":[]}}}}"#,
                prompt::ABSTAIN_PREFIX
            )]),
        ));
        let ids = two_sources(&core).await;
        let pid = core
            .store
            .insert_pursuit(100, &["mann whitney u".into()], &ids, None)
            .await
            .unwrap();

        generate(&core, &pid).await.unwrap();

        assert!(
            core.store
                .synthesized_artifacts(10)
                .await
                .unwrap()
                .is_empty(),
            "a refusal was stored as an artifact"
        );
        let p = core.store.get_pursuit(&pid).await.unwrap();
        assert_eq!(p.state, "unsatisfied", "{p:?}");
        assert!(p.artifact_id.is_none(), "{p:?}");
    }

    #[tokio::test]
    async fn an_answer_captured_after_the_searching_closes_the_pursuit_satisfied() {
        let mut core = test_core().await;
        core.generator = Some(std::sync::Arc::new(
            crate::infer::fake::ScriptedCompleter::new(vec![
                r#"{"artifact":{"title":"T","text":"the model must never be asked","category":"concept","tags":[],"caveats":[]}}"#.into(),
            ]),
        ));
        let ids = two_sources(&core).await;
        let v = core.embedder.embed_query("read the journal").await.unwrap();
        let opened_at = crate::store::now() - 100;
        let pid = core
            .store
            .insert_pursuit(
                opened_at,
                &["read the journal".into()],
                &ids,
                Some((&v, core.embedder.model())),
            )
            .await
            .unwrap();
        let src = core
            .ingest("read the journal at /var/log/journal", "web", None)
            .await
            .unwrap();
        while crate::jobs::run_one(&core).await.unwrap() {}
        core.background.wait_idle().await;
        let at = core
            .vectors
            .search(
                &v,
                &Default::default(),
                1,
                &crate::vector::SearchFilter {
                    corpus_id: Some(src.id.clone()),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .first()
            .and_then(|h| h.similarity)
            .expect("the capture embedded nothing")
            - 0.01;
        core.set_weak_below(at);

        generate(&core, &pid).await.unwrap();

        assert!(
            core.store
                .synthesized_artifacts(10)
                .await
                .unwrap()
                .is_empty(),
            "wrote up a need the base had since answered"
        );
        let p = core.store.get_pursuit(&pid).await.unwrap();
        assert_eq!(p.state, "satisfied", "{p:?}");
    }

    #[tokio::test]
    async fn a_pursuit_swept_up_after_the_capture_is_still_closed_by_it() {
        let mut core = pursuing_core().await;
        let q = "Netcat can serve a file to one client, then exits";
        let v = core.embedder.embed_query(q).await.unwrap();
        let at = crate::store::now() - 100;
        let eid = core
            .store
            .record_search(
                crate::store::feedback::NewEvent {
                    fold_onto: None,
                    query: q.into(),
                    door: crate::store::feedback::Door::Ui,
                    scope: Some("me".into()),
                    filters: "{}".into(),
                    query_vec: v.clone(),
                    embed_model: core.embedder.model().to_string(),
                    candidates: vec![],
                    answered: false,
                    context: None,
                },
                0,
            )
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE search_events SET created_at = ? WHERE id = ?",
            at,
            &eid
        )
        .execute(&core.store.pool)
        .await
        .unwrap();

        // Driven through the queue, where `cover` runs and finds no pursuit to close yet.
        let corpus = core
            .ingest_capture(crate::core::ingest::Capture::new(q, "web").with_search(&eid))
            .await
            .unwrap()
            .id;
        while crate::jobs::run_one(&core).await.unwrap() {}
        core.background.wait_idle().await;
        core.set_weak_below(1.0);

        run(&core).await.unwrap();

        let states: Vec<(String, String)> =
            sqlx::query_as("SELECT id, state FROM pursuits ORDER BY id")
                .fetch_all(&core.store.pool)
                .await
                .unwrap();
        assert_eq!(
            states.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>(),
            vec!["unsatisfied"],
            "the sweep decides on its own evidence — nothing engaged — and the \
             capture changes only whether the hole is still open"
        );
        let covered = core.store.gaps_covered_by(&corpus).await.unwrap();
        assert!(
            covered
                .iter()
                .any(|g| g.kind == crate::store::gaps::GapKind::Pursuit),
            "the capture cannot say it answered the pursuit it was typed from: {covered:?}"
        );
        let open = core
            .store
            .open_gaps(core.embedder.model(), core.weak_below())
            .await
            .unwrap();
        assert!(
            open.gaps
                .iter()
                .all(|g| g.gap.kind != crate::store::gaps::GapKind::Pursuit),
            "the paste is still on the capture page as a hole in the base: {:?}",
            open.gaps.iter().map(|g| &g.gap).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn a_literal_no_source_carries_is_flagged() {
        let mut core = test_core().await;
        core.generator = Some(std::sync::Arc::new(
            crate::infer::fake::ScriptedCompleter::new(vec![
                r#"{"artifact":{"title":"T","text":"Run `wipefs --all /dev/sdX` first.","category":"procedure","tags":[],"caveats":[]}}"#.into(),
            ]),
        ));
        let ids = two_sources(&core).await;
        let pid = core
            .store
            .insert_pursuit(100, &["q".into()], &ids, None)
            .await
            .unwrap();
        generate(&core, &pid).await.unwrap();
        let g = &core.store.synthesized_artifacts(10).await.unwrap()[0];
        assert!(
            g.flags
                .iter()
                .any(|f| f == crate::infer::verify::FLAG_LITERALS),
            "{:?}",
            g.flags
        );
    }

    fn need(engaged: &[(&str, f64)]) -> Need {
        Need {
            queries: vec!["q".into()],
            engagement: engaged.iter().map(|(i, w)| (i.to_string(), *w)).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn the_decision_table_follows_the_spec() {
        let mut n = need(&[("a", 5.0), ("b", 5.0)]);
        n.answered = true;
        assert!(matches!(decide(&n, 2, 3.0), Decision::Satisfied(_)));
        assert_eq!(
            decide(&need(&[("a", 9.0)]), 2, 3.0),
            Decision::Satisfied("nothing to assemble".into())
        );
        assert!(matches!(
            decide(&need(&[("a", 1.0), ("b", 1.0), ("c", 1.0)]), 2, 3.0),
            Decision::Satisfied(_)
        ));
        let mut n = need(&[("a", 3.0), ("b", 1.0), ("c", 1.0)]);
        n.returns = 1;
        assert_eq!(decide(&n, 2, 3.0), Decision::Generate);
        let mut n = need(&[("a", 1.0), ("b", 1.0)]);
        n.abandoned = true;
        assert!(matches!(decide(&n, 2, 3.0), Decision::Unsatisfied(_)));
        let mut n = need(&[("a", 1.5), ("b", 1.5)]);
        n.abandoned = true;
        assert_eq!(decide(&n, 2, 3.0), Decision::Generate);
        let mut n = need(&[("a", 2.0), ("b", 1.0)]);
        n.abstained = true;
        assert_eq!(decide(&n, 2, 3.0), Decision::Generate);
        let mut n = need(&[("a", 2.0), ("b", 1.0)]);
        n.refined = 2;
        assert_eq!(decide(&n, 2, 3.0), Decision::Generate);
        let mut n = need(&[]);
        n.abandoned = true;
        assert!(matches!(decide(&n, 2, 3.0), Decision::Unsatisfied(_)));
    }

    #[test]
    fn a_short_pursuit_says_how_much_was_engaged_not_that_nothing_was() {
        let mut n = need(&[("a", 1.0), ("b", 1.0)]);
        n.abandoned = true;
        let Decision::Unsatisfied(why) = decide(&n, 3, 3.0) else {
            panic!("{:?}", decide(&n, 3, 3.0));
        };
        assert!(!why.contains("nothing engaged"), "{why}");
        assert!(why.contains('2'), "{why}");
    }

    async fn pursuing_core() -> crate::core::Core {
        let mut core = test_core().await;
        core.learn.enabled = true;
        core.pursuit.idle_secs = 10;
        core
    }

    async fn search_event(
        core: &crate::core::Core,
        q: &str,
        vec: Vec<f32>,
        shown: &[&str],
        at: i64,
    ) -> String {
        event_from(
            core,
            crate::store::feedback::Door::Ui,
            Some("me"),
            q,
            vec,
            shown,
            at,
        )
        .await
    }

    async fn event_from(
        core: &crate::core::Core,
        door: crate::store::feedback::Door,
        scope: Option<&str>,
        q: &str,
        vec: Vec<f32>,
        shown: &[&str],
        at: i64,
    ) -> String {
        let id = core
            .store
            .record_search(
                crate::store::feedback::NewEvent {
                    fold_onto: None,
                    query: q.into(),
                    door,
                    scope: scope.map(str::to_string),
                    filters: "{}".into(),
                    query_vec: vec,
                    embed_model: "fake".into(),
                    candidates: shown
                        .iter()
                        .enumerate()
                        .map(|(i, a)| crate::store::feedback::NewCandidate {
                            artifact_id: a.to_string(),
                            score: 1.0 - i as f32 * 0.1,
                            similarity: Some(0.9),
                            shown: true,
                            ..Default::default()
                        })
                        .collect(),
                    answered: false,
                    context: None,
                },
                0,
            )
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE search_events SET created_at = ? WHERE id = ?",
            at,
            &id
        )
        .execute(&core.store.pool)
        .await
        .unwrap();
        id
    }

    #[tokio::test]
    async fn an_open_at_a_shell_attaches_to_the_search_that_led_to_it() {
        let core = pursuing_core().await;
        let ids = two_sources(&core).await;
        let now = crate::store::now();
        event_from(
            &core,
            crate::store::feedback::Door::Cli,
            Some("me"),
            "how do I read the journal",
            vec![1.0, 0.0],
            &[&ids[0], &ids[1]],
            now - 100,
        )
        .await;
        core.store
            .record_interaction(&ids[0], "opened", None, Some("me"), now - 97)
            .await
            .unwrap();
        core.store
            .record_interaction(&ids[1], "opened", None, Some("me"), now - 96)
            .await
            .unwrap();

        assert_eq!(run(&core).await.unwrap(), 1);
        let p = &core.store.recent_pursuits(10).await.unwrap()[0];
        assert_eq!(
            p.sources.len(),
            2,
            "the opens did not reach the pursuit: {p:?}"
        );
        assert_ne!(
            p.reason.as_deref(),
            Some("nothing engaged"),
            "a shell that read two artifacts engaged two artifacts: {p:?}"
        );
    }

    #[tokio::test]
    async fn an_unscoped_search_cannot_be_joined_to_the_opens_after_it() {
        let core = pursuing_core().await;
        let ids = two_sources(&core).await;
        let now = crate::store::now();
        event_from(
            &core,
            crate::store::feedback::Door::Cli,
            None,
            "how do I read the journal",
            vec![1.0, 0.0],
            &[&ids[0], &ids[1]],
            now - 100,
        )
        .await;
        core.store
            .record_interaction(&ids[0], "opened", None, Some("me"), now - 97)
            .await
            .unwrap();
        core.store
            .record_interaction(&ids[1], "opened", None, Some("me"), now - 96)
            .await
            .unwrap();

        assert_eq!(run(&core).await.unwrap(), 1);
        let p = &core.store.recent_pursuits(10).await.unwrap()[0];
        assert_eq!(p.state, "unsatisfied", "{p:?}");
        assert_eq!(p.reason.as_deref(), Some("nothing engaged"), "{p:?}");
    }

    #[tokio::test]
    async fn an_offer_that_was_only_shown_is_not_engagement() {
        let core = pursuing_core().await;
        let ids = two_sources(&core).await;
        let now = crate::store::now();
        let t0 = now - 100;
        search_event(
            &core,
            "recycling centre hours",
            vec![1.0, 0.0],
            &[&ids[0]],
            t0,
        )
        .await;
        core.store
            .record_recommendation(
                &ids[0],
                "recommended_shown",
                r#"{"rung":"pattern","slot":0}"#,
                Some("me"),
                t0 + 1,
            )
            .await
            .unwrap();

        run(&core).await.unwrap();
        let pursuits = core.store.recent_pursuits(50).await.unwrap();
        assert!(
            pursuits.iter().all(|p| !p.sources.contains(&ids[0])),
            "an offer nobody clicked is not a source: {pursuits:?}"
        );
    }

    #[tokio::test]
    async fn the_sweep_groups_quiet_searches_into_pursuits_and_arms_a_generation() {
        let core = pursuing_core().await;
        let ids = two_sources(&core).await;
        let now = crate::store::now();
        let t0 = now - 100;
        search_event(
            &core,
            "read the journal",
            vec![1.0, 0.0],
            &[&ids[0], &ids[1]],
            t0,
        )
        .await;
        search_event(
            &core,
            "journal location",
            vec![0.99, 0.1],
            &[&ids[1], &ids[0]],
            t0 + 2,
        )
        .await;
        search_event(&core, "something else", vec![0.0, 1.0], &[&ids[0]], t0 + 50).await;
        core.store
            .record_interaction(&ids[0], "opened", None, Some("me"), t0 + 1)
            .await
            .unwrap();
        core.store
            .record_interaction(&ids[1], "pivoted", Some(&ids[0]), Some("me"), t0 + 3)
            .await
            .unwrap();
        core.store
            .record_interaction(&ids[0], "opened", None, Some("me"), t0 + 4)
            .await
            .unwrap();

        let written = run(&core).await.unwrap();
        assert_eq!(written, 2);
        let ps = core.store.recent_pursuits(10).await.unwrap();
        let assembled = ps
            .iter()
            .find(|p| p.queries.iter().any(|q| q == "read the journal"))
            .expect("the journal pursuit");
        assert_eq!(assembled.state, "open", "{assembled:?}");
        assert_eq!(assembled.queries.len(), 2, "{assembled:?}");
        assert_eq!(assembled.sources, ids, "{assembled:?}");
        assert!(
            core.store
                .live_job(Stage::Generate, &assembled.id)
                .await
                .unwrap()
        );
        let lone = ps
            .iter()
            .find(|p| p.queries.iter().any(|q| q == "something else"))
            .expect("the lone pursuit");
        assert_eq!(lone.state, "unsatisfied", "{lone:?}");
        assert_eq!(run(&core).await.unwrap(), 0);
    }

    async fn ask_event(
        core: &crate::core::Core,
        q: &str,
        vec: Vec<f32>,
        cited: &[(&str, bool)],
        abstained: bool,
        at: i64,
    ) -> String {
        let id = core
            .store
            .record_ask(crate::store::asks::NewAsk {
                question: q.into(),
                scope: Some("me".into()),
                filters: "{}".into(),
                query_vec: vec,
                embed_model: "fake".into(),
                answer: "an answer".into(),
                abstained,
                citations: cited
                    .iter()
                    .map(|(a, used)| crate::store::asks::NewAskCitation {
                        artifact_id: a.to_string(),
                        score: 0.9,
                        used: *used,
                    })
                    .collect(),
                ..Default::default()
            })
            .await
            .unwrap();
        sqlx::query!("UPDATE ask_events SET created_at = ? WHERE id = ?", at, &id)
            .execute(&core.store.pool)
            .await
            .unwrap();
        id
    }

    #[tokio::test]
    async fn a_question_the_model_declined_does_not_pay_for_a_generation() {
        // An ask is shown every excerpt it packed; scoring them would make its abstention count as
        // engagement.
        let core = pursuing_core().await;
        let ids = two_sources(&core).await;
        let now = crate::store::now();
        let shown: Vec<(&str, bool)> = vec![(ids[0].as_str(), false), (ids[1].as_str(), false)];
        ask_event(
            &core,
            "how do I read the journal",
            vec![1.0, 0.0],
            &shown,
            true,
            now - 100,
        )
        .await;

        assert_eq!(run(&core).await.unwrap(), 1);
        let p = &core.store.recent_pursuits(10).await.unwrap()[0];
        assert_eq!(p.state, "unsatisfied", "{p:?}");
        assert!(
            !core.store.live_job(Stage::Generate, &p.id).await.unwrap(),
            "an unanswered question is a need, not an engagement"
        );

        let core = pursuing_core().await;
        let ids = two_sources(&core).await;
        let used: Vec<(&str, bool)> = vec![(ids[0].as_str(), true), (ids[1].as_str(), true)];
        ask_event(
            &core,
            "how do I read it",
            vec![1.0, 0.0],
            &used,
            false,
            now - 100,
        )
        .await;
        search_event(
            &core,
            "journal location",
            vec![0.99, 0.1],
            &[&ids[0]],
            now - 98,
        )
        .await;
        core.store
            .record_interaction(&ids[0], "opened", None, Some("me"), now - 97)
            .await
            .unwrap();
        core.store
            .record_interaction(&ids[1], "pivoted", Some(&ids[0]), Some("me"), now - 96)
            .await
            .unwrap();
        assert_eq!(run(&core).await.unwrap(), 1);
        let p = &core.store.recent_pursuits(10).await.unwrap()[0];
        assert_eq!(p.state, "open", "{p:?}");
        assert!(core.store.live_job(Stage::Generate, &p.id).await.unwrap());
    }

    #[tokio::test]
    async fn a_sweep_that_failed_before_its_cursor_moved_writes_no_second_copy() {
        // The cursor advances once, after the loop; a retry re-clusters the same events and finds
        // its own rows.
        let core = pursuing_core().await;
        let ids = two_sources(&core).await;
        let now = crate::store::now();
        let t0 = now - 100;
        search_event(&core, "read the journal", vec![1.0, 0.0], &[&ids[0]], t0).await;
        search_event(
            &core,
            "journal location",
            vec![0.99, 0.1],
            &[&ids[1]],
            t0 + 2,
        )
        .await;
        search_event(&core, "something else", vec![0.0, 1.0], &[&ids[0]], t0 + 50).await;
        core.store
            .record_interaction(&ids[0], "opened", None, Some("me"), t0 + 1)
            .await
            .unwrap();
        core.store
            .record_interaction(&ids[1], "pivoted", Some(&ids[0]), Some("me"), t0 + 3)
            .await
            .unwrap();

        let first = run(&core).await.unwrap();
        let before = core.store.recent_pursuits(50).await.unwrap();
        assert_eq!(before.len(), first);

        core.store.meta_set(PURSUIT_AFTER, "0").await.unwrap();
        run(&core).await.unwrap();

        let after = core.store.recent_pursuits(50).await.unwrap();
        assert_eq!(
            after.len(),
            before.len(),
            "the same sitting was written twice: {after:?}"
        );
        let mut ids_before: Vec<&str> = before.iter().map(|p| p.id.as_str()).collect();
        let mut ids_after: Vec<&str> = after.iter().map(|p| p.id.as_str()).collect();
        ids_before.sort_unstable();
        ids_after.sort_unstable();
        assert_eq!(ids_before, ids_after);
    }

    #[tokio::test]
    async fn an_open_the_search_never_showed_attaches_to_nothing() {
        let core = pursuing_core().await;
        let ids = two_sources(&core).await;
        let now = crate::store::now();
        search_event(
            &core,
            "the css when the page changes",
            vec![1.0, 0.0],
            &[],
            now - 100,
        )
        .await;
        for (i, id) in ids.iter().enumerate() {
            core.store
                .record_interaction(id, "opened", None, Some("me"), now - 90 + i as i64)
                .await
                .unwrap();
        }
        assert_eq!(run(&core).await.unwrap(), 1);
        let p = &core.store.recent_pursuits(10).await.unwrap()[0];
        assert_eq!(p.state, "unsatisfied", "{p:?}");
        assert_eq!(p.reason.as_deref(), Some("nothing engaged"), "{p:?}");
        assert!(p.sources.is_empty(), "{p:?}");
    }

    #[tokio::test]
    async fn a_pivot_from_a_shown_hit_is_engagement_and_so_is_opening_where_it_landed() {
        let core = pursuing_core().await;
        let ids = two_sources(&core).await;
        let now = crate::store::now();
        search_event(
            &core,
            "read the journal",
            vec![1.0, 0.0],
            &[&ids[0]],
            now - 100,
        )
        .await;
        core.store
            .record_interaction(&ids[1], "pivoted", Some(&ids[0]), Some("me"), now - 95)
            .await
            .unwrap();
        core.store
            .record_interaction(&ids[1], "opened", None, Some("me"), now - 94)
            .await
            .unwrap();
        assert_eq!(run(&core).await.unwrap(), 1);
        let p = &core.store.recent_pursuits(10).await.unwrap()[0];
        assert_eq!(p.sources, vec![ids[1].clone()], "{p:?}");
    }

    #[tokio::test]
    async fn grouping_does_not_chain_through_a_bridge() {
        let mut core = pursuing_core().await;
        core.pursuit.idle_secs = 1_000;
        let ids = two_sources(&core).await;
        let now = crate::store::now();
        let a = vec![1.0, 0.0];
        let b = vec![0.7, 0.7]; // 0.71 to each end
        let c = vec![0.0, 1.0]; // 0.0 to `a`
        search_event(&core, "a", a, &[&ids[0]], now - 3_000).await;
        search_event(&core, "b", b, &[&ids[0]], now - 2_900).await;
        search_event(&core, "c", c, &[&ids[0]], now - 2_800).await;
        assert_eq!(run(&core).await.unwrap(), 2);
        let ps = core.store.recent_pursuits(10).await.unwrap();
        let mut sizes: Vec<usize> = ps.iter().map(|p| p.queries.len()).collect();
        sizes.sort();
        assert_eq!(sizes, vec![1, 2], "{ps:?}");
    }

    #[tokio::test]
    async fn two_sittings_on_one_subject_are_two_pursuits() {
        // `cluster` groups on words, which do not say when.
        let core = pursuing_core().await;
        let ids = two_sources(&core).await;
        let now = crate::store::now();
        let t0 = now - 10_000;
        let gap = core.pursuit.idle_secs as i64 * 10;
        search_event(&core, "read the journal", vec![1.0, 0.0], &[&ids[0]], t0).await;
        search_event(
            &core,
            "read the journal again",
            vec![1.0, 0.0],
            &[&ids[0]],
            t0 + gap,
        )
        .await;

        assert_eq!(run(&core).await.unwrap(), 2, "one sitting each");
        let ps = core.store.recent_pursuits(10).await.unwrap();
        assert!(
            ps.iter().all(|p| p.queries.len() == 1),
            "a pursuit spans one sitting: {ps:?}"
        );
    }

    #[tokio::test]
    async fn the_sweep_never_reaches_further_back_than_its_lookback() {
        let core = pursuing_core().await;
        let ids = two_sources(&core).await;
        let now = crate::store::now();
        search_event(
            &core,
            "from another era",
            vec![1.0, 0.0],
            &[&ids[0]],
            now - MAX_LOOKBACK_SECS - 60,
        )
        .await;
        search_event(&core, "from today", vec![0.0, 1.0], &[&ids[0]], now - 100).await;

        assert!(
            core.store.meta_get(PURSUIT_AFTER).await.unwrap().is_none(),
            "the fixture must start without a cursor"
        );
        assert_eq!(run(&core).await.unwrap(), 1);
        let ps = core.store.recent_pursuits(10).await.unwrap();
        assert_eq!(ps.len(), 1);
        assert_eq!(ps[0].queries, vec!["from today".to_string()], "{ps:?}");
    }

    #[tokio::test]
    async fn the_sweep_waits_while_the_operator_is_still_searching() {
        let core = pursuing_core().await;
        let ids = two_sources(&core).await;
        let now = crate::store::now();
        search_event(&core, "fresh", vec![1.0, 0.0], &[&ids[0]], now).await;
        assert_eq!(run(&core).await.unwrap(), 0);
        assert!(core.store.recent_pursuits(10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_need_already_written_up_closes_satisfied_naming_the_generation() {
        let core = pursuing_core().await;
        let ids = two_sources(&core).await;
        let existing = core
            .store
            .insert_synthesized_artifact(
                &NewSynthesized {
                    text: "already written".into(),
                    title: Some("G".into()),
                    category: None,
                    tags: vec![],
                    caveats: vec![],
                    cues: vec![],
                },
                &ids,
            )
            .await
            .unwrap();
        let now = crate::store::now();
        let t0 = now - 100;
        search_event(
            &core,
            "read the journal",
            vec![1.0, 0.0],
            &[&ids[0], &ids[1]],
            t0,
        )
        .await;
        core.store
            .record_interaction(&ids[0], "opened", None, Some("me"), t0 + 1)
            .await
            .unwrap();
        core.store
            .record_interaction(&ids[1], "pivoted", Some(&ids[0]), Some("me"), t0 + 2)
            .await
            .unwrap();
        core.store
            .record_interaction(&ids[0], "opened", None, Some("me"), t0 + 3)
            .await
            .unwrap();
        run(&core).await.unwrap();
        let p = &core.store.recent_pursuits(10).await.unwrap()[0];
        assert_eq!(p.state, "satisfied", "{p:?}");
        assert!(
            p.reason
                .as_deref()
                .unwrap_or_default()
                .contains(&existing.id),
            "{p:?}"
        );
        assert!(!core.store.live_job(Stage::Generate, &p.id).await.unwrap());
    }

    #[tokio::test]
    async fn one_engaged_artifact_after_refining_closes_unsatisfied() {
        let core = pursuing_core().await;
        let out = core
            .ingest("a verbatim passage", "web", None)
            .await
            .unwrap();
        crate::jobs::passages::capture_verbatim(&core, &out.id)
            .await
            .unwrap();
        let p = core.store.artifacts_for_corpus(&out.id).await.unwrap()[0]
            .id
            .clone();
        let now = crate::store::now();
        search_event(&core, "where is it stored", vec![1.0, 0.0], &[&p], now - 40).await;
        search_event(&core, "where is it kept", vec![1.0, 0.0], &[&p], now - 36).await;
        search_event(&core, "storage location", vec![1.0, 0.0], &[&p], now - 32).await;
        core.store
            .record_interaction(&p, "opened", None, Some("me"), now - 31)
            .await
            .unwrap();
        run(&core).await.unwrap();
        // Capture armed this corpus's one synthesis call; the sweep must not have driven it to
        // completion.
        assert_ne!(
            core.store.segment_state(&out.id, 0).await.unwrap(),
            Some(crate::store::segments::SegmentState::Done),
            "nothing should have been promoted"
        );
        let row = &core.store.recent_pursuits(10).await.unwrap()[0];
        assert_eq!(row.state, "unsatisfied", "{row:?}");
    }

    #[tokio::test]
    async fn dwell_alone_never_crosses_min_engagement_but_orders_the_sources() {
        let core = pursuing_core().await;
        let ids = two_sources(&core).await;
        let now = crate::store::now();
        let t0 = now - 100;
        search_event(
            &core,
            "only looked",
            vec![1.0, 0.0],
            &[&ids[0], &ids[1]],
            t0,
        )
        .await;
        core.store
            .record_dwell(&ids[0], 600, Some("me"), t0 + 1)
            .await
            .unwrap();
        core.store
            .record_dwell(&ids[1], 600, Some("me"), t0 + 2)
            .await
            .unwrap();
        run(&core).await.unwrap();
        let p = &core.store.recent_pursuits(10).await.unwrap()[0];
        assert_eq!(p.state, "unsatisfied", "{p:?}");

        // A fresh core: the first sweep moved the watermark past these timestamps.
        let core = pursuing_core().await;
        let ids = two_sources(&core).await;
        let t1 = now - 50;
        search_event(&core, "read both", vec![0.0, 1.0], &[&ids[0], &ids[1]], t1).await;
        core.store
            .record_interaction(&ids[0], "opened", None, Some("me"), t1 + 1)
            .await
            .unwrap();
        core.store
            .record_interaction(&ids[1], "opened", None, Some("me"), t1 + 2)
            .await
            .unwrap();
        core.store
            .record_dwell(&ids[1], 300, Some("me"), t1 + 3)
            .await
            .unwrap();
        core.store
            .record_interaction(&ids[0], "opened", None, Some("me"), t1 + 4)
            .await
            .unwrap();
        core.store
            .record_interaction(&ids[1], "opened", None, Some("me"), t1 + 5)
            .await
            .unwrap();
        run(&core).await.unwrap();
        let p = core
            .store
            .recent_pursuits(10)
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.queries.iter().any(|q| q == "read both"))
            .unwrap();
        assert_eq!(p.sources[0], ids[1], "{p:?}");
    }
}
