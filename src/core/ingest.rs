use super::Core;
use crate::error::{Error, Result};
use crate::store::artifacts::ArtifactStatus;
use crate::store::corpora::{CorpusStatus, Insertion, NearDuplicate, content_hash};
use crate::store::jobs::Stage;
use crate::store::now;
use sha2::Digest;

/// The channel an image arrives through. Its own value, like `upload`, so
/// the queue and the detail page can tell a photo from a paste.
pub const ORIGIN_IMAGE: &str = "image";
/// Longest note kept. Context, not a document: someone wanting to say more
/// than this has a paste box.
pub const MAX_NOTE_CHARS: usize = 2000;

/// The user's context for a capture, cleaned: trimmed, capped, `None` when
/// there is nothing in it.
fn clean_note(note: Option<String>) -> Option<String> {
    let n = note?.trim().to_string();
    if n.is_empty() {
        return None;
    }
    Some(n.chars().take(MAX_NOTE_CHARS).collect())
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct IngestOutcome {
    pub id: String,
    pub status: CorpusStatus,
    /// True when the text was already stored byte for byte and no new source
    /// was created.
    pub duplicate: bool,
    /// Set when the text is not identical to anything stored but is close
    /// enough that segmenting it would produce artifacts competing with ones
    /// that already exist. The capture is stored and parked, never dropped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub near_duplicate: Option<NearDuplicate>,
}

/// What one pass of `Core::heal_store_drift` put back.
///
/// Reported rather than merely logged because "the stores agreed" and "the
/// stores disagreed and were repaired" are different facts, and a repair that
/// fires on every sweep over a base in agreement is a bug that hides behind a
/// correct end state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct StoreDrift {
    /// Artifact rows rebuilt from a surviving vector payload.
    pub rows_restored: usize,
    /// Placeholder corpus rows written because a restored artifact's source was
    /// not stored either.
    pub corpora_restored: usize,
    /// Artifacts whose row claimed `done` but had no point, re-queued for
    /// embedding.
    pub points_requeued: usize,
}

/// What an operator decided about a parked capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NearDupeAction {
    /// The new capture is the better copy: delete the old corpus and its
    /// artifacts, then process this one.
    Replace,
    /// They are genuinely different despite the score. Process this one and
    /// leave the other alone.
    KeepBoth,
    /// The new capture adds nothing. Delete it.
    Discard,
}

/// One image, whichever door it arrived through.
#[derive(Debug, Clone)]
pub struct ImageCapture {
    pub bytes: Vec<u8>,
    pub filename: Option<String>,
    pub title_hint: Option<String>,
    pub note: Option<String>,
}

/// One capture, whichever door it arrived through.
///
/// A struct rather than four positional arguments: `origin` and `source_url`
/// are two different facts about the same event, and every existing caller
/// that has nothing to say about the second should not have to say `None`.
#[derive(Debug, Clone)]
pub struct Capture {
    pub text: String,
    /// The channel: `web`, `mcp`, `extension`, `fetch`, `upload`.
    pub origin: String,
    pub title_hint: Option<String>,
    /// Where the text was read. Provenance, never an instruction: nothing
    /// downstream ever fetches this.
    pub source_url: Option<String>,
    /// What the door knew beyond the text. Namespaced; see the schema comment.
    pub metadata: serde_json::Value,
}

impl Capture {
    pub fn new(text: impl Into<String>, origin: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            origin: origin.into(),
            title_hint: None,
            source_url: None,
            metadata: serde_json::json!({}),
        }
    }

    pub fn with_note(mut self, note: Option<String>) -> Self {
        if let Some(n) = clean_note(note) {
            self.metadata["note"] = serde_json::Value::String(n);
        }
        self
    }

    /// The `file` facts of an uploaded text file.
    pub fn with_file(mut self, name: Option<&str>, size: usize, mime: &str) -> Self {
        let mut f = serde_json::json!({ "size": size, "mime": mime });
        if let Some(n) = name {
            f["name"] = serde_json::Value::String(n.to_string());
        }
        self.metadata["file"] = f;
        self
    }

    pub fn with_title(mut self, title: Option<String>) -> Self {
        self.title_hint = title;
        self
    }

    pub fn with_source_url(mut self, url: Option<String>) -> Self {
        self.source_url = url;
        self
    }
}

impl Core {
    /// Store the text and queue processing. Deliberately makes no inference
    /// call: capture must stay instant and must survive a dead endpoint.
    pub async fn ingest(
        &self,
        text: &str,
        origin: &str,
        title_hint: Option<&str>,
    ) -> Result<IngestOutcome> {
        self.ingest_capture(Capture::new(text, origin).with_title(title_hint.map(str::to_string)))
            .await
    }

    /// The same thing, for a door that also knows where the text was read.
    pub async fn ingest_capture(&self, c: Capture) -> Result<IngestOutcome> {
        let text = c.text.as_str();
        let origin = c.origin.as_str();
        let title_hint = c.title_hint.as_deref();
        if text.trim().is_empty() {
            return Err(Error::Validation("text is empty".into()));
        }

        if let Some(existing) = self.store.find_by_hash(&content_hash(text)).await? {
            tracing::info!(corpus_id = %existing.id, "duplicate ingest, returning existing source");
            return Ok(IngestOutcome {
                id: existing.id,
                status: existing.status,
                duplicate: true,
                near_duplicate: None,
            });
        }

        // Computed once, before the insert, so the same signature answers "is
        // this a near-duplicate" and becomes the row's stored column.
        let sig = crate::store::shingle::signature(text);
        let near = self
            .store
            .find_near_duplicate(&sig, self.consolidate.near_dupe_min)
            .await?;

        let src = match self
            .store
            .insert_corpus_with_signature(
                text,
                origin,
                title_hint,
                sig,
                c.source_url.as_deref(),
                &c.metadata,
            )
            .await?
        {
            Insertion::Created(src) => src,
            // Another capture of the same bytes landed while this one was
            // scanning for near-duplicates. That scan reads every stored
            // signature, so the window is wide enough to hit in practice — a
            // double-submitted form is enough. The other writer owns the row
            // and has already queued whatever it queued; saying "duplicate" is
            // both true and the same answer this call would have given a
            // moment earlier.
            Insertion::Existing(existing) => {
                tracing::info!(corpus_id = %existing.id, "concurrent duplicate ingest, returning the stored source");
                return Ok(IngestOutcome {
                    id: existing.id,
                    status: existing.status,
                    duplicate: true,
                    near_duplicate: None,
                });
            }
        };

        match &near {
            // Parked. Synthesis is the expensive stage and this text may not
            // deserve it; an operator decides on Ops. Nothing is lost either
            // way — the corpus is stored verbatim like any other.
            Some(n) => {
                self.store
                    .set_near_dupe(&src.id, Some(&n.corpus_id), Some(n.similarity))
                    .await?;
                self.store
                    .set_corpus_status(&src.id, CorpusStatus::NeedsReview)
                    .await?;
                tracing::info!(
                    corpus_id = %src.id,
                    near = %n.corpus_id,
                    similarity = n.similarity,
                    "capture looks like an existing corpus; parked for review"
                );
            }
            None => {
                self.store
                    .enqueue(Stage::Synthesize, "corpus", &src.id)
                    .await?;
                tracing::info!(corpus_id = %src.id, origin, bytes = text.len(), "ingested");
            }
        }

        Ok(IngestOutcome {
            id: src.id,
            status: if near.is_some() {
                CorpusStatus::NeedsReview
            } else {
                CorpusStatus::Raw
            },
            duplicate: false,
            near_duplicate: near,
        })
    }

    /// Store the image and queue the vision stage. Like `ingest_capture`, this
    /// makes no inference call: the phone gets its answer the moment the bytes
    /// are safe, and a dead vision endpoint costs a wait, not a photo.
    pub async fn ingest_image(&self, c: ImageCapture) -> Result<IngestOutcome> {
        if self.describer.is_none() {
            return Err(Error::Validation(
                "image capture is not configured — set [infer.vision] to enable it".into(),
            ));
        }
        let prepared = super::image::prepare(&c.bytes, self.capture.image_preview_edge)?;
        let hash = hex::encode(sha2::Sha256::digest(&c.bytes));
        if let Some(existing) = self.store.find_by_hash(&hash).await? {
            tracing::info!(corpus_id = %existing.id, "duplicate image, returning existing source");
            return Ok(IngestOutcome {
                id: existing.id,
                status: existing.status,
                duplicate: true,
                near_duplicate: None,
            });
        }

        let mut metadata = serde_json::json!({
            "file": super::image::file_facts(c.filename.as_deref(), c.bytes.len(), &prepared),
        });
        if prepared.exif.as_object().is_some_and(|o| !o.is_empty()) {
            metadata["exif"] = prepared.exif.clone();
        }
        if let Some(n) = clean_note(c.note) {
            metadata["note"] = serde_json::Value::String(n);
        }
        let title_hint = c.title_hint.or_else(|| c.filename.clone());

        let src = match self
            .store
            .insert_image_corpus(&hash, ORIGIN_IMAGE, title_hint.as_deref(), &metadata)
            .await?
        {
            Insertion::Created(src) => src,
            Insertion::Existing(existing) => {
                return Ok(IngestOutcome {
                    id: existing.id,
                    status: existing.status,
                    duplicate: true,
                    near_duplicate: None,
                });
            }
        };
        self.store
            .insert_attachment(&crate::store::attachments::NewAttachment {
                corpus_id: &src.id,
                kind: "image",
                mime: prepared.mime,
                filename: c.filename.as_deref(),
                bytes: &c.bytes,
                preview: &prepared.preview_jpeg,
                width: Some(prepared.width as i64),
                height: Some(prepared.height as i64),
            })
            .await?;
        self.store
            .enqueue(Stage::Describe, "corpus", &src.id)
            .await?;
        tracing::info!(
            corpus_id = %src.id,
            bytes = c.bytes.len(),
            mime = prepared.mime,
            "image captured; queued for reading"
        );
        Ok(IngestOutcome {
            id: src.id,
            status: CorpusStatus::Describing,
            duplicate: false,
            near_duplicate: None,
        })
    }

    /// Act on a parked capture. Every branch ends with a corpus that is either
    /// in the pipeline or gone; none of them leaves a corpus stuck in
    /// `needs_review` with no way out.
    pub async fn resolve_near_duplicate(
        &self,
        corpus_id: &str,
        action: NearDupeAction,
    ) -> Result<()> {
        let src = self.store.get_corpus(corpus_id).await?;
        let Some(other) = src.near_dupe_of.clone() else {
            return Err(Error::Validation(
                "this corpus is not parked as a near-duplicate".into(),
            ));
        };

        match action {
            NearDupeAction::Discard => {
                self.delete_corpus(&src.id).await?;
                tracing::info!(corpus_id = %src.id, "discarded a near-duplicate capture");
            }
            NearDupeAction::Replace | NearDupeAction::KeepBoth => {
                if action == NearDupeAction::Replace {
                    // The older corpus goes first. If this fails the new one is
                    // still parked, which is a state an operator can retry from;
                    // releasing it first would leave both live on a failure.
                    //
                    // Unless it is already gone: `near_dupe_of` can name a
                    // corpus that has since been deleted — including another
                    // parked capture that was discarded, since a parked corpus
                    // is still matchable. Failing there would leave the only
                    // way out of the queue behind a 404, with nothing on the
                    // page to say that "keep both" is now the same decision.
                    match self.delete_corpus(&other).await {
                        Ok(()) => {
                            tracing::info!(corpus_id = %src.id, replaced = %other, "replaced an older corpus");
                        }
                        Err(Error::NotFound) => {
                            tracing::info!(
                                corpus_id = %src.id,
                                replaced = %other,
                                "the corpus this capture replaces was already deleted"
                            );
                        }
                        Err(e) => return Err(e),
                    }
                }
                self.store.set_near_dupe(&src.id, None, None).await?;
                self.store
                    .set_corpus_status(&src.id, CorpusStatus::Raw)
                    .await?;
                self.store
                    .enqueue(Stage::Synthesize, "corpus", &src.id)
                    .await?;
            }
        }
        Ok(())
    }

    /// Put a superseded artifact back in search.
    ///
    /// The payload flag first, then the row — the reverse of the order the
    /// sweep wrote them, and for the same reason. The two stores cannot be
    /// written atomically, so the intermediate state has to be one an operator
    /// can act on: with the flag cleared and the row still set, the artifact is
    /// listed on Ops with its restore button and the next press finishes the
    /// job. Clearing the row first loses the only page that offers the undo
    /// while the artifact is still hidden from search.
    pub async fn unsupersede(&self, artifact_id: &str) -> Result<()> {
        let _guard = self.lifecycle_lock.lock().await;
        self.unsupersede_locked(artifact_id).await
    }

    /// The body of `unsupersede`, entered with `lifecycle_lock` already held —
    /// `reactivate` routes here so a superseded artifact is not locked twice.
    async fn unsupersede_locked(&self, artifact_id: &str) -> Result<()> {
        // Read before clearing: afterwards nothing says who was hiding it.
        let winner = self.store.get_artifact(artifact_id).await?.superseded_by;
        // Marked before either store is touched. This direction writes the
        // payload first, so without it a crash between the two would leave
        // drift no row write ever announced.
        self.store.mark_lifecycle_dirty(artifact_id).await?;
        self.vectors
            .set_lifecycle(artifact_id, ArtifactStatus::Active, None)
            .await?;
        self.store.set_superseded_by(artifact_id, None).await?;
        // Both stores agree now, so the marker the row write set has nothing
        // left to describe. Cleared only here, never before the payload write.
        self.store
            .clear_lifecycle_dirty(std::slice::from_ref(&artifact_id.to_string()))
            .await?;
        // A restore out of a merge is an operator overruling the merge for
        // this one source. Recorded on the lineage, or the sweep's
        // unfinished-merge repair re-hides it on the next tick, every tick.
        if let Some(w) = winner
            && let Ok(wc) = self.store.get_artifact(&w).await
            && wc.provenance == crate::store::artifacts::Provenance::Merged
        {
            self.store.mark_source_restored(artifact_id).await?;
        }
        tracing::info!(artifact_id, "restored a superseded artifact to search");
        Ok(())
    }

    /// Move an already-hidden artifact from one winner to another.
    ///
    /// Not `supersede`, which refuses a side that is not active — and both sides
    /// here are exactly that: the artifact is already superseded, and it is
    /// being re-pointed precisely because its current winner is about to be
    /// hidden too.
    ///
    /// A supersession chain is what this exists to prevent. `A -> B -> C` leaves
    /// the reader who opens A at an artifact that is not in results either, and
    /// nothing in the UI can follow the second hop. The sweep avoids chains by
    /// grouping with union-find before it decides anything; the merge path
    /// cannot, because the group it is collapsing was already partly collapsed
    /// by an earlier merge.
    ///
    /// Row before payload, like `supersede`.
    pub async fn repoint_supersession(&self, artifact_id: &str, winner_id: &str) -> Result<()> {
        let _guard = self.lifecycle_lock.lock().await;
        let winner = self.store.get_artifact(winner_id).await?;
        if !winner.in_results() {
            return Err(Error::Validation(format!(
                "cannot re-point {artifact_id}: {winner_id} is not active"
            )));
        }
        self.store
            .set_superseded_by(artifact_id, Some(winner_id))
            .await?;
        self.vectors
            .set_lifecycle(artifact_id, ArtifactStatus::Superseded, Some(winner_id))
            .await?;
        self.store
            .clear_lifecycle_dirty(std::slice::from_ref(&artifact_id.to_string()))
            .await?;
        tracing::info!(artifact_id, winner_id, "re-pointed a supersession");
        Ok(())
    }

    /// Hide `loser_id` in favour of `winner_id`. Row before payload, matching
    /// the sweep's existing auto-supersede order (`jobs::consolidate`): the
    /// intermediate state after a partial failure is one an operator can act
    /// on, since the artifact is already listed on Ops with its `superseded_by`
    /// set, even if the search-side flag has not caught up yet.
    pub async fn supersede(&self, loser_id: &str, winner_id: &str) -> Result<()> {
        let _guard = self.lifecycle_lock.lock().await;
        // Neither side may be retired. `set_superseded_by` writes
        // `status = 'superseded'` unconditionally, so superseding an artifact
        // an operator deprecated would silently overwrite that decision with
        // one nothing distinguishes from the sweep's own work. And a deprecated
        // *winner* is worse: the loser gets hidden in favour of an artifact
        // that is itself out of results, so the answer disappears entirely.
        for (id, role) in [(loser_id, "loser"), (winner_id, "winner")] {
            let status = self.store.get_artifact(id).await?.status;
            if status != ArtifactStatus::Active {
                return Err(Error::Validation(format!(
                    "cannot supersede: {role} {id} is {}",
                    status.as_str()
                )));
            }
        }
        self.store
            .set_superseded_by(loser_id, Some(winner_id))
            .await?;
        self.vectors
            .set_lifecycle(loser_id, ArtifactStatus::Superseded, Some(winner_id))
            .await?;
        self.store
            .clear_lifecycle_dirty(std::slice::from_ref(&loser_id.to_string()))
            .await?;
        tracing::info!(loser_id, winner_id, "superseded an artifact");
        Ok(())
    }

    /// Flag an artifact stale with no specific replacement. Unlike `supersede`,
    /// there is no winning artifact on the other end — an operator judged the
    /// content itself no longer current.
    ///
    /// Row before payload. SQLite is the source of truth, so the state a
    /// partial failure leaves — row deprecated, still in results — is the one
    /// the sweep's drift repair (`jobs::consolidate::repair_lifecycle_drift`)
    /// finishes by pushing the row's status into the payload. Writing the
    /// payload first would instead hide the artifact behind a row that still
    /// says active, and the repair, reading the row, would undo it.
    pub async fn deprecate(&self, id: &str) -> Result<()> {
        let _guard = self.lifecycle_lock.lock().await;
        // A superseded artifact is already out of search, and `deprecate` does
        // not clear `superseded_by`, so this would leave a row that is both:
        // listed on Ops under "deprecated" *and* under "hidden as near
        // identical", with a detail page that renders only the supersession
        // branch and therefore never offers the button that undoes it. The
        // payload would carry `status: deprecated, superseded_by: <winner>`, a
        // combination nothing else in the system produces. The Ops page does not
        // render the button in that state, but the route is reachable directly
        // and this is a public API — so the rule lives here, next to
        // `supersede`'s own guard.
        if self.store.get_artifact(id).await?.superseded_by.is_some() {
            return Err(Error::Validation(format!(
                "cannot deprecate: {id} is already hidden in favour of another artifact; \
                 reactivate it first"
            )));
        }
        self.store
            .set_artifact_status(id, ArtifactStatus::Deprecated)
            .await?;
        self.vectors
            .set_lifecycle(id, ArtifactStatus::Deprecated, None)
            .await?;
        self.store
            .clear_lifecycle_dirty(std::slice::from_ref(&id.to_string()))
            .await?;
        tracing::info!(artifact_id = id, "deprecated an artifact");
        Ok(())
    }

    /// Move an artifact back to active.
    ///
    /// An artifact that was hidden by a supersession is handed to
    /// `unsupersede`, which clears `superseded_by` on both sides. Flipping the
    /// status alone would leave the row pointing at its winner while the
    /// payload no longer does, so Ops would keep listing it as hidden and the
    /// next consolidation sweep would re-apply `Superseded` to the vector
    /// store — undoing the operator without saying so.
    ///
    /// Payload before row, as `unsupersede` does it and for the same reason:
    /// this direction *reveals*, so the intermediate state has to be the
    /// visible one. Clearing the row first would leave the artifact hidden by a
    /// payload nothing on the page explains — off the Ops deprecated list,
    /// because the row now says active, and so out of reach of the very button
    /// that would fix it. With the payload cleared first the artifact is back
    /// in results immediately, still listed on Ops as deprecated, and one more
    /// press finishes the job.
    pub async fn reactivate(&self, id: &str) -> Result<()> {
        let _guard = self.lifecycle_lock.lock().await;
        if self.store.get_artifact(id).await?.superseded_by.is_some() {
            return self.unsupersede_locked(id).await;
        }
        // As in `unsupersede`: payload first, so the marker has to go first.
        self.store.mark_lifecycle_dirty(id).await?;
        self.vectors
            .set_lifecycle(id, ArtifactStatus::Active, None)
            .await?;
        self.store
            .set_artifact_status(id, ArtifactStatus::Active)
            .await?;
        self.store
            .clear_lifecycle_dirty(std::slice::from_ref(&id.to_string()))
            .await?;
        tracing::info!(artifact_id = id, "reactivated an artifact");
        Ok(())
    }

    /// Stamp an artifact as confirmed accurate now — what search ranking's
    /// recency decay reads.
    ///
    /// Also zeroes `hit_count`: `stale_max_hits` is "retrieved at most this
    /// many times *since*" the last verification, and a lifetime counter would
    /// mean one appearance in a marked search result kept an artifact off the
    /// review list forever.
    /// A failed vector write puts the row back the way it was. Nothing else
    /// reconciles this pair: `repair_lifecycle_drift` only examines artifacts
    /// that are non-active on one side or the other, so an artifact that is
    /// active on both but stamped on only one is invisible to it — it would keep
    /// ranking as stale and keep appearing on the review list while the row
    /// claimed it had just been confirmed, and the operator would have no way to
    /// tell that from a press that simply did not take. Rolling back leaves both
    /// stores saying the same (old) thing and surfaces the error, so pressing
    /// again is a repair rather than a guess.
    pub async fn verify(&self, id: &str) -> Result<()> {
        let at = now();
        let previous = self.store.get_artifact(id).await?.last_verified_at;
        self.store.set_last_verified_at(id, at).await?;
        if let Err(e) = self.vectors.set_last_verified_at(id, at, true).await {
            // A row that never had a stamp has nothing to roll back to, and
            // clearing the column is not available here; the drift it leaves is
            // the one the backfill already stamps.
            if let Some(previous) = previous
                && let Err(undo) = self.store.set_last_verified_at(id, previous).await
            {
                tracing::warn!(
                    artifact_id = id,
                    error = %undo,
                    "could not undo the verification stamp; sqlite now claims a \
                     verification the vector store did not record"
                );
            }
            return Err(e);
        }
        tracing::info!(artifact_id = id, "verified an artifact");
        Ok(())
    }

    /// Push every artifact's SQLite-side lifecycle state (source of truth) into
    /// the vector store. Runs automatically at startup when any point is
    /// missing its stamp, and on demand via `--backfill-lifecycle` — existing
    /// points have no `status`/`last_verified_at` until it does, which every
    /// filter safely treats as active in the meantime, just not yet filterable
    /// as deprecated.
    ///
    /// Batched, and restartable by construction: the work is idempotent and
    /// driven by a list SQLite regenerates, so a run that dies halfway is
    /// resumed simply by running it again. One artifact that cannot be read is
    /// logged and skipped rather than abandoning every artifact after it —
    /// a single missing row must not be what keeps a base unbackfilled.
    ///
    /// Finishes with `heal_store_drift`, so a point whose row is gone gets the
    /// row back and is stamped by the next pass, rather than sitting unstamped
    /// forever and re-triggering this one on every start.
    pub async fn backfill_lifecycle(&self) -> Result<usize> {
        const BATCH: usize = 256;
        let ids = self.store.list_all_artifact_ids().await?;
        let total = ids.len();
        let mut n = 0;
        for chunk in ids.chunks(BATCH) {
            let mut rows = Vec::with_capacity(chunk.len());
            for id in chunk {
                match self.store.get_artifact(id).await {
                    Ok(c) => rows.push(crate::vector::LifecycleRow {
                        artifact_id: c.id.clone(),
                        status: c.status,
                        superseded_by: c.superseded_by.clone(),
                        last_verified_at: c.last_verified_at.unwrap_or(c.created_at),
                    }),
                    Err(e) => {
                        tracing::warn!(artifact_id = %id, error = %e, "skipped in the lifecycle backfill");
                    }
                }
            }
            self.vectors.apply_lifecycle(&rows).await?;
            n += rows.len();
            tracing::info!(done = n, total, "lifecycle backfill progress");
        }
        self.heal_store_drift().await?;
        // The two-sided scan runs here and nowhere else. It is O(hidden
        // artifacts), which autonomous merging makes grow without bound, so it
        // stopped being something the sweep can afford on every tick — but a
        // backfill is exactly the moment to catch drift with no SQLite write
        // behind it: a payload edited out of band, or a base that predates
        // `lifecycle_dirty` and therefore has interrupted writes nothing marked.
        match crate::jobs::consolidate::full_lifecycle_reconcile(self).await {
            Ok(0) => {}
            Ok(fixed) => tracing::info!(fixed, "reconciled lifecycle drift the marker never saw"),
            Err(e) => tracing::warn!(error = %e, "could not run the full lifecycle reconcile"),
        }
        tracing::info!(n, "backfilled lifecycle fields into the vector store");
        Ok(n)
    }

    /// Make the two stores agree about which artifacts exist, by restoring
    /// whichever side is missing one — never by deleting.
    ///
    /// The two stores hold complementary halves of the same artifact and are
    /// written separately, so either can end up with an entry the other lacks: a
    /// crash between the two writes, a restore of one store from a backup taken
    /// at a different moment, an operator pointing a process at the wrong
    /// `store.path`. Each direction has its own repair, and neither destroys
    /// anything:
    ///
    /// - A point with no row rebuilds the row from the payload (see
    ///   `Store::restore_artifact`) and queues a re-embed. The vector store
    ///   carries the text, the title, the tags and the lifecycle stamps, so the
    ///   artifact comes back searchable and correctly retired if it was retired.
    /// - A row that says `done` with no point queues a re-embed, which writes
    ///   the point through the ordinary pipeline. A row still `pending` is not
    ///   drift at all — it is every artifact that was just ingested.
    ///
    /// Deleting used to be this method's whole job, and it was a loaded gun. It
    /// ran unconditionally from the backfill, which startup spawns in the
    /// background, so a process started against an empty or wrong SQLite file
    /// found every point orphaned and emptied the collection — no confirmation,
    /// no dry run, and nothing to reindex from afterwards, because the vectors
    /// were the last copy. Restoring is the safe direction of the same repair:
    /// the worst a wrong `store.path` can now do is fill that database with the
    /// artifacts it was missing.
    ///
    /// What this costs is that a deletion interrupted between its two writes
    /// comes back instead of finishing. That is the deliberate trade — an
    /// artifact that reappears is a nuisance an operator can fix with the delete
    /// button, and an artifact that is gone is gone. See `Core::delete_artifact`
    /// for the deliberate path.
    ///
    /// The read order matters. The point list is read *first* and the row list
    /// second, so an artifact captured while this runs is either absent from the
    /// scroll or present in the newer row list — never mistaken for an orphan
    /// point and restored on top of itself.
    pub async fn heal_store_drift(&self) -> Result<StoreDrift> {
        use std::collections::{BTreeMap, HashSet};

        let points = self.vectors.all_artifact_ids().await?;
        let rows = self.store.list_all_artifact_ids().await?;
        let embedded = self.store.list_embedded_artifact_ids().await?;

        let has_row: HashSet<&str> = rows.iter().map(String::as_str).collect();
        let has_point: HashSet<&str> = points.iter().map(String::as_str).collect();

        let mut out = StoreDrift::default();

        // Vector store has it, SQLite does not: rebuild the row.
        let orphan_points: Vec<String> = points
            .iter()
            .filter(|id| !has_row.contains(id.as_str()))
            .cloned()
            .collect();
        if !orphan_points.is_empty() {
            let payloads = self.vectors.payloads_of(&orphan_points).await?;
            // Grouped by corpus so a placeholder parent, if one is needed, is
            // written once and holds every artifact restored under it rather
            // than only whichever happened to come first.
            let mut by_corpus: BTreeMap<&str, Vec<&crate::vector::VectorPayload>> = BTreeMap::new();
            for id in &orphan_points {
                match payloads.get(id) {
                    Some(p) => by_corpus.entry(p.corpus_id.as_str()).or_default().push(p),
                    // The point was there for the id scroll and gone for the
                    // retrieve, or its payload does not parse as a chunk.
                    // Neither is an error and neither is restorable.
                    None => tracing::debug!(
                        artifact_id = %id,
                        "no readable payload for an artifact the vector store listed"
                    ),
                }
            }
            for (corpus_id, group) in by_corpus {
                if self.store.get_corpus(corpus_id).await.is_err() {
                    let joined = group
                        .iter()
                        .map(|p| p.text.as_str())
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    if self
                        .store
                        .ensure_restored_corpus(corpus_id, &joined)
                        .await?
                    {
                        out.corpora_restored += 1;
                        tracing::info!(
                            corpus_id,
                            artifacts = group.len(),
                            "the source of a restored artifact was not stored; \
                             wrote a placeholder so the artifact has a parent"
                        );
                    }
                }
                for p in group {
                    // A payload with no `provenance` key predates merging, and
                    // nothing that predates merging is merged — so `captured` is
                    // the right reading rather than a guess.
                    let provenance = p
                        .provenance
                        .as_deref()
                        .map(crate::store::artifacts::Provenance::parse)
                        .unwrap_or(crate::store::artifacts::Provenance::Captured);
                    let restored = crate::store::artifacts::RestoredArtifact {
                        id: p.artifact_id.clone(),
                        // A merged artifact carries "" here, which is not a
                        // corpus that exists; writing it would fail the foreign
                        // key. The kind above is what says which case this is.
                        corpus_id: match provenance {
                            crate::store::artifacts::Provenance::Merged => None,
                            crate::store::artifacts::Provenance::Captured => {
                                Some(p.corpus_id.clone())
                            }
                        },
                        provenance,
                        text: p.text.clone(),
                        title: p.title.clone(),
                        category: p.category.clone(),
                        tags: p.tags.clone(),
                        created_at: p.created_at,
                        // A payload with no `status` key predates lifecycle
                        // tracking, which every filter reads as active — so the
                        // restored row has to say the same thing, or the heal
                        // would quietly retire artifacts on an unbackfilled base.
                        status: p.status.unwrap_or(ArtifactStatus::Active),
                        last_verified_at: p.last_verified_at,
                        superseded_by: p.superseded_by.clone(),
                    };
                    if self.store.restore_artifact(&restored).await? {
                        out.rows_restored += 1;
                        // A payload records neither `source_count` nor lineage
                        // rows, so a restored merge definitionally cannot
                        // support its provenance claim — and
                        // `merged_missing_a_source` (0 > 0) will never say so.
                        // Said here instead, with the flag that pass would
                        // have set. `roots_of` already refuses to hand such a
                        // merge back as its own root; this is the half an
                        // operator sees.
                        if provenance == crate::store::artifacts::Provenance::Merged {
                            self.store
                                .set_artifact_flags(
                                    &p.artifact_id,
                                    &["orphaned_source".to_string()],
                                    Some(
                                        "restored from the index; the record of its \
                                         sources was lost",
                                    ),
                                )
                                .await?;
                        }
                        self.store
                            .enqueue(Stage::Embed, "artifact", &p.artifact_id)
                            .await?;
                    }
                }
            }
        }

        // SQLite says the vector was written and the vector store has nothing:
        // send it back through the pipeline that writes it.
        for id in embedded
            .iter()
            .filter(|id| !has_point.contains(id.as_str()))
        {
            self.store.enqueue(Stage::Embed, "artifact", id).await?;
            out.points_requeued += 1;
        }

        if out.rows_restored > 0 || out.points_requeued > 0 {
            tracing::info!(
                rows_restored = out.rows_restored,
                corpora_restored = out.corpora_restored,
                points_requeued = out.points_requeued,
                "the two stores disagreed about which artifacts exist; restored both ways"
            );
        }
        Ok(out)
    }

    /// Delete an artifact from both stores, on purpose.
    ///
    /// The vector point goes first. `heal_store_drift` restores a row from a
    /// surviving point, so deleting the row first would mean an interrupted
    /// delete is undone by the very next heal; this way the interrupted state is
    /// a row whose point is gone, which the heal answers by re-embedding — the
    /// artifact survives intact instead of half-existing, and pressing delete
    /// again finishes it.
    pub async fn delete_artifact(&self, id: &str) -> Result<()> {
        self.store.get_artifact(id).await?;
        self.vectors
            .delete_artifacts(std::slice::from_ref(&id.to_string()))
            .await?;
        self.store.delete_artifact(id).await?;
        // This artifact may have been the reason another one is hidden, and a
        // keeper that no longer exists leaves its loser out of search in favour
        // of nothing.
        self.heal_dangling_supersessions().await?;
        tracing::info!(artifact_id = id, "deleted an artifact");
        Ok(())
    }

    /// Put back anything that was hidden in favour of an artifact which has
    /// since been deleted. Cheap — one query, and vector writes only for the
    /// rows it actually frees — so it runs after every deletion.
    ///
    /// Payload before row, exactly as `unsupersede` does it, and for the same
    /// reason. Clearing the rows first would leave every artifact whose vector
    /// write then failed hidden from search with `superseded_by` already NULL:
    /// off the Ops list, past the sweep's self-heal branch — which only repairs
    /// the opposite skew — and unreachable by any button. This runs first in a
    /// sweep, when Qdrant being unavailable is precisely the case at hand.
    ///
    /// One failure does not abandon the rest: each artifact is independent, and
    /// the ones that can be freed should be. The state a failure leaves behind
    /// is the recoverable one, so the next deletion or sweep finishes the job.
    pub(crate) async fn heal_dangling_supersessions(&self) -> Result<()> {
        // Under the lifecycle lock like every other transition: this path
        // reveals payload-first, and interleaving with the sweep's repair —
        // which reads rows and writes payloads — is exactly the sequence that
        // hides an artifact with no marker left to find it by.
        let _guard = self.lifecycle_lock.lock().await;
        let mut first_err = None;
        for id in self.store.dangling_superseded().await? {
            // Marked before the payload write, as `unsupersede` does and for
            // the same reason: this direction writes the payload first, so
            // without it a crash between the two stores would leave drift no
            // row write ever announced. A payload write that fails below
            // leaves the marker standing — correctly: the failed reveal is
            // then findable drift, and the heal retries on the next sweep.
            self.store.mark_lifecycle_dirty(&id).await?;
            if let Err(e) = self
                .vectors
                .set_lifecycle(&id, ArtifactStatus::Active, None)
                .await
            {
                tracing::warn!(
                    artifact_id = %id,
                    error = %e,
                    "could not clear the hidden flag; the artifact stays listed on Ops"
                );
                first_err.get_or_insert(e);
                continue;
            }
            self.store.set_superseded_by(&id, None).await?;
            // Cleared here as everywhere else, because `set_superseded_by`
            // raises the marker in the same statement as the change it
            // describes. This path writes the payload first and the row second,
            // so by now the two already agree — and a marker left standing would
            // have the next sweep's `repair_lifecycle_drift` rewrite a payload
            // that is correct, on a base whose stores are in agreement, which is
            // the one condition that function's own comment says would mean a
            // bug somewhere.
            self.store
                .clear_lifecycle_dirty(std::slice::from_ref(&id))
                .await?;
            tracing::info!(
                artifact_id = %id,
                "restored an artifact whose surviving copy was deleted"
            );
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Vectors first: an orphaned row is invisible, but an orphaned vector is
    /// still returned by search.
    pub async fn delete_corpus(&self, id: &str) -> Result<()> {
        self.store.get_corpus(id).await?;
        self.vectors.delete_by_corpus(id).await?;
        self.store.delete_corpus(id).await?;
        self.heal_dangling_supersessions().await?;
        tracing::info!(corpus_id = %id, "deleted source and its vectors");
        Ok(())
    }

    pub async fn reprocess(&self, id: &str, stage: Stage) -> Result<()> {
        let src = self.store.get_corpus(id).await?;
        match stage {
            Stage::Synthesize | Stage::Enrich => {
                // Re-segmenting replaces every chunk, so the old vectors and
                // rows go first.
                self.vectors.delete_by_corpus(&src.id).await?;
                for c in self.store.artifacts_for_corpus(&src.id).await? {
                    self.store.delete_artifact(&c.id).await?;
                }
                // The window rows are the segment job's memory of what it has
                // already done. Leaving them behind means the rerun finds every
                // window `done`, segments nothing, and lands on a source with
                // no chunks at all. Re-windowing is also the point of a
                // reprocess after a model or budget change.
                self.store.clear_segments(&src.id).await?;
                // The measure those windows produced goes with them. It is not
                // just stale: the reconciliation sweep identifies a document
                // whose last window resolved but whose `settle` never ran by
                // having no coverage, so a value left over from the previous run
                // reads as "already finished". A rerun that dies in exactly that
                // window would then be stuck in `segmenting` for good — nothing
                // resolves again to trigger `settle`, and the one sweep that
                // repairs it has been told there is nothing to repair.
                self.store.clear_corpus_coverage(&src.id).await?;
                // And the units that name those windows, which outlive them.
                // Planning arms idle-only, so a unit still queued from the run
                // being replaced would carry its attempts into the rerun — the
                // person who asked for another try would get a window that gives
                // up after one.
                self.store.delete_window_jobs(&src.id).await?;
                // The title unit is armed once per corpus and never again, so
                // that a document the model will not name stops costing calls.
                // The row is what remembers that, which also means a corpus left
                // unnamed by a transient failure could never be named again —
                // including by the person who noticed and asked for the rerun.
                // An explicit reprocess is exactly the case that rule is not
                // meant to cover.
                self.store.delete_job(Stage::Title, &src.id).await?;
                // Reprocessing a parked capture is a decision to process it, so
                // the park has to be lifted with it. Leaving the flag set means
                // a fully synthesized and embedded corpus sits on the review
                // queue forever, where the discard button now deletes real work.
                self.store.set_near_dupe(&src.id, None, None).await?;
                self.store
                    .set_corpus_status(&src.id, CorpusStatus::Raw)
                    .await?;
                // The artifacts just deleted may have been the surviving half of
                // a consolidated pair; whatever they hid comes back.
                self.heal_dangling_supersessions().await?;
                self.store
                    .enqueue(Stage::Synthesize, "corpus", &src.id)
                    .await?;
            }
            Stage::Embed => {
                self.store.reset_embed_state(&src.id).await?;
                self.store.enqueue(Stage::Embed, "corpus", &src.id).await?;
                self.store
                    .set_corpus_status(&src.id, CorpusStatus::Embedding)
                    .await?;
            }
            // Consolidation looks at the whole collection, so there is no such
            // thing as reprocessing one corpus through it. Saying so beats
            // silently queueing a sweep the caller did not ask for.
            Stage::Consolidate => {
                return Err(Error::Validation(
                    "consolidate is a collection-wide sweep, not a per-corpus stage".into(),
                ));
            }
            // Units the queue arms for itself, one per artifact or inference
            // call. An operator reprocesses a document, not one of its windows:
            // asking for `synthesize` re-windows the whole thing and arms them
            // all, and `embed` re-arms a `relate` unit per artifact behind it.
            Stage::SegmentWindow | Stage::Title | Stage::Dedupe | Stage::Relate => {
                return Err(Error::Validation(
                    "that stage is a single inference call the queue arms itself; \
                     reprocess the document instead"
                        .into(),
                ));
            }
            // Re-reading a stored image with a different model is a later
            // feature; the original is kept so it stays possible.
            Stage::Describe => {
                return Err(Error::Validation(
                    "re-reading an image is not supported yet; capture it again".into(),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::core::ingest::{
        Capture, ImageCapture, MAX_NOTE_CHARS, NearDupeAction, ORIGIN_IMAGE, StoreDrift,
    };
    use crate::core::test_support::{test_core, test_core_with_failing_synthesizer};
    use crate::error::Error;
    use crate::store::artifacts::EmbedState;
    use crate::store::corpora::CorpusStatus;
    use crate::store::jobs::Stage;

    #[tokio::test]
    async fn healing_a_dangling_supersession_leaves_no_unfinished_write_behind() {
        // `set_superseded_by` raises `lifecycle_dirty` in the same statement as
        // the change it describes, and every other caller clears it once the
        // payload write is acknowledged. This path writes the payload *first*,
        // so the two stores already agree by the time the row is written — and a
        // marker left standing has the next sweep's `repair_lifecycle_drift`
        // rewrite a payload that is already correct, on a base in agreement.
        // That function returns a count precisely so that a repair firing on
        // such a base is visible rather than hiding behind a correct end state.
        let core = test_core().await;
        let ids = crate::jobs::consolidate::tests::seed(
            &core,
            &[("first", [1.0, 0.0]), ("second", [0.9999, 0.01])],
        )
        .await;
        core.supersede(&ids[0], &ids[1]).await.unwrap();
        assert!(
            core.store
                .dirty_lifecycle_artifacts(10)
                .await
                .unwrap()
                .is_empty(),
            "supersede itself left the write marked as unfinished"
        );

        // Through `Core`, not the store: the heal hangs off this deletion.
        core.delete_artifact(&ids[1]).await.unwrap();

        assert!(
            core.store
                .get_artifact(&ids[0])
                .await
                .unwrap()
                .superseded_by
                .is_none(),
            "the artifact still points at a keeper that is gone"
        );
        assert!(
            core.store
                .dirty_lifecycle_artifacts(10)
                .await
                .unwrap()
                .is_empty(),
            "the healed artifact is still marked as an unfinished write"
        );
    }

    #[tokio::test]
    async fn a_capture_remembers_where_it_came_from() {
        let core = test_core().await;
        let out = core
            .ingest_capture(
                crate::core::ingest::Capture::new("alpha para\n\nbeta para", "extension")
                    .with_source_url(Some("https://example.test/notes".into())),
            )
            .await
            .unwrap();
        let src = core.store.get_corpus(&out.id).await.unwrap();
        // The channel and the location are two different facts. Overloading
        // one with the other loses the channel and leaves the URL unqueryable.
        assert_eq!(src.origin, "extension");
        assert_eq!(
            src.source_url.as_deref(),
            Some("https://example.test/notes")
        );
    }

    #[tokio::test]
    async fn an_ordinary_capture_has_no_source_url() {
        let core = test_core().await;
        let out = core.ingest("alpha\n\nbeta", "web", None).await.unwrap();
        let src = core.store.get_corpus(&out.id).await.unwrap();
        assert_eq!(src.source_url, None);
    }

    /// A body long enough to have a stable shingle signature.
    fn manual(marker: &str) -> String {
        (0..200)
            .map(|i| format!("step {i}: run the {marker} command and read its output"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn reprocessing_forgets_the_previous_runs_coverage() {
        // The reconciliation sweep identifies a document whose last window
        // resolved but whose `settle` never ran by its having no coverage. A
        // reprocess that left the previous run's measure behind made its own
        // rerun the one case the repair could not see: if that rerun died in
        // exactly that window, nothing would resolve again to trigger `settle`,
        // and the sweep would read the stale number as "already finished". The
        // document sits in `segmenting` for good — never renumbered, never
        // embedded, never in search.
        let core = test_core().await;
        let out = core
            .ingest("alpha para\n\nbeta para", "web", None)
            .await
            .unwrap();
        core.store.set_corpus_coverage(&out.id, 0.87).await.unwrap();

        core.reprocess(&out.id, Stage::Synthesize).await.unwrap();

        assert!(
            core.store
                .get_corpus(&out.id)
                .await
                .unwrap()
                .coverage
                .is_none(),
            "the rerun carried the previous run's coverage, hiding it from the repair sweep"
        );
    }

    #[tokio::test]
    async fn reprocessing_gives_every_window_its_attempts_back() {
        // Planning arms idle-only now, so the window units are the one piece of
        // the previous run that a reprocess would otherwise inherit: a rerun
        // asked for by a person would start its windows four attempts in and
        // give up on them almost at once. `clear_segments` drops the rows the
        // units name but not the units, which outlive them.
        let core = test_core().await;
        let out = core
            .ingest("alpha para\n\nbeta para", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::plan(&core, &out.id).await.unwrap();
        sqlx::query("UPDATE jobs SET attempts = 4 WHERE stage = 'segment_window'")
            .execute(&core.store.pool)
            .await
            .unwrap();

        core.reprocess(&out.id, Stage::Synthesize).await.unwrap();
        crate::jobs::synthesize::plan(&core, &out.id).await.unwrap();

        let attempts: Vec<i64> =
            sqlx::query_scalar("SELECT attempts FROM jobs WHERE stage = 'segment_window'")
                .fetch_all(&core.store.pool)
                .await
                .unwrap();
        assert!(
            !attempts.is_empty(),
            "the rerun should have armed its windows again"
        );
        assert!(
            attempts.iter().all(|&a| a == 0),
            "the rerun inherited the previous run's attempts: {attempts:?}"
        );
    }

    #[tokio::test]
    async fn reprocessing_gives_a_corpus_that_was_never_named_another_chance() {
        // A title unit is armed once per corpus and never again, so that a
        // document the model will not name stops costing four calls a day
        // forever. The row is what remembers that — and it outlived a
        // reprocess, so a corpus left unnamed by an endpoint that was briefly
        // down could never be named again, including by the person who noticed
        // and asked for the rerun.
        let core = test_core().await;
        let src = core.ingest(&manual("mount"), "web", None).await.unwrap();
        for _ in 0..200 {
            sqlx::query("UPDATE jobs SET run_after = 0 WHERE state = 'pending'")
                .execute(&core.store.pool)
                .await
                .unwrap();
            if !crate::jobs::run_one(&core).await.unwrap_or(false) {
                break;
            }
        }
        assert!(
            core.store.has_job(Stage::Title, &src.id).await.unwrap(),
            "the fixture must arm a title unit"
        );

        core.reprocess(&src.id, Stage::Synthesize).await.unwrap();

        assert!(
            !core.store.has_job(Stage::Title, &src.id).await.unwrap(),
            "reprocess left the spent title unit behind, so the corpus can never be named"
        );
    }

    #[tokio::test]
    async fn a_near_identical_capture_is_parked_rather_than_synthesised() {
        // The whole point: a re-pasted chapter must not cost a model call, and
        // must not become a second set of artifacts competing with the first.
        let core = test_core().await;
        let first = core.ingest(&manual("mount"), "web", None).await.unwrap();
        while core.store.claim_job().await.unwrap().is_some() {}

        let edited = manual("mount").replacen("step 7:", "step seven:", 1);
        let second = core.ingest(&edited, "web", None).await.unwrap();

        assert_ne!(second.id, first.id, "the capture must still be stored");
        assert!(!second.duplicate, "it is not a byte-identical duplicate");
        assert_eq!(second.status, CorpusStatus::NeedsReview);
        let near = second.near_duplicate.expect("no near-duplicate reported");
        assert_eq!(near.corpus_id, first.id);
        assert!(near.similarity > 0.90);
        assert!(
            core.store.claim_job().await.unwrap().is_none(),
            "a parked capture must not queue synthesis"
        );
    }

    #[tokio::test]
    async fn an_ordinary_capture_is_unaffected() {
        let core = test_core().await;
        core.ingest(&manual("mount"), "web", None).await.unwrap();
        while core.store.claim_job().await.unwrap().is_some() {}

        let out = core.ingest(&manual("pastry"), "web", None).await.unwrap();
        assert_eq!(out.status, CorpusStatus::Raw);
        assert!(out.near_duplicate.is_none());
        assert!(
            core.store.claim_job().await.unwrap().is_some(),
            "an unrelated capture must still queue synthesis"
        );
    }

    #[tokio::test]
    async fn keeping_both_releases_the_capture_into_the_pipeline() {
        let core = test_core().await;
        core.ingest(&manual("mount"), "web", None).await.unwrap();
        while core.store.claim_job().await.unwrap().is_some() {}
        let second = core
            .ingest(
                &manual("mount").replacen("step 7:", "step seven:", 1),
                "web",
                None,
            )
            .await
            .unwrap();

        core.resolve_near_duplicate(&second.id, NearDupeAction::KeepBoth)
            .await
            .unwrap();

        let got = core.store.get_corpus(&second.id).await.unwrap();
        assert_eq!(got.status, CorpusStatus::Raw);
        assert!(got.near_dupe_of.is_none(), "the flag must be cleared");
        assert!(core.store.claim_job().await.unwrap().is_some());
    }

    #[tokio::test]
    async fn replacing_deletes_the_older_corpus_and_its_vectors() {
        let core = test_core().await;
        let first = core.ingest(&manual("mount"), "web", None).await.unwrap();
        while crate::jobs::run_one(&core).await.unwrap() {}
        assert!(core.vectors.count().await.unwrap() > 0);

        let second = core
            .ingest(
                &manual("mount").replacen("step 7:", "step seven:", 1),
                "web",
                None,
            )
            .await
            .unwrap();
        core.resolve_near_duplicate(&second.id, NearDupeAction::Replace)
            .await
            .unwrap();

        assert!(matches!(
            core.store.get_corpus(&first.id).await,
            Err(crate::error::Error::NotFound)
        ));
        assert_eq!(
            core.store.get_corpus(&second.id).await.unwrap().status,
            CorpusStatus::Raw
        );
        assert!(core.store.claim_job().await.unwrap().is_some());
    }

    #[tokio::test]
    async fn discarding_removes_the_new_capture_only() {
        let core = test_core().await;
        let first = core.ingest(&manual("mount"), "web", None).await.unwrap();
        while core.store.claim_job().await.unwrap().is_some() {}
        let second = core
            .ingest(
                &manual("mount").replacen("step 7:", "step seven:", 1),
                "web",
                None,
            )
            .await
            .unwrap();

        core.resolve_near_duplicate(&second.id, NearDupeAction::Discard)
            .await
            .unwrap();

        assert!(matches!(
            core.store.get_corpus(&second.id).await,
            Err(crate::error::Error::NotFound)
        ));
        assert!(core.store.get_corpus(&first.id).await.is_ok());
    }

    #[tokio::test]
    async fn replacing_a_corpus_that_is_already_gone_still_releases_the_capture() {
        // `near_dupe_of` can name a corpus that has since been deleted —
        // including another parked capture that was discarded, since a parked
        // corpus is still matchable. Failing here put the only way out of the
        // review queue behind a 404.
        let core = test_core().await;
        let first = core.ingest(&manual("mount"), "web", None).await.unwrap();
        while core.store.claim_job().await.unwrap().is_some() {}
        let second = core
            .ingest(
                &manual("mount").replacen("step 7:", "step seven:", 1),
                "web",
                None,
            )
            .await
            .unwrap();
        core.delete_corpus(&first.id).await.unwrap();

        core.resolve_near_duplicate(&second.id, NearDupeAction::Replace)
            .await
            .unwrap();

        let got = core.store.get_corpus(&second.id).await.unwrap();
        assert_eq!(got.status, CorpusStatus::Raw);
        assert!(got.near_dupe_of.is_none());
        assert!(core.store.claim_job().await.unwrap().is_some());
    }

    #[tokio::test]
    async fn reprocessing_a_parked_capture_takes_it_off_the_review_queue() {
        // Reprocessing is a decision to process. Leaving the park set left a
        // fully synthesized corpus on the queue where "discard" deletes it.
        let core = test_core().await;
        core.ingest(&manual("mount"), "web", None).await.unwrap();
        while core.store.claim_job().await.unwrap().is_some() {}
        let second = core
            .ingest(
                &manual("mount").replacen("step 7:", "step seven:", 1),
                "web",
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            core.store.get_corpus(&second.id).await.unwrap().status,
            CorpusStatus::NeedsReview
        );

        core.reprocess(&second.id, Stage::Synthesize).await.unwrap();

        let got = core.store.get_corpus(&second.id).await.unwrap();
        assert!(
            got.near_dupe_of.is_none(),
            "the capture is being processed and still asks to be decided on"
        );
        assert_eq!(got.status, CorpusStatus::Raw);
    }

    #[tokio::test]
    async fn resolving_a_corpus_that_is_not_parked_is_rejected() {
        let core = test_core().await;
        let out = core.ingest("ordinary text", "web", None).await.unwrap();
        assert!(matches!(
            core.resolve_near_duplicate(&out.id, NearDupeAction::Replace)
                .await,
            Err(crate::error::Error::Validation(_))
        ));
    }

    #[tokio::test]
    async fn ingest_returns_immediately_and_enqueues_segmentation() {
        let core = test_core().await;
        let out = core
            .ingest("a procedure", "web", Some("title"))
            .await
            .unwrap();
        assert_eq!(out.status, CorpusStatus::Raw);
        assert!(!out.duplicate);

        let src = core.store.get_corpus(&out.id).await.unwrap();
        assert_eq!(src.raw_text, "a procedure");
        assert!(
            core.store.claim_job().await.unwrap().is_some(),
            "segment job was not enqueued"
        );
    }

    #[tokio::test]
    async fn ingest_is_not_blocked_by_a_dead_chunker() {
        // The whole point of deferred processing: a broken inference endpoint
        // must not turn into a failed capture.
        let core = test_core_with_failing_synthesizer().await;
        let out = core.ingest("still accepted", "web", None).await.unwrap();
        assert_eq!(out.status, CorpusStatus::Raw);
    }

    #[tokio::test]
    async fn identical_text_returns_the_existing_source() {
        let core = test_core().await;
        let a = core.ingest("same words", "web", None).await.unwrap();
        let b = core.ingest("same words", "mcp", None).await.unwrap();
        assert_eq!(a.id, b.id);
        assert!(b.duplicate);
        assert_eq!(core.store.list_corpora(10, 0).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn two_captures_of_the_same_text_at_once_both_get_an_answer() {
        // The duplicate check and the insert are separated by a scan over every
        // stored signature, so a double-submitted form is enough to have both
        // calls pass the check. The loser used to hit the UNIQUE constraint and
        // surface as a 500 on a capture that was, in fact, a duplicate.
        let core = test_core().await;
        let (a, b) = tokio::join!(
            core.ingest("filed twice at once", "web", None),
            core.ingest("filed twice at once", "mcp", None),
        );
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_eq!(a.id, b.id);
        assert!(
            a.duplicate ^ b.duplicate,
            "exactly one of the two should be the duplicate: {a:?} {b:?}"
        );
        assert_eq!(core.store.list_corpora(10, 0).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn empty_text_is_rejected() {
        let core = test_core().await;
        assert!(matches!(
            core.ingest("   \n ", "web", None).await,
            Err(crate::error::Error::Validation(_))
        ));
    }

    #[tokio::test]
    async fn deleting_a_source_also_deletes_its_vectors() {
        let core = test_core().await;
        let out = core.ingest("some text", "web", None).await.unwrap();
        let chunks = core
            .store
            .insert_artifacts(
                &out.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: "t".into(),
                    corpus_span: None,
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: None,
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();
        core.vectors
            .upsert(vec![crate::vector::VectorPoint {
                sparse: Default::default(),
                vector: vec![0.1; 8],
                payload: crate::vector::VectorPayload {
                    artifact_id: chunks[0].id.clone(),
                    corpus_id: out.id.clone(),
                    text: "t".into(),
                    title: None,
                    category: None,
                    tags: vec![],
                    created_at: 0,
                    last_seen_at: None,
                    hit_count: None,
                    superseded: None,
                    status: None,
                    last_verified_at: None,
                    superseded_by: None,
                    provenance: None,
                },
            }])
            .await
            .unwrap();

        core.delete_corpus(&out.id).await.unwrap();
        assert_eq!(
            core.vectors.count().await.unwrap(),
            0,
            "orphaned vectors would still be searchable"
        );
        assert!(matches!(
            core.store.get_corpus(&out.id).await,
            Err(crate::error::Error::NotFound)
        ));
    }

    #[tokio::test]
    async fn deleting_a_missing_source_is_not_found() {
        let core = test_core().await;
        assert!(matches!(
            core.delete_corpus("nope").await,
            Err(crate::error::Error::NotFound)
        ));
    }

    #[tokio::test]
    async fn reprocess_requeues_the_requested_stage() {
        let core = test_core().await;
        let out = core.ingest("text", "web", None).await.unwrap();
        core.store.claim_job().await.unwrap(); // drain the initial segment job
        core.reprocess(&out.id, Stage::Synthesize).await.unwrap();
        let j = core.store.claim_job().await.unwrap().unwrap();
        assert_eq!(j.target_id, out.id);
        assert_eq!(
            j.attempts, 1,
            "reprocess must start from a clean attempt count"
        );
    }

    #[tokio::test]
    async fn reprocessing_re_segments_instead_of_finding_every_window_done() {
        // The window rows say what the segment job has already finished.
        // Keeping them across a reprocess left the rerun with nothing pending,
        // no chunks, and a source marked failed.
        let core = test_core().await;
        let out = core
            .ingest("alpha para\n\nbeta para", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        let first = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .len();
        assert!(first > 0);

        core.reprocess(&out.id, Stage::Synthesize).await.unwrap();
        assert!(
            core.store
                .segments_for_corpus(&out.id)
                .await
                .unwrap()
                .is_empty(),
            "reprocess must forget the windowing so it can be redone"
        );

        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        assert_eq!(
            core.store
                .artifacts_for_corpus(&out.id)
                .await
                .unwrap()
                .len(),
            first,
            "the rerun must produce the chunks again, not an empty source"
        );
        assert_ne!(
            core.store.get_corpus(&out.id).await.unwrap().status,
            CorpusStatus::Failed
        );
    }

    /// One point for `artifact_id`, with nothing set beyond what a write needs.
    fn point(artifact_id: &str, corpus_id: &str) -> crate::vector::VectorPoint {
        crate::vector::VectorPoint {
            sparse: Default::default(),
            vector: vec![0.1; 8],
            payload: crate::vector::VectorPayload {
                artifact_id: artifact_id.to_string(),
                corpus_id: corpus_id.to_string(),
                text: "t".into(),
                title: None,
                category: None,
                tags: vec![],
                created_at: 0,
                last_seen_at: None,
                hit_count: None,
                superseded: None,
                status: None,
                last_verified_at: None,
                superseded_by: None,
                provenance: None,
            },
        }
    }

    /// A corpus with one artifact in it, and the ids of both.
    async fn one_artifact(core: &crate::core::Core) -> (String, String) {
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: "t".into(),
                    corpus_span: None,
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: None,
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();
        (src.id, made[0].id.clone())
    }

    #[tokio::test]
    async fn deprecating_an_already_superseded_artifact_is_refused() {
        // `set_artifact_status` leaves `superseded_by` alone, so this used to
        // produce a row that is deprecated *and* hidden in favour of a winner:
        // listed in both Ops tables, rendered only as a supersession, and so out
        // of reach of the button that would undo it.
        let core = test_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[
                    crate::store::artifacts::NewArtifact {
                        ordinal: 0,
                        text: "loser".into(),
                        corpus_span: None,
                        title: None,
                        category: None,
                        tags: vec![],
                        segment_idx: None,
                        caveats: vec![],
                    },
                    crate::store::artifacts::NewArtifact {
                        ordinal: 1,
                        text: "winner".into(),
                        corpus_span: None,
                        title: None,
                        category: None,
                        tags: vec![],
                        segment_idx: None,
                        caveats: vec![],
                    },
                ],
            )
            .await
            .unwrap();
        core.supersede(&made[0].id, &made[1].id).await.unwrap();

        assert!(matches!(
            core.deprecate(&made[0].id).await,
            Err(crate::error::Error::Validation(_))
        ));
        let after = core.store.get_artifact(&made[0].id).await.unwrap();
        assert_eq!(
            after.status,
            crate::store::artifacts::ArtifactStatus::Superseded,
            "the refused call changed the row anyway"
        );
    }

    #[tokio::test]
    async fn a_point_whose_row_is_gone_gets_its_row_back() {
        // This used to delete the point. The backfill it ran from is spawned at
        // startup whenever anything is unstamped, so a process pointed at an
        // empty or wrong sqlite file found every point orphaned and emptied the
        // collection — with the vectors being the last copy, there was nothing
        // left to reindex from.
        let core = test_core().await;
        let (corpus, artifact) = one_artifact(&core).await;
        core.vectors
            .upsert(vec![point(&artifact, &corpus), point("gone", &corpus)])
            .await
            .unwrap();

        let drift = core.heal_store_drift().await.unwrap();

        assert_eq!(drift.rows_restored, 1, "{drift:?}");
        assert_eq!(
            core.vectors.all_artifact_ids().await.unwrap().len(),
            2,
            "the heal deleted a point instead of restoring its row"
        );
        let back = core.store.get_artifact("gone").await.unwrap();
        assert_eq!(back.text, "t");
        assert_eq!(back.corpus_id.as_deref(), Some(corpus.as_str()));
        assert_eq!(
            back.embed_state,
            EmbedState::Pending,
            "a restored row must be re-embedded: the stored vector may be from \
             another model, and nothing else would ever check"
        );
    }

    #[tokio::test]
    async fn a_merge_restored_from_the_index_is_flagged_as_orphaned() {
        // `restore_artifact` cannot recreate lineage rows and leaves
        // `source_count` at 0, so `merged_missing_a_source` (0 > 0) never
        // fires and nothing ever flagged the restored merge — it stood as a
        // merge with no record of its sources, silently.
        let core = test_core().await;
        let mut p = point("lost-merge", "");
        p.payload.provenance = Some("merged".into());
        core.vectors.upsert(vec![p]).await.unwrap();

        let drift = core.heal_store_drift().await.unwrap();

        assert_eq!(drift.rows_restored, 1, "{drift:?}");
        let back = core.store.get_artifact("lost-merge").await.unwrap();
        assert!(
            back.flags.iter().any(|f| f == "orphaned_source"),
            "a restored merge must say it cannot support its provenance claim"
        );
    }

    #[tokio::test]
    async fn restoring_an_artifact_whose_corpus_is_also_gone_writes_a_marked_placeholder() {
        // The whole-database-lost case. `artifacts.corpus_id` is NOT NULL and
        // references `corpora`, so without a parent the restore cannot happen at
        // all — and a placeholder that did not say it was one would present
        // reconstructed fragments as a captured document.
        let core = test_core().await;
        core.vectors
            .upsert(vec![point("orphan", "corpus-that-never-existed")])
            .await
            .unwrap();

        let drift = core.heal_store_drift().await.unwrap();

        assert_eq!(drift.rows_restored, 1, "{drift:?}");
        assert_eq!(drift.corpora_restored, 1, "{drift:?}");
        let stub = core
            .store
            .get_corpus("corpus-that-never-existed")
            .await
            .unwrap();
        assert!(
            stub.restored_at.is_some(),
            "the placeholder is indistinguishable from a real capture"
        );
        assert_eq!(stub.raw_text, "t", "the stub holds what was recoverable");
    }

    #[tokio::test]
    async fn healing_twice_changes_nothing_the_second_time() {
        // The heal runs at startup and on every consolidation sweep, so a pass
        // that keeps finding work on a base already in agreement is a permanent
        // background rewrite — and, with `restore_artifact` keeping the original
        // id, the way to get that wrong is to mint a new one and orphan the
        // point all over again.
        let core = test_core().await;
        let (corpus, _) = one_artifact(&core).await;
        core.vectors
            .upsert(vec![point("gone", &corpus)])
            .await
            .unwrap();

        core.heal_store_drift().await.unwrap();
        let second = core.heal_store_drift().await.unwrap();

        assert_eq!(second, StoreDrift::default(), "the heal is not idempotent");
    }

    #[tokio::test]
    async fn a_row_that_claims_it_is_embedded_with_no_point_is_requeued() {
        // The other direction. A row still `pending` is not drift — that is
        // every artifact that was just captured — so only a row claiming `done`
        // counts, and the repair is to send it back through the pipeline that
        // writes points rather than to delete the row.
        let core = test_core().await;
        let (_, artifact) = one_artifact(&core).await;
        core.store
            .mark_embedded(&artifact, "some-model", 0)
            .await
            .unwrap();

        let drift = core.heal_store_drift().await.unwrap();

        assert_eq!(drift.points_requeued, 1, "{drift:?}");
        assert!(
            core.store.get_artifact(&artifact).await.is_ok(),
            "the heal deleted the row instead of re-embedding it"
        );
    }

    #[tokio::test]
    async fn an_artifact_still_waiting_to_embed_is_not_drift() {
        // Everything just ingested has a row and no point. Treating that as
        // drift would re-queue the entire backlog on every sweep.
        let core = test_core().await;
        one_artifact(&core).await;

        let drift = core.heal_store_drift().await.unwrap();

        assert_eq!(drift, StoreDrift::default(), "{drift:?}");
    }

    fn a_png() -> Vec<u8> {
        use image::{ImageBuffer, Rgb};
        let img = ImageBuffer::from_fn(40, 20, |x, _| Rgb([(x * 6) as u8, 0, 0]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    #[tokio::test]
    async fn an_image_capture_stores_the_original_and_queues_describe_without_calling_the_model() {
        let describer = std::sync::Arc::new(crate::infer::fake::FakeDescriber::default());
        let core = crate::core::test_support::test_core_with_describer(describer.clone()).await;
        let bytes = a_png();
        let out = core
            .ingest_image(ImageCapture {
                bytes: bytes.clone(),
                filename: Some("IMG_1.png".into()),
                title_hint: None,
                note: Some("  the kitchen whiteboard ".into()),
            })
            .await
            .unwrap();
        assert_eq!(out.status, CorpusStatus::Describing);
        assert!(!out.duplicate);
        assert_eq!(describer.calls(), 0, "capture must not call the model");

        let src = core.store.get_corpus(&out.id).await.unwrap();
        assert_eq!(src.origin, ORIGIN_IMAGE);
        assert_eq!(src.raw_text, "");
        assert_eq!(src.title_hint.as_deref(), Some("IMG_1.png"));
        assert_eq!(src.metadata["note"], "the kitchen whiteboard");
        assert_eq!(src.metadata["file"]["name"], "IMG_1.png");
        assert_eq!(src.metadata["file"]["mime"], "image/png");
        assert_eq!(src.metadata["file"]["width"], 40);

        let a = core
            .store
            .attachment_for_corpus(&out.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(a.bytes, bytes, "the original is stored byte for byte");
        assert_eq!(a.mime, "image/png");
        assert!(image::load_from_memory(&a.preview).is_ok());

        let job = core
            .store
            .claim_job()
            .await
            .unwrap()
            .expect("a job was queued");
        assert_eq!(job.stage, Stage::Describe);
        assert_eq!(job.target_id, out.id);
    }

    #[tokio::test]
    async fn the_same_photo_twice_is_a_duplicate_before_any_model_call() {
        let core = crate::core::test_support::test_core().await;
        let first = core
            .ingest_image(ImageCapture {
                bytes: a_png(),
                filename: None,
                title_hint: None,
                note: None,
            })
            .await
            .unwrap();
        let again = core
            .ingest_image(ImageCapture {
                bytes: a_png(),
                filename: None,
                title_hint: None,
                note: None,
            })
            .await
            .unwrap();
        assert!(again.duplicate);
        assert_eq!(again.id, first.id);
        assert_eq!(core.store.list_corpora(10, 0).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn without_a_vision_role_the_image_door_is_closed() {
        let core = crate::core::test_support::test_core_without_vision().await;
        let e = core
            .ingest_image(ImageCapture {
                bytes: a_png(),
                filename: None,
                title_hint: None,
                note: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(e, Error::Validation(_)));
        assert!(e.to_string().contains("not configured"), "{e}");
        assert!(core.store.list_corpora(10, 0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn junk_is_refused_before_anything_is_stored() {
        let core = crate::core::test_support::test_core().await;
        let e = core
            .ingest_image(ImageCapture {
                bytes: b"nope".to_vec(),
                filename: Some("x.jpg".into()),
                title_hint: None,
                note: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(e, Error::Validation(_)));
        assert!(core.store.list_corpora(10, 0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_note_is_capped_and_a_blank_one_is_dropped() {
        let core = crate::core::test_support::test_core().await;
        let long = "x".repeat(MAX_NOTE_CHARS + 50);
        let out = core
            .ingest_capture(Capture::new("some text", "upload").with_note(Some(long)))
            .await
            .unwrap();
        let src = core.store.get_corpus(&out.id).await.unwrap();
        assert_eq!(src.metadata["note"].as_str().unwrap().len(), MAX_NOTE_CHARS);

        let out = core
            .ingest_capture(Capture::new("other text", "upload").with_note(Some("   ".into())))
            .await
            .unwrap();
        assert!(
            core.store
                .get_corpus(&out.id)
                .await
                .unwrap()
                .metadata
                .get("note")
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_text_capture_carries_its_file_facts() {
        let core = crate::core::test_support::test_core().await;
        let out = core
            .ingest_capture(Capture::new("hello", "upload").with_file(
                Some("n.txt"),
                5,
                "text/plain",
            ))
            .await
            .unwrap();
        let m = core.store.get_corpus(&out.id).await.unwrap().metadata;
        assert_eq!(
            m["file"],
            serde_json::json!({"name": "n.txt", "size": 5, "mime": "text/plain"})
        );
    }
}
