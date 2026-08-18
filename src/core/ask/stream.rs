//! What one ask emits while it happens.

use crate::core::search::SearchResult;

/// One step of an ask, in the order it occurs.
///
/// The page renders these; `Core::ask` collects them back into an
/// `AskResponse`. Having exactly one producer is what keeps the streaming and
/// blocking doors describing the same ask.
#[derive(Debug, Clone)]
pub enum AskEvent {
    /// A retrieval round finished. `round` is 1, or 2 for the follow-up.
    Retrieved {
        round: u8,
        shown: usize,
        dropped: usize,
        cliff_at: Option<usize>,
    },
    /// What the model said it still needed. Round 2 only.
    Needs(String),
    /// The excerpts the model will see. Emitted once, after the final
    /// retrieval and before the first token, so the rail is readable while the
    /// answer is still being written.
    Citations(Vec<SearchResult>),
    Reasoning(String),
    Token(String),
    /// Terminal, and carries exactly what the blocking door returns.
    Done(Box<super::AskResponse>),
}
