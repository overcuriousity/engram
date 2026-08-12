/// How many tokens of surrounding material a window may carry on top of its
/// own text. Absolute rather than a fraction of the window: a large context
/// should make context free, not proportionally expensive.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ContextBudget {
    /// Tokens of the document's verbatim opening.
    pub opening: usize,
    /// Tokens of each neighbouring window, on both sides.
    pub overlap: usize,
}

/// The fence lines and their labels, which are prompt text like any other and
/// have to be paid for out of the same budget.
const FENCE_TOKENS: usize = 40;

impl ContextBudget {
    /// Everything the context blocks cost, fences included. This is subtracted
    /// from the window so the assembled prompt still fits the model.
    pub fn total(&self) -> usize {
        if self.opening == 0 && self.overlap == 0 {
            return 0;
        }
        self.opening + 2 * self.overlap + FENCE_TOKENS
    }
}
