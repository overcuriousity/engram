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

/// What the dedupe pass decided about a group of near-duplicate artifacts.
///
/// Four outcomes, and the ordering between them is the design. `Replaced` is
/// preferred over `Duplicate` wherever it applies: the survivor is then a stored
/// original with a valid span and corpus lines to render beside it, which is
/// strictly better than a rewrite. A merge is the answer only when *both* sides
/// carry something the other lacks — the case where neither original is
/// sufficient and the pre-merge state was already losing something.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Relation {
    /// Different subjects. Both stay exactly where they are.
    Distinct,
    /// The same subject with a different value for one detail, and no way to
    /// tell which is current. Escalated to a person; never merged.
    Conflict,
    /// One artifact plainly replaces another. Superseded, with no synthetic
    /// text written at all.
    Replaced,
    /// The same claim, each side carrying detail the other lacks. Merged.
    Duplicate,
}

/// The artifact a `Duplicate` verdict asks to be written.
#[derive(Debug, Clone)]
pub struct MergedDraft {
    pub title: Option<String>,
    pub text: String,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub caveats: Vec<String>,
}

/// One dedupe verdict, parsed.
#[derive(Debug, Clone)]
pub struct Dedupe {
    pub relation: Relation,
    pub detail: Option<String>,
    /// Which artifact was named obsolete, as the letter it was shown under.
    /// Only meaningful for `Replaced`.
    pub supersedes: Option<char>,
    /// `Some` if and only if `relation` is `Duplicate`.
    pub merged: Option<MergedDraft>,
}

pub const DEDUPE_SYSTEM: &str = r#"You compare knowledge artifacts that may be about the same thing, and decide what should happen to them.

First decide whether they are about the same subject. Their titles say what each one is about, and the body may never repeat it — an artifact titled "FAT32 Specifications" can open with "32 Bit Clusternummern" and never name FAT32 again.

If the titles name different things — two versions, two variants, two products, two filesystems, two commands — then they are neither duplicates nor in conflict, no matter how far apart their numbers are. Different things have different numbers; that is what makes them different things. Answer "distinct" and stop.

Only when they describe the same subject, choose one of:

- "replaced" — one artifact plainly supersedes another: a deprecated flag, step or default versus its current replacement. Prefer this whenever it applies. It keeps the surviving artifact's original wording, which is always better than rewriting.
- "duplicate" — they make the same claim, and each carries some detail the others lack. Write one artifact that says everything all of them said.
- "conflict" — they give a different value for the same detail of the same subject, and you cannot tell which is current. Do not choose a side and do not merge; a person decides this one.
- "distinct" — different subjects, or one covers something the others simply do not.

These are NOT conflicts:
- The same fact in different words.
- Different levels of detail about the same thing.
- One artifact mentioning something the others do not cover.

When you answer "duplicate", the merged text must contain every number, version, date, path, flag, command and error string that appeared in any input, and must read as one self-contained artifact rather than a list of sources. If you cannot write one that keeps all of them, the answer is "conflict", not "duplicate".

Reply with JSON only, no commentary, in exactly this shape:

{"relation": "duplicate", "detail": "...", "supersedes": "a", "merged": {"title": "...", "text": "...", "category": "...", "tags": [], "caveats": []}}

- relation: one of "duplicate", "replaced", "conflict", "distinct".
- detail: one short sentence saying why. Always.
- supersedes: the letter of the artifact that is obsolete. Only with "replaced"; omit it otherwise.
- merged: only with "duplicate"; omit it entirely otherwise. `text` must stand on its own without its sources. `caveats` are the conditions under which it does not apply."#;

/// The artifacts, each under a letter and its title.
///
/// The title is not decoration here, it is the subject. Synthesis writes a body
/// that stands on its own within its segment, which is not the same as naming
/// what it is about: a section headed "FAT32" becomes an artifact whose text
/// opens "32 Bit Clusternummern" and never says FAT32 again. Handed the bodies
/// alone, the model saw two anonymous spec lists with different numbers and
/// called them a contradiction — correctly, on the evidence it was given.
///
/// `differing_values` is what `facts::fact_tokens` found stated differently
/// across the artifacts. It is a prior, not a verdict: it cannot tell a real
/// disagreement from the same subject described at two levels of detail, which
/// is the whole reason a model is asked. It used to be an admission gate, and
/// as a gate it was backwards — a pair stating no differing value is the
/// cleanest thing there is to merge, and gating on difference hid exactly those.
///
/// `attempt` is how many times this group has already been asked about, and it
/// is in the prompt for one reason: the endpoint caches by exact prompt text and
/// replays a cached reply in milliseconds. A retry of a reply the parser could
/// not read would otherwise re-read the same unreadable bytes, five times, and
/// call it five attempts.
///
/// Zero adds nothing at all, so a first ask stays byte-identical between runs —
/// and keeps hitting the cache when it should, on a group re-armed after a
/// settled verdict was lost.
pub fn dedupe_prompt(
    members: &[(&str, &str)],
    differing_values: &[String],
    attempt: i64,
) -> String {
    let mut s = String::new();
    if attempt > 0 {
        s.push_str(&format!("(attempt {})\n", attempt + 1));
    }
    for (i, (title, text)) in members.iter().enumerate() {
        let letter = (b'a' + i as u8) as char;
        s.push_str(&format!(
            "----- ARTIFACT {} -----\nTitle: {title}\n\n{text}\n",
            letter.to_ascii_uppercase()
        ));
    }
    s.push_str("----- END -----");
    if !differing_values.is_empty() {
        s.push_str(&format!(
            "\n\nThese values are not stated the same way by all of them: {}. \
             That may be a real disagreement, or the same subject described at \
             different levels of detail. Decide which.",
            differing_values.join(", ")
        ));
    }
    s
}

/// Two knowledge artifacts keep being retrieved by the same searches, and this
/// call says what that means, in one line a reader would find useful.
pub const LINK_SYSTEM: &str = r#"Two knowledge artifacts keep being retrieved by the same searches. You say what that means, in one line a reader would find useful.

Choose exactly one:

- "related" — being needed together makes sense: one is the configuration and the other its failure mode, one is the procedure and the other the tool it needs, one explains why the other is done. Say what the relation is, in the reader's own terms, in one sentence.
- "unrelated" — the searches that returned both were about something else, and there is no connection worth showing. A shared word is not a connection.
- "duplicate" — they say the same thing in different words. Only this, and not "related", when neither adds anything the other lacks.

Judge the relation between the artifacts, not their similarity. Two texts that share no vocabulary at all can be strongly related; two that read alike can be about different subjects.

Reply with JSON only, no commentary, in exactly this shape:

{"relation": "related", "reason": "..."}

- relation: one of "related", "unrelated", "duplicate".
- reason: one sentence. For "related" it is shown to the reader beside the link, so write it for them and not about the task."#;

/// Two artifacts, and the questions that kept returning both.
///
/// The cues are the evidence. Without them this asks whether two arbitrary texts
/// are related, which is a worse question with a worse answer: what is being
/// judged is why these two keep being *needed at once*.
///
/// `attempt` is in the prompt for the same reason it is in `dedupe_prompt`: the
/// endpoint caches by exact prompt text, and a retry of a reply the parser could
/// not read would otherwise re-read the same unreadable bytes. Zero adds
/// nothing, so a first ask stays byte-identical between runs.
pub fn link_prompt(a: (&str, &str), b: (&str, &str), cues: &[String], attempt: i64) -> String {
    let mut s = String::new();
    if attempt > 0 {
        s.push_str(&format!("(attempt {})\n", attempt + 1));
    }
    s.push_str(&format!(
        "----- ARTIFACT A -----\nTitle: {}\n\n{}\n----- ARTIFACT B -----\nTitle: {}\n\n{}\n----- END -----",
        a.0, a.1, b.0, b.1
    ));
    if !cues.is_empty() {
        s.push_str(&format!(
            "\n\nBoth were returned by these searches: {}.",
            cues.join("; ")
        ));
    }
    s
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

/// A reply that cannot be read is an error, not a verdict.
///
/// Defaulting to "conflict" would fill the escalation queue with noise a person
/// has to clear by hand; defaulting to "distinct" would quietly close real
/// duplicates. Failing leaves the group pending, and the unit retries under the
/// queue's backoff with a prompt that differs by its attempt number.
pub fn parse_dedupe(body: &str) -> Result<Dedupe> {
    #[derive(serde::Deserialize)]
    struct Raw {
        relation: String,
        #[serde(default)]
        detail: Option<String>,
        #[serde(default)]
        supersedes: Option<String>,
        #[serde(default)]
        merged: Option<RawMerged>,
    }
    #[derive(serde::Deserialize)]
    struct RawMerged {
        text: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        category: Option<String>,
        #[serde(default)]
        tags: Vec<String>,
        #[serde(default)]
        caveats: Vec<String>,
    }

    let r: Raw = serde_json::from_str(extract_json(body)).map_err(|e| {
        Error::MalformedLlmOutput(format!("dedupe reply was not the expected JSON: {e}"))
    })?;

    // Any single letter, because `dedupe_prompt` letters as many artifacts as
    // the component has and the fan-in cap — not this parser — is what bounds
    // that. Stopping at "d" silently downgraded every direction named in a group
    // of five or more to a conflict, which turned the cheapest and most faithful
    // outcome, superseding one stored original by another, into a queue entry
    // for a person.
    //
    // How far the letters actually run is the caller's to know: it resolves this
    // against the list it showed, and a letter past the end downgrades there.
    // Anything else the model wrote — a stray word, a whole sentence — is
    // treated the same as omitting it. An unreadable direction must not fail an
    // otherwise perfectly readable verdict.
    let side = r.supersedes.as_deref().and_then(|s| {
        let mut chars = s.trim().chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) if c.is_ascii_alphabetic() => Some(c.to_ascii_lowercase()),
            _ => None,
        }
    });

    let relation = match r.relation.trim().to_ascii_lowercase().as_str() {
        "duplicate" => Relation::Duplicate,
        // A direction the model would not name is not a direction. Falling back
        // to a conflict is what stops this picking a side by accident, which on
        // a supersede means hiding an artifact for no stated reason.
        "replaced" if side.is_some() => Relation::Replaced,
        "replaced" | "conflict" => Relation::Conflict,
        "distinct" => Relation::Distinct,
        other => {
            return Err(Error::MalformedLlmOutput(format!(
                "dedupe reply named an unknown relation {other:?}"
            )));
        }
    };

    // `merged` belongs to `duplicate` and to nothing else. A conflict verdict
    // that still handed us text to write would defeat the one outcome that
    // verdict exists to produce — and a duplicate with no text is a merge the
    // write path cannot carry out.
    match (&relation, &r.merged) {
        (Relation::Duplicate, None) => {
            return Err(Error::MalformedLlmOutput(
                "dedupe reply said duplicate but wrote no merged artifact".into(),
            ));
        }
        (rel, Some(_)) if *rel != Relation::Duplicate => {
            return Err(Error::MalformedLlmOutput(format!(
                "dedupe reply carried a merged artifact on a {} verdict",
                r.relation.trim()
            )));
        }
        _ => {}
    }

    if let Some(m) = &r.merged
        && m.text.trim().is_empty()
    {
        return Err(Error::MalformedLlmOutput(
            "dedupe reply said duplicate and wrote an empty artifact".into(),
        ));
    }

    Ok(Dedupe {
        relation,
        detail: r
            .detail
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty()),
        supersedes: side,
        merged: r.merged.map(|m| MergedDraft {
            title: m.title,
            text: m.text,
            category: m.category,
            tags: m.tags,
            caveats: m.caveats,
        }),
    })
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

/// The shape `parse_response` will accept, as a JSON Schema for the endpoint to
/// constrain generation with.
///
/// Lives beside `RawArtifact` so the two are read together: a schema that has
/// drifted from the struct it describes constrains the model into output the
/// parser then rejects, which is worse than not constraining it at all.
///
/// Every field is required. The optional ones are optional to *serde*, so an
/// older reply still parses, but there is no reason to let a model that is
/// being told the shape anyway omit the line range or the tags.
pub fn artifacts_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "artifacts": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "text": {"type": "string"},
                        "title": {"type": "string"},
                        "category": {"type": "string"},
                        "tags": {"type": "array", "items": {"type": "string"}},
                        "corpus_lines": {
                            "type": "array",
                            "items": {"type": "integer"},
                            "minItems": 2,
                            "maxItems": 2
                        },
                        "caveats": {"type": "array", "items": {"type": "string"}}
                    },
                    "required": ["text", "title", "category", "tags", "corpus_lines", "caveats"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["artifacts"],
        "additionalProperties": false
    })
}

/// The shape `parse_dedupe` will accept. Lives beside `Raw` for the same reason
/// `artifacts_schema` lives beside `RawArtifact`.
///
/// One variant per relation, rather than one object with everything optional.
///
/// A single flat object cannot say "the merged artifact is required exactly when
/// the relation is duplicate" — and requiring it unconditionally would make the
/// model write a merged artifact for every pair it was asked to keep apart. But
/// a union of per-relation objects says exactly that, and an endpoint that
/// compiles the schema into a decoding constraint then makes the pairing
/// unwritable rather than merely wrong: `duplicate` cannot be emitted without
/// `merged`, and `distinct` cannot be emitted with it.
///
/// That is worth more here than anywhere else, because `parse_dedupe` has no
/// salvage path. A verdict it rejects is not a degraded verdict, it is no
/// verdict, and the pair waits for a whole sweep to be asked about again — which
/// left pairs pending after ten attempts at a conditional a 9B model kept
/// getting wrong.
///
/// `parse_dedupe` still checks the same conditions. A grammar is only as good as
/// the endpoint honouring it, and `structured_output` can be switched off.
pub fn dedupe_schema() -> serde_json::Value {
    let merged = serde_json::json!({
        "type": "object",
        "properties": {
            "text": {"type": "string"},
            "title": {"type": "string"},
            "category": {"type": "string"},
            "tags": {"type": "array", "items": {"type": "string"}},
            "caveats": {"type": "array", "items": {"type": "string"}}
        },
        "required": ["text"]
    });
    serde_json::json!({
        "anyOf": [
            {
                "type": "object",
                "properties": {
                    "relation": {"type": "string", "enum": ["duplicate"]},
                    "detail": {"type": "string"},
                    "merged": merged
                },
                "required": ["relation", "merged"]
            },
            {
                // `supersedes` is required for the same reason `merged` is: a
                // direction the model would not name is downgraded to a conflict
                // by `parse_dedupe`, which turns the cheapest faithful outcome
                // into a queue entry for a person.
                "type": "object",
                "properties": {
                    "relation": {"type": "string", "enum": ["replaced"]},
                    "detail": {"type": "string"},
                    "supersedes": {"type": "string"}
                },
                "required": ["relation", "supersedes"]
            },
            {
                "type": "object",
                "properties": {
                    "relation": {"type": "string", "enum": ["distinct", "conflict"]},
                    "detail": {"type": "string"}
                },
                "required": ["relation"]
            }
        ]
    })
}

/// Models wrap JSON in fences and preface it with prose no matter what the
/// prompt says, so slice from the first `{` to the last `}` before parsing.
pub(crate) fn extract_json(body: &str) -> &str {
    let start = body.find('{');
    let end = body.rfind('}');
    match (start, end) {
        (Some(s), Some(e)) if e > s => &body[s..=e],
        _ => body,
    }
}

/// Recover the artifact objects a truncated or malformed reply still got right.
///
/// A small local model routinely runs out of output budget mid-list, or drops
/// a comma, or leaves a quote unescaped in a passage that itself quotes
/// something — and any of those fails the whole list however complete the
/// rest is. Losing nine good artifacts to one bad one is the worst trade in
/// the write path: it costs a segment of someone's corpus, and re-running it
/// means minutes of a local model's time for a reply just as likely to
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
            // Salvage before giving up: read the list object by object and
            // keep whatever stands up on its own. Asking a slow model to try
            // again is expensive.
            let objects = salvage_objects(json);
            if objects.is_empty() {
                // The parser's complaint names an offset into a reply nobody
                // kept, which is not enough to tell a truncated list from a
                // bad escape from prose where JSON was asked for. Debug rather
                // than warn: this is model output, so it carries corpus text,
                // and it belongs in a log only when someone has gone looking.
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
                "synthesizer output was cut off or malformed; keeping the artifacts that parsed"
            );
            Envelope { artifacts: objects }
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

pub const DESCRIBE_SYSTEM: &str = r#"You read images for a personal knowledge base and write down everything in them worth keeping, as markdown.

Rules:
- Transcribe any visible text faithfully and completely. Keep its structure: headings as headings, lists as lists, tables as markdown tables, code as code blocks. Do not correct, summarize or reorder it.
- Where there is no text, or beside it, describe what is shown: diagrams (their parts and how they connect), charts (axes, series, the values that can be read), scenes, objects, people's roles if evident, places, labels, brands, numbers, dates. Name what is identifiable.
- Prefer specifics over impressions. Do not pad, do not speculate beyond what is visible, do not add advice.
- You may be given context about the capture: a note from the person who took it, when and where it was taken, the device. Where it is relevant, weave it in naturally so the text can be found again by it — as a short opening line or where it explains the content — but do not repeat it mechanically or invent detail around it.
- Output markdown only. No preamble, no closing remarks, no mention of these instructions."#;

/// The user turn's text part for `Describer::describe`: the note first, then
/// the facts the file carried, each only when present.
pub fn describe_context(metadata: &serde_json::Value) -> String {
    let mut lines: Vec<String> = Vec::new();
    if let Some(note) = metadata["note"].as_str().filter(|n| !n.trim().is_empty()) {
        lines.push(format!(
            "Context from the person who captured this: {}",
            note.trim()
        ));
    }
    let mut facts: Vec<String> = Vec::new();
    let exif = &metadata["exif"];
    if let Some(t) = exif["taken_at"].as_str() {
        facts.push(format!("taken {t}"));
    }
    if let (Some(lat), Some(lon)) = (exif["gps"]["lat"].as_f64(), exif["gps"]["lon"].as_f64()) {
        facts.push(format!("GPS {lat:.4},{lon:.4}"));
    }
    if let Some(c) = exif["camera"].as_str() {
        facts.push(format!("device {c}"));
    }
    if let Some(n) = metadata["file"]["name"].as_str() {
        facts.push(format!("file {n}"));
    }
    if !facts.is_empty() {
        lines.push(format!("Capture facts: {}.", facts.join(", ")));
    }
    lines.push("Read the image and write down everything worth keeping.".into());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_link_prompt_carries_both_titles_and_the_questions_that_bound_them() {
        // The binding queries are the evidence. Without them the model is being
        // asked whether two arbitrary texts are related, which is a different
        // and much worse question than why these two keep being needed at once.
        let p = link_prompt(
            ("Mounting E01 images", "ewfmount /dev/..."),
            ("Loop device limits", "max_loop=64"),
            &["mount forensic image".into()],
            0,
        );
        assert!(p.contains("Mounting E01 images"));
        assert!(p.contains("max_loop=64"));
        assert!(p.contains("mount forensic image"));
        assert!(
            !p.contains("attempt"),
            "a first ask must stay cache-identical"
        );
        assert!(link_prompt(("a", "b"), ("c", "d"), &[], 2).contains("attempt 3"));
    }

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

    /// The schemas are sent to the endpoint to constrain decoding, so a schema
    /// that has drifted from its parser constrains the model into output the
    /// parser then rejects — a failure that looks exactly like a bad model and
    /// cannot be fixed by retrying.
    #[test]
    fn a_reply_that_satisfies_the_artifact_schema_parses() {
        let required = artifacts_schema()["properties"]["artifacts"]["items"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        let reply = r#"{"artifacts":[{"text":"body","title":"A","category":"note",
            "tags":["t"],"corpus_lines":[1,4],"caveats":["only on linux"]}]}"#;
        // The literal above is the model's side of the bargain: every field the
        // schema makes mandatory has to be one this parser reads.
        for field in &required {
            assert!(
                reply.contains(&format!("\"{field}\"")),
                "the schema requires {field}, which this test never proves parsable"
            );
        }
        let out = parse_response(reply).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].corpus_lines, Some((1, 4)));
        assert_eq!(out[0].caveats, vec!["only on linux".to_string()]);
    }

    #[test]
    fn every_relation_the_dedupe_schema_allows_is_one_the_parser_knows() {
        let schema = dedupe_schema();
        let variants = schema["anyOf"].as_array().expect("a union of variants");
        let mut seen = Vec::new();

        for variant in variants {
            let required: Vec<&str> = variant["required"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| f.as_str().unwrap())
                .collect();

            for relation in variant["properties"]["relation"]["enum"]
                .as_array()
                .unwrap()
            {
                let r = relation.as_str().unwrap();
                seen.push(r.to_string());

                // The minimum this variant permits: the relation plus whatever
                // it makes mandatory alongside. A grammar built from this schema
                // cannot emit less, so a parser that rejects it rejects a reply
                // the model was steered into writing.
                let mut body = serde_json::json!({ "relation": r });
                for field in &required {
                    match *field {
                        "relation" => {}
                        "merged" => body["merged"] = serde_json::json!({"text": "merged body"}),
                        // A single letter, which is all `parse_dedupe` reads as
                        // a direction; anything else downgrades to a conflict.
                        "supersedes" => body["supersedes"] = serde_json::json!("a"),
                        other => {
                            panic!("the schema requires {other:?}, which this test cannot build")
                        }
                    }
                }
                assert!(
                    parse_dedupe(&body.to_string()).is_ok(),
                    "the schema lets the model answer {body}, which the parser rejects"
                );
            }
        }

        seen.sort();
        assert_eq!(
            seen,
            ["conflict", "distinct", "duplicate", "replaced"],
            "the union must still cover every relation, exactly once"
        );
    }

    /// The pairing the flat schema could not express. These are the two replies
    /// that stalled real pairs for ten attempts each, and a union of per-relation
    /// variants is what makes them ungrammatical rather than merely rejected.
    #[test]
    fn no_dedupe_variant_permits_a_duplicate_without_a_merge_or_a_distinct_with_one() {
        let schema = dedupe_schema();
        for variant in schema["anyOf"].as_array().unwrap() {
            let relations = variant["properties"]["relation"]["enum"]
                .as_array()
                .unwrap();
            let names: Vec<&str> = relations.iter().map(|r| r.as_str().unwrap()).collect();
            let required = variant["required"].as_array().unwrap();
            let requires_merged = required.iter().any(|f| f == "merged");
            let offers_merged = variant["properties"].get("merged").is_some();

            if names.contains(&"duplicate") {
                assert!(requires_merged, "duplicate may be written without a merge");
            } else {
                assert!(
                    !offers_merged,
                    "{names:?} may carry a merge, which the parser refuses"
                );
            }
        }

        // And the parser still refuses both, because a grammar is only as good
        // as the endpoint honouring it and `structured_output` can be off.
        assert!(parse_dedupe(r#"{"relation":"duplicate"}"#).is_err());
        assert!(parse_dedupe(r#"{"relation":"distinct","merged":{"text":"x"}}"#).is_err());
    }

    #[test]
    fn the_dedupe_pass_is_told_what_each_artifact_is_about() {
        // The real case this fixes: an artifact headed "FAT32 Specifications"
        // whose body opens "32 Bit Clusternummern" and never says FAT32 again.
        // Given the bodies alone, the model saw two anonymous spec lists with
        // different numbers and called them a contradiction — which was the
        // only honest answer to the question it was actually asked.
        let p = dedupe_prompt(
            &[
                ("FAT16 Specifications", "Die max. Partitionsgröße: 2 GB."),
                ("FAT32 Specifications", "32 Bit Clusternummern."),
            ],
            &[],
            0,
        );
        assert!(p.contains("Title: FAT16 Specifications"), "{p}");
        assert!(p.contains("Title: FAT32 Specifications"), "{p}");
        assert!(p.contains("Die max. Partitionsgröße: 2 GB."));
    }

    #[test]
    fn a_component_is_lettered_so_a_direction_can_name_one() {
        // `supersedes` answers with a letter, so the letters have to be in the
        // prompt and in the same order the caller will read them back in.
        let p = dedupe_prompt(&[("one", "a"), ("two", "b"), ("three", "c")], &[], 0);
        assert!(p.contains("ARTIFACT A"), "{p}");
        assert!(p.contains("ARTIFACT B"), "{p}");
        assert!(p.contains("ARTIFACT C"), "{p}");
    }

    #[test]
    fn differing_values_are_named_as_a_prior_not_a_verdict() {
        let p = dedupe_prompt(
            &[("t", "timeout is 30s"), ("t", "timeout is 90s")],
            &["30s".into(), "90s".into()],
            0,
        );
        assert!(p.contains("30s, 90s"), "{p}");
        assert!(p.contains("Decide which."), "{p}");
        // And nothing is added when there is nothing to say.
        assert!(!dedupe_prompt(&[("t", "x"), ("t", "y")], &[], 0).contains("Decide which."));
    }

    #[test]
    fn a_retry_does_not_ask_the_endpoint_the_question_it_has_cached() {
        // The endpoint replays a cached reply for an identical prompt in
        // milliseconds. A group whose reply the parser could not read is retried
        // up to `MAX_ATTEMPTS` times, and every one of those would have read the
        // same unreadable bytes back.
        let members: &[(&str, &str)] = &[
            ("FAT16 Specifications", "Die max. Partitionsgröße: 2 GB."),
            ("FAT32 Specifications", "32 Bit Clusternummern."),
        ];
        let first = dedupe_prompt(members, &[], 0);
        let second = dedupe_prompt(members, &[], 1);
        assert_ne!(first, second);
        assert_ne!(second, dedupe_prompt(members, &[], 2));
        // A first ask stays exactly what it was, so the cache still earns its
        // keep on a group re-armed after a verdict was lost.
        assert!(first.starts_with("----- ARTIFACT A -----"), "{first}");
    }

    #[test]
    fn the_dedupe_pass_is_told_that_different_subjects_are_not_a_conflict() {
        // Two sections of one reference document are near-identical in form and
        // deliberately different in content, so similarity puts them in a pair
        // and every number in them differs. Without this rule the feature fires
        // hardest exactly where it is most wrong — and now it would not merely
        // flag them, it would merge them into mush.
        assert!(DEDUPE_SYSTEM.contains("same subject"));
        assert!(DEDUPE_SYSTEM.contains("different things"));
        assert!(DEDUPE_SYSTEM.contains(r#"Answer "distinct" and stop."#));
        // And the fidelity preference, which is what keeps most groups from
        // producing synthetic text at all.
        assert!(DEDUPE_SYSTEM.contains("Prefer this whenever it applies"));
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
    fn a_duplicate_verdict_carries_a_merged_draft() {
        let d = parse_dedupe(
            r#"{"relation":"duplicate","detail":"same command, more detail",
                "merged":{"title":"Bind mounts","text":"Use mount --bind.",
                          "tags":["mount"],"caveats":[],"category":"howto"}}"#,
        )
        .unwrap();
        assert_eq!(d.relation, Relation::Duplicate);
        let m = d.merged.as_ref().unwrap();
        assert_eq!(m.text, "Use mount --bind.");
        assert_eq!(m.title.as_deref(), Some("Bind mounts"));
        assert_eq!(m.tags, vec!["mount".to_string()]);
    }

    #[test]
    fn a_merged_block_on_a_non_duplicate_verdict_is_unreadable() {
        // `merged` belongs to `duplicate` and to nothing else. Accepting it
        // elsewhere would let a reply that classified a group as a conflict
        // still hand us text to write — which is the one outcome the conflict
        // verdict exists to prevent.
        for relation in ["conflict", "replaced", "distinct"] {
            let body = format!(
                r#"{{"relation":"{relation}","supersedes":"a",
                     "merged":{{"text":"x","tags":[],"caveats":[]}}}}"#
            );
            assert!(
                matches!(parse_dedupe(&body), Err(Error::MalformedLlmOutput(_))),
                "a {relation} verdict was allowed to carry a merge"
            );
        }
    }

    #[test]
    fn a_duplicate_verdict_with_nothing_to_write_is_unreadable() {
        // A merge the write path cannot carry out. Failing re-asks; accepting
        // would settle the group having done nothing.
        assert!(matches!(
            parse_dedupe(r#"{"relation":"duplicate","detail":"x"}"#),
            Err(Error::MalformedLlmOutput(_))
        ));
        assert!(matches!(
            parse_dedupe(
                r#"{"relation":"duplicate","merged":{"text":"   ","tags":[],"caveats":[]}}"#
            ),
            Err(Error::MalformedLlmOutput(_))
        ));
    }

    #[test]
    fn a_replacement_names_the_obsolete_side() {
        let d = parse_dedupe(
            r#"{"relation":"replaced","supersedes":"B","detail":"a uses --old-flag"}"#,
        )
        .unwrap();
        assert_eq!(d.relation, Relation::Replaced);
        assert_eq!(d.supersedes, Some('b'));
    }

    #[test]
    fn a_direction_reaches_as_far_as_the_letters_the_prompt_hands_out() {
        // `dedupe_prompt` letters one artifact per component member, and the
        // fan-in cap defaults to eight — so H is a letter the model is routinely
        // invited to answer with. A parser that stopped at D turned every one of
        // those into a conflict, which spends a person on a group the model had
        // already resolved the cheap way.
        for (letter, want) in [("E", 'e'), ("f", 'f'), ("H", 'h')] {
            let d = parse_dedupe(&format!(
                r#"{{"relation":"replaced","supersedes":"{letter}","detail":"stale"}}"#
            ))
            .unwrap();
            assert_eq!(
                d.relation,
                Relation::Replaced,
                "{letter} was not a direction"
            );
            assert_eq!(d.supersedes, Some(want));
        }
    }

    #[test]
    fn a_replacement_naming_no_side_falls_back_to_a_conflict() {
        // A direction the model would not name is not a direction. Treating it
        // as one would pick a side by accident, and on a supersede that means
        // hiding an artifact for no stated reason.
        let d = parse_dedupe(r#"{"relation":"replaced","detail":"one of them is old"}"#).unwrap();
        assert_eq!(d.relation, Relation::Conflict);
        let d =
            parse_dedupe(r#"{"relation":"replaced","supersedes":"not sure honestly"}"#).unwrap();
        assert_eq!(d.relation, Relation::Conflict);
    }

    #[test]
    fn a_verdict_wrapped_in_prose_and_fences_still_parses() {
        // The same models that fence the synthesis reply fence this one.
        let d = parse_dedupe("Sure:\n```json\n{\"relation\": \"distinct\"}\n```").unwrap();
        assert_eq!(d.relation, Relation::Distinct);
    }

    #[test]
    fn an_unparsable_verdict_is_an_error_not_a_default() {
        // Defaulting to "conflict" would fill the escalation queue with noise a
        // person has to clear by hand; defaulting to "distinct" would quietly
        // close real duplicates. Neither: it fails, the group stays pending, and
        // the unit asks again with a prompt the endpoint has not cached.
        assert!(parse_dedupe("I could not decide.").is_err());
        assert!(parse_dedupe(r#"{"relation":"maybe"}"#).is_err());
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

    #[test]
    fn describe_context_leads_with_the_note_then_the_facts_and_omits_what_is_absent() {
        let m = serde_json::json!({
            "note": "whiteboard from Tuesday planning",
            "file": {"name": "IMG_2041.jpeg"},
            "exif": {"taken_at": "2026-08-09T14:12:03", "camera": "Apple iPhone 15",
                     "gps": {"lat": 48.2082, "lon": 16.3738}}
        });
        let ctx = describe_context(&m);
        let note_at = ctx.find("whiteboard from Tuesday planning").unwrap();
        let taken_at = ctx.find("2026-08-09T14:12:03").unwrap();
        assert!(note_at < taken_at, "{ctx}");
        assert!(ctx.contains("48.2082"), "{ctx}");
        assert!(ctx.contains("Apple iPhone 15"));
        assert!(ctx.contains("IMG_2041.jpeg"));

        let bare = describe_context(&serde_json::json!({}));
        assert!(!bare.contains("taken"), "{bare}");
        assert!(!bare.contains("GPS"), "{bare}");
        assert!(bare.contains("Read the image"), "{bare}");
    }
}
