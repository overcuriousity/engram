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

{"artifacts":[{"text":"...","title":"...","category":"...","corpus_lines":[start,end],"caveats":["..."]}]}

- title: a short noun phrase naming the artifact.
- category: exactly one of: concept, procedure, reference, snippet,
  configuration, definition, example, other. This is what kind of thing the
  artifact is, never what subject it is about.
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

{"verdict": {"relation": "duplicate", "detail": "...", "merged": {"title": "...", "text": "...", "category": "...", "caveats": []}}}

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

{"verdict": {"relation": "related", "reason": "..."}}

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

/// The words an abstaining answer opens with. One definition for the string
/// the model is told and the string `abstained` looks for, for the reason
/// `Caveat:` is: splitting the two apart is how the agreement quietly breaks.
pub const ABSTAIN_PREFIX: &str = "Not in the knowledge base";

pub const ASK_SYSTEM: &str = "You answer questions using only the provided knowledge-base excerpts. \
Quote commands, paths and code exactly as they appear. If the excerpts do not contain the answer, \
begin your reply with the exact words `Not in the knowledge base.` and say what is missing rather \
than guessing. Cite excerpts by their number. \
An excerpt may carry lines beginning `Caveat:` — the conditions under which it does not apply. \
Repeat any caveat that bears on your answer rather than dropping it.";

/// Whether an answer opened with `ABSTAIN_PREFIX`. Leading whitespace, markdown
/// emphasis, heading and list marks, and opening quotes are skipped, because
/// models wrap an opening sentence in them no matter what they were told; the
/// comparison is case-insensitive for the same reason. Mentioning the phrase
/// later in a real answer is not an abstention.
///
/// Skipping an opening quote is what makes that last rule hard, because the one
/// thing a quote mark can mean is that the phrase is being *quoted* — `"Not in
/// the knowledge base" is the wrong read here; excerpt 3 covers it.` is an
/// answer, not an abstention, and scoring it as one records a gap that is not
/// there. So a lead-in that included a quote is only accepted when the phrase
/// runs on: a quote closing immediately after it, with an answer behind that,
/// is the shape of a quotation and nothing else. An abstention that was itself
/// quoted closes after its full stop — `„Not in the knowledge base.“ Nothing
/// covers it.` — and a bare quoted phrase with nothing behind it has no answer
/// to be the quotation's point, so both still count.
///
/// Compared over characters rather than over a byte slice of the prefix's
/// length: a non-ASCII lead-in — a typographic quote, a bullet — moves the
/// prefix off a byte boundary, and slicing there yields `None`, which scored a
/// correct abstention as a wrong answer.
pub fn abstained(answer: &str) -> bool {
    let quote = |c: char| matches!(c, '"' | '\'' | '„' | '“' | '”' | '‘' | '’' | '«' | '»');
    let marks = |c: char| {
        c.is_whitespace()
            || quote(c)
            || matches!(c, '*' | '_' | '#' | '>' | '`' | '-' | '+' | '•' | '·')
    };
    let opening = answer.trim_start_matches(marks);
    // An ordered list marker — `1.` or `1)` — is the one lead-in that is not a
    // single class of character.
    let digits = opening.len()
        - opening
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .len();
    let opening = match opening[digits..].strip_prefix(['.', ')']) {
        Some(rest) if digits > 0 => rest.trim_start_matches(marks),
        _ => opening,
    };
    let head: String = opening
        .chars()
        .take(ABSTAIN_PREFIX.chars().count())
        .collect();
    if !head.eq_ignore_ascii_case(ABSTAIN_PREFIX) {
        return false;
    }
    if !answer[..answer.len() - opening.len()].contains(quote) {
        return true;
    }
    // A quote was opened. Where it closes decides what it was doing: right after
    // the phrase with an answer behind it, the phrase was being quoted.
    let rest = &opening[head.len()..];
    match rest.chars().next() {
        Some(c) if quote(c) => rest[c.len_utf8()..].trim().is_empty(),
        _ => true,
    }
}

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

/// The claim check behind the ask harness. It never runs on a request path.
pub const CLAIMS_SYSTEM: &str = r#"You check an answer against the excerpts it was written from. Split the answer into its atomic factual claims — one statement each, in the answer's own words. For every claim, list the numbers of the excerpts that state or directly entail it. A claim no excerpt supports gets an empty list. Do not judge whether a claim is true, only whether the excerpts say it. Reply with JSON only: {"claims":[{"claim":"…","supported_by":[1,3]}]}"#;

pub fn claims_prompt(answer: &str, excerpts: &[String]) -> String {
    format!(
        "Answer:\n{answer}\n\nExcerpts:\n\n{}",
        excerpts.join("\n\n---\n\n")
    )
}

/// The shape `eval::claims::parse_claims` reads. Rooted in an object and
/// closed, like every judge schema.
pub fn claims_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "claims": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "claim": {"type": "string"},
                        "supported_by": {"type": "array", "items": {"type": "integer"}}
                    },
                    "required": ["claim", "supported_by"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["claims"],
        "additionalProperties": false
    })
}

/// The one question the retrieval loop is allowed to ask itself.
///
/// Deliberately narrow: it may name one more thing to look for, or nothing at
/// all. It is never asked to answer, to plan, or to say how many rounds it
/// wants — "let the model say once what it still needs" is the bounded version
/// of a mechanism whose unbounded version is an agent, and an agent is not what
/// this is.
pub const FOLLOW_UP_SYSTEM: &str = r#"You are helping a search system decide whether it has enough material.

You are given a question and the excerpts retrieved for it. Decide whether the
excerpts together contain what is needed to answer.

Reply with JSON only, in exactly this shape:

{"need": "a short search query" }   or   {"need": null}

- null: the excerpts are sufficient. This is the common answer.
- a query: name the ONE thing that is missing, as the words you would search
  for. Not a question, not a sentence — a query. Never repeat the original
  question back."#;

/// `need` is nullable in the schema rather than optional, because "I have
/// enough" is the common answer and a grammar that can only express a query
/// would put the model in the position of inventing one.
pub fn follow_up_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": { "need": {"type": ["string", "null"]} },
        "required": ["need"],
        "additionalProperties": false
    })
}

/// The query to run next, or `None` for "the excerpts are enough".
///
/// Every failure reads as `None`: unparsable output, a missing field, a reply
/// that was prose. A follow-up is an optional extra round, so anything that is
/// not unambiguously a query has to mean "do not spend a second retrieval on
/// it" — the alternative is searching for a fragment of an error message.
///
/// Whitespace-only counts as nothing further for the same reason a model that
/// answers `{"need": ""}` means it has enough: an empty search finds the whole
/// base or none of it, and neither is what was asked for.
pub fn parse_follow_up(reply: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(extract_json(reply)).ok()?;
    let need = v["need"].as_str()?.trim();
    (!need.is_empty()).then(|| need.to_string())
}

/// Names a knowledge gap from the questions in it. Sees questions only, never
/// answers: it names the hole, not the guess.
pub const GAP_LABEL_SYSTEM: &str = r#"You name topics. Given several questions a knowledge base could not answer, reply with the name of the subject they share — three to six words, a noun phrase, no quotes, no trailing punctuation. Reply with JSON only: {"label":"…"}"#;

/// How many questions one naming call is shown, and how much of each.
///
/// A cluster can hold every open gap of a kind, and every other prompt in this
/// module packs to a budget while this one concatenated whatever it was handed.
/// A prompt over the context window fails the call, the group falls back to a
/// terms label, and — because a terms label is offered to the model again — it
/// pays that failed call on every sweep for as long as the group lives. Twelve
/// questions is far more than naming a subject needs, and a question long enough
/// to be cut is a pasted paragraph rather than a question.
pub const GAP_LABEL_MAX_QUESTIONS: usize = 12;
pub const GAP_LABEL_MAX_CHARS: usize = 200;

/// The caller passes its questions newest first, so the cap keeps the newest.
pub fn gap_label_prompt(questions: &[&str]) -> String {
    let mut s = String::from("Questions:\n");
    for q in questions.iter().take(GAP_LABEL_MAX_QUESTIONS) {
        s.push_str("- ");
        match q.char_indices().nth(GAP_LABEL_MAX_CHARS) {
            Some((cut, _)) => {
                s.push_str(&q[..cut]);
                s.push('…');
            }
            None => s.push_str(q),
        }
        s.push('\n');
    }
    s
}

pub fn gap_label_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": { "label": {"type": "string"} },
        "required": ["label"],
        "additionalProperties": false
    })
}

/// The label out of the reply, trimmed of quotes and trailing punctuation;
/// an empty label is an error, because a cluster must be called something.
pub fn parse_gap_label(reply: &str) -> Result<String> {
    let v: serde_json::Value = serde_json::from_str(extract_json(reply))
        .map_err(|e| Error::MalformedLlmOutput(format!("gap label was not JSON: {e}")))?;
    let label = v["label"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .trim_matches(|c: char| c == '"' || c == '\'' || c == '.')
        .trim()
        .to_string();
    if label.is_empty() {
        return Err(Error::MalformedLlmOutput("gap label was empty".into()));
    }
    Ok(label)
}

/// The verdict inside the envelope, or the reply itself if it came without one.
///
/// `dedupe_schema` and `link_schema` both wrap their union under `verdict`,
/// because a strict `json_schema` response format needs an object at the root.
/// The bare shape is still accepted: `structured_output` can be off, and a model
/// told the shape in prose sometimes writes the inner object alone. Either is
/// readable, and refusing one of them would only cost a verdict that was fine.
pub fn unwrap_verdict(body: &str) -> Result<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| Error::MalformedLlmOutput(format!("judge reply was not JSON at all: {e}")))?;
    Ok(match v.get("verdict") {
        Some(inner) if inner.is_object() => inner.clone(),
        _ => v,
    })
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
        caveats: Vec<String>,
    }

    let r: Raw = serde_json::from_value(unwrap_verdict(extract_json(body))?).map_err(|e| {
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
            category: m.category.map(|c| normalize_category(&c)),
            // Same rule as a synthesised artifact: nothing writes tags on a
            // caller's behalf, and a merge inventing its own would be the
            // drifting vocabulary arriving by a second door. What the sources
            // were already filed under is added back by `merge::carried_tags`,
            // which is a different question from what the model may name.
            tags: Vec::new(),
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
    corpus_lines: Option<Vec<i64>>,
    #[serde(default)]
    caveats: Vec<String>,
}

/// What kind of thing an artifact is.
///
/// A field about *form*, not about subject. These words are true of a corpus of
/// recipes, of case law, or of forensics notes alike, which is what makes them
/// safe to state here: naming forms hard-codes no domain.
///
/// Closed, because leaving it open is what let a domain in. Given a free
/// string the model answered "System Administration" and "Forensic Science /
/// Criminalistics" — subject words in a form field, which the filter row then
/// offered beside `concept` and `procedure` as though they were the same kind
/// of choice, in a second unlabelled taxonomy nothing had asked for. Anything
/// off this list becomes `other`.
pub const CATEGORIES: &[&str] = &[
    "concept",
    "procedure",
    "reference",
    "snippet",
    "configuration",
    "definition",
    "example",
    "other",
];

/// The stored form of whatever the model answered.
///
/// Never rejects: a good artifact carrying an unrecognised label is still a
/// good artifact, and refusing it would spend the call again to get the same
/// text back with a different word beside it.
pub fn normalize_category(raw: &str) -> String {
    let t = raw.trim().to_ascii_lowercase();
    if CATEGORIES.contains(&t.as_str()) {
        t
    } else {
        "other".to_string()
    }
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
/// being told the shape anyway omit the line range or the caveats.
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
                        "category": {"type": "string", "enum": CATEGORIES},
                        "corpus_lines": {
                            "type": "array",
                            "items": {"type": "integer"},
                            "minItems": 2,
                            "maxItems": 2
                        },
                        "caveats": {"type": "array", "items": {"type": "string"}}
                    },
                    "required": ["text", "title", "category", "corpus_lines", "caveats"],
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
/// Both halves of that guarantee rest on `additionalProperties: false`. Without
/// it a JSON Schema object accepts any property it does not mention, so
/// `{"relation":"distinct","merged":{…}}` satisfies the third variant and the
/// union constrains nothing it was written to constrain. llama.cpp's grammar
/// generator closes objects implicitly and hid that; a validating gateway does
/// not.
///
/// The union is nested under `verdict` rather than sitting at the root because
/// a strict `json_schema` response format must have an object at the root — a
/// bare `anyOf` there is rejected outright by the hosted APIs, which turns a
/// loose constraint into a failed call. `parse_dedupe` unwraps the envelope.
///
/// That is worth more here than anywhere else, because `parse_dedupe` has no
/// salvage path. A verdict it rejects is not a degraded verdict, it is no
/// verdict, and the pair waits for a whole sweep to be asked about again — which
/// left pairs pending after ten attempts at a conditional a 9B model kept
/// getting wrong.
///
/// `parse_dedupe` still checks the same conditions. A grammar is only as good as
/// the endpoint honouring it, and `structured_output` can be switched off.
///
/// Every variant requires every property it names, which `strict` demands: a
/// listed-but-optional property is not a looser schema, it is a rejected one.
/// `supersedes`'s `pattern` is in the same supported set as the item bounds
/// `artifacts_schema` already relies on.
pub fn dedupe_schema() -> serde_json::Value {
    let merged = serde_json::json!({
        "type": "object",
        "properties": {
            "text": {"type": "string"},
            "title": {"type": "string"},
            "category": {"type": "string", "enum": CATEGORIES},
            "caveats": {"type": "array", "items": {"type": "string"}}
        },
        // Every field, for the reason `artifacts_schema` requires every field:
        // a model being told the shape anyway has no reason to omit a field,
        // and `strict` makes the rule structural rather than stylistic — a
        // property a strict schema lists and does not require is a schema the
        // hosted APIs reject outright, which fails the call rather than
        // loosening it.
        "required": ["text", "title", "category", "caveats"],
        "additionalProperties": false
    });
    serde_json::json!({
        "type": "object",
        "properties": {
            "verdict": {
                "anyOf": [
                    {
                        "type": "object",
                        "properties": {
                            "relation": {"type": "string", "enum": ["duplicate"]},
                            "detail": {"type": "string"},
                            "merged": merged
                        },
                        "required": ["relation", "detail", "merged"],
                        "additionalProperties": false
                    },
                    {
                        // `supersedes` is required for the same reason `merged`
                        // is: a direction the model would not name is downgraded
                        // to a conflict by `parse_dedupe`, which turns the
                        // cheapest faithful outcome into a queue entry for a
                        // person.
                        //
                        // The pattern is that parser's rule written down. Left
                        // unconstrained, a grammar satisfies "string" with "the
                        // second one" and the verdict downgrades anyway — the
                        // exact outcome requiring the field was meant to avoid.
                        "type": "object",
                        "properties": {
                            "relation": {"type": "string", "enum": ["replaced"]},
                            "detail": {"type": "string"},
                            "supersedes": {"type": "string", "pattern": "^[A-Za-z]$"}
                        },
                        "required": ["relation", "detail", "supersedes"],
                        "additionalProperties": false
                    },
                    {
                        "type": "object",
                        "properties": {
                            "relation": {"type": "string", "enum": ["distinct", "conflict"]},
                            "detail": {"type": "string"}
                        },
                        "required": ["relation", "detail"],
                        "additionalProperties": false
                    }
                ]
            }
        },
        "required": ["verdict"],
        "additionalProperties": false
    })
}

/// The shape `associate::parse_link` will accept.
///
/// Its own schema, and not the dedupe one, because the two judges share an
/// endpoint and a `HttpCompleter` used to share a response format with it. A
/// link asked under the dedupe grammar can only answer with a dedupe verdict —
/// `related` and `unrelated` are not in that enum and `reason` is not one of its
/// properties — so every link either came back as a spurious `duplicate` or
/// failed to parse until the pair was shelved as unreadable.
///
/// Nested under `verdict` for the same reason `dedupe_schema` is, so both
/// judges unwrap the same envelope shape.
pub fn link_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "verdict": {
                "type": "object",
                "properties": {
                    "relation": {"type": "string", "enum": ["related", "unrelated", "duplicate"]},
                    "reason": {"type": "string"}
                },
                "required": ["relation", "reason"],
                "additionalProperties": false
            }
        },
        "required": ["verdict"],
        "additionalProperties": false
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
            // An absent category stays absent: a model that answered nothing
            // made no claim, and `other` is a claim. Anything it did answer is
            // held to the list — the schema asks for one of them, and a model
            // that ignores an enum would otherwise put its own word straight
            // into the filter row.
            category: c
                .category
                .filter(|t| !t.trim().is_empty())
                .map(|t| normalize_category(&t)),
            // Nothing writes tags automatically any more. No domain-agnostic
            // vocabulary exists for subject words, so a generated one is a
            // vocabulary that drifts: `forensics` and `forensik`, `security`
            // and `sicherheit`, two filters over one idea and nothing able to
            // tell they are the same. The field stays — it is the channel
            // pinning rides on and a public API filter — written by a caller
            // who means a particular tag.
            tags: Vec::new(),
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

    /// `null` is the common answer and must be readable as "I have enough",
    /// never as a query to run.
    #[test]
    fn a_null_need_parses_as_nothing_further() {
        assert_eq!(parse_follow_up(r#"{"need": null}"#), None);
        assert_eq!(parse_follow_up(r#"{"need": ""}"#), None);
        assert_eq!(parse_follow_up(r#"{"need": "   "}"#), None);
    }

    /// A follow-up is an optional extra round, so anything that is not
    /// unambiguously a query means "spend no second retrieval on it" rather
    /// than "search for whatever this was".
    #[test]
    fn a_reply_that_is_not_the_shape_asked_for_is_nothing_further() {
        assert_eq!(parse_follow_up("I think you need more on tickers"), None);
        assert_eq!(parse_follow_up(r#"{"query": "tickers"}"#), None);
        assert_eq!(parse_follow_up(r#"{"need": {"q": "tickers"}}"#), None);
        assert_eq!(parse_follow_up(""), None);
    }

    #[test]
    fn a_need_parses_as_the_query_to_run() {
        assert_eq!(
            parse_follow_up(r#"{"need": "engram retention ticker interval"}"#),
            Some("engram retention ticker interval".to_string())
        );
        // Models fence their JSON and preface it with prose no matter what the
        // prompt says, which is what `extract_json` is for.
        assert_eq!(
            parse_follow_up("Here you go:\n```json\n{\"need\": \"ticker interval\"}\n```"),
            Some("ticker interval".to_string())
        );
    }

    /// The schemas are sent to the endpoint to constrain decoding, so a schema
    /// that has drifted from its parser constrains the model into output the
    /// parser then rejects — a failure that looks exactly like a bad model.
    #[test]
    fn a_reply_that_satisfies_the_follow_up_schema_parses() {
        let schema = follow_up_schema();
        assert!(schema["properties"]["need"].is_object());
        assert_eq!(
            schema["properties"]["need"]["type"],
            serde_json::json!(["string", "null"]),
            "the grammar must be able to say `null`, which is the common answer"
        );
        assert!(parse_follow_up(r#"{"need":"x"}"#).is_some());
        assert!(parse_follow_up(r#"{"need":null}"#).is_none());
    }

    #[test]
    fn the_schema_no_longer_asks_for_tags() {
        // No domain-agnostic vocabulary exists for subject words, so a
        // generated one is a vocabulary that drifts: `forensics` and `forensik`
        // became two filters over one idea. The field survives for callers who
        // mean a particular tag; nothing writes it on their behalf.
        let items = &artifacts_schema()["properties"]["artifacts"]["items"];
        assert!(items["properties"]["tags"].is_null());
        let required: Vec<&str> = items["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(!required.contains(&"tags"), "{required:?}");
    }

    #[test]
    fn a_category_off_the_list_becomes_other() {
        // Subject words are what an unconstrained field collects: a corpus of
        // forensics notes filled it with "Forensic Science / Criminalistics",
        // which then appeared in the filter row beside "concept" as though it
        // were the same kind of choice. The field is about form, and the enum
        // is what keeps a domain out of the schema.
        assert_eq!(
            normalize_category("Forensic Science / Criminalistics"),
            "other"
        );
        assert_eq!(normalize_category("System Administration"), "other");
        assert_eq!(normalize_category(""), "other");
    }

    #[test]
    fn a_category_on_the_list_survives_case_and_padding() {
        assert_eq!(normalize_category("Procedure"), "procedure");
        assert_eq!(normalize_category("  snippet "), "snippet");
        assert_eq!(normalize_category("reference"), "reference");
    }

    #[test]
    fn the_schema_constrains_the_category_to_the_list() {
        let schema = artifacts_schema();
        let cat = &schema["properties"]["artifacts"]["items"]["properties"]["category"];
        let listed: Vec<&str> = cat["enum"]
            .as_array()
            .expect("the category is an enum, not a free string")
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(listed, CATEGORIES);
    }

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
        let variants = schema["properties"]["verdict"]["anyOf"]
            .as_array()
            .expect("a union of variants under the envelope");
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
                        "detail" => body["detail"] = serde_json::json!("because"),
                        "merged" => {
                            body["merged"] = serde_json::json!({
                                "text": "merged body",
                                "title": "t",
                                "category": "note",
                                "tags": [],
                                "caveats": []
                            })
                        }
                        // A single letter, which is all `parse_dedupe` reads as
                        // a direction; anything else downgrades to a conflict.
                        "supersedes" => body["supersedes"] = serde_json::json!("a"),
                        other => {
                            panic!("the schema requires {other:?}, which this test cannot build")
                        }
                    }
                }
                // Read through the envelope the schema actually asks for, which
                // is what a constrained endpoint will send.
                let sent = serde_json::json!({ "verdict": body });
                assert!(
                    parse_dedupe(&sent.to_string()).is_ok(),
                    "the schema lets the model answer {sent}, which the parser rejects"
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
        for variant in schema["properties"]["verdict"]["anyOf"].as_array().unwrap() {
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
        assert!(parse_dedupe(r#"{"verdict":{"relation":"duplicate"}}"#).is_err());
        assert!(
            parse_dedupe(r#"{"verdict":{"relation":"distinct","merged":{"text":"x"}}}"#).is_err()
        );
    }

    /// What actually makes a variant exclude a field, as opposed to declining to
    /// mention it. A JSON Schema object with no `additionalProperties: false`
    /// accepts every property it never named, so the `distinct`-with-a-merge
    /// reply above validates against the third variant and the union constrains
    /// nothing. llama.cpp's grammar generator closes objects implicitly and hid
    /// this; a validating gateway does not.
    ///
    /// And every property a strict schema names must also be required, which is
    /// not a style rule: a hosted API answers a strict schema with an optional
    /// property with a 400, so the judge would not be loosely constrained but
    /// permanently broken.
    #[test]
    fn every_judge_schema_object_is_closed_and_rooted_in_an_object() {
        fn closed(v: &serde_json::Value, path: &str) {
            if v["type"] == "object" {
                assert_eq!(
                    v["additionalProperties"],
                    serde_json::json!(false),
                    "{path} is an open object, so it accepts fields it never named"
                );
                let required: Vec<&str> = v["required"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|f| f.as_str())
                    .collect();
                for (k, sub) in v["properties"].as_object().into_iter().flatten() {
                    assert!(
                        required.contains(&k.as_str()),
                        "{path}.{k} is named but not required, which a strict schema refuses"
                    );
                    closed(sub, &format!("{path}.{k}"));
                }
            }
            // Into arrays too: the one array of objects in the set lives here,
            // and a walker that stops at `properties` never checks it — it would
            // pass this test while shipping exactly the open object the test
            // exists to catch.
            if v.get("items").is_some() {
                closed(&v["items"], &format!("{path}[]"));
            }
            for (i, sub) in v["anyOf"].as_array().into_iter().flatten().enumerate() {
                closed(sub, &format!("{path}.anyOf[{i}]"));
            }
        }

        for (name, schema) in [
            ("dedupe", dedupe_schema()),
            ("link", link_schema()),
            ("claims", claims_schema()),
            ("gap_label", gap_label_schema()),
            ("follow_up", follow_up_schema()),
            ("artifacts", artifacts_schema()),
        ] {
            // A strict `json_schema` response format needs an object at the
            // root: the hosted APIs reject a bare `anyOf` there outright, which
            // turns a loose constraint into a failed call.
            assert_eq!(
                schema["type"], "object",
                "the {name} schema is not rooted in an object"
            );
            closed(&schema, name);
        }
    }

    /// `parse_dedupe` reads a direction only as a single letter and downgrades
    /// anything else to a conflict — the outcome requiring `supersedes` exists
    /// to avoid. An unconstrained `{"type":"string"}` lets a grammar satisfy the
    /// field with "the second one" and land in exactly that case.
    #[test]
    fn the_supersedes_the_schema_permits_is_a_direction_the_parser_reads() {
        let schema = dedupe_schema();
        let replaced = schema["properties"]["verdict"]["anyOf"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["properties"]["relation"]["enum"][0] == "replaced")
            .expect("a variant for replaced");
        // Asserted literally rather than matched: the crate carries no regex
        // engine, and the point is that the field is anchored to exactly one
        // letter in either case — `dedupe_prompt` heads artifacts uppercase and
        // its example answers lowercase. Anything looser ("artifact B", "the
        // second one") satisfies the grammar and downgrades in the parser.
        assert_eq!(
            replaced["properties"]["supersedes"]["pattern"].as_str(),
            Some("^[A-Za-z]$"),
            "supersedes is not constrained to a direction parse_dedupe reads"
        );

        // And the parser agrees about which of those is a direction.
        let verdict =
            parse_dedupe(r#"{"verdict":{"relation":"replaced","supersedes":"B"}}"#).unwrap();
        assert_eq!(verdict.relation, Relation::Replaced);
        assert_eq!(verdict.supersedes, Some('b'));
    }

    /// The link judge shares an endpoint with the dedupe judge and used to share
    /// its response format, which no link verdict can satisfy: `related` and
    /// `unrelated` are not in the dedupe enum, and `reason` is not one of its
    /// properties.
    #[test]
    fn every_relation_the_link_schema_allows_is_one_its_parser_knows() {
        let schema = link_schema();
        let relations = schema["properties"]["verdict"]["properties"]["relation"]["enum"]
            .as_array()
            .unwrap();
        let names: Vec<&str> = relations.iter().map(|r| r.as_str().unwrap()).collect();
        assert_eq!(names, ["related", "unrelated", "duplicate"]);

        for r in &names {
            let sent = serde_json::json!({"verdict": {"relation": r, "reason": "because"}});
            let (_, reason) =
                crate::jobs::associate::parse_link(&sent.to_string()).unwrap_or_else(|e| {
                    panic!("the schema allows {sent}, which the parser rejects: {e}")
                });
            assert_eq!(reason, "because");
        }

        // No dedupe verdict is a link verdict, which is the whole point of the
        // two schemas being separate.
        for r in ["distinct", "conflict", "replaced"] {
            assert!(
                !names.contains(&r),
                "the link judge may answer {r:?}, which its parser refuses"
            );
        }
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
        // Offered by the model and not kept: a merge writes no tags of its own,
        // for the same reason synthesis does not.
        assert!(m.tags.is_empty());
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
        // The reply still carries tags and they are still dropped: the schema
        // stopped asking, and what a model volunteers anyway is the same
        // drifting vocabulary by another route.
        assert!(out[0].tags.is_empty());
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

    #[test]
    fn an_answer_that_opens_with_the_sentinel_is_an_abstention() {
        assert!(abstained(
            "Not in the knowledge base. Nothing covers mounting E01 images."
        ));
        assert!(abstained(
            "  not in the knowledge base — the excerpts are about FAT."
        ));
        // Models wrap the opening in emphasis or a heading; that is still the opening.
        assert!(abstained(
            "**Not in the knowledge base.** The excerpts describe…"
        ));
        assert!(abstained("# Not in the knowledge base\n\nThe excerpts…"));
        // A non-ASCII lead-in puts the prefix off a byte boundary; a list
        // marker is a lead-in the emphasis set does not cover.
        assert!(abstained("„Not in the knowledge base.“ Nothing covers it."));
        assert!(abstained(
            "• Not in the knowledge base — the excerpts are about FAT."
        ));
        assert!(abstained("- Not in the knowledge base."));
        assert!(abstained("1. Not in the knowledge base."));
        // The whole answer, quoted and nothing behind it: there is no answer for
        // the quotation to have been making a point about.
        assert!(abstained("\"Not in the knowledge base\""));
    }

    #[test]
    fn an_answer_that_quotes_the_phrase_to_argue_with_it_is_not_an_abstention() {
        // The cost of skipping an opening quote. Scored as an abstention, this
        // records a gap for a question the base did answer, and the gap sweep
        // then groups and names it.
        assert!(!abstained(
            "\"Not in the knowledge base\" is the wrong read here; excerpt 3 covers it."
        ));
        assert!(!abstained(
            "“Not in the knowledge base” would be wrong — see excerpt 1."
        ));
    }

    #[test]
    fn an_answer_that_merely_mentions_the_phrase_is_not_an_abstention() {
        assert!(!abstained(
            "Mount it with `ewfmount`. (Details on E02 are not in the knowledge base.)"
        ));
        assert!(!abstained(""));
        assert!(!abstained(
            "Not in the manual, but in the excerpts: use -o ro."
        ));
    }

    #[test]
    fn the_system_prompt_tells_the_model_the_exact_sentinel_the_code_reads() {
        assert!(ASK_SYSTEM.contains(ABSTAIN_PREFIX), "{ASK_SYSTEM}");
    }

    #[test]
    fn a_gap_label_is_read_out_of_the_envelope_and_tidied() {
        assert_eq!(
            parse_gap_label(r#"{"label": "\"Forensic image mounting.\""}"#).unwrap(),
            "Forensic image mounting"
        );
        assert!(parse_gap_label(r#"{"label": ""}"#).is_err());
        assert!(parse_gap_label("nope").is_err());
    }

    #[test]
    fn naming_a_gap_is_shown_a_bounded_number_of_bounded_questions() {
        // A cluster can hold every open gap of a kind. Unbounded, the prompt
        // goes over the context window, the call fails, the group falls back to
        // a terms label — and a terms label is offered to the model again, so
        // the failure is paid for on every sweep for as long as the group lives.
        let long = "x".repeat(GAP_LABEL_MAX_CHARS + 50);
        let mut qs: Vec<&str> = vec![long.as_str(); GAP_LABEL_MAX_QUESTIONS + 20];
        qs[0] = "the newest question, which the caller passes first";
        let p = gap_label_prompt(&qs);
        assert_eq!(
            p.lines().count(),
            GAP_LABEL_MAX_QUESTIONS + 1,
            "one header and {GAP_LABEL_MAX_QUESTIONS} questions: {p}"
        );
        assert!(
            p.contains("the newest question"),
            "the cap keeps the newest, which the caller passes first: {p}"
        );
        assert!(p.contains('…'), "an overlong question is cut: {p}");
        assert!(
            !p.contains(&long),
            "no question reaches the prompt at full length"
        );
    }
}
