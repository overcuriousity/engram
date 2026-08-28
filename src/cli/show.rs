//! `--show`: one artifact, read in full.
//!
//! The ranked list clips every hit to a couple of lines, which is right for a
//! list and useless for reading. This is the door that never clips: whatever
//! the artifact holds is what is printed.

use crate::cli::endpoint::Endpoint;
use crate::error::Result;

/// What the reading door renders, which is less than the API answers with.
///
/// Deserialised into its own shape rather than into `store::artifacts::Chunk`:
/// the client renders a title, a body and a provenance line, and a struct that
/// named every column would have to be kept in step with a schema it never
/// reads.
#[derive(serde::Deserialize, Debug)]
pub struct Detail {
    pub id: String,
    pub title: Option<String>,
    pub text: String,
    pub category: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub source: Option<SourceRef>,
}

#[derive(serde::Deserialize, Debug)]
pub struct SourceRef {
    pub title: Option<String>,
    pub origin: String,
    pub source_url: Option<String>,
}

/// The form a pipe, a test and a script see: no colour, no glyph outside
/// ASCII, and the whole body.
pub fn render_plain(d: &Detail) -> String {
    let mut out = String::new();
    out.push_str(d.title.as_deref().unwrap_or("(untitled)"));
    out.push('\n');
    out.push_str(&d.id);
    out.push('\n');
    if let Some(src) = &d.source {
        let name = src.title.as_deref().unwrap_or("(untitled document)");
        out.push_str(&format!("from: {name} ({})\n", src.origin));
        if let Some(url) = &src.source_url {
            out.push_str(&format!("      {url}\n"));
        }
    }
    if let Some(c) = &d.category {
        out.push_str(&format!("category: {c}\n"));
    }
    if !d.tags.is_empty() {
        out.push_str(&format!("tags: {}\n", d.tags.join(", ")));
    }
    out.push('\n');
    // Whole, and unwrapped: the terminal knows its own width, and a client that
    // rewrapped would destroy the one thing this door promises.
    out.push_str(&d.text);
    if !d.text.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Ask for one artifact and print it.
///
/// The rank and the prefix are resolved here, against the list `-s` left
/// behind, so the server only ever receives an id.
///
/// `last` is passed rather than loaded, for the reason `Face::decide` takes its
/// facts as arguments: a test that read the developer's own state file would
/// pass or fail on what they last searched for.
pub async fn run(
    e: &Endpoint,
    which: &str,
    last: Option<crate::cli::last::LastSearch>,
) -> Result<i32> {
    let id = crate::cli::last::resolve(which, last.as_ref())?;
    match fetch(e, &id).await? {
        Some(detail) => {
            print!("{}", render_plain(&detail));
            Ok(0)
        }
        None => {
            // `1`, the same code `-s` answers with when it found nothing: a
            // shell branches on "it is not there" the same way whichever door
            // asked.
            eprintln!("no artifact {id}");
            Ok(1)
        }
    }
}

/// The artifact behind an id, or `None` when there is none.
///
/// Split from `run` because what a reading is *of* is the thing worth
/// asserting, and it cannot be asserted through a function whose only output
/// is stdout and an exit code.
pub async fn fetch(e: &Endpoint, id: &str) -> Result<Option<Detail>> {
    let http = reqwest::Client::builder()
        .user_agent(crate::cli::capture::USER_AGENT)
        .build()
        .map_err(|err| crate::error::Error::Internal(format!("http client: {err}")))?;
    let res = http
        .get(e.api(&format!("/artifacts/{}", crate::cli::encode(id))))
        .bearer_auth(&e.token)
        .send()
        .await
        .map_err(|err| crate::error::Error::Validation(format!("{err}")))?;
    if res.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !res.status().is_success() {
        return Err(crate::error::Error::Validation(format!(
            "the reading was refused: {}",
            res.status()
        )));
    }
    let body = res
        .text()
        .await
        .map_err(|err| crate::error::Error::Validation(format!("{err}")))?;
    let detail: Detail = serde_json::from_str(&body)
        .map_err(|err| crate::error::Error::Internal(format!("artifact: {err}")))?;
    Ok(Some(detail))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End to end against the real router: the rank the list printed reaches
    /// the right artifact, and its body arrives whole.
    #[tokio::test]
    async fn a_rank_from_the_last_search_reads_the_artifact_it_named() {
        let (url, token, core) = crate::cli::test_support::serve_test_app().await;
        let e = Endpoint { url, token };
        let src = core
            .store
            .insert_corpus("raw", "web", Some("Handbuch Mobilforensik"))
            .await
            .unwrap();
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &["first".to_string(), "second".to_string()]
                    .iter()
                    .enumerate()
                    .map(|(i, t)| crate::store::artifacts::NewArtifact {
                        ordinal: i as i64,
                        text: t.clone(),
                        corpus_span: None,
                        title: None,
                        category: None,
                        tags: vec![],
                        segment_idx: None,
                        caveats: vec![],
                    })
                    .collect::<Vec<_>>(),
            )
            .await
            .unwrap();
        let held = crate::cli::last::LastSearch {
            query: "whatever was asked".into(),
            at: 0,
            ids: made.iter().map(|c| c.id.clone()).collect(),
        };

        assert_eq!(run(&e, "2", Some(held.clone())).await.unwrap(), 0);

        // Which one it actually read. The exit code alone would pass on an
        // off-by-one, and an off-by-one is the whole risk in a one-based list
        // indexed into a zero-based vector.
        let id = crate::cli::last::resolve("2", Some(&held)).unwrap();
        let got = fetch(&e, &id).await.unwrap().expect("the second hit");
        assert_eq!(got.id, made[1].id, "rank 2 on screen is the second hit");
        assert_eq!(got.text, "second");
        assert_eq!(
            got.source.and_then(|s| s.title).as_deref(),
            Some("Handbuch Mobilforensik"),
            "the document it came from must travel with a reading"
        );
    }

    /// The same code `-s` answers with when it found nothing, so a shell
    /// branches on it the same way.
    #[tokio::test]
    async fn an_id_that_is_not_there_exits_one() {
        let (url, token, _core) = crate::cli::test_support::serve_test_app().await;
        let e = Endpoint { url, token };
        assert_eq!(
            run(&e, "01a00000-0000-7000-8000-000000000000", None)
                .await
                .unwrap(),
            1
        );
    }

    fn detail(text: &str) -> Detail {
        Detail {
            id: "01a04209-3b06-7af1-aead-4fbf5dd0a4b4".into(),
            title: Some("Physische Extraktion".into()),
            text: text.into(),
            category: Some("methode".into()),
            tags: vec!["forensik".into(), "mobil".into()],
            source: Some(SourceRef {
                title: Some("Handbuch Mobilforensik".into()),
                origin: "upload".into(),
                source_url: None,
            }),
        }
    }

    /// The whole reason this door exists. A rendering that clips is the
    /// rendering `-s` already has.
    #[test]
    fn the_body_is_printed_whole_however_long_it_is() {
        let long: String = (1..=40).map(|n| format!("line {n}\n")).collect();
        let out = render_plain(&detail(&long));
        for n in 1..=40 {
            assert!(out.contains(&format!("line {n}")), "line {n} was clipped");
        }
    }

    #[test]
    fn a_reading_says_which_artifact_it_is_and_where_it_came_from() {
        let out = render_plain(&detail("body"));
        assert!(out.contains("Physische Extraktion"), "{out}");
        assert!(
            out.contains("01a04209-3b06-7af1-aead-4fbf5dd0a4b4"),
            "{out}"
        );
        assert!(out.contains("Handbuch Mobilforensik"), "{out}");
        assert!(out.contains("forensik"), "{out}");
    }

    /// A merged artifact belongs to no corpus, and a corpus deleted since
    /// leaves its artifacts readable. Neither is a reason to print nothing.
    #[test]
    fn an_artifact_with_no_document_behind_it_still_reads() {
        let mut d = detail("body");
        d.source = None;
        d.title = None;
        let out = render_plain(&d);
        assert!(out.contains("body"), "{out}");
        assert!(out.contains("(untitled)"), "{out}");
    }

    #[test]
    fn the_plain_form_holds_no_escape_byte_at_all() {
        let out = render_plain(&detail("body"));
        assert!(!out.contains('\u{1b}'), "an escape reached it: {out:?}");
    }
}
