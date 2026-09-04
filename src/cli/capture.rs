//! `-c`: put a path, a link or a pipe into the base.

use crate::cli::encode;
use crate::cli::endpoint::Endpoint;
use crate::error::{Error, Result};

/// The client's user agent, so a token's row says what asked for it and a log
/// line says what called.
pub(crate) const USER_AGENT: &str = concat!("engram-cli/", env!("CARGO_PKG_VERSION"));

/// Does this argument address something, or is it a word somebody typed?
///
/// The question `run` asks of a multi-argument invocation, and it is
/// `read_target`'s own reading stated ahead of the read: stdin, a link, a name
/// that is really there, or a shape only a path has. Everything else is prose.
fn addresses_something(target: &str) -> bool {
    target == "-"
        || target.starts_with("http://")
        || target.starts_with("https://")
        || looks_like_a_path(target)
        || std::path::Path::new(target).exists()
}

/// Capture each target in turn, answering with the corpus ids in the order they
/// were given.
///
/// One request per target rather than one multipart body holding all of them: a
/// glob of forty PDFs that fails on the nineteenth should have stored eighteen,
/// and the operator should be told which one stopped it.
///
/// Several arguments mean several captures only where every one of them
/// addresses something. `-c` takes a list so that the shell can glob — `engram
/// -c *.pdf` is one invocation and three corpora — and the same list is what an
/// unquoted note arrives as: `engram -c buy milk` stored two corpora holding
/// the single words "buy" and "milk", exit 0, where before `-c` accepted prose
/// at all it had failed loudly with `buy: No such file or directory`. So the
/// arguments are joined back into the sentence the shell took apart, which is
/// what every other text verb does with its words (`args::read`), and the
/// glob keeps its meaning because a glob is all paths.
///
/// Joined, they are prose outright and are not put back through the path
/// heuristic: "dir/file plus a comment" would find `dir` on disk and be refused
/// as a missing file, which is the failure this is here to remove.
pub async fn run(
    e: &Endpoint,
    targets: &[String],
    title: Option<&str>,
    note: Option<&str>,
    face: &crate::cli::face::Face,
) -> Result<Vec<String>> {
    let http = client()?;
    let mut ids = Vec::new();
    let joined;
    let (targets, as_prose) =
        match targets.len() > 1 && !targets.iter().all(|t| addresses_something(t)) {
            true => {
                joined = [targets.join(" ")];
                (&joined[..], true)
            }
            false => (targets, false),
        };
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
            origin: None,
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
            origin: None,
            intent: None,
        },
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

/// The zone this process is in, or none where the platform cannot say — or
/// says something the server cannot read.
///
/// `/capture` parses `tz` with `chrono_tz` and answers 400 for anything it
/// will not take, and a host has several ways to name a zone that is not an
/// IANA identifier: `/etc/localtime` symlinked into the leap-second tree
/// (`right/Europe/Berlin`), a hand-written `/etc/timezone`, a `TZ` holding a
/// POSIX rule. Sent on regardless, that failed every `-c`, `-r`, `-j` and
/// piped capture from that host, naming a field the operator never typed.
///
/// So the same parser this end, and where it will not take the name the zone
/// is simply not sent: the server then reads dates in its own zone, which is
/// what it does for every client that cannot name one at all.
pub(crate) fn local_zone() -> Option<String> {
    let name = iana_time_zone::get_timezone().ok()?;
    if name.parse::<chrono_tz::Tz>().is_err() {
        tracing::debug!(zone = %name, "this host names a zone the server cannot read; sending none");
        return None;
    }
    Some(name)
}

/// The zone to send for a capture whose kind this end has not been told — a
/// path, a link, a pipe — which is to say: only where the server will take it.
///
/// `/capture` refuses the three time fields on every branch that is not a
/// verbatim text capture, because a PDF, a photo and a fetched page are read on
/// the server's own terms and a zone attached to one is a parameter that would
/// be silently dropped. Sent unconditionally, as it was, this made every
/// `engram -c report.pdf`, `-c photo.jpg`, `-c https://example.com` and
/// `echo https://x | engram` fail with `tz only applies to a text capture` on
/// any host that can name its zone at all.
///
/// The condition is `refuse_time_fields`' own, mirrored: text/plain, and not a
/// body that is nothing but a link — which the server reads as a fetch.
fn zone_for(content_type: &str, bytes: &[u8]) -> Option<String> {
    if !content_type.starts_with("text/plain") {
        return None;
    }
    let is_a_link = std::str::from_utf8(bytes)
        .ok()
        .is_some_and(|t| crate::web::api::only_a_url(t).is_some());
    if is_a_link {
        return None;
    }
    local_zone()
}

/// One piece of text, sent as the reminder or the entry a verb said it is.
///
/// A body that is nothing but a link is refused here rather than sent. The
/// server reads such a body as a page to fetch, and a fetched page carries none
/// of the three fields this path sets — `refuse_time_fields` answers with a 400
/// naming `tz, intent`, which is two fields the operator did not type and one
/// verb they did. Said in the client, in terms of what they wrote: `-c` is the
/// verb that captures a link.
pub async fn run_text(
    e: &Endpoint,
    text: String,
    title: Option<&str>,
    note: Option<&str>,
    origin: Option<&str>,
    intent: Option<&str>,
    face: &crate::cli::face::Face,
) -> Result<String> {
    if crate::web::api::only_a_url(&text).is_some() {
        let what = if intent == Some("remind") {
            "a reminder"
        } else {
            "a journal entry"
        };
        return Err(Error::Validation(format!(
            "a link on its own is fetched and read by the server, so it cannot be {what}. \
             Capture it with `engram -c`, then say what you want remembered about it."
        )));
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
            origin,
            intent,
        },
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
        for (key, value) in [
            ("tz", meta.tz),
            ("origin", meta.origin),
            ("intent", meta.intent),
        ] {
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

/// What one `-c` target turned out to be: the bytes to send, what to call
/// them to the server, and what to call it back to the operator when something
/// goes wrong. The label is the path for a file and `text` for a sentence —
/// putting a whole paragraph in front of every error message it can produce is
/// no way to report one.
struct Read {
    bytes: Vec<u8>,
    content_type: String,
    label: String,
}

/// Does this argument look like an attempt at a path? A slash or a `~` says so
/// outright, and so does a lone word carrying an extension. Prose does not:
/// it has spaces in it, and a sentence's final full stop is not a suffix.
///
/// This is only ever asked about something that is *not* on disk, and only to
/// choose between two answers: refuse it as a missing file, or store it as a
/// note. `notes.pdf` typed with the file in the other directory should be the
/// error it is, and `engram -c "buy milk"` should be the note it is.
/// Whitespace argues against a path: a sentence that happens to carry a slash
/// — "siehe https://example.com/x für Details", "Zahlung 12/2026 überweisen" —
/// is exactly the prose the text fallthrough exists for, and the slash branch
/// used to claim it anyway, failing the capture with the `File name too long`
/// this heuristic was written to remove.
///
/// It does not settle it, though, and the answer is in two parts.
///
/// An *absolute* argument is a path and nothing else. Prose does not begin at
/// the root, and `~/` is the same statement typed for a shell that did not
/// expand it — quoted, on purpose. `engram -c /no/such/file` is a miss worth
/// reporting, not a note, however little of the path is really there. That also
/// closes the case this heuristic left open: `~/Documents/My Notes/plan.pdf`,
/// quoted with a typo in it, used to be stored as a note whose whole text was
/// the path somebody typed, exit 0, with nothing said.
///
/// A *relative* one with a slash in it is the genuinely ambiguous shape, and
/// the filesystem answers what the form cannot: it is a path exactly when its
/// parent directory is really there. Neither prose case above has one — "siehe
/// https:/example.com", "Zahlung 12" — and both stay the notes they are.
/// Whitespace does not enter into it. Requiring it as proof of prose refused a
/// whole class of ordinary short notes as missing files — `engram -c
/// "TODO/urgent"` and `engram -c 24/12` exited 2 with "No such file or
/// directory" — while the examples above survived only by having spaces in
/// them. The price is `dir/file`, meant as a path, typed where `dir` is not:
/// that is stored as a note, because nothing on disk contradicts it and
/// nothing in the shape does either.
fn looks_like_a_path(target: &str) -> bool {
    let expanded = expand_home(target);
    // Nobody's prose begins at the root. `~/` is the same statement typed for
    // a shell that did not expand it, which is to say quoted on purpose.
    if expanded.is_absolute() || target.starts_with("~/") {
        return true;
    }
    // A lone word carrying an extension and naming no directory: `notes.pdf`,
    // typed with the file somewhere else. Its parent is the working directory,
    // which is always there, so the filesystem has nothing to add and the shape
    // settles it on its own.
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

/// A leading `~/` resolved, for the directory question above and nothing else:
/// the read itself goes through the argument as typed, the way every other
/// program that is handed an unexpanded tilde does. A shell expands it before
/// this ever sees it; what reaches here was quoted.
fn expand_home(target: &str) -> std::path::PathBuf {
    match target.strip_prefix("~/") {
        Some(rest) => match std::env::home_dir() {
            Some(home) => home.join(rest),
            None => std::path::PathBuf::from(target),
        },
        None => std::path::PathBuf::from(target),
    }
}

/// The bytes to send and what to call them.
///
/// A link is sent as `text/plain` and the server decides it is a link, which is
/// the same decision a share sheet's body gets: one rule about what a bare URL
/// means, in one place, rather than one per client.
///
/// And what is not a file is what a person typed. `engram -c "PUID steht bei
/// Microsoft für…"` is the same gesture as piping that paragraph in, and
/// answering it with `Filename too long` is the client insisting on a reading
/// of the argument that nothing about the argument supports.
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
        // A path that misses is an error; a sentence was never a path.
        //
        // Keyed on `NotFound` and not on the error alone: every other kind
        // says the target *is* a file and could not be read. `engram -c
        // archive` in a directory holding one answered `EISDIR`, which fell
        // through here and captured the word "archive" as a note, exit 0 —
        // and an unreadable file (`EACCES`) did the same. Only "there is
        // nothing at this name" leaves room for the reading that it was prose.
        // `ENAMETOOLONG` is the same answer as `NotFound` for this purpose:
        // there is no file at that name and there can be none. A sentence of
        // more than 255 bytes — one ordinary paragraph — exited 2 with "File
        // name too long" and the whole paragraph echoed back as the path.
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
    // The extension is what this end knows; the server sniffs the bytes anyway.
    // Guessing wrong here costs nothing, and guessing at all saves the common
    // case of a PDF arriving labelled as text.
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

    #[test]
    fn a_sentence_is_not_a_path_and_a_missing_file_is_not_a_sentence() {
        for prose in [
            "PUID steht bei Microsoft für „Personal User ID“.\nEine zweite Zeile.",
            "buy milk",
            "erinnere mich",
            "siehe https://example.com/x für Details",
            "Zahlung 12/2026 überweisen",
            // A relative slash whose directory is not there. Prose, and the
            // shape a short note reaches for: `TODO/urgent`, `24/12`.
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

    /// Only "there is nothing at this name" leaves room for the reading that
    /// the argument was prose. Every other error says the target *is* a file
    /// and could not be read, and swallowing those captured the word the
    /// operator typed as a note and exited 0 — `engram -c archive` in a
    /// directory holding one used to report `archive: Is a directory`.
    #[test]
    fn a_target_that_exists_but_cannot_be_read_is_an_error_not_a_note() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("archive");
        std::fs::create_dir(&sub).unwrap();
        let Err(err) = read_target(sub.to_str().unwrap()) else {
            panic!("a directory is not prose");
        };
        assert!(err.to_string().contains("archive"), "{err}");

        // A path that misses is still an error, and a sentence is still prose.
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

    /// The shell takes an unquoted note apart, and `-c` used to keep the
    /// pieces.
    ///
    /// `num_args = 1..` is there so a glob is one invocation, and it is also
    /// what an unquoted sentence arrives as: `engram -c buy milk` stored two
    /// corpora holding the words "buy" and "milk", exit 0. Before `-c` took
    /// prose at all the same command had failed loudly with `buy: No such file
    /// or directory`, which is the worse failure to have replaced with a
    /// quieter one.
    #[test]
    fn several_arguments_are_several_captures_only_when_each_addresses_something() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.pdf");
        let b = dir.path().join("b.pdf");
        std::fs::write(&a, b"%PDF-1.4\n").unwrap();
        std::fs::write(&b, b"%PDF-1.4\n").unwrap();
        // The glob: every argument names a file, so each is its own capture.
        for p in [&a, &b] {
            assert!(addresses_something(p.to_str().unwrap()), "{p:?}");
        }
        assert!(addresses_something("-"));
        assert!(addresses_something("https://example.com/x"));
        assert!(
            addresses_something("notes.pdf"),
            "a lone name with a suffix"
        );
        // And the prose the shell split.
        for word in ["buy", "milk", "erinnere", "zahnarzt"] {
            assert!(!addresses_something(word), "{word}");
        }
    }

    /// A paragraph is longer than a file name may be, and the kernel says so
    /// with `ENAMETOOLONG` rather than `ENOENT`. Keyed on `NotFound` alone,
    /// the fallthrough missed it: a 265-byte sentence exited 2 with "File
    /// name too long" and the whole paragraph echoed back as a path.
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

    /// A body that is one link is read by the server as a page to fetch, and a
    /// fetched page carries none of the fields this path sets. The server said
    /// so by naming `tz, intent` — two fields the operator never typed and one
    /// verb they did.
    #[tokio::test]
    async fn a_bare_link_cannot_be_a_reminder_or_an_entry() {
        let (e, _core) = endpoint().await;
        for (intent, origin, word) in [
            (Some("remind"), None, "a reminder"),
            (None, Some("journal"), "a journal entry"),
        ] {
            let err = run_text(
                &e,
                "https://example.com/pay-invoice".into(),
                None,
                None,
                origin,
                intent,
                &off(),
            )
            .await
            .expect_err("a link is not a sentence");
            let msg = err.to_string();
            assert!(msg.contains(word), "{msg}");
            assert!(
                msg.contains("engram -c"),
                "it names the verb that does work: {msg}"
            );
            assert!(!msg.contains("tz"), "and not a field nobody typed: {msg}");
        }

        // Prose that merely opens with a link is prose, and still goes.
        run_text(
            &e,
            "https://example.com/x is the invoice, pay it".into(),
            None,
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
        // `engram -c "PUID steht bei Microsoft für…"` is the same gesture as
        // piping that paragraph in; `Filename too long` was the client
        // insisting on a reading nothing about the argument supports.
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
        // The three time fields are refused on every branch of `/capture` that
        // is not a verbatim text capture, and the zone used to be attached to
        // every target regardless. On any host that can name its zone — which
        // is every ordinary desktop — this made `-c report.pdf`, `-c photo.jpg`
        // and `-c https://…` fail outright with `tz only applies to a text
        // capture`. The one branch the CLI tests exercised was `.txt`, which is
        // the one branch that accepts it.
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

        // A link is a fetch, and this fixture has nothing to fetch — so it is
        // the *reason* that is asserted. What must not come back is the door
        // refusing the request before it ever tried.
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
