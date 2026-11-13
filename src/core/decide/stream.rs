//! What one decision emits while it happens.

use crate::infer::decide::Answer;

/// One passage's single read.
#[derive(Debug, Clone, PartialEq)]
pub struct PassageRead {
    /// Retrieval order, from 1.
    pub rank: usize,
    pub of: usize,
    pub artifact_id: String,
    pub title: Option<String>,
    pub bears: f64,
    pub bearing: bool,
    /// The caller's questions, in their order; `bears` is not among them.
    pub read: Vec<(String, Answer)>,
}

/// One step of a decision; its single producer keeps streaming and blocking decisions identical.
#[derive(Debug, Clone)]
pub enum DecideEvent {
    /// Always first, even when nothing was found.
    Retrieved { passages: usize },
    /// One per passage, as each read arrives.
    Read(Box<PassageRead>),
    /// Terminal, and exactly what the blocking door returns.
    Done(Box<super::Decision>),
}
