//! `-a`: one question, streamed to the terminal as it is written.

use crate::cli::endpoint::Endpoint;
use crate::cli::endpoint::client;
use crate::error::{Error, Result};

/// One line on stderr, redrawn in place; gone before the answer starts so it never reaches
/// the scrollback.
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

/// Taken back on every way out, including a dying transport and a stream with no token.
impl Drop for Note {
    fn drop(&mut self) {
        self.clear();
    }
}

/// One lamp line within the terminal width; clipped before escapes make counting columns wrong.
fn within(face: &crate::cli::face::Face, lamp: crate::cli::face::Lamp, said: &str) -> String {
    face.lamp_line(
        lamp,
        &crate::cli::face::clip(said, face.width.saturating_sub(4), face.unicode),
    )
}

/// What a `retrieved` frame says; `None` where the face is off.
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

/// What a `needs` frame says: the subjects the model named as still missing.
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
    // Unbounded model words: `show` erases one physical row, so a wrapped line would stay.
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
                // The SSE spec joins several `data:` lines in one frame.
                data.push_str(rest);
            }
        }
        if name.is_empty() {
            // A keep-alive comment, sent while a slow model outlasts a proxy's idle timeout.
            continue;
        }
        let value = serde_json::from_str(&data).unwrap_or(serde_json::Value::Null);
        out.push((name, value));
    }
    out
}

/// Take every complete character out of `pending`, leaving a partial one: a network chunk may
/// cut a multi-byte sequence, which per-chunk `from_utf8_lossy` mangles.
pub fn decode(pending: &mut Vec<u8>) -> Result<String> {
    let whole = match std::str::from_utf8(pending) {
        Ok(_) => pending.len(),
        // A sequence cut by a chunk boundary: everything before it is complete. Keep the tail.
        Err(e) if e.error_len().is_none() => e.valid_up_to(),
        // Not UTF-8 at all: our own SSE carries JSON, so this is a broken transport.
        Err(e) => {
            return Err(Error::Internal(format!(
                "the answer stream is not UTF-8 at byte {}",
                e.valid_up_to()
            )));
        }
    };
    let head: Vec<u8> = pending.drain(..whole).collect();
    Ok(String::from_utf8_lossy(&head).into_owned())
}

/// The sources under an answer, numbered as the answer cited them, drawn as a tree (ASCII where
/// the terminal cannot draw one).
pub fn render_citations(hits: &[serde_json::Value], unicode: bool) -> String {
    let (tee, last) = if unicode {
        ("├─", "└─")
    } else {
        ("|-", "`-")
    };
    let mut out = String::from("\nfrom:\n");
    let room = hits.len().to_string().len();
    for (i, c) in hits.iter().enumerate() {
        let branch = if i + 1 == hits.len() { last } else { tee };
        // Named by its note when it has no heading, by its own opening when it has no note.
        let title = match c["title"].as_str() {
            Some(t) => t.to_string(),
            None => crate::web::markdown::stand_in_title(c["text"].as_str().unwrap_or(""), 60),
        };
        out.push_str(&format!(
            "{branch} [{:>room$}] {title}  {}\n",
            i + 1,
            c["artifact_id"].as_str().unwrap_or(""),
        ));
    }
    out
}

/// The ids of those sources in citation order, so `--show 9` reaches `[9]`. An uncited source
/// keeps its place as an empty string, or every later rank would shift.
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
    let http = client()?;
    let res = http
        // `door=cli`, as `-s` names its door.
        .post(format!("{}?door=cli", e.api("/ask/stream")))
        .bearer_auth(&e.token)
        .json(&serde_json::json!({ "q": question }))
        .send()
        .await
        .map_err(|err| e.unreachable(err))?;
    if !res.status().is_success() {
        return Err(Error::Validation(format!(
            "the question was refused: {}",
            res.status()
        )));
    }

    let mut body = res.bytes_stream();
    // `pending`: bytes not yet a whole character; `buf`: characters not yet a whole frame.
    let mut pending: Vec<u8> = Vec::new();
    let mut buf = String::new();
    let mut citations = Vec::new();
    let mut said_anything = false;
    // Retrieval and second-round notes until the first token; the readout takes over from there.
    let mut note = Note::default();
    let mut readout = face.readout();
    let began = std::time::Instant::now();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|err| Error::Validation(format!("{err}")))?;
        pending.extend_from_slice(&chunk);
        buf.push_str(&decode(&mut pending)?);
        for (name, data) in frames(&mut buf) {
            match name.as_str() {
                "retrieved" => note.show(retrieved_line(&face, &data)),
                "needs" => note.show(needs_line(&face, &data)),
                "token" => {
                    if let Some(t) = data["text"].as_str() {
                        use std::io::Write;
                        note.clear();
                        print!("{}", readout.push(t, began.elapsed().as_millis() as u64));
                        // Flushed per token, or the answer looks stalled.
                        std::io::stdout().flush().ok();
                        said_anything = true;
                    }
                }
                "citations" => citations = data["hits"].as_array().cloned().unwrap_or_default(),
                // Said: a stopped answer and a failed one look alike on a terminal.
                "error" => {
                    use std::io::Write;
                    note.clear();
                    print!("{}", readout.finish());
                    // The erase has no newline and waits in stdout's buffer: flush before stderr.
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
    // Here, not in the drop: after citations with no token the drop's erase hits the wrong row.
    note.clear();
    print!("{}", readout.finish());
    println!();
    if !citations.is_empty() {
        print!("{}", render_citations(&citations, face.unicode && face.on));
        // The numbered citations are a list `--show` can name a rank from, as a search's is.
        if crate::cli::last::worth_remembering(
            std::io::IsTerminal::is_terminal(&std::io::stdout()),
            false,
        ) {
            crate::cli::last::save(question, citation_ids(&citations));
        }
    }
    // `1` when the base had nothing to say, matching `-s`.
    Ok(if said_anything { 0 } else { 1 })
}

#[cfg(test)]
mod tests {

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
        assert!(needs_line(&face, &serde_json::json!({"queries": []})).is_none());
    }

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

    #[test]
    fn a_face_that_is_off_says_nothing_between_the_frames() {
        let face = crate::cli::face::Face::decide(&Default::default(), false, false, None);
        assert!(retrieved_line(&face, &serde_json::json!({"retrieved": 1, "shown": 1})).is_none());
        assert!(needs_line(&face, &serde_json::json!({"queries": ["x"]})).is_none());
    }
    use super::*;

    fn cite(title: Option<&str>, id: &str) -> serde_json::Value {
        serde_json::json!({ "title": title, "artifact_id": id, "text": format!("the opening of {id}") })
    }

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
        assert!(lines[1].contains("the opening of id-b"), "{out}");
        assert!(!out.contains("untitled"), "{out}");
    }

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
        // Cut inside the em dash: one byte, then two.
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
        // `0xff` begins no sequence.
        let mut pending = b"ok \xff".to_vec();
        assert!(decode(&mut pending).is_err());
    }

    #[tokio::test]
    async fn an_answer_streams_and_the_stream_terminates() {
        let (url, token, _core) = crate::cli::test_support::serve_test_app().await;
        let e = Endpoint { url, token };
        // The test core's synthesizer is a fake, so this asserts the transport and frame reader.
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
