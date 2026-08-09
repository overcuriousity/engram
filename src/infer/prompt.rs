use super::ProposedChunk;
use crate::error::{Error, Result};

pub const CHUNKER_SYSTEM: &str = r#"You split reference material into atomic, self-contained knowledge chunks.

Each chunk holds exactly one thing: one technique, one procedure, one fact, one
configuration. If a passage covers three techniques, emit three chunks.

Rewrite each chunk so it stands alone without the surrounding document. Resolve
pronouns and implicit references: "this command" becomes the actual command,
"the above directory" becomes the actual path.

Reproduce commands, file paths, registry keys, error strings, code, and version
numbers VERBATIM. Never paraphrase, reformat, correct, or abbreviate them. The
rewriting applies to the connective prose around them, never to the literals
themselves.

Write chunk text as markdown: fenced code blocks with a language tag, lists for
step-by-step procedures, tables where they fit. Do NOT use an H1 (`# `) heading;
the title is a separate field, so any headings inside the text start at `## `.

Reply with JSON only, no commentary, in exactly this shape:

{"chunks":[{"text":"...","title":"...","category":"...","tags":["..."],"source_lines":[start,end]}]}

- title: a short noun phrase naming the chunk.
- category: one lowercase word, e.g. procedure, concept, reference, snippet.
- tags: 1-5 lowercase keywords for filtering.
- source_lines: the 1-based line range in the input this chunk came from."#;

pub fn user_prompt(window_text: &str, first_line: i64, max_chunk_tokens: usize) -> String {
    format!(
        "The input below starts at line {first_line}. Keep each chunk under roughly \
         {max_chunk_tokens} tokens; split into more chunks rather than exceeding it.\n\n\
         ----- INPUT -----\n{window_text}\n----- END INPUT -----"
    )
}

pub fn repair_prompt(previous: &str, err: &str) -> String {
    format!(
        "Your previous reply could not be parsed as JSON.\n\nParser error: {err}\n\n\
         Your reply was:\n{previous}\n\n\
         Reply again with valid JSON only, matching the required shape exactly. \
         No prose, no code fences."
    )
}

#[derive(serde::Deserialize)]
struct Envelope {
    chunks: Vec<RawChunk>,
}

#[derive(serde::Deserialize)]
struct RawChunk {
    text: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    source_lines: Option<Vec<i64>>,
}

/// Models wrap JSON in fences and preface it with prose no matter what the
/// prompt says, so slice from the first `{` to the last `}` before parsing.
fn extract_json(body: &str) -> &str {
    let start = body.find('{');
    let end = body.rfind('}');
    match (start, end) {
        (Some(s), Some(e)) if e > s => &body[s..=e],
        _ => body,
    }
}

/// Recover the chunk objects a truncated response did finish.
///
/// A small local model told to rewrite a window routinely runs out of output
/// budget mid-list, and the window it was working on is otherwise lost: the
/// parse fails, a repair call costs another minute or more on consumer
/// hardware, and that reply is just as likely to be cut off. The objects
/// before the cut are complete and correct, so scan the array and keep every
/// one that closed.
///
/// Returns `None` when nothing complete can be salvaged.
fn salvage_truncated(json: &str) -> Option<String> {
    let start = json.find("\"chunks\"")?;
    let open = json[start..].find('[')? + start;

    let bytes = json.as_bytes();
    let (mut depth, mut in_string, mut escaped) = (0i32, false, false);
    let mut last_complete: Option<usize> = None;
    for (i, &b) in bytes.iter().enumerate().skip(open + 1) {
        if escaped {
            escaped = false;
            continue;
        }
        match b {
            b'\\' if in_string => escaped = true,
            b'"' => in_string = !in_string,
            b'{' if !in_string => depth += 1,
            b'}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    last_complete = Some(i);
                }
            }
            b']' if !in_string && depth == 0 => break,
            _ => {}
        }
    }

    let end = last_complete?;
    Some(format!("{}]}}", &json[..=end]))
}

pub fn parse_response(body: &str) -> Result<Vec<ProposedChunk>> {
    let json = extract_json(body);
    let env: Envelope = match serde_json::from_str(json) {
        Ok(env) => env,
        Err(e) => {
            // Salvage before giving up: a truncated list still holds whole
            // chunks, and asking a slow model to try again is expensive.
            let salvaged = salvage_truncated(json)
                .and_then(|repaired| serde_json::from_str::<Envelope>(&repaired).ok());
            match salvaged {
                Some(env) => {
                    tracing::warn!(
                        error = %e,
                        chunks = env.chunks.len(),
                        "chunker output was cut off; keeping the chunks it finished"
                    );
                    env
                }
                None => return Err(Error::MalformedLlmOutput(e.to_string())),
            }
        }
    };

    let chunks: Vec<ProposedChunk> = env
        .chunks
        .into_iter()
        .filter(|c| !c.text.trim().is_empty())
        .map(|c| ProposedChunk {
            text: c.text.trim().to_string(),
            title: c.title.filter(|t| !t.trim().is_empty()),
            category: c.category.filter(|t| !t.trim().is_empty()),
            tags: c
                .tags
                .into_iter()
                .filter(|t| !t.trim().is_empty())
                .collect(),
            source_lines: match c.source_lines.as_deref() {
                Some([a, b]) => Some((*a, *b)),
                _ => None,
            },
        })
        .collect();

    if chunks.is_empty() {
        return Err(Error::MalformedLlmOutput(
            "model returned no usable chunks".into(),
        ));
    }
    Ok(chunks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_truncated_list_keeps_the_chunks_that_finished() {
        // Exactly what a small local model emits when it runs out of output
        // budget: two complete objects, then a third cut mid-string.
        let cut = r###"{"chunks":[
          {"text":"first complete","title":"one","tags":[],"source_lines":[1,2]},
          {"text":"second complete","title":"two","tags":[]},
          {"text":"third was cut off here"###;
        let out = parse_response(cut).expect("the finished chunks must survive");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].text, "first complete");
        assert_eq!(out[1].text, "second complete");
    }

    #[test]
    fn a_brace_inside_a_string_does_not_end_a_chunk_early() {
        let cut = r###"{"chunks":[
          {"text":"awk '{print $1}' file.txt","title":"awk","tags":[]},
          {"text":"cut off"###;
        let out = parse_response(cut).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "awk '{print $1}' file.txt");
    }

    #[test]
    fn a_response_cut_before_any_chunk_closed_is_still_an_error() {
        let cut = r###"{"chunks":[{"text":"nothing finished"###;
        assert!(parse_response(cut).is_err());
    }

    // r###: the JSON contains the sequence `"##` (a quoted markdown H2),
    // which would terminate both an r#"..."# and an r##"..."## literal.
    const GOOD: &str = r###"{"chunks":[
      {"text":"## Mount an image\nRun `ewfmount evidence.E01 /mnt/ewf`.",
       "title":"Mount an E01 image","category":"procedure",
       "tags":["forensics","linux"],"source_lines":[3,9]}
    ]}"###;

    #[test]
    fn parses_a_well_formed_response() {
        let out = parse_response(GOOD).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title.as_deref(), Some("Mount an E01 image"));
        assert_eq!(
            out[0].tags,
            vec!["forensics".to_string(), "linux".to_string()]
        );
        assert_eq!(out[0].source_lines, Some((3, 9)));
    }

    #[test]
    fn strips_code_fences_models_add_anyway() {
        let fenced = format!("Here you go:\n```json\n{GOOD}\n```\n");
        assert_eq!(parse_response(&fenced).unwrap().len(), 1);
    }

    #[test]
    fn missing_optional_fields_are_tolerated() {
        let minimal = r#"{"chunks":[{"text":"bare text"}]}"#;
        let out = parse_response(minimal).unwrap();
        assert_eq!(out[0].text, "bare text");
        assert!(out[0].title.is_none());
        assert!(out[0].tags.is_empty());
        assert!(out[0].source_lines.is_none());
    }

    #[test]
    fn malformed_json_is_a_retryable_error() {
        let e = parse_response("not json at all").unwrap_err();
        assert!(matches!(e, crate::error::Error::MalformedLlmOutput(_)));
        assert!(e.retryable());
    }

    #[test]
    fn empty_chunk_list_is_rejected() {
        // Silently accepting this would lose the whole source.
        assert!(parse_response(r#"{"chunks":[]}"#).is_err());
    }

    #[test]
    fn blank_chunk_texts_are_dropped_not_stored() {
        let body = r#"{"chunks":[{"text":"real"},{"text":"   "}]}"#;
        let out = parse_response(body).unwrap();
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn code_fences_inside_chunk_text_survive_extraction() {
        // The `}` inside a fenced snippet must not confuse the brace slicing,
        // and the code itself must come through byte-for-byte.
        let body =
            r#"{"chunks":[{"text":"Run:\n```bash\nawk '{print $1}' file\n```","title":"awk"}]}"#;
        let out = parse_response(body).unwrap();
        assert!(
            out[0].text.contains("awk '{print $1}' file"),
            "code mangled: {}",
            out[0].text
        );
    }

    #[test]
    fn a_non_array_source_lines_is_ignored_rather_than_fatal() {
        let body = r#"{"chunks":[{"text":"t","source_lines":[1,2,3]}]}"#;
        assert_eq!(parse_response(body).unwrap()[0].source_lines, None);
    }

    #[test]
    fn system_prompt_states_the_hard_rules() {
        // These instructions are the guardrail against paraphrased commands.
        assert!(CHUNKER_SYSTEM.contains("VERBATIM"));
        assert!(CHUNKER_SYSTEM.contains("markdown"));
        assert!(CHUNKER_SYSTEM.contains("H1") || CHUNKER_SYSTEM.contains("`#`"));
    }

    #[test]
    fn repair_prompt_includes_the_parse_error() {
        let p = repair_prompt("{bad", "expected value at line 1");
        assert!(p.contains("expected value at line 1"));
        assert!(p.contains("{bad"));
    }
}
