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
    let face = crate::cli::face::Face::decide(
        cli,
        std::io::IsTerminal::is_terminal(&std::io::stdout()),
        std::env::var_os("NO_COLOR").is_some(),
        std::env::var("LANG").ok().as_deref(),
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

    if cli.json {
        // The server's own JSON, unchanged. A client that re-serialised it
        // would be a second definition of the response shape.
        println!("{body}");
    } else {
        print!("{}", face.render(&hits));
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
        for line in h.text.lines().take(3) {
            out.push_str(&format!("      {line}\n"));
        }
        out.push('\n');
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
