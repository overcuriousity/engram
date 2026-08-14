//! Every instruction the application puts in front of a model, in one file.
//!
//! Four calls, in pipeline order: the synthesizer that turns a segment into
//! artifacts, the titler that names the document they came from, the judge that
//! decides whether two artifacts contradict each other, and the answerer behind
//! `ask`. Nothing else in the tree writes model-facing prose, so changing how
//! engram talks to a model is an edit here and nowhere else.
//!
//! The parsers live here too, deliberately. Three of the four prompts specify a
//! JSON shape, and the code that reads that shape back has to change in the same
//! breath as the prompt that asks for it.

use super::ProposedArtifact;
use crate::error::{Error, Result};

pub const SYNTHESIZER_SYSTEM: &str = r#"You turn reference material into atomic, self-contained knowledge artifacts.

Each artifact holds exactly one thing: one technique, one procedure, one fact,
one configuration. If a passage covers three techniques, emit three artifacts.

Always use the language the input was written in.

Rewrite each artifact so it stands alone without the surrounding document. Resolve
pronouns and implicit references: "this command" becomes the actual command,
"the above directory" becomes the actual path.

Reproduce commands, file paths, registry keys, error strings, code, and version
numbers VERBATIM. Never paraphrase, reformat, correct, or abbreviate them. The
rewriting applies to the connective prose around them, never to the literals
themselves.

A block labelled "context only" is there so you can resolve references — what a
pronoun points at, which version or platform the document is about. Use it to
write artifacts that stand alone. Never emit an artifact for material that
appears only in a context block: the window that owns that material will emit
it, and emitting it twice puts two copies in the knowledge base. Extract
exclusively from the INPUT block.

Write artifact text as markdown: fenced code blocks with a language tag, lists for
step-by-step procedures, tables where they fit. Do NOT use an H1 (`# `) heading;
the title is a separate field, so any headings inside the text start at `## `.

Reply with JSON only, no commentary, in exactly this shape:

{"artifacts":[{"text":"...","title":"...","category":"...","tags":["..."],"corpus_lines":[start,end],"caveats":["..."]}]}

- title: a short noun phrase naming the artifact.
- category: one lowercase word, e.g. procedure, concept, reference, snippet.
- tags: 1-5 lowercase keywords for filtering.
- corpus_lines: the 1-based line range in the input this artifact came from.
- caveats: 0-3 short sentences for conditions under which this artifact does
  not hold — a prerequisite, a version or platform it is specific to, a
  destructive effect, a documented failure. Take these only from what the input
  states or plainly implies. Never invent a caveat, never add general advice,
  and never put a command in a caveat that is not in the input. Use an empty
  list when the input states none, which is the common case."#;

pub fn user_prompt(
    segment_text: &str,
    first_line: i64,
    max_artifact_tokens: usize,
    context: &crate::infer::context::WindowContext,
) -> String {
    let mut out = String::new();
    // The opening leads so that the system prompt followed by it is a
    // byte-identical prefix for every window of a corpus, which a prompt cache
    // or a llama.cpp slot can reuse. Everything per-window follows.
    if let Some(o) = &context.opening {
        out.push_str(&format!(
            "----- DOCUMENT OPENING (context only) -----\n{o}\n\
             ----- END DOCUMENT OPENING -----\n\n"
        ));
    }
    if let Some(b) = &context.before {
        out.push_str(&format!(
            "----- PRECEDING CONTEXT (context only) -----\n{b}\n\
             ----- END PRECEDING CONTEXT -----\n\n"
        ));
    }
    out.push_str(&format!(
        "The input below starts at line {first_line}. Keep each artifact under roughly \
         {max_artifact_tokens} tokens; split into more artifacts rather than exceeding it.\n\n\
         ----- INPUT -----\n{segment_text}\n----- END INPUT -----"
    ));
    if let Some(a) = &context.after {
        out.push_str(&format!(
            "\n\n----- FOLLOWING CONTEXT (context only) -----\n{a}\n\
             ----- END FOLLOWING CONTEXT -----"
        ));
    }
    out
}

pub fn repair_prompt(previous: &str, err: &str) -> String {
    format!(
        "Your previous reply could not be parsed as JSON.\n\nParser error: {err}\n\n\
         Your reply was:\n{previous}\n\n\
         Reply again with valid JSON only, matching the required shape exactly. \
         No prose, no code fences."
    )
}

/// The judge is asked one question and given no room to be helpful.
///
/// It is not asked which artifact is right, nor to merge them, nor to rewrite
/// anything. Deciding which of two contradictory artifacts is current needs
/// context the base does not hold — what the reader is actually running — and
/// is a judgement only they can make. All this call does is tell them there is
/// a judgement waiting.
pub const TITLE_SYSTEM: &str = r#"You name documents. Given the opening of a document and the titles of the notes taken from it, reply with one short title — at most eight words, no quotes, no trailing punctuation, no preamble.

Name what the document is about, not what it is. Never "Document", "Notes", "Guide", "Untitled"."#;

/// The opening rather than the whole document: a title needs the subject, and
/// the artifact titles already say what the rest of it turned out to cover.
pub fn title_prompt(text: &str, artifact_titles: &[String]) -> String {
    let opening: String = text.chars().take(2000).collect();
    format!(
        "Opening of the document:\n{opening}\n\nTitles of the notes taken from it:\n{}\n\nTitle:",
        artifact_titles.join("\n")
    )
}

pub const JUDGE_SYSTEM: &str = r#"You compare two knowledge artifacts and answer one question: do they state some specific detail differently?

First decide whether the two are about the same subject. Their titles say what each one is about, and the body may never repeat it — an artifact titled "FAT32 Specifications" can open with "32 Bit Clusternummern" and never name FAT32 again.

If the titles name different things — two versions, two variants, two products, two filesystems, two commands — then the artifacts are not in conflict no matter how far apart their numbers are. Different things have different numbers; that is what makes them different things. Answer false and stop.

Only when both describe the same subject: a contradiction is a concrete disagreement about it — a different version, number, date, path, flag, default, or step order for that one subject.

These are NOT contradictions:
- The same fact in different words.
- Different levels of detail about the same thing.
- Two different subjects that happen to use similar language, or the same layout.
- One artifact mentioning something the other simply does not cover.
- Two values that both appear in the same artifact.

Reply with JSON only, no commentary, in exactly this shape:

{"contradicts": true, "detail": "...", "obsolete": "a"}

- contradicts: true only for a concrete disagreement about one subject, as above.
- detail: when true, one short sentence naming the two conflicting values and which artifact holds each. Omit it when false.
- obsolete: "a" or "b" — only when you are confident one artifact plainly replaces the other (a deprecated flag, step, or default versus its current replacement), not merely that they differ. Omit this field whenever the direction is unclear, the two describe genuinely different but still-valid options, or you are not sure."#;

/// The two artifacts, each under its title.
///
/// The title is not decoration here, it is the subject. Synthesis writes a body
/// that stands on its own within its segment, which is not the same as naming
/// what it is about: a section headed "FAT32" becomes an artifact whose text
/// opens "32 Bit Clusternummern" and never says FAT32 again. Handed the bodies
/// alone, the model saw two anonymous spec lists with different numbers and
/// called them a contradiction — correctly, on the evidence it was given.
/// `attempt` is how many times this pair has already been asked about, and it is
/// in the prompt for one reason: the endpoint caches by exact prompt text and
/// replays a cached reply in milliseconds. A retry of a reply the parser could
/// not read would otherwise re-read the same unreadable bytes, five times, and
/// call it five attempts.
///
/// Zero adds nothing at all, so a first ask stays byte-identical to what it has
/// always been — and keeps hitting the cache when it should, on a pair the sweep
/// re-arms after a settled verdict was lost.
pub fn judge_prompt(a: (&str, &str), b: (&str, &str), attempt: i64) -> String {
    let retry = if attempt > 0 {
        format!("(attempt {})\n", attempt + 1)
    } else {
        String::new()
    };
    format!(
        "{retry}----- ARTIFACT A -----\nTitle: {}\n\n{}\n----- ARTIFACT B -----\nTitle: {}\n\n{}\n----- END -----",
        a.0, a.1, b.0, b.1
    )
}

pub const ASK_SYSTEM: &str = "You answer questions using only the provided knowledge-base excerpts. \
Quote commands, paths and code exactly as they appear. If the excerpts do not contain the answer, \
say so plainly rather than guessing. Cite excerpts by their number. \
An excerpt may carry lines beginning `Caveat:` — the conditions under which it does not apply. \
Repeat any caveat that bears on your answer rather than dropping it.";

/// One retrieved excerpt, numbered so the answer can cite it.
///
/// The caveats are appended here rather than left to the caller because their
/// `Caveat:` prefix is the exact string `ASK_SYSTEM` tells the model to look
/// for. Splitting the two apart is how that agreement quietly breaks.
pub fn ask_excerpt(number: usize, title: &str, text: &str, caveats: &[String]) -> String {
    let mut block = format!("[{number}] {title}\n{text}");
    for c in caveats {
        block.push_str("\nCaveat: ");
        block.push_str(c);
    }
    block
}

/// The question and whatever excerpts survived the context budget.
pub fn ask_prompt(question: &str, excerpts: &[String]) -> String {
    format!(
        "Question: {question}\n\nExcerpts:\n\n{}",
        excerpts.join("\n\n---\n\n")
    )
}

#[derive(serde::Deserialize)]
struct Judgement {
    contradicts: bool,
    #[serde(default)]
    detail: Option<String>,
    #[serde(default)]
    obsolete: Option<String>,
}

/// A reply that cannot be read is an error, not a verdict.
///
/// Defaulting to "contradicts" would fill the review queue with noise an
/// operator has to clear by hand; defaulting to "no" would quietly close real
/// conflicts. Failing leaves the pair pending, and the next sweep asks again.
///
/// The third element is which side the judge named obsolete — `'a'` or
/// `'b'` — when it named one with confidence. Anything else the model wrote
/// there (a stray word, a full sentence) is treated the same as omitting it:
/// an unreadable direction must not fail the whole reply, since the
/// contradiction verdict itself may still be perfectly readable.
pub fn parse_judgement(body: &str) -> Result<(bool, Option<String>, Option<char>)> {
    let j: Judgement = serde_json::from_str(extract_json(body)).map_err(|e| {
        Error::MalformedLlmOutput(format!("judge reply was not the expected JSON: {e}"))
    })?;
    let detail = j
        .detail
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty() && j.contradicts);
    let obsolete = j.obsolete.as_deref().and_then(|o| match o.trim() {
        "a" | "A" => Some('a'),
        "b" | "B" => Some('b'),
        _ => None,
    });
    Ok((j.contradicts, detail, obsolete))
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
    #[serde(default)]
    caveats: Vec<String>,
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
            // Capped at the three the prompt asks for: a model that starts
            // listing general advice must not turn one artifact into a page of
            // it, and the tail is the least source-grounded part of the list.
            caveats: c
                .caveats
                .into_iter()
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty())
                .take(3)
                .collect(),
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
    fn context_blocks_are_fenced_and_labelled_as_context_only() {
        use crate::infer::context::WindowContext;

        let ctx = WindowContext {
            opening: Some("# Guide\nPBS 3.x on Debian 12.".into()),
            before: Some("previous window tail".into()),
            after: Some("next window head".into()),
        };
        let p = user_prompt("the window body", 1, 1024, &ctx);

        assert!(p.contains("PBS 3.x on Debian 12."));
        assert!(p.contains("previous window tail"));
        assert!(p.contains("next window head"));
        assert!(p.contains("----- INPUT -----\nthe window body\n----- END INPUT -----"));

        // The opening leads, so system prompt + opening is a byte-identical
        // prefix across every window of a corpus and a prompt cache can reuse
        // it. Everything that varies per window sits after it.
        let opening_at = p.find("PBS 3.x").unwrap();
        let before_at = p.find("previous window tail").unwrap();
        let input_at = p.find("----- INPUT -----").unwrap();
        let after_at = p.find("next window head").unwrap();
        assert!(opening_at < before_at && before_at < input_at && input_at < after_at);
    }

    #[test]
    fn an_empty_context_renders_exactly_the_prompt_of_before() {
        use crate::infer::context::WindowContext;

        let p = user_prompt("body", 1, 1024, &WindowContext::default());
        assert!(
            !p.contains("context only"),
            "an empty context must not emit empty fences: {p}"
        );
        assert!(p.starts_with("The input below starts at line 1."));
        assert!(p.ends_with("----- END INPUT -----"));
    }

    #[test]
    fn the_system_prompt_forbids_extracting_from_context() {
        assert!(SYNTHESIZER_SYSTEM.contains("context only"));
        assert!(SYNTHESIZER_SYSTEM.contains("INPUT"));
    }

    #[test]
    fn the_judge_is_told_what_each_artifact_is_about() {
        // The real case this fixes: an artifact headed "FAT32 Specifications"
        // whose body opens "32 Bit Clusternummern" and never says FAT32 again.
        // Given the bodies alone, the model saw two anonymous spec lists with
        // different numbers and called them a contradiction — which was the
        // only honest answer to the question it was actually asked.
        let p = judge_prompt(
            ("FAT16 Specifications", "Die max. Partitionsgröße: 2 GB."),
            ("FAT32 Specifications", "32 Bit Clusternummern."),
            0,
        );
        assert!(p.contains("Title: FAT16 Specifications"), "{p}");
        assert!(p.contains("Title: FAT32 Specifications"), "{p}");
        assert!(p.contains("Die max. Partitionsgröße: 2 GB."));
    }

    #[test]
    fn a_retry_does_not_ask_the_endpoint_the_question_it_has_cached() {
        // The endpoint replays a cached reply for an identical prompt in
        // milliseconds. A pair whose reply the parser could not read is retried
        // up to `MAX_ATTEMPTS` times, and every one of those would have read the
        // same unreadable bytes back.
        let pair = (
            ("FAT16 Specifications", "Die max. Partitionsgröße: 2 GB."),
            ("FAT32 Specifications", "32 Bit Clusternummern."),
        );
        let first = judge_prompt(pair.0, pair.1, 0);
        let second = judge_prompt(pair.0, pair.1, 1);
        assert_ne!(first, second);
        assert_ne!(second, judge_prompt(pair.0, pair.1, 2));
        // A first ask stays exactly what it was, so the cache still earns its
        // keep on a pair the sweep re-arms after a verdict was lost.
        assert!(first.starts_with("----- ARTIFACT A -----"), "{first}");
    }

    #[test]
    fn the_judge_is_told_that_different_subjects_are_not_a_conflict() {
        // Two sections of one reference document are near-identical in form and
        // deliberately different in content, so similarity puts them in a pair
        // and every number in them differs. Without this rule the feature fires
        // hardest exactly where it is most wrong.
        assert!(JUDGE_SYSTEM.contains("same subject"));
        assert!(JUDGE_SYSTEM.contains("Answer false and stop."));
    }

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

    #[test]
    fn a_judgement_parses() {
        let (yes, detail, obsolete) =
            parse_judgement(r#"{"contradicts":true,"detail":"one says 1.2, the other 1.4"}"#)
                .unwrap();
        assert!(yes);
        assert_eq!(detail.as_deref(), Some("one says 1.2, the other 1.4"));
        assert!(obsolete.is_none());
    }

    #[test]
    fn a_negative_judgement_carries_no_detail() {
        let (yes, detail, _) = parse_judgement(r#"{"contradicts":false}"#).unwrap();
        assert!(!yes);
        assert!(detail.is_none());
    }

    #[test]
    fn a_judgement_wrapped_in_prose_and_fences_still_parses() {
        // The same models that fence the synthesis reply fence this one.
        let (yes, ..) = parse_judgement("Sure:\n```json\n{\"contradicts\": true}\n```").unwrap();
        assert!(yes);
    }

    #[test]
    fn a_judgement_names_the_obsolete_side() {
        let (yes, _, obsolete) = parse_judgement(
            r#"{"contradicts":true,"detail":"a uses --old-flag, b uses --new-flag","obsolete":"a"}"#,
        )
        .unwrap();
        assert!(yes);
        assert_eq!(obsolete, Some('a'));
    }

    #[test]
    fn an_unreadable_obsolete_value_is_treated_as_absent() {
        // An unparsable direction must not fail the whole reply — the
        // contradiction verdict itself is still readable.
        let (yes, _, obsolete) =
            parse_judgement(r#"{"contradicts":true,"obsolete":"not sure honestly"}"#).unwrap();
        assert!(yes);
        assert!(obsolete.is_none());
    }

    #[test]
    fn an_unparsable_judgement_is_an_error_not_a_yes() {
        // Defaulting to "contradicts" would fill the review queue with noise;
        // defaulting to "no" would hide real conflicts. Neither: it fails, the
        // pair stays pending, and the next sweep tries again.
        assert!(parse_judgement("I could not decide.").is_err());
    }

    #[test]
    fn caveats_are_parsed_when_the_model_supplies_them() {
        let body = r#"{"artifacts":[{
            "text":"Run `mkfs.ext4 /dev/sdb1` to format the partition.",
            "title":"Formatting a partition",
            "category":"procedure",
            "tags":["disk"],
            "corpus_lines":[3,9],
            "caveats":["Destroys every existing file on the device.",
                       "Requires root."]
        }]}"#;
        let got = parse_response(body).unwrap();
        assert_eq!(
            got[0].caveats,
            vec![
                "Destroys every existing file on the device.".to_string(),
                "Requires root.".to_string()
            ]
        );
    }

    #[test]
    fn an_artifact_without_caveats_parses_to_an_empty_list() {
        // Most models will omit the field most of the time, and a missing
        // field must never fail a segment that is otherwise fine.
        let body = r#"{"artifacts":[{"text":"plain","title":"t","category":"c","tags":[]}]}"#;
        assert!(parse_response(body).unwrap()[0].caveats.is_empty());
    }

    #[test]
    fn the_system_prompt_asks_for_caveats_and_forbids_inventing_them() {
        assert!(SYNTHESIZER_SYSTEM.contains("caveats"));
        assert!(
            SYNTHESIZER_SYSTEM.contains("Never invent"),
            "the prompt must tie caveats to what the source says"
        );
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
