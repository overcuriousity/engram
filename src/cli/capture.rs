//! `-c`: put a path, a link or a pipe into the base.

use crate::cli::encode;
use crate::cli::endpoint::Endpoint;
use crate::error::{Error, Result};

/// The client's user agent, so a token's row says what asked for it and a log
/// line says what called.
pub(crate) const USER_AGENT: &str = concat!("engram-cli/", env!("CARGO_PKG_VERSION"));

/// Capture each target in turn, answering with the corpus ids in the order they
/// were given.
///
/// One request per target rather than one multipart body holding all of them: a
/// glob of forty PDFs that fails on the nineteenth should have stored eighteen,
/// and the operator should be told which one stopped it.
pub async fn run(
    e: &Endpoint,
    targets: &[String],
    title: Option<&str>,
    note: Option<&str>,
) -> Result<Vec<String>> {
    let http = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .map_err(|err| Error::Internal(format!("http client: {err}")))?;
    let mut ids = Vec::new();
    for target in targets {
        let (bytes, content_type) = read_target(target)?;
        let mut url = format!("{}?", e.api("/capture"));
        if let Some(t) = title {
            url.push_str(&format!("title={}&", encode(t)));
        }
        if let Some(n) = note {
            url.push_str(&format!("note={}", encode(n)));
        }
        let res = http
            .post(url.trim_end_matches(['?', '&']))
            .bearer_auth(&e.token)
            .header("content-type", content_type)
            .body(bytes)
            .send()
            .await
            .map_err(|err| Error::Validation(format!("{target}: {err}")))?;
        let status = res.status();
        let body: serde_json::Value = res.json().await.unwrap_or(serde_json::Value::Null);
        if !status.is_success() {
            let said = body["error"]
                .as_str()
                .unwrap_or("the server refused it without saying why");
            return Err(Error::Validation(format!("{target}: {said}")));
        }
        // Said out loud rather than folded into a success: a parked capture is
        // stored and nothing more — not segmented, not embedded, not searchable
        // until someone decides between it and what it resembles.
        if let Some(n) = body.get("near_duplicate").filter(|v| !v.is_null()) {
            eprintln!(
                "{target}: held for review — {:.0}% similar to {}. \
                 Nothing is indexed until it is resolved in the web UI.",
                n["similarity"].as_f64().unwrap_or(0.0) * 100.0,
                n["corpus_id"]
                    .as_str()
                    .unwrap_or("something already stored")
            );
        }
        ids.push(body["id"].as_str().unwrap_or_default().to_string());
    }
    Ok(ids)
}

/// Follow a capture through the stages that run after it is stored.
///
/// The other doors describe those stages in a sentence and leave; a terminal is
/// a place a person is already sitting, so it can show them happening. It ends
/// at the first status nothing moves out of — including `NeedsReview`, the
/// parked near-duplicate, which is precisely the one a client must not sit
/// waiting on because only a person can move it.
///
/// Matched on the parsed variant rather than on the string: `CorpusStatus`
/// spells `NeedsReview` two different ways depending on whether it is going to
/// the database or to a response, and a comparison against either literal would
/// be right half the time.
pub async fn watch(e: &Endpoint, id: &str, face: &crate::cli::face::Face) -> Result<()> {
    use crate::store::corpora::CorpusStatus;
    let http = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .map_err(|err| Error::Internal(format!("http client: {err}")))?;
    let lamps = face.pulse("reading");
    for _ in 0..WATCH_POLLS {
        let res = http
            .get(format!("{}/corpora/{id}", e.api("")))
            .bearer_auth(&e.token)
            .send()
            .await
            .map_err(|err| Error::Validation(format!("{err}")))?;
        let body: serde_json::Value = res.json().await.unwrap_or(serde_json::Value::Null);
        let status: Option<CorpusStatus> = serde_json::from_value(body["status"].clone()).ok();
        if let Some(s) = status
            && matches!(
                s,
                CorpusStatus::Ready
                    | CorpusStatus::Partial
                    | CorpusStatus::Failed
                    | CorpusStatus::NeedsReview
            )
        {
            drop(lamps);
            println!("{id}  {}", s.as_str());
            return Ok(());
        }
        tokio::time::sleep(WATCH_EVERY).await;
    }
    drop(lamps);
    // Said rather than hidden: a capture still moving after five minutes is
    // information, and a client that exits silently claims a completion it
    // never saw.
    eprintln!("{id}: still being read — it carries on without this client");
    Ok(())
}

/// Five minutes of polling, twice a second. Long enough for a book through
/// extraction and short enough that a wedged queue is not watched all night.
const WATCH_POLLS: usize = 600;
const WATCH_EVERY: std::time::Duration = std::time::Duration::from_millis(500);

/// The bytes to send and what to call them.
///
/// A link is sent as `text/plain` and the server decides it is a link, which is
/// the same decision a share sheet's body gets: one rule about what a bare URL
/// means, in one place, rather than one per client.
fn read_target(target: &str) -> Result<(Vec<u8>, String)> {
    if target == "-" {
        use std::io::Read;
        let mut buf = Vec::new();
        std::io::stdin()
            .read_to_end(&mut buf)
            .map_err(|e| Error::Validation(format!("stdin: {e}")))?;
        return Ok((buf, "text/plain".into()));
    }
    if target.starts_with("http://") || target.starts_with("https://") {
        return Ok((target.as_bytes().to_vec(), "text/plain".into()));
    }
    let bytes = std::fs::read(target).map_err(|e| Error::Validation(format!("{target}: {e}")))?;
    // The extension is what this end knows; the server sniffs the bytes anyway.
    // Guessing wrong here costs nothing, and guessing at all saves the common
    // case of a PDF arriving labelled as text.
    let mime = mime_guess::from_path(target)
        .first_raw()
        .unwrap_or("text/plain")
        .to_string();
    Ok((bytes, mime))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn endpoint() -> (Endpoint, crate::core::Core) {
        let (url, token, core) = crate::cli::test_support::serve_test_app().await;
        (Endpoint { url, token }, core)
    }

    #[tokio::test]
    async fn a_path_lands_as_a_corpus_holding_what_the_file_held() {
        let (e, core) = endpoint().await;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.txt");
        std::fs::write(&file, "a procedure worth keeping").unwrap();

        let ids = run(&e, &[file.display().to_string()], None, None)
            .await
            .unwrap();
        assert_eq!(ids.len(), 1);
        let stored = core.store.get_corpus(&ids[0]).await.expect("stored");
        assert!(stored.raw_text.contains("a procedure worth keeping"));
    }

    #[tokio::test]
    async fn several_paths_in_one_invocation_are_several_corpora() {
        let (e, _core) = endpoint().await;
        let dir = tempfile::tempdir().unwrap();
        let mut targets = Vec::new();
        for (i, body) in ["the first procedure", "the second procedure"]
            .iter()
            .enumerate()
        {
            let p = dir.path().join(format!("{i}.txt"));
            std::fs::write(&p, body).unwrap();
            targets.push(p.display().to_string());
        }
        let ids = run(&e, &targets, None, None).await.unwrap();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1]);
    }

    #[tokio::test]
    async fn a_title_reaches_the_corpus_it_was_given_for() {
        let (e, core) = endpoint().await;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.txt");
        std::fs::write(&file, "losetup takes the offset in bytes").unwrap();
        let ids = run(
            &e,
            &[file.display().to_string()],
            Some("A title with spaces"),
            None,
        )
        .await
        .unwrap();
        let stored = core.store.get_corpus(&ids[0]).await.expect("stored");
        assert_eq!(stored.title_hint.as_deref(), Some("A title with spaces"));
    }

    #[tokio::test]
    async fn watching_ends_at_a_state_nothing_moves_out_of() {
        use crate::store::corpora::CorpusStatus;
        let (e, core) = endpoint().await;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.txt");
        std::fs::write(&file, "a procedure worth keeping").unwrap();
        let ids = run(&e, &[file.display().to_string()], None, None)
            .await
            .unwrap();
        let face = crate::cli::face::Face::decide(&Default::default(), false, true, None);

        // A fresh capture is `raw`, which a worker moves along — so the client
        // is right to keep waiting there, and this drives it to each state
        // that nothing moves out of instead. `NeedsReview` is the one that
        // matters most: only a person can move a parked near-duplicate, so a
        // client that waited on it would wait for ever.
        for terminal in [
            CorpusStatus::Ready,
            CorpusStatus::Partial,
            CorpusStatus::Failed,
            CorpusStatus::NeedsReview,
        ] {
            core.store
                .set_corpus_status(&ids[0], terminal.clone())
                .await
                .expect("set the status");
            tokio::time::timeout(
                std::time::Duration::from_secs(10),
                watch(&e, &ids[0], &face),
            )
            .await
            .unwrap_or_else(|_| panic!("watching did not end at {terminal:?}"))
            .expect("watching must not fail");
        }
    }

    #[tokio::test]
    async fn a_target_that_stops_the_run_is_named_in_the_error() {
        let (e, _core) = endpoint().await;
        let err = run(&e, &["/no/such/file/anywhere".into()], None, None)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("/no/such/file/anywhere"),
            "which one stopped it must be in the words: {err}"
        );
    }

    #[tokio::test]
    async fn a_server_that_cannot_be_reached_is_reported_rather_than_panicked() {
        // Port 1 is reserved and nothing listens on it.
        let e = Endpoint {
            url: "http://127.0.0.1:1".into(),
            token: "engram_nope".into(),
        };
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "anything").unwrap();
        let err = run(&e, &[file.display().to_string()], None, None)
            .await
            .unwrap_err();
        assert!(!err.to_string().is_empty());
    }
}
