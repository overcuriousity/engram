//! What one ask emits while it happens.

use crate::core::search::SearchResult;

/// One step of an ask, in the order it occurs.
///
/// The page renders these; `Core::ask` collects them back into an
/// `AskResponse`. Having exactly one producer is what keeps the streaming and
/// blocking doors describing the same ask.
#[derive(Debug, Clone)]
pub enum AskEvent {
    /// Retrieval finished. `round` is 1 for the question as it was asked, or 2
    /// for the fanned-out rounds the plan named — however many of those there
    /// were, they are packed once and reported once, because what the reader is
    /// owed is what the model ends up seeing and there is one such list.
    Retrieved {
        round: u8,
        /// How many artifacts the ranking returned across every round folded
        /// into this one, cliff and all. Reported beside `shown` because the
        /// pair is what the reader can act on: a wide search that showed a
        /// handful is the fan-out working, and `dropped` alone reads as a
        /// count of failures when it is a count of a bigger net.
        retrieved: usize,
        shown: usize,
        dropped: usize,
        cliff_at: Option<usize>,
    },
    /// The subjects the model said were still missing, as the queries it named
    /// for them. Round 2 only, and never empty — a plan with nothing in it is
    /// not a round that happened.
    Needs(Vec<String>),
    /// The excerpts the model will see. Emitted once, after the final
    /// retrieval and before the first token, so the rail is readable while the
    /// answer is still being written.
    Citations(Vec<SearchResult>),
    Reasoning(String),
    Token(String),
    /// Terminal, and carries exactly what the blocking door returns.
    Done(Box<super::AskResponse>),
}
