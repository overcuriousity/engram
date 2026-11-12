use crate::core::Core;
use crate::error::{Error, Result};
use crate::infer::EmbedDoc;
use crate::store::artifacts::{ArtifactStatus, Chunk, NewArtifact};
use crate::store::corpora::CorpusStatus;
use crate::store::jobs::Stage;
use crate::vector::{VectorPayload, VectorPoint};

/// Fraction of the embedder's hard limit a chunk may occupy; the rest absorbs tokenizer estimation
/// error.
const SAFETY: f32 = 0.8;

/// Endpoints cap how many inputs one request accepts.
const BATCH: usize = 32;

fn doc_of(chunk: &Chunk) -> EmbedDoc {
    EmbedDoc {
        title: chunk.title.clone(),
        text: chunk.text.clone(),
    }
}

/// Every budget in this file measures this string, so the splitter and the embedder agree on size.
fn render(core: &Core, chunk: &Chunk) -> String {
    core.embedder.render_document(&doc_of(chunk))
}

fn envelope_cost(core: &Core, title: Option<&str>) -> usize {
    core.counter
        .count(&core.embedder.render_document(&EmbedDoc {
            title: title.map(str::to_string),
            text: String::new(),
        }))
}

/// The words, not the template: `title:` and `text:` would match every query containing them.
fn lexical_text(chunk: &Chunk) -> String {
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

/// A server's physical batch limit (llama.cpp: `input (1030 tokens) is too large`) arrives in a
/// body or as a 413/400 rejection; both mean cut the chunk, not retry it.
fn input_too_large(e: &Error) -> bool {
    let detail = match e {
        Error::Inference {
            role: "embed",
            detail,
        }
        | Error::InferenceRejected {
            role: "embed",
            detail,
        } => detail,
        _ => return false,
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
    let text = render(core, &chunk);

    if core.counter.count(&text) > limit {
        // Our own estimate can be wrong either way; do not shred the chunk on a guess.
        return split_oversize(core, &chunk, limit, false).await;
    }

    // `background_light`: an encoder serves no cooldown, and one document can leave hundreds of
    // these.
    let permit = core.gate.background_light().await;
    let outcome = embed_batch(core, std::slice::from_ref(&chunk)).await;
    permit.finished();
    match outcome {
        Ok(()) => {}
        Err(e) if input_too_large(&e) => {
            // The server's ceiling is below the configured one, so halve what the chunk measures.
            let measured = core.counter.count(&text);
            let smaller = (measured / 2).max(crate::infer::budget::MIN_SEGMENT_TOKENS);
            tracing::warn!(
                artifact_id,
                measured,
                smaller,
                error = %e,
                "endpoint refused the chunk as too large; splitting instead of retrying"
            );
            // The server said no, so this split has to succeed whatever the text looks like.
            return split_oversize(core, &chunk, smaller, true).await;
        }
        Err(e) => return Err(e),
    }
    match &chunk.corpus_id {
        Some(corpus_id) => settle_corpus(core, corpus_id).await,
        // A merged artifact belongs to no corpus; nothing to settle.
        None => Ok(()),
    }
}

/// One call per source: the embedding endpoint is the slow, rate-limited, often paid part of
/// ingest.
pub async fn run_corpus(core: &Core, corpus_id: &str) -> Result<()> {
    run_corpus_with_limit(core, corpus_id, default_limit(core)).await
}

pub async fn run_corpus_with_limit(core: &Core, corpus_id: &str, limit: usize) -> Result<()> {
    let pending = core.store.pending_artifacts_for_corpus(corpus_id).await?;

    // Cutting an oversize chunk can cost a call, so it gets a unit of its own.
    let mut batch: Vec<Chunk> = Vec::with_capacity(pending.len());
    for chunk in pending {
        let text = render(core, &chunk);
        if core.counter.count(&text) > limit {
            // Idle-only: `enqueue` would reset the attempts of a unit already queued.
            core.store
                .rearm_idle_seq(Stage::Embed, "artifact", &chunk.id, 0)
                .await?;
        } else {
            batch.push(chunk);
        }
    }

    // One batch per run, so other queued work and questions interleave.
    let take = BATCH.min(batch.len());
    if take == 0 {
        return settle_corpus(core, corpus_id).await;
    }

    let permit = core.gate.background_light().await;
    let outcome = embed_batch(core, &batch[..take]).await;
    permit.finished();
    match outcome {
        Ok(()) => {}
        // The batch cannot say which chunk was refused; per-chunk jobs isolate it.
        Err(e) if input_too_large(&e) => {
            tracing::warn!(corpus_id, error = %e, "batch held a chunk the endpoint will not take; isolating");
            return split_into_artifact_jobs(core, corpus_id).await;
        }
        Err(e) => return Err(e),
    }

    // Re-arming is the caller's (`rearm_if_more`).
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

/// Called by `run_one` after the job completes, never from the handler: the queue is keyed by
/// `(stage, target)`, so re-arming inside would be marked done by `complete_job`.
pub async fn rearm_if_more(core: &Core, corpus_id: &str) -> Result<()> {
    if core
        .store
        .pending_artifacts_for_corpus(corpus_id)
        .await?
        .is_empty()
    {
        return Ok(());
    }
    // Per-chunk units replaced the batch; re-arming it would waste a call and reset attempts.
    if core.store.pending_artifacts_are_isolated(corpus_id).await? {
        return Ok(());
    }
    // `seq` climbs, so later batches sink below the first batches of documents captured since.
    let next_seq = core
        .store
        .job_seq(Stage::Embed, corpus_id)
        .await?
        .unwrap_or(0)
        + 1;
    // Idle-only: between `complete_job` and here another worker can reopen and claim the row, and a
    // `Guard::Any` upsert would put two workers into `run_corpus` for one corpus.
    core.store
        .rearm_idle_seq(Stage::Embed, "corpus", corpus_id, next_seq)
        .await
}

/// Chunks are marked embedded only once Qdrant durably accepted their vectors.
async fn embed_batch(core: &Core, chunks: &[Chunk]) -> Result<()> {
    if chunks.is_empty() {
        return Ok(());
    }
    let docs: Vec<EmbedDoc> = chunks.iter().map(doc_of).collect();
    let vectors = core.embedder.embed_documents(&docs).await?;
    let origins = origins_for(core, chunks).await?;
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
        .zip(vectors)
        .map(|(c, vector)| VectorPoint {
            vector,
            // Without the template, so the lexical and semantic halves describe the same document.
            sparse: crate::vector::sparse::encode_document(&lexical_text(c)),
            payload: with_origins(payload_of(c), &origins),
        })
        .collect();
    upsert_with_current_lifecycle(core, points).await?;

    for c in chunks {
        mark_indexed(core, c).await?;
    }
    Ok(())
}

/// The revision is the one read before the call, so an edit landing in between wins.
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
    // Armed, not run, and not propagated: a failure here must not re-buy the embedding; the sweep
    // backstops both. Passages and merges are never relate anchors (spec §6; a merge files its own
    // next pair). `Merged`, not `is_model_written()`: a synthesis is ordinary dedupe material.
    if chunk.provenance != crate::store::artifacts::Provenance::Passage
        && chunk.provenance != crate::store::artifacts::Provenance::Merged
        && let Err(e) = crate::jobs::relate::arm(core, &chunk.id, 0).await
    {
        tracing::warn!(
            artifact_id = %chunk.id,
            error = %e,
            "could not arm the neighbour query; the sweep will find its pairs"
        );
    }
    if chunk.provenance != crate::store::artifacts::Provenance::Passage
        && !chunk.cues.is_empty()
        && let Err(e) = crate::jobs::probe::arm(core, &chunk.id).await
    {
        tracing::warn!(artifact_id = %chunk.id, error = %e, "could not arm the cue probes");
    }
    // A merge hides what it replaced only once indexed, so the knowledge is never out of search.
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

/// One bad chunk must not keep its siblings out of search.
pub async fn split_into_artifact_jobs(core: &Core, corpus_id: &str) -> Result<()> {
    let pending = core.store.pending_artifacts_for_corpus(corpus_id).await?;
    for c in &pending {
        // Idle-only: a `Guard::Any` upsert could reset a running unit and split one parent twice.
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

/// Truncating would silently discard knowledge.
async fn split_oversize(core: &Core, chunk: &Chunk, limit: usize, hard: bool) -> Result<()> {
    // A merge has no corpus to write siblings into, so cutting is skipped rather than refused
    // forever.
    let splittable = chunk.corpus_id.is_some();
    // Siblings inherit the title and template, so the text gets only what the envelope leaves.
    let title_cost = envelope_cost(core, chunk.title.as_deref());
    let budget = limit.saturating_sub(title_cost);

    // A title that fills the limit leaves no split of the text that can help.
    if splittable && budget > 0 {
        let parts = split_by_paragraphs(&chunk.text, budget, &core.counter);
        if parts.len() > 1 {
            return replace_with_siblings(core, chunk, parts).await;
        }
        // No blank line to cut on: on a real refusal, cut on lines and then characters.
        if hard {
            let parts = split_by_lines(&chunk.text, budget, &core.counter);
            if parts.len() > 1 {
                return replace_with_siblings(core, chunk, parts).await;
            }
        }
    }

    // Optimism, once: one over-long vector beats shredding a chunk on a guess.
    tracing::warn!(
        artifact_id = %chunk.id,
        title_cost,
        splittable,
        "oversize chunk has no split available; embedding as-is"
    );
    let permit = core.gate.background_light().await;
    let embedded = core
        .embedder
        .embed_documents(std::slice::from_ref(&doc_of(chunk)))
        .await;
    permit.finished();
    let vectors = match embedded {
        Ok(v) => v,
        // Not gated on `budget`: a refusal we cannot act on is still a refusal.
        Err(e) if input_too_large(&e) => {
            // No split can land for a merge; index what fits rather than leave it out of search.
            if !splittable {
                return embed_head(core, chunk, limit).await;
            }
            // At a budget of zero the line split shreds the text into fragments still oversize with
            // the title.
            if budget > 0 {
                let parts = split_by_lines(&chunk.text, budget, &core.counter);
                if parts.len() > 1 {
                    tracing::warn!(artifact_id = %chunk.id, parts = parts.len(), "endpoint refused it whole; cutting on lines");
                    return replace_with_siblings(core, chunk, parts).await;
                }
            }
            return Err(e);
        }
        Err(e) => return Err(e),
    };
    let origins = origins_for(core, std::slice::from_ref(chunk)).await?;
    upsert_with_current_lifecycle(
        core,
        vec![VectorPoint {
            vector: vectors.into_iter().next().unwrap_or_default(),
            sparse: crate::vector::sparse::encode_document(&lexical_text(chunk)),
            payload: with_origins(payload_of(chunk), &origins),
        }],
    )
    .await?;
    mark_indexed(core, chunk).await?;
    match &chunk.corpus_id {
        Some(corpus_id) => settle_corpus(core, corpus_id).await,
        // A merged artifact belongs to no corpus; nothing to settle.
        None => Ok(()),
    }
}

/// For a merge, which has no corpus to split into: the dense vector is placed by the head, the
/// sparse vector still encodes the whole text, and the stored text is untouched.
async fn embed_head(core: &Core, chunk: &Chunk, limit: usize) -> Result<()> {
    let title_cost = envelope_cost(core, chunk.title.as_deref());
    let budget = limit.saturating_sub(title_cost);
    let head = if budget > 0 {
        split_by_lines(&chunk.text, budget, &core.counter)
            .into_iter()
            .next()
            .unwrap_or_default()
    } else {
        String::new()
    };
    // A title filling the limit is a broken title; reported, not worked around.
    if head.trim().is_empty() {
        return Err(Error::Validation(format!(
            "cannot embed merged artifact {}: its title alone fills the {limit}-token limit",
            chunk.id
        )));
    }

    tracing::warn!(
        artifact_id = %chunk.id,
        kept = head.len(),
        of = chunk.text.len(),
        "merged artifact is too long to embed and has no corpus to split into; \
         indexing its opening"
    );

    let input = EmbedDoc {
        title: chunk.title.clone(),
        text: head,
    };
    let permit = core.gate.background_light().await;
    let embedded = core
        .embedder
        .embed_documents(std::slice::from_ref(&input))
        .await;
    permit.finished();
    let vectors = embedded?;

    let origins = origins_for(core, std::slice::from_ref(chunk)).await?;
    upsert_with_current_lifecycle(
        core,
        vec![VectorPoint {
            vector: vectors.into_iter().next().unwrap_or_default(),
            // Lexical retrieval has no length limit, so the tail stays searchable there.
            sparse: crate::vector::sparse::encode_document(&lexical_text(chunk)),
            payload: with_origins(payload_of(chunk), &origins),
        }],
    )
    .await?;
    // Also finishes the merge; its roots stay in search until this lands.
    mark_indexed(core, chunk).await
}

/// Returns one part when no boundary helps.
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

/// Always returns parts smaller than the input; a line too long is cut on character count.
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

    // Four characters per token, the ratio the budget estimate uses.
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
    // One part is the parent under a new id, re-queued and replaced forever.
    if parts.len() < 2 {
        return Err(Error::Validation(format!(
            "refusing to replace artifact {} with {} sibling(s)",
            chunk.id,
            parts.len()
        )));
    }

    // Backstop: `split_oversize` never routes a merge here; reaching this is a bug in the caller.
    let Some(corpus_id) = chunk.corpus_id.as_deref() else {
        return Err(Error::Validation(format!(
            "refusing to split merged artifact {}: it belongs to no corpus",
            chunk.id
        )));
    };

    tracing::info!(artifact_id = %chunk.id, parts = parts.len(), "split oversize chunk into siblings");

    let base = chunk.ordinal;
    // Make room first: `base * 1000 + i` sorts siblings after later chunks.
    core.store
        .make_room_after(corpus_id, base, parts.len() as i64 - 1)
        .await?;
    let new: Vec<NewArtifact> = parts
        .iter()
        .enumerate()
        .map(|(i, text)| NewArtifact {
            ordinal: base + i as i64,
            text: text.clone(),
            corpus_span: chunk.corpus_span.clone(),
            title: chunk.title.clone(),
            category: chunk.category.clone(),
            // A caveat covers the whole passage, so every fragment inherits it.
            caveats: chunk.caveats.clone(),
            tags: chunk.tags.clone(),
            // Or a re-segmentation of that window would leave them behind.
            segment_idx: chunk.segment_idx,
        })
        .collect();

    let inserted = core.store.insert_artifacts(corpus_id, &new).await?;
    core.store.delete_artifact(&chunk.id).await?;
    core.vectors
        .delete_artifacts(std::slice::from_ref(&chunk.id))
        .await?;
    // Anything hidden behind the parent now points at an artifact that does not exist.
    core.heal_dangling_supersessions().await?;

    for c in &inserted {
        core.store.enqueue(Stage::Embed, "artifact", &c.id).await?;
    }
    Ok(())
}

/// One query for the batch; a passage or captured row answers with its own corpus.
async fn origins_for(
    core: &Core,
    chunks: &[Chunk],
) -> Result<std::collections::BTreeMap<String, Vec<crate::store::lineage::Origin>>> {
    let ids: Vec<String> = chunks.iter().map(|c| c.id.clone()).collect();
    core.store.origins_of(&ids).await
}

/// Sorted and distinct, so `cap_per_corpus` can count a merge against each corpus.
fn with_origins(
    mut payload: VectorPayload,
    origins: &std::collections::BTreeMap<String, Vec<crate::store::lineage::Origin>>,
) -> VectorPayload {
    let set: std::collections::BTreeSet<String> = origins
        .get(&payload.artifact_id)
        .map(|o| o.iter().map(|x| x.corpus_id.clone()).collect())
        .unwrap_or_default();
    payload.origin_corpora = set.into_iter().collect();
    payload
}

fn payload_of(chunk: &Chunk) -> VectorPayload {
    VectorPayload {
        artifact_id: chunk.id.clone(),
        // Empty for a merge, so no corpus filter matches it.
        corpus_id: chunk.corpus_id.clone().unwrap_or_default(),
        text: chunk.text.clone(),
        title: chunk.title.clone(),
        category: chunk.category.clone(),
        tags: chunk.tags.clone(),
        created_at: chunk.created_at,
        // Unset keeps the stored stamp, so a re-embed does not make a chunk look forgotten.
        last_seen_at: None,
        hit_count: None,
        // Retired is a fact; active cannot tell "still active" from a stale read, so it defers.
        status: (chunk.status != ArtifactStatus::Active).then_some(chunk.status),
        // A missing stamp reads as epoch, maximally stale.
        last_verified_at: chunk.last_verified_at.or(Some(chunk.created_at)),
        superseded_by: chunk.superseded_by.clone(),
        origin_corpora: vec![],
        provenance: Some(chunk.provenance.as_str().to_string()),
    }
}

/// The lifecycle fields are re-read under `lifecycle_lock` before and after the upsert, without
/// holding the lock across the network write; `lifecycle_dirty` marks retired rows until the write
/// is acknowledged.
async fn upsert_with_current_lifecycle(core: &Core, mut points: Vec<VectorPoint>) -> Result<()> {
    if points.is_empty() {
        return Ok(());
    }
    let mut marked: Vec<String> = Vec::new();
    let mut gone: Vec<String> = Vec::new();
    {
        let _guard = core.lifecycle_lock.lock().await;
        for p in &mut points {
            let id = p.payload.artifact_id.clone();
            match core.store.get_artifact(&id).await {
                Ok(fresh) => {
                    p.payload.status =
                        (fresh.status != ArtifactStatus::Active).then_some(fresh.status);
                    p.payload.superseded_by = fresh.superseded_by.clone();
                    if !fresh.in_results() {
                        core.store.mark_lifecycle_dirty(&id).await?;
                        marked.push(id);
                    }
                }
                // Deleted in flight: a point no row explains is an orphan nothing can repair.
                Err(Error::NotFound) => {
                    tracing::info!(
                        artifact_id = %id,
                        "artifact was deleted while it was being embedded; dropping its point"
                    );
                    gone.push(id);
                }
                Err(e) => return Err(e),
            }
        }
    }
    points.retain(|p| !gone.contains(&p.payload.artifact_id));
    if points.is_empty() {
        core.store.clear_lifecycle_dirty(&marked).await?;
        return Ok(());
    }

    let written: Vec<(String, ArtifactStatus, Option<String>)> = points
        .iter()
        .map(|p| {
            (
                p.payload.artifact_id.clone(),
                p.payload.status.unwrap_or(ArtifactStatus::Active),
                p.payload.superseded_by.clone(),
            )
        })
        .collect();
    core.vectors.upsert(points).await?;

    // A transition may have landed during the unlocked write; a row that disagrees now is the race.
    {
        let _guard = core.lifecycle_lock.lock().await;
        let mut fix = Vec::new();
        for (id, status, superseded_by) in &written {
            match core.store.get_artifact(id).await {
                Ok(fresh) if fresh.status != *status || fresh.superseded_by != *superseded_by => {
                    // Marked whichever way the row moved: the point exists now, so either
                    // disagreement is drift.
                    core.store.mark_lifecycle_dirty(id).await?;
                    marked.push(id.clone());
                    fix.push(crate::jobs::consolidate::lifecycle_row_of(&fresh));
                }
                Ok(_) => {}
                // An orphan is `heal_store_drift`'s to report.
                Err(Error::NotFound) => {}
                Err(e) => return Err(e),
            }
        }
        if !fix.is_empty() {
            tracing::info!(
                repaired = fix.len(),
                "a lifecycle transition landed while these points were being written"
            );
            core.vectors.apply_lifecycle(&fix).await?;
        }
    }

    // Only after every write returns, or a failed write becomes permanent drift.
    core.store.clear_lifecycle_dirty(&marked).await?;
    Ok(())
}

/// `ready` if every chunk embedded, `partial` if any gave up.
pub async fn settle_corpus(core: &Core, corpus_id: &str) -> Result<()> {
    // Only `embedding`/`partial` mean the windows are written; a note embedding at capture must not
    // report an unextracted document `ready`.
    let was = core.store.get_corpus(corpus_id).await?.status;
    if !matches!(was, CorpusStatus::Embedding | CorpusStatus::Partial) {
        return Ok(());
    }
    if core.store.pending_embed_count(corpus_id).await? > 0 {
        return Ok(());
    }
    let status = if core.store.failed_embed_count(corpus_id).await? > 0 {
        CorpusStatus::Partial
    } else if was == CorpusStatus::Partial {
        // A window the model refused leaves a hole that clean chunks do not fill.
        CorpusStatus::Partial
    } else {
        CorpusStatus::Ready
    };
    core.store.set_corpus_status(corpus_id, status).await?;
    // `partial` included: the chunks that did embed can still answer. Backgrounded so the last
    // chunk's embed does not wait on a vector query per gap.
    let core = core.clone();
    let id = corpus_id.to_string();
    core.clone().background.spawn(async move {
        if let Err(e) = crate::jobs::gaps::cover(&core, &id).await {
            tracing::warn!(corpus_id = %id, error = %e, "could not check what this capture answered");
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_upsert_landing_after_a_restore_does_not_hide_the_artifact_again() {
        // The row is read minutes before the upsert; writing its lifecycle back would hide an
        // artifact whose row says active, with no dirty marker to repair it.
        let core = crate::core::test_support::test_core().await;
        let ids = crate::jobs::consolidate::tests::seed(
            &core,
            &[("first", [1.0, 0.0]), ("second", [0.0, 1.0])],
        )
        .await;
        core.supersede(&ids[0], &ids[1]).await.unwrap();
        let stale = core.store.get_artifact(&ids[0]).await.unwrap();
        assert!(stale.superseded_by.is_some(), "the fixture is not hidden");

        core.unsupersede(&ids[0]).await.unwrap();
        embed_batch(&core, std::slice::from_ref(&stale))
            .await
            .unwrap();

        let hits = core
            .vectors
            .search(&[1.0, 0.0], &Default::default(), 10, &Default::default())
            .await
            .unwrap();
        assert!(
            hits.iter().any(|h| h.payload.artifact_id == ids[0]),
            "a stale embed hid an artifact an operator had restored"
        );
    }

    #[tokio::test]
    async fn an_artifact_deleted_while_it_embedded_leaves_no_point_behind() {
        let core = crate::core::test_support::test_core().await;
        let (_, ids) = corpus_with_pending_chunks(&core, 1).await;
        let stale = core.store.get_artifact(&ids[0]).await.unwrap();

        core.store.delete_artifact(&ids[0]).await.unwrap();
        embed_batch(&core, std::slice::from_ref(&stale))
            .await
            .unwrap();

        assert!(
            !core
                .vectors
                .all_artifact_ids()
                .await
                .unwrap()
                .contains(&ids[0]),
            "the embed wrote a point for an artifact that no longer exists"
        );
    }

    #[tokio::test]
    async fn a_re_embed_of_a_hidden_artifact_leaves_no_lifecycle_marker_behind() {
        // Left standing after success, the marker makes every sweep rewrite the same payload.
        let core = crate::core::test_support::test_core().await;
        let ids = crate::jobs::consolidate::tests::seed(
            &core,
            &[("first", [1.0, 0.0]), ("second", [0.0, 1.0])],
        )
        .await;
        core.supersede(&ids[0], &ids[1]).await.unwrap();
        let hidden = core.store.get_artifact(&ids[0]).await.unwrap();

        embed_batch(&core, std::slice::from_ref(&hidden))
            .await
            .unwrap();

        assert!(
            core.store
                .dirty_lifecycle_artifacts(10)
                .await
                .unwrap()
                .is_empty(),
            "the re-embed left a marker on a base the two stores agree about"
        );
    }

    async fn corpus_with_pending_chunks(core: &Core, n: usize) -> (String, Vec<String>) {
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let new: Vec<_> = (0..n)
            .map(|i| NewArtifact {
                ordinal: i as i64,
                text: format!("chunk {i}"),
                segment_idx: Some(i as i64),
                ..Default::default()
            })
            .collect();
        let made = core.store.insert_artifacts(&src.id, &new).await.unwrap();
        (src.id, made.iter().map(|c| c.id.clone()).collect())
    }

    async fn job_state(core: &Core, stage: Stage, target: &str) -> Option<String> {
        sqlx::query_scalar!(
            "SELECT state FROM jobs WHERE stage = ? AND target_id = ?",
            stage.as_str(),
            target
        )
        .fetch_optional(&core.store.control.pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn an_embedded_artifact_arms_the_moments_read() {
        let core = test_core().await;
        let _out = core
            .ingest_capture(crate::core::ingest::Capture::new("Anything at all", "ui"))
            .await
            .unwrap();
        crate::jobs::test_support::drain(&core).await;
    }

    async fn merged_over(core: &Core, roots: &[String]) -> crate::store::artifacts::Chunk {
        core.store
            .insert_merged_artifact(
                &crate::store::artifacts::NewMerged {
                    title: Some("merged".into()),
                    text: "both wordings".into(),
                    ..Default::default()
                },
                roots,
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_merged_artifact_does_not_arm_a_neighbour_query() {
        let core = crate::core::test_support::test_core().await;
        let roots = crate::jobs::consolidate::tests::seed(
            &core,
            &[
                ("first wording", [1.0, 0.0]),
                ("second wording", [0.99, 0.05]),
            ],
        )
        .await;
        let m = merged_over(&core, &roots).await;

        mark_indexed(&core, &m).await.unwrap();

        assert_eq!(
            job_state(&core, Stage::Relate, &m.id).await,
            None,
            "a merge that queries its neighbours walks the corpus one passage at a time"
        );
    }

    #[tokio::test]
    async fn isolating_a_batch_leaves_a_chunk_already_being_split_alone() {
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
        assert_eq!(
            job_state(&core, Stage::Embed, &chunks[1]).await.as_deref(),
            Some("pending")
        );
    }

    #[tokio::test]
    async fn coming_back_for_another_batch_does_not_disturb_a_running_row() {
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

        sqlx::query!(
            "UPDATE jobs SET state = 'done' WHERE stage = 'embed' AND target_id = ?",
            &src
        )
        .execute(&core.store.control.pool)
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
        // A first embed has no stored status to defer to.
        let core = crate::core::test_support::test_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    text: "a stale instruction".into(),
                    segment_idx: Some(0),
                    ..Default::default()
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
        // Config claimed 8192 tokens; llama.cpp's physical batch was 1024.
        let mut core = crate::core::test_support::test_core().await;
        let strict = std::sync::Arc::new(crate::infer::fake::StrictEmbedder::new(
            crate::core::test_support::TEST_DIM,
            200,
        ));
        core.embedder = strict.clone();

        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let body = (0..40)
            .map(|i| format!("paragraph {i} with a good deal of filler text in it"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    text: body,
                    segment_idx: Some(0),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();

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
                    text: body,
                    segment_idx: Some(0),
                    ..Default::default()
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
        let core = crate::core::test_support::test_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();

        // 3.5 characters per estimated token: the title is what puts the text over the limit.
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
                    text: text.clone(),
                    title: Some(title),
                    segment_idx: Some(0),
                    ..Default::default()
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
        for c in &chunks {
            assert!(
                core.counter.count(&render(&core, c)) <= limit,
                "sibling is still oversize: {:?}",
                c.text
            );
        }
    }

    #[tokio::test]
    async fn the_envelope_is_charged_so_a_chunk_that_fits_bare_and_overflows_rendered_splits_once()
    {
        // With a real template, `title: ` and ` | text: ` cost tokens too.
        let mut core = crate::core::test_support::test_core().await;
        core.embedder = std::sync::Arc::new(crate::infer::fake::FakeEmbedder::with_templates(
            crate::core::test_support::TEST_DIM,
            crate::config::EmbedTemplates::default(),
        ));
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();

        let title = "heading".to_string();
        let text = format!("{}\n\n{}", "alpha ".repeat(12), "beta ".repeat(12));
        let limit = 40;
        let bare = format!("{title}\n{text}");
        assert!(
            core.counter.count(&bare) <= limit,
            "title + text must fit without the envelope ({})",
            core.counter.count(&bare)
        );
        let rendered = core.embedder.render_document(&crate::infer::EmbedDoc {
            title: Some(title.clone()),
            text: text.clone(),
        });
        assert!(
            core.counter.count(&rendered) > limit,
            "the envelope must be what pushes it over ({})",
            core.counter.count(&rendered)
        );

        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    text: text.clone(),
                    title: Some(title),
                    segment_idx: Some(0),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();

        run_with_limit(&core, &made[0].id, limit).await.unwrap();

        let chunks = core.store.artifacts_for_corpus(&src.id).await.unwrap();
        assert!(
            chunks.len() > 1,
            "the chunk was not split: {} row(s)",
            chunks.len()
        );
        assert!(
            !chunks.iter().any(|c| c.text == text && c.id != made[0].id),
            "the parent was replaced by an identical copy of itself"
        );
        for c in &chunks {
            assert!(
                core.counter.count(&render(&core, c)) <= limit,
                "sibling is still oversize once rendered: {:?}",
                c.text
            );
        }
    }

    #[tokio::test]
    async fn a_refusal_with_no_budget_left_is_reported_rather_than_shredded() {
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
                    text: text.clone(),
                    title: Some(title),
                    segment_idx: Some(0),
                    ..Default::default()
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
    async fn a_413_from_a_real_endpoint_splits_the_chunk_like_a_bare_servers_refusal() {
        let mut core = crate::core::test_support::test_core().await;
        core.embedder = std::sync::Arc::new(crate::infer::fake::StrictEmbedder::over_http(
            crate::core::test_support::TEST_DIM,
            120,
        ));

        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let text = (0..40)
            .map(|i| format!("paragraph {i} with enough words in it to measure\n\nmore"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    text,
                    segment_idx: Some(0),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();

        // Our estimate passes, so only the endpoint's answer can start the split.
        run_with_limit(&core, &made[0].id, 8192).await.unwrap();

        let chunks = core.store.artifacts_for_corpus(&src.id).await.unwrap();
        assert!(
            chunks.len() > 1,
            "a 413 has to cut the chunk; got {} chunk(s)",
            chunks.len()
        );
        for c in &chunks {
            assert_ne!(
                c.embed_state,
                crate::store::artifacts::EmbedState::Failed,
                "the chunk was parked instead of split"
            );
        }
    }

    #[tokio::test]
    async fn a_refusal_during_the_as_is_attempt_still_ends_in_a_split() {
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
                    text: body,
                    segment_idx: Some(0),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();

        run_with_limit(&core, &made[0].id, 100).await.unwrap();

        let chunks = core.store.artifacts_for_corpus(&src.id).await.unwrap();
        assert!(chunks.len() > 1, "got {} chunks", chunks.len());
    }

    #[test]
    fn splitting_by_lines_always_returns_something_smaller() {
        let counter = crate::infer::budget::TokenCounter::default();
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

        let refused = Error::InferenceRejected {
            role: "embed",
            detail: "HTTP 413 Payload Too Large: input is too large to process".into(),
        };
        assert!(input_too_large(&refused));

        // Transient failures must stay retryable, not start shredding chunks.
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

    async fn corpus_with_chunks(core: &Core, n: usize) -> String {
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let new: Vec<NewArtifact> = (0..n)
            .map(|i| NewArtifact {
                ordinal: i as i64,
                text: format!("chunk number {i}"),
                title: Some(format!("t{i}")),
                segment_idx: Some(0),
                ..Default::default()
            })
            .collect();
        core.store.insert_artifacts(&src.id, &new).await.unwrap();
        src.id
    }

    #[tokio::test]
    async fn one_run_embeds_at_most_one_batch() {
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
        let core = test_core().await;
        let (_src, ids) = seed(&core, &["one"]).await;

        // A path through the gate cannot start while another call holds the turn.
        let permit = core.gate.background().await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), run(&core, &ids[0]))
                .await
                .is_err(),
            "the per-chunk embed path went straight to the endpoint"
        );

        permit.finished();
        run(&core, &ids[0]).await.unwrap();
        assert_eq!(
            core.store.get_artifact(&ids[0]).await.unwrap().embed_state,
            EmbedState::Embedded
        );
    }

    #[tokio::test]
    async fn isolating_a_batch_does_not_put_the_batch_straight_back() {
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

        let batch_armed: i64 = sqlx::query_scalar!(
            "SELECT count(*) FROM jobs
              WHERE stage = 'embed' AND target_kind = 'corpus' AND state = 'pending'"
        )
        .fetch_one(&core.store.control.pool)
        .await
        .unwrap();
        assert_eq!(
            batch_armed, 0,
            "the batch was re-armed beside the per-chunk units that replaced it"
        );
    }

    #[tokio::test]
    async fn oversize_chunks_do_not_turn_one_batch_into_many_calls() {
        let (core, embedder) = crate::core::test_support::test_core_counting_embed_calls().await;
        let big = "alpha ".repeat(400);
        let (src_id, _) = seed(&core, &[&big, &big, &big, "small"]).await;

        run_corpus_with_limit(&core, &src_id, 200).await.unwrap();

        assert_eq!(
            embedder.calls(),
            1,
            "the batch job made more than one inference call"
        );
        let armed: i64 = sqlx::query_scalar!(
            "SELECT count(*) FROM jobs
              WHERE stage = 'embed' AND target_kind = 'artifact' AND state = 'pending'"
        )
        .fetch_one(&core.store.control.pool)
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

        // The exact total pins termination: three batch claims plus one `relate` unit per indexed
        // artifact.
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
        let core = test_core().await;
        let long = corpus_with_chunks(&core, BATCH * 2).await;
        core.store
            .enqueue(Stage::Embed, "corpus", &long)
            .await
            .unwrap();
        // One claim: the re-arm happens in `run_one` after the job is closed.
        crate::jobs::run_one(&core).await.unwrap();

        let seq = core
            .store
            .job_seq(Stage::Embed, &long)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(seq, 1, "the re-armed batch stayed at the front");
    }

    #[tokio::test]
    async fn an_embed_batch_does_not_serve_the_generation_cooldown() {
        // Real time: a paused clock times out sqlx's pool acquire. 30 s against sub-second leaves
        // margin.
        let mut core = crate::core::test_support::test_core().await;
        core.gate = std::sync::Arc::new(crate::infer::gate::InferenceGate::new(
            std::time::Duration::from_secs(30),
        ));
        let (src, _ids) = seed(&core, &["one", "two"]).await;
        // A generation has just ended, so a paced call would serve the gap.
        core.gate.call_finished();

        let started = std::time::Instant::now();
        run_corpus(&core, &src).await.unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "the embed job served a generation's cooldown: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn an_embed_batch_does_not_hold_the_next_generation_off() {
        let mut core = crate::core::test_support::test_core().await;
        core.gate = std::sync::Arc::new(crate::infer::gate::InferenceGate::new(
            std::time::Duration::from_secs(600),
        ));
        let (src, _ids) = seed(&core, &["one", "two"]).await;
        run_corpus(&core, &src).await.unwrap();

        tokio::time::pause();
        let started = tokio::time::Instant::now();
        core.gate.background().await;
        assert_eq!(started.elapsed(), std::time::Duration::ZERO);
    }

    async fn seed(core: &crate::core::Core, texts: &[&str]) -> (String, Vec<String>) {
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let new: Vec<NewArtifact> = texts
            .iter()
            .enumerate()
            .map(|(i, t)| NewArtifact {
                ordinal: i as i64,
                text: t.to_string(),
                title: Some(format!("t{i}")),
                category: Some("reference".into()),
                tags: vec!["x".into()],
                ..Default::default()
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

        let q = core
            .embedder
            .embed_query("t0\n## A\nthe body")
            .await
            .unwrap();
        let hits = core
            .vectors
            .search(&q, &Default::default(), 5, &Default::default())
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
        // The split is across units, so the corpus is driven through the queue.
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
        let core = test_core().await;
        let big = format!("{}\n\n{}", "alpha ".repeat(400), "beta ".repeat(400));
        let (src_id, ids) = seed(&core, &["small one", &big, "small two"]).await;

        run_corpus_with_limit(&core, &src_id, 200).await.unwrap();

        assert_eq!(
            core.vectors.count().await.unwrap(),
            2,
            "the two small chunks should be embedded"
        );
        let armed: i64 = sqlx::query_scalar!(
            "SELECT count(*) FROM jobs
              WHERE stage = 'embed' AND target_kind = 'artifact' AND target_id = ?
                AND state = 'pending'",
            &ids[1]
        )
        .fetch_one(&core.store.control.pool)
        .await
        .unwrap();
        assert_eq!(armed, 1, "the oversize chunk was not given its own unit");

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
        let core = test_core().await;
        let big = "alpha ".repeat(800);
        let (_src, ids) = seed(&core, &[&big]).await;
        run_with_limit(&core, &ids[0], 200).await.unwrap();
        assert_eq!(core.vectors.count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn a_chunk_edited_mid_embed_is_not_reported_as_indexed() {
        let core = test_core().await;
        let (_src, ids) = seed(&core, &["one"]).await;
        let stale = core.store.get_artifact(&ids[0]).await.unwrap();

        core.store
            .update_artifact_text(&ids[0], "edited while embedding")
            .await
            .unwrap();

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

        run(&core, &ids[0]).await.unwrap();
        assert_eq!(
            core.store.get_artifact(&ids[0]).await.unwrap().embed_state,
            EmbedState::Embedded
        );
    }

    #[tokio::test]
    async fn reprocessing_a_source_outlives_a_worker_already_embedding_it() {
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
        // A point write replaces the payload rather than merging it.
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
