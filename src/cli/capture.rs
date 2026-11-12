//! `-c`: put a path, a link or a pipe into the base.

use crate::cli::encode;
use crate::cli::endpoint::{Endpoint, client};
use crate::error::{Error, Result};

/// Does this argument address something (stdin, a link, a real name, a path's shape)?
fn addresses_something(target: &str) -> bool {
    target == "-"
        || target.starts_with("http://")
        || target.starts_with("https://")
        || looks_like_a_path(target)
        || std::path::Path::new(target).exists()
}

/// Capture each target in turn, answering with the corpus ids in order; one request each so a
/// failure stops one target. An unquoted note is joined back into one sentence (see `reading`).
pub async fn run(
    e: &Endpoint,
    targets: &[String],
    title: Option<&str>,
    note: Option<&str>,
    face: &crate::cli::face::Face,
) -> Result<Vec<String>> {
    let joined;
    let (targets, as_prose) = match reading(targets)? {
        // One argument, or all paths; `read_target` refuses a single one that names nothing.
        Reading::Targets => (targets, false),
        Reading::Prose(line) => {
            joined = [line];
            (&joined[..], true)
        }
    };
    let http = client()?;
    let mut ids = Vec::new();
    for target in targets {
        let read = if as_prose {
            Read {
                bytes: target.as_bytes().to_vec(),
                content_type: "text/plain".into(),
                label: "text".into(),
            }
        } else {
            read_target(target)?
        };
        let tz = zone_for(&read.content_type, &read.bytes);
        let meta = Meta {
            title,
            note,
            tz: tz.as_deref(),
            intent: None,
        };
        ids.push(
            post(
                &http,
                e,
                read.bytes,
                &read.content_type,
                read.label,
                meta,
                face,
            )
            .await?,
        );
    }
    Ok(ids)
}

enum Reading {
    Targets,
    Prose(String),
}

/// A word with no path shape at all: what the shell hands over for an unquoted note.
fn plainly_prose(target: &str) -> bool {
    !addresses_something(target)
        && !target.contains('/')
        && !target.contains(std::path::MAIN_SEPARATOR)
}

/// How `run` reads its arguments: as they stand when all address something, joined when none do.
/// Partly paths: any plain word makes it a note; path-shaped misses are a typo and refused.
fn reading(targets: &[String]) -> Result<Reading> {
    let addressing = targets.iter().filter(|t| addresses_something(t)).count();
    if targets.len() < 2 || addressing == targets.len() {
        return Ok(Reading::Targets);
    }
    if addressing == 0 || targets.iter().any(|t| plainly_prose(t)) {
        return Ok(Reading::Prose(targets.join(" ")));
    }
    let strays: Vec<&str> = targets
        .iter()
        .filter(|t| !addresses_something(t))
        .map(String::as_str)
        .collect();
    Err(Error::Validation(format!(
        "{}: names nothing that can be read, and the other arguments are paths. \
         Nothing was captured. Correct the path, or quote the whole line to \
         store it as a note.",
        strays.join(", ")
    )))
}

/// Capture what arrived on stdin, already read: stdin does not rewind.
pub async fn run_piped(
    e: &Endpoint,
    text: String,
    title: Option<&str>,
    note: Option<&str>,
    face: &crate::cli::face::Face,
) -> Result<Vec<String>> {
    let http = client()?;
    let tz = zone_for("text/plain", text.as_bytes());
    let id = post(
        &http,
        e,
        text.into_bytes(),
        "text/plain",
        "stdin",
        Meta {
            title,
            note,
            tz: tz.as_deref(),
            intent: None,
        },
        face,
    )
    .await?;
    Ok(vec![id])
}

/// What a door knows about a capture beyond its bytes.
#[derive(Clone, Copy)]
struct Meta<'a> {
    title: Option<&'a str>,
    note: Option<&'a str>,
    /// The process's IANA zone, so a date in the text is read where it was typed.
    tz: Option<&'a str>,
    intent: Option<&'a str>,
}

/// The zone this process is in, or none where it is not a name `chrono_tz` takes: the server
/// answers 400 to one (`right/Europe/Berlin`, a POSIX `TZ`), failing every capture.
pub(crate) fn local_zone() -> Option<String> {
    let name = iana_time_zone::get_timezone().ok()?;
    if name.parse::<chrono_tz::Tz>().is_err() {
        tracing::debug!(zone = %name, "this host names a zone the server cannot read; sending none");
        return None;
    }
    Some(name)
}

/// The zone to send where the server takes it: text/plain that is not a bare link. An older
/// server refuses `tz` on every other branch.
fn zone_for(content_type: &str, bytes: &[u8]) -> Option<String> {
    if !content_type.starts_with("text/plain") {
        return None;
    }
    let is_a_link = std::str::from_utf8(bytes)
        .ok()
        .is_some_and(|t| crate::core::fetch::only_a_url(t).is_some());
    if is_a_link {
        return None;
    }
    local_zone()
}

/// One piece of text, sent as the reminder a verb said it is. A bare link is refused here:
/// the server would fetch it and 400 on the time fields.
pub async fn run_text(
    e: &Endpoint,
    text: String,
    title: Option<&str>,
    note: Option<&str>,
    intent: Option<&str>,
    face: &crate::cli::face::Face,
) -> Result<String> {
    if crate::core::fetch::only_a_url(&text).is_some() {
        return Err(Error::Validation(
            "a link on its own is fetched and read by the server, so it cannot be a reminder. \
             Capture it with `engram -c`, then say what you want remembered about it."
                .into(),
        ));
    }
    let http = client()?;
    let tz = local_zone();
    post(
        &http,
        e,
        text.into_bytes(),
        "text/plain",
        "text",
        Meta {
            title,
            note,
            tz: tz.as_deref(),
            intent,
        },
        face,
    )
    .await
}

/// One capture, answering with the corpus id; `label` names the target in errors.
async fn post(
    http: &reqwest::Client,
    e: &Endpoint,
    bytes: Vec<u8>,
    content_type: &str,
    label: impl AsRef<str>,
    meta: Meta<'_>,
    face: &crate::cli::face::Face,
) -> Result<String> {
    let target = label.as_ref();
    {
        let mut url = format!("{}?", e.api("/capture"));
        if let Some(t) = meta.title {
            url.push_str(&format!("title={}&", encode(t)));
        }
        if let Some(n) = meta.note {
            url.push_str(&format!("note={}&", encode(n)));
        }
        for (key, value) in [("tz", meta.tz), ("intent", meta.intent)] {
            if let Some(v) = value {
                url.push_str(&format!("{key}={}&", encode(v)));
            }
        }
        // Sent in pieces so the track fills during a long upload; whole where the face is off.
        // `content-length` is set so the framing never depends on stdout being a terminal.
        let len = bytes.len();
        let res = http
            .post(url.trim_end_matches(['?', '&']))
            .bearer_auth(&e.token)
            .header("content-type", content_type)
            .header(reqwest::header::CONTENT_LENGTH, len)
            .body(tracked_body(bytes, face))
            .send()
            .await
            .map_err(|err| match e.unreachable(err) {
                Error::Validation(m) => Error::Validation(format!("{target}: {m}")),
                other => other,
            })?;
        let status = res.status();
        let body: serde_json::Value = res.json().await.unwrap_or(serde_json::Value::Null);
        if !status.is_success() {
            let said = body["error"]
                .as_str()
                .unwrap_or("the server refused it without saying why");
            return Err(Error::Validation(format!("{target}: {said}")));
        }
        // Said, so a person knows where the undo is; a flagged capture is searchable as usual.
        // A link that could not be read is stored as the link and retried.
        if let Some(why) = body.get("link_unread").and_then(|v| v.as_str()) {
            eprintln!(
                "{target}: the page could not be read ({why}) — stored as the link, \
                 and tried again in the background."
            );
        }
        if let Some(n) = body.get("near_duplicate").filter(|v| !v.is_null()) {
            eprintln!(
                "{target}: stored, and {:.0}% similar to {} — \
                 the base merges or hides what repeats on its own, with an undo on Insights.",
                n["similarity"].as_f64().unwrap_or(0.0) * 100.0,
                n["corpus_id"]
                    .as_str()
                    .unwrap_or("something already stored")
            );
        }
        Ok(body["id"].as_str().unwrap_or_default().to_string())
    }
}

/// Upload piece size: the track moves on a slow link, and a book is not a hundred thousand yields.
const PIECE: usize = 64 * 1024;

/// The request body, filling a track as the transport reads it; one vector where the face is off.
/// The caller sets `content-length` so both forms are framed alike.
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
        // Before the response is printed; a failed upload is covered by `Fill`'s own `Drop`.
        fill.clear();
    };
    reqwest::Body::wrap_stream(stream)
}

/// Follow a capture through its background stages to the first status nothing moves out of,
/// `NeedsReview` included. Matched on the variant: `CorpusStatus` spells that one two ways.
pub async fn watch(e: &Endpoint, id: &str, face: &crate::cli::face::Face) -> Result<()> {
    use crate::store::corpora::CorpusStatus;
    let http = client()?;
    // The three stages, drawn from the poll the loop already makes.
    let mut track = face.track();
    for _ in 0..WATCH_POLLS {
        let res = http
            .get(format!("{}/corpora/{id}", e.api("")))
            .bearer_auth(&e.token)
            .send()
            .await
            .map_err(|err| e.unreachable(err))?;
        // Give up on 4xx: waiting fixes no wrong token or id. 5xx and restarts are retried.
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
    // Said: exiting silently would claim a completion this client never saw.
    eprintln!("{id}: still being read — it carries on without this client");
    Ok(())
}

/// Five minutes at two polls a second: enough for a book, not a night on a wedged queue.
const WATCH_POLLS: usize = 600;
const WATCH_EVERY: std::time::Duration = std::time::Duration::from_millis(500);

/// What one `-c` target turned out to be: bytes, a name for the server, and a label for errors
/// (`text` for a sentence, not the whole paragraph).
struct Read {
    bytes: Vec<u8>,
    content_type: String,
    label: String,
}

/// Does an argument that is not on disk look like an attempt at a path? Absolute or `~/` always;
/// a relative slash only when its parent directory exists; a lone word with an extension.
fn looks_like_a_path(target: &str) -> bool {
    let expanded = expand_home(target);
    // Prose never begins at the root; an unexpanded `~/` was quoted on purpose.
    if expanded.is_absolute() || target.starts_with("~/") {
        return true;
    }
    // A lone word with an extension: its parent is the working directory, so the shape decides.
    let addressed = target.contains(std::path::MAIN_SEPARATOR) || target.contains('/');
    if !addressed {
        return !target.chars().any(char::is_whitespace)
            && std::path::Path::new(target)
                .extension()
                .is_some_and(|e| !e.is_empty());
    }
    expanded
        .parent()
        .is_some_and(|p| !p.as_os_str().is_empty() && p.is_dir())
}

/// A leading `~/` resolved, for the directory question only; the read uses the argument as typed.
fn expand_home(target: &str) -> std::path::PathBuf {
    match target.strip_prefix("~/") {
        Some(rest) => match std::env::home_dir() {
            Some(home) => home.join(rest),
            None => std::path::PathBuf::from(target),
        },
        None => std::path::PathBuf::from(target),
    }
}

/// The bytes to send and what to call them. A link goes as `text/plain` for the server to decide;
/// what is not a file is a note.
fn read_target(target: &str) -> Result<Read> {
    let text = |bytes: Vec<u8>, label: &str| Read {
        bytes,
        content_type: "text/plain".into(),
        label: label.into(),
    };
    if target == "-" {
        use std::io::Read as _;
        let mut buf = Vec::new();
        std::io::stdin()
            .read_to_end(&mut buf)
            .map_err(|e| Error::Validation(format!("stdin: {e}")))?;
        return Ok(text(buf, "stdin"));
    }
    if target.starts_with("http://") || target.starts_with("https://") {
        return Ok(text(target.as_bytes().to_vec(), target));
    }
    let bytes = match std::fs::read(target) {
        Ok(b) => b,
        // Only `NotFound` and `ENAMETOOLONG` leave room for prose; any other error means a file
        // (`EISDIR`, `EACCES`) that could not be read.
        Err(e)
            if looks_like_a_path(target)
                || !matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidFilename
                ) =>
        {
            return Err(Error::Validation(format!("{target}: {e}")));
        }
        Err(_) => return Ok(text(target.as_bytes().to_vec(), "text")),
    };
    // The server sniffs the bytes anyway; guessing saves the common PDF-labelled-as-text case.
    let mime = mime_guess::from_path(target)
        .first_raw()
        .unwrap_or("text/plain")
        .to_string();
    Ok(Read {
        bytes,
        content_type: mime,
        label: target.into(),
    })
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn a_sentence_is_not_a_path_and_a_missing_file_is_not_a_sentence() {
        for prose in [
            "PUID steht bei Microsoft für „Personal User ID“.\nEine zweite Zeile.",
            "buy milk",
            "erinnere mich",
            "siehe https://example.com/x für Details",
            "Zahlung 12/2026 überweisen",
            // A relative slash whose directory is not there: prose.
            "TODO/urgent",
            "24/12",
            "dir/file",
        ] {
            assert!(!looks_like_a_path(prose), "{prose}");
        }
        for path in [
            "notes.pdf",
            "./missing",
            "/etc/hosts",
            "~/notes.md",
            // Absolute, and none of it on disk: still a path, still an error.
            "/no/such/file/anywhere",
            // Relative, and its directory is really there.
            "src/main.rs",
        ] {
            assert!(looks_like_a_path(path), "{path}");
        }
    }

    #[test]
    fn a_target_that_exists_but_cannot_be_read_is_an_error_not_a_note() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("archive");
        std::fs::create_dir(&sub).unwrap();
        let Err(err) = read_target(sub.to_str().unwrap()) else {
            panic!("a directory is not prose");
        };
        assert!(err.to_string().contains("archive"), "{err}");

        let missing = dir.path().join("no-such-file.txt");
        assert!(
            read_target(missing.to_str().unwrap()).is_err(),
            "a path that misses is an error"
        );
        let Ok(read) = read_target("just some words nobody could open") else {
            panic!("a sentence is a note");
        };
        assert_eq!(read.label, "text");
    }

    #[test]
    fn several_arguments_are_several_captures_only_when_each_addresses_something() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.pdf");
        let b = dir.path().join("b.pdf");
        std::fs::write(&a, b"%PDF-1.4\n").unwrap();
        std::fs::write(&b, b"%PDF-1.4\n").unwrap();
        for p in [&a, &b] {
            assert!(addresses_something(p.to_str().unwrap()), "{p:?}");
        }
        assert!(addresses_something("-"));
        assert!(addresses_something("https://example.com/x"));
        assert!(
            addresses_something("notes.pdf"),
            "a lone name with a suffix"
        );
        for word in ["buy", "milk", "erinnere", "zahnarzt"] {
            assert!(!addresses_something(word), "{word}");
        }
    }

    #[test]
    fn a_plain_word_in_the_list_makes_the_whole_line_a_note() {
        let words = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        match reading(&words(&["pay", "12.50", "to", "Anna"])).unwrap() {
            Reading::Prose(line) => assert_eq!(line, "pay 12.50 to Anna"),
            Reading::Targets => panic!("a sentence with a number in it was read as files"),
        }
        assert!(
            matches!(reading(&words(&["12.50"])).unwrap(), Reading::Targets),
            "one argument is still asked of the filesystem"
        );

        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("report.pdf");
        std::fs::write(&real, b"%PDF-1.4\n").unwrap();
        assert!(
            reading(&[
                real.to_string_lossy().into_owned(),
                "notes/typo/q3.pdf".to_string()
            ])
            .is_err(),
            "a mistyped path beside a real one was stored as a note"
        );
    }

    #[test]
    fn a_sentence_longer_than_a_file_name_is_still_prose() {
        let prose = "eine Zeile, die sich wiederholt und wiederholt, ".repeat(6);
        assert!(prose.len() > 255, "the fixture must exceed NAME_MAX");
        let Ok(read) = read_target(&prose) else {
            panic!("a paragraph is a note, not a path");
        };
        assert_eq!(read.label, "text");
        assert_eq!(read.bytes, prose.as_bytes());
    }

    #[tokio::test]
    async fn a_bare_link_cannot_be_a_reminder() {
        let (e, _core) = endpoint().await;
        let err = run_text(
            &e,
            "https://example.com/pay-invoice".into(),
            None,
            None,
            Some("remind"),
            &off(),
        )
        .await
        .expect_err("a link is not a sentence");
        let msg = err.to_string();
        assert!(msg.contains("a reminder"), "{msg}");
        assert!(
            msg.contains("engram -c"),
            "it names the verb that does work: {msg}"
        );
        assert!(!msg.contains("tz"), "and not a field nobody typed: {msg}");

        // Prose that merely opens with a link is prose.
        run_text(
            &e,
            "https://example.com/x is the invoice, pay it".into(),
            None,
            None,
            Some("remind"),
            &off(),
        )
        .await
        .expect("a line of prose is a line of prose");
    }

    #[tokio::test]
    async fn a_sentence_nobody_could_open_is_captured_as_the_note_it_is() {
        let (e, core) = endpoint().await;
        let text = "PUID steht bei Microsoft für „Personal User ID“ (Persönliche Benutzer-ID).\n\
                    Es handelt sich um einen eindeutigen alphanumerischen Code.";
        let ids = run(&e, &[text.to_string()], None, None, &off())
            .await
            .unwrap();
        let stored = core.store.get_corpus(&ids[0]).await.expect("stored");
        assert_eq!(stored.raw_text, text);
    }

    #[tokio::test]
    async fn a_photo_a_document_and_a_link_are_captured_where_this_host_knows_its_zone() {
        // `/capture` refuses the time fields on every non-text branch; the zone must not go there.
        let (e, _core) = endpoint().await;
        assert!(
            local_zone().is_some(),
            "the fixture needs a host that names its zone"
        );
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("shot.png");
        std::fs::write(&png, crate::web::test_support::a_png()).unwrap();

        run(&e, &[png.display().to_string()], None, None, &off())
            .await
            .expect("a photo");

        // The fixture has nothing to fetch, so the reason is asserted: not a refusal before trying.
        for said in [
            run(&e, &["https://example.com/a".into()], None, None, &off())
                .await
                .err(),
            run_piped(&e, "https://example.com/b".into(), None, None, &off())
                .await
                .err(),
        ] {
            let said = said.map(|e| e.to_string()).unwrap_or_default();
            assert!(
                !said.contains("tz only applies"),
                "the door refused the link over a zone: {said}"
            );
        }
    }

    #[tokio::test]
    async fn a_note_still_travels_with_the_zone_it_was_typed_in() {
        let (e, core) = endpoint().await;
        let want = local_zone().expect("the fixture needs a host that names its zone");
        let id = run_text(
            &e,
            "Remind me tomorrow at 9".into(),
            None,
            None,
            Some("remind"),
            &off(),
        )
        .await
        .unwrap();
        assert_eq!(
            core.store.get_corpus(&id).await.unwrap().metadata["tz"],
            want
        );

        let ids = run_piped(&e, "an ordinary note".into(), None, None, &off())
            .await
            .unwrap();
        assert_eq!(
            core.store.get_corpus(&ids[0]).await.unwrap().metadata["tz"],
            want,
            "a pipe of prose is a text capture"
        );
    }

    #[tokio::test]
    async fn a_path_that_misses_is_still_an_error() {
        let (e, _core) = endpoint().await;
        let err = run(&e, &["notes.pdf".into()], None, None, &off())
            .await
            .unwrap_err();
        assert!(
            format!("{err}").contains("notes.pdf"),
            "the file it could not find, not the text: {err}"
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

        // A fresh capture is `raw`, which a worker moves; `NeedsReview` only a person moves.
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
