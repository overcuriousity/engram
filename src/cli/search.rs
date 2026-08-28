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
    // `door=cli` rather than the default `api`: a query typed at a shell is
    // composed before anything came back, which is the least contaminated
    // question the base receives, and the judge queue should be able to tell.
    let mut url = format!("{}?q={}&door=cli", e.api("/search"), encode(query));
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
    let http = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .map_err(|err| Error::Internal(format!("http client: {err}")))?;
    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let face = crate::cli::face::Face::decide(
        cli,
        is_tty,
        std::env::var_os("NO_COLOR").is_some(),
        crate::cli::face::locale().as_deref(),
    );
    let waiting = face.pulse("searching");
    let res = http
        .get(query_url(e, limit, query, cli))
        .bearer_auth(&e.token)
        .send()
        .await;
    // The response is here; the animation ends now, before anything is printed.
    drop(waiting);
    let res = res.map_err(|err| Error::Validation(format!("{err}")))?;
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

    // What `--show 3` will mean. Written before anything is printed, so the
    // list on screen and the list on disk cannot disagree, and only for a
    // search a person watched: see `last::worth_remembering`.
    if crate::cli::last::worth_remembering(is_tty, cli.json) {
        crate::cli::last::save(query, hits.iter().map(|h| h.artifact_id.clone()).collect());
    }

    if cli.json {
        // The server's own JSON, unchanged. A client that re-serialised it
        // would be a second definition of the response shape.
        println!("{body}");
    } else {
        print!("{}", face.render(&hits, None));
    }
    // `1` for nothing found, so `engram -s "x" || …` is a usable branch.
    Ok(if hits.is_empty() { 1 } else { 0 })
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
        let title = h.title.as_deref().unwrap_or("(untitled)");
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
                Some(last)
                    if !fresh && last.chars().count() + 1 + word.chars().count() <= room =>
                {
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
    if h.past_cliff {
        said.push("past the cliff");
    }
    if h.weak {
        said.push("loose match");
    }
    if h.model_written {
        said.push("model-written");
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
            past_cliff,
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
        assert!(!out.contains("Four"), "a fourth line is past the budget: {out}");
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

    #[test]
    fn the_client_claims_the_cli_door_and_asks_as_wide_as_it_was_told() {
        let e = Endpoint {
            url: "https://engram.test".into(),
            token: "engram_x".into(),
        };
        let url = query_url(&e, Some(40), "loop device", &Default::default());
        assert!(url.contains("door=cli"), "{url}");
        assert!(url.contains("limit=40"), "{url}");
        assert!(url.contains("q=loop%20device"), "{url}");
    }
}
