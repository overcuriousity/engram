//! `-a`: one question, streamed to the terminal as it is written.

use crate::cli::capture::USER_AGENT;
use crate::cli::endpoint::Endpoint;
use crate::error::{Error, Result};

/// Take every complete SSE frame out of `buf`, leaving a partial one behind.
///
/// A chunk from the network splits wherever it likes — mid-event, mid-JSON — so
/// the reader has to be able to say "not yet" and keep what it has. Split out of
/// the request loop because that is the only way to test it against a split that
/// actually happened rather than one that was imagined.
/// One line on stderr, redrawn in place and taken back before anything else is
/// written.
///
/// Its own small type because the rule is not about what it says: whatever is
/// drawn here must be gone before the answer starts, or it survives into the
/// scrollback the answer is read from tomorrow.
#[derive(Default)]
struct Note {
    drawn: bool,
}

impl Note {
    fn show(&mut self, line: Option<String>) {
        let Some(line) = line else { return };
        use std::io::Write;
        eprint!("\r\u{1b}[2K  {line}");
        std::io::stderr().flush().ok();
        self.drawn = true;
    }

    fn clear(&mut self) {
        if self.drawn {
            use std::io::Write;
            eprint!("\r\u{1b}[2K");
            std::io::stderr().flush().ok();
            self.drawn = false;
        }
    }
}

/// Taken back on every way out, not only the two that were written down.
///
/// `clear` was called from the `token` and `error` arms, which leaves the line
/// on screen for the two exits nobody typed: a transport that dies after a
/// `retrieved` frame, where the error is printed straight onto it, and a stream
/// that ends with citations and no token at all. `Stages` and `Fill` have both
/// had this since they were written; the rule is the type's, not the caller's.
impl Drop for Note {
    fn drop(&mut self) {
        self.clear();
    }
}

/// One lamp line, held inside the terminal's width.
///
/// Clipped here rather than in `Note::show`, because by then the text is
/// wrapped in escapes and a count of its characters is not a count of its
/// columns. Four columns go to what is drawn around it: the two of indent
/// `show` prints and the two of the lamp and its space.
fn within(face: &crate::cli::face::Face, lamp: crate::cli::face::Lamp, said: &str) -> String {
    face.lamp_line(
        lamp,
        &crate::cli::face::clip(said, face.width.saturating_sub(4)),
    )
}

/// What a `retrieved` frame says, in words. `None` where the face is off, so a
/// redirected answer receives none of it.
fn retrieved_line(face: &crate::cli::face::Face, data: &serde_json::Value) -> Option<String> {
    if !face.on {
        return None;
    }
    let (got, shown) = (data["retrieved"].as_u64()?, data["shown"].as_u64()?);
    Some(within(
        face,
        crate::cli::face::Lamp::Done,
        &format!("retrieved {got}, showing {shown}"),
    ))
}

/// What a `needs` frame says: the subjects the model named as still missing,
/// which is the second round beginning.
fn needs_line(face: &crate::cli::face::Face, data: &serde_json::Value) -> Option<String> {
    if !face.on {
        return None;
    }
    let said: Vec<&str> = data["queries"]
        .as_array()?
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    if said.is_empty() {
        return None;
    }
    // The subjects are the model's own words and have no bound; two or three
    // named ones routinely pass eighty columns, and `show` erases one physical
    // row, so a wrapped line leaves its head on the screen for the answer to
    // be written under.
    Some(within(
        face,
        crate::cli::face::Lamp::Running,
        &format!("still missing: {}", said.join(", ")),
    ))
}

pub fn frames(buf: &mut String) -> Vec<(String, serde_json::Value)> {
    let mut out = Vec::new();
    while let Some(end) = buf.find("\n\n") {
        let block = buf[..end].to_string();
        buf.drain(..end + 2);
        let mut name = String::new();
        let mut data = String::new();
        for line in block.lines() {
            if let Some(rest) = line.strip_prefix("event: ") {
                name = rest.trim().to_string();
            } else if let Some(rest) = line.strip_prefix("data: ") {
                // A single frame may carry several `data:` lines, which the
                // spec says to join. Ours never does today; reading it
                // correctly costs one `push_str`.
                data.push_str(rest);
            }
        }
        if name.is_empty() {
            // A keep-alive comment, which arrives whenever a slow model thinks
            // for longer than a proxy's idle timeout.
            continue;
        }
        let value = serde_json::from_str(&data).unwrap_or(serde_json::Value::Null);
        out.push((name, value));
    }
    out
}

/// One question, asked and printed as it is answered.
///
/// Takes the `CliArgs` for `--plain` and `--fancy` only: `--tag`, `--category`
/// and `--json` are refused alongside `-a` by clap rather than accepted and
/// dropped. This used to take nothing at all, on the grounds that the face had
/// nothing to draw for a stream of prose — which stopped being true when the
/// stream got a readout and the sources got a shape.
/// Take every complete character out of `pending`, leaving a partial one behind.
///
/// `frames` is this one boundary up: a chunk from the network is cut at
/// neither. Decoding each chunk on its own was the bug this replaces — a
/// boundary falls wherever HTTP framing puts it, including inside a multi-byte
/// sequence, and `from_utf8_lossy` turns the leading half into one replacement
/// character and the trailing half into more. An answer drawn from this base
/// is full of em dashes and umlauts, so that is not a corner case; it is most
/// sentences, some of the time.
///
/// Split out for the same reason `frames` was: a split that actually happened
/// is the only kind worth testing against.
pub fn decode(pending: &mut Vec<u8>) -> Result<String> {
    let whole = match std::str::from_utf8(pending) {
        Ok(_) => pending.len(),
        // A sequence cut by a chunk boundary: `error_len` is `None`, and
        // everything before it is a complete character. Keep the tail.
        Err(e) if e.error_len().is_none() => e.valid_up_to(),
        // Not a boundary — bytes that are not UTF-8 at all. Said rather than
        // papered over: this stream is our own SSE carrying JSON, so this is a
        // broken transport and not a character to approximate.
        Err(e) => {
            return Err(Error::Internal(format!(
                "the answer stream is not UTF-8 at byte {}",
                e.valid_up_to()
            )));
        }
    };
    let head: Vec<u8> = pending.drain(..whole).collect();
    Ok(String::from_utf8(head).expect("checked just above"))
}

/// The sources under an answer, numbered the way the answer cited them and
/// drawn as a tree rooted at the answer.
///
/// The numbering is the server's: `ask` hands the model this list in this
/// order and the model writes `[9]` for the ninth of it. Printing the list
/// without the numbers left the reader holding a citation they could not
/// follow, which is most of what a citation is for.
///
/// The tree is drawn where the terminal can draw it and said in ASCII where it
/// cannot, and neither form carries a claim the other does not: the numbers,
/// the titles and the ids are the content, and the branches are how it is
/// shaped.
pub fn render_citations(hits: &[serde_json::Value], unicode: bool) -> String {
    let (tee, last) = if unicode {
        ("├─", "└─")
    } else {
        ("|-", "`-")
    };
    let mut out = String::from("\nfrom:\n");
    // Right-aligned to the widest number, so the titles line up whether the
    // answer cited three sources or twenty-two.
    let room = hits.len().to_string().len();
    for (i, c) in hits.iter().enumerate() {
        let branch = if i + 1 == hits.len() { last } else { tee };
        out.push_str(&format!(
            "{branch} [{:>room$}] {}  {}\n",
            i + 1,
            c["title"].as_str().unwrap_or("(untitled)"),
            c["artifact_id"].as_str().unwrap_or(""),
        ));
    }
    out
}

/// The ids of those sources, in the order they were numbered, so `--show 9`
/// after an answer reaches what `[9]` meant.
///
/// A citation carrying no `artifact_id` keeps its place as an empty string
/// rather than being dropped: `render_citations` numbers every hit it is given,
/// so skipping one here would shift every rank below it and `--show 9` would
/// open the tenth source. `last::resolve` is what turns the empty place back
/// into a sentence — before that it was sent to the server as an id, which
/// asked for `/api/v1/artifacts/` and answered with a bare 404.
pub fn citation_ids(hits: &[serde_json::Value]) -> Vec<String> {
    hits.iter()
        .map(|c| c["artifact_id"].as_str().unwrap_or_default().to_string())
        .collect()
}

pub async fn run(e: &Endpoint, question: &str, cli: &crate::cli::args::CliArgs) -> Result<i32> {
    use tokio_stream::StreamExt;
    let face = crate::cli::face::Face::decide(
        cli,
        std::io::IsTerminal::is_terminal(&std::io::stdout()),
        std::env::var_os("NO_COLOR").is_some(),
        crate::cli::face::locale().as_deref(),
    );
    let http = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .map_err(|err| Error::Internal(format!("http client: {err}")))?;
    let res = http
        // `door=cli`, exactly as `-s` names its door: a question typed at a
        // shell is recorded, and the log should hold what it was.
        .post(format!("{}?door=cli", e.api("/ask/stream")))
        .bearer_auth(&e.token)
        .json(&serde_json::json!({ "q": question }))
        .send()
        .await
        .map_err(|err| Error::Validation(format!("{err}")))?;
    if !res.status().is_success() {
        return Err(Error::Validation(format!(
            "the question was refused: {}",
            res.status()
        )));
    }

    let mut body = res.bytes_stream();
    // Two buffers, because a chunk from the network is cut at neither
    // boundary that matters here. `pending` holds bytes that are not yet a
    // whole character; `buf` holds characters that are not yet a whole frame.
    let mut pending: Vec<u8> = Vec::new();
    let mut buf = String::new();
    let mut citations = Vec::new();
    let mut said_anything = false;
    // What the ask says about itself while nothing has been written yet. It
    // has emitted `retrieved`, `needs` and `citations` since it was written and
    // this client dropped all three on the floor, drawing a strand on a timer
    // over the gap instead. The readout takes over at the first token, driven
    // by the answer's own arrival rate.
    let mut note = Note::default();
    let mut readout = face.readout();
    let began = std::time::Instant::now();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|err| Error::Validation(format!("{err}")))?;
        pending.extend_from_slice(&chunk);
        buf.push_str(&decode(&mut pending)?);
        for (name, data) in frames(&mut buf) {
            match name.as_str() {
                // A retrieval, said. The pool and what came out of it is the
                // one number a reader can act on: a wide search that showed a
                // handful is the fan-out working.
                "retrieved" => note.show(retrieved_line(&face, &data)),
                // Round two happening, in the model's own words for what the
                // first round missed.
                "needs" => note.show(needs_line(&face, &data)),
                "token" => {
                    if let Some(t) = data["text"].as_str() {
                        use std::io::Write;
                        // The line ends the moment there is something to show,
                        // before a byte of the answer is printed.
                        note.clear();
                        print!("{}", readout.push(t, began.elapsed().as_millis() as u64));
                        // Flushed per token: an answer that appears a
                        // paragraph at a time is an answer that looks stalled.
                        std::io::stdout().flush().ok();
                        said_anything = true;
                    }
                }
                "citations" => citations = data["hits"].as_array().cloned().unwrap_or_default(),
                // Said, not swallowed: an answer that stopped and an answer
                // that failed look identical on a terminal otherwise.
                "error" => {
                    use std::io::Write;
                    note.clear();
                    print!("{}", readout.finish());
                    // Flushed before a word goes to stderr. The erase is a
                    // bare escape with no newline, so stdout's line buffer
                    // holds it: without this the failure prints over a readout
                    // still on screen, and the erase lands at process exit,
                    // eating part of whatever line the cursor is on by then.
                    std::io::stdout().flush().ok();
                    eprintln!(
                        "\n{}",
                        data["error"].as_str().unwrap_or("the answer failed")
                    );
                    return Ok(2);
                }
                _ => {}
            }
        }
    }
    // Taken back before anything else is printed, so no part of it survives
    // into the scrollback the answer is read from tomorrow.
    print!("{}", readout.finish());
    println!();
    if !citations.is_empty() {
        print!("{}", render_citations(&citations, face.unicode && face.on));
        // The numbered list is one `--show` can name a rank out of, exactly as
        // a search's is: a citation nobody can open is half a citation.
        if crate::cli::last::worth_remembering(
            std::io::IsTerminal::is_terminal(&std::io::stdout()),
            false,
        ) {
            crate::cli::last::save(question, citation_ids(&citations));
        }
    }
    // `1` when the base had nothing to say, matching `-s`: a shell branches on
    // it the same way.
    Ok(if said_anything { 0 } else { 1 })
}

#[cfg(test)]
mod tests {

    /// The frames were being sent and dropped on the floor while a strand
    /// animated over the gap. What they say is worth more than what it said.
    #[test]
    fn a_retrieved_frame_becomes_a_line_a_person_can_read() {
        let face = crate::cli::face::Face::decide(
            &crate::cli::args::CliArgs {
                fancy: crate::cli::args::Fancy::Always,
                ..Default::default()
            },
            true,
            false,
            Some("en_US.UTF-8"),
        );
        let line = retrieved_line(
            &face,
            &serde_json::json!({"round": 1, "retrieved": 24, "shown": 6, "dropped": 18, "cliff_at": 6}),
        )
        .expect("a line");
        assert!(line.contains("24") && line.contains('6'), "{line}");
        assert!(
            line.contains("retrieved") && line.contains("showing"),
            "a number with no word beside it is a number nobody can read: {line}"
        );
    }

    #[test]
    fn a_needs_frame_names_what_the_first_round_missed() {
        let face = crate::cli::face::Face::decide(
            &crate::cli::args::CliArgs {
                fancy: crate::cli::args::Fancy::Always,
                ..Default::default()
            },
            true,
            false,
            Some("en_US.UTF-8"),
        );
        let line = needs_line(
            &face,
            &serde_json::json!({"queries": ["journal rotation policy"]}),
        )
        .expect("a line");
        assert!(line.contains("journal rotation policy"), "{line}");
        // A round that named nothing is not a round that happened.
        assert!(needs_line(&face, &serde_json::json!({"queries": []})).is_none());
    }

    /// `show` erases one physical row, so a line that wraps leaves its head on
    /// the screen and the answer is written under a note that was taken back.
    #[test]
    fn a_note_longer_than_the_terminal_is_clipped_rather_than_wrapped() {
        for width in [24usize, 40, 80] {
            let face = crate::cli::face::Face {
                on: true,
                color: false,
                unicode: true,
                width,
            };
            let line = needs_line(
                &face,
                &serde_json::json!({"queries": [
                    "journal rotation policy",
                    "retention window for archived corpora",
                    "who signs off on a deletion"
                ]}),
            )
            .expect("a line");
            // The lamp is already in `line`; `show` adds the two of indent.
            assert!(
                line.chars().count() + 2 <= width,
                "width {width}: over the width: {line:?}"
            );
        }
    }

    /// A redirected answer receives none of it.
    #[test]
    fn a_face_that_is_off_says_nothing_between_the_frames() {
        let face = crate::cli::face::Face::decide(&Default::default(), false, false, None);
        assert!(retrieved_line(&face, &serde_json::json!({"retrieved": 1, "shown": 1})).is_none());
        assert!(needs_line(&face, &serde_json::json!({"queries": ["x"]})).is_none());
    }
    use super::*;

    fn cite(title: Option<&str>, id: &str) -> serde_json::Value {
        serde_json::json!({ "title": title, "artifact_id": id })
    }

    /// The answer cites `[9]`, and until now the list under it was 22 unnumbered
    /// lines: there was no way to tell which one `[9]` meant. The number the
    /// model used is the position in this list, and the list has to say so.
    #[test]
    fn every_source_carries_the_number_the_answer_cited_it_by() {
        let out = render_citations(
            &[
                cite(Some("Physische Extraktion"), "id-a"),
                cite(None, "id-b"),
                cite(Some("JTAG"), "id-c"),
            ],
            true,
        );
        let lines: Vec<&str> = out.lines().filter(|l| l.contains("id-")).collect();
        assert_eq!(lines.len(), 3, "{out}");
        assert!(lines[0].contains("[1]"), "{out}");
        assert!(lines[2].contains("[3]"), "{out}");
        assert!(
            lines[2].contains("JTAG") && lines[2].contains("id-c"),
            "{out}"
        );
        assert!(lines[1].contains("(untitled)"), "{out}");
    }

    /// A reading follows a citation: the numbers under an answer are a list
    /// `--show` can name a rank out of, the same way the ones under a search
    /// are.
    #[test]
    fn the_sources_are_a_list_show_can_read_a_rank_out_of() {
        let ids = citation_ids(&[cite(None, "id-a"), cite(None, "id-b")]);
        assert_eq!(ids, vec!["id-a".to_string(), "id-b".to_string()]);
    }

    #[test]
    fn frames_are_read_out_of_a_buffer_that_splits_mid_event() {
        let mut buf = String::from("event: token\ndata: {\"text\":\"hel\"}\n\nevent: tok");
        let got = frames(&mut buf);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "token");
        assert_eq!(got[0].1["text"], "hel");
        assert_eq!(
            buf, "event: tok",
            "the half-arrived frame is kept for the next read"
        );

        buf.push_str("en\ndata: {\"text\":\"lo\"}\n\n");
        let got = frames(&mut buf);
        assert_eq!(got[0].1["text"], "lo");
        assert!(buf.is_empty());
    }

    #[test]
    fn a_keep_alive_between_frames_is_not_mistaken_for_one() {
        // What axum's `KeepAlive` sends while a slow model thinks.
        let mut buf = String::from(":\n\nevent: token\ndata: {\"text\":\"a\"}\n\n");
        let got = frames(&mut buf);
        assert_eq!(got.len(), 1, "the comment is not a frame: {got:?}");
        assert_eq!(got[0].0, "token");
    }

    #[test]
    fn a_character_split_across_two_chunks_survives() {
        // "held — for review", cut inside the em dash. Three bytes, arriving
        // one and then two, which is exactly what a `from_utf8_lossy` per
        // chunk turned into `held \u{fffd}\u{fffd} for review`.
        let text = "held — for review";
        let dash = text.find('—').expect("the dash is in there");
        let raw = text.as_bytes();

        let mut pending = raw[..dash + 1].to_vec();
        let mut out = decode(&mut pending).expect("valid so far");
        assert_eq!(out, "held ", "the half-arrived character is not printed");
        assert_eq!(pending.len(), 1, "its first byte is kept for the next read");

        pending.extend_from_slice(&raw[dash + 1..]);
        out.push_str(&decode(&mut pending).expect("valid"));
        assert_eq!(out, text);
        assert!(pending.is_empty());
    }

    #[test]
    fn a_byte_that_is_not_utf8_at_all_is_said_rather_than_approximated() {
        // `0xff` begins no sequence, so this is a broken transport rather
        // than a character still on its way.
        let mut pending = b"ok \xff".to_vec();
        assert!(decode(&mut pending).is_err());
    }

    #[tokio::test]
    async fn an_answer_streams_and_the_stream_terminates() {
        let (url, token, _core) = crate::cli::test_support::serve_test_app().await;
        let e = Endpoint { url, token };
        // The test core's synthesizer is a fake, so this asserts the transport
        // and the frame reader rather than the words that come back.
        let code = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            run(&e, "what is stored here", &Default::default()),
        )
        .await
        .expect("the stream must end")
        .expect("the stream must not fail");
        assert!(code == 0 || code == 1, "unexpected exit code {code}");
    }
}
