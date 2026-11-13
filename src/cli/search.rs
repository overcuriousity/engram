//! `-s`: a ranked list in a terminal, saying everything the rail says.

use crate::cli::args::CliArgs;
use crate::cli::encode;
use crate::cli::endpoint::Endpoint;
use crate::cli::endpoint::client;
use crate::core::search::SearchResult;
use crate::error::{Error, Result};

/// The URL a search is asked for at; split out so a test can assert the door.
pub fn query_url(e: &Endpoint, limit: Option<usize>, query: &str, cli: &CliArgs) -> String {
    url_for(e, "/search", limit, query, cli)
}

pub fn stream_url(e: &Endpoint, limit: Option<usize>, query: &str, cli: &CliArgs) -> String {
    url_for(e, "/search/stream", limit, query, cli)
}

fn url_for(e: &Endpoint, path: &str, limit: Option<usize>, query: &str, cli: &CliArgs) -> String {
    // `door=cli`: a shell query is composed before anything came back, the least contaminated kind.
    let mut url = format!("{}?q={}&door=cli", e.api(path), encode(query));
    if let Some(n) = limit {
        url.push_str(&format!("&limit={n}"));
    }
    if !cli.tags.is_empty() {
        url.push_str(&format!("&tags={}", encode(&cli.tags.join(","))));
    }
    if let Some(c) = &cli.category {
        url.push_str(&format!("&category={}", encode(c)));
    }
    url
}

pub async fn run(e: &Endpoint, limit: Option<usize>, query: &str, cli: &CliArgs) -> Result<i32> {
    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let face = crate::cli::face::Face::decide(
        cli,
        is_tty,
        std::env::var_os("NO_COLOR").is_some(),
        crate::cli::face::locale().as_deref(),
    );
    // `--json` never streams: its reader wants the server's own body.
    let streamed = match cli.json {
        true => None,
        false => streaming(e, limit, query, cli, &face).await?,
    };
    let (hits, elapsed, body) = match streamed {
        Some((hits, ms)) => (hits, Some(ms), None),
        // The door every server has had: the stream route is unknown, failed before a frame, or the
        // search was refused, which this door explains.
        None => {
            let (hits, ms, body) = plain(e, limit, query, cli).await?;
            (hits, Some(ms), Some(body))
        }
    };

    // Written before printing, so screen and disk agree; see `last::worth_remembering`.
    if crate::cli::last::worth_remembering(is_tty, cli.json) {
        crate::cli::last::save(query, hits.iter().map(|h| h.artifact_id.clone()).collect());
    }

    match body {
        // A bare array, not the `{items, next}` envelope: scripts pipe `--json` into `jq`, and the
        // envelope says nothing on a search.
        Some(items) if cli.json => println!("{items}"),
        _ => print!("{}", face.render(&hits, elapsed)),
    }
    // `1` for nothing found, so `engram -s "x" || …` is a usable branch.
    Ok(if hits.is_empty() { 1 } else { 0 })
}

/// The streaming door: hits and round-trip time, or `None` to ask the plain door instead.
/// After the first `stage` frame a failure is an error: the server may have recorded the search.
async fn streaming(
    e: &Endpoint,
    limit: Option<usize>,
    query: &str,
    cli: &CliArgs,
    face: &crate::cli::face::Face,
) -> Result<Option<(Vec<SearchResult>, u128)>> {
    use tokio_stream::StreamExt as _;
    let http = client()?;
    let began = std::time::Instant::now();
    let res = http
        .get(stream_url(e, limit, query, cli))
        .bearer_auth(&e.token)
        .send()
        .await;
    let res = match res {
        Ok(r) if r.status().is_success() => r,
        _ => return Ok(None),
    };

    let mut stages = face.stages();
    let mut body = res.bytes_stream();
    // Two buffers: a network chunk is cut at neither the character nor the frame boundary.
    let mut pending: Vec<u8> = Vec::new();
    let mut buf = String::new();
    let mut hits: Option<Vec<SearchResult>> = None;
    // Set from the first stage frame and never unset: the search may be recorded over there.
    let mut ran = false;
    while let Some(chunk) = body.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(_) if !ran => return Ok(None),
            Err(err) => return Err(Error::Validation(format!("{err}"))),
        };
        pending.extend_from_slice(&chunk);
        buf.push_str(&crate::cli::ask::decode(&mut pending)?);
        for (name, data) in crate::cli::ask::frames(&mut buf) {
            match name.as_str() {
                "stages" => {
                    let named: Vec<crate::core::search::SearchStage> =
                        serde_json::from_value(data["stages"].clone()).unwrap_or_default();
                    stages.start(&named);
                }
                "stage" => {
                    ran = true;
                    if let Ok(now) = serde_json::from_value(data["stage"].clone()) {
                        stages.show(now);
                    }
                }
                "results" => {
                    ran = true;
                    hits = serde_json::from_value(data["results"].clone()).ok();
                }
                // The plain door reruns it and says why; a refusal recorded nothing.
                "error" => return Ok(None),
                _ => {}
            }
        }
    }
    // Taken back before a result lands on top of half a stage line.
    stages.clear();
    match (hits, ran) {
        (Some(h), _) => Ok(Some((h, began.elapsed().as_millis()))),
        // The search started and its results never arrived; it may be recorded, so do not rerun it.
        (None, true) => Err(Error::Validation(
            "the search ran and its results did not arrive".into(),
        )),
        (None, false) => Ok(None),
    }
}

/// The door every server has had, and the one that says why a search was refused; also the body.
async fn plain(
    e: &Endpoint,
    limit: Option<usize>,
    query: &str,
    cli: &CliArgs,
) -> Result<(Vec<SearchResult>, u128, String)> {
    let http = client()?;
    let began = std::time::Instant::now();
    let res = http
        .get(query_url(e, limit, query, cli))
        .bearer_auth(&e.token)
        .send()
        .await
        .map_err(|err| e.unreachable(err))?;
    let status = res.status();
    let body = res
        .text()
        .await
        .map_err(|err| Error::Validation(format!("{err}")))?;
    if !status.is_success() {
        let said: serde_json::Value =
            serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
        return Err(Error::Validation(
            said["error"]
                .as_str()
                .unwrap_or("the search was refused without saying why")
                .into(),
        ));
    }
    // `next` is always null on a search, so it is not read.
    let mut body: serde_json::Value =
        serde_json::from_str(&body).map_err(|err| Error::Internal(format!("results: {err}")))?;
    let items = items_of(&mut body);
    let hits: Vec<SearchResult> = serde_json::from_value(items.clone())
        .map_err(|err| Error::Internal(format!("results: {err}")))?;
    // The server's rows, not re-shaped by this client.
    Ok((hits, began.elapsed().as_millis(), items.to_string()))
}

/// The rows out of the list envelope. `get_mut`, not indexing: indexing a `Value` panics on an
/// older server's bare array.
fn items_of(body: &mut serde_json::Value) -> serde_json::Value {
    body.get_mut("items")
        .map(serde_json::Value::take)
        .unwrap_or(serde_json::Value::Null)
}

/// The form a pipe, a test and a script see: no colour, motion or non-ASCII, every claim in words.
pub fn render_plain(hits: &[SearchResult]) -> String {
    let mut out = String::new();
    for (i, h) in hits.iter().enumerate() {
        let mark = if h.past_cliff { "." } else { " " };
        // Named by its note when it has no heading, by its own opening when it has no note.
        let title = match &h.title {
            Some(t) => t.clone(),
            None => crate::web::markdown::stand_in_title(&h.text, 60),
        };
        out.push_str(&format!(
            "{mark}{:>2} {:.2}  {title}  {}\n",
            i + 1,
            h.score,
            h.artifact_id
        ));
        if let Some(said) = badges(h) {
            out.push_str(&format!("      [{said}]\n"));
        }
        for line in body_lines(&h.text, 3) {
            out.push_str(&format!("      {line}\n"));
        }
        out.push('\n');
    }
    out
}

/// The first `n` non-blank lines of a hit's text, unindented: a blank must not spend the clip.
pub(crate) fn body_lines(text: &str, n: usize) -> impl Iterator<Item = &str> {
    text.lines()
        .map(str::trim_end)
        .map(str::trim_start)
        .filter(|l| !l.is_empty())
        .take(n)
}

/// A hit's text wrapped to `width`, up to `budget` display lines, not source lines.
pub(crate) fn excerpt(text: &str, budget: usize, width: usize) -> Vec<String> {
    let room = width.max(24);
    let mut out: Vec<String> = Vec::new();
    for line in body_lines(text, budget) {
        // Each source line starts a new display line, so a heading never runs into its paragraph.
        let mut fresh = true;
        for word in line.split_whitespace() {
            match out.last_mut() {
                // `+ 1` is the joining space.
                Some(last) if !fresh && last.chars().count() + 1 + word.chars().count() <= room => {
                    last.push(' ');
                    last.push_str(word);
                }
                _ => {
                    if out.len() == budget {
                        return out;
                    }
                    // A word longer than the line gets the line to itself and is cut there.
                    out.push(word.chars().take(room).collect());
                    fresh = false;
                }
            }
        }
    }
    out
}

/// What this hit is, in words; shared with the drawn rendering so the two cannot drift.
pub(crate) fn badges(h: &SearchResult) -> Option<String> {
    let mut said: Vec<&str> = Vec::new();
    let due_word = h.due_in.as_ref().map(|d| format!("due {d}"));
    if h.past_cliff {
        said.push("past the cliff");
    }
    if h.weak {
        said.push("loose match");
    }
    if h.model_written {
        said.push("model-written");
    }
    if let Some(d) = &due_word {
        said.push(d);
    }
    if h.primed {
        said.push("primed");
    }
    (!said.is_empty()).then(|| said.join(", "))
}

#[cfg(test)]
pub(crate) mod fixture {
    use crate::core::search::SearchResult;

    /// One hit with only the fields a rendering reads; shared with the face's tests.
    pub(crate) fn hit(title: &str, score: f32, weak: bool, past_cliff: bool) -> SearchResult {
        SearchResult {
            artifact_id: format!("art-{title}"),
            corpus_id: "corpus-1".into(),
            title: Some(title.into()),
            text: format!("the body of {title}"),
            category: None,
            tags: vec![],
            score,
            status: Some(crate::store::artifacts::ArtifactStatus::Active),
            superseded_by: None,
            last_verified_at: None,
            weak,
            model_written: false,
            synthesized: false,
            origin_count: 0,
            primed: false,
            in_sitting: false,
            due_at: None,
            due_in: None,
            past_cliff,
            retired: false,
            similarity: None,
            borrowed_name: false,
            via: None,
            reason: None,
            disagrees_with: Vec::new(),
            explanation: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fixture::hit;

    #[test]
    fn an_older_servers_bare_array_is_not_a_panic() {
        let mut bare = serde_json::json!([{ "id": "a" }]);
        assert!(items_of(&mut bare).is_null());
        let mut enveloped = serde_json::json!({ "items": [{ "id": "a" }], "next": null });
        assert_eq!(items_of(&mut enveloped), serde_json::json!([{ "id": "a" }]));
    }

    #[test]
    fn the_plain_form_clips_to_lines_that_carry_text() {
        let mut h = hit("a", -4.18, false, false);
        h.text = "One\n\nTwo\n\nThree\n\nFour".into();
        let out = render_plain(&[h]);
        assert!(out.contains("Three"), "{out}");
        assert!(
            !out.contains("Four"),
            "a fourth line is past the budget: {out}"
        );
    }

    #[test]
    fn the_plain_form_holds_no_escape_byte_at_all() {
        let out = render_plain(&[hit("a", 0.8, false, false), hit("b", 0.2, true, true)]);
        assert!(
            !out.contains('\u{1b}'),
            "an escape reached the plain form: {out:?}"
        );
        assert!(out.is_ascii(), "a glyph outside ASCII reached it: {out:?}");
    }

    #[test]
    fn the_cliff_and_the_loose_match_are_words_not_colours() {
        let out = render_plain(&[hit("a", 0.8, false, false), hit("b", 0.2, true, true)]);
        assert!(out.contains("past the cliff"), "{out}");
        assert!(out.contains("loose match"), "{out}");
    }

    #[test]
    fn a_hit_prints_its_rank_score_title_and_id() {
        let out = render_plain(&[hit("a", 0.83, false, false)]);
        assert!(out.contains(" 1 "), "{out}");
        assert!(out.contains("0.83"), "{out}");
        assert!(out.contains("art-a"), "{out}");
        assert!(out.contains("the body of a"), "{out}");
    }

    #[test]
    fn a_hit_with_no_title_is_named_by_its_opening_and_never_untitled() {
        let mut h = hit("a", 0.83, false, false);
        h.title = None;
        let out = render_plain(&[h]);
        assert!(out.contains("the body of a"), "{out}");
        assert!(!out.contains("untitled"), "{out}");
    }

    #[tokio::test]
    async fn nothing_found_exits_one() {
        let (url, token, _core) = crate::cli::test_support::serve_test_app().await;
        let e = Endpoint { url, token };
        let cli = CliArgs {
            plain: true,
            ..Default::default()
        };
        assert_eq!(
            run(&e, None, "nothing has been stored in this base yet", &cli)
                .await
                .unwrap(),
            1,
            "a shell branches on this"
        );
    }

    #[tokio::test]
    async fn both_doors_answer_one_search_the_same_way() {
        let (url, token, core) = crate::cli::test_support::serve_test_app().await;
        let e = Endpoint { url, token };
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    text: "the journal is a ring buffer on disk".into(),
                    title: Some("journald".into()),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();
        for c in &made {
            crate::jobs::embed::run(&core, &c.id).await.unwrap();
        }
        let cli = CliArgs {
            plain: true,
            ..Default::default()
        };
        let face = crate::cli::face::Face::decide(&cli, false, false, None);
        let (streamed, _ms) = streaming(&e, None, "journal", &cli, &face)
            .await
            .unwrap()
            .expect("the streaming door answered");
        let (plainly, _ms, _body) = plain(&e, None, "journal", &cli).await.unwrap();
        assert!(!streamed.is_empty(), "the base holds one artifact");
        assert_eq!(
            streamed.iter().map(|h| &h.artifact_id).collect::<Vec<_>>(),
            plainly.iter().map(|h| &h.artifact_id).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn a_door_that_is_not_there_falls_back_rather_than_failing() {
        let (url, token, _core) = crate::cli::test_support::serve_test_app().await;
        // A path the router does not know: the same 404 an older server gives.
        let e = Endpoint {
            url: format!("{url}/nowhere"),
            token,
        };
        let cli = CliArgs {
            plain: true,
            ..Default::default()
        };
        let face = crate::cli::face::Face::decide(&cli, false, false, None);
        assert!(
            streaming(&e, None, "journal", &cli, &face)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_stream_that_dies_after_a_stage_is_not_run_a_second_time() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a port");
        let addr = listener.local_addr().expect("the port it got");
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            let (mut sock, _) = listener.accept().await.expect("a client");
            // Read the request first: closing it unread sends a reset, losing the frames below.
            let mut head = Vec::new();
            let mut buf = [0u8; 1024];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                match sock.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => head.extend_from_slice(&buf[..n]),
                }
            }
            // Two frames, then no terminating chunk: a proxy timing out mid-search.
            let body = "event: stages\ndata: {\"stages\":[\"embed\",\"retrieve\"]}\n\n\
                        event: stage\ndata: {\"stage\":\"embed\"}\n\n";
            sock.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                     Transfer-Encoding: chunked\r\n\r\n{:x}\r\n{body}\r\n",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .ok();
            sock.flush().await.ok();
        });
        let e = Endpoint {
            url: format!("http://{addr}"),
            token: "engram_x".into(),
        };
        let cli = CliArgs {
            plain: true,
            ..Default::default()
        };
        let face = crate::cli::face::Face::decide(&cli, false, false, None);
        assert!(
            streaming(&e, None, "journal", &cli, &face).await.is_err(),
            "a search that started over there is reported, not repeated"
        );
    }

    #[test]
    fn the_client_claims_the_cli_door_and_asks_as_wide_as_it_was_told() {
        let e = Endpoint {
            url: "https://engram.test".into(),
            token: "engram_x".into(),
        };
        let url = query_url(&e, Some(40), "loop device", &Default::default());
        assert!(url.contains("door=cli"), "{url}");
        let streaming = stream_url(&e, Some(40), "loop device", &Default::default());
        assert!(streaming.contains("/search/stream?"), "{streaming}");
        assert!(streaming.contains("door=cli"), "{streaming}");
        assert!(url.contains("limit=40"), "{url}");
        assert!(url.contains("q=loop%20device"), "{url}");
    }
}
