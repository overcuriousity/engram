//! The extraction stage: one captured PDF, no model call, and a corpus that
//! from here on is text like any other.
//!
//! `jobs::describe` is the same shape for a photograph. The difference worth
//! knowing is that this one needs nothing configured, so an exhausted attempt
//! is always a real failure and never a wait for a role that has not arrived.

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

    // `to_markdown` walks the whole document without yielding. Held on a Tokio
    // worker that is seconds during which search, health and the queue poll on
    // that thread all wait; see `web::api::extract` for the same move.
    let read = tokio::task::spawn_blocking(move || crate::core::pdf::to_markdown(&bytes))
        .await
        .map_err(|e| Error::Internal(format!("extraction did not finish: {e}")))?;

    let text = match read {
        Ok(t) => t,
        // A PDF that cannot be parsed will not parse better on the fourth
        // attempt, and a scan will not grow a text layer. Park it now, with
        // the reason on its page, rather than retrying to the ceiling.
        // The reason is shown to a person on the corpus page, so it is the
        // message and not the `Error`'s `Display` — which would put
        // "validation: " in front of a sentence someone has to read.
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

    // Before the text is written: the scan reads every stored signature, and
    // this row's is still empty, so it cannot match itself.
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
        parked = near.is_some(),
        "pdf extracted"
    );
    Ok(())
}

/// The extraction is not going to happen — the bytes are not a document this
/// build can read. The file stays; the corpus says why it stopped, on its page
/// and on Ops, and `reprocess(Extract)` is the way back once the build changes.
pub async fn park_failed(core: &Core, corpus_id: &str, reason: &str) -> Result<()> {
    let src = core.store.get_corpus(corpus_id).await?;
    let mut meta = src.metadata.clone();
    // Indexing a `Value` that is not an object panics, and this one comes
    // straight out of a column. The reason is the whole point of the write —
    // it is what the corpus page and Ops show — so a column holding something
    // else is started over rather than left to take the worker down.
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
        // Through the door rather than around it: the door stores bytes and
        // does not read them, so this is exactly what a corrupt upload does.
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
        // Re-segmenting starts from `raw_text`, and there is none: the button
        // that promised to process it would leave a PDF never read.
        let core = test_core().await;
        let id = captured(&core, a_pdf()).await;
        assert!(matches!(
            core.reprocess(&id, Stage::Synthesize).await,
            Err(Error::Validation(_))
        ));
    }

    #[tokio::test]
    async fn parking_survives_a_metadata_column_that_is_not_an_object() {
        // `meta["extract"] = ...` panics on anything but an object or null,
        // and the value comes out of a column. A worker is the wrong place to
        // find that out. Same hazard as `jobs::describe::park_failed`.
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
