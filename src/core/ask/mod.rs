pub mod check;
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

            // Held for the whole answer rather than around the completion, because
            // a search embeds the query and that is a model call too. A gap between
            // them is a gap the worker would fill with a window, and a window is
            // twenty to seventy seconds of somebody waiting.
            //
            // Taking the lane does not make an in-flight call stop; nothing here
            // cancels. It keeps the worker from putting anything new in front of
            // this one. Held to the end of the stream, which for the streaming
            // door is the last token rather than the return of a call.
            let _lane = core.gate.interactive();

            // No per-source cap: an answer often lives in one document, and
            // withholding its paragraphs to keep the citation list varied would
            // make the answer worse, not fairer.
            let (mut hits, _) = core
                .search_with(
                    &SearchQuery {
                        q: req.q.clone(),
                        limit: req.limit.unwrap_or(8),
                        tags: req.tags.clone(),
                        category: req.category.clone(),
                        // Asking a question is as deliberate as a search gets.
                        mark: true,
                        include_deprecated: false,
                        include_superseded: false,
                    },
                    None,
                    // Deliberately not captured: the right answer to a question is
                    // a synthesis across several artifacts, so "which one was it"
                    // has no well-defined meaning for someone judging it later.
                    crate::store::feedback::Door::Ask,
                )
                .await?;

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
                yield AskEvent::Retrieved { round: 1, shown: 0, dropped: 0, cliff_at: None };
                yield AskEvent::Citations(vec![]);
                let response = core.record_ask(&req, &origin, response).await?;
                yield AskEvent::Done(Box::new(response));
                return;
            }

            // Cut the ranked list where its relevance falls off, before a single
            // row is read for it: an excerpt below the cliff makes the answer worse
            // as well as dearer, and its caveats are a lookup spent on something
            // that will not be sent. `dropped` is still measured against everything
            // retrieved, so a citation lost to the cliff is as visible as one lost
            // to the window.
            let retrieved = hits.len();
            let scores: Vec<f32> = hits.iter().map(|h| h.score).collect();
            let cliff_at = crate::core::search::cliff(&scores);
            hits.truncate(retrieve::above_cliff(&scores));

            // Artifacts reached sideways rather than retrieved are appended here,
            // after the cliff has been taken — they carry no score comparable to a
            // ranked hit, and must never enter the scores it is computed from.
            // `scores` is consumed above and never recomputed, so nothing appended
            // from here on can reach `cliff` at all.
            let ranked = hits.len();
            core.reach_sideways(&mut hits, cliff_at).await;

            // Caveats are the conditions under which an excerpt does not apply, and
            // an answer that quotes "run `mkfs` on the device" without "destroys
            // everything already on it" is worse than no answer. They are not in
            // the vector payload — what gets embedded is a separate decision — so
            // they are read from the store, which costs one cheap SQLite lookup per
            // hit and no inference. An excerpt whose row has since been deleted
            // simply carries none.
            let mut blocks: Vec<String> = Vec::with_capacity(hits.len());
            for (i, h) in hits.iter().enumerate() {
                let caveats = core
                    .store
                    .get_artifact(&h.artifact_id)
                    .await
                    .map(|c| c.caveats)
                    .unwrap_or_default();
                blocks.push(ask_excerpt(
                    i + 1,
                    h.title.as_deref().unwrap_or_default(),
                    &h.text,
                    &caveats,
                ));
            }

            // Reserve what the completer will ask for, not a constant. The endpoint
            // counts the prompt and the requested ceiling against one window, so
            // packing excerpts up to `context - 1024` while the call goes out
            // demanding `max_output_tokens` of reply is a request the server refuses
            // outright — and it refuses it on precisely the queries that retrieved
            // enough to be worth answering.
            //
            // The margin comes off the top too. `ceiling_for_prompt` holds back
            // headroom for the estimate being an estimate, and it holds it back out
            // of the *reply*: packing up to `max_output_tokens` exactly and then
            // being charged that margin is how a 32k window with a 2k ceiling ends
            // up asking for one token of answer. Reserving it here is what makes
            // the two halves agree.
            //
            // Never more than half the window, though. The reserve is configuration
            // and the window is configuration, and nothing makes the two agree: a
            // role whose ceiling is its whole context (4096 and 4096, which is an
            // ordinary shape for a local model) reserves everything, packs nothing,
            // and answers "too large for the context window" to every question ever
            // asked without once calling the model. Half a window of excerpts and
            // half a window of answer is a worse answer than the operator asked for;
            // no answer is not an answer.
            let context = core.completer.context_tokens();
            let reserve = core
                .completer
                .max_output_tokens()
                .saturating_add(crate::infer::budget::MAX_HEADROOM_TOKENS)
                .min(context / 2);
            let budget = context
                .saturating_sub(core.counter.count(ASK_SYSTEM))
                .saturating_sub(core.counter.count(&req.q))
                .saturating_sub(reserve);

            // Highest score first, so what gets cut is what mattered least.
            let kept = pack_by_budget(&blocks, &core.counter, budget);
            // Measured over the retrieved hits alone. `dropped` answers "what did I
            // ask for and not get shown", and nobody asked for a neighbour — one
            // that does not fit was never owed a place, and counting it would make
            // `dropped` grow every time the reach worked. Packing keeps a prefix
            // and the neighbours sit after the ranked hits, so the ranked ones that
            // survived are exactly `kept.min(ranked)`.
            let dropped = retrieved - kept.min(ranked);
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
                yield AskEvent::Retrieved { round: 1, shown: 0, dropped, cliff_at };
                yield AskEvent::Citations(vec![]);
                let response = core.record_ask(&req, &origin, response).await?;
                yield AskEvent::Done(Box::new(response));
                return;
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
                context,
                spent,
                core.completer.max_output_tokens(),
            );

            // The excerpts go out before the first token, so the rail beside the
            // answer is readable while the answer is still being written.
            let citations: Vec<SearchResult> = hits.into_iter().take(kept).collect();
            yield AskEvent::Retrieved {
                round: 1,
                shown: citations.len(),
                dropped,
                cliff_at,
            };
            yield AskEvent::Citations(citations.clone());

            // The sink is bounded, so the call has to run *beside* this loop and
            // not before it. A producer that awaited the call and drained
            // afterwards would block the endpoint's read loop on the first answer
            // longer than the channel — which is every answer worth reading — and
            // would still pass any test whose fake reply fits in 64 deltas.
            let (tx, mut rx) = tokio::sync::mpsc::channel::<crate::infer::Delta>(64);
            let completer = core.completer.clone();
            let prompt = user.clone();
            let call = tokio::spawn(async move {
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
            let response = core.record_ask(&req, &origin, response).await?;
            yield AskEvent::Done(Box::new(response));
        }
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
                // Weakness is read from a similarity to the query, and there is
                // no similarity here to read. It has to be demonstrated, never
                // assumed — in either direction.
                weak: false,
                primed: false,
                // The cliff was computed over scores this one was never in.
                past_cliff: false,
                // What makes a reached artifact tellable apart from a retrieved
                // one, by a reader and by a test alike: a ranked hit has no
                // `via`, and this one names the hit it was reached from.
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
    ) -> Result<AskResponse> {
        if !(self.feedback.enabled && origin.door == Door::Ui) {
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
            citations: response
                .citations
                .iter()
                .map(|c| NewAskCitation {
                    artifact_id: c.artifact_id.clone(),
                    score: c.score,
                })
                .collect(),
        };
        match self.store.record_ask(ask).await {
            Ok(id) => response.event_id = Some(id),
            Err(e) => tracing::warn!(error = %e, "could not record the question"),
        }
        Ok(response)
    }
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
        core.completer = probe.clone();
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
        core.completer = std::sync::Arc::new(crate::infer::fake::FakeCompleter { reply: None });
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
            core.completer = completer.clone();
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
        core.completer = std::sync::Arc::new(Ceilinged::new(4096, 4096));
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
        core.completer = std::sync::Arc::new(Truncating);
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
        // `associate.enabled` is already the shipped default; links are learned
        // from recorded searches, so the reach stays shut until this is on too.
        core.feedback.enabled = true;
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
        core.feedback.enabled = true;
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
    async fn an_api_or_mcp_ask_is_never_recorded() {
        let mut core = test_core().await;
        core.feedback.enabled = true;
        seed(&core, 3, 4).await;
        for door in [Door::Api, Door::Mcp] {
            let out = core.ask(&req("chunk"), door).await.unwrap();
            assert!(out.event_id.is_none(), "{door:?} recorded a question");
        }
        assert_eq!(core.store.ask_stats().await.unwrap().asked, 0);
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
        core.feedback.enabled = true;
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
        core.completer = std::sync::Arc::new(crate::infer::fake::FakeCompleter {
            // Carries a literal no excerpt does, so the abstention branch of
            // the check is what keeps `unsupported` empty below rather than the
            // reply happening to have nothing in it.
            reply: Some(
                "Not in the knowledge base. The excerpts cover `chunk 0` only, not `wipefs --all /dev/sdX`."
                    .into(),
            ),
        });
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
        core.completer = std::sync::Arc::new(crate::infer::fake::FakeCompleter {
            reply: Some("Run `wipefs --all /dev/sdX` first, then read chunk 0.".into()),
        });
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
        core.completer = std::sync::Arc::new(crate::infer::fake::FakeCompleter {
            reply: Some("The excerpt says `chunk 0 filler` and nothing else.".into()),
        });
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
        core.completer = std::sync::Arc::new(Chatty { parts: 3 });
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
        core.feedback.enabled = true;
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
        core.completer = std::sync::Arc::new(Chatty { parts: PARTS });
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
}
