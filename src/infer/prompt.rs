use super::ProposedArtifact;
use crate::error::{Error, Result};

pub const SYNTHESIZER_SYSTEM: &str = r#"You turn reference material into atomic, self-contained knowledge artifacts.

Each artifact holds exactly one thing: one technique, one procedure, one fact,
one configuration. If a passage covers three techniques, emit three artifacts.

Rewrite each artifact so it stands alone without the surrounding document. Resolve
pronouns and implicit references: "this command" becomes the actual command,
"the above directory" becomes the actual path.

Reproduce commands, file paths, registry keys, error strings, code, and version
numbers VERBATIM. Never paraphrase, reformat, correct, or abbreviate them. The
rewriting applies to the connective prose around them, never to the literals
themselves.

Write artifact text as markdown: fenced code blocks with a language tag, lists for
step-by-step procedures, tables where they fit. Do NOT use an H1 (`# `) heading;
the title is a separate field, so any headings inside the text start at `## `.

Reply with JSON only, no commentary, in exactly this shape:

{"artifacts":[{"text":"...","title":"...","category":"...","tags":["..."],"corpus_lines":[start,end]}]}

- title: a short noun phrase naming the artifact.
- category: one lowercase word, e.g. procedure, concept, reference, snippet.
- tags: 1-5 lowercase keywords for filtering.
- corpus_lines: the 1-based line range in the input this artifact came from."#;

pub fn user_prompt(segment_text: &str, first_line: i64, max_artifact_tokens: usize) -> String {
    format!(
        "The input below starts at line {first_line}. Keep each artifact under roughly \
         {max_artifact_tokens} tokens; split into more artifacts rather than exceeding it.\n\n\
         ----- INPUT -----\n{segment_text}\n----- END INPUT -----"
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
    artifacts: Vec<RawArtifact>,
}

#[derive(serde::Deserialize)]
struct RawArtifact {
    text: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    corpus_lines: Option<Vec<i64>>,
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

/// Recover the artifact objects a truncated response did finish.
///
/// A small local model told to rewrite a segment routinely runs out of output
/// budget mid-list, and the segment it was working on is otherwise lost: the
/// parse fails, a repair call costs another minute or more on consumer
/// hardware, and that reply is just as likely to be cut off. The objects
/// before the cut are complete and correct, so scan the array and keep every
/// one that closed.
///
/// Returns `None` when nothing complete can be salvaged.
fn salvage_truncated(json: &str) -> Option<String> {
    let start = json.find("\"artifacts\"")?;
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

/// Recover the artifact objects a *malformed* reply still got right.
///
/// Truncation is the tidy failure: everything before the cut is valid, so
/// `salvage_truncated` can repair the envelope and reparse. A bad object in the
/// middle is the untidy one — a missing comma, an unescaped quote in a passage
/// that itself quotes something — and it fails the whole list however complete
/// the rest is. Losing nine good artifacts to one bad one is the worst trade
/// in the write path: it costs a segment of someone's corpus, and re-running
/// it means minutes of a local model's time for a reply just as likely to
/// stumble in the same place.
///
/// So parse the objects one at a time and keep the ones that stand up. A fault
/// that also derails the scanner's idea of where strings end makes everything
/// after it unreliable, which is why this returns what it could read rather
/// than claiming completeness — the caller flags the result as degraded.
fn salvage_objects(json: &str) -> Vec<RawArtifact> {
    let Some(start) = json.find("\"artifacts\"") else {
        return Vec::new();
    };
    let Some(open) = json[start..].find('[').map(|i| i + start) else {
        return Vec::new();
    };

    let bytes = json.as_bytes();
    let (mut depth, mut in_string, mut escaped) = (0i32, false, false);
    let mut object_start: Option<usize> = None;
    let mut out = Vec::new();
    for (i, &b) in bytes.iter().enumerate().skip(open + 1) {
        if escaped {
            escaped = false;
            continue;
        }
        match b {
            b'\\' if in_string => escaped = true,
            b'"' => in_string = !in_string,
            b'{' if !in_string => {
                if depth == 0 {
                    object_start = Some(i);
                }
                depth += 1;
            }
            b'}' if !in_string => {
                depth -= 1;
                if depth == 0
                    && let Some(s) = object_start.take()
                    && let Ok(raw) = serde_json::from_str::<RawArtifact>(&json[s..=i])
                {
                    out.push(raw);
                }
            }
            b']' if !in_string && depth == 0 => break,
            _ => {}
        }
    }
    out
}

/// Characters of an unparsable reply kept for diagnosis. Enough to see the
/// shape and the first offending construct without pasting a whole segment of
/// someone's corpus into a log file.
const RAW_ON_FAILURE: usize = 800;

pub fn parse_response(body: &str) -> Result<Vec<ProposedArtifact>> {
    let json = extract_json(body);
    let env: Envelope = match serde_json::from_str(json) {
        Ok(env) => env,
        Err(e) => {
            // Salvage before giving up: a truncated list still holds whole
            // artifacts, and asking a slow model to try again is expensive.
            let salvaged = salvage_truncated(json)
                .and_then(|repaired| serde_json::from_str::<Envelope>(&repaired).ok());
            match salvaged {
                Some(env) => {
                    tracing::warn!(
                        error = %e,
                        artifacts = env.artifacts.len(),
                        "synthesizer output was cut off; keeping the artifacts it finished"
                    );
                    env
                }
                // Not a clean cut, so read the list object by object and keep
                // whatever stands up on its own.
                None => {
                    let objects = salvage_objects(json);
                    if objects.is_empty() {
                        // The parser's complaint names an offset into a reply
                        // nobody kept, which is not enough to tell a truncated
                        // list from a bad escape from prose where JSON was
                        // asked for. Debug rather than warn: this is model
                        // output, so it carries corpus text, and it belongs in
                        // a log only when someone has gone looking.
                        tracing::debug!(
                            error = %e,
                            raw = %json.chars().take(RAW_ON_FAILURE).collect::<String>(),
                            "synthesizer output could not be parsed or salvaged"
                        );
                        return Err(Error::MalformedLlmOutput(e.to_string()));
                    }
                    tracing::warn!(
                        error = %e,
                        artifacts = objects.len(),
                        "synthesizer output was malformed; keeping the artifacts that parsed"
                    );
                    Envelope { artifacts: objects }
                }
            }
        }
    };

    let artifacts: Vec<ProposedArtifact> = env
        .artifacts
        .into_iter()
        .filter(|c| !c.text.trim().is_empty())
        .map(|c| ProposedArtifact {
            text: c.text.trim().to_string(),
            title: c.title.filter(|t| !t.trim().is_empty()),
            category: c.category.filter(|t| !t.trim().is_empty()),
            tags: c
                .tags
                .into_iter()
                .filter(|t| !t.trim().is_empty())
                .collect(),
            corpus_lines: match c.corpus_lines.as_deref() {
                Some([a, b]) => Some((*a, *b)),
                _ => None,
            },
        })
        .collect();

    if artifacts.is_empty() {
        return Err(Error::MalformedLlmOutput(
            "model returned no usable artifacts".into(),
        ));
    }
    Ok(artifacts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_truncated_list_keeps_the_artifacts_that_finished() {
        // Exactly what a small local model emits when it runs out of output
        // budget: two complete objects, then a third cut mid-string.
        let cut = r###"{"artifacts":[
          {"text":"first complete","title":"one","tags":[],"corpus_lines":[1,2]},
          {"text":"second complete","title":"two","tags":[]},
          {"text":"third was cut off here"###;
        let out = parse_response(cut).expect("the finished artifacts must survive");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].text, "first complete");
        assert_eq!(out[1].text, "second complete");
    }

    #[test]
    fn a_brace_inside_a_string_does_not_end_a_chunk_early() {
        let cut = r###"{"artifacts":[
          {"text":"awk '{print $1}' file.txt","title":"awk","tags":[]},
          {"text":"cut off"###;
        let out = parse_response(cut).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "awk '{print $1}' file.txt");
    }

    #[test]
    fn a_response_cut_before_any_chunk_closed_is_still_an_error() {
        let cut = r###"{"artifacts":[{"text":"nothing finished"###;
        assert!(parse_response(cut).is_err());
    }

    #[test]
    fn one_malformed_object_costs_only_itself() {
        // The list is complete and the outer shape is fine; the middle object
        // is missing a comma. Whole-document loss over one bad object is the
        // failure this salvage exists to prevent — a legal text with unusual
        // punctuation took down every artifact in its segment this way.
        let broken = r###"{"artifacts":[
          {"text":"first good","title":"one","tags":[]},
          {"text":"middle bad" "title":"two","tags":[]},
          {"text":"third good","title":"three","tags":[]}
        ]}"###;
        let out = parse_response(broken).expect("the parsable objects must survive");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].text, "first good");
        assert_eq!(out[1].text, "third good");
    }

    #[test]
    fn a_bad_escape_does_not_cost_the_objects_before_it() {
        // An unescaped quote also derails the scanner's idea of where strings
        // end, so what comes after is unreliable. What must hold is that the
        // objects it had already closed are still returned.
        let broken = r###"{"artifacts":[
          {"text":"first good","title":"one","tags":[]},
          {"text":"he said "stop" here","title":"two","tags":[]}
        ]}"###;
        let out = parse_response(broken).expect("the objects before the fault must survive");
        assert!(!out.is_empty(), "salvage returned nothing at all");
        assert_eq!(out[0].text, "first good");
    }

    #[test]
    fn a_reply_with_no_parsable_object_is_still_an_error() {
        // Salvage must not turn prose into an empty success: a segment that
        // produced nothing has to fail so the artifact is recorded as missing
        // rather than silently dropped.
        let prose = r###"{"artifacts":[{"text":"unterminated and "broken, "tags":}]}"###;
        assert!(parse_response(prose).is_err());
    }

    // r###: the JSON contains the sequence `"##` (a quoted markdown H2),
    // which would terminate both an r#"..."# and an r##"..."## literal.
    const GOOD: &str = r###"{"artifacts":[
      {"text":"## Mount an image\nRun `ewfmount evidence.E01 /mnt/ewf`.",
       "title":"Mount an E01 image","category":"procedure",
       "tags":["forensics","linux"],"corpus_lines":[3,9]}
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
        assert_eq!(out[0].corpus_lines, Some((3, 9)));
    }

    #[test]
    fn strips_code_fences_models_add_anyway() {
        let fenced = format!("Here you go:\n```json\n{GOOD}\n```\n");
        assert_eq!(parse_response(&fenced).unwrap().len(), 1);
    }

    #[test]
    fn missing_optional_fields_are_tolerated() {
        let minimal = r#"{"artifacts":[{"text":"bare text"}]}"#;
        let out = parse_response(minimal).unwrap();
        assert_eq!(out[0].text, "bare text");
        assert!(out[0].title.is_none());
        assert!(out[0].tags.is_empty());
        assert!(out[0].corpus_lines.is_none());
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
        assert!(parse_response(r#"{"artifacts":[]}"#).is_err());
    }

    #[test]
    fn blank_chunk_texts_are_dropped_not_stored() {
        let body = r#"{"artifacts":[{"text":"real"},{"text":"   "}]}"#;
        let out = parse_response(body).unwrap();
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn code_fences_inside_artifact_text_survive_extraction() {
        // The `}` inside a fenced snippet must not confuse the brace slicing,
        // and the code itself must come through byte-for-byte.
        let body =
            r#"{"artifacts":[{"text":"Run:\n```bash\nawk '{print $1}' file\n```","title":"awk"}]}"#;
        let out = parse_response(body).unwrap();
        assert!(
            out[0].text.contains("awk '{print $1}' file"),
            "code mangled: {}",
            out[0].text
        );
    }

    #[test]
    fn a_non_array_source_lines_is_ignored_rather_than_fatal() {
        let body = r#"{"artifacts":[{"text":"t","corpus_lines":[1,2,3]}]}"#;
        assert_eq!(parse_response(body).unwrap()[0].corpus_lines, None);
    }

    #[test]
    fn system_prompt_states_the_hard_rules() {
        // These instructions are the guardrail against paraphrased commands.
        assert!(SYNTHESIZER_SYSTEM.contains("VERBATIM"));
        assert!(SYNTHESIZER_SYSTEM.contains("markdown"));
        assert!(SYNTHESIZER_SYSTEM.contains("H1") || SYNTHESIZER_SYSTEM.contains("`#`"));
    }

    #[test]
    fn repair_prompt_includes_the_parse_error() {
        let p = repair_prompt("{bad", "expected value at line 1");
        assert!(p.contains("expected value at line 1"));
        assert!(p.contains("{bad"));
    }
}
