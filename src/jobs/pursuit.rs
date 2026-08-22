//! Pursuits: a coherent run of searches, what was engaged with, and — when the
//! base did not answer or the answer was assembled by hand — the one artifact
//! that earns.
//!
//! Local decides, the model only writes. The sweep (`run`) groups quiet
//! searches by their stored vectors, scores engagement, and arms `Generate`
//! for a pursuit that earned it; `generate` makes the one call.

use crate::core::Core;
use crate::error::Result;
use crate::infer::prompt;
use crate::store::artifacts::NewSynthesized;
use crate::store::jobs::Stage;

/// Write the artifact a pursuit earned. One call; supersedes nothing.
///
/// Idempotent: a pursuit that is no longer `open`, or already names an
/// artifact, is left alone — a retry after a crash between the insert and the
/// pursuit update must not write twice.
pub async fn generate(core: &Core, pursuit_id: &str) -> Result<()> {
    let p = core.store.get_pursuit(pursuit_id).await?;
    if p.state != "open" || p.artifact_id.is_some() {
        return Ok(());
    }
    let Some(generator) = core.generator.clone() else {
        return Ok(());
    };
    let now = crate::store::now();

    // The engaged artifacts, whatever their provenance — a generated artifact
    // the operator pivoted through contributes its own text, unresolved. In
    // engagement order, the way the sweep stored them.
    let rows = core.store.artifacts_by_ids(&p.sources).await?;
    let mut sources: Vec<crate::store::artifacts::Chunk> = p
        .sources
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

    // Packed to the window the way the dedupe judge packs: the questions and
    // the system prompt always go out; sources are dropped from the tail when
    // they would not fit.
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
        // Dropping the tail is what makes it fit, and `min_sources` is the
        // floor below which a generation has too little to stand on. Reaching
        // that floor still over budget is not a smaller prompt to try — it is
        // a fact about how long these particular artifacts are. Sending it
        // anyway spends a 400 and then `MAX_ATTEMPTS` more on byte-identical
        // content, so the pursuit is closed here with the reason on it,
        // the way `dedupe` refuses a pair that cannot fit one call.
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
    // Drift is caught rather than prevented: a literal in the generated text
    // that no source carries is flagged for whoever reads it.
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

/// Cursor: the moment everything up to which has been grouped into pursuits.
pub const PURSUIT_AFTER: &str = "pursuit.events_after";

/// How far back one sweep may look, whatever the cursor says.
///
/// A ceiling rather than a budget: pursuits are swept every time the operator
/// goes quiet, so nothing in ordinary use comes near it. What it stops is the
/// first sweep after `[learn]` is turned on, where there is no cursor
/// and the window would otherwise open at the epoch. Recording is on by
/// default, so that window holds every search the base has taken since it was
/// installed — and the clustering below is quadratic in *memory* as well as
/// time, so reading it is not a slow sweep, it is one `Vec` the size of the
/// square of the log.
///
/// A day is chosen because a pursuit is a sitting, and the sweep after an
/// idle stretch has already seen everything older than the last one.
const MAX_LOOKBACK_SECS: i64 = 24 * 60 * 60;

/// One clustered pursuit before a decision: what was asked, and what was done
/// with the results.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Need {
    pub queries: Vec<String>,
    pub opened_at: i64,
    pub last_at: i64,
    /// A synthesized artifact led one of the lists above `weak_below`.
    pub answered: bool,
    /// A question was answered "not in the knowledge base".
    pub abstained: bool,
    /// Engagement weight per artifact, in first-engagement order.
    pub engagement: Vec<(String, f64)>,
    /// Dwell weight per artifact, ≤ 0.5 each. A tiebreak for the order the
    /// sources go into the prompt; never part of what decides.
    pub dwell: Vec<(String, f64)>,
    pub pivots: usize,
    pub returns: usize,
    /// Something opened or confirmed was a strong hit — at or above
    /// `weak_below` — or a question cited it.
    pub strong_engaged: bool,
    /// Searches with nothing opened that were followed by another search.
    pub refined: usize,
    /// The last search opened nothing and nothing followed it.
    pub abandoned: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Satisfied(String),
    Unsatisfied(String),
    Generate,
}

/// The signals that say the need went unmet, whatever else happened: no strong
/// hit was engaged, the question was rephrased and rephrased, the last search
/// was walked away from, the model declined to answer.
///
pub fn unsatisfied(n: &Need) -> bool {
    !n.strong_engaged || n.refined >= 2 || n.abandoned || n.abstained
}

/// The analysis pass, in the spec's order. Pure: every input is in `Need`.
pub fn decide(n: &Need, min_sources: usize, min_engagement: f64) -> Decision {
    if n.answered {
        return Decision::Satisfied("a synthesized artifact led the list".into());
    }
    let total: f64 = n.engagement.iter().map(|(_, w)| *w).sum();
    let unsatisfied = unsatisfied(n);
    if n.engagement.len() < min_sources {
        return if unsatisfied {
            // Above the shipped `min_sources = 2` this is reachable with
            // artifacts engaged, and "nothing engaged" would then be a lie.
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

/// One event of either kind, as the sweep clusters it.
struct Ev {
    at: i64,
    scope: Option<String>,
    query: String,
    vec: Vec<f32>,
    /// Search only.
    answered: bool,
    confirmed: Option<String>,
    shown: Vec<(String, Option<f32>)>,
    /// Ask only.
    is_ask: bool,
    abstained: bool,
    /// The excerpts the *answer* referenced — never everything the ask packed
    /// into its prompt. See `RecordedAsk::cited`.
    cited: Vec<String>,
}

/// Group quiet searches into pursuits and decide each. Returns how many
/// pursuit rows were written.
///
/// Runs only when everything unprocessed has been idle for `idle_secs`: a
/// pursuit is a thing that ended, and the cheapest correct idle rule is to
/// wait until the operator stopped. Local throughout — the only model call is
/// the one `Generate` makes later, and only for a pursuit that earned it.
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

    // An interaction belongs to the latest event before it, in the same scope,
    // within the idle window. Never to an answered search: the base answered,
    // and what was opened afterwards is not a need.
    let mut attached: Vec<Vec<usize>> = vec![Vec::new(); evs.len()];
    for (k, i) in interactions.iter().enumerate() {
        let owner = evs
            .iter()
            .enumerate()
            .filter(|(_, e)| e.at <= i.at && i.at - e.at <= idle && e.scope == i.scope)
            .max_by_key(|(_, e)| e.at)
            .map(|(idx, _)| idx);
        if let Some(idx) = owner {
            attached[idx].push(k);
        }
    }

    let vecs: Vec<Vec<f32>> = evs.iter().map(|e| e.vec.clone()).collect();
    let line = crate::core::gaps::link_threshold(&vecs);
    let refs: Vec<&[f32]> = vecs.iter().map(Vec::as_slice).collect();
    let clusters = crate::core::gaps::cluster(&refs, line);

    // `cluster` groups on the words alone, and the words do not say when. Two
    // sittings a week apart on one subject come back as one group, and every
    // count taken over it is then taken across the gap: `opened_at`/`last_at`
    // span the week, and `refined`/`abandoned` are read from events that were
    // never consecutive. Splitting on the same idle gap that decides when the
    // sweep runs is what makes each piece a pursuit — something that ended —
    // rather than a topic.
    let clusters: Vec<Vec<usize>> = clusters
        .into_iter()
        .flat_map(|mut members| {
            members.sort_by_key(|&m| evs[m].at);
            let mut sittings: Vec<Vec<usize>> = Vec::new();
            for m in members {
                match sittings.last_mut() {
                    Some(run) if evs[m].at - evs[*run.last().unwrap()].at <= idle => run.push(m),
                    _ => sittings.push(vec![m]),
                }
            }
            sittings
        })
        .collect();

    let mut written = 0;
    for members in clusters {
        let need = need_of(core, &evs, &members, &attached, &interactions);
        // Sources in the order they go to the model: engagement first, dwell
        // breaking ties. Dwell moves an artifact up the list and nothing else.
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
        // The leading event's vector, carried onto the row. A pursuit that
        // closes unsatisfied is a gap, and a gap is a question plus the vector
        // it was found by; the sweep is holding both right here, and embedding
        // the words again later would be a call spent on a vector that has
        // already been computed.
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
        // `insert_pursuit` is keyed on the cluster, so a sitting the previous
        // sweep already reached comes back with the row it wrote. Only a
        // pursuit still `open` is undecided — either brand new, or written by
        // a sweep that failed between the insert and the decision. Anything
        // closed, generated or dismissed has had its answer and must not be
        // decided a second time: that is what would arm a second generation
        // for one need, or re-open a pursuit the operator dismissed.
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
            }
            Decision::Generate => {
                // Nothing to arm without a generator. `run_claimed` would drop
                // the unit with a warning and the pursuit would stay `open`
                // with no reason — one more row on Ops after every sweep,
                // saying a call is coming that never is. Pursuits still earn
                // their keep at `synthesis = "off"`: `Promote` needs no model.
                if !core.synthesizes() {
                    core.store
                        .close_pursuit(
                            &pid,
                            "unsatisfied",
                            "earned an artifact, but there is no [infer.synthesize] to write it",
                            now,
                        )
                        .await?;
                } else if let Some(covering) = covered_by_existing(core, &sources).await? {
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

/// Score one cluster.
fn need_of(
    core: &Core,
    evs: &[Ev],
    members: &[usize],
    attached: &[Vec<usize>],
    interactions: &[crate::store::pursuits::Interaction],
) -> Need {
    use crate::store::links::normalize_query;
    let mut n = Need {
        opened_at: evs[members[0]].at,
        last_at: evs[*members.last().unwrap()].at,
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
            // A question that drew on an artifact engaged it as surely as
            // opening it does. A question the model declined to answer drew on
            // nothing and arrives here with an empty list — which is what
            // keeps an abstention from being both the evidence that a need
            // exists and the engagement that says it was pursued.
            for id in &e.cited {
                bump(id, 1.0);
                n.strong_engaged = true;
            }
            continue;
        }
        if let Some(id) = &e.confirmed {
            bump(id, 3.0);
            n.strong_engaged = true;
        }
        let mine: Vec<&crate::store::pursuits::Interaction> = attached[m]
            .iter()
            .map(|&k| &interactions[k])
            // What this base *offered* is not something a person did. Every
            // kind that is not `dwell` or `pivoted` is weighed at 1.0 below, so
            // without this every artifact the recommender ever guessed at would
            // count as engaged — and `engaged`, a few lines down, would call a
            // search followed by nothing a search that was followed.
            .filter(|i| i.kind != "recommended_shown")
            .collect();
        if !e.answered {
            for i in &mine {
                if i.kind == "dwell" {
                    // ≤ 0.5 per artifact: a tenth per minute, capped. Tiebreak
                    // only — see `Need::dwell`.
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
                let strong = e
                    .shown
                    .iter()
                    .find(|(id, _)| id == &i.artifact_id)
                    .is_some_and(|(_, sim)| sim.is_none_or(|s| s >= core.weak_below));
                n.strong_engaged |= strong;
            }
        }
        let followed = pos + 1 < members.len();
        // Dwell alone is not engagement: a search whose only trace is time
        // spent is still a search nothing was opened on.
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

/// The second anchor: are these sources' roots all inside an existing active
/// generation's roots? Then the need is already written up, whatever the
/// ranking did with it.
async fn covered_by_existing(core: &Core, sources: &[String]) -> Result<Option<String>> {
    let roots = core.store.roots_of(sources).await?;
    let mine: std::collections::BTreeSet<String> = roots.values().flatten().cloned().collect();
    if mine.is_empty() {
        return Ok(None);
    }
    // One lineage read for the whole candidate set rather than one per
    // candidate inside the loop: the answer is the same, and the walk stays a
    // single awaited call however many generations are on the list.
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
            corpus_span: None,
            title: Some(format!("S{o}")),
            category: None,
            tags: vec![],
            segment_idx: None,
            caveats: vec![],
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
        // Its sources stay active: nothing was superseded.
        for id in &ids {
            assert!(core.store.get_artifact(id).await.unwrap().in_results());
        }
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
            strong_engaged: true,
            ..Default::default()
        }
    }

    #[test]
    fn the_decision_table_follows_the_spec() {
        // Answered: nothing else matters.
        let mut n = need(&[("a", 5.0), ("b", 5.0)]);
        n.answered = true;
        assert!(matches!(decide(&n, 2, 3.0), Decision::Satisfied(_)));
        // One engaged artifact is below `min_sources` like any other shortfall.
        // It used to be a promotion case of its own; the engagement that made
        // it one now promotes at the bump, hours before this sweep runs.
        assert_eq!(
            decide(&need(&[("a", 9.0)]), 2, 3.0),
            Decision::Satisfied("nothing to assemble".into())
        );
        // Three strong opens in a row, then stopped: reading, not assembling.
        assert!(matches!(
            decide(&need(&[("a", 1.0), ("b", 1.0), ("c", 1.0)]), 2, 3.0),
            Decision::Satisfied(_)
        ));
        // The same three with one return: assembled.
        let mut n = need(&[("a", 3.0), ("b", 1.0), ("c", 1.0)]);
        n.returns = 1;
        assert_eq!(decide(&n, 2, 3.0), Decision::Generate);
        // Two weak opens totalling 2.0: unsatisfied but not worth a call.
        let mut n = need(&[("a", 1.0), ("b", 1.0)]);
        n.strong_engaged = false;
        assert!(matches!(decide(&n, 2, 3.0), Decision::Unsatisfied(_)));
        // Two weak opens totalling 3.0: generate.
        let mut n = need(&[("a", 1.5), ("b", 1.5)]);
        n.strong_engaged = false;
        assert_eq!(decide(&n, 2, 3.0), Decision::Generate);
        // An abstention with two cited sources: generate.
        let mut n = need(&[("a", 2.0), ("b", 1.0)]);
        n.abstained = true;
        assert_eq!(decide(&n, 2, 3.0), Decision::Generate);
        // Refined twice, assembled: generate.
        let mut n = need(&[("a", 2.0), ("b", 1.0)]);
        n.refined = 2;
        assert_eq!(decide(&n, 2, 3.0), Decision::Generate);
        // Nothing engaged and abandoned: unsatisfied.
        let mut n = need(&[]);
        n.strong_engaged = false;
        n.abandoned = true;
        assert!(matches!(decide(&n, 2, 3.0), Decision::Unsatisfied(_)));
    }

    #[test]
    fn a_short_pursuit_says_how_much_was_engaged_not_that_nothing_was() {
        // Only reachable above the shipped `min_sources = 2`, where two
        // engaged artifacts are still too few to write from.
        let mut n = need(&[("a", 1.0), ("b", 1.0)]);
        n.abandoned = true;
        let Decision::Unsatisfied(why) = decide(&n, 3, 3.0) else {
            panic!("{:?}", decide(&n, 3, 3.0));
        };
        assert!(!why.contains("nothing engaged"), "{why}");
        assert!(why.contains('2'), "{why}");
    }

    /// A core at earned with pursuits on, recording, and a tiny idle window.
    async fn pursuing_core() -> crate::core::Core {
        let mut core = test_core().await;
        core.synthesis = crate::config::SynthesisMode::Earned;
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
        let id = core
            .store
            .record_search(
                crate::store::feedback::NewEvent {
                    query: q.into(),
                    door: crate::store::feedback::Door::Ui,
                    scope: Some("me".into()),
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
                        })
                        .collect(),
                    answered: false,
                },
                0,
            )
            .await
            .unwrap();
        // Back-date it: `record_search` stamps now, and the sweep wants quiet.
        sqlx::query("UPDATE search_events SET created_at = ? WHERE id = ?")
            .bind(at)
            .bind(&id)
            .execute(&core.store.pool)
            .await
            .unwrap();
        id
    }

    #[tokio::test]
    async fn an_offer_that_was_only_shown_is_not_engagement() {
        // `recommended_shown` is the recommender's own guess, not something a
        // person did. Every kind that is not `dwell` or `pivoted` is weighed at
        // 1.0, so without an explicit exclusion every artifact the recommender
        // ever guessed at would count as engaged — and a search followed by
        // nothing at all would read as a search that was followed.
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
        // Two unrelated needs: one assembled by pivoting between two sources,
        // one a single search with nothing opened.
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
        // The watermark moved; a second sweep finds nothing new.
        assert_eq!(run(&core).await.unwrap(), 0);
    }

    /// Record a question at `at`, with `cited` naming each excerpt the model
    /// was shown and whether the answer referenced it.
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
                dropped: 0,
                truncated: false,
                citations: cited
                    .iter()
                    .map(|(a, used)| crate::store::asks::NewAskCitation {
                        artifact_id: a.to_string(),
                        score: 0.9,
                        used: *used,
                    })
                    .collect(),
            })
            .await
            .unwrap();
        sqlx::query("UPDATE ask_events SET created_at = ? WHERE id = ?")
            .bind(at)
            .bind(&id)
            .execute(&core.store.pool)
            .await
            .unwrap();
        id
    }

    #[tokio::test]
    async fn a_question_the_model_declined_does_not_pay_for_a_generation() {
        // An ask packs whatever fits its window, so a question that retrieved
        // three excerpts and then abstained was *shown* three artifacts. Score
        // those as engagement and the abstention is both the evidence that a
        // need exists and the engagement that says it was pursued: the base
        // spends a call writing an artifact out of the very excerpts it has
        // just declared insufficient.
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

        // The same question, answered out of the same two excerpts: the
        // engagement is real and the pursuit is worth a call.
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
        // The cursor advances once, after the loop. Anything that fails inside
        // it — a locked database on the fourth cluster — leaves the pursuits
        // already written under a cursor that never moved, and the retry reads
        // the same events and clusters them the same way. The pursuit's
        // identity is the sitting, so the second pass finds its own rows.
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

        // The cursor back where the failed sweep left it.
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
    async fn two_sittings_on_one_subject_are_two_pursuits() {
        // `cluster` groups on the words, and the words do not say when. Without
        // the split these two come back as one pursuit whose `opened_at` and
        // `last_at` span the gap between them and whose engagement is summed
        // across a session boundary.
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
        // With no cursor the window would open at the epoch, and on a base that
        // has been recording since it was installed that is every search ever
        // taken — clustered quadratically, in one allocation.
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
    async fn a_pursuit_that_earns_a_write_up_with_no_generator_is_closed_not_left_open() {
        // `run_claimed` drops a `Generate` unit it cannot run, with a warning.
        // Nothing then closes the pursuit, so it sits `open` with no reason on
        // Ops, one more of it after every sweep, promising a call that is never
        // coming.
        let mut core = pursuing_core().await;
        core.synthesizer = None;
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

        run(&core).await.unwrap();
        let ps = core.store.recent_pursuits(10).await.unwrap();
        let p = ps
            .iter()
            .find(|p| p.queries.iter().any(|q| q == "read the journal"))
            .expect("the journal pursuit");
        assert_eq!(p.state, "unsatisfied", "{p:?}");
        assert!(
            !core.store.live_job(Stage::Generate, &p.id).await.unwrap(),
            "nothing to arm without a generator"
        );
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
        // Three phrasings of one question with a single open at the end is the
        // shape of a need that went unmet. Nothing the sweep does can help it:
        // one engaged artifact is below `min_sources`, and the sweep no longer
        // has a promotion of its own to try. Inside one sitting — further apart
        // than `idle_secs` and these would be three pursuits, not one need
        // asked three ways.
        search_event(&core, "where is it stored", vec![1.0, 0.0], &[&p], now - 40).await;
        search_event(&core, "where is it kept", vec![1.0, 0.0], &[&p], now - 36).await;
        search_event(&core, "storage location", vec![1.0, 0.0], &[&p], now - 32).await;
        core.store
            .record_interaction(&p, "opened", None, Some("me"), now - 31)
            .await
            .unwrap();
        run(&core).await.unwrap();
        assert_eq!(
            core.store.segment_state(&out.id, 0).await.unwrap(),
            Some(crate::store::segments::SegmentState::Verbatim),
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
        // Nothing opened; long dwell on both. A search with only dwell is a
        // search nothing was opened on: unsatisfied, not generated.
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

        // Opened both equally, dwelt on the second: the second leads. A fresh
        // core — the first sweep moved the watermark past these timestamps.
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
