pub mod check;
mod plan;
mod retrieve;
pub mod stream;

use super::Core;
use super::search::{SearchQuery, SearchResult};
use crate::error::{Error, Result};
use crate::infer::budget::pack_by_budget;
use crate::infer::prompt::{ABSTAIN_PREFIX, ASK_SYSTEM, abstained, ask_excerpt, ask_prompt};
use crate::store::asks::{NewAsk, NewAskCitation};
use crate::store::feedback::{Door, Origin};
use stream::AskEvent;
use tokio_stream::StreamExt;

#[derive(Debug, Clone, serde::Deserialize)]
pub struct AskRequest {
    pub q: String,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub category: Option<String>,
}

struct Round {
    /// On the round, not zipped beside it: `fan_out` drops failed rounds and a parallel list would
    /// mislabel gaps.
    query: String,
    /// Above the cliff, with whatever was reached sideways appended.
    hits: Vec<SearchResult>,
    /// How many of `hits` are ranked. Everything after them was reached.
    ranked: usize,
    /// Everything ranked, cliff and all, so a citation lost to the cliff counts in `dropped`.
    retrieved: Vec<String>,
    cliff_at: Option<usize>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AskResponse {
    pub answer: String,
    /// Exactly the excerpts the model saw.
    pub citations: Vec<SearchResult>,
    /// Retrieved but left out, below the cliff or past the window.
    pub dropped: usize,
    /// The answer stopped at its output ceiling (`ask.max_output_tokens`), not where the model
    /// meant to.
    pub truncated: bool,
    /// Opened with `prompt::ABSTAIN_PREFIX`, or there was nothing to show the model.
    pub abstained: bool,
    /// Literals the answer carries that no excerpt it was shown does.
    pub unsupported: Vec<String>,
    /// Every excerpt was retired; a completed reminder's note may be the only answer there is.
    pub retired_only: bool,
    /// Contradiction pairs with both sides among `citations`, one row per pair; the base never
    /// picks a side.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disagreements: Vec<crate::store::pairs::Disagreement>,
    /// Set only where this door records; the page shows a verdict bar only then.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
}

impl Core {
    /// A collector over `ask_events`, so there is exactly one account of what asking means.
    pub async fn ask(&self, req: &AskRequest, origin: impl Into<Origin>) -> Result<AskResponse> {
        let s = self.ask_events(req, origin);
        tokio::pin!(s);
        let mut done = None;
        while let Some(ev) = s.next().await {
            if let AskEvent::Done(d) = ev? {
                done = Some(*d);
            }
        }
        done.ok_or_else(|| Error::Internal("ask produced no answer".into()))
    }

    /// One ask as it happens. `'static`: an SSE response outlives its handler, so it owns its
    /// inputs.
    pub fn ask_events<O: Into<Origin>>(
        &self,
        req: &AskRequest,
        origin: O,
    ) -> impl tokio_stream::Stream<Item = Result<AskEvent>> + 'static + use<O> {
        let core = self.clone();
        let req = req.clone();
        let origin = origin.into();

        async_stream::try_stream! {
            if req.q.trim().is_empty() {
                Err(Error::Validation("question is empty".into()))?;
            }
            // No ask model: a caller that found the route anyway is told why.
            if core.completer.is_none() {
                Err(Error::Validation("[infer.ask] is not configured".into()))?;
            }
            let completer = core
                .completer
                .clone()
                .ok_or_else(|| Error::Internal("ask without a completer".into()))?;

            // Held for the whole answer, since the query embedding is a model call too; shared with
            // the completion task, which outlives the stream when the reader leaves while the GPU
            // is still busy.
            let lane = std::sync::Arc::new(core.gate.interactive());

            // Asking a question is as deliberate as a search gets.
            let first = core.retrieve_round(&req, &req.q, true).await?;
            let mut hits = first.hits;
            let mut retrieved = first.retrieved;

            if hits.is_empty() {
                // No completion for no retrieval; the sentinel makes it read as an abstention.
                let response = AskResponse {
                    answer: format!("{ABSTAIN_PREFIX}. Nothing matches that question."),
                    citations: vec![],
                    dropped: 0,
                    truncated: false,
                    abstained: true,
                    unsupported: vec![],
                    retired_only: false,
                    disagreements: vec![],
                    event_id: None,
                };
                // Emitted even when empty, so a reader waiting for the rail does not wait forever.
                yield AskEvent::Retrieved { round: 1, retrieved: 0, shown: 0, dropped: 0, cliff_at: None };
                yield AskEvent::Citations(vec![]);
                let response = core.record_ask(&req, &origin, response, &[]).await?;
                yield AskEvent::Done(Box::new(response));
                return;
            }

            let budget = core.excerpt_budget(&req.q);

            // Highest score first, so what gets cut is what mattered least.
            let (mut blocks, mut kept) = core.packed_excerpts(&hits, budget).await;
            core.stitch_passages(&hits[..kept], &mut blocks[..kept]).await;
            let mut ranked = first.ranked;
            let mut dropped = retrieve::dropped_count(&retrieved, &hits[..kept], ranked);
            if dropped > 0 {
                tracing::info!(
                    dropped,
                    kept,
                    "ask: excerpts trimmed to the cliff and to what fits"
                );
            }

            if kept == 0 {
                let response = AskResponse {
                    answer: "The best matching excerpt is too large for the configured context window."
                        .into(),
                    citations: vec![],
                    dropped,
                    truncated: false,
                    // A configuration failure, not a statement about the base.
                    abstained: false,
                    unsupported: vec![],
                    retired_only: false,
                    disagreements: vec![],
                    event_id: None,
                };
                yield AskEvent::Retrieved {
                    round: 1,
                    retrieved: retrieved.len(),
                    shown: 0,
                    dropped,
                    cliff_at: first.cliff_at,
                };
                yield AskEvent::Citations(vec![]);
                let response = core.record_ask(&req, &origin, response, &[]).await?;
                yield AskEvent::Done(Box::new(response));
                return;
            }

            // Reported before the planner runs, so the first round lands visibly.
            yield AskEvent::Retrieved {
                round: 1,
                retrieved: retrieved.len(),
                shown: kept,
                dropped,
                cliff_at: first.cliff_at,
            };

            // One plan, one call site, no loop: the bounded form of an agent, and not an agent.
            let mut uncovered: Vec<String> = Vec::new();
            let queries = plan::needed_queries(&core, &req.q, &blocks[..kept]).await;
            if !queries.is_empty() {
                yield AskEvent::Needs(queries.clone());

                // Concurrent; `PLAN_MAX_QUERIES` caps the plan and so caps this.
                let extra = core.fan_out(&req, &queries).await;
                // Read before the merge, after which no round is left to ask.
                uncovered = plan::uncovered(&extra);

                // Round one first, so round-robin starts with the question as asked.
                let mut parts = vec![retrieve::Part::of(hits.clone(), ranked, first.cliff_at)];
                let mut fanned_retrieved: Vec<String> = Vec::new();
                for round in extra {
                    fanned_retrieved.extend(round.retrieved);
                    parts.push(retrieve::Part::of(round.hits, round.ranked, round.cliff_at));
                }

                // Every planned round failed; round one stands and the page is told the fan-out
                // ended.
                if parts.len() == 1 {
                    tracing::warn!("ask: every planned round failed; answering from the first");
                    yield AskEvent::Retrieved {
                        round: 2,
                        retrieved: retrieved.len(),
                        shown: kept,
                        dropped,
                        cliff_at: None,
                    };
                } else {
                    let merged = retrieve::merge(parts);

                    // Packed again over the whole merged list: the fan-out must fit the same
                    // window.
                    let (mut merged_blocks, merged_kept) =
                        core.packed_excerpts(&merged.hits, budget).await;
                    core.stitch_passages(
                        &merged.hits[..merged_kept],
                        &mut merged_blocks[..merged_kept],
                    )
                    .await;

                    // A re-pack that fits nothing leaves round one standing.
                    if merged_kept > 0 {
                        for id in fanned_retrieved {
                            if !retrieved.contains(&id) {
                                retrieved.push(id);
                            }
                        }
                        hits = merged.hits;
                        ranked = merged.ranked;
                        blocks = merged_blocks;
                        kept = merged_kept;
                        dropped = retrieve::dropped_count(&retrieved, &hits[..kept], ranked);
                    } else {
                        tracing::info!("ask: the fan-out fit nothing the first round had not");
                    }

                    // Reported even when nothing changed, or "looking further…" stays on screen
                    // after `Needs`. `shown` may fall below round one's as planned hits displace
                    // neighbours; no cliff across merged rankings.
                    yield AskEvent::Retrieved {
                        round: 2,
                        retrieved: retrieved.len(),
                        shown: kept,
                        dropped,
                        cliff_at: None,
                    };
                }
            }

            let user = ask_prompt(&req.q, &blocks[..kept]);

            // The ceiling comes from the prompt's actual cost, bounded by the configured one, so
            // prompt plus ceiling still fits the window with `ceiling_for_prompt`'s margin.
            let spent = core.counter.count(ASK_SYSTEM) + core.counter.count(&user);
            let ceiling = crate::infer::budget::ceiling_for_prompt(
                completer.context_tokens(),
                spent,
                completer.max_output_tokens(),
            );

            // Once, after the last retrieval and before the first token.
            let citations: Vec<SearchResult> = hits.into_iter().take(kept).collect();
            yield AskEvent::Citations(citations.clone());

            // The call runs beside this loop: the sink is bounded, and awaiting first would block
            // on long answers.
            let (tx, mut rx) = tokio::sync::mpsc::channel::<crate::infer::Delta>(64);
            let prompt = user.clone();
            let held = std::sync::Arc::clone(&lane);
            let call = tokio::spawn(async move {
                // Dropped when the call returns, whether or not anyone is still reading.
                let _lane = held;
                completer
                    .answer_streaming(ASK_SYSTEM, &prompt, ceiling, tx)
                    .await
            });
            while let Some(delta) = rx.recv().await {
                match delta {
                    crate::infer::Delta::Token(t) => yield AskEvent::Token(t),
                    crate::infer::Delta::Reasoning(r) => yield AskEvent::Reasoning(r),
                }
            }
            // The completion, not the accumulated deltas, so a reader who stopped cannot truncate
            // what is stored.
            let answer = call
                .await
                .map_err(|e| Error::Internal(format!("the answer never finished: {e}")))??;
            if answer.truncated {
                tracing::warn!(
                    ceiling,
                    "ask: the answer hit its output ceiling and is cut off"
                );
            }

            // Checked against exactly the blocks in the prompt; an abstention claims nothing.
            let abstained = abstained(&answer.text);
            let unsupported = match abstained {
                true => vec![],
                false => check::unsupported_literals(&answer.text, &blocks[..kept]),
            };
            if !unsupported.is_empty() {
                tracing::info!(
                    n = unsupported.len(),
                    "ask: the answer carries literals no excerpt does"
                );
            }

            // Off the excerpts shown, not the ranking.
            let retired_only = !citations.is_empty() && citations.iter().all(|c| c.retired);
            if retired_only {
                tracing::info!("ask: every excerpt the answer was written from is retired");
            }
            // Both sides must have been shown. One row per pair, from the side cited first.
            let cited: std::collections::HashSet<&str> =
                citations.iter().map(|c| c.artifact_id.as_str()).collect();
            let mut disagreements: Vec<crate::store::pairs::Disagreement> = Vec::new();
            for c in &citations {
                for d in &c.disagrees_with {
                    let seen = disagreements
                        .iter()
                        .any(|x| x.artifact_id == d.other_id && x.other_id == d.artifact_id);
                    if cited.contains(d.other_id.as_str()) && !seen {
                        disagreements.push(d.clone());
                    }
                }
            }
            let response = AskResponse {
                abstained,
                answer: answer.text,
                citations,
                dropped,
                truncated: answer.truncated,
                unsupported,
                retired_only,
                disagreements,
                event_id: None,
            };
            // Recorded here, not by a door, so one ask is one row.
            let response = core.record_ask(&req, &origin, response, &uncovered).await?;
            yield AskEvent::Done(Box::new(response));
        }
    }
}

/// Dropping these cancels the planned rounds: a dropped `JoinHandle` does not cancel its task.
/// The answer call is deliberately not here; it holds the interactive lane until the GPU is done.
struct Fanned(Vec<tokio::task::JoinHandle<Result<Round>>>);

impl Drop for Fanned {
    fn drop(&mut self) {
        for t in &self.0 {
            t.abort();
        }
    }
}

impl Core {
    /// Concurrent, at most `PLAN_MAX_QUERIES` wide, and awaited in plan order, which the merge
    /// reads as priority. A failed round is dropped with a warning; the others stand.
    async fn fan_out(&self, req: &AskRequest, queries: &[String]) -> Vec<Round> {
        let mut spawned = Fanned(
            queries
                .iter()
                .map(|q| {
                    let (core, req, q) = (self.clone(), req.clone(), q.clone());
                    tokio::spawn(async move { core.retrieve_round(&req, &q, false).await })
                })
                .collect(),
        );

        let mut out = Vec::with_capacity(spawned.0.len());
        for (task, q) in spawned.0.iter_mut().zip(queries) {
            match task.await {
                Ok(Ok(round)) => out.push(round),
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, query = %q, "ask: a planned round failed")
                }
                Err(e) => {
                    tracing::warn!(error = %e, query = %q, "ask: a planned round never finished")
                }
            }
        }
        out
    }

    /// Every round comes through here, so a planned one is retrieved on the question's terms.
    /// `deliberate` is false for planned rounds, so the model's own queries never shape activation.
    async fn retrieve_round(&self, req: &AskRequest, q: &str, deliberate: bool) -> Result<Round> {
        // No per-source cap: an answer often lives in one document.
        let (mut hits, _) = self
            .search_with(
                &SearchQuery {
                    q: q.to_string(),
                    limit: req.limit.unwrap_or(8),
                    tags: req.tags.clone(),
                    category: req.category.clone(),
                    mark: deliberate,
                    include_deprecated: false,
                    include_superseded: false,
                    // Whether ask reranks is the scope's decision; nobody is typing here.
                    rerank: true,
                    explain: false,
                },
                None,
                // Not captured: "which one was it" has no meaning for an answer synthesised across
                // artifacts.
                Door::Ask,
            )
            .await?;

        // Kept before the cliff, so an excerpt it cuts counts in `dropped`.
        let retrieved: Vec<String> = hits.iter().map(|h| h.artifact_id.clone()).collect();

        // Read off the search's marks: only it knows which scale the cliff may be taken on.
        let cliff_at = hits.iter().position(|h| h.past_cliff);
        hits.truncate(retrieve::above_cliff(cliff_at, hits.len()));
        let ranked = hits.len();

        // Reached artifacts are appended after the cliff and never enter its scores; the ranked
        // hits are finished first, since their disagreements decide what the reach brings in.
        self.finish_hits(&mut hits).await;
        self.reach_sideways(&mut hits, cliff_at).await;
        // Neighbours came off the store and missed the titling and disagreement read.
        self.finish_hits(&mut hits[ranked..]).await;

        Ok(Round {
            query: q.to_string(),
            hits,
            ranked,
            retrieved,
            cliff_at,
        })
    }

    /// Packed with long-form caveats, then the kept prefix re-rendered with partners numbered,
    /// which is shorter, so it still fits and no caveat names a cut excerpt.
    async fn packed_excerpts(&self, hits: &[SearchResult], budget: usize) -> (Vec<String>, usize) {
        let mut blocks = self.excerpts(hits, 0).await;
        let kept = pack_by_budget(&blocks, &self.counter, budget);
        if hits[..kept].iter().any(|h| !h.disagrees_with.is_empty()) {
            let numbered = self.excerpts(&hits[..kept], kept).await;
            blocks.splice(..kept, numbered);
        }
        (blocks, kept)
    }

    /// Caveats are read from the store in one query, since they are not in the vector payload.
    /// A partner is named by number only when among the first `shown`; use `packed_excerpts`.
    async fn excerpts(&self, hits: &[SearchResult], shown: usize) -> Vec<String> {
        let ids: Vec<String> = hits.iter().map(|h| h.artifact_id.clone()).collect();
        let caveats = match self.store.caveats_for(&ids).await {
            Ok(m) => m,
            Err(e) => {
                // An answer without caveats beats no answer, but the failure is worth a line.
                tracing::warn!(error = %e, "ask: could not read caveats; excerpts carry none");
                Default::default()
            }
        };
        hits.iter()
            .enumerate()
            .map(|(i, h)| {
                let mut lines: Vec<String> =
                    caveats.get(&h.artifact_id).cloned().unwrap_or_default();
                // A disagreement is a caveat on both notes; passages are never one side of a pair.
                for d in &h.disagrees_with {
                    let detail = d
                        .detail
                        .as_deref()
                        .map(|x| format!(": {x}"))
                        .unwrap_or_default();
                    let partner = hits[..shown.min(hits.len())]
                        .iter()
                        .position(|o| o.artifact_id == d.other_id);
                    lines.push(match partner {
                        Some(j) => format!("excerpt [{}] states this differently{detail}", j + 1),
                        None => format!(
                            "another note, \"{}\" ({}), not among these excerpts, states this \
                             differently{detail}",
                            d.other_title.as_deref().unwrap_or("untitled"),
                            crate::fmt::fmt_day(d.other_created_at),
                        ),
                    });
                }
                // Dated where it takes part in a disagreement, so both readings carry their dates.
                let dated = h
                    .disagrees_with
                    .first()
                    .map(|d| crate::fmt::fmt_day(d.created_at));
                ask_excerpt(
                    i + 1,
                    h.title.as_deref().unwrap_or_default(),
                    dated.as_deref(),
                    &h.text,
                    &lines,
                )
            })
            .collect()
    }

    /// Abutting passages of one `(corpus, segment)` merge into the run's first member's block, in
    /// document order; other members keep their numbered block pointing at it, so every `[n]`
    /// resolves to text the model read.
    async fn stitch_passages(&self, hits: &[SearchResult], blocks: &mut [String]) {
        use crate::store::artifacts::{CorpusSpan, Provenance};
        let ids: Vec<String> = hits.iter().map(|h| h.artifact_id.clone()).collect();
        let rows = match self.store.artifacts_by_ids(&ids).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "ask: could not read spans; excerpts stay unstitched");
                return;
            }
        };
        let mut pos: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for (i, h) in hits.iter().enumerate() {
            pos.insert(h.artifact_id.as_str(), i);
        }
        let mut members: Vec<(String, i64, CorpusSpan, usize)> = rows
            .iter()
            .filter(|c| c.provenance == Provenance::Passage)
            .filter_map(|c| {
                Some((
                    c.corpus_id.clone()?,
                    c.segment_idx?,
                    c.corpus_span.clone()?,
                    *pos.get(c.id.as_str())?,
                ))
            })
            .collect();
        members.sort_by(|a, b| {
            (a.0.as_str(), a.1, a.2.start_line).cmp(&(b.0.as_str(), b.1, b.2.start_line))
        });

        let mut i = 0;
        while i < members.len() {
            let mut run = vec![i];
            while i + 1 < members.len()
                && members[i + 1].0 == members[i].0
                && members[i + 1].1 == members[i].1
                && members[i + 1].2.start_line == members[i].2.end_line + 1
            {
                i += 1;
                run.push(i);
            }
            i += 1;
            if run.len() < 2 {
                continue;
            }
            // `run` is in document order, so its first member is where the text and heading begin.
            let head_pos = members[run[0]].3;
            let head = hits[head_pos].title.as_deref().unwrap_or_default();
            let text: Vec<&str> = run
                .iter()
                .map(|&m| hits[members[m].3].text.as_str())
                .collect();
            // The caveats of every member: the others are no longer rendered anywhere.
            let mut caveats: Vec<String> = Vec::new();
            for &m in &run {
                let id = &hits[members[m].3].artifact_id;
                if let Some(c) = rows.iter().find(|r| &r.id == id) {
                    for cv in &c.caveats {
                        if !caveats.contains(cv) {
                            caveats.push(cv.clone());
                        }
                    }
                }
            }
            blocks[head_pos] = crate::infer::prompt::ask_excerpt(
                head_pos + 1,
                head,
                None,
                &text.join("\n"),
                &caveats,
            );
            // So the model can cite the passage it drew on.
            for &m in &run {
                let p = members[m].3;
                if p != head_pos {
                    blocks[p] = crate::infer::prompt::ask_continues(p + 1, head_pos + 1);
                }
            }
        }
    }

    fn excerpt_budget(&self, question: &str) -> usize {
        // Only reached from `ask_events`, which returned before here without a completer.
        self.completer.as_deref().map_or(0, |completer| {
            excerpt_budget(completer, &self.counter, ASK_SYSTEM, question)
        })
    }

    /// One hop sideways from the best hits: corpus neighbours and one-hop associations.
    /// Structure, not search, so it costs no inference; every failure is best-effort.
    async fn reach_sideways(&self, hits: &mut Vec<SearchResult>, cliff_at: Option<usize>) {
        let anchors = retrieve::anchor_count(cliff_at, hits.len());
        if anchors == 0 {
            return;
        }

        let mut reached: Vec<(String, String, Option<String>)> = Vec::new();
        // Disagreements first: answering from one side alone is the wrong answer the base can
        // avoid.
        for h in hits.iter().take(anchors) {
            for d in &h.disagrees_with {
                reached.push((
                    d.other_id.clone(),
                    h.artifact_id.clone(),
                    Some("states this differently".into()),
                ));
            }
        }
        for h in hits.iter().take(anchors) {
            // For the ordinal, which the vector payload does not carry.
            if let Ok(anchor) = self.store.get_artifact(&h.artifact_id).await
                && let Some(corpus) = anchor.corpus_id.as_deref()
            {
                match self.store.adjacent_artifacts(corpus, anchor.ordinal).await {
                    Ok(next) => reached.extend(
                        next.into_iter()
                            .map(|c| (c.id, h.artifact_id.clone(), None)),
                    ),
                    Err(e) => tracing::warn!(error = %e, "could not read adjacent artifacts"),
                }
            }

            // Only with the associative layer live; same half-life and floor as the results rail.
            if !self.associating() {
                continue;
            }
            match self
                .store
                .links_from(
                    std::slice::from_ref(&h.artifact_id),
                    &[
                        crate::store::links::LinkState::Learning,
                        crate::store::links::LinkState::Related,
                    ],
                    self.associate.half_life_days,
                    crate::core::search::now_secs(),
                    self.associate.show_min,
                    retrieve::NEIGHBOUR_MAX as i64,
                )
                .await
            {
                Ok(links) => reached.extend(links.into_iter().map(|l| (l.other, l.via, l.reason))),
                Err(e) => {
                    tracing::warn!(error = %e, "could not read links; the reach is adjacency only")
                }
            }
        }

        let ranked: Vec<String> = hits.iter().map(|h| h.artifact_id.clone()).collect();
        let ranked_len = ranked.len();

        // Hydrated before a place is spent, so a superseded or deleted candidate never takes one.
        let mut live: Vec<(crate::store::artifacts::Chunk, String, Option<String>)> = Vec::new();
        let mut vetted: std::collections::HashSet<&str> =
            ranked.iter().map(String::as_str).collect();
        for (id, via, reason) in &reached {
            if live.len() == retrieve::NEIGHBOUR_MAX {
                break;
            }
            if !vetted.insert(id.as_str()) {
                continue;
            }
            let Ok(c) = self.store.get_artifact(id).await else {
                continue;
            };
            if !c.in_results() {
                continue;
            }
            live.push((c, via.clone(), reason.clone()));
        }

        // `append_neighbours` stays authoritative about ordering; its dedup and cap are belt and
        // braces here.
        let ids: Vec<String> = live.iter().map(|(c, _, _)| c.id.clone()).collect();
        // A neighbour of a retired note is retired too, or `retired_only` turns false wrongly.
        let cids: Vec<String> = live
            .iter()
            .filter_map(|(c, _, _)| c.corpus_id.clone())
            .collect();
        // Not defaulted to empty, which would read as "nothing is retired"; skip the neighbours
        // instead.
        let retired = match self.store.retired_among(&cids).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "could not read retirement for the reached neighbours; reaching no further");
                return;
            }
        };
        let merged = retrieve::append_neighbours(ranked, ids, retrieve::NEIGHBOUR_MAX);

        for id in merged.into_iter().skip(ranked_len) {
            let Some(i) = live.iter().position(|(c, _, _)| c.id == id) else {
                continue;
            };
            let (c, via, reason) = live.swap_remove(i);
            let corpus_id = c.corpus_id.unwrap_or_default();
            let is_retired = retired.contains(&corpus_id);
            hits.push(SearchResult {
                artifact_id: c.id,
                corpus_id,
                title: c.title,
                text: c.text,
                category: c.category,
                tags: c.tags,
                // Not a rank or similarity: it was reached, and `record_ask` stores this.
                score: 0.0,
                status: Some(c.status),
                superseded_by: c.superseded_by,
                last_verified_at: c.last_verified_at,
                model_written: c.provenance.is_model_written(),
                synthesized: c.provenance == crate::store::artifacts::Provenance::Synthesized,
                origin_count: 0,
                // Weakness needs a similarity to the query, and there is none here.
                weak: false,
                primed: false,
                due_at: None,
                due_in: None,
                in_sitting: false,
                // The cliff was computed over scores this one was never in.
                past_cliff: false,
                retired: is_retired,
                similarity: None,
                // As `model_written` above: a reached passage carries its section's heading too.
                borrowed_name: !c.provenance.names_its_own_text(),
                // A ranked hit has no `via`; this one names the hit it was reached from.
                disagrees_with: Vec::new(),
                explanation: Some(crate::core::explain::HitExplanation::recalled(&via)),
                via: Some(via),
                reason,
            });
        }
    }

    /// Records only from the UI and with feedback on; a failure is logged and the answer still goes
    /// out.
    async fn record_ask(
        &self,
        req: &AskRequest,
        origin: &Origin,
        mut response: AskResponse,
        uncovered: &[String],
    ) -> Result<AskResponse> {
        // A person composing a question where they will read the answer; never an agent on a token.
        if !(self.learn.enabled && matches!(origin.door, Door::Ui | Door::Cli | Door::App)) {
            return Ok(response);
        }
        let ask = NewAsk {
            question: req.q.trim().to_string(),
            scope: origin.scope.clone(),
            filters: serde_json::json!({
                "tags": req.tags,
                "category": req.category,
                "limit": req.limit.unwrap_or(8),
            })
            .to_string(),
            query_vec: self.cached_query_vector(&req.q).unwrap_or_default(),
            embed_model: self.embedder.model().to_string(),
            answer: response.answer.clone(),
            abstained: response.abstained,
            dropped: response.dropped,
            truncated: response.truncated,
            // The number recorded is the number shown.
            unsupported: response.unsupported.len(),
            // What the answer referenced, not what it was shown: the sweep reads these as
            // engagement.
            citations: {
                let used = check::referenced(&response.answer, response.citations.len());
                response
                    .citations
                    .iter()
                    .zip(used)
                    .map(|(c, used)| NewAskCitation {
                        artifact_id: c.artifact_id.clone(),
                        score: c.score,
                        used,
                    })
                    .collect()
            },
        };
        // Off the recording path's success: an insert that fails does not unmake the use.
        self.mark_artifacts_cited(
            ask.citations
                .iter()
                .filter(|c| c.used)
                .map(|c| c.artifact_id.clone())
                .collect(),
        );
        match self.store.record_ask(ask).await {
            Ok(id) => {
                // Only where the question was recorded, as gaps hang off it; the vector comes from
                // the query cache.
                for subject in uncovered {
                    let vec = self.cached_query_vector(subject).unwrap_or_default();
                    if let Err(e) = self
                        .store
                        .record_uncovered_subject(&id, subject, &vec, self.embedder.model())
                        .await
                    {
                        tracing::warn!(error = %e, subject, "could not record an uncovered subject");
                    }
                }
                response.event_id = Some(id);
            }
            Err(e) => tracing::warn!(error = %e, "could not record the question"),
        }
        Ok(response)
    }
}

/// Reserves the completer's output ceiling plus `ceiling_for_prompt`'s margin, at most half the
/// window, so a role whose ceiling equals its context still packs something. Shared by the answer
/// and plan calls.
pub(super) fn excerpt_budget(
    model: &dyn crate::infer::Completer,
    counter: &crate::infer::budget::TokenCounter,
    system: &str,
    question: &str,
) -> usize {
    let context = model.context_tokens();
    let reserve = model
        .max_output_tokens()
        .saturating_add(crate::infer::budget::MAX_HEADROOM_TOKENS)
        .min(context / 2);
    context
        .saturating_sub(counter.count(system))
        .saturating_sub(counter.count(question))
        .saturating_sub(reserve)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_support::test_core;
    use crate::store::artifacts::NewArtifact;
    use crate::store::feedback::Door;

    struct LaneProbe {
        gate: std::sync::Arc<crate::infer::gate::InferenceGate>,
        background_was_held: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl crate::infer::Completer for LaneProbe {
        async fn complete(&self, _system: &str, _user: &str) -> Result<String> {
            let held =
                tokio::time::timeout(std::time::Duration::from_millis(50), self.gate.background())
                    .await
                    .is_err();
            self.background_was_held
                .store(held, std::sync::atomic::Ordering::SeqCst);
            Ok("an answer".into())
        }
        fn context_tokens(&self) -> usize {
            4096
        }
        fn max_output_tokens(&self) -> usize {
            1024
        }
    }

    #[tokio::test]
    async fn a_question_holds_the_interactive_lane_for_its_whole_answer() {
        // Priority, not preemption: the window already running (~73 s) is not stopped.
        let mut core = test_core().await;
        let probe = std::sync::Arc::new(LaneProbe {
            gate: std::sync::Arc::clone(&core.gate),
            background_was_held: std::sync::atomic::AtomicBool::new(false),
        });
        core.completer = Some(probe.clone());
        seed(&core, 3, 4).await;

        core.ask(
            &AskRequest {
                q: "chunk".into(),
                limit: Some(3),
                tags: vec![],
                category: None,
            },
            Door::Api,
        )
        .await
        .unwrap();

        assert!(
            probe
                .background_was_held
                .load(std::sync::atomic::Ordering::SeqCst),
            "a window could have started while the question was still being answered"
        );
    }

    #[tokio::test]
    async fn asking_a_question_leaves_the_lane_free_afterwards() {
        let core = test_core().await;
        seed(&core, 3, 4).await;

        core.ask(
            &AskRequest {
                q: "chunk".into(),
                limit: Some(3),
                tags: vec![],
                category: None,
            },
            Door::Api,
        )
        .await
        .unwrap();

        // Returns immediately if the lane was released; hangs the test if not.
        tokio::time::timeout(std::time::Duration::from_secs(5), core.gate.background())
            .await
            .expect("the interactive lane was never released");
    }

    struct Counting {
        reply: String,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl Counting {
        fn saying(reply: &str) -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                reply: reply.into(),
                calls: std::sync::atomic::AtomicUsize::new(0),
            })
        }
        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl crate::infer::Completer for Counting {
        async fn complete(&self, _system: &str, _user: &str) -> Result<String> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(self.reply.clone())
        }
        fn context_tokens(&self) -> usize {
            4096
        }
        fn max_output_tokens(&self) -> usize {
            1024
        }
    }

    async fn core_with_planner(reply: Option<&str>) -> (Core, std::sync::Arc<Counting>) {
        let mut core = test_core().await;
        let model = Counting::saying(reply.unwrap_or_default());
        core.planner = reply.map(|_| model.clone() as std::sync::Arc<dyn crate::infer::Completer>);
        seed(&core, 3, 4).await;
        (core, model)
    }

    async fn rounds_and_needs(core: &Core) -> (Vec<u8>, Vec<String>) {
        let (mut rounds, mut needs) = (vec![], vec![]);
        let s = core.ask_events(&req("chunk"), Door::Api);
        tokio::pin!(s);
        while let Some(ev) = s.next().await {
            match ev.unwrap() {
                AskEvent::Retrieved { round, .. } => rounds.push(round),
                AskEvent::Needs(queries) => needs.extend(queries),
                _ => {}
            }
        }
        (rounds, needs)
    }

    #[tokio::test]
    async fn planning_off_makes_no_extra_call() {
        let mut core = test_core().await;
        let model = Counting::saying("fake answer");
        core.completer = Some(model.clone());
        seed(&core, 3, 4).await;
        assert!(core.planner.is_none(), "the test core wired a planner");

        core.ask(&req("chunk"), Door::Api).await.unwrap();

        assert_eq!(
            model.calls(),
            1,
            "with planning off, the only model call is the answer"
        );
    }

    #[tokio::test]
    async fn an_empty_plan_skips_the_fan_out() {
        let (core, model) = core_with_planner(Some(r#"{"need": []}"#)).await;
        let (rounds, needs) = rounds_and_needs(&core).await;
        assert_eq!(rounds, vec![1]);
        assert!(needs.is_empty(), "nothing was needed, so nothing was said");
        assert_eq!(model.calls(), 1, "asked once, and once is the whole budget");
    }

    #[tokio::test]
    async fn a_plan_is_asked_for_once_and_never_planned_again() {
        let (core, model) = core_with_planner(Some(r#"{"need": ["ticker interval"]}"#)).await;
        let (rounds, needs) = rounds_and_needs(&core).await;
        assert_eq!(rounds, vec![1, 2], "exactly one fan-out, never a loop");
        assert_eq!(needs, vec!["ticker interval".to_string()]);
        assert_eq!(
            model.calls(),
            1,
            "a plan that plans again is a third round waiting to happen"
        );
    }

    #[tokio::test]
    async fn a_planned_subject_the_base_could_not_cover_becomes_a_gap() {
        let (mut core, _) = core_with_planner(Some(r#"{"need": ["mounting an E01"]}"#)).await;
        core.learn.enabled = true;
        // Cosine tops out at 1, so every candidate is under this.
        core.set_weak_below(1.0);
        core.ask(&req("chunk"), Door::Ui.by("me")).await.unwrap();

        let gaps = core
            .store
            .open_gap_refs(core.embedder.model(), core.weak_below())
            .await
            .unwrap();
        let subjects: Vec<&str> = gaps
            .iter()
            .filter(|g| g.kind == crate::store::gaps::GapKind::Subject)
            .map(|g| g.text.as_str())
            .collect();
        assert_eq!(subjects, vec!["mounting an E01"]);
    }

    #[tokio::test]
    async fn the_api_door_names_no_subjects() {
        let (mut core, _) = core_with_planner(Some(r#"{"need": ["mounting an E01"]}"#)).await;
        core.learn.enabled = true;
        core.set_weak_below(1.0);
        core.ask(&req("chunk"), Door::Api).await.unwrap();

        let gaps = core
            .store
            .open_gap_refs(core.embedder.model(), core.weak_below())
            .await
            .unwrap();
        assert!(
            !gaps
                .iter()
                .any(|g| g.kind == crate::store::gaps::GapKind::Subject),
            "a door that records no question recorded what its plan named"
        );
    }

    #[tokio::test]
    async fn a_plan_naming_several_subjects_fans_out_to_all_of_them_at_once() {
        let (core, model) =
            core_with_planner(Some(r#"{"need": ["one", "two", "three", "four"]}"#)).await;
        let (rounds, needs) = rounds_and_needs(&core).await;
        assert_eq!(
            rounds,
            vec![1, 2],
            "the fan-out is reported once, not per query"
        );
        assert_eq!(
            needs,
            vec!["one".to_string(), "two".to_string(), "three".to_string()],
            "the plan ran wider than `PLAN_MAX_QUERIES` allows"
        );
        assert_eq!(model.calls(), 1);
    }

    #[tokio::test]
    async fn the_fan_out_merges_deduped_by_artifact() {
        let (core, _) = core_with_planner(Some(r#"{"need": ["filler", "chunk filler"]}"#)).await;
        let out = core.ask(&req("chunk"), Door::Api).await.unwrap();
        let mut ids: Vec<&str> = out
            .citations
            .iter()
            .map(|c| c.artifact_id.as_str())
            .collect();
        let seen = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), seen, "an artifact was shown to the model twice");
    }

    #[tokio::test]
    async fn a_planned_query_that_is_the_question_back_is_not_worth_a_round() {
        let (core, model) = core_with_planner(Some(r#"{"need": ["CHUNK "]}"#)).await;
        let (rounds, needs) = rounds_and_needs(&core).await;
        assert_eq!(rounds, vec![1], "the question was searched for twice");
        assert!(needs.is_empty());
        assert_eq!(
            model.calls(),
            1,
            "the call still happened; only its answer was refused"
        );
    }

    #[tokio::test]
    async fn an_echoed_query_is_dropped_without_taking_the_rest_of_the_plan_with_it() {
        let (core, _) = core_with_planner(Some(r#"{"need": ["chunk", "filler"]}"#)).await;
        let (rounds, needs) = rounds_and_needs(&core).await;
        assert_eq!(rounds, vec![1, 2], "the surviving query never ran");
        assert_eq!(needs, vec!["filler".to_string()]);
    }

    struct Keyed;

    #[async_trait::async_trait]
    impl crate::infer::Embedder for Keyed {
        async fn embed_raw(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            Ok(texts
                .iter()
                .map(|t| {
                    let mut v = vec![0f32; crate::core::test_support::TEST_DIM];
                    v[0] = t.contains("alpha") as u8 as f32;
                    v[1] = t.contains("beta") as u8 as f32;
                    v[2] = t.contains("gamma") as u8 as f32;
                    // Never the zero vector: a cosine against nothing is not a ranking.
                    v[3] = (v[0] == 0.0 && v[1] == 0.0 && v[2] == 0.0) as u8 as f32;
                    v
                })
                .collect())
        }
        fn templates(&self) -> &crate::config::EmbedTemplates {
            static LEGACY: std::sync::LazyLock<crate::config::EmbedTemplates> =
                std::sync::LazyLock::new(crate::config::EmbedTemplates::legacy);
            &LEGACY
        }
        fn dim(&self) -> usize {
            crate::core::test_support::TEST_DIM
        }
        fn model(&self) -> &str {
            "fake-keyed"
        }
        fn max_input_tokens(&self) -> usize {
            8192
        }
    }

    async fn seed_topics(core: &Core, topics: &[&str]) {
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let new: Vec<NewArtifact> = (0..topics.len() as i64 * 3)
            .map(|i| NewArtifact {
                ordinal: i,
                text: format!("{} topic {i} filler filler", topics[i as usize / 3]),
                title: Some(format!("t{i}")),
                category: Some("reference".into()),
                ..Default::default()
            })
            .collect();
        let made = core.store.insert_artifacts(&src.id, &new).await.unwrap();
        for c in &made {
            crate::jobs::embed::run(core, &c.id).await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_planned_round_shows_what_the_first_rounds_cliff_cut() {
        let mut core = test_core().await;
        core.embedder = std::sync::Arc::new(Keyed);
        core.planner = Some(Counting::saying(r#"{"need": ["beta"]}"#));
        seed_topics(&core, &["alpha", "beta"]).await;

        let (mut rounds, mut shown) = (vec![], vec![]);
        let s = core.ask_events(&req("alpha"), Door::Api);
        tokio::pin!(s);
        while let Some(ev) = s.next().await {
            if let AskEvent::Retrieved {
                round, shown: n, ..
            } = ev.unwrap()
            {
                rounds.push(round);
                shown.push(n);
            }
        }
        assert_eq!(rounds, vec![1, 2]);
        assert!(
            shown[0] < 6,
            "round one already had everything, so the fan-out proves nothing: {shown:?}"
        );

        let out = core.ask(&req("alpha"), Door::Api).await.unwrap();
        let ids: std::collections::HashSet<&str> = out
            .citations
            .iter()
            .map(|c| c.artifact_id.as_str())
            .collect();
        assert_eq!(ids.len(), 6, "the planned round did not add its half");
        assert_eq!(
            out.dropped, 0,
            "an artifact the model was shown was reported missing"
        );
    }

    #[tokio::test]
    async fn a_plan_naming_two_subjects_puts_both_of_them_in_front_of_the_model() {
        let mut core = test_core().await;
        core.embedder = std::sync::Arc::new(Keyed);
        core.planner = Some(Counting::saying(r#"{"need": ["beta", "gamma"]}"#));
        seed_topics(&core, &["alpha", "beta", "gamma"]).await;

        let out = core.ask(&req("alpha"), Door::Api).await.unwrap();
        let seen: String = out.citations.iter().map(|c| c.text.as_str()).collect();
        for topic in ["alpha", "beta", "gamma"] {
            assert!(
                seen.contains(topic),
                "the fan-out left `{topic}` out of the prompt"
            );
        }
        assert_eq!(
            out.dropped, 0,
            "an artifact a planned round retrieved was reported missing"
        );
    }

    #[tokio::test]
    async fn a_planned_round_that_fails_leaves_the_rest_of_the_fan_out_standing() {
        let mut core = test_core().await;
        core.embedder = std::sync::Arc::new(Keyed);
        seed_topics(&core, &["alpha", "beta"]).await;

        let queries = ["".to_string(), "beta".to_string()];
        let rounds = core.fan_out(&req("alpha"), &queries).await;

        assert_eq!(rounds.len(), 1, "the failure took the good round with it");
        let seen: String = rounds[0].hits.iter().map(|h| h.text.as_str()).collect();
        assert!(seen.contains("beta"), "the wrong round survived");
    }

    #[tokio::test]
    async fn a_failed_plan_leaves_the_ask_a_single_round() {
        struct Failing;
        #[async_trait::async_trait]
        impl crate::infer::Completer for Failing {
            async fn complete(&self, _system: &str, _user: &str) -> Result<String> {
                Err(Error::Inference {
                    role: "plan",
                    detail: "the endpoint is loading".into(),
                })
            }
            fn context_tokens(&self) -> usize {
                4096
            }
            fn max_output_tokens(&self) -> usize {
                1024
            }
        }
        let mut core = test_core().await;
        core.planner = Some(std::sync::Arc::new(Failing));
        seed(&core, 3, 4).await;

        let (rounds, needs) = rounds_and_needs(&core).await;
        assert_eq!(rounds, vec![1]);
        assert!(needs.is_empty());
        assert_eq!(
            core.ask(&req("chunk"), Door::Api).await.unwrap().answer,
            "fake answer",
            "a failed plan cost the answer that was already retrievable"
        );
    }

    #[tokio::test]
    async fn an_unparsable_plan_leaves_the_ask_a_single_round() {
        let (core, _) = core_with_planner(Some("I would look for the ticker interval")).await;
        let (rounds, needs) = rounds_and_needs(&core).await;
        assert_eq!(rounds, vec![1]);
        assert!(needs.is_empty());
    }

    async fn seed(core: &crate::core::Core, n: usize, size: usize) {
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let new: Vec<NewArtifact> = (0..n)
            .map(|i| NewArtifact {
                ordinal: i as i64,
                text: format!("chunk {i} ") + &"filler ".repeat(size),
                title: Some(format!("t{i}")),
                category: Some("reference".into()),
                ..Default::default()
            })
            .collect();
        let made = core.store.insert_artifacts(&src.id, &new).await.unwrap();
        for c in &made {
            crate::jobs::embed::run(core, &c.id).await.unwrap();
        }
    }

    fn req(q: &str) -> AskRequest {
        AskRequest {
            q: q.into(),
            limit: None,
            tags: vec![],
            category: None,
        }
    }

    #[tokio::test]
    async fn a_question_asked_at_a_shell_is_recorded() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        seed(&core, 2, 2).await;
        core.ask(&req("how do I do the thing"), Door::Cli.by("me"))
            .await
            .unwrap();
        let asks = core
            .store
            .asks_between(0, crate::store::now() + 1)
            .await
            .unwrap();
        assert_eq!(asks.len(), 1, "{asks:?}");
        assert_eq!(
            asks[0].scope.as_deref(),
            Some("me"),
            "unscoped, the sweep cannot join it to anything"
        );
    }

    #[tokio::test]
    async fn a_question_from_an_agent_is_still_not_recorded() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        seed(&core, 2, 2).await;
        core.ask(&req("how do I do the thing"), Door::Api)
            .await
            .unwrap();
        core.ask(&req("how do I do the other thing"), Door::Mcp)
            .await
            .unwrap();
        assert!(
            core.store
                .asks_between(0, crate::store::now() + 1)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn ask_returns_an_answer_with_the_chunks_it_used() {
        let core = test_core().await;
        seed(&core, 2, 2).await;
        let out = core
            .ask(&req("how do I do the thing"), Door::Api)
            .await
            .unwrap();
        assert_eq!(out.answer, "fake answer");
        assert!(
            !out.citations.is_empty(),
            "an answer with no citations is unverifiable"
        );
    }

    #[tokio::test]
    async fn the_model_is_shown_the_caveats_of_every_excerpt() {
        let mut core = test_core().await;
        core.completer = Some(std::sync::Arc::new(crate::infer::fake::FakeCompleter {
            reply: None,
        }));
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    text: "Format the device with mkfs.".into(),
                    title: Some("Format a device".into()),
                    caveats: vec!["Destroys every existing file on the device.".into()],
                    ..Default::default()
                }],
            )
            .await
            .unwrap();
        crate::jobs::embed::run(&core, &made[0].id).await.unwrap();

        let out = core
            .ask(&req("how do I format a device"), Door::Api)
            .await
            .unwrap();
        assert!(
            out.answer
                .contains("Caveat: Destroys every existing file on the device."),
            "the caveat never reached the model: {}",
            out.answer
        );
    }

    #[tokio::test]
    async fn ask_reports_chunks_dropped_for_budget() {
        let core = test_core().await;
        // FakeCompleter reports a 4096-token context; oversized excerpts force
        // some to be left out.
        seed(&core, 20, 400).await;
        let out = core.ask(&req("anything"), Door::Api).await.unwrap();
        assert!(
            out.dropped > 0,
            "a silently dropped citation is worse than a reported one"
        );
        assert!(out.citations.len() < 20);
    }

    struct Ceilinged {
        context: usize,
        max_output: usize,
        asked_for: std::sync::atomic::AtomicUsize,
    }

    impl Ceilinged {
        fn new(context: usize, max_output: usize) -> Self {
            Self {
                context,
                max_output,
                asked_for: std::sync::atomic::AtomicUsize::new(0),
            }
        }
        fn ceiling(&self) -> usize {
            self.asked_for.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl crate::infer::Completer for Ceilinged {
        async fn complete(&self, _system: &str, user: &str) -> Result<String> {
            Ok(user.to_string())
        }
        async fn answer(
            &self,
            _system: &str,
            user: &str,
            ceiling: usize,
        ) -> Result<crate::infer::Completion> {
            self.asked_for
                .store(ceiling, std::sync::atomic::Ordering::SeqCst);
            Ok(crate::infer::Completion {
                text: user.to_string(),
                truncated: false,
            })
        }
        fn context_tokens(&self) -> usize {
            self.context
        }
        fn max_output_tokens(&self) -> usize {
            self.max_output
        }
    }

    #[tokio::test]
    async fn what_is_packed_leaves_room_for_the_reply_that_was_asked_for() {
        use crate::infer::budget::MAX_HEADROOM_TOKENS;

        for (context, max_output) in [(4096, 3072), (8192, 1024), (4096, 4096), (32768, 2048)] {
            let completer = std::sync::Arc::new(Ceilinged::new(context, max_output));
            let mut core = test_core().await;
            core.completer = Some(completer.clone());
            // Sized so packing actually fills the window in every case.
            seed(&core, 20, context / 20).await;
            let mut r = req("anything");
            r.limit = Some(20);
            let out = core.ask(&r, Door::Api).await.unwrap();

            let prompt = core.counter.count(&out.answer) + core.counter.count(ASK_SYSTEM);
            let ceiling = completer.ceiling();
            assert!(
                prompt + ceiling <= context,
                "at context {context} / ceiling {max_output}: the prompt was {prompt} tokens \
                 and the call asked for {ceiling} more, which is {} over the window",
                prompt + ceiling - context
            );
            assert!(
                ceiling > 0 && ceiling <= max_output,
                "at context {context} / ceiling {max_output}: the call asked for {ceiling}, \
                 which is not a ceiling the role allows"
            );

            // Usable, not merely positive: `> 0` let a ceiling of 1 through at the ordinary 32k/2k
            // shape.
            let floor = max_output.min(context / 2) - MAX_HEADROOM_TOKENS;
            assert!(
                ceiling >= floor,
                "at context {context} / ceiling {max_output}: the call asked for {ceiling}, \
                 but packing reserved room for at least {floor}"
            );
        }
    }

    #[tokio::test]
    async fn a_ceiling_as_wide_as_its_window_still_answers() {
        let mut core = test_core().await;
        core.completer = Some(std::sync::Arc::new(Ceilinged::new(4096, 4096)));
        seed(&core, 3, 4).await;
        let out = core.ask(&req("chunk"), Door::Api).await.unwrap();
        assert!(
            !out.citations.is_empty(),
            "nothing was packed, so nothing was answered: {}",
            out.answer
        );
    }

    #[tokio::test]
    async fn an_answer_cut_off_by_its_ceiling_says_so() {
        struct Truncating;
        #[async_trait::async_trait]
        impl crate::infer::Completer for Truncating {
            async fn complete(&self, _system: &str, _user: &str) -> Result<String> {
                Ok("half an ans".into())
            }
            async fn answer(
                &self,
                _system: &str,
                _user: &str,
                _ceiling: usize,
            ) -> Result<crate::infer::Completion> {
                Ok(crate::infer::Completion {
                    text: "half an ans".into(),
                    truncated: true,
                })
            }
            fn context_tokens(&self) -> usize {
                4096
            }
            fn max_output_tokens(&self) -> usize {
                1024
            }
        }

        let mut core = test_core().await;
        core.completer = Some(std::sync::Arc::new(Truncating));
        seed(&core, 3, 4).await;
        let out = core.ask(&req("chunk"), Door::Api).await.unwrap();
        assert!(
            out.truncated,
            "a cut-off answer was reported as a whole one"
        );
    }

    struct Cliffed;

    #[async_trait::async_trait]
    impl crate::infer::Reranker for Cliffed {
        async fn rerank(
            &self,
            _query: &str,
            docs: &[String],
            top_n: usize,
        ) -> Result<Vec<(usize, f32)>> {
            const SCORES: [f32; 5] = [0.9, 0.88, 0.86, 0.20, 0.19];
            Ok((0..docs.len().min(SCORES.len()).min(top_n))
                .map(|i| (i, SCORES[i]))
                .collect())
        }
    }

    #[tokio::test]
    async fn the_cliff_cuts_the_excerpts_the_window_would_have_kept() {
        // The budget cuts nothing here, so anything missing was cut by the cliff.
        let mut core = test_core().await;
        core.reranker = Some(std::sync::Arc::new(Cliffed));
        seed(&core, 5, 4).await;

        let out = core
            .ask(
                &AskRequest {
                    q: "chunk".into(),
                    limit: Some(5),
                    tags: vec![],
                    category: None,
                },
                Door::Api,
            )
            .await
            .unwrap();

        // Reached neighbours never competed; `via` tells them apart.
        assert_eq!(
            ranked(&out),
            3,
            "the two excerpts below the cliff were still shown to the model"
        );
        assert_eq!(
            ranked(&out) + out.dropped,
            5,
            "an excerpt cut by the cliff must be reported, not lost silently"
        );
    }

    fn ranked(out: &AskResponse) -> usize {
        out.citations.iter().filter(|c| c.via.is_none()).count()
    }

    #[tokio::test]
    async fn the_artifact_beside_a_hit_is_reached_even_though_the_cliff_cut_it() {
        let mut core = test_core().await;
        core.reranker = Some(std::sync::Arc::new(Cliffed));
        seed(&core, 5, 4).await;

        let out = core
            .ask(
                &AskRequest {
                    q: "chunk".into(),
                    limit: Some(5),
                    tags: vec![],
                    category: None,
                },
                Door::Api,
            )
            .await
            .unwrap();

        let reached: Vec<&SearchResult> =
            out.citations.iter().filter(|c| c.via.is_some()).collect();
        assert!(
            !reached.is_empty(),
            "nothing was reached sideways: {:?}",
            out.citations
                .iter()
                .map(|c| c.title.as_deref())
                .collect::<Vec<_>>()
        );
        for c in &reached {
            let via = c.via.as_deref().unwrap();
            assert!(
                out.citations
                    .iter()
                    .take(ranked(&out))
                    .any(|r| r.artifact_id == via),
                "a neighbour named an anchor that is not one of the ranked hits"
            );
        }
    }

    #[tokio::test]
    async fn every_reached_artifact_sits_after_every_ranked_one_and_carries_no_score() {
        let mut core = test_core().await;
        core.reranker = Some(std::sync::Arc::new(Cliffed));
        seed(&core, 5, 4).await;

        let out = core
            .ask(
                &AskRequest {
                    q: "chunk".into(),
                    limit: Some(5),
                    tags: vec![],
                    category: None,
                },
                Door::Api,
            )
            .await
            .unwrap();

        let first_reached = out.citations.iter().position(|c| c.via.is_some());
        assert!(first_reached.is_some(), "nothing was reached sideways");
        assert!(
            out.citations
                .iter()
                .skip(first_reached.unwrap())
                .all(|c| c.via.is_some()),
            "a ranked hit was interleaved behind a neighbour"
        );
        for c in out.citations.iter().filter(|c| c.via.is_some()) {
            assert_eq!(
                c.score, 0.0,
                "a reached artifact was given a score it never earned"
            );
            assert!(!c.past_cliff && !c.primed && !c.weak);
        }

        // The only test of the call site's cap.
        assert!(
            out.citations.len() <= ranked(&out) + retrieve::NEIGHBOUR_MAX,
            "{} citations against {} ranked hits is past the cap",
            out.citations.len(),
            ranked(&out)
        );
    }

    #[tokio::test]
    async fn a_reached_neighbour_is_never_counted_as_a_dropped_citation() {
        let mut core = test_core().await;
        core.reranker = Some(std::sync::Arc::new(Cliffed));
        seed(&core, 5, 4).await;

        let out = core
            .ask(
                &AskRequest {
                    q: "chunk".into(),
                    limit: Some(5),
                    tags: vec![],
                    category: None,
                },
                Door::Api,
            )
            .await
            .unwrap();

        assert!(
            out.citations.len() > ranked(&out),
            "the reach added nothing, so this proves nothing"
        );
        assert_eq!(
            out.dropped, 2,
            "only the two hits the cliff cut are missing from the answer"
        );
    }

    #[tokio::test]
    async fn an_artifact_linked_to_a_hit_is_reached_where_adjacency_cannot_go() {
        let mut core = test_core().await;
        // `test_core` records no searches, so the reach stays shut until the layer is on.
        core.learn.enabled = true;
        seed(&core, 5, 4).await;

        // Different raw text: `insert_corpus` dedups on the shingle signature, which would make it
        // adjacent.
        let other = core
            .store
            .insert_corpus("a different document entirely", "web", None)
            .await
            .unwrap();
        assert_ne!(
            core.store
                .get_artifact(&core.store.list_all_artifact_ids().await.unwrap()[0])
                .await
                .unwrap()
                .corpus_id
                .as_deref(),
            Some(other.id.as_str()),
            "the two corpora collapsed into one, and adjacency could reach across"
        );
        let made = core
            .store
            .insert_artifacts(
                &other.id,
                &[NewArtifact {
                    text: "Reconciling the quarterly ledger against the register.".into(),
                    title: Some("ledger".into()),
                    category: Some("reference".into()),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();
        // No vector, so retrieval cannot return it: structure is the only route.
        let far = made[0].id.clone();

        // Weight well above `associate.show_min`, at the clock the reach decays to.
        let now = crate::core::search::now_secs();
        for id in core.store.list_all_artifact_ids().await.unwrap() {
            core.store
                .bump_link(
                    &id,
                    &far,
                    9.0,
                    Some("chunk"),
                    core.associate.half_life_days,
                    now,
                )
                .await
                .unwrap();
        }

        let out = core
            .ask(
                &AskRequest {
                    q: "chunk".into(),
                    limit: Some(3),
                    tags: vec![],
                    category: None,
                },
                Door::Api,
            )
            .await
            .unwrap();

        let cited = out
            .citations
            .iter()
            .find(|c| c.artifact_id == far)
            .expect("the linked artifact never reached the prompt");
        let via = cited
            .via
            .as_deref()
            .expect("the linked artifact arrived as though it had been retrieved");
        assert!(
            out.citations
                .iter()
                .take(ranked(&out))
                .any(|r| r.artifact_id == via),
            "a reached artifact named an anchor that is not one of the ranked hits"
        );
        assert_eq!(cited.score, 0.0, "a link is not a score");
    }

    #[tokio::test]
    async fn citations_match_exactly_what_the_model_was_shown() {
        let core = test_core().await;
        seed(&core, 20, 400).await;
        let out = core.ask(&req("anything"), Door::Api).await.unwrap();
        assert_eq!(
            ranked(&out) + out.dropped,
            8,
            "citations plus dropped must account for every retrieved excerpt"
        );
    }

    #[tokio::test]
    async fn ask_with_no_matches_says_so_without_calling_the_model() {
        let core = test_core().await;
        let out = core
            .ask(&req("nothing is stored"), Door::Api)
            .await
            .unwrap();
        assert!(out.citations.is_empty());
        assert!(
            out.answer.to_lowercase().contains("nothing"),
            "got: {}",
            out.answer
        );
    }

    #[tokio::test]
    async fn empty_question_is_rejected() {
        let core = test_core().await;
        assert!(matches!(
            core.ask(&req("  "), Door::Api).await,
            Err(crate::error::Error::Validation(_))
        ));
    }

    #[tokio::test]
    async fn a_ui_ask_is_recorded_with_its_citations_when_feedback_is_on() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        seed(&core, 3, 4).await;
        let out = core.ask(&req("chunk"), Door::Ui.by("me")).await.unwrap();
        let id = out.event_id.expect("a UI ask is recorded");
        let ev = core.store.ask_event(&id).await.unwrap().expect("stored");
        assert_eq!(ev.question, "chunk");
        assert_eq!(ev.answer, out.answer);
        assert_eq!(
            ev.citations
                .iter()
                .map(|c| c.artifact_id.as_str())
                .collect::<Vec<_>>(),
            out.citations
                .iter()
                .map(|c| c.artifact_id.as_str())
                .collect::<Vec<_>>(),
            "the stored citations must be exactly the excerpts the model saw, in order"
        );
        assert!(!out.abstained);
        let dim: i64 = sqlx::query_scalar!("SELECT vec_dim FROM ask_events WHERE id = ?", &id)
            .fetch_one(&core.store.pool)
            .await
            .unwrap();
        assert!(dim > 0);
    }

    #[tokio::test]
    async fn only_the_artifacts_an_answer_cited_are_engaged_by_it() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        core.completer = Some(std::sync::Arc::new(crate::infer::fake::FakeCompleter {
            reply: Some("the answer rests on this [1]".into()),
        }));
        seed(&core, 3, 4).await;

        let out = core.ask(&req("chunk"), Door::Ui.by("me")).await.unwrap();
        core.background.wait_idle().await;

        assert!(out.citations.len() > 1, "need a shown-but-uncited one");
        let ids: Vec<String> = out
            .citations
            .iter()
            .map(|c| c.artifact_id.clone())
            .collect();
        // Same searches, same baseline: the only difference left is the citation.
        let act = core.store.activation_of(&ids).await.unwrap();
        let of = |id: &String| act.get(id).map(|(a, _, _)| *a).unwrap_or(0.0);
        let uncited = of(&ids[1]);
        for id in &ids[2..] {
            assert_eq!(of(id), uncited, "the excerpts did not share a baseline");
        }
        assert!(
            (of(&ids[0]) - uncited - core.activation.cited).abs() < 1e-6,
            "cited {} against uncited {uncited}, expected a lift of {}",
            of(&ids[0]),
            core.activation.cited
        );
    }

    #[tokio::test]
    async fn an_api_or_mcp_ask_is_never_recorded() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        seed(&core, 3, 4).await;
        for door in [Door::Api, Door::Mcp] {
            let out = core.ask(&req("chunk"), door).await.unwrap();
            assert!(out.event_id.is_none(), "{door:?} recorded a question");
        }
        assert_eq!(core.store.ask_stats().await.unwrap().asked, 0);
    }

    #[tokio::test]
    async fn an_mcp_or_api_ask_engages_nothing_by_citing_it() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        core.completer = Some(std::sync::Arc::new(crate::infer::fake::FakeCompleter {
            reply: Some("the answer rests on this [1]".into()),
        }));
        seed(&core, 3, 4).await;

        for door in [Door::Api, Door::Mcp] {
            let out = core.ask(&req("chunk"), door).await.unwrap();
            core.background.wait_idle().await;
            let ids: Vec<String> = out
                .citations
                .iter()
                .map(|c| c.artifact_id.clone())
                .collect();
            let act = core.store.activation_of(&ids).await.unwrap();
            let of = |id: &String| act.get(id).map(|(a, _, _)| *a).unwrap_or(0.0);
            assert_eq!(
                of(&ids[0]),
                of(&ids[1]),
                "{door:?} engaged what its answer cited"
            );
        }
    }

    #[tokio::test]
    async fn a_ui_ask_is_not_recorded_when_feedback_is_off() {
        let core = test_core().await;
        seed(&core, 3, 4).await;
        let out = core.ask(&req("chunk"), Door::Ui.by("me")).await.unwrap();
        assert!(out.event_id.is_none());
    }

    #[tokio::test]
    async fn an_ask_with_no_hits_is_recorded_as_an_abstention_without_a_model_call() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        let out = core
            .ask(&req("nothing is stored"), Door::Ui.by("me"))
            .await
            .unwrap();
        assert!(out.abstained);
        assert!(
            crate::infer::prompt::abstained(&out.answer),
            "{}",
            out.answer
        );
        let ev = core
            .store
            .ask_event(out.event_id.as_deref().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert!(ev.abstained && ev.citations.is_empty());
    }

    #[tokio::test]
    async fn an_answer_that_opens_with_the_sentinel_is_flagged_abstained() {
        let mut core = test_core().await;
        core.completer = Some(std::sync::Arc::new(crate::infer::fake::FakeCompleter {
            // Carries a literal no excerpt does, so the abstention branch is what keeps
            // `unsupported` empty.
            reply: Some(
                "Not in the knowledge base. The excerpts cover `chunk 0` only, not `wipefs --all /dev/sdX`."
                    .into(),
            ),
        }));
        seed(&core, 3, 4).await;
        let out = core.ask(&req("chunk"), Door::Api).await.unwrap();
        assert!(out.abstained);
        assert!(
            !out.citations.is_empty(),
            "abstaining does not hide what was shown"
        );
        assert!(
            out.unsupported.is_empty(),
            "an abstention claims nothing, so there is nothing in it to check: {:?}",
            out.unsupported
        );
    }

    #[tokio::test]
    async fn an_answer_that_invents_a_command_reports_it_as_unsupported() {
        let mut core = test_core().await;
        core.completer = Some(std::sync::Arc::new(crate::infer::fake::FakeCompleter {
            reply: Some("Run `wipefs --all /dev/sdX` first, then read chunk 0.".into()),
        }));
        seed(&core, 3, 4).await;
        let out = core.ask(&req("chunk"), Door::Api).await.unwrap();
        assert_eq!(
            out.unsupported,
            vec!["wipefs --all /dev/sdX".to_string()],
            "the excerpts are filler text; the command is the model's own"
        );
    }

    #[tokio::test]
    async fn an_answer_quoting_only_its_excerpts_reports_nothing_unsupported() {
        let mut core = test_core().await;
        core.completer = Some(std::sync::Arc::new(crate::infer::fake::FakeCompleter {
            reply: Some("The excerpt says `chunk 0 filler` and nothing else.".into()),
        }));
        seed(&core, 3, 4).await;
        let out = core.ask(&req("chunk"), Door::Api).await.unwrap();
        assert!(out.unsupported.is_empty(), "{:?}", out.unsupported);
    }

    #[tokio::test]
    async fn the_collected_answer_equals_the_streamed_one() {
        let mut core = test_core().await;
        core.reranker = Some(std::sync::Arc::new(Cliffed));
        seed(&core, 5, 4).await;

        let blocking = core.ask(&req("chunk"), Door::Api).await.unwrap();

        let mut streamed = None;
        let s = core.ask_events(&req("chunk"), Door::Api);
        tokio::pin!(s);
        while let Some(ev) = s.next().await {
            if let AskEvent::Done(d) = ev.unwrap() {
                streamed = Some(*d);
            }
        }
        let streamed = streamed.expect("the stream must terminate with Done");

        assert_eq!(blocking.answer, streamed.answer);
        assert_eq!(blocking.abstained, streamed.abstained);
        assert_eq!(blocking.truncated, streamed.truncated);
        assert_eq!(blocking.dropped, streamed.dropped);
        assert_eq!(blocking.unsupported, streamed.unsupported);
        assert_eq!(
            blocking
                .citations
                .iter()
                .map(|c| (c.artifact_id.as_str(), c.via.as_deref()))
                .collect::<Vec<_>>(),
            streamed
                .citations
                .iter()
                .map(|c| (c.artifact_id.as_str(), c.via.as_deref()))
                .collect::<Vec<_>>(),
            "the same excerpts were shown to the model, in the same order"
        );
    }

    #[tokio::test]
    async fn citations_arrive_before_the_first_token_and_done_is_last() {
        let mut core = test_core().await;
        // Several deltas, so "before the first token" is a claim about ordering.
        core.completer = Some(std::sync::Arc::new(Chatty { parts: 3 }));
        seed(&core, 3, 4).await;

        let mut order: Vec<&'static str> = vec![];
        let s = core.ask_events(&req("chunk"), Door::Api);
        tokio::pin!(s);
        while let Some(ev) = s.next().await {
            order.push(match ev.unwrap() {
                AskEvent::Retrieved { .. } => "retrieved",
                AskEvent::Needs(_) => "needs",
                AskEvent::Citations(_) => "citations",
                AskEvent::Reasoning(_) => "reasoning",
                AskEvent::Token(_) => "token",
                AskEvent::Done(_) => "done",
            });
        }

        let first_token = order
            .iter()
            .position(|e| *e == "token")
            .expect("nothing was streamed, so the ordering claim is vacuous");
        let citations = order
            .iter()
            .position(|e| *e == "citations")
            .expect("citations emitted");
        assert!(
            citations < first_token,
            "citations must precede the first token: {order:?}"
        );
        assert_eq!(
            order.last(),
            Some(&"done"),
            "Done must be terminal: {order:?}"
        );
        assert_eq!(
            order.iter().filter(|e| **e == "done").count(),
            1,
            "one ask, one answer: {order:?}"
        );
    }

    #[tokio::test]
    async fn a_streamed_ask_is_recorded_exactly_once() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        seed(&core, 3, 4).await;

        let before = core.store.ask_stats().await.unwrap().asked;
        let s = core.ask_events(&req("chunk"), Door::Ui.by("me"));
        tokio::pin!(s);
        let mut done = None;
        while let Some(ev) = s.next().await {
            if let AskEvent::Done(d) = ev.unwrap() {
                done = Some(*d);
            }
        }
        let after = core.store.ask_stats().await.unwrap().asked;

        assert_eq!(after, before + 1);
        assert!(
            done.unwrap().event_id.is_some(),
            "the streamed answer must carry the id of the row it recorded"
        );
    }

    #[tokio::test]
    async fn an_answer_longer_than_the_sink_can_hold_still_finishes() {
        let mut core = test_core().await;
        const PARTS: usize = 500;
        core.completer = Some(std::sync::Arc::new(Chatty { parts: PARTS }));
        seed(&core, 3, 4).await;

        let out = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let mut tokens = 0usize;
            let mut done = None;
            let s = core.ask_events(&req("chunk"), Door::Api);
            tokio::pin!(s);
            while let Some(ev) = s.next().await {
                match ev.unwrap() {
                    AskEvent::Token(_) => tokens += 1,
                    AskEvent::Done(d) => done = Some(*d),
                    _ => {}
                }
            }
            (tokens, done.expect("the stream must terminate with Done"))
        })
        .await
        .expect("the deltas were not drained while the call was in flight");

        assert_eq!(out.0, PARTS, "a delta was swallowed on the way to the page");
        assert_eq!(out.1.answer.split_whitespace().count(), PARTS);
    }

    struct Stalling {
        release: std::sync::Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl crate::infer::Completer for Stalling {
        async fn complete(&self, _system: &str, _user: &str) -> Result<String> {
            Ok("a stalled answer".into())
        }
        async fn answer_streaming(
            &self,
            _system: &str,
            _user: &str,
            _ceiling: usize,
            sink: tokio::sync::mpsc::Sender<crate::infer::Delta>,
        ) -> Result<crate::infer::Completion> {
            let _ = sink
                .send(crate::infer::Delta::Token("a stalled ".into()))
                .await;
            self.release.notified().await;
            let _ = sink.send(crate::infer::Delta::Token("answer".into())).await;
            Ok(crate::infer::Completion {
                text: "a stalled answer".into(),
                truncated: false,
            })
        }
        fn context_tokens(&self) -> usize {
            4096
        }
        fn max_output_tokens(&self) -> usize {
            1024
        }
    }

    #[tokio::test]
    async fn a_reader_who_leaves_mid_answer_does_not_hand_the_gpu_to_the_worker() {
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let mut core = test_core().await;
        core.learn.enabled = true;
        core.completer = Some(std::sync::Arc::new(Stalling {
            release: release.clone(),
        }));
        seed(&core, 3, 4).await;

        // Read to the first token, so the call is in flight rather than merely spawned.
        {
            let s = core.ask_events(&req("chunk"), Door::Ui.by("me"));
            tokio::pin!(s);
            loop {
                match s.next().await.expect("the stream ended before it answered") {
                    Ok(AskEvent::Token(_)) => break,
                    Ok(_) => continue,
                    Err(e) => panic!("{e}"),
                }
            }
        }

        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(200),
                core.gate.background()
            )
            .await
            .is_err(),
            "the reader left and the lane went with them, while the model call \
             it was taken for is still running"
        );

        release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), core.gate.background())
            .await
            .expect("the lane was never released after the call ended");

        assert_eq!(core.store.ask_stats().await.unwrap().asked, 0);
    }

    struct Chatty {
        parts: usize,
    }

    impl Chatty {
        fn text(&self) -> String {
            (0..self.parts).map(|i| format!("w{i} ")).collect()
        }
    }

    #[async_trait::async_trait]
    impl crate::infer::Completer for Chatty {
        async fn complete(&self, _system: &str, _user: &str) -> Result<String> {
            Ok(self.text())
        }
        async fn answer_streaming(
            &self,
            _system: &str,
            _user: &str,
            _ceiling: usize,
            sink: tokio::sync::mpsc::Sender<crate::infer::Delta>,
        ) -> Result<crate::infer::Completion> {
            for i in 0..self.parts {
                let _ = sink
                    .send(crate::infer::Delta::Token(format!("w{i} ")))
                    .await;
            }
            Ok(crate::infer::Completion {
                text: self.text(),
                truncated: false,
            })
        }
        fn context_tokens(&self) -> usize {
            4096
        }
        fn max_output_tokens(&self) -> usize {
            1024
        }
    }

    #[tokio::test]
    async fn consecutive_passages_are_stitched_into_one_excerpt_and_every_id_is_cited() {
        let mut core = test_core().await;
        let src = core
            .store
            .insert_corpus("l1\nl2\nl3", "web", None)
            .await
            .unwrap();
        let mk = |i: i64, text: &str, from: i64, to: i64| NewArtifact {
            ordinal: i,
            text: text.into(),
            corpus_span: Some(crate::store::artifacts::CorpusSpan {
                start_line: from,
                end_line: to,
                source: crate::store::artifacts::SpanSource::Located,
            }),
            title: Some("Recovery".into()),
            segment_idx: Some(0),
            ..Default::default()
        };
        let made = core
            .store
            .insert_artifacts_with_provenance(
                &src.id,
                &[
                    mk(0, "first part", 1, 1),
                    mk(1, "second part", 2, 2),
                    mk(2, "third part", 3, 3),
                ],
                crate::store::artifacts::Provenance::Passage,
            )
            .await
            .unwrap();
        for c in &made {
            crate::jobs::embed::run(&core, &c.id).await.unwrap();
        }
        let probe = std::sync::Arc::new(crate::infer::fake::ScriptedCompleter::new(vec![
            "See [1].".into(),
        ]));
        core.completer = Some(probe.clone());

        let out = core
            .ask(
                &AskRequest {
                    q: "Recovery\nfirst part".into(),
                    limit: Some(8),
                    tags: vec![],
                    category: None,
                },
                Door::Api,
            )
            .await
            .unwrap();

        let prompt = probe.prompts().pop().expect("the model was called");
        assert!(
            prompt.contains("first part\nsecond part\nthird part"),
            "{prompt}"
        );
        assert_eq!(
            prompt.matches("Recovery").count(),
            2,
            "heading once in the excerpt, once in the question: {prompt}"
        );
        let cited: std::collections::HashSet<String> = out
            .citations
            .iter()
            .map(|c| c.artifact_id.clone())
            .collect();
        for c in &made {
            assert!(cited.contains(&c.id), "{} missing from citations", c.text);
        }
    }

    #[tokio::test]
    async fn a_stitched_run_is_numbered_and_headed_by_where_it_starts() {
        // Called directly: retrieval does not promise a run whose best hit is not its first line.
        let core = test_core().await;
        let src = core
            .store
            .insert_corpus("l1\nl2", "web", None)
            .await
            .unwrap();
        let mk = |i: i64, text: &str, title: &str, line: i64| NewArtifact {
            ordinal: i,
            text: text.into(),
            corpus_span: Some(crate::store::artifacts::CorpusSpan {
                start_line: line,
                end_line: line,
                source: crate::store::artifacts::SpanSource::Located,
            }),
            title: Some(title.into()),
            segment_idx: Some(0),
            ..Default::default()
        };
        let made = core
            .store
            .insert_artifacts_with_provenance(
                &src.id,
                &[
                    mk(0, "the tail of the section above", "Recovery", 1),
                    mk(1, "rotate the offsite copy weekly", "Backups", 2),
                ],
                crate::store::artifacts::Provenance::Passage,
            )
            .await
            .unwrap();

        let hit =
            |c: &crate::store::artifacts::Chunk, score: f32| crate::core::search::SearchResult {
                artifact_id: c.id.clone(),
                corpus_id: c.corpus_id.clone().unwrap_or_default(),
                title: c.title.clone(),
                text: c.text.clone(),
                score,
                ..Default::default()
            };
        let hits = vec![hit(&made[1], 0.9), hit(&made[0], 0.5)];
        let mut blocks = vec![
            crate::infer::prompt::ask_excerpt(1, "Backups", None, &made[1].text, &[]),
            crate::infer::prompt::ask_excerpt(2, "Recovery", None, &made[0].text, &[]),
        ];
        core.stitch_passages(&hits, &mut blocks).await;

        assert!(
            blocks[1].contains("the tail of the section above\nrotate the offsite copy weekly"),
            "stitched in document order: {}",
            blocks[1]
        );
        assert!(
            blocks[1].starts_with("[2] Recovery\n"),
            "the block is numbered and headed by where the text begins: {}",
            blocks[1]
        );
        assert_eq!(
            blocks[0], "[1] (continues [2])",
            "every constituent keeps a citable number: {}",
            blocks[0]
        );
    }

    async fn disagree(core: &Core, a: &str, b: &str, detail: &str) {
        use crate::store::pairs::{DecidedBy, PairState};
        core.store.record_pair(a, b, 0.9).await.unwrap();
        let id = core.store.pair_between(a, b).await.unwrap().unwrap().id;
        core.store
            .set_pair_state(id, PairState::Contradiction, Some(detail), DecidedBy::Model)
            .await
            .unwrap();
    }

    async fn unreachable_note(core: &Core, doc: &str, title: &str, text: &str) -> String {
        let src = core.store.insert_corpus(doc, "web", None).await.unwrap();
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    text: text.into(),
                    title: Some(title.into()),
                    category: Some("reference".into()),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();
        made[0].id.clone()
    }

    #[tokio::test]
    async fn a_disagreeing_note_is_brought_in_and_marked() {
        let mut core = test_core().await;
        let src = core
            .store
            .insert_corpus("the backup policy as first written", "web", None)
            .await
            .unwrap();
        let a = core
            .store
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    text: "backup retention is 30 days".into(),
                    title: Some("Retention policy".into()),
                    category: Some("reference".into()),
                    ..Default::default()
                }],
            )
            .await
            .unwrap()[0]
            .id
            .clone();
        crate::jobs::embed::run(&core, &a).await.unwrap();
        let b = unreachable_note(
            &core,
            "the backup policy as later revised",
            "Retention, revised",
            "backup retention is 14 days",
        )
        .await;
        let probe = std::sync::Arc::new(crate::infer::fake::ScriptedCompleter::new(vec![
            "See [1].".into(),
            "See [1] and [2].".into(),
        ]));
        core.completer = Some(probe.clone());
        let q = req("how long is backup retention");
        let ask = || core.ask(&q, Door::Api);

        let before = ask().await.unwrap();
        assert!(
            before.citations.iter().all(|c| c.artifact_id != b),
            "the far note was reachable without the disagreement"
        );
        assert!(before.disagreements.is_empty());

        disagree(&core, &a, &b, "30 days there, 14 here").await;
        let out = ask().await.unwrap();

        let partner = out
            .citations
            .iter()
            .find(|c| c.artifact_id == b)
            .expect("the other side of the disagreement never reached the prompt");
        assert_eq!(partner.via.as_deref(), Some(a.as_str()));
        assert_eq!(partner.reason.as_deref(), Some("states this differently"));

        let prompt = probe.prompts().pop().expect("the model was called");
        assert!(
            prompt.contains("Caveat: excerpt [2] states this differently: 30 days there, 14 here"),
            "{prompt}"
        );
        assert!(
            prompt.contains("Caveat: excerpt [1] states this differently: 30 days there, 14 here"),
            "{prompt}"
        );
        assert!(!prompt.contains("not among these excerpts"), "{prompt}");
        for (n, id, title) in [(1, &a, "Retention policy"), (2, &b, "Retention, revised")] {
            let day = crate::fmt::fmt_day(core.store.get_artifact(id).await.unwrap().created_at);
            assert!(
                prompt.contains(&format!("[{n}] {title} ({day})")),
                "{prompt}"
            );
        }

        assert_eq!(out.disagreements.len(), 1, "{:?}", out.disagreements);
        assert_eq!(
            out.disagreements[0].artifact_id, a,
            "read from the side cited first"
        );
        assert_eq!(out.disagreements[0].other_id, b);
    }

    fn disagreeing_hits() -> Vec<SearchResult> {
        let side =
            |id: &str, other: &str, title: &str, other_title: &str, text: &str| SearchResult {
                artifact_id: id.into(),
                title: Some(title.into()),
                text: text.into(),
                disagrees_with: vec![crate::store::pairs::Disagreement {
                    artifact_id: id.into(),
                    created_at: 1_767_225_600,
                    other_id: other.into(),
                    other_title: Some(other_title.into()),
                    other_created_at: 1_772_366_400,
                    detail: Some("30 days there, 14 here".into()),
                }],
                ..Default::default()
            };
        vec![
            side(
                "a",
                "b",
                "Retention policy",
                "Retention, revised",
                "retention is 30 days",
            ),
            side(
                "b",
                "a",
                "Retention, revised",
                "Retention policy",
                &"retention is 14 days, as revised. ".repeat(40),
            ),
        ]
    }

    #[tokio::test]
    async fn a_partner_the_packing_cut_is_not_named_by_a_number() {
        let core = test_core().await;
        let hits = disagreeing_hits();

        let (blocks, kept) = core.packed_excerpts(&hits, usize::MAX).await;
        assert_eq!(kept, 2);
        assert!(
            blocks[0].contains("excerpt [2] states this differently"),
            "{}",
            blocks[0]
        );

        let first = core.excerpts(&hits[..1], 0).await;
        let budget = core.counter.count(&first[0]);
        let (blocks, kept) = core.packed_excerpts(&hits, budget).await;
        assert_eq!(kept, 1);
        assert!(!blocks[0].contains("excerpt [2]"), "{}", blocks[0]);
        assert!(
            blocks[0].contains(&format!(
                "another note, \"Retention, revised\" ({}), not among these excerpts, \
                 states this differently: 30 days there, 14 here",
                crate::fmt::fmt_day(1_772_366_400)
            )),
            "{}",
            blocks[0]
        );
        assert!(
            blocks[0].starts_with(&format!(
                "[1] Retention policy ({})",
                crate::fmt::fmt_day(1_767_225_600)
            )),
            "{}",
            blocks[0]
        );
    }

    struct Flat;

    #[async_trait::async_trait]
    impl crate::infer::Reranker for Flat {
        async fn rerank(
            &self,
            _query: &str,
            docs: &[String],
            top_n: usize,
        ) -> Result<Vec<(usize, f32)>> {
            Ok((0..docs.len().min(top_n))
                .map(|i| (i, 0.9 - i as f32 * 0.001))
                .collect())
        }
    }

    #[tokio::test]
    async fn partners_never_displace_a_ranked_hit() {
        let mut core = test_core().await;
        core.reranker = Some(std::sync::Arc::new(Flat));
        seed(&core, 5, 4).await;
        let three = AskRequest {
            q: "chunk".into(),
            limit: Some(3),
            tags: vec![],
            category: None,
        };
        let ids = |out: &AskResponse| -> Vec<String> {
            out.citations
                .iter()
                .map(|c| c.artifact_id.clone())
                .collect()
        };

        let before = core.ask(&three, Door::Api).await.unwrap();
        assert_eq!(ranked(&before), 3, "{:?}", ids(&before));
        let hits: Vec<String> = ids(&before)[..3].to_vec();

        let mut partners = Vec::new();
        for (i, h) in hits.iter().enumerate() {
            let far = unreachable_note(
                &core,
                &format!("a far document number {i}"),
                &format!("far {i}"),
                &format!("far note {i} says otherwise"),
            )
            .await;
            disagree(&core, h, &far, "differs").await;
            partners.push(far);
        }

        let out = core.ask(&three, Door::Api).await.unwrap();
        let got = ids(&out);
        assert_eq!(ranked(&out), 3, "{got:?}");
        assert_eq!(
            got[..3],
            hits[..],
            "the ranked hits keep their places and order"
        );
        assert_eq!(
            got[3..6],
            partners[..],
            "partners take the reach's first places, in anchor order"
        );
        for (c, h) in out.citations[3..6].iter().zip(&hits) {
            assert_eq!(c.via.as_deref(), Some(h.as_str()));
        }
        assert!(
            got.len() <= 3 + retrieve::NEIGHBOUR_MAX,
            "{} citations is past the reach's cap",
            got.len()
        );
        assert_eq!(out.disagreements.len(), 3, "{:?}", out.disagreements);
    }
}
