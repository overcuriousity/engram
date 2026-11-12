//! The extraction stage: one captured PDF, no model call. Needs nothing configured, so exhaustion
//! is always a real failure.

use crate::core::Core;
use crate::error::{Error, Result};
use crate::store::corpora::CorpusStatus;

pub async fn run(core: &Core, corpus_id: &str) -> Result<()> {
    let src = core.store.get_corpus(corpus_id).await?;
    if src.status != CorpusStatus::Extracting {
        tracing::info!(
            corpus_id,
            status = src.status.as_str(),
            "already extracted; nothing to do"
        );
        return Ok(());
    }
    let Some((_, bytes)) = core.store.attachment_original(corpus_id).await? else {
        return Err(Error::Store(format!(
            "pdf corpus {corpus_id} has no attachment"
        )));
    };

    // `to_markdown` never yields; on a Tokio worker it would stall everything polled on that
    // thread.
    let read = tokio::task::spawn_blocking(move || crate::core::pdf::to_markdown(&bytes))
        .await
        .map_err(|e| Error::Internal(format!("extraction did not finish: {e}")))?;

    let text = match read {
        Ok(t) => t,
        // An unparseable PDF or a scan will not improve with retries: park it now. The reason is
        // shown to a person, so it is the message, not the `Error`'s `Display`.
        Err(Error::Validation(reason)) => {
            park_failed(core, corpus_id, &reason).await?;
            return Ok(());
        }
        Err(e) if !e.retryable() => {
            park_failed(core, corpus_id, &e.to_string()).await?;
            return Ok(());
        }
        Err(e) => return Err(e),
    };

    // Before the text is written: this row's signature is still empty, so it cannot match itself.
    let sig = crate::store::shingle::signature(&text);
    let near = core
        .store
        .find_near_duplicate(&sig, core.consolidate.near_dupe_min)
        .await?;
    core.store.set_read_text(corpus_id, &text, sig).await?;
    core.park_or_queue(corpus_id, near.as_ref()).await?;
    tracing::info!(
        corpus_id,
        chars = text.len(),
        flagged = near.is_some(),
        "pdf extracted"
    );
    Ok(())
}

/// The bytes are not a document this build can read; `reprocess(Extract)` is the way back.
pub async fn park_failed(core: &Core, corpus_id: &str, reason: &str) -> Result<()> {
    let src = core.store.get_corpus(corpus_id).await?;
    let mut meta = src.metadata.clone();
    // Indexing a non-object `Value` panics, and a column can hold anything.
    if !meta.is_object() {
        meta = serde_json::json!({});
    }
    meta["extract"] = serde_json::json!({ "error": reason });
    core.store.set_corpus_metadata(corpus_id, &meta).await?;
    core.store
        .set_corpus_status(corpus_id, CorpusStatus::Failed)
        .await?;
    tracing::warn!(corpus_id, reason, "pdf could not be read; parked as failed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ingest::PdfCapture;
    use crate::core::test_support::test_core;
    use crate::store::jobs::Stage;

    fn a_pdf() -> Vec<u8> {
        include_bytes!("../../tests/fixtures/one-heading.pdf").to_vec()
    }

    async fn captured(core: &Core, bytes: Vec<u8>) -> String {
        core.ingest_pdf(PdfCapture {
            bytes,
            filename: Some("plan.pdf".into()),
            title_hint: None,
            note: None,
            lang: crate::infer::lang::Lang::default(),
        })
        .await
        .unwrap()
        .id
    }

    #[tokio::test]
    async fn extraction_writes_the_markdown_and_hands_off_to_synthesize() {
        let core = test_core().await;
        let id = captured(&core, a_pdf()).await;
        core.store.claim_job().await.unwrap(); // the Extract job
        run(&core, &id).await.unwrap();

        let src = core.store.get_corpus(&id).await.unwrap();
        assert_eq!(src.status, CorpusStatus::Raw);
        assert!(src.raw_text.contains("quarterly plan lists three goals"));
        assert!(!src.shingles.is_empty(), "comparable to other captures");

        let next = core
            .store
            .claim_job()
            .await
            .unwrap()
            .expect("synthesize queued");
        assert_eq!(next.stage, Stage::Synthesize);
        assert_eq!(next.target_id, id);
    }

    #[tokio::test]
    async fn the_whole_pipeline_takes_a_pdf_to_ready() {
        let core = test_core().await;
        let id = captured(&core, a_pdf()).await;
        while crate::jobs::run_one(&core).await.unwrap() {}
        let src = core.store.get_corpus(&id).await.unwrap();
        assert_eq!(src.status, CorpusStatus::Ready, "{:?}", src.status);
        assert!(
            !core
                .store
                .artifacts_for_corpus(&id)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_pdf_that_cannot_be_read_is_parked_as_failed_with_the_reason() {
        let core = test_core().await;
        // The door stores bytes without reading them, so this is what a corrupt upload does.
        let id = captured(&core, b"%PDF-1.4 and then garbage".to_vec()).await;
        assert!(crate::jobs::run_one(&core).await.unwrap());

        let src = core.store.get_corpus(&id).await.unwrap();
        assert_eq!(src.status, CorpusStatus::Failed);
        assert!(
            src.metadata["extract"]["error"]
                .as_str()
                .unwrap()
                .contains("PDF"),
            "{:?}",
            src.metadata
        );
        assert!(
            !core.store.live_job(Stage::Extract, &id).await.unwrap(),
            "the job is closed, not re-armed: these bytes will not improve"
        );
        assert!(
            src.near_dupe_of.is_none(),
            "not a near-duplicate; not on the review queue"
        );
    }

    #[tokio::test]
    async fn a_scan_is_parked_with_the_reason_naming_the_build_that_would_read_it() {
        let core = test_core().await;
        let id = captured(
            &core,
            include_bytes!("../../tests/fixtures/no-text.pdf").to_vec(),
        )
        .await;
        assert!(crate::jobs::run_one(&core).await.unwrap());

        let src = core.store.get_corpus(&id).await.unwrap();
        assert_eq!(src.status, CorpusStatus::Failed);
        let reason = src.metadata["extract"]["error"].as_str().unwrap();
        assert!(reason.contains("no extractable text"), "{reason}");
        assert!(reason.contains("pdf-ml"), "{reason}");
        assert!(
            !reason.starts_with("validation:"),
            "a person reads this on the corpus page: {reason}"
        );
    }

    #[tokio::test]
    async fn a_re_extraction_replaces_the_reading_and_everything_from_it() {
        let core = test_core().await;
        let id = captured(&core, a_pdf()).await;
        while crate::jobs::run_one(&core).await.unwrap() {}

        core.reprocess(&id, Stage::Extract).await.unwrap();
        let src = core.store.get_corpus(&id).await.unwrap();
        assert_eq!(src.status, CorpusStatus::Extracting);
        assert_eq!(src.raw_text, "", "the old reading is gone, not merged");
        assert!(
            core.store
                .artifacts_for_corpus(&id)
                .await
                .unwrap()
                .is_empty()
        );

        while crate::jobs::run_one(&core).await.unwrap() {}
        let src = core.store.get_corpus(&id).await.unwrap();
        assert_eq!(src.status, CorpusStatus::Ready);
        assert!(src.raw_text.contains("quarterly plan"));
    }

    #[tokio::test]
    async fn only_a_captured_pdf_can_be_re_extracted() {
        let core = test_core().await;
        let out = core
            .ingest("just some pasted text", "web", None)
            .await
            .unwrap();
        assert!(matches!(
            core.reprocess(&out.id, Stage::Extract).await,
            Err(Error::Validation(_))
        ));
    }

    #[tokio::test]
    async fn a_pdf_with_no_extraction_yet_cannot_be_re_segmented() {
        // Re-segmenting starts from `raw_text`, which a PDF never read does not have.
        let core = test_core().await;
        let id = captured(&core, a_pdf()).await;
        assert!(matches!(
            core.reprocess(&id, Stage::Synthesize).await,
            Err(Error::Validation(_))
        ));
    }

    #[tokio::test]
    async fn parking_survives_a_metadata_column_that_is_not_an_object() {
        let core = test_core().await;
        let id = captured(&core, a_pdf()).await;
        core.store
            .set_corpus_metadata(&id, &serde_json::json!("not an object"))
            .await
            .unwrap();

        park_failed(&core, &id, "gpu on fire").await.unwrap();

        let got = core.store.get_corpus(&id).await.unwrap();
        assert_eq!(got.status, CorpusStatus::Failed);
        assert_eq!(
            got.metadata["extract"]["error"].as_str(),
            Some("gpu on fire"),
            "the reason is what the corpus page shows; it cannot be lost"
        );
    }

    #[tokio::test]
    async fn a_job_for_a_corpus_that_is_gone_is_not_found() {
        let core = test_core().await;
        assert!(matches!(run(&core, "nope").await, Err(Error::NotFound)));
    }
}
