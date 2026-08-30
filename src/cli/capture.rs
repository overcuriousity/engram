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
    face: &crate::cli::face::Face,
) -> Result<Vec<String>> {
    let http = client()?;
    let mut ids = Vec::new();
    for target in targets {
        let (bytes, content_type) = read_target(target)?;
        let tz = local_zone();
        let meta = Meta { title, note, tz: tz.as_deref(), origin: None, intent: None };
        ids.push(post(&http, e, bytes, &content_type, target, meta, face).await?);
    }
    Ok(ids)
}

/// Capture what arrived on stdin, already read.
///
/// Its own entry point because deciding whether a pipe held anything consumes
/// it — see `args::verb` — and stdin does not rewind, so the bytes have to
/// travel rather than be read a second time.
pub async fn run_piped(
    e: &Endpoint,
    text: String,
    title: Option<&str>,
    note: Option<&str>,
    face: &crate::cli::face::Face,
) -> Result<Vec<String>> {
    let http = client()?;
    let tz = local_zone();
    let id = post(
        &http,
        e,
        text.into_bytes(),
        "text/plain",
        "stdin",
        Meta { title, note, tz: tz.as_deref(), origin: None, intent: None },
        face,
    )
    .await?;
    Ok(vec![id])
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .map_err(|err| Error::Internal(format!("http client: {err}")))
}

/// What a door knows about a capture beyond its bytes.
///
/// The two travel together everywhere — `run`, `run_piped` and `post` each
/// take both or neither — and threading them as a separate pair is what pushed
/// `post` past the argument count that is worth reading at a call site.
#[derive(Clone, Copy)]
struct Meta<'a> {
    title: Option<&'a str>,
    note: Option<&'a str>,
    /// The process's IANA zone, so a date in the text is read where it was
    /// typed rather than where the server runs.
    tz: Option<&'a str>,
    /// `journal` for `-j`; the server accepts nothing else.
    origin: Option<&'a str>,
    /// `remind` for `-r`.
    intent: Option<&'a str>,
}

/// The zone this process is in, or none where the platform cannot say.
pub(crate) fn local_zone() -> Option<String> {
    iana_time_zone::get_timezone().ok()
}

/// One piece of text, sent as the reminder or the entry a verb said it is.
pub async fn run_text(
    e: &Endpoint,
    text: String,
    title: Option<&str>,
    note: Option<&str>,
    origin: Option<&str>,
    intent: Option<&str>,
    face: &crate::cli::face::Face,
) -> Result<String> {
    let http = client()?;
    let tz = local_zone();
    post(
        &http,
        e,
        text.into_bytes(),
        "text/plain",
        "text",
        Meta { title, note, tz: tz.as_deref(), origin, intent },
        face,
    )
    .await
}

/// One capture, answering with the corpus id. `label` is what the target is
/// called when something goes wrong — a path, or `stdin`.
async fn post(
    http: &reqwest::Client,
    e: &Endpoint,
    bytes: Vec<u8>,
    content_type: &str,
    label: &str,
    meta: Meta<'_>,
    face: &crate::cli::face::Face,
) -> Result<String> {
    let target = label;
    {
        let mut url = format!("{}?", e.api("/capture"));
        if let Some(t) = meta.title {
            url.push_str(&format!("title={}&", encode(t)));
        }
        if let Some(n) = meta.note {
            url.push_str(&format!("note={}&", encode(n)));
        }
        for (key, value) in [("tz", meta.tz), ("origin", meta.origin), ("intent", meta.intent)] {
            if let Some(v) = value {
                url.push_str(&format!("{key}={}&", encode(v)));
            }
        }
        // The body is handed over in pieces so the track can fill as it goes.
        // A book is tens of megabytes and the upload is the part of a capture
        // an operator waits through; a client that showed nothing until the
        // response arrived looked wedged for the whole of it.
        //
        // Where the face is off the whole vector goes at once, exactly as it
        // did before: a pipe gets no chunked encoding it did not ask for.
        //
        // And where it is on, `content-length` is set by hand so the pieces are
        // sent length-framed too. The length is known — it is `bytes.len()`,
        // the whole reason the track can show a percentage — and without the
        // header a streamed body goes out chunked, which made what the server
        // and every proxy in front of it saw depend on nothing but whether the
        // operator's stdout happened to be a terminal.
        let len = bytes.len();
        let res = http
            .post(url.trim_end_matches(['?', '&']))
            .bearer_auth(&e.token)
            .header("content-type", content_type)
            .header(reqwest::header::CONTENT_LENGTH, len)
            .body(tracked_body(bytes, face))
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
        Ok(body["id"].as_str().unwrap_or_default().to_string())
    }
}

/// How much of the body goes out at a time. Small enough that the track moves
/// on a slow link, large enough that a book is not a hundred thousand yields.
const PIECE: usize = 64 * 1024;

/// The request body, filling a track as the transport reads it.
///
/// One vector where the face is off, so nothing about a scripted capture
/// changes: no track, and the length comes from the body itself.
///
/// The streamed form carries no length of its own, so the caller sets
/// `content-length` from the vector before handing it over; both forms then go
/// out identically framed.
fn tracked_body(bytes: Vec<u8>, face: &crate::cli::face::Face) -> reqwest::Body {
    if !face.on {
        return reqwest::Body::from(bytes);
    }
    let total = bytes.len();
    let mut fill = face.fill();
    let stream = async_stream::stream! {
        let mut at = 0usize;
        while at < total {
            let end = (at + PIECE).min(total);
            let piece = bytes[at..end].to_vec();
            at = end;
            fill.show(at, total);
            yield Ok::<Vec<u8>, std::io::Error>(piece);
        }
        // Before the response is read, and so before anything is printed. The
        // request that never gets here — a reset, a refusal mid-upload — is
        // covered by `Fill`'s own `Drop`, which this only makes earlier.
        fill.clear();
    };
    reqwest::Body::wrap_stream(stream)
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
    // The three background stages, redrawn in place from each poll. The
    // generic strand that used to sit here said only "something is happening";
    // these say which of the three it is, which is the whole reason `--watch`
    // exists. No poll is added for them: they are drawn from the answer the
    // loop was already asking for.
    let mut track = face.track();
    for _ in 0..WATCH_POLLS {
        let res = http
            .get(format!("{}/corpora/{id}", e.api("")))
            .bearer_auth(&e.token)
            .send()
            .await
            .map_err(|err| Error::Validation(format!("{err}")))?;
        // Looked at before the body is read. Without this the loop polls a
        // 401, a 404 or a deleted id six hundred times: `res.json()` falls
        // back to `Null`, the status fails to parse, and five minutes later
        // the client claims the capture is "still being read". A wrong token
        // or a wrong `ENGRAM_URL` is not something waiting will fix.
        //
        // Only 4xx. A 502, or a restart caught mid-poll, is exactly what the
        // retry is for, and giving up on one would be worse than the bug this
        // fixes.
        let code = res.status();
        if code.is_client_error() {
            track.clear();
            return Err(Error::Validation(format!(
                "{id}: the server would not say how it is getting on ({code})"
            )));
        }
        let body: serde_json::Value = res.json().await.unwrap_or(serde_json::Value::Null);
        let status: Option<CorpusStatus> = serde_json::from_value(body["status"].clone()).ok();
        if let Some(s) = status {
            track.show(s);
            if matches!(
                s,
                CorpusStatus::Ready
                    | CorpusStatus::Partial
                    | CorpusStatus::Failed
                    | CorpusStatus::NeedsReview
            ) {
                track.clear();
                println!("{id}  {}", s.as_str());
                return Ok(());
            }
        }
        tokio::time::sleep(WATCH_EVERY).await;
    }
    track.clear();
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
    /// A face with nothing drawn: every assertion in this module is about the
    /// bytes a capture sends, and a track on stderr is not one of them.
    fn off() -> crate::cli::face::Face {
        crate::cli::face::Face::decide(&Default::default(), false, false, None)
    }

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

        let ids = run(&e, &[file.display().to_string()], None, None, &off())
            .await
            .unwrap();
        assert_eq!(ids.len(), 1);
        let stored = core.store.get_corpus(&ids[0]).await.expect("stored");
        assert!(stored.raw_text.contains("a procedure worth keeping"));
    }

    /// The one case `off()` cannot reach: with the face on the body goes out in
    /// pieces, and the length is set by hand so it is framed the same way the
    /// single vector is. Bigger than `PIECE`, so several chunks actually go.
    ///
    /// What this guards is that the two forms are the same request. Before the
    /// header was set, a streamed body went out chunked with no
    /// `content-length` — so what the server and any proxy in front of it saw
    /// depended on nothing but whether the operator's stdout was a terminal.
    #[tokio::test]
    async fn a_body_sent_in_pieces_arrives_whole() {
        let (e, core) = endpoint().await;
        let face = crate::cli::face::Face::decide(
            &crate::cli::args::CliArgs {
                fancy: crate::cli::args::Fancy::Always,
                ..Default::default()
            },
            true,
            false,
            None,
        );
        let body = "eine Zeile, die sich wiederholt\n".repeat(8_000);
        assert!(body.len() > PIECE, "the test body fits in one piece");
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("lang.txt");
        std::fs::write(&file, &body).unwrap();

        let ids = run(&e, &[file.display().to_string()], None, None, &face)
            .await
            .unwrap();
        let stored = core.store.get_corpus(&ids[0]).await.expect("stored");
        assert_eq!(
            stored.raw_text.len(),
            body.len(),
            "the streamed body arrived a different length than it left"
        );
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
        let ids = run(&e, &targets, None, None, &off()).await.unwrap();
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
            &off(),
        )
        .await
        .unwrap();
        let stored = core.store.get_corpus(&ids[0]).await.expect("stored");
        assert_eq!(stored.title_hint.as_deref(), Some("A title with spaces"));
    }

    /// The five minutes this stops: an id the server will not talk about used
    /// to be polled six hundred times, the pulse animating the whole way, and
    /// then reported as "still being read". A revoked token, a wrong
    /// `ENGRAM_URL` and a deleted corpus all landed there, and none of them is
    /// something waiting fixes.
    #[tokio::test]
    async fn watching_gives_up_at_once_on_an_id_the_server_refuses() {
        let (e, _core) = endpoint().await;
        let face = crate::cli::face::Face::decide(&Default::default(), false, true, None);
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            watch(&e, "no-such-corpus", &face),
        )
        .await
        .expect("it must not sit in the poll loop")
        .expect_err("an id the server refuses is an error, not a wait");
        assert!(err.to_string().contains("404"), "{err}");
    }

    #[tokio::test]
    async fn watching_ends_at_a_state_nothing_moves_out_of() {
        use crate::store::corpora::CorpusStatus;
        let (e, core) = endpoint().await;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.txt");
        std::fs::write(&file, "a procedure worth keeping").unwrap();
        let ids = run(&e, &[file.display().to_string()], None, None, &off())
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
                .set_corpus_status(&ids[0], terminal)
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
        let err = run(&e, &["/no/such/file/anywhere".into()], None, None, &off())
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
        let err = run(&e, &[file.display().to_string()], None, None, &off())
            .await
            .unwrap_err();
        assert!(!err.to_string().is_empty());
    }
}
