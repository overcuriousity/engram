use super::Core;
use crate::error::{Error, Result};
use crate::store::artifacts::ArtifactStatus;
use crate::store::corpora::{CorpusStatus, NearDuplicate, content_hash};
use crate::store::jobs::Stage;
use crate::store::now;

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

impl Core {
    /// Store the text and queue processing. Deliberately makes no inference
    /// call: capture must stay instant and must survive a dead endpoint.
    pub async fn ingest(
        &self,
        text: &str,
        origin: &str,
        title_hint: Option<&str>,
    ) -> Result<IngestOutcome> {
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

        let src = self
            .store
            .insert_corpus_with_signature(text, origin, title_hint, sig)
            .await?;

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
        self.vectors
            .set_lifecycle(artifact_id, ArtifactStatus::Active, None)
            .await?;
        self.store.set_superseded_by(artifact_id, None).await?;
        tracing::info!(artifact_id, "restored a superseded artifact to search");
        Ok(())
    }

    /// Hide `loser_id` in favour of `winner_id`. Row before payload, matching
    /// the sweep's existing auto-supersede order (`jobs::consolidate`): the
    /// intermediate state after a partial failure is one an operator can act
    /// on, since the artifact is already listed on Ops with its `superseded_by`
    /// set, even if the search-side flag has not caught up yet.
    pub async fn supersede(&self, loser_id: &str, winner_id: &str) -> Result<()> {
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
        self.store
            .set_artifact_status(id, ArtifactStatus::Deprecated)
            .await?;
        self.vectors
            .set_lifecycle(id, ArtifactStatus::Deprecated, None)
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
        if self.store.get_artifact(id).await?.superseded_by.is_some() {
            return self.unsupersede(id).await;
        }
        self.vectors
            .set_lifecycle(id, ArtifactStatus::Active, None)
            .await?;
        self.store
            .set_artifact_status(id, ArtifactStatus::Active)
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
    pub async fn verify(&self, id: &str) -> Result<()> {
        let at = now();
        self.store.set_last_verified_at(id, at).await?;
        self.vectors.set_last_verified_at(id, at, true).await?;
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
        tracing::info!(n, "backfilled lifecycle fields into the vector store");
        Ok(n)
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
        let mut first_err = None;
        for id in self.store.dangling_superseded().await? {
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
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::core::ingest::NearDupeAction;
    use crate::core::test_support::{test_core, test_core_with_failing_synthesizer};
    use crate::store::corpora::CorpusStatus;
    use crate::store::jobs::Stage;

    /// A body long enough to have a stable shingle signature.
    fn manual(marker: &str) -> String {
        (0..200)
            .map(|i| format!("step {i}: run the {marker} command and read its output"))
            .collect::<Vec<_>>()
            .join("\n")
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
        crate::jobs::synthesize::run(&core, &out.id).await.unwrap();
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

        crate::jobs::synthesize::run(&core, &out.id).await.unwrap();
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
}
