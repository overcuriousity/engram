pub mod budget;
pub mod context;
pub mod decide;
pub mod fake;
pub mod gate;
pub mod lang;
#[cfg(feature = "contained")]
pub mod local;
pub mod openai;
#[cfg(feature = "contained")]
pub mod pcm;
pub mod prompt;
pub mod retry;
pub mod split;
pub mod verify;
#[cfg(feature = "contained")]
pub mod whisper;

use crate::error::Result;
use async_trait::async_trait;

#[derive(Debug, Clone, PartialEq)]
pub struct ProposedArtifact {
    pub text: String,
    pub title: Option<String>,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub corpus_lines: Option<(i64, i64)>,
    /// As the source states them; the model already holds the segment, so they cost tokens, not a call.
    pub caveats: Vec<String>,
    /// A decision or commitment the operator made. Honored only on the judged capture path; a promotion never pins.
    pub pinned: bool,
}

/// Id-addressable, so the reply's links can only name what was on the table.
#[derive(Debug, Clone)]
pub struct Neighbor {
    pub id: String,
    pub title: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct JudgeAsk {
    /// "2026-09-04 09:00 (Friday)" — local wall clock at capture.
    pub now_local: String,
    pub tz: String,
    /// A hint, not a bypass.
    pub forced_intent: Option<String>,
    pub neighbors: Vec<Neighbor>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProposedLink {
    pub artifact_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Judgement {
    /// "remind" | "none".
    pub intent: Option<String>,
    /// Local ISO-8601, no zone.
    pub when: Option<String>,
    /// An iCalendar RRULE.
    pub rule: Option<String>,
    /// Datetimes the note states without being the reminder.
    pub events: Vec<String>,
    pub links: Vec<ProposedLink>,
}

impl Judgement {
    /// A reply without artifacts fails unless this is true. `intent: "none"` is an answer, not a claim.
    pub fn says_something(&self) -> bool {
        self.intent.as_deref() == Some("remind")
            || !self.events.is_empty()
            || !self.links.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct SegmentReply {
    pub artifacts: Vec<ProposedArtifact>,
    pub judgement: Option<Judgement>,
}

#[derive(Debug, Clone, Copy)]
pub struct SynthesisBudget {
    pub context_tokens: usize,
    pub max_output_tokens: usize,
    pub output_ratio: f32,
    pub context: crate::infer::context::ContextBudget,
}

/// Window text and context are kept apart: span location and literal checks must know which is which.
pub struct SegmentInput<'a> {
    pub core: &'a str,
    pub context: &'a crate::infer::context::WindowContext,
    /// `Some` on the judged capture path, where the same call reads intent, events and links.
    pub judge: Option<&'a JudgeAsk>,
    /// Per call, not per synthesizer: it belongs to the document's corpus. See `infer::lang`.
    pub lang: crate::infer::lang::Lang,
}

#[async_trait]
pub trait Synthesizer: Send + Sync {
    /// Windowing itself is the caller's job.
    async fn segment(&self, input: SegmentInput<'_>) -> Result<Vec<ProposedArtifact>>;
    /// Defaulted so a test double with no opinion about moments needs no code for them.
    async fn segment_judged(&self, input: SegmentInput<'_>) -> Result<SegmentReply> {
        Ok(SegmentReply {
            artifacts: self.segment(input).await?,
            judgement: None,
        })
    }
    fn budget(&self) -> SynthesisBudget;
    /// `None` means this synthesizer does not name documents, and the corpus stays unnamed.
    async fn title(
        &self,
        _text: &str,
        _artifact_titles: &[String],
        _lang: crate::infer::lang::Lang,
    ) -> Result<Option<String>> {
        Ok(None)
    }
}

/// Built by `jobs::embed` from a `Chunk`.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbedDoc {
    pub title: Option<String>,
    pub text: String,
}

/// Query and document templates are applied inside the trait so no call site can forget them.
/// `embed_raw` is the wire call implementations supply; nothing outside an `Embedder` should call it.
#[async_trait]
pub trait Embedder: Send + Sync {
    async fn embed_raw(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    fn templates(&self) -> &crate::config::EmbedTemplates;
    fn dim(&self) -> usize;
    fn model(&self) -> &str;
    fn max_input_tokens(&self) -> usize;

    /// Exposed so budgets measure what is sent: the envelope costs tokens too.
    fn render_document(&self, doc: &EmbedDoc) -> String {
        self.templates()
            .render_document(doc.title.as_deref(), &doc.text)
    }

    async fn embed_documents(&self, docs: &[EmbedDoc]) -> Result<Vec<Vec<f32>>> {
        let texts: Vec<String> = docs.iter().map(|d| self.render_document(d)).collect();
        self.embed_raw(&texts).await
    }

    async fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        let mut out = self
            .embed_raw(&[self.templates().render_query(query)])
            .await?;
        if out.is_empty() {
            return Err(crate::error::Error::Inference {
                role: "embed",
                detail: "no vector came back for the query".into(),
            });
        }
        Ok(out.remove(0))
    }
}

#[async_trait]
pub trait Reranker: Send + Sync {
    /// (original index, score) pairs, best first, at most `top_n`.
    async fn rerank(&self, query: &str, docs: &[String], top_n: usize)
    -> Result<Vec<(usize, f32)>>;
}

/// Reasoning stays apart from the answer: the literal check and citation parser never read it.
#[derive(Debug, Clone)]
pub enum Delta {
    Token(String),
    Reasoning(String),
}

pub struct Completion {
    pub text: String,
    /// The ceiling ended the reply; what that costs is the caller's to decide.
    pub truncated: bool,
}

#[async_trait]
pub trait Completer: Send + Sync {
    async fn complete(&self, system: &str, user: &str) -> Result<String>;

    /// `ceiling` is a maximum the implementation clamps to its own; a packed prompt knows the largest that fits.
    async fn answer(&self, system: &str, user: &str, _ceiling: usize) -> Result<Completion> {
        Ok(Completion {
            text: self.complete(system, user).await?,
            truncated: false,
        })
    }

    /// The returned `Completion` is authoritative, so a dropped receiver cannot truncate a stored answer.
    async fn answer_streaming(
        &self,
        system: &str,
        user: &str,
        ceiling: usize,
        sink: tokio::sync::mpsc::Sender<Delta>,
    ) -> Result<Completion> {
        let c = self.answer(system, user, ceiling).await?;
        // Unchecked on purpose: the call still finishes so the endpoint is left clean and the pacer sees it end.
        let _ = sink.send(Delta::Token(c.text.clone())).await;
        Ok(c)
    }

    fn context_tokens(&self) -> usize;
    /// The endpoint counts prompt plus ceiling against the context; reserving less turns a fit into a 400.
    fn max_output_tokens(&self) -> usize;
}

/// Nothing is stored: the words are typed into the box.
#[async_trait]
pub trait Transcriber: Send + Sync {
    /// The recorder's own type (`audio/webm`, Safari `audio/mp4`), passed through: the endpoint decodes containers.
    async fn transcribe(&self, audio: &[u8], mime: &str) -> Result<String>;
}

/// The caller has already decoded, oriented and downscaled the picture into a JPEG.
#[async_trait]
pub trait Describer: Send + Sync {
    /// `context` is the user's note and when and where it was taken. Markdown comes back.
    async fn describe(&self, image_jpeg: &[u8], context: &str) -> Result<String>;
}
