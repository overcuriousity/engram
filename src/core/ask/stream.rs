//! What one ask emits while it happens.

use crate::core::search::SearchResult;

/// One step of an ask; its single producer keeps streaming and blocking asks identical.
#[derive(Debug, Clone)]
pub enum AskEvent {
    /// Retrieval finished: round 1 is the question as asked, round 2 every planned round packed
    /// once.
    Retrieved {
        round: u8,
        /// Artifacts ranked across the rounds, beside `shown`, so a wide net does not read as
        /// failures.
        retrieved: usize,
        shown: usize,
        dropped: usize,
        cliff_at: Option<usize>,
    },
    /// Round 2 only, never empty.
    Needs(Vec<String>),
    /// Emitted once, after the final retrieval and before the first token.
    Citations(Vec<SearchResult>),
    Reasoning(String),
    Token(String),
    /// Terminal, and carries exactly what the blocking door returns.
    Done(Box<super::AskResponse>),
}
