//! `Stage::Fetch`: a link that could not be read when it was captured, tried
//! again.
//!
//! The capture is already stored and searchable as the link and whatever came
//! with it (`Core::ingest_link`). What arrives here replaces that text; what
//! does not is tried again at the backoff, and after `ATTEMPTS` the link is
//! left as it is, with the reason on it.

use crate::core::Core;
use crate::error::Result;

/// How many times a held link is tried before it is left alone. The backoff
/// doubles to its six-hour ceiling around the fifteenth, so this is a day or
/// two of trying: long enough for a site that was down, short enough that a
/// login wall is not asked every six hours for ever.
pub const ATTEMPTS: i64 = 20;

pub async fn run(core: &Core, corpus_id: &str) -> Result<()> {
    let src = core.store.get_corpus(corpus_id).await?;
    // Read already, or asked again by nobody: nothing owed.
    if src.metadata["fetch"]["pending"].as_bool() != Some(true) {
        return Ok(());
    }
    let Some(raw) = src.source_url.as_deref() else {
        return Ok(());
    };
    let url =
        url::Url::parse(raw).map_err(|e| crate::error::Error::Validation(format!("url: {e}")))?;
    let read = core.read_link(&url).await?;
    core.fill_held_link(&src, read).await
}

#[cfg(test)]
mod tests {
    use crate::core::test_support::test_core;
    use crate::store::corpora::CorpusStatus;
    use crate::store::jobs::Stage;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn page() -> String {
        format!(
            "<html><head><title>REST, explained</title></head><body><article><h1>REST</h1>{}</article></body></html>",
            "<p>A REST API exposes resources over HTTP, and clients change them with verbs.</p>"
                .repeat(6)
        )
    }

    #[tokio::test]
    async fn a_link_that_cannot_be_read_is_stored_with_what_came_with_it() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/down"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let core = test_core().await;
        let u = url::Url::parse(&format!("{}/down", server.uri())).unwrap();
        let out = core
            .ingest_link(
                &u,
                None,
                Some("read this for the API talk".into()),
                Some("What is a REST API?".into()),
                crate::infer::lang::Lang::default(),
            )
            .await
            .expect("a link that cannot be read is still stored");
        assert!(
            out.link_unread
                .as_deref()
                .is_some_and(|w| w.contains("503")),
            "the receipt says why: {:?}",
            out.link_unread
        );
        let c = core.store.get_corpus(&out.id).await.unwrap();
        assert!(c.raw_text.contains(u.as_str()), "{}", c.raw_text);
        assert!(
            c.raw_text.contains("What is a REST API?"),
            "the shared text is kept"
        );
        assert_eq!(c.metadata["note"], "read this for the API talk");
        assert_eq!(c.metadata["fetch"]["pending"], true);
        assert_eq!(c.source_url.as_deref(), Some(u.as_str()));
        assert!(core.store.has_job(Stage::Fetch, &out.id).await.unwrap());
        assert!(
            core.store
                .live_job(Stage::Synthesize, &out.id)
                .await
                .unwrap(),
            "and read like any capture, so it is searchable now"
        );
    }

    #[tokio::test]
    async fn a_held_link_whose_page_arrives_is_read_in_place() {
        let server = MockServer::start().await;
        let core = test_core().await;
        let u = url::Url::parse(&format!("{}/later", server.uri())).unwrap();
        let out = core
            .ingest_url(&u, None, None, crate::infer::lang::Lang::default())
            .await
            .unwrap();
        crate::jobs::test_support::drain(&core).await;
        assert!(
            core.store.has_job(Stage::Fetch, &out.id).await.unwrap(),
            "still owed after a failed try"
        );

        Mock::given(method("GET"))
            .and(path("/later"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(page(), "text/html"))
            .mount(&server)
            .await;
        super::run(&core, &out.id).await.unwrap();
        crate::jobs::test_support::drain(&core).await;

        let c = core.store.get_corpus(&out.id).await.unwrap();
        assert!(
            c.raw_text.contains("clients change them with verbs"),
            "{}",
            c.raw_text
        );
        assert_eq!(c.title_hint.as_deref(), Some("REST, explained"));
        assert!(c.metadata["fetch"]["pending"].is_null());
        assert_eq!(c.status, CorpusStatus::Ready);
    }

    #[tokio::test]
    async fn a_title_the_person_gave_survives_the_page_arriving() {
        let server = MockServer::start().await;
        let core = test_core().await;
        let u = url::Url::parse(&format!("{}/named", server.uri())).unwrap();
        let out = core
            .ingest_url(
                &u,
                Some("mine".into()),
                None,
                crate::infer::lang::Lang::default(),
            )
            .await
            .unwrap();
        Mock::given(method("GET"))
            .and(path("/named"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(page(), "text/html"))
            .mount(&server)
            .await;
        super::run(&core, &out.id).await.unwrap();
        let c = core.store.get_corpus(&out.id).await.unwrap();
        assert_eq!(c.title_hint.as_deref(), Some("mine"));
    }

    #[tokio::test]
    async fn a_scheme_nothing_reads_is_still_refused() {
        let core = test_core().await;
        let u = url::Url::parse("ftp://example.com/a").unwrap();
        assert!(matches!(
            core.ingest_url(&u, None, None, crate::infer::lang::Lang::default())
                .await,
            Err(crate::error::Error::Validation(_))
        ));
    }
}
