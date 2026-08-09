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

pub fn parse_response(body: &str) -> Result<Vec<ProposedChunk>> {
    let json = extract_json(body);
    let env: Envelope =
        serde_json::from_str(json).map_err(|e| Error::MalformedLlmOutput(e.to_string()))?;

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
