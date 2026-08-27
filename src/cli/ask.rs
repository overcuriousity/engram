//! `-a`: one question, streamed to the terminal as it is written.

use crate::cli::args::CliArgs;
use crate::cli::capture::USER_AGENT;
use crate::cli::endpoint::Endpoint;
use crate::error::{Error, Result};

/// Take every complete SSE frame out of `buf`, leaving a partial one behind.
///
/// A chunk from the network splits wherever it likes — mid-event, mid-JSON — so
/// the reader has to be able to say "not yet" and keep what it has. Split out of
/// the request loop because that is the only way to test it against a split that
/// actually happened rather than one that was imagined.
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

pub async fn run(e: &Endpoint, question: &str, _cli: &CliArgs) -> Result<i32> {
    use tokio_stream::StreamExt;
    let http = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .map_err(|err| Error::Internal(format!("http client: {err}")))?;
    let res = http
        .post(e.api("/ask/stream"))
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
    let mut buf = String::new();
    let mut citations = Vec::new();
    let mut said_anything = false;
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|err| Error::Validation(format!("{err}")))?;
        buf.push_str(&String::from_utf8_lossy(&chunk));
        for (name, data) in frames(&mut buf) {
            match name.as_str() {
                "token" => {
                    if let Some(t) = data["text"].as_str() {
                        use std::io::Write;
                        print!("{t}");
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
    println!();
    if !citations.is_empty() {
        println!("\nfrom:");
        for c in &citations {
            println!(
                "  \\- {}  {}",
                c["title"].as_str().unwrap_or("(untitled)"),
                c["artifact_id"].as_str().unwrap_or("")
            );
        }
    }
    // `1` when the base had nothing to say, matching `-s`: a shell branches on
    // it the same way.
    Ok(if said_anything { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[tokio::test]
    async fn an_answer_streams_and_the_stream_terminates() {
        let (url, token, _core) = crate::cli::test_support::serve_test_app().await;
        let e = Endpoint { url, token };
        let cli = CliArgs {
            plain: true,
            ..Default::default()
        };
        // The test core's synthesizer is a fake, so this asserts the transport
        // and the frame reader rather than the words that come back.
        let code = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            run(&e, "what is stored here", &cli),
        )
        .await
        .expect("the stream must end")
        .expect("the stream must not fail");
        assert!(code == 0 || code == 1, "unexpected exit code {code}");
    }
}
