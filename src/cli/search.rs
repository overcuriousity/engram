//! `-s`: a ranked list in a terminal, saying everything the rail says.

use crate::cli::args::CliArgs;
use crate::cli::capture::USER_AGENT;
use crate::cli::encode;
use crate::cli::endpoint::Endpoint;
use crate::core::search::SearchResult;
use crate::error::{Error, Result};

/// The URL a search is asked for at.
///
/// Its own function because one test asserts the door this client claims, and a
/// whole request is a poor place to assert it from.
pub fn query_url(e: &Endpoint, limit: Option<usize>, query: &str, cli: &CliArgs) -> String {
    url_for(e, "/search", limit, query, cli)
}

/// The same search, at the door that reports its stages.
pub fn stream_url(e: &Endpoint, limit: Option<usize>, query: &str, cli: &CliArgs) -> String {
    url_for(e, "/search/stream", limit, query, cli)
}

fn url_for(e: &Endpoint, path: &str, limit: Option<usize>, query: &str, cli: &CliArgs) -> String {
    // `door=cli` rather than the default `api`: a query typed at a shell is
    // composed before anything came back, which is the least contaminated
    // question the base receives, and the judge queue should be able to tell.
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
    // `--json` never streams: its reader wants the server's own body, and
    // frames are of no use to it.
    let streamed = match cli.json {
        true => None,
        false => streaming(e, limit, query, cli, &face).await?,
    };
    let (hits, elapsed, body) = match streamed {
        Some((hits, ms)) => (hits, Some(ms), None),
        // The door every version of the server has had. Reached when this one
        // does not know the streaming route, when the transport failed before a
        // frame arrived, or when the search was refused — and in that last case
        // it is this door that says why, in the words it has always used.
        None => {
            let (hits, ms, body) = plain(e, limit, query, cli).await?;
            (hits, Some(ms), Some(body))
        }
    };

    // What `--show 3` will mean. Written before anything is printed, so the
    // list on screen and the list on disk cannot disagree, and only for a
    // search a person watched: see `last::worth_remembering`.
    if crate::cli::last::worth_remembering(is_tty, cli.json) {
        crate::cli::last::save(query, hits.iter().map(|h| h.artifact_id.clone()).collect());
    }

    match body {
        // The server's own JSON, unchanged. A client that re-serialised it
        // would be a second definition of the response shape.
        Some(body) if cli.json => println!("{body}"),
        _ => print!("{}", face.render(&hits, elapsed)),
    }
    // `1` for nothing found, so `engram -s "x" || …` is a usable branch.
    Ok(if hits.is_empty() { 1 } else { 0 })
}

/// The streaming door: the hits and what the whole round trip took, or `None`
/// where this client should ask the plain one instead.
///
/// A failure before the search ran is a `None` rather than an error. The plain
/// door runs the same search and has said why a search was refused since before
/// this one existed, so one code path reports failure and it is the older one.
///
/// A failure after it ran is an error, and this is the whole of the difference.
/// Falling back across that line runs the same query a second time and records
/// it a second time, which is a duplicate in the data nobody typed and nobody
/// can tell from a real repeat — `record_search` coalesces the two typing
/// doors and not this one, so a shell's duplicate never folds.
///
/// The line is drawn at the first `stage` frame rather than at `results`,
/// which is later than it needs to be and is the point. `record_search` is
/// spawned onto the server's background inside `search_inner`, before the
/// `results` frame is yielded and out of reach of the client hanging up, so a
/// transport that dies in that window has recorded a search this client never
/// saw. A stage frame proves the search is running over there; from then on
/// the honest answer to a dead stream is to say so, not to run it again.
async fn streaming(
    e: &Endpoint,
    limit: Option<usize>,
    query: &str,
    cli: &CliArgs,
    face: &crate::cli::face::Face,
) -> Result<Option<(Vec<SearchResult>, u128)>> {
    use tokio_stream::StreamExt as _;
    let http = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .map_err(|err| Error::Internal(format!("http client: {err}")))?;
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
    // Two buffers, for the reason `ask` keeps two: a chunk from the network is
    // cut at neither boundary that matters.
    let mut pending: Vec<u8> = Vec::new();
    let mut buf = String::new();
    let mut hits: Option<Vec<SearchResult>> = None;
    // Whether this search may already have been written down over there. Set
    // from the first stage frame, for the reason above, and never unset.
    let mut ran = false;
    while let Some(chunk) = body.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            // Nothing has been recorded yet, so the older door can run it.
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
                // Said by the plain door, which is about to run this search
                // again and refuse it in the words it has always used. A
                // refusal recorded nothing, whatever stages preceded it.
                "error" => return Ok(None),
                _ => {}
            }
        }
    }
    // Taken back before a result lands on top of half a stage line.
    stages.clear();
    match (hits, ran) {
        (Some(h), _) => Ok(Some((h, began.elapsed().as_millis()))),
        // The search started and its results never arrived here: a frame in a
        // shape this client could not read, or a stream that stopped between
        // the stages and the list. Either way it may already be recorded, so
        // the plain door must not be asked to run it again.
        (None, true) => Err(Error::Validation(
            "the search ran and its results did not arrive".into(),
        )),
        // The stream ended before the search started. Nothing was recorded and
        // the older door can be asked cleanly.
        (None, false) => Ok(None),
    }
}

/// The door every version of the server has had, and the one that says why a
/// search was refused. Answers the body as well, for `--json`.
async fn plain(
    e: &Endpoint,
    limit: Option<usize>,
    query: &str,
    cli: &CliArgs,
) -> Result<(Vec<SearchResult>, u128, String)> {
    let http = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .map_err(|err| Error::Internal(format!("http client: {err}")))?;
    let began = std::time::Instant::now();
    let res = http
        .get(query_url(e, limit, query, cli))
        .bearer_auth(&e.token)
        .send()
        .await
        .map_err(|err| Error::Validation(format!("{err}")))?;
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
    let hits: Vec<SearchResult> =
        serde_json::from_str(&body).map_err(|err| Error::Internal(format!("results: {err}")))?;
    Ok((hits, began.elapsed().as_millis(), body))
}

/// The form a pipe, a test and a script see.
///
/// No colour, no motion, no glyph outside ASCII — and every claim the rail makes
/// said in words, because a rendering that marks the cliff only by being dim
/// marks it for nobody in a monochrome terminal, in a screenshot, or in a
/// redirected file.
pub fn render_plain(hits: &[SearchResult]) -> String {
    let mut out = String::new();
    for (i, h) in hits.iter().enumerate() {
        let mark = if h.past_cliff { "." } else { " " };
        // The server names a passage by its note when it has no heading; a
        // merged artifact has no note, and is named by its own opening.
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

/// The first `n` lines of a hit's text that have anything on them.
///
/// A clip is a budget for showing text, so a blank line must not consume any
/// of it: an artifact whose first line is a heading spends line two on the gap
/// beneath it, and the list shows one line where it promised `n`. Leading
/// indentation goes with it — it positions text against a document that is not
/// on screen here.
pub(crate) fn body_lines(text: &str, n: usize) -> impl Iterator<Item = &str> {
    text.lines()
        .map(str::trim_end)
        .map(str::trim_start)
        .filter(|l| !l.is_empty())
        .take(n)
}

/// A hit's text laid out to fit `width`, up to `budget` display lines.
///
/// Clipping a source line at the terminal's edge threw away the rest of the
/// sentence and gave the space back to nobody — a hit whose one interesting
/// clause sat past column 74 read as a fragment. Wrapping spends the same
/// budget on text instead, and the budget is display lines rather than source
/// lines so a document's own line breaks cannot decide how much you are shown.
pub(crate) fn excerpt(text: &str, budget: usize, width: usize) -> Vec<String> {
    let room = width.max(24);
    let mut out: Vec<String> = Vec::new();
    for line in body_lines(text, budget) {
        // Each source line starts a new display line. Flowing one into the next
        // would run a heading into the paragraph under it, which is the one
        // break in a document that was carrying meaning.
        let mut fresh = true;
        for word in line.split_whitespace() {
            match out.last_mut() {
                // `+ 1` is the space that joining would add.
                Some(last) if !fresh && last.chars().count() + 1 + word.chars().count() <= room => {
                    last.push(' ');
                    last.push_str(word);
                }
                _ => {
                    if out.len() == budget {
                        return out;
                    }
                    // A word longer than the line gets the line to itself and
                    // is cut there; nothing else can be done with it, and the
                    // alternative is a row that breaks the layout.
                    out.push(word.chars().take(room).collect());
                    fresh = false;
                }
            }
        }
    }
    out
}

/// What this hit is, in words, or `None` when there is nothing to say.
///
/// Shared with the drawn rendering so the two cannot drift into saying
/// different things about the same hit.
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

    /// One hit, with only the fields a rendering reads set to anything
    /// interesting. Shared with the face's tests.
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
            titled_by_corpus: false,
            via: None,
            reason: None,
            explanation: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fixture::hit;

    /// Same budget, same rule, in the form a pipe sees.
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

    /// The streaming door is the same search, so it has to answer with the
    /// same hits in the same order — and the fallback has to be reachable
    /// without either of them saying anything different.
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
                    ordinal: 0,
                    text: "the journal is a ring buffer on disk".into(),
                    corpus_span: None,
                    title: Some("journald".into()),
                    category: None,
                    tags: vec![],
                    segment_idx: None,
                    caveats: vec![],
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

    /// A server that has never heard of the streaming route is answered by
    /// falling back, not by failing.
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

    /// A stream that reported a stage and then died is not run again.
    ///
    /// The server spawns `record_search` onto its own background before the
    /// `results` frame is yielded, and that spawn outlives the client hanging
    /// up. Falling back here asked the plain door for the same query, which
    /// records a second time — and `record_search` coalesces the two typing
    /// doors and not the shell, so the duplicate is permanent.
    #[tokio::test]
    async fn a_stream_that_dies_after_a_stage_is_not_run_a_second_time() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a port");
        let addr = listener.local_addr().expect("the port it got");
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt as _;
            let (mut sock, _) = listener.accept().await.expect("a client");
            // Enough of a chunked response to carry two frames, and then the
            // socket goes away without its terminating chunk: a proxy timing
            // out mid-search, seen from here.
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
