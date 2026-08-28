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

/// What one retrieval round produced.
struct Round {
    /// The query this round ran. Round one carries the question as it was
    /// asked; a planned round carries the subject the model named.
    ///
    /// Held on the round rather than zipped alongside it, because `fan_out`
    /// drops a round that failed — a list of queries and a list of rounds stop
    /// lining up the moment one endpoint times out, and the thing that would
    /// then be mislabelled is a claim about what the base does not hold.
    query: String,
    /// Above the cliff, with whatever was reached sideways appended.
    hits: Vec<SearchResult>,
    /// How many of `hits` are ranked. Everything after them was reached.
    ranked: usize,
    /// Every artifact the ranking returned, cliff and all. What `dropped` is
    /// measured against, so a citation lost to the cliff is as visible as one
    /// lost to the window.
    retrieved: Vec<String>,
    cliff_at: Option<usize>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AskResponse {
    pub answer: String,
    /// Exactly the excerpts the model saw.
    pub citations: Vec<SearchResult>,
    /// Retrieved but left out — below the relevance cliff, or past what the
    /// context window holds. Reported so a missing citation is visible rather
    /// than silent.
    pub dropped: usize,
    /// The answer stops where its output ceiling did, not where the model meant
    /// to. Reported for the same reason `dropped` is: an answer cut off
    /// mid-sentence is otherwise indistinguishable from a complete one, and the
    /// fix — a higher `ask.max_output_tokens` — belongs to whoever is reading
    /// it.
    pub truncated: bool,
    /// The answer opened with `prompt::ABSTAIN_PREFIX`, or there was nothing
    /// to show the model. What the harness counts as "said nothing here".
    pub abstained: bool,
    /// Literals the answer carries that no excerpt it was shown does — the
    /// fidelity thesis extended from synthesis to generation. `ask` is the one
    /// place a model writes something a person reads as fact, and a command or
    /// number here that is in no excerpt is the model's own rather than
    /// something the base holds. Reported so the page can say which.
    pub unsupported: Vec<String>,
    /// The recorded question, when this door records — the UI, with feedback
    /// on. The page shows a verdict bar only when this is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
}

impl Core {
    /// Ask, and wait for the whole answer.
    ///
    /// A collector over `ask_events`, not a second implementation: `/api/v1/ask`
    /// and the MCP tool cannot stream and are not asked to, and there must be
    /// exactly one account of what asking means.
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

    /// One ask, as it happens: what was retrieved, what the model will be
    /// shown, and the answer as it is written.
    ///
    /// This is the implementation, and `ask` is a collector over it, so a
    /// change to what asking means reaches both doors or neither.
    ///
    /// `'static` on purpose: an SSE response outlives the handler that built
    /// it, so the stream owns a clone of `Core` and its own copy of the
    /// request rather than borrowing either.
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
            // No ask model: the door is not offered anywhere, and a caller that
            // found the route anyway is told why rather than served an answer
            // from nothing.
            if core.completer.is_none() {
                Err(Error::Validation("[infer.ask] is not configured".into()))?;
            }
            let completer = core
                .completer
                .clone()
                .expect("checked just above");

            // Held for the whole answer rather than around the completion, because
            // a search embeds the query and that is a model call too. A gap between
            // them is a gap the worker would fill with a window, and a window is
            // twenty to seventy seconds of somebody waiting.
            //
            // Taking the lane does not make an in-flight call stop; nothing here
            // cancels. It keeps the worker from putting anything new in front of
            // this one.
            //
            // Shared with the completion's task rather than held here alone,
            // because the two do not end together. A reader that closes the tab
            // drops this stream while the model call keeps running on the GPU,
            // and a lease that ended with the stream would hand the worker a
            // window against hardware an interactive call still occupies — the
            // exact interleaving this exists to prevent, inverted. Whichever
            // side finishes last drops the last handle.
            let lane = std::sync::Arc::new(core.gate.interactive());

            // Asking a question is as deliberate as a search gets.
            let first = core.retrieve_round(&req, &req.q, true).await?;
            let mut hits = first.hits;
            let mut retrieved = first.retrieved;

            if hits.is_empty() {
                // No retrieval, no completion: spending a model call to say
                // "nothing found" is pure latency. Opens with the sentinel so the
                // page and the harness read it as the abstention it is.
                let response = AskResponse {
                    answer: format!("{ABSTAIN_PREFIX}. Nothing matches that question."),
                    citations: vec![],
                    dropped: 0,
                    truncated: false,
                    abstained: true,
                    // Nothing was shown and nothing was claimed.
                    unsupported: vec![],
                    event_id: None,
                };
                // Emitted even though it is empty: a reader that waits for the
                // rail before the answer must not wait forever on the one
                // question that retrieved nothing.
                yield AskEvent::Retrieved { round: 1, retrieved: 0, shown: 0, dropped: 0, cliff_at: None };
                yield AskEvent::Citations(vec![]);
                // Nothing was planned — this returns before the plan runs — so
                // there are no uncovered subjects to record.
                let response = core.record_ask(&req, &origin, response, &[]).await?;
                yield AskEvent::Done(Box::new(response));
                return;
            }

            let mut blocks = core.excerpts(&hits).await;

            let budget = core.excerpt_budget(&req.q);

            // Highest score first, so what gets cut is what mattered least.
            let mut kept = pack_by_budget(&blocks, &core.counter, budget);
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
                // Nothing was planned — this returns before the plan runs — so
                // there are no uncovered subjects to record.
                let response = core.record_ask(&req, &origin, response, &[]).await?;
                yield AskEvent::Done(Box::new(response));
                return;
            }

            // Round one is over as far as anyone watching is concerned: what it
            // retrieved is reported before the planner is asked anything, so a
            // reader sees the first round land rather than a pause of unknown
            // cause.
            yield AskEvent::Retrieved {
                round: 1,
                retrieved: retrieved.len(),
                shown: kept,
                dropped,
                cliff_at: first.cliff_at,
            };

            // One plan, however many rounds it names, and structurally so: one
            // call site, no loop, and nothing downstream of here plans again. A
            // second plan has nowhere to go — which is the point. "Let the model
            // say once what is missing" is the bounded version of a mechanism
            // whose unbounded version is an agent, and an agent is not what this
            // is.
            //
            // `needed_queries` returns empty the moment no planning model is
            // wired, so with the feature off this costs one `Option` check and
            // no call at all.
            // The subjects the plan named that nothing came back for. Empty
            // with planning off, and empty when the plan named nothing.
            let mut uncovered: Vec<String> = Vec::new();
            let queries = plan::needed_queries(&core, &req.q, &blocks[..kept]).await;
            if !queries.is_empty() {
                yield AskEvent::Needs(queries.clone());

                // Every planned query at once. They are independent searches
                // against the same store, so running them in sequence would
                // charge the reader one embedding round trip per subject for no
                // reason. `PLAN_MAX_QUERIES` caps the plan and so caps this.
                let extra = core.fan_out(&req, &queries).await;
                // Read before the rounds are merged away. A subject the base
                // held nothing near is a hole it just named itself, and after
                // the merge there is no round left to ask.
                uncovered = plan::uncovered(&extra);

                // Round one's hits are the first part, so round-robin starts
                // with the question as it was actually asked.
                let mut parts = vec![retrieve::Part::of(hits.clone(), ranked, first.cliff_at)];
                let mut fanned_retrieved: Vec<String> = Vec::new();
                for round in extra {
                    fanned_retrieved.extend(round.retrieved);
                    parts.push(retrieve::Part::of(round.hits, round.ranked, round.cliff_at));
                }

                // Nothing came back at all: every planned round failed. Round
                // one still stands, and the page is told how the fan-out ended
                // rather than left with "looking further…" on screen for the
                // rest of the answer.
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

                    // Packed again over the whole merged list rather than
                    // appended to what round one packed: the fanned-out
                    // excerpts have to fit the same window as the first, and
                    // the only honest way to know what fits is to pack it.
                    let mut merged_blocks = core.excerpts(&merged.hits).await;
                    let merged_kept = pack_by_budget(&merged_blocks, &core.counter, budget);
                    core.stitch_passages(
                        &merged.hits[..merged_kept],
                        &mut merged_blocks[..merged_kept],
                    )
                    .await;

                    // A re-pack that fits nothing leaves round one standing.
                    // The prompt cannot be empty because the fan-out found
                    // nothing extra to say.
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

                    // Reported either way, and with round one's numbers when
                    // the re-pack changed nothing. `Needs` has already told the
                    // page a wider search is happening; ending without a
                    // matching `Retrieved` would leave "looking further…" on
                    // screen describing something that finished. A fan-out that
                    // changed nothing is an outcome, not an absence.
                    //
                    // `shown` here can be *lower* than round one's, and that is
                    // the priority working rather than a regression: a hit a
                    // planned round asked for packs ahead of round one's
                    // neighbours, so a window that held three speculative
                    // excerpts may hold one hit instead. Reporting the smaller
                    // number is honest — those neighbours are no longer in the
                    // prompt.
                    //
                    // No cliff. Each round cut at its own before it got here and
                    // the merged list is several rankings interleaved, so there
                    // is no one position in it where relevance fell off; naming
                    // one would be inventing it.
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

            // The ceiling comes from what the prompt actually cost, not from what
            // was reserved for it: packing is an estimate made before the excerpts
            // were chosen, and whatever the window has left over after them is room
            // the answer may have. Bounded by the configured ceiling, so this only
            // ever gives back what the reserve above took away — and it keeps the
            // one invariant the endpoint enforces, that prompt plus ceiling fits the
            // window, with the margin `ceiling_for_prompt` leaves for the estimate
            // being an estimate.
            let spent = core.counter.count(ASK_SYSTEM) + core.counter.count(&user);
            let ceiling = crate::infer::budget::ceiling_for_prompt(
                completer.context_tokens(),
                spent,
                completer.max_output_tokens(),
            );

            // The excerpts go out before the first token, so the rail beside the
            // answer is readable while the answer is still being written. Once,
            // after the last retrieval: the rail names what the model was shown,
            // and there is one such list however many rounds found it.
            let citations: Vec<SearchResult> = hits.into_iter().take(kept).collect();
            yield AskEvent::Citations(citations.clone());

            // The sink is bounded, so the call has to run *beside* this loop and
            // not before it. A producer that awaited the call and drained
            // afterwards would block the endpoint's read loop on the first answer
            // longer than the channel — which is every answer worth reading — and
            // would still pass any test whose fake reply fits in 64 deltas.
            let (tx, mut rx) = tokio::sync::mpsc::channel::<crate::infer::Delta>(64);
            let prompt = user.clone();
            let held = std::sync::Arc::clone(&lane);
            let call = tokio::spawn(async move {
                // Dropped when this task ends, whether or not anyone is still
                // reading: the GPU is busy until the call returns, and the lane
                // says so for exactly that long.
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
            // The completion, and not the deltas this loop accumulated, is what
            // gets checked and recorded: a reader that stopped reading must not
            // be able to truncate what the base stores.
            let answer = call
                .await
                .map_err(|e| Error::Internal(format!("the answer never finished: {e}")))??;
            if answer.truncated {
                tracing::warn!(
                    ceiling,
                    "ask: the answer hit its output ceiling and is cut off"
                );
            }

            // Checked against exactly the blocks that went into the prompt, so what
            // the page flags is measured against what the model actually saw — not
            // against the excerpts packing dropped, nor the raw artifact text the
            // model was never shown. An abstention claims nothing, so there is
            // nothing in it to check.
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

            let response = AskResponse {
                abstained,
                answer: answer.text,
                citations,
                dropped,
                truncated: answer.truncated,
                unsupported,
                event_id: None,
            };
            // Recorded here rather than by either door, so one ask is one row
            // however it was asked. The harness reads these.
            let response = core.record_ask(&req, &origin, response, &uncovered).await?;
            yield AskEvent::Done(Box::new(response));
        }
    }
}

/// The planned rounds, held so that dropping them stops them.
///
/// A `JoinHandle` dropped on the floor does not cancel its task, and these
/// tasks are searches: an embedding call and a vector query each. A reader who
/// closes the tab drops the stream, and everything the ask was doing inline
/// stops at its next await — but three detached rounds would run to completion
/// against the same hardware, for an answer nobody will read. Round two used to
/// run inline in the generator and was cancelled that way; spawning is what
/// took the property away, so spawning is what has to give it back.
///
/// Not the answer call, deliberately. That one keeps running on purpose — it
/// holds the interactive lane, and killing it mid-generation would hand the
/// worker a window against a GPU still finishing the work. A retrieval that
/// nobody is waiting for has no such claim.
struct Fanned(Vec<tokio::task::JoinHandle<Result<Round>>>);

impl Drop for Fanned {
    fn drop(&mut self) {
        for t in &self.0 {
            t.abort();
        }
    }
}

impl Core {
    /// Every planned query at once, in plan order.
    ///
    /// Concurrent because the rounds are independent searches against the same
    /// store: run in sequence, a three-subject question would charge the reader
    /// three embedding round trips end to end for work that shares nothing.
    ///
    /// The width is bounded by `PLAN_MAX_QUERIES`, which is what the plan itself
    /// is capped at — one number, so the fan-out can never run wider than the
    /// plan can name.
    ///
    /// Awaited in the order they were spawned rather than as they finish,
    /// because that order is the plan's, and the plan's order is what the merge
    /// reads as priority. Completion order would make the packing priority a
    /// race between endpoints.
    ///
    /// A round that fails is dropped with a warning and the rest stand. Failing
    /// the ask because one of several extra searches failed would trade an
    /// answer for a strictly better answer's absence — the operator asked a
    /// question, not for a retrieval strategy.
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

    /// One retrieval round: search, cut at the cliff, reach sideways.
    ///
    /// Every round comes through here, so a planned one is retrieved on exactly
    /// the terms the verbatim question was. A second copy of this path would be
    /// a second definition of what an excerpt is allowed to be, and the round
    /// that used the stale copy would be the one nobody was watching.
    ///
    /// `deliberate` is false for every planned round: marking a search is a
    /// claim that a person meant it, and a planned query is one the model
    /// wrote. Letting it mark would have the base's activation shaped by the
    /// model's own guesses about what it needs.
    async fn retrieve_round(&self, req: &AskRequest, q: &str, deliberate: bool) -> Result<Round> {
        // No per-source cap: an answer often lives in one document, and
        // withholding its paragraphs to keep the citation list varied would
        // make the answer worse, not fairer.
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
                    // Whether ask reranks is the scope's decision, not this
                    // call's: nobody is typing here.
                    rerank: true,
                    explain: false,
                },
                None,
                // Deliberately not captured: the right answer to a question is
                // a synthesis across several artifacts, so "which one was it"
                // has no well-defined meaning for someone judging it later.
                Door::Ask,
            )
            .await?;

        // Everything the ranking actually returned, kept before the cliff takes
        // its share: `dropped` answers "what did I ask for and not get shown",
        // and an excerpt cut here is as absent from the answer as one cut by the
        // window.
        let retrieved: Vec<String> = hits.iter().map(|h| h.artifact_id.clone()).collect();

        // Cut the ranked list where its relevance falls off, before a single row
        // is read for it: an excerpt below the cliff makes the answer worse as
        // well as dearer, and its caveats are a lookup spent on something that
        // will not be sent.
        //
        // Read off the marks the search made rather than recomputed here over
        // `score`: which scale the cliff may be taken on — reranker scores,
        // or cosine similarity, never the fused rank — is the search's
        // knowledge, and a second computation over the wrong number is how
        // this answered from one excerpt when three were relevant.
        let cliff_at = hits.iter().position(|h| h.past_cliff);
        hits.truncate(retrieve::above_cliff(cliff_at, hits.len()));
        let ranked = hits.len();

        // Artifacts reached sideways rather than retrieved are appended here,
        // after the cliff has been taken — they carry no score comparable to a
        // ranked hit, and must never enter the scores it is computed from.
        // `scores` is consumed above and never recomputed, so nothing appended
        // from here on can reach `cliff` at all.
        self.reach_sideways(&mut hits, cliff_at).await;
        // The neighbours just appended came off the store, not the ranking,
        // and so never passed the titling the ranked hits got.
        self.fill_titles(&mut hits).await;

        Ok(Round {
            query: q.to_string(),
            hits,
            ranked,
            retrieved,
            cliff_at,
        })
    }

    /// The numbered excerpts a set of hits becomes, caveats and all.
    ///
    /// Caveats are the conditions under which an excerpt does not apply, and an
    /// answer that quotes "run `mkfs` on the device" without "destroys
    /// everything already on it" is worse than no answer. They are not in the
    /// vector payload — what gets embedded is a separate decision — so they are
    /// read from the store, which costs one cheap SQLite lookup per hit and no
    /// inference. An excerpt whose row has since been deleted simply carries
    /// none.
    ///
    /// One query for the lot rather than one per hit: this runs once per round,
    /// over the whole list — round two's call covers round one's hits again —
    /// so per-hit round trips would sit in front of every model call twice.
    async fn excerpts(&self, hits: &[SearchResult]) -> Vec<String> {
        let ids: Vec<String> = hits.iter().map(|h| h.artifact_id.clone()).collect();
        let caveats = match self.store.caveats_for(&ids).await {
            Ok(m) => m,
            Err(e) => {
                // The caveats are the safety margin on an excerpt, not the
                // excerpt; an answer without them beats no answer, but the
                // store failing here is worth a line.
                tracing::warn!(error = %e, "ask: could not read caveats; excerpts carry none");
                Default::default()
            }
        };
        hits.iter()
            .enumerate()
            .map(|(i, h)| {
                ask_excerpt(
                    i + 1,
                    h.title.as_deref().unwrap_or_default(),
                    &h.text,
                    caveats
                        .get(&h.artifact_id)
                        .map(Vec::as_slice)
                        .unwrap_or_default(),
                )
            })
            .collect()
    }

    /// Put consecutive passages back together as the text they are.
    ///
    /// Passages are verbatim slices whose spans tile a segment; two that abut
    /// are literally continuous text, and showing them as two excerpts repeats
    /// the carried heading, hides the continuity from the model, and cuts the
    /// sentence running across the boundary. Here — after packing, over the
    /// kept prefix only — runs of abutting passages from one `(corpus,
    /// segment)` are merged into the block of the run's *first* member, in
    /// document order, and every other member keeps its own numbered block
    /// carrying a pointer to it.
    ///
    /// The pointer, rather than an emptied block, is what keeps a citation
    /// honest. The rail beside the answer links `[n]` to the artifact at
    /// position `n` of `hits`, so a run rendered under one number offers the
    /// model exactly one number for text drawn from three artifacts, and a
    /// claim taken from the second passage of a run is then linked to a page
    /// that does not contain it. With a block each, whichever number the model
    /// cites resolves to a passage of the run it actually read.
    ///
    /// The text goes to the run's first member and not to its best-ranked one
    /// for the same reason: the block's heading is the heading the text opens
    /// under, the rail row for that number is the artifact where the text
    /// begins, and the reader who follows the link lands where they were
    /// reading. Numbering the block after the best-ranked member instead put a
    /// heading from one artifact over a rail row naming another.
    ///
    /// A stitched excerpt is a presentation, not a new unit: `hits` is
    /// untouched, every constituent stays a citation, and the literal check
    /// reads the same text the model did. Passages only — two adjacent
    /// *artifacts* are two rewrites, not continuous text.
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
        // (corpus, segment, span, position in hits), for passages with a span.
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
            // `run` is in document order by construction — that is what it
            // was built by — so its first member is where the text begins, and
            // the heading the reader is under is that member's.
            let head_pos = members[run[0]].3;
            let head = hits[head_pos].title.as_deref().unwrap_or_default();
            let text: Vec<&str> = run
                .iter()
                .map(|&m| hits[members[m].3].text.as_str())
                .collect();
            // The caveats of everything the block now contains, not of the
            // anchor alone. A stitched excerpt is one presentation of several
            // passages, and the run's other members are no longer rendered
            // anywhere — so a warning carried by the third passage in a run
            // would leave the prompt entirely if only the anchor's came along.
            // The caveats are the safety margin on an excerpt: dropping them
            // silently is the one direction this must not fail in.
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
            blocks[head_pos] =
                crate::infer::prompt::ask_excerpt(head_pos + 1, head, &text.join("\n"), &caveats);
            // Every other member keeps its slot and points at the block its
            // text went into. Cheap — one line each — and it is what lets the
            // model cite the passage it drew on rather than the one that
            // happened to rank highest.
            for &m in &run {
                let p = members[m].3;
                if p != head_pos {
                    blocks[p] = crate::infer::prompt::ask_continues(p + 1, head_pos + 1);
                }
            }
        }
    }

    /// How many tokens of excerpt one answer may be built from: the rule
    /// `excerpt_budget` states, for the answer model under the ask prompt.
    fn excerpt_budget(&self, question: &str) -> usize {
        // Only reached from `ask_events`, which returned before here without a
        // completer; a caller that gets past that has one.
        let completer = self
            .completer
            .as_deref()
            .expect("ask_events refuses before budgeting without [infer.ask]");
        excerpt_budget(completer, &self.counter, ASK_SYSTEM, question)
    }

    /// One hop sideways from the hits that placed best: the artifacts adjacent
    /// in their corpus, and their one-hop associations.
    ///
    /// The answer is often in the artifact *next to* the one that matched — the
    /// paragraph that names the caveat, the step after the step. Retrieval
    /// cannot find those, because they do not contain the question's terms and
    /// do not sit near it in the embedding space; they are only reachable
    /// through structure, which is why this is a lookup rather than a second
    /// search and costs no inference at all.
    ///
    /// Everything here is best-effort. A neighbour is a bonus, and no failure
    /// to read one may cost the answer that was already retrievable.
    async fn reach_sideways(&self, hits: &mut Vec<SearchResult>, cliff_at: Option<usize>) {
        let anchors = retrieve::anchor_count(cliff_at, hits.len());
        if anchors == 0 {
            return;
        }

        let mut reached: Vec<(String, String, Option<String>)> = Vec::new();
        for h in hits.iter().take(anchors) {
            // Read for the ordinal, which the vector payload does not carry.
            // The same row is read again below for its caveats; both are cheap
            // SQLite lookups against the primary key.
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

            // Only when the associative layer is actually live: links are
            // learned from recorded searches, and an install that never opted
            // into `feedback` must not have that layer read on its behalf.
            // Same half-life and same floor as the results rail, so the reach
            // cannot surface a link the rail would have called too faint.
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

        // Hydration comes before the cap is spent, not after. A candidate whose
        // row has since been superseded or deleted cannot be shown, and giving
        // it one of the six places anyway would shrink the reach below its cap
        // while live candidates queued behind it were never looked at. So a
        // place is only ever spent on an artifact that can actually be cited.
        //
        // Bounded work: at most three anchors contribute two adjacent artifacts
        // and `NEIGHBOUR_MAX` links each, and the loop stops as soon as it has
        // the cap, so this is a handful of primary-key lookups in the worst
        // case and none at all when nothing was reached.
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

        // Still routed through `append_neighbours`, which is the one place the
        // ordering property is stated and tested. Its dedup and cap are belt
        // and braces here — the loop above already satisfied both — and that is
        // the right way round: the helper stays authoritative about where a
        // neighbour may appear, and the caller only ever hands it candidates it
        // could genuinely show.
        let ids: Vec<String> = live.iter().map(|(c, _, _)| c.id.clone()).collect();
        let merged = retrieve::append_neighbours(ranked, ids, retrieve::NEIGHBOUR_MAX);

        for id in merged.into_iter().skip(ranked_len) {
            let Some(i) = live.iter().position(|(c, _, _)| c.id == id) else {
                continue;
            };
            let (c, via, reason) = live.swap_remove(i);
            hits.push(SearchResult {
                artifact_id: c.id,
                corpus_id: c.corpus_id.unwrap_or_default(),
                title: c.title,
                text: c.text,
                category: c.category,
                tags: c.tags,
                // Not a rank and not a similarity: this artifact did not
                // compete for a place in the list, it was reached beside one.
                // Any other number would be a claim about relevance that
                // nothing measured, and `record_ask` stores it.
                score: 0.0,
                status: Some(c.status),
                superseded_by: c.superseded_by,
                last_verified_at: c.last_verified_at,
                model_written: c.provenance.is_model_written(),
                synthesized: c.provenance == crate::store::artifacts::Provenance::Synthesized,
                origin_count: 0,
                // Weakness is read from a similarity to the query, and there is
                // no similarity here to read. It has to be demonstrated, never
                // assumed — in either direction.
                weak: false,
                primed: false,
                in_sitting: false,
                // The cliff was computed over scores this one was never in.
                past_cliff: false,
                similarity: None,
                titled_by_corpus: false,
                // What makes a reached artifact tellable apart from a retrieved
                // one, by a reader and by a test alike: a ranked hit has no
                // `via`, and this one names the hit it was reached from.
                // Reached beside a ranked hit, never ranked itself.
                explanation: Some(crate::core::explain::HitExplanation::recalled(&via)),
                via: Some(via),
                reason,
            });
        }
    }

    /// Record the question when this door records. Only the UI, and only with
    /// feedback on: a question is personal data of the same kind as a query,
    /// and API and MCP callers asked for the smallest footprint. Recorded
    /// synchronously — the id goes back to the page — and after the answer,
    /// which has already taken seconds; one insert costs nothing beside it.
    /// A failure to record must not cost the answer: it is logged and the
    /// response goes out without an id.
    async fn record_ask(
        &self,
        req: &AskRequest,
        origin: &Origin,
        mut response: AskResponse,
        uncovered: &[String],
    ) -> Result<AskResponse> {
        // `Ui` and `Cli`. Both are a person composing a question in a place
        // where they will read the answer; neither is an agent on a token. A
        // shell question was recorded nowhere at all until now, which made the
        // strongest statement of a need engram can receive the one thing it
        // never wrote down.
        //
        // A CLI ask carries no `event_id` back to the judging view, and that
        // stays true: recorded and unjudged is a coherent state. The sweep
        // learns what was needed; the judge still grades only answers somebody
        // saw somewhere they could grade them.
        if !(self.learn.enabled && matches!(origin.door, Door::Ui | Door::Cli)) {
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
            // What the answer referenced, not what it was shown. The sweep
            // reads these as engagement, and every excerpt that fit the window
            // would otherwise count as one — enough, on its own, to arm a
            // generation off a question the model declined to answer.
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
        // The same distinction, read the other way: what the answer used was
        // engaged, and that is the one signal a question honestly gives about
        // an artifact. Off the recording path's success — an insert that fails
        // does not unmake the use.
        self.mark_artifacts_cited(
            ask.citations
                .iter()
                .filter(|c| c.used)
                .map(|c| c.artifact_id.clone())
                .collect(),
        );
        match self.store.record_ask(ask).await {
            Ok(id) => {
                // The subjects the plan named and the base could not cover,
                // written as gaps of their own. They hang off the question,
                // so this runs only where the question was recorded — the
                // same door, in the same breath, and never for an ask whose
                // insert failed and has no row to hang from.
                //
                // The vector costs nothing: the fan-out embedded each subject
                // in order to search for it, and the query cache still holds
                // what it embedded. A subject whose vector has fallen out of
                // the cache is stored without one and skipped by every reader
                // here — the fact that it was named is still worth more than
                // the row is worth refusing.
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

/// How many tokens of excerpt one answer may be built from.
///
/// Reserve what the completer will ask for, not a constant. The endpoint
/// counts the prompt and the requested ceiling against one window, so
/// packing excerpts up to `context - 1024` while the call goes out demanding
/// `max_output_tokens` of reply is a request the server refuses outright —
/// and it refuses it on precisely the queries that retrieved enough to be
/// worth answering.
///
/// The margin comes off the top too. `ceiling_for_prompt` holds back
/// headroom for the estimate being an estimate, and it holds it back out of
/// the *reply*: packing up to `max_output_tokens` exactly and then being
/// charged that margin is how a 32k window with a 2k ceiling ends up asking
/// for one token of answer. Reserving it here is what makes the two halves
/// agree.
///
/// Never more than half the window, though. The reserve is configuration and
/// the window is configuration, and nothing makes the two agree: a role
/// whose ceiling is its whole context (4096 and 4096, which is an ordinary
/// shape for a local model) reserves everything, packs nothing, and answers
/// "too large for the context window" to every question ever asked without
/// once calling the model. Half a window of excerpts and half a window of
/// answer is a worse answer than the operator asked for; no answer is not an
/// answer.
///
/// One rule for the answer call and the follow-up call, which packs the same
/// excerpts against a different model under a different system prompt. The
/// half-window clamp was itself a fix; a rule kept in two places gets tuned in
/// one and leaves the other refusing the second round for size on exactly the
/// questions worth it.
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

    /// A completer that asks, from inside the model call, whether background
    /// work could start right now. That is the question the lane exists to
    /// answer, and the only place it can be asked honestly.
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
        // Priority, not preemption: the worker must not put a new window in
        // front of someone who is waiting. What it cannot do is stop the window
        // already running, which is the ~73s ceiling this deliberately accepts.
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
        // The lease is RAII, so the interesting failure is one that leaks: a
        // single question would then hold background work off forever.
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

    /// Answers with one reply however often it is asked, and counts the
    /// asking. `ScriptedCompleter` runs out of replies, which would turn a
    /// second call into an error the ask swallows; this one would happily
    /// answer a third round, so a test that sees only one saw a bound in the
    /// code rather than a fake refusing to play along.
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

    /// A seeded core whose planning model answers `reply`, plus the handle to
    /// count what it was asked. `None` wires no planner at all.
    async fn core_with_planner(reply: Option<&str>) -> (Core, std::sync::Arc<Counting>) {
        let mut core = test_core().await;
        let model = Counting::saying(reply.unwrap_or_default());
        core.planner = reply.map(|_| model.clone() as std::sync::Arc<dyn crate::infer::Completer>);
        seed(&core, 3, 4).await;
        (core, model)
    }

    /// Every `Retrieved` round and every query any `Needs` named, in the order
    /// they were emitted.
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

    /// "Off" must mean no call at all — asserted on a counting fake rather than
    /// inferred from the answer looking the same.
    ///
    /// The counting model is the *answer* model here, which is the tempting
    /// wrong implementation: `plan_tier` falls back to the ask role's own
    /// endpoint, and a fallback read at call time rather than at config time
    /// would spend an ask-endpoint call on every question with the feature
    /// switched off.
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

    /// On, and the model says it has enough: still exactly one retrieval.
    #[tokio::test]
    async fn an_empty_plan_skips_the_fan_out() {
        let (core, model) = core_with_planner(Some(r#"{"need": []}"#)).await;
        let (rounds, needs) = rounds_and_needs(&core).await;
        assert_eq!(rounds, vec![1]);
        assert!(needs.is_empty(), "nothing was needed, so nothing was said");
        assert_eq!(model.calls(), 1, "asked once, and once is the whole budget");
    }

    /// On, and the model names something: one fan-out and never a second.
    /// Bounded means bounded — and this fake answers with a need every time it
    /// is asked, so a loop would run until the base ran out of patience rather
    /// than stopping itself.
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

    /// The plan says out loud what the base does not hold, in the model's own
    /// words, for a question a person asked in earnest — and the call was
    /// already paid for. The subject that came back with nothing becomes a gap
    /// rather than being discarded with the round.
    #[tokio::test]
    async fn a_planned_subject_the_base_could_not_cover_becomes_a_gap() {
        let (mut core, _) = core_with_planner(Some(r#"{"need": ["mounting an E01"]}"#)).await;
        core.learn.enabled = true;
        // A base where nothing matches closely. Cosine tops out at 1, so every
        // candidate is under this — which is the situation the kind exists for,
        // stated as a threshold rather than hoped for from a fake embedder.
        core.weak_below = 1.0;
        core.ask(&req("chunk"), Door::Ui.by("me")).await.unwrap();

        let gaps = core
            .store
            .open_gap_refs(core.embedder.model(), core.weak_below)
            .await
            .unwrap();
        let subjects: Vec<&str> = gaps
            .iter()
            .filter(|g| g.kind == crate::store::gaps::GapKind::Subject)
            .map(|g| g.text.as_str())
            .collect();
        assert_eq!(subjects, vec!["mounting an E01"]);
    }

    /// The same rule `record_ask` already draws, and for the same reason: a
    /// subject is derived from a question, a question is personal data of the
    /// same kind as a query, and API and MCP callers asked for the smallest
    /// footprint. The bump rides with the recording rather than around it.
    #[tokio::test]
    async fn the_api_door_names_no_subjects() {
        let (mut core, _) = core_with_planner(Some(r#"{"need": ["mounting an E01"]}"#)).await;
        core.learn.enabled = true;
        core.weak_below = 1.0;
        core.ask(&req("chunk"), Door::Api).await.unwrap();

        let gaps = core
            .store
            .open_gap_refs(core.embedder.model(), core.weak_below)
            .await
            .unwrap();
        assert!(
            !gaps
                .iter()
                .any(|g| g.kind == crate::store::gaps::GapKind::Subject),
            "a door that records no question recorded what its plan named"
        );
    }

    /// A plan naming several subjects is one round of retrieval per subject and
    /// still one plan. The cap belongs to the parser, so a model that names
    /// more subjects than the fan-out may run cannot widen it from the wire.
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

    /// The excerpts the model sees are one list, not several concatenated: an
    /// artifact already in front of it is a wasted excerpt, not a stronger one.
    /// Every round retrieves the same small base here, so everything the
    /// planned ones found is a duplicate of something the first did.
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

    /// The prompt forbids repeating the question back; nothing but this
    /// enforces it. An echo is a search whose results the first round already
    /// holds, followed by a re-pack that can take its neighbours out of the
    /// prompt in exchange for nothing.
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

    /// One echoed query beside a useful one is one useful round. Refusing the
    /// whole plan over its first entry would throw away retrieval the model
    /// asked for and was right to ask for.
    #[tokio::test]
    async fn an_echoed_query_is_dropped_without_taking_the_rest_of_the_plan_with_it() {
        let (core, _) = core_with_planner(Some(r#"{"need": ["chunk", "filler"]}"#)).await;
        let (rounds, needs) = rounds_and_needs(&core).await;
        assert_eq!(rounds, vec![1, 2], "the surviving query never ran");
        assert_eq!(needs, vec!["filler".to_string()]);
    }

    /// Embeds on one word each way, so two queries can retrieve two disjoint
    /// halves of one corpus.
    ///
    /// The hashing fake cannot do this: every artifact is some arbitrary
    /// distance from every query, so both rounds retrieve the same small base
    /// and the second one is never asked to contribute anything. That is the
    /// gap that let the merge's ordering be pinned only on hand-built lists.
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
                    // Never the zero vector: a cosine against nothing is not a
                    // ranking, it is a division the store would have to guess at.
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

    /// One corpus of `topics.len()` × 3 artifacts, each topic's three adjacent
    /// and the topics in the order given — so a search for one term ranks that
    /// topic's three, the cliff cuts the rest, and the reach sideways from the
    /// last hit pulls in a neighbour from whatever follows.
    async fn seed_topics(core: &Core, topics: &[&str]) {
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let new: Vec<NewArtifact> = (0..topics.len() as i64 * 3)
            .map(|i| NewArtifact {
                ordinal: i,
                text: format!("{} topic {i} filler filler", topics[i as usize / 3]),
                corpus_span: None,
                title: Some(format!("t{i}")),
                category: Some("reference".into()),
                tags: vec![],
                segment_idx: None,
                caveats: vec![],
            })
            .collect();
        let made = core.store.insert_artifacts(&src.id, &new).await.unwrap();
        for c in &made {
            crate::jobs::embed::run(core, &c.id).await.unwrap();
        }
    }

    /// The whole mechanism, end to end: a question the base answers by halves.
    ///
    /// Round one ranks the three `alpha` artifacts, the cliff cuts the three
    /// `beta` ones, and adjacency reaches exactly one of them back as a
    /// neighbour. The plan then asks for `beta`, and that round ranks all
    /// three — including the one round one had only reached.
    ///
    /// `dropped` is the assertion that matters. Every artifact retrieved is in
    /// front of the model by the end, so nothing was dropped; a merge that let
    /// round one's `via` keep that artifact in the speculative tail would leave
    /// it below the seam, counted as a hit the ranking lost while it sat in the
    /// prompt — and packed as the first thing a tighter window would throw
    /// away.
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

    /// The reason the fan-out exists, end to end. A question whose subject the
    /// base holds three separate halves of: round one ranks `alpha`, the cliff
    /// cuts `beta` and `gamma`, and one plan names both of them. Two rounds run
    /// at once and their hits interleave, so the model ends up seeing all three
    /// subjects rather than whichever one the single ranked list favoured.
    ///
    /// A `Some(query)` follow-up could only ever have recovered one of the two.
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

    /// One planned round failing does not take the others with it: the fan-out
    /// returns what came back and the ask answers from it. Failing the whole
    /// question because one of several extra searches failed would trade an
    /// answer for a strictly better answer's absence.
    ///
    /// Driven through `fan_out` rather than through a plan, because the empty
    /// query that makes `search_with` fail is one `parse_plan` drops before it
    /// ever reaches a round — going in through the planner would assert nothing
    /// but the parser's filtering.
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

    /// A planning call that fails must never fail the ask: the operator asked
    /// a question, not for a retrieval strategy.
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

    /// Prose where JSON was asked for means "no fan-out" rather than "search
    /// for whatever that was".
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
                corpus_span: None,
                title: Some(format!("t{i}")),
                category: Some("reference".into()),
                tags: vec![],
                segment_idx: None,
                caveats: vec![],
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

    /// A question typed at a shell is the strongest statement of a need engram
    /// can receive, and it was written down nowhere at all: `record_ask`
    /// admitted `Door::Ui` alone.
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

    /// And the doors that are not a person, which must stay out of the log.
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
        // A caveat is the condition under which an artifact does not apply, and
        // an answer that quotes a destructive command without it is worse than
        // no answer. Caveats are not in the vector payload, so this asserts the
        // store lookup that puts them back.
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
                    ordinal: 0,
                    text: "Format the device with mkfs.".into(),
                    corpus_span: None,
                    title: Some("Format a device".into()),
                    category: None,
                    tags: vec![],
                    segment_idx: None,
                    caveats: vec!["Destroys every existing file on the device.".into()],
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

    /// Echoes its prompt back and records the ceiling it was handed, which
    /// together are the whole request the endpoint would have measured.
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

    /// The prompt and the reply are counted against one window by the endpoint,
    /// so packing excerpts up to a constant reserve while the call demands
    /// `max_output_tokens` of reply is a request the server refuses — and it
    /// refuses it on exactly the questions that retrieved enough to be worth
    /// answering. What is packed has to leave room for what is asked for.
    ///
    /// Both directions, in the two shapes an operator actually configures: a
    /// ceiling that fits comfortably inside its window, and one that eats most
    /// of it.
    #[tokio::test]
    async fn what_is_packed_leaves_room_for_the_reply_that_was_asked_for() {
        use crate::infer::budget::MAX_HEADROOM_TOKENS;

        for (context, max_output) in [(4096, 3072), (8192, 1024), (4096, 4096), (32768, 2048)] {
            let completer = std::sync::Arc::new(Ceilinged::new(context, max_output));
            let mut core = test_core().await;
            core.completer = Some(completer.clone());
            // Excerpts sized against the window, so packing actually fills it
            // in every case — a prompt that leaves the window half empty tests
            // nothing about what happens when it does not.
            seed(&core, 20, context / 20).await;
            let mut r = req("anything");
            r.limit = Some(20);
            let out = core.ask(&r, Door::Api).await.unwrap();

            // The echoed prompt is what actually went to the endpoint, and the
            // recorded ceiling is what the call asked for on top of it.
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

            // And it is a *usable* ceiling, not merely a positive one. The
            // reserve is what packing set aside for the answer; the answer must
            // get it back, less the margin held for the estimate. Asserting
            // only `> 0` let a ceiling of 1 — an empty reply, every time —
            // through at the very ordinary 32k/2k shape.
            let floor = max_output.min(context / 2) - MAX_HEADROOM_TOKENS;
            assert!(
                ceiling >= floor,
                "at context {context} / ceiling {max_output}: the call asked for {ceiling}, \
                 but packing reserved room for at least {floor}"
            );
        }
    }

    /// The reserve is configuration and the window is configuration, and nothing
    /// makes the two agree. A role whose ceiling is its whole window — 4096 and
    /// 4096, an ordinary local shape, and what every default lands on — reserved
    /// everything and packed nothing, so every question ever asked was answered
    /// "too large for the configured context window" without the model being
    /// called at all.
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

    /// An answer that stops because it ran out of room reads exactly like one
    /// that stopped because it was finished, and the difference is the reader's
    /// to act on.
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

    /// A reranker whose scores fall off a cliff after the third document, so
    /// what `ask` does with a relevance cliff can be asserted rather than
    /// inferred. The fused scores a search produces on their own are smooth by
    /// construction — no gap in them stands out — so a cliff has to be put
    /// there deliberately for this to be about `ask` at all.
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
        // Five small excerpts and a whole context window to put them in: the
        // budget can cut nothing here, so anything missing from the citations
        // was cut by the cliff and by nothing else.
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

        // Counted over the ranked citations only. Reached neighbours are in
        // this list too and are supposed to be: they never competed, so the
        // cliff has nothing to say about them. `via` is what tells them apart.
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

    /// Citations that placed in the ranking, as opposed to ones reached
    /// sideways from those that did.
    fn ranked(out: &AskResponse) -> usize {
        out.citations.iter().filter(|c| c.via.is_none()).count()
    }

    /// The answer is often in the artifact next to the one that matched, and
    /// retrieval cannot find it: it does not carry the question's terms and
    /// does not sit near it in the embedding space. Only structure reaches it.
    ///
    /// The same cliff as the test above, so the artifact at ordinal 3 is one
    /// the ranking cut and adjacency puts back — reached, not retrieved, and
    /// marked as such.
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

    /// The ordering is the safety property: a reached artifact carries no score
    /// comparable to a retrieved one, so anything that reads this list as
    /// ranked — the cliff, the packing order, `record_ask` — must find every
    /// ranked hit ahead of every neighbour.
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

        // The cap, asserted where it is actually spent. `the_neighbour_cap_holds`
        // pins `append_neighbours` in isolation; nothing else pins the call
        // site, so a wiring bug admitting fifty neighbours would pass every
        // other test here.
        assert!(
            out.citations.len() <= ranked(&out) + retrieve::NEIGHBOUR_MAX,
            "{} citations against {} ranked hits is past the cap",
            out.citations.len(),
            ranked(&out)
        );
    }

    /// A reach that found nothing new must not report anything as lost. The
    /// question asked for the retrieved hits, and `dropped` answers only for
    /// those — otherwise it would grow every time the reach worked.
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

    /// The other half of the reach, and the half adjacency cannot do: an
    /// artifact linked to a hit by having been retrieved beside it before.
    ///
    /// The linked artifact is in a second corpus, so it shares its `ordinal`
    /// sequence with nothing that was retrieved and adjacency provably cannot
    /// be what brought it in. And `search_with` refuses to associate for
    /// `Door::Ask` — text that never matched the question must not become
    /// source material for the answer — so `via` on a citation here can only
    /// have been set by `reach_sideways`.
    #[tokio::test]
    async fn an_artifact_linked_to_a_hit_is_reached_where_adjacency_cannot_go() {
        let mut core = test_core().await;
        // Links are learned from recorded searches, and `test_core` records
        // none, so the reach stays shut until the layer is switched on.
        core.learn.enabled = true;
        seed(&core, 5, 4).await;

        // A second corpus, on a subject the query does not ask about. Its raw
        // text has to differ from `seed`'s: `insert_corpus` deduplicates on the
        // document's shingle signature, so re-using "raw" hands back the corpus
        // that is already there and the artifact below lands beside the seeded
        // ones with a colliding ordinal — reachable by adjacency, which is
        // precisely what this test must rule out.
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
                    ordinal: 0,
                    text: "Reconciling the quarterly ledger against the register.".into(),
                    corpus_span: None,
                    title: Some("ledger".into()),
                    category: Some("reference".into()),
                    tags: vec![],
                    segment_idx: None,
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();
        // Deliberately never embedded. It has no vector, so retrieval provably
        // cannot return it however the fake embedder happens to score things —
        // structure is the only route to it, which is the whole claim here.
        let far = made[0].id.clone();

        // Linked to every artifact of the first corpus, so whichever of them
        // place highest, the anchors have somewhere to hop. Weight well above
        // `associate.show_min`, at the clock the reach decays to.
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
        // The vector it retrieved with travels with it, so a gap can be
        // clustered later without re-embedding.
        let dim: i64 = sqlx::query_scalar("SELECT vec_dim FROM ask_events WHERE id = ?")
            .bind(&id)
            .fetch_one(&core.store.pool)
            .await
            .unwrap();
        assert!(dim > 0);
    }

    #[tokio::test]
    async fn only_the_artifacts_an_answer_cited_are_engaged_by_it() {
        // `used` already separates shown from used, and an artifact the model
        // cited was used. Every excerpt that merely fit the window is not — the
        // association layer already learns from display.
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
        // Every excerpt was retrieved by the same searches, so they share a
        // baseline and the only difference left between them is the citation.
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
        // The recorded decision about what `/mcp` counts as use: nothing. Its
        // search already bumps activation like every other door, and counting
        // what it returned as engagement on top would only relearn what
        // association learns from display. The citation is the one honest
        // signal a question gives, and it is only recorded at the web door —
        // so the bump rides with the recording rather than around it.
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
            // Carries a literal no excerpt does, so the abstention branch of
            // the check is what keeps `unsupported` empty below rather than the
            // reply happening to have nothing in it.
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
        // The common case, and the one that must not badge every answer: the
        // seeded artifacts read "chunk 0 filler filler …".
        let mut core = test_core().await;
        core.completer = Some(std::sync::Arc::new(crate::infer::fake::FakeCompleter {
            reply: Some("The excerpt says `chunk 0 filler` and nothing else.".into()),
        }));
        seed(&core, 3, 4).await;
        let out = core.ask(&req("chunk"), Door::Api).await.unwrap();
        assert!(out.unsupported.is_empty(), "{:?}", out.unsupported);
    }

    /// The two doors must never drift. `/api/v1/ask` and MCP collect; the page
    /// streams; both must describe the same ask. This is the test that keeps
    /// them honest as the code changes.
    #[tokio::test]
    async fn the_collected_answer_equals_the_streamed_one() {
        let mut core = test_core().await;
        // A cliff and a reach, so the two doors are compared on an ask that
        // exercises everything between retrieval and the answer rather than on
        // the shortest path through it.
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

    /// The rail must be readable while the model is still writing, which means
    /// the excerpts have to arrive before the first token.
    #[tokio::test]
    async fn citations_arrive_before_the_first_token_and_done_is_last() {
        let mut core = test_core().await;
        // Several deltas rather than one, so "before the first token" is a
        // claim about ordering and not about there being a single token.
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

    /// An ask is recorded once, whichever door it came through — the harness
    /// reads these rows and would double-count otherwise.
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

    /// The sink the completer writes into is bounded, and `send` waits when it
    /// is full. A producer that awaited the call before draining would stall
    /// the endpoint's read loop on the first answer longer than the channel —
    /// which is every answer worth reading — while still passing every test
    /// whose fake reply fits in one delta.
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

    /// A completer that stops mid-answer and waits to be let go, so a test can
    /// observe the world at the one instant that matters: the reader has gone
    /// and the model call has not.
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

    /// Closing the tab is the streaming door's ordinary ending, not an edge
    /// case, and dropping the stream does not stop the call: it is detached and
    /// the GPU stays busy until it returns. A lane that ended with the reader
    /// would let the worker start a seventy-second window against hardware an
    /// interactive call still occupies — the interleaving the lane exists to
    /// prevent, inverted rather than merely leaked.
    #[tokio::test]
    async fn a_reader_who_leaves_mid_answer_does_not_hand_the_gpu_to_the_worker() {
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let mut core = test_core().await;
        core.learn.enabled = true;
        core.completer = Some(std::sync::Arc::new(Stalling {
            release: release.clone(),
        }));
        seed(&core, 3, 4).await;

        // Read as far as the first token, so the call is provably in flight
        // rather than merely spawned, and then leave.
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

        // And it is a lease, not a block: once the call it was taken for really
        // has ended, the worker gets its turn.
        release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), core.gate.background())
            .await
            .expect("the lane was never released after the call ended");

        // Nothing recorded it, and nothing recorded it twice. Whether an answer
        // nobody read should be stored at all is a decision for the door that
        // streams, not a side effect of this one.
        assert_eq!(core.store.ask_stats().await.unwrap().asked, 0);
    }

    /// Streams its answer in more pieces than the sink holds, which is the
    /// shape every real answer has and the one a one-delta fake never has.
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
        core.synthesis = crate::config::SynthesisMode::Off;
        // One corpus, one segment, three abutting passages, all hits.
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
            }),
            title: Some("Recovery".into()),
            category: None,
            tags: vec![],
            segment_idx: Some(0),
            caveats: vec![],
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
        // A completer that keeps its prompts, so the test can read what the
        // model was shown.
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
        // One excerpt carries the stitched text in document order, once.
        assert!(
            prompt.contains("first part\nsecond part\nthird part"),
            "{prompt}"
        );
        assert_eq!(
            prompt.matches("Recovery").count(),
            2,
            "heading once in the excerpt, once in the question: {prompt}"
        );
        // Every constituent is still a citation.
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
        // The run's text, its heading, and the number the rail resolves to an
        // artifact all have to name the same passage: the one the text opens
        // with. Numbering the block after whichever member ranked highest put
        // one artifact's heading over another artifact's rail row, and linked
        // a claim to a page that does not hold the words.
        //
        // `stitch_passages` directly rather than through `ask`: what this is
        // about is a run whose best hit is not its first line, and asking the
        // retrieval to produce that ordering is asking it for something it does
        // not promise. Handed in, it is the case every time.
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
            }),
            title: Some(title.into()),
            category: None,
            tags: vec![],
            segment_idx: Some(0),
            caveats: vec![],
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

        // The second passage ranked first: it is the anchor, and it keeps the
        // block's number — but not the heading, which belongs to line 1.
        let hit =
            |c: &crate::store::artifacts::Chunk, score: f32| crate::core::search::SearchResult {
                artifact_id: c.id.clone(),
                corpus_id: c.corpus_id.clone().unwrap_or_default(),
                title: c.title.clone(),
                text: c.text.clone(),
                category: None,
                tags: vec![],
                score,
                status: None,
                superseded_by: None,
                last_verified_at: None,
                weak: false,
                primed: false,
                in_sitting: false,
                past_cliff: false,
                similarity: None,
                titled_by_corpus: false,
                via: None,
                reason: None,
                explanation: None,
                model_written: false,
                synthesized: false,
                origin_count: 0,
            };
        let hits = vec![hit(&made[1], 0.9), hit(&made[0], 0.5)];
        let mut blocks = vec![
            crate::infer::prompt::ask_excerpt(1, "Backups", &made[1].text, &[]),
            crate::infer::prompt::ask_excerpt(2, "Recovery", &made[0].text, &[]),
        ];
        core.stitch_passages(&hits, &mut blocks).await;

        // The text goes under the number of the passage it starts with —
        // `made[0]`, which retrieval placed second — heading and all.
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
        // And the other member keeps its slot, pointing at it. A claim drawn
        // from `made[1]` can be cited as [1] and still resolve to `made[1]`.
        assert_eq!(
            blocks[0], "[1] (continues [2])",
            "every constituent keeps a citable number: {}",
            blocks[0]
        );
    }
}
