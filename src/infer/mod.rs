pub mod budget;
pub mod context;
pub mod facts;
pub mod fake;
pub mod gate;
pub mod openai;
pub mod prompt;
pub mod split;
pub mod verify;

use crate::error::Result;
use async_trait::async_trait;

#[derive(Debug, Clone, PartialEq)]
pub struct ProposedArtifact {
    pub text: String,
    pub title: Option<String>,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub corpus_lines: Option<(i64, i64)>,
    /// Conditions under which the artifact does not apply, as the source states
    /// them. The model is already holding this segment, so asking for these
    /// costs output tokens rather than another call.
    pub caveats: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct SynthesisBudget {
    pub context_tokens: usize,
    pub max_output_tokens: usize,
    pub output_ratio: f32,
    /// What the window gives up so each call can carry the document's opening
    /// and its neighbours' edges.
    pub context: crate::infer::context::ContextBudget,
}

/// One window as the synthesizer sees it: the text artifacts are drawn from,
/// and the surrounding material that exists only so references can be
/// resolved. They are separate fields rather than one assembled string
/// because everything downstream — span location, literal checking,
/// paraphrase detection — has to be told which is which.
pub struct SegmentInput<'a> {
    pub core: &'a str,
    pub context: &'a crate::infer::context::WindowContext,
}

#[async_trait]
pub trait Synthesizer: Send + Sync {
    /// Segment one window of text. Windowing itself is the caller's job.
    async fn segment(&self, input: SegmentInput<'_>) -> Result<Vec<ProposedArtifact>>;
    fn budget(&self) -> SynthesisBudget;
    /// A short name for a whole document, given its opening and the titles of
    /// the artifacts drawn from it.
    ///
    /// `None` means this synthesizer does not name documents, and the caller
    /// leaves the corpus unnamed rather than inventing a name for it. Defaulted
    /// rather than required because most implementations of this trait are test
    /// doubles that have no opinion about titles.
    async fn title(&self, _text: &str, _artifact_titles: &[String]) -> Result<Option<String>> {
        Ok(None)
    }
}

#[async_trait]
pub trait Embedder: Send + Sync {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    fn dim(&self) -> usize;
    fn model(&self) -> &str;
    fn max_input_tokens(&self) -> usize;
}

#[async_trait]
pub trait Reranker: Send + Sync {
    /// Returns (original index, score) pairs, best first, at most `top_n`.
    async fn rerank(&self, query: &str, docs: &[String], top_n: usize)
    -> Result<Vec<(usize, f32)>>;
}

/// One piece of a streamed completion.
///
/// Reasoning is kept apart from the answer rather than concatenated: the page
/// shows it dimmed and above, and it is never part of what the literal check
/// or the citation parser reads.
#[derive(Debug, Clone)]
pub enum Delta {
    Token(String),
    Reasoning(String),
}

/// A completion, and whether the ceiling is what ended it.
pub struct Completion {
    pub text: String,
    /// The model did not stop; it ran out of room. What that costs is the
    /// caller's to decide — an answer says so, a salvageable artifact list does
    /// not need to.
    pub truncated: bool,
}

#[async_trait]
pub trait Completer: Send + Sync {
    async fn complete(&self, system: &str, user: &str) -> Result<String>;

    /// `complete`, with the ceiling this one call may spend and a reply that
    /// says whether it was hit.
    ///
    /// A caller packing a prompt against `context_tokens` knows what the prompt
    /// actually cost, and therefore knows the largest ceiling that still fits
    /// the window — which `max_output_tokens` alone cannot express, since a
    /// role whose ceiling is most of its context leaves nothing to pack and
    /// answers nothing at all. The ceiling asked for here is a maximum, never a
    /// minimum: an implementation clamps it to its own.
    ///
    /// Defaults to `complete`, because a completer that sends no ceiling of its
    /// own has nothing to cap and nothing to report.
    async fn answer(&self, system: &str, user: &str, _ceiling: usize) -> Result<Completion> {
        Ok(Completion {
            text: self.complete(system, user).await?,
            truncated: false,
        })
    }

    /// `answer`, delivering the text as it arrives.
    ///
    /// Defaults to `answer` followed by one delta, so an implementor that
    /// cannot stream — every fake in the test suite, and any endpoint without
    /// SSE — still satisfies the streaming caller without a hand-written
    /// override. Only `HttpCompleter` overrides this.
    ///
    /// The returned `Completion` is authoritative: a caller assembles its
    /// answer from it, not from the deltas it accumulated, so a dropped
    /// receiver can never silently truncate a stored answer.
    async fn answer_streaming(
        &self,
        system: &str,
        user: &str,
        ceiling: usize,
        sink: tokio::sync::mpsc::Sender<Delta>,
    ) -> Result<Completion> {
        let c = self.answer(system, user, ceiling).await?;
        // Unchecked on purpose: a receiver that went away is a reader that
        // stopped reading, and the call is finished rather than abandoned so the
        // endpoint is left clean and whatever paces the GPU sees the call end
        // when it actually ends. The result then goes nowhere — the only thing
        // that records an answer is the caller that dropped the receiver.
        let _ = sink.send(Delta::Token(c.text.clone())).await;
        Ok(c)
    }

    fn context_tokens(&self) -> usize;
    /// The largest ceiling this completer will send on a call.
    ///
    /// Exposed because a caller that packs a prompt against `context_tokens`
    /// has to leave the reply room in the same window: the endpoint counts
    /// prompt plus ceiling against the context, and refuses the request when
    /// the two together exceed it. Reserving less than this is how a request
    /// that would have fit becomes a 400.
    fn max_output_tokens(&self) -> usize;
}

/// Reads a captured image into text. One call per image; the caller has
/// already decoded, oriented and downscaled the picture into a JPEG.
#[async_trait]
pub trait Describer: Send + Sync {
    /// `context` is what is known about the capture beyond its pixels — the
    /// user's note, when and where it was taken. Markdown comes back.
    async fn describe(&self, image_jpeg: &[u8], context: &str) -> Result<String>;
}
