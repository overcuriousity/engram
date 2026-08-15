//! The vision stage: one captured image, one call, and a corpus that from
//! here on is text like any other.

use crate::core::Core;
use crate::error::{Error, Result};
use crate::store::corpora::CorpusStatus;

pub async fn run(core: &Core, corpus_id: &str) -> Result<()> {
    let src = core.store.get_corpus(corpus_id).await?;
    if src.status != CorpusStatus::Describing {
        tracing::info!(
            corpus_id,
            status = src.status.as_str(),
            "already described; nothing to do"
        );
        return Ok(());
    }
    let Some(describer) = core.describer.as_ref() else {
        // Not a validation error: the photo is stored and the job should wait
        // for the role, not be dropped.
        return Err(Error::Inference {
            role: "vision",
            detail: "no vision role configured".into(),
        });
    };
    let Some((_, preview)) = core.store.attachment_preview(corpus_id).await? else {
        return Err(Error::Store(format!(
            "image corpus {corpus_id} has no attachment"
        )));
    };
    let context = crate::infer::prompt::describe_context(&src.metadata);

    let permit = core.gate.background().await;
    let read = describer.describe(&preview, &context).await;
    match &read {
        Ok(_) => permit.succeeded(),
        Err(e) => permit.failed(e),
    }
    let text = read?;

    if text.trim().is_empty() {
        // Not a near-duplicate, so not the review queue: that page offers
        // "keep" and "discard" against another corpus, and there is none.
        park_failed(core, corpus_id, "the model returned no text for this image").await?;
        return Ok(());
    }

    // Before the text is written: the scan reads every stored signature, and
    // this row's is still empty, so it cannot match itself.
    let sig = crate::store::shingle::signature(&text);
    let near = core
        .store
        .find_near_duplicate(&sig, core.consolidate.near_dupe_min)
        .await?;
    core.store.set_described_text(corpus_id, &text, sig).await?;
    core.park_or_queue(corpus_id, near.as_ref()).await?;
    tracing::info!(
        corpus_id,
        chars = text.len(),
        parked = near.is_some(),
        "image read"
    );
    Ok(())
}

/// The read is not going to happen — the model said no, or said nothing for
/// as long as we were willing to ask. The photo stays; the corpus says why it
/// stopped, on its page and on Ops, and `reprocess(Describe)` is the way back.
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
    meta["describe"] = serde_json::json!({ "error": reason });
    core.store.set_corpus_metadata(corpus_id, &meta).await?;
    core.store
        .set_corpus_status(corpus_id, CorpusStatus::Failed)
        .await?;
    tracing::warn!(
        corpus_id,
        reason,
        "image could not be read; parked as failed"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ingest::ImageCapture;
    use crate::core::test_support::{test_core_with_describer, test_core_without_vision};
    use crate::infer::fake::FakeDescriber;
    use crate::store::jobs::Stage;
    use std::sync::Arc;

    fn a_png(seed: u8) -> Vec<u8> {
        use image::{ImageBuffer, Rgb};
        let img = ImageBuffer::from_fn(32, 32, |x, y| Rgb([seed, x as u8, y as u8]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    async fn captured(core: &Core, seed: u8, note: Option<&str>) -> String {
        core.ingest_image(ImageCapture {
            bytes: a_png(seed),
            filename: Some("p.png".into()),
            title_hint: None,
            note: note.map(str::to_string),
        })
        .await
        .unwrap()
        .id
    }

    #[tokio::test]
    async fn describing_writes_the_text_and_hands_off_to_synthesize() {
        let d = Arc::new(FakeDescriber::saying("# Board\n\n- ship\n- test"));
        let core = test_core_with_describer(d.clone()).await;
        let id = captured(&core, 1, Some("kitchen board")).await;
        core.store.claim_job().await.unwrap(); // the Describe job
        run(&core, &id).await.unwrap();

        let src = core.store.get_corpus(&id).await.unwrap();
        assert_eq!(src.status, CorpusStatus::Raw);
        assert_eq!(src.raw_text, "# Board\n\n- ship\n- test");
        assert!(!src.shingles.is_empty());
        assert!(
            d.last_context().contains("kitchen board"),
            "{}",
            d.last_context()
        );
        assert!(d.last_context().contains("p.png"));
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
    async fn the_whole_pipeline_takes_a_photo_to_ready() {
        let core = test_core_with_describer(Arc::new(FakeDescriber::default())).await;
        let id = captured(&core, 2, None).await;
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
    async fn a_failing_model_leaves_the_corpus_describing_and_the_error_retryable() {
        let core = test_core_with_describer(Arc::new(FakeDescriber::failing("gpu on fire"))).await;
        let id = captured(&core, 3, None).await;
        let e = run(&core, &id).await.unwrap_err();
        assert!(e.retryable());
        assert_eq!(
            core.store.get_corpus(&id).await.unwrap().status,
            CorpusStatus::Describing
        );
    }

    #[tokio::test]
    async fn an_empty_reading_parks_the_corpus_as_failed_with_the_reason() {
        let core = test_core_with_describer(Arc::new(FakeDescriber::saying("  \n"))).await;
        let id = captured(&core, 4, None).await;
        core.store.claim_job().await.unwrap();
        run(&core, &id).await.unwrap();
        let src = core.store.get_corpus(&id).await.unwrap();
        assert_eq!(src.status, CorpusStatus::Failed);
        assert!(
            src.metadata["describe"]["error"]
                .as_str()
                .unwrap()
                .contains("no text")
        );
        assert!(
            src.near_dupe_of.is_none(),
            "not a near-duplicate; not on the review queue"
        );
        assert!(core.store.parked_corpora(10).await.unwrap().is_empty());
        assert!(
            core.store.claim_job().await.unwrap().is_none(),
            "nothing further queued"
        );
    }

    #[tokio::test]
    async fn a_reading_that_matches_an_existing_corpus_is_parked_as_a_near_duplicate() {
        let text = "The quarterly plan lists three goals: ship the beta, hire two engineers, and cut latency in half by autumn.";
        let core = test_core_with_describer(Arc::new(FakeDescriber::saying(text))).await;
        let first = core.ingest(text, "web", None).await.unwrap();
        let id = captured(&core, 5, None).await;
        core.store.claim_job().await.unwrap(); // synthesize for the paste
        core.store.claim_job().await.unwrap(); // describe
        run(&core, &id).await.unwrap();
        let src = core.store.get_corpus(&id).await.unwrap();
        assert_eq!(src.status, CorpusStatus::NeedsReview);
        assert_eq!(src.near_dupe_of.as_deref(), Some(first.id.as_str()));
        assert_eq!(src.raw_text, text, "the reading is kept even when parked");
    }

    async fn clear_backoff(core: &Core) {
        sqlx::query("UPDATE jobs SET run_after = 0")
            .execute(&core.store.pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_rejected_reading_parks_the_corpus_as_failed_with_the_reason() {
        let core = test_core_with_describer(Arc::new(FakeDescriber::rejecting(
            "HTTP 400: this model does not accept images",
        )))
        .await;
        let id = captured(&core, 6, None).await;
        assert!(crate::jobs::run_one(&core).await.unwrap());
        let src = core.store.get_corpus(&id).await.unwrap();
        assert_eq!(src.status, CorpusStatus::Failed);
        assert!(
            src.metadata["describe"]["error"]
                .as_str()
                .unwrap()
                .contains("does not accept images")
        );
        assert!(
            !core.store.live_job(Stage::Describe, &id).await.unwrap(),
            "the job is closed, not re-armed"
        );
    }

    #[tokio::test]
    async fn a_reading_that_keeps_failing_parks_the_corpus_after_its_attempts() {
        let core = test_core_with_describer(Arc::new(FakeDescriber::failing("gpu on fire"))).await;
        let id = captured(&core, 7, None).await;
        for _ in 0..crate::store::jobs::MAX_ATTEMPTS {
            clear_backoff(&core).await;
            assert!(crate::jobs::run_one(&core).await.unwrap());
        }
        let src = core.store.get_corpus(&id).await.unwrap();
        assert_eq!(src.status, CorpusStatus::Failed);
        assert!(
            src.metadata["describe"]["error"]
                .as_str()
                .unwrap()
                .contains("gpu on fire")
        );
        assert!(!core.store.live_job(Stage::Describe, &id).await.unwrap());
    }

    /// A stored image corpus with `metadata`, one pixel big, no reading yet.
    async fn image_corpus(
        core: &Core,
        metadata: serde_json::Value,
    ) -> crate::store::corpora::Corpus {
        core.store
            .insert_image_corpus(
                "h",
                "image",
                None,
                &metadata,
                &crate::store::attachments::NewImage {
                    kind: "image",
                    mime: "image/png",
                    filename: None,
                    bytes: b"orig",
                    preview: b"prev",
                    width: Some(1),
                    height: Some(1),
                },
            )
            .await
            .unwrap()
            .into_corpus()
    }

    #[tokio::test]
    async fn parking_survives_a_metadata_column_that_is_not_an_object() {
        // `meta["describe"] = ...` panics on anything but an object or null,
        // and the value comes out of a column. A worker is the wrong place to
        // find that out.
        let core = test_core_without_vision().await;
        let src = image_corpus(&core, serde_json::json!("not an object")).await;

        park_failed(&core, &src.id, "gpu on fire").await.unwrap();

        let got = core.store.get_corpus(&src.id).await.unwrap();
        assert_eq!(got.status, CorpusStatus::Failed);
        assert_eq!(
            got.metadata["describe"]["error"].as_str(),
            Some("gpu on fire"),
            "the reason is what the corpus page shows; it cannot be lost"
        );
    }

    #[tokio::test]
    async fn without_a_vision_role_an_exhausted_job_keeps_waiting() {
        let core = test_core_without_vision().await;
        let src = image_corpus(&core, serde_json::json!({})).await;
        core.store
            .enqueue(Stage::Describe, "corpus", &src.id)
            .await
            .unwrap();
        for _ in 0..crate::store::jobs::MAX_ATTEMPTS + 1 {
            clear_backoff(&core).await;
            assert!(crate::jobs::run_one(&core).await.unwrap());
        }
        assert_eq!(
            core.store.get_corpus(&src.id).await.unwrap().status,
            CorpusStatus::Describing
        );
        assert!(core.store.live_job(Stage::Describe, &src.id).await.unwrap());
    }

    #[tokio::test]
    async fn a_job_for_a_corpus_that_is_gone_is_not_found() {
        let core = test_core_with_describer(Arc::new(FakeDescriber::default())).await;
        assert!(matches!(
            run(&core, "nope").await,
            Err(crate::error::Error::NotFound)
        ));
    }
}
