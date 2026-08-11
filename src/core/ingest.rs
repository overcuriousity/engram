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
    pub duplicate: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub near_duplicate: Option<NearDuplicate>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct StoreDrift {
    pub rows_restored: usize,
    pub corpora_restored: usize,
    pub points_requeued: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NearDupeAction {
    Replace,
    KeepBoth,
    Discard,
}

impl Core {
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

    pub async fn unsupersede(&self, artifact_id: &str) -> Result<()> {
        self.vectors
            .set_lifecycle(artifact_id, ArtifactStatus::Active, None)
            .await?;
        self.store.set_superseded_by(artifact_id, None).await?;
        tracing::info!(artifact_id, "restored a superseded artifact to search");
        Ok(())
    }

    pub async fn supersede(&self, loser_id: &str, winner_id: &str) -> Result<()> {
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

    pub async fn deprecate(&self, id: &str) -> Result<()> {
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
        tracing::info!(artifact_id = id, "deprecated an artifact");
        Ok(())
    }

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

    pub async fn verify(&self, id: &str) -> Result<()> {
        let at = now();
        let previous = self.store.get_artifact(id).await?.last_verified_at;
        self.store.set_last_verified_at(id, at).await?;
        if let Err(e) = self.vectors.set_last_verified_at(id, at, true).await {
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
        tracing::info!(n, "backfilled lifecycle fields into the vector store");
        Ok(n)
    }

    pub async fn heal_store_drift(&self) -> Result<StoreDrift> {
        use std::collections::{BTreeMap, HashSet};

        let points = self.vectors.all_artifact_ids().await?;
        let rows = self.store.list_all_artifact_ids().await?;
        let embedded = self.store.list_embedded_artifact_ids().await?;

        let has_row: HashSet<&str> = rows.iter().map(String::as_str).collect();
        let has_point: HashSet<&str> = points.iter().map(String::as_str).collect();

        let mut out = StoreDrift::default();

        let orphan_points: Vec<String> = points
            .iter()
            .filter(|id| !has_row.contains(id.as_str()))
            .cloned()
            .collect();
        if !orphan_points.is_empty() {
            let payloads = self.vectors.payloads_of(&orphan_points).await?;
            let mut by_corpus: BTreeMap<&str, Vec<&crate::vector::VectorPayload>> = BTreeMap::new();
            for id in &orphan_points {
                match payloads.get(id) {
                    Some(p) => by_corpus.entry(p.corpus_id.as_str()).or_default().push(p),
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
                    let restored = crate::store::artifacts::RestoredArtifact {
                        id: p.artifact_id.clone(),
                        corpus_id: p.corpus_id.clone(),
                        text: p.text.clone(),
                        title: p.title.clone(),
                        category: p.category.clone(),
                        tags: p.tags.clone(),
                        created_at: p.created_at,
                        status: p.status.unwrap_or(ArtifactStatus::Active),
                        last_verified_at: p.last_verified_at,
                        superseded_by: p.superseded_by.clone(),
                    };
                    if self.store.restore_artifact(&restored).await? {
                        out.rows_restored += 1;
                        self.store
                            .enqueue(Stage::Embed, "artifact", &p.artifact_id)
                            .await?;
                    }
                }
            }
        }

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

    pub async fn delete_artifact(&self, id: &str) -> Result<()> {
        self.store.get_artifact(id).await?;
        self.vectors
            .delete_artifacts(std::slice::from_ref(&id.to_string()))
            .await?;
        self.store.delete_artifact(id).await?;
        self.heal_dangling_supersessions().await?;
        tracing::info!(artifact_id = id, "deleted an artifact");
        Ok(())
    }

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
                self.vectors.delete_by_corpus(&src.id).await?;
                for c in self.store.artifacts_for_corpus(&src.id).await? {
                    self.store.delete_artifact(&c.id).await?;
                }
                self.store.clear_segments(&src.id).await?;
                self.store.set_near_dupe(&src.id, None, None).await?;
                self.store
                    .set_corpus_status(&src.id, CorpusStatus::Raw)
                    .await?;
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
    use crate::core::ingest::{NearDupeAction, StoreDrift};
    use crate::core::test_support::{test_core, test_core_with_failing_synthesizer};
    use crate::store::artifacts::EmbedState;
    use crate::store::corpora::CorpusStatus;
    use crate::store::jobs::Stage;

    fn manual(marker: &str) -> String {
        (0..200)
            .map(|i| format!("step {i}: run the {marker} command and read its output"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn a_near_identical_capture_is_parked_rather_than_synthesised() {
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
        core.store.claim_job().await.unwrap();
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
            },
        }
    }

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
        assert_eq!(back.corpus_id, corpus);
        assert_eq!(
            back.embed_state,
            EmbedState::Pending,
            "a restored row must be re-embedded: the stored vector may be from \
             another model, and nothing else would ever check"
        );
    }

    #[tokio::test]
    async fn restoring_an_artifact_whose_corpus_is_also_gone_writes_a_marked_placeholder() {
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
        let core = test_core().await;
        one_artifact(&core).await;

        let drift = core.heal_store_drift().await.unwrap();

        assert_eq!(drift, StoreDrift::default(), "{drift:?}");
    }
}
