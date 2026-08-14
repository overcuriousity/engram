use crate::core::Core;
use crate::error::{Error, Result};
use crate::store::artifacts::{ArtifactStatus, Chunk, NewArtifact};
use crate::store::corpora::CorpusStatus;
use crate::store::jobs::Stage;
use crate::vector::{VectorPayload, VectorPoint};

/// Fraction of the embedder's hard limit a chunk is allowed to occupy. The
/// remaining headroom absorbs tokenizer estimation error.
const SAFETY: f32 = 0.8;

/// Chunks sent to the embedder in one request. Bounded because a long source
/// can hold hundreds of chunks and endpoints cap how many inputs they accept.
const BATCH: usize = 32;

/// Text for the embedding carries the title: it holds topical signal the body
/// often leaves implicit.
fn embed_text(chunk: &Chunk) -> String {
    match &chunk.title {
        Some(t) => format!("{t}\n{}", chunk.text),
        None => chunk.text.clone(),
    }
}

pub async fn run(core: &Core, artifact_id: &str) -> Result<()> {
    run_with_limit(core, artifact_id, default_limit(core)).await
}

fn default_limit(core: &Core) -> usize {
    (core.embedder.max_input_tokens() as f32 * SAFETY) as usize
}

/// Does this error mean the input itself is too big, rather than the endpoint
/// being unwell?
///
/// A local inference server has a hard physical batch size — llama.cpp answers
/// `input (1030 tokens) is too large to process` — and no amount of retrying
/// changes that. Configuration is meant to keep chunks under it, but the
/// configured ceiling is a claim about the model while this is the server's
/// real limit, and the two disagree more often than not.
fn input_too_large(e: &Error) -> bool {
    let Error::Inference {
        role: "embed",
        detail,
    } = e
    else {
        return false;
    };
    let d = detail.to_ascii_lowercase();
    d.contains("too large")
        || d.contains("too long")
        || d.contains("exceeds")
        || d.contains("413")
        || d.contains("batch size")
}

pub async fn run_with_limit(core: &Core, artifact_id: &str, limit: usize) -> Result<()> {
    let chunk = core.store.get_artifact(artifact_id).await?;
    let text = embed_text(&chunk);

    if core.counter.count(&text) > limit {
        // Our own estimate, which can be wrong in either direction. Do not
        // shred the chunk on a guess.
        return split_oversize(core, &chunk, limit, false).await;
    }

    // Paced like every other inference call. `split_into_artifact_jobs` can
    // leave hundreds of these behind for one document, and against a dead
    // endpoint an ungated path spends a timeout apiece without any of those
    // failures ever reaching the breaker.
    let permit = core.gate.background().await;
    match embed_batch(core, std::slice::from_ref(&chunk), vec![text.clone()]).await {
        Ok(()) => permit.succeeded(),
        Err(e) if input_too_large(&e) => {
            // Refused, not failed: the endpoint answered. Counting this toward
            // the breaker let a handful of oversize chunks stop every
            // background call in the system.
            permit.refused();
            // The endpoint's real ceiling is lower than the configured one, so
            // halving the configured limit would change nothing. Halve what the
            // chunk actually measures instead: that shrinks on every refusal
            // and converges on whatever the server will take.
            let measured = core.counter.count(&text);
            let smaller = (measured / 2).max(crate::infer::budget::MIN_SEGMENT_TOKENS);
            tracing::warn!(
                artifact_id,
                measured,
                smaller,
                error = %e,
                "endpoint refused the chunk as too large; splitting instead of retrying"
            );
            // The server said no. That is a fact rather than an estimate, so
            // this split has to succeed whatever the text looks like.
            return split_oversize(core, &chunk, smaller, true).await;
        }
        Err(e) => {
            permit.failed(&e);
            return Err(e);
        }
    }
    match &chunk.corpus_id {
        Some(corpus_id) => settle_corpus(core, corpus_id).await,
        // A merged artifact belongs to no corpus, so there is no document whose
        // coverage this embedding advances and nothing to settle.
        None => Ok(()),
    }
}

/// Embed every chunk of a source that is still waiting, in as few inference
/// calls as the batch size allows.
///
/// One call per source rather than per chunk is the whole point: the embedding
/// endpoint is the slow, rate-limited, and often paid part of ingest.
pub async fn run_corpus(core: &Core, corpus_id: &str) -> Result<()> {
    run_corpus_with_limit(core, corpus_id, default_limit(core)).await
}

pub async fn run_corpus_with_limit(core: &Core, corpus_id: &str, limit: usize) -> Result<()> {
    let pending = core.store.pending_artifacts_for_corpus(corpus_id).await?;

    // An oversize chunk becomes siblings instead of a vector, so it cannot ride
    // along in a batch — and finding out how to cut it can itself cost a call.
    // It gets a unit of its own and this job goes on without it.
    let mut batch: Vec<Chunk> = Vec::with_capacity(pending.len());
    let mut texts: Vec<String> = Vec::with_capacity(pending.len());
    for chunk in pending {
        let text = embed_text(&chunk);
        if core.counter.count(&text) > limit {
            // Splitting is not free: `split_oversize` falls through to embedding
            // the chunk whole when there is no boundary to cut on, and that is a
            // model call. Doing it here made a job that is allowed one call make
            // one per oversize chunk — fifty of them held the turn for fifty
            // cooldowns, which is the head-of-line blocking one-batch-per-run
            // exists to prevent. Its own unit instead, where `run_with_limit`
            // splits it paced like everything else.
            //
            // Idle-only: `rearm_if_more` brings this job back for every batch of
            // a long document, and `enqueue` would wind the attempts of a unit
            // already queued back to zero on each of them.
            core.store
                .rearm_idle_seq(Stage::Embed, "artifact", &chunk.id, 0)
                .await?;
        } else {
            texts.push(text);
            batch.push(chunk);
        }
    }

    // One batch per run, not every batch. A document with 277 chunks is nine
    // calls, and doing them in one job puts nine of them in front of everything
    // else the queue holds — unpaced, and with no chance for a question to slip
    // between them.
    let take = BATCH.min(batch.len());
    if take == 0 {
        return settle_corpus(core, corpus_id).await;
    }

    let permit = core.gate.background().await;
    match embed_batch(core, &batch[..take], texts[..take].to_vec()).await {
        Ok(()) => permit.succeeded(),
        // One oversize chunk fails the whole batch, and the batch cannot say
        // which. Per-chunk jobs isolate it, and that path splits it.
        Err(e) if input_too_large(&e) => {
            // Refused, not failed: the endpoint answered, and what it answered
            // is about one chunk in this batch rather than about the endpoint.
            permit.refused();
            tracing::warn!(corpus_id, error = %e, "batch held a chunk the endpoint will not take; isolating");
            return split_into_artifact_jobs(core, corpus_id).await;
        }
        Err(e) => {
            permit.failed(&e);
            return Err(e);
        }
    }

    // Settling only once nothing is left. Whether to come back for another
    // batch is the caller's to decide and act on — see `rearm_if_more`.
    if core
        .store
        .pending_artifacts_for_corpus(corpus_id)
        .await?
        .is_empty()
    {
        return settle_corpus(core, corpus_id).await;
    }
    Ok(())
}

/// Queue the next batch of a corpus that is not finished embedding.
///
/// Called by `run_one` *after* the job is completed, never from inside the
/// handler. The queue is keyed by `(stage, target)`, so re-arming from within
/// would upsert the very row the `complete_job` that follows then marks done —
/// and the corpus would silently stop half-embedded. The same trap took the
/// untried windows of a source once already.
pub async fn rearm_if_more(core: &Core, corpus_id: &str) -> Result<()> {
    if core
        .store
        .pending_artifacts_for_corpus(corpus_id)
        .await?
        .is_empty()
    {
        return Ok(());
    }
    // What is left may not be batch work any more. A batch that met a chunk the
    // endpoint refuses gives way to one unit per chunk and returns `Ok`, so this
    // runs straight afterwards and would put the same doomed batch back beside
    // the units that replaced it — converging, since those carry `seq = 0` and
    // win the ordering, but a wasted call and a reset attempt count per round.
    if core.store.pending_artifacts_are_isolated(corpus_id).await? {
        return Ok(());
    }
    // `seq` climbs, so later batches of a long document sink below the first
    // batches of documents captured since, instead of one source owning the
    // embedder until it is finished.
    let next_seq = core
        .store
        .job_seq(Stage::Embed, corpus_id)
        .await?
        .unwrap_or(0)
        + 1;
    //
    // Idle-only, and not merely because every automatic arming is. `run_one`
    // calls this after `complete_job`, and between those two awaits another
    // worker's `settle` can reach `finish`, whose own idle-only arming
    // legitimately resurrects the row this just closed — at which point a second
    // worker can claim it. A `Guard::Any` upsert would then flip that `running`
    // row back to `pending` and put two workers into `run_corpus` for one
    // corpus, where whichever finishes second closes the row the other re-armed
    // and the source is left half-embedded. That is the trap the doc comment
    // above describes, reached from the other side.
    core.store
        .rearm_idle_seq(Stage::Embed, "corpus", corpus_id, next_seq)
        .await
}

/// One inference call and one upsert for the whole slice. Chunks are marked
/// embedded only once Qdrant has durably accepted their vectors, so a crash
/// leaves work to redo rather than a chunk that claims to be searchable.
async fn embed_batch(core: &Core, chunks: &[Chunk], texts: Vec<String>) -> Result<()> {
    if chunks.is_empty() {
        return Ok(());
    }
    let vectors = core.embedder.embed(&texts).await?;
    if vectors.len() != chunks.len() {
        return Err(Error::Inference {
            role: "embed",
            detail: format!(
                "asked for {} embeddings and got {}; pairing them would attach vectors \
                 to the wrong chunks",
                chunks.len(),
                vectors.len()
            ),
        });
    }

    let points = chunks
        .iter()
        .zip(texts.iter())
        .zip(vectors)
        .map(|((c, text), vector)| VectorPoint {
            vector,
            // The same text the embedder saw, so the lexical and the semantic
            // half of a hit always describe the same thing.
            sparse: crate::vector::sparse::encode_document(text),
            payload: payload_of(c),
        })
        .collect();
    core.vectors.upsert(points).await?;

    for c in chunks {
        mark_indexed(core, c).await?;
    }
    Ok(())
}

/// Report a chunk indexed, unless it was edited while it was being embedded.
///
/// The revision is the one read before the inference call, so an edit that
/// landed in between wins: the mark does not apply, the chunk stays pending,
/// and the job the editor queued embeds the text that is actually there.
async fn mark_indexed(core: &Core, chunk: &Chunk) -> Result<()> {
    let landed = core
        .store
        .mark_embedded(&chunk.id, core.embedder.model(), chunk.embed_rev)
        .await?;
    if !landed {
        tracing::info!(
            artifact_id = %chunk.id,
            "chunk was edited while it was being embedded; leaving it pending"
        );
        return Ok(());
    }
    // Only now: the vector is in the index, so a neighbour query has something
    // to find. Armed rather than run inline — a Qdrant query that fails must not
    // fail the embed job, whose retry would pay for the embedding again.
    //
    // Which is why neither of these propagates. Both are follow-up work over an
    // artifact that is already indexed, and returning their errors from here
    // would have failed the embed job for exactly the reason the arming exists
    // to avoid — buying the same embedding twice to retry a step that costs
    // nothing. Both have a backstop: the sweep re-finds an unarmed artifact's
    // pairs through `near_pairs`, and an unfinished merge through
    // `merged_with_active_roots`.
    //
    // This is what makes duplicate detection complete rather than sampled; see
    // the module header of `jobs::relate`.
    if let Err(e) = crate::jobs::relate::arm(core, &chunk.id, 0).await {
        tracing::warn!(
            artifact_id = %chunk.id,
            error = %e,
            "could not arm the neighbour query; the sweep will find its pairs"
        );
    }
    // A merged artifact hides what it replaced only once it is itself in the
    // index, so the knowledge is never out of search on both sides at once.
    if chunk.provenance == crate::store::artifacts::Provenance::Merged
        && let Err(e) = crate::jobs::merge::finish(core, &chunk.id).await
    {
        tracing::warn!(
            merged = %chunk.id,
            error = %e,
            "could not finish the merge; the next sweep will"
        );
    }
    Ok(())
}

/// Fall back from one job per source to one job per chunk. A batch that has
/// exhausted its retries may be failing on a single chunk the embedder rejects,
/// and one bad chunk must not keep its siblings out of search.
pub async fn split_into_artifact_jobs(core: &Core, corpus_id: &str) -> Result<()> {
    let pending = core.store.pending_artifacts_for_corpus(corpus_id).await?;
    for c in &pending {
        // Idle-only, like every other automatic arming. Two workers reach this
        // for the same corpus whenever one batch meets a refused chunk while
        // another worker is already inside the per-chunk unit an earlier batch
        // armed — and a `Guard::Any` upsert would put that `running` row back to
        // `pending` under it. A third claim then runs `split_oversize` on the
        // same chunk beside the first, and one parent ends up with two full sets
        // of siblings.
        core.store
            .rearm_idle_seq(Stage::Embed, "artifact", &c.id, 0)
            .await?;
    }
    tracing::info!(
        corpus_id,
        chunks = pending.len(),
        "split batch into per-chunk embed jobs"
    );
    Ok(())
}

/// A chunk larger than the embedder accepts becomes several sibling chunks
/// split at a paragraph boundary. Truncating would silently discard knowledge,
/// and one vector per fragment keeps the data model unchanged.
async fn split_oversize(core: &Core, chunk: &Chunk, limit: usize, hard: bool) -> Result<()> {
    // The limit is checked against what actually gets embedded, and that is the
    // title followed by the text. Siblings inherit the title, so only what the
    // title leaves over is available to their text. Giving the text the whole
    // limit produced siblings that measured oversize again the instant they
    // were re-queued, each one replaced by another exactly like it, forever.
    let title_cost = chunk
        .title
        .as_deref()
        .map_or(0, |t| core.counter.count(&format!("{t}\n")));
    let budget = limit.saturating_sub(title_cost);

    // A title that fills the limit on its own cannot be cut out of the way:
    // every sibling would carry it too, so no split of the text can help.
    if budget > 0 {
        let parts = split_by_paragraphs(&chunk.text, budget, &core.counter);
        if parts.len() > 1 {
            return replace_with_siblings(core, chunk, parts).await;
        }
        // No blank line to cut on. Code, tables and reference entries look like
        // this, and they are exactly what a local embedding server refuses for
        // exceeding its physical batch — so when the refusal is real, cut on
        // lines and then on characters rather than trying the same thing again.
        if hard {
            let parts = split_by_lines(&chunk.text, budget, &core.counter);
            if parts.len() > 1 {
                return replace_with_siblings(core, chunk, parts).await;
            }
        }
    }

    // Optimism, once: our token estimate may simply be wrong, and one
    // over-long vector beats shredding a chunk on a guess. But if the
    // server refuses it, the guess is settled and the text has to be cut.
    tracing::warn!(
        artifact_id = %chunk.id,
        title_cost,
        "oversize chunk has nothing left to cut on; embedding as-is"
    );
    let permit = core.gate.background().await;
    let vectors = match core.embedder.embed(std::slice::from_ref(&chunk.text)).await {
        Ok(v) => {
            permit.succeeded();
            v
        }
        // Refused, not failed: the endpoint answered. This arm no longer tests
        // `budget`, because a refusal we cannot act on is still a refusal —
        // reporting it as a sick endpoint was how a single untouchable chunk
        // could hold the breaker open on every retry of it.
        Err(e) if input_too_large(&e) => {
            permit.refused();
            // Still nothing to cut with when the title alone fills the limit:
            // `split_by_lines` at a budget of zero puts every line in a part of
            // its own and then falls to the 64-character floor, which shreds the
            // text into fragments that are each still oversize once they inherit
            // the title. A refusal we cannot act on is reported as one.
            if budget > 0 {
                let parts = split_by_lines(&chunk.text, budget, &core.counter);
                if parts.len() > 1 {
                    tracing::warn!(artifact_id = %chunk.id, parts = parts.len(), "endpoint refused it whole; cutting on lines");
                    return replace_with_siblings(core, chunk, parts).await;
                }
            }
            return Err(e);
        }
        Err(e) => {
            permit.failed(&e);
            return Err(e);
        }
    };
    core.vectors
        .upsert(vec![VectorPoint {
            vector: vectors.into_iter().next().unwrap(),
            sparse: crate::vector::sparse::encode_document(&chunk.text),
            payload: payload_of(chunk),
        }])
        .await?;
    mark_indexed(core, chunk).await?;
    match &chunk.corpus_id {
        Some(corpus_id) => settle_corpus(core, corpus_id).await,
        // A merged artifact belongs to no corpus, so there is no document whose
        // coverage this embedding advances and nothing to settle.
        None => Ok(()),
    }
}

/// Cut text on blank lines, packing as many paragraphs into each part as the
/// budget allows. Returns one part when there is no boundary that helps.
fn split_by_paragraphs(
    text: &str,
    limit: usize,
    counter: &crate::infer::budget::TokenCounter,
) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    for p in text.split("\n\n").filter(|p| !p.trim().is_empty()) {
        let candidate = if current.is_empty() {
            p.to_string()
        } else {
            format!("{current}\n\n{p}")
        };
        if counter.count(&candidate) > limit && !current.is_empty() {
            parts.push(std::mem::take(&mut current));
            current = p.to_string();
        } else {
            current = candidate;
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

/// Cut text that has no paragraph breaks. Lines first, and a single line that
/// still will not fit is cut on character count — the point is that this always
/// returns something smaller than it was given.
fn split_by_lines(
    text: &str,
    limit: usize,
    counter: &crate::infer::budget::TokenCounter,
) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        let candidate = if current.is_empty() {
            line.to_string()
        } else {
            format!("{current}\n{line}")
        };
        if counter.count(&candidate) > limit && !current.is_empty() {
            parts.push(std::mem::take(&mut current));
            current = line.to_string();
        } else {
            current = candidate;
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }

    // One line longer than the limit on its own: a minified blob or a very long
    // command. Characters are the last resort, and four per token is the same
    // conservative ratio the budget estimate uses.
    let max_chars = limit.saturating_mul(4).max(64);
    parts
        .into_iter()
        .flat_map(|p| {
            if counter.count(&p) <= limit {
                return vec![p];
            }
            p.chars()
                .collect::<Vec<_>>()
                .chunks(max_chars)
                .map(|c| c.iter().collect::<String>())
                .collect::<Vec<_>>()
        })
        .collect()
}

async fn replace_with_siblings(core: &Core, chunk: &Chunk, parts: Vec<String>) -> Result<()> {
    // A single part is the parent again under a new id: the replacement is
    // re-queued, measured the same way, and replaced again — a loop that
    // burns a core and grows the table until someone notices. No caller can
    // recover from it, so it is refused here rather than trusted upstream.
    if parts.len() < 2 {
        return Err(Error::Validation(format!(
            "refusing to replace artifact {} with {} sibling(s)",
            chunk.id,
            parts.len()
        )));
    }

    // Splitting means writing siblings into the parent's document, at ordinals
    // around its own. A merged artifact has neither: no corpus to insert into
    // and no reading order to preserve, and its siblings would lose the lineage
    // that says what it was made of. A merge that will not embed is a bug in
    // the merge — the draft is too long, or the model ran away — and it belongs
    // on Ops rather than being quietly chopped into fragments nothing can trace.
    let Some(corpus_id) = chunk.corpus_id.as_deref() else {
        return Err(Error::Validation(format!(
            "refusing to split merged artifact {}: it belongs to no corpus",
            chunk.id
        )));
    };

    tracing::info!(artifact_id = %chunk.id, parts = parts.len(), "split oversize chunk into siblings");

    let base = chunk.ordinal;
    // Make the siblings' room before numbering them. Numbering them apart from
    // their neighbours instead — `base * 1000 + i` — sorts chunk 2's siblings
    // after chunks 3 onward rather than before them, and the next segmentation
    // pass renumbers that wrong order into place permanently.
    core.store
        .make_room_after(corpus_id, base, parts.len() as i64 - 1)
        .await?;
    let new: Vec<NewArtifact> = parts
        .iter()
        .enumerate()
        .map(|(i, text)| NewArtifact {
            // Siblings sort after the original position and before the next
            // original chunk, which keeps reading order intact.
            ordinal: base + i as i64,
            text: text.clone(),
            corpus_span: chunk.corpus_span.clone(),
            title: chunk.title.clone(),
            category: chunk.category.clone(),
            // A caveat applies to the whole passage the parent held, so every
            // fragment of it inherits the warning rather than losing it.
            caveats: chunk.caveats.clone(),
            tags: chunk.tags.clone(),
            // Siblings belong to the window their parent came from, or a
            // re-segmentation of that window would leave them behind.
            segment_idx: chunk.segment_idx,
        })
        .collect();

    let inserted = core.store.insert_artifacts(corpus_id, &new).await?;
    core.store.delete_artifact(&chunk.id).await?;
    core.vectors
        .delete_artifacts(std::slice::from_ref(&chunk.id))
        .await?;
    // The parent is gone, so anything it was hiding is now hidden in favour of
    // an artifact that does not exist. The siblings are not a substitute: they
    // are new ids nothing points at.
    core.heal_dangling_supersessions().await?;

    for c in &inserted {
        core.store.enqueue(Stage::Embed, "artifact", &c.id).await?;
    }
    Ok(())
}

fn payload_of(chunk: &Chunk) -> VectorPayload {
    VectorPayload {
        artifact_id: chunk.id.clone(),
        // A merged artifact belongs to no corpus and carries the empty string
        // here. `provenance` below is what tells the two apart — a corpus
        // filter genuinely should not match an artifact that belongs to none,
        // and `restore_artifact` reads the kind rather than guessing what an
        // empty id meant.
        corpus_id: chunk.corpus_id.clone().unwrap_or_default(),
        provenance: Some(chunk.provenance.as_str().to_string()),
        text: chunk.text.clone(),
        title: chunk.title.clone(),
        category: chunk.category.clone(),
        tags: chunk.tags.clone(),
        created_at: chunk.created_at,
        // Unset means "whatever is already stored": the vector store carries
        // the existing stamp forward rather than letting a re-embed make a
        // chunk look forgotten.
        last_seen_at: None,
        hit_count: None,
        // Retired state is written; active state is deferred. The asymmetry is
        // the point.
        //
        // Deferring unconditionally loses the state outright on a *first*
        // embed: there is no stored value to carry forward, so an artifact
        // deprecated while its embed job was still pending — the detail page
        // offers the button whatever the embed state — lands as a point with no
        // `status` at all and is back in ordinary results. Nothing notices until
        // the next sweep's `repair_lifecycle_drift`, and nothing ever does if
        // consolidation is disabled.
        //
        // Writing unconditionally has the opposite failure: `false`/`active` on
        // every re-embed would revive an artifact the sweep hid, because the row
        // is read here before the embedding call and an operator can retire the
        // artifact while that call is in flight.
        //
        // Writing only the retired values takes neither. SQLite is the source of
        // truth (`set_superseded_by` maintains `status` alongside
        // `superseded_by`), so a row that says retired is a fact worth writing;
        // a row that says active cannot distinguish "still active" from "stale
        // read", so it defers to the stored value as before.
        superseded: (chunk.superseded_by.is_some()).then_some(true),
        status: (chunk.status != ArtifactStatus::Active).then_some(chunk.status),
        // Written, not deferred: a brand-new point has no stored stamp to carry
        // forward, and the scoring formula reads a missing `last_verified_at`
        // as epoch — so leaving it unset would rank every freshly ingested
        // artifact as maximally stale and put it straight on the
        // deprecation-review list. SQLite is the source of truth here (set at
        // insert, updated by `Core::verify`), so this is the current value.
        // The `created_at` fallback covers any row written before the column
        // existed.
        last_verified_at: chunk.last_verified_at.or(Some(chunk.created_at)),
        // Same rule as `status` directly above, and it has to be the same rule:
        // a point that says superseded while naming no winner is a hidden
        // artifact whose replacement the UI cannot show.
        superseded_by: chunk.superseded_by.clone(),
    }
}

/// Advance the parent source once no chunk is still pending: `ready` if every
/// chunk embedded, `partial` if any gave up.
pub async fn settle_corpus(core: &Core, corpus_id: &str) -> Result<()> {
    if core.store.pending_embed_count(corpus_id).await? > 0 {
        return Ok(());
    }
    let status = if core.store.failed_embed_count(corpus_id).await? > 0 {
        CorpusStatus::Partial
    } else if core.store.get_corpus(corpus_id).await?.status == CorpusStatus::Partial {
        // A source with a window the model refused is already partial. Its
        // chunks embedding cleanly does not fill the hole those lines left,
        // and reporting `ready` would hide it.
        CorpusStatus::Partial
    } else {
        CorpusStatus::Ready
    };
    core.store.set_corpus_status(corpus_id, status).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A corpus with `n` chunks, none of them embedded yet.
    async fn corpus_with_pending_chunks(core: &Core, n: usize) -> (String, Vec<String>) {
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let new: Vec<_> = (0..n)
            .map(|i| NewArtifact {
                ordinal: i as i64,
                text: format!("chunk {i}"),
                corpus_span: None,
                title: None,
                category: None,
                tags: vec![],
                segment_idx: Some(i as i64),
                caveats: vec![],
            })
            .collect();
        let made = core.store.insert_artifacts(&src.id, &new).await.unwrap();
        (src.id, made.iter().map(|c| c.id.clone()).collect())
    }

    async fn job_state(core: &Core, stage: Stage, target: &str) -> Option<String> {
        sqlx::query_scalar::<_, String>("SELECT state FROM jobs WHERE stage = ? AND target_id = ?")
            .bind(stage.as_str())
            .bind(target)
            .fetch_optional(&core.store.pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn isolating_a_batch_leaves_a_chunk_already_being_split_alone() {
        // Two workers reach this for one corpus: A is inside `split_oversize`
        // for a chunk an earlier batch isolated, while B's batch meets a
        // different refused chunk and isolates everything pending. Arming
        // whatever state it found flipped A's row back to `pending`, a third
        // claim ran `split_oversize` beside A, and one parent chunk ended up
        // with two full sets of siblings.
        let core = crate::core::test_support::test_core().await;
        let (src, chunks) = corpus_with_pending_chunks(&core, 2).await;

        core.store
            .enqueue(Stage::Embed, "artifact", &chunks[0])
            .await
            .unwrap();
        let claimed = core.store.claim_job().await.unwrap().unwrap();
        assert_eq!(claimed.target_id, chunks[0]);

        split_into_artifact_jobs(&core, &src).await.unwrap();

        assert_eq!(
            job_state(&core, Stage::Embed, &chunks[0]).await.as_deref(),
            Some("running"),
            "isolating the batch reset a chunk a worker was already splitting"
        );
        // Its sibling, which nothing was inside, is armed as it should be.
        assert_eq!(
            job_state(&core, Stage::Embed, &chunks[1]).await.as_deref(),
            Some("pending")
        );
    }

    #[tokio::test]
    async fn coming_back_for_another_batch_does_not_disturb_a_running_row() {
        // `run_one` calls this after `complete_job`, and in that gap another
        // worker's `settle` can reach `finish`, whose idle-only arming
        // legitimately reopens the row — and a second worker can claim it.
        // Arming whatever state it found then put two workers into `run_corpus`
        // for one corpus, and whichever finished second closed the row the other
        // had re-armed, leaving the source half-embedded.
        let core = crate::core::test_support::test_core().await;
        let (src, _) = corpus_with_pending_chunks(&core, 2).await;

        core.store
            .enqueue(Stage::Embed, "corpus", &src)
            .await
            .unwrap();
        core.store.claim_job().await.unwrap().unwrap();

        rearm_if_more(&core, &src).await.unwrap();
        assert_eq!(
            job_state(&core, Stage::Embed, &src).await.as_deref(),
            Some("running"),
            "a second worker was let into run_corpus for a corpus already being embedded"
        );

        // The path this exists for is untouched: a row closed with chunks still
        // pending comes back for the next batch.
        sqlx::query("UPDATE jobs SET state = 'done' WHERE stage = 'embed' AND target_id = ?")
            .bind(&src)
            .execute(&core.store.pool)
            .await
            .unwrap();
        rearm_if_more(&core, &src).await.unwrap();
        assert_eq!(
            job_state(&core, Stage::Embed, &src).await.as_deref(),
            Some("pending"),
            "a corpus with chunks left stopped half-embedded"
        );
    }

    #[tokio::test]
    async fn an_artifact_retired_before_its_first_embed_lands_retired() {
        // The detail page offers Deprecate whatever the embed state, so this is
        // reachable by pressing it on a freshly captured artifact. The payload
        // deferred `status` to whatever was already stored — and on a first
        // embed nothing is, so the point arrived with no status at all and the
        // deprecated artifact was back in ordinary results. Nothing noticed
        // until the next sweep, and nothing ever did with consolidation off.
        let core = crate::core::test_support::test_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    ordinal: 0,
                    text: "a stale instruction".into(),
                    corpus_span: None,
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: Some(0),
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();
        core.deprecate(&made[0].id).await.unwrap();

        run(&core, &made[0].id).await.unwrap();

        let stored = core
            .vectors
            .payloads_of(&[made[0].id.clone()])
            .await
            .unwrap();
        assert_eq!(
            stored[&made[0].id].status,
            Some(crate::store::artifacts::ArtifactStatus::Deprecated),
            "the first embed put a deprecated artifact back into results"
        );
    }

    #[tokio::test]
    async fn a_chunk_the_endpoint_refuses_is_split_rather_than_retried() {
        // The deployment that produced this: config claimed 8192 input tokens,
        // llama.cpp's physical batch was 1024, and the chunk in between failed
        // five identical times before anyone looked.
        let mut core = crate::core::test_support::test_core().await;
        let strict = std::sync::Arc::new(crate::infer::fake::StrictEmbedder::new(
            crate::core::test_support::TEST_DIM,
            200,
        ));
        core.embedder = strict.clone();

        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        // Several paragraphs, comfortably over the endpoint's real ceiling.
        let body = (0..40)
            .map(|i| format!("paragraph {i} with a good deal of filler text in it"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: body,
                    corpus_span: None,
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: Some(0),
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();

        // The configured limit is the lie; the endpoint's is what bites.
        run_with_limit(&core, &made[0].id, 8192).await.unwrap();

        let chunks = core.store.artifacts_for_corpus(&src.id).await.unwrap();
        assert!(
            chunks.len() > 1,
            "the refused chunk should have become siblings, got {}",
            chunks.len()
        );
        assert!(
            chunks.iter().all(|c| c.segment_idx == Some(0)),
            "siblings must stay attached to the window that produced them"
        );
    }

    #[tokio::test]
    async fn a_chunk_with_no_paragraph_breaks_is_still_split() {
        // Code, tables and reference entries have no blank lines, and they are
        // exactly what a local embedding server refuses for exceeding its
        // physical batch. Giving up on them meant retrying forever.
        let mut core = crate::core::test_support::test_core().await;
        core.embedder = std::sync::Arc::new(crate::infer::fake::StrictEmbedder::new(
            crate::core::test_support::TEST_DIM,
            120,
        ));

        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let body = (0..60)
            .map(|i| format!("    command --flag-{i} /path/to/thing-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !body.contains("\n\n"),
            "the point is that there are no paragraphs"
        );
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: body,
                    corpus_span: None,
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: Some(0),
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();

        run_with_limit(&core, &made[0].id, 8192).await.unwrap();

        let chunks = core.store.artifacts_for_corpus(&src.id).await.unwrap();
        assert!(chunks.len() > 1, "got {} chunks", chunks.len());
    }

    #[tokio::test]
    async fn a_chunk_only_its_title_pushes_over_the_limit_does_not_respawn_itself() {
        // The loop this closes: the limit is checked against title + text, the
        // split only ever cut text, so a chunk whose text fits on its own was
        // "split" into one identical sibling — which was enqueued, measured
        // with its title again, and split into one identical sibling, forever.
        let core = crate::core::test_support::test_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();

        // 3.5 characters per estimated token: the two paragraphs fit under the
        // limit by themselves, and the title is what puts them over it.
        let title = "a rather long heading that costs real tokens".to_string();
        let text = format!("{}\n\n{}", "alpha ".repeat(10), "beta ".repeat(10));
        let limit = 40;
        assert!(core.counter.count(&text) <= limit, "text must fit alone");
        assert!(
            core.counter.count(&format!("{title}\n{text}")) > limit,
            "the title must be what pushes it over"
        );

        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    ordinal: 0,
                    text: text.clone(),
                    corpus_span: None,
                    title: Some(title),
                    category: None,
                    tags: vec![],
                    segment_idx: Some(0),
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();

        run_with_limit(&core, &made[0].id, limit).await.unwrap();

        let chunks = core.store.artifacts_for_corpus(&src.id).await.unwrap();
        assert!(
            !chunks.iter().any(|c| c.text == text && c.id != made[0].id),
            "the parent was replaced by an identical copy of itself"
        );
        // Every sibling has to fit with the title it inherited, or the next
        // pass splits it again and the loop is only slower.
        for c in &chunks {
            assert!(
                core.counter.count(&embed_text(c)) <= limit,
                "sibling is still oversize: {:?}",
                c.text
            );
        }
    }

    #[tokio::test]
    async fn a_refusal_with_no_budget_left_is_reported_rather_than_shredded() {
        // A title costing the whole limit leaves the text nothing, which is why
        // the paragraph and line splits are skipped. The as-is attempt then ran
        // the line split anyway, at a budget of zero: every line becomes a part
        // of its own and the character floor cuts the rest to 64 at a time, so
        // the chunk was replaced by dozens of fragments that are each still
        // oversize once they inherit the same title — and mean nothing on their
        // own. Failing is the honest answer; the operator can shorten a title.
        let mut core = crate::core::test_support::test_core().await;
        core.embedder = std::sync::Arc::new(crate::infer::fake::StrictEmbedder::new(
            crate::core::test_support::TEST_DIM,
            1,
        ));
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();

        let title = "a heading long enough to cost the entire limit by itself".to_string();
        let text = (0..12)
            .map(|i| format!("line {i} of something that has no blank lines in it"))
            .collect::<Vec<_>>()
            .join("\n");
        let limit = core.counter.count(&format!("{title}\n"));

        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    ordinal: 0,
                    text: text.clone(),
                    corpus_span: None,
                    title: Some(title),
                    category: None,
                    tags: vec![],
                    segment_idx: Some(0),
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();

        let err = run_with_limit(&core, &made[0].id, limit)
            .await
            .expect_err("a refusal nothing can act on has to surface");
        assert!(input_too_large(&err), "wrong error: {err}");

        let chunks = core.store.artifacts_for_corpus(&src.id).await.unwrap();
        assert_eq!(chunks.len(), 1, "the chunk was cut into meaningless pieces");
        assert_eq!(chunks[0].text, text);
    }

    #[tokio::test]
    async fn a_refusal_during_the_as_is_attempt_still_ends_in_a_split() {
        // The trap this closes: the estimate says the chunk is oversize, there
        // is no paragraph to cut on, so it is embedded whole — and the server
        // refuses it, forever, because nothing along that path can shrink it.
        let mut core = crate::core::test_support::test_core().await;
        core.embedder = std::sync::Arc::new(crate::infer::fake::StrictEmbedder::new(
            crate::core::test_support::TEST_DIM,
            120,
        ));

        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let body = (0..60)
            .map(|i| format!("    command --flag-{i} /path/to/thing-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: body,
                    corpus_span: None,
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: Some(0),
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();

        // A limit low enough that the proactive path runs, which is the path
        // that used to dead-end.
        run_with_limit(&core, &made[0].id, 100).await.unwrap();

        let chunks = core.store.artifacts_for_corpus(&src.id).await.unwrap();
        assert!(chunks.len() > 1, "got {} chunks", chunks.len());
    }

    #[tokio::test]
    async fn a_size_refusal_does_not_hold_the_rest_of_the_queue() {
        // Three unsplittable oversize chunks — code blocks, tables, the exact
        // thing `split_oversize`'s hard path exists for — used to be three
        // consecutive transport failures. At the default `breaker_after` that
        // opened the breaker and stopped synthesis, judging and embedding alike
        // for the whole probe window, against an endpoint that was answering
        // every request it was given.
        let mut core = crate::core::test_support::test_core().await;
        core.gate = std::sync::Arc::new(
            crate::infer::gate::InferenceGate::new(std::time::Duration::ZERO)
                .with_breaker(1, std::time::Duration::from_secs(60)),
        );
        core.embedder = std::sync::Arc::new(crate::infer::fake::StrictEmbedder::new(
            crate::core::test_support::TEST_DIM,
            200,
        ));

        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let body = (0..40)
            .map(|i| format!("paragraph {i} with a good deal of filler text in it"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    ordinal: 0,
                    text: body,
                    corpus_span: None,
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: Some(0),
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();

        // The configured limit is the lie; the endpoint's is what bites.
        run_with_limit(&core, &made[0].id, 8192).await.unwrap();

        // Asked in real time rather than on a paused clock: building a `Core`
        // opens a pool, and a pool acquired under `start_paused` waits on a
        // timer that never fires. An open breaker would hold this for the whole
        // probe window, so a short timeout tells the two apart.
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                core.gate.background()
            )
            .await
            .is_ok(),
            "a chunk the endpoint refused opened the breaker"
        );
    }

    #[test]
    fn splitting_by_lines_always_returns_something_smaller() {
        let counter = crate::infer::budget::TokenCounter::Estimate;
        // A single line far over the limit, with no whitespace to cut on.
        let blob = "x".repeat(4000);
        let parts = split_by_lines(&blob, 100, &counter);
        assert!(parts.len() > 1, "a long single line must still be cut");
        assert!(
            parts.iter().all(|p| counter.count(p) <= 100 * 2),
            "each part must be near the limit rather than the original size"
        );
        assert_eq!(parts.concat(), blob, "cutting must not lose text");
    }

    #[test]
    fn an_endpoint_size_refusal_is_told_apart_from_a_sick_endpoint() {
        let too_big = Error::Inference {
            role: "embed",
            detail: "input (1030 tokens) is too large to process. increase the physical \
                     batch size (current batch size: 1024)"
                .into(),
        };
        assert!(input_too_large(&too_big));

        // A transient failure must stay retryable, or a flaky local server
        // would start shredding chunks instead of waiting for it to recover.
        let flaky = Error::Inference {
            role: "embed",
            detail: "error sending request".into(),
        };
        assert!(!input_too_large(&flaky));
        let wrong_role = Error::Inference {
            role: "chunk",
            detail: "context too large".into(),
        };
        assert!(!input_too_large(&wrong_role));
    }
    use crate::core::test_support::test_core;
    use crate::store::artifacts::{EmbedState, NewArtifact};
    use crate::store::corpora::CorpusStatus;

    /// A corpus with `n` chunks already written and none embedded.
    async fn corpus_with_chunks(core: &Core, n: usize) -> String {
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let new: Vec<NewArtifact> = (0..n)
            .map(|i| NewArtifact {
                ordinal: i as i64,
                text: format!("chunk number {i}"),
                corpus_span: None,
                title: Some(format!("t{i}")),
                category: None,
                tags: vec![],
                caveats: vec![],
                segment_idx: Some(0),
            })
            .collect();
        core.store.insert_artifacts(&src.id, &new).await.unwrap();
        src.id
    }

    #[tokio::test]
    async fn one_run_embeds_at_most_one_batch() {
        // Nine calls in one job is nine calls in front of everything else the
        // queue is holding, with no chance for a question to slip between them.
        let core = test_core().await;
        let id = corpus_with_chunks(&core, BATCH + 10).await;

        let before = core
            .store
            .pending_artifacts_for_corpus(&id)
            .await
            .unwrap()
            .len();
        run_corpus(&core, &id).await.unwrap();
        let after = core
            .store
            .pending_artifacts_for_corpus(&id)
            .await
            .unwrap()
            .len();

        assert_eq!(before - after, BATCH, "a run embedded more than one batch");
    }

    #[tokio::test]
    async fn the_per_chunk_path_is_paced_like_every_other_call() {
        // `split_into_artifact_jobs` can leave hundreds of these behind for one
        // document. Ungated they ran back to back at the endpoint, and against a
        // dead one they each spent a timeout without a single failure ever
        // reaching the breaker that exists to stop exactly that.
        let core = test_core().await;
        let (_src, ids) = seed(&core, &["one"]).await;

        // Holding the turn is the question asked directly: a path that goes
        // through the gate cannot start while another call has it.
        let permit = core.gate.background().await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), run(&core, &ids[0]))
                .await
                .is_err(),
            "the per-chunk embed path went straight to the endpoint"
        );

        permit.succeeded();
        run(&core, &ids[0]).await.unwrap();
        assert_eq!(
            core.store.get_artifact(&ids[0]).await.unwrap().embed_state,
            EmbedState::Embedded
        );
    }

    #[tokio::test]
    async fn isolating_a_batch_does_not_put_the_batch_straight_back() {
        // The batch meets a chunk the endpoint refuses, gives way to one unit
        // per chunk, and returns `Ok` — so `run_one` completes it and re-arms
        // it, beside the units that just replaced it. It converges, because
        // those carry `seq = 0` and win the ordering, but every round costs
        // another call that can only fail the same way.
        let mut core = test_core().await;
        core.embedder = std::sync::Arc::new(crate::infer::fake::StrictEmbedder::new(
            crate::core::test_support::TEST_DIM,
            20,
        ));
        let big = "alpha ".repeat(200);
        let (src_id, _) = seed(&core, &["small", &big]).await;
        core.store
            .enqueue(Stage::Embed, "corpus", &src_id)
            .await
            .unwrap();

        // One claim: the re-arm happens in `run_one` after the job is closed.
        crate::jobs::run_one(&core).await.unwrap();

        let batch_armed: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM jobs
              WHERE stage = 'embed' AND target_kind = 'corpus' AND state = 'pending'",
        )
        .fetch_one(&core.store.pool)
        .await
        .unwrap();
        assert_eq!(
            batch_armed, 0,
            "the batch was re-armed beside the per-chunk units that replaced it"
        );
    }

    #[tokio::test]
    async fn oversize_chunks_do_not_turn_one_batch_into_many_calls() {
        // `split_oversize` can cost a model call of its own — its fall-through
        // embeds the chunk whole to find out whether our estimate was wrong —
        // and the scan ran it once per oversize chunk. Fifty of them was fifty
        // sequential calls inside a job that is allowed exactly one, holding the
        // turn for fifty cooldowns before anything else could run.
        let (core, embedder) = crate::core::test_support::test_core_counting_embed_calls().await;
        let big = "alpha ".repeat(400);
        let (src_id, _) = seed(&core, &[&big, &big, &big, "small"]).await;

        run_corpus_with_limit(&core, &src_id, 200).await.unwrap();

        assert_eq!(
            embedder.calls(),
            1,
            "the batch job made more than one inference call"
        );
        let armed: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM jobs
              WHERE stage = 'embed' AND target_kind = 'artifact' AND state = 'pending'",
        )
        .fetch_one(&core.store.pool)
        .await
        .unwrap();
        assert_eq!(armed, 3, "each oversize chunk should have got its own unit");
    }

    #[tokio::test]
    async fn a_long_document_re_arms_itself_until_it_is_drained() {
        let core = test_core().await;
        let chunks = BATCH * 2 + 5;
        let id = corpus_with_chunks(&core, chunks).await;
        core.store
            .enqueue(Stage::Embed, "corpus", &id)
            .await
            .unwrap();

        // The termination guard is what this test exists for: the batch job
        // re-arms itself, and a re-arm that does not stop is an infinite queue
        // rather than a slow one.
        //
        // The exact total is what pins it, and it is now two things rather than
        // one: three batch claims, plus one `relate` unit per artifact that
        // reached the index. Either number drifting shows up here — an extra
        // batch claim means the re-arm ran long, and a missing relate unit means
        // an artifact was indexed without ever being checked for duplicates,
        // which is the silent half.
        let mut claims = 0;
        while crate::jobs::run_one(&core).await.unwrap() {
            claims += 1;
            assert!(
                claims <= 3 + chunks,
                "the re-arm never terminated: {claims} claims"
            );
        }

        assert!(
            core.store
                .pending_artifacts_for_corpus(&id)
                .await
                .unwrap()
                .is_empty(),
            "the re-arm did not drain the corpus"
        );
        assert_eq!(
            claims,
            3 + chunks,
            "expected one claim per batch, plus one relate unit per artifact"
        );
    }

    #[tokio::test]
    async fn a_re_armed_batch_sinks_below_a_fresher_document() {
        // The point of climbing `seq`: batch two of a long document must not
        // outrank batch one of a document captured since.
        let core = test_core().await;
        let long = corpus_with_chunks(&core, BATCH * 2).await;
        core.store
            .enqueue(Stage::Embed, "corpus", &long)
            .await
            .unwrap();
        // One claim: the re-arm happens in `run_one` after the job is closed,
        // so calling the handler directly would show nothing.
        crate::jobs::run_one(&core).await.unwrap();

        let seq = core
            .store
            .job_seq(Stage::Embed, &long)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(seq, 1, "the re-armed batch stayed at the front");
    }

    async fn seed(core: &crate::core::Core, texts: &[&str]) -> (String, Vec<String>) {
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let new: Vec<NewArtifact> = texts
            .iter()
            .enumerate()
            .map(|(i, t)| NewArtifact {
                ordinal: i as i64,
                text: t.to_string(),
                corpus_span: None,
                title: Some(format!("t{i}")),
                category: Some("note".into()),
                tags: vec!["x".into()],
                segment_idx: None,
                caveats: vec![],
            })
            .collect();
        let made = core.store.insert_artifacts(&src.id, &new).await.unwrap();
        core.store
            .set_corpus_status(&src.id, CorpusStatus::Embedding)
            .await
            .unwrap();
        (src.id, made.into_iter().map(|c| c.id).collect())
    }

    #[tokio::test]
    async fn embeds_a_chunk_and_writes_a_searchable_point() {
        let core = test_core().await;
        let (src_id, ids) = seed(&core, &["## A\nthe body"]).await;

        run(&core, &ids[0]).await.unwrap();

        let c = core.store.get_artifact(&ids[0]).await.unwrap();
        assert_eq!(c.embed_state, EmbedState::Embedded);
        assert_eq!(c.embed_model.as_deref(), Some("fake-embed"));
        assert_eq!(core.vectors.count().await.unwrap(), 1);

        // The payload must carry enough to render a result without touching SQLite.
        let q = core
            .embedder
            .embed(&["t0\n## A\nthe body".to_string()])
            .await
            .unwrap();
        let hits = core
            .vectors
            .search(&q[0], &Default::default(), 5, &Default::default())
            .await
            .unwrap();
        assert_eq!(hits[0].payload.corpus_id, src_id);
        assert_eq!(hits[0].payload.text, "## A\nthe body");
        assert_eq!(hits[0].payload.tags, vec!["x".to_string()]);
    }

    #[tokio::test]
    async fn source_becomes_ready_only_after_the_last_chunk() {
        let core = test_core().await;
        let (src_id, ids) = seed(&core, &["one", "two"]).await;

        run(&core, &ids[0]).await.unwrap();
        assert_eq!(
            core.store.get_corpus(&src_id).await.unwrap().status,
            CorpusStatus::Embedding
        );

        run(&core, &ids[1]).await.unwrap();
        assert_eq!(
            core.store.get_corpus(&src_id).await.unwrap().status,
            CorpusStatus::Ready
        );
    }

    #[tokio::test]
    async fn a_failed_chunk_leaves_the_source_partial() {
        let core = test_core().await;
        let (src_id, ids) = seed(&core, &["one", "two"]).await;
        run(&core, &ids[0]).await.unwrap();
        core.store.mark_embed_failed(&ids[1]).await.unwrap();
        settle_corpus(&core, &src_id).await.unwrap();
        assert_eq!(
            core.store.get_corpus(&src_id).await.unwrap().status,
            CorpusStatus::Partial
        );
    }

    #[tokio::test]
    async fn a_whole_source_is_embedded_in_one_inference_call() {
        // The embedding endpoint is the slow, rate-limited part of ingest.
        // Five chunks must cost one call, not five.
        let (core, embedder) = crate::core::test_support::test_core_counting_embed_calls().await;
        let (src_id, ids) = seed(&core, &["one", "two", "three", "four", "five"]).await;

        run_corpus(&core, &src_id).await.unwrap();

        assert_eq!(embedder.calls(), 1, "chunks were embedded one at a time");
        assert_eq!(core.vectors.count().await.unwrap(), 5);
        for id in &ids {
            assert_eq!(
                core.store.get_artifact(id).await.unwrap().embed_state,
                EmbedState::Embedded
            );
        }
        assert_eq!(
            core.store.get_corpus(&src_id).await.unwrap().status,
            CorpusStatus::Ready
        );
    }

    #[tokio::test]
    async fn a_batch_larger_than_the_request_limit_is_split_across_calls() {
        // Endpoints cap how many inputs they accept, so the batch is bounded.
        // The split is across units now rather than within one job, so the
        // corpus is driven through the queue to see both calls.
        let (core, embedder) = crate::core::test_support::test_core_counting_embed_calls().await;
        let texts: Vec<String> = (0..BATCH + 5).map(|i| format!("chunk {i}")).collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let (src_id, _) = seed(&core, &refs).await;

        core.store
            .enqueue(Stage::Embed, "corpus", &src_id)
            .await
            .unwrap();
        while crate::jobs::run_one(&core).await.unwrap() {}

        assert_eq!(embedder.calls(), 2, "the batch was not bounded");
        assert_eq!(core.vectors.count().await.unwrap(), (BATCH + 5) as u64);
    }

    #[tokio::test]
    async fn a_source_with_nothing_pending_still_settles() {
        // Re-running a finished job must not leave the source stuck in
        // `embedding` forever.
        let core = test_core().await;
        let (src_id, _) = seed(&core, &["one"]).await;
        run_corpus(&core, &src_id).await.unwrap();
        run_corpus(&core, &src_id).await.unwrap();
        assert_eq!(
            core.store.get_corpus(&src_id).await.unwrap().status,
            CorpusStatus::Ready
        );
    }

    #[tokio::test]
    async fn an_oversize_chunk_does_not_block_its_siblings() {
        // It becomes siblings rather than a vector, so it cannot ride along in
        // the batch. The rest of the source must still be embedded, and the
        // oversize one must be handed to a unit of its own rather than split
        // here — splitting can cost a call this job has already spent.
        let core = test_core().await;
        let big = format!("{}\n\n{}", "alpha ".repeat(400), "beta ".repeat(400));
        let (src_id, ids) = seed(&core, &["small one", &big, "small two"]).await;

        run_corpus_with_limit(&core, &src_id, 200).await.unwrap();

        assert_eq!(
            core.vectors.count().await.unwrap(),
            2,
            "the two small chunks should be embedded"
        );
        let armed: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM jobs
              WHERE stage = 'embed' AND target_kind = 'artifact' AND target_id = ?
                AND state = 'pending'",
        )
        .bind(&ids[1])
        .fetch_one(&core.store.pool)
        .await
        .unwrap();
        assert_eq!(armed, 1, "the oversize chunk was not given its own unit");

        // And that unit does the split, so nothing is lost by deferring it.
        run_with_limit(&core, &ids[1], 200).await.unwrap();
        let chunks = core.store.artifacts_for_corpus(&src_id).await.unwrap();
        assert!(
            chunks.len() > 3,
            "the oversize chunk should have become siblings, got {}",
            chunks.len()
        );
    }

    #[tokio::test]
    async fn a_partially_segmented_source_is_not_promoted_to_ready() {
        // `partial` records that segmentation was degraded. Every chunk
        // embedding cleanly does not undo that, and reporting `ready` would
        // hide it.
        let core = test_core().await;
        let (src_id, _) = seed(&core, &["one", "two"]).await;
        core.store
            .set_corpus_status(&src_id, CorpusStatus::Partial)
            .await
            .unwrap();

        run_corpus(&core, &src_id).await.unwrap();

        assert_eq!(
            core.store.get_corpus(&src_id).await.unwrap().status,
            CorpusStatus::Partial
        );
    }

    #[tokio::test]
    async fn oversize_chunks_are_split_into_siblings_not_truncated() {
        let core = test_core().await;
        let big = format!("{}\n\n{}", "alpha ".repeat(400), "beta ".repeat(400));
        let (src_id, ids) = seed(&core, &[&big]).await;

        run_with_limit(&core, &ids[0], 200).await.unwrap();

        let chunks = core.store.artifacts_for_corpus(&src_id).await.unwrap();
        assert!(chunks.len() > 1, "oversize chunk must become siblings");
        let joined: String = chunks
            .iter()
            .map(|c| c.text.clone())
            .collect::<Vec<_>>()
            .join("");
        assert!(joined.contains("beta"), "no text may be dropped");
        assert!(joined.contains("alpha"));
    }

    #[tokio::test]
    async fn a_single_paragraph_oversize_chunk_is_still_embedded() {
        // No paragraph boundary to split on. Better one over-long vector than
        // a chunk that never becomes searchable at all.
        let core = test_core().await;
        let big = "alpha ".repeat(800);
        let (_src, ids) = seed(&core, &[&big]).await;
        run_with_limit(&core, &ids[0], 200).await.unwrap();
        assert_eq!(core.vectors.count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn a_chunk_edited_mid_embed_is_not_reported_as_indexed() {
        // The job read the chunk, called a slow endpoint, and is about to write
        // back "indexed". An edit that landed in that window must win, or the
        // vector describes text that no longer exists and nothing says so.
        let core = test_core().await;
        let (_src, ids) = seed(&core, &["one"]).await;
        let stale = core.store.get_artifact(&ids[0]).await.unwrap();

        core.store
            .update_artifact_text(&ids[0], "edited while embedding")
            .await
            .unwrap();

        // What the in-flight job would have done, with the revision it read.
        assert!(
            !core
                .store
                .mark_embedded(&stale.id, "fake-embed", stale.embed_rev)
                .await
                .unwrap(),
            "a stale job overwrote a newer edit"
        );
        assert_eq!(
            core.store.get_artifact(&ids[0]).await.unwrap().embed_state,
            EmbedState::Pending,
            "the chunk must stay queued for the text that is actually there"
        );

        // And the retry, reading the current row, does land.
        run(&core, &ids[0]).await.unwrap();
        assert_eq!(
            core.store.get_artifact(&ids[0]).await.unwrap().embed_state,
            EmbedState::Embedded
        );
    }

    #[tokio::test]
    async fn reprocessing_a_source_outlives_a_worker_already_embedding_it() {
        // `reset_embed_state` and an in-flight batch race by construction: both
        // write the same chunk, and only the revision says which is current.
        let core = test_core().await;
        let (src_id, ids) = seed(&core, &["one", "two"]).await;
        let inflight: Vec<_> = core
            .store
            .pending_artifacts_for_corpus(&src_id)
            .await
            .unwrap();

        core.store.reset_embed_state(&src_id).await.unwrap();
        for c in &inflight {
            assert!(
                !core
                    .store
                    .mark_embedded(&c.id, "fake-embed", c.embed_rev)
                    .await
                    .unwrap()
            );
        }

        assert_eq!(
            core.store.pending_embed_count(&src_id).await.unwrap(),
            ids.len() as i64,
            "the reprocess was silently cancelled by the job it interrupted"
        );
    }

    #[tokio::test]
    async fn re_embedding_does_not_make_a_chunk_look_forgotten() {
        // A point write replaces the payload rather than merging it, so a
        // re-embed built from the chunk row used to clear `last_seen_at` — and
        // `resurface` would offer a chunk read yesterday as forgotten.
        let core = test_core().await;
        let (_src, ids) = seed(&core, &["text"]).await;
        run(&core, &ids[0]).await.unwrap();
        core.vectors
            .touch(&[crate::vector::Touch::shown(&ids[0])], 1_700_000_000)
            .await
            .unwrap();

        core.store
            .update_artifact_text(&ids[0], "edited text")
            .await
            .unwrap();
        run(&core, &ids[0]).await.unwrap();

        let forgotten = core
            .vectors
            .resurface(10, i64::MAX, 1_700_000_000)
            .await
            .unwrap();
        assert!(
            forgotten.is_empty(),
            "the re-embed dropped the stamp and the chunk now reads as unseen"
        );
    }

    #[tokio::test]
    async fn split_siblings_keep_the_reading_order_of_the_chunk_they_replace() {
        // `base * 1000 + i` put chunk 1's siblings after chunks 2 onward, and
        // the next segmentation pass renumbered that order into place for good.
        let core = test_core().await;
        let big = format!("{}\n\n{}", "alpha ".repeat(400), "beta ".repeat(400));
        let (src_id, ids) = seed(&core, &["first", &big, "last"]).await;

        run_with_limit(&core, &ids[1], 200).await.unwrap();

        let texts: Vec<String> = core
            .store
            .artifacts_for_corpus(&src_id)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.text)
            .collect();
        assert_eq!(texts.first().map(String::as_str), Some("first"));
        assert_eq!(
            texts.last().map(String::as_str),
            Some("last"),
            "the siblings sorted past the chunk that follows them: {texts:?}"
        );
    }

    #[tokio::test]
    async fn re_embedding_replaces_the_point_rather_than_adding_one() {
        let core = test_core().await;
        let (_src, ids) = seed(&core, &["text"]).await;
        run(&core, &ids[0]).await.unwrap();
        core.store
            .update_artifact_text(&ids[0], "edited text")
            .await
            .unwrap();
        run(&core, &ids[0]).await.unwrap();
        assert_eq!(core.vectors.count().await.unwrap(), 1);
    }
}
