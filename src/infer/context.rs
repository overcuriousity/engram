use crate::infer::budget::TokenCounter;
use crate::infer::split::segment_text;

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

/// The material surrounding one window: what document it belongs to, and what
/// its neighbours say on either side of it.
///
/// Derived, never stored. A retry rebuilds it byte-for-byte from the stored
/// line numbers, which is what keeps a resumed job idempotent.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WindowContext {
    /// The document's verbatim first lines. Absent for the window that already
    /// contains them.
    pub opening: Option<String>,
    /// The tail of the previous window.
    pub before: Option<String>,
    /// The head of the next window.
    pub after: Option<String>,
}

impl WindowContext {
    pub fn build(
        raw_text: &str,
        spans: &[(i64, i64)],
        idx: usize,
        budget: ContextBudget,
        counter: &TokenCounter,
    ) -> WindowContext {
        let Some(&(start, _)) = spans.get(idx) else {
            return WindowContext::default();
        };

        // Window 0 opens with the document, so repeating it would spend the
        // budget on text the model is already reading. Later windows take the
        // opening only up to their own first line: an opening that ran into
        // the window would put the same lines in the prompt twice.
        let opening = (idx > 0 && budget.opening > 0 && start > 1)
            .then(|| segment_text(raw_text, 1, start - 1))
            .and_then(|t| head_lines(&t, budget.opening, counter));

        let before = (budget.overlap > 0)
            .then(|| idx.checked_sub(1))
            .flatten()
            .and_then(|i| spans.get(i))
            .map(|&(s, e)| segment_text(raw_text, s, e))
            .and_then(|t| tail_lines(&t, budget.overlap, counter));

        let after = (budget.overlap > 0)
            .then(|| spans.get(idx + 1))
            .flatten()
            .map(|&(s, e)| segment_text(raw_text, s, e))
            .and_then(|t| head_lines(&t, budget.overlap, counter));

        WindowContext {
            opening,
            before,
            after,
        }
    }

    /// Every block that ended up populated. Callers use this to ask whether a
    /// piece of text came from context rather than from the window itself.
    pub fn blocks(&self) -> impl Iterator<Item = &str> {
        [
            self.opening.as_deref(),
            self.before.as_deref(),
            self.after.as_deref(),
        ]
        .into_iter()
        .flatten()
    }

    pub fn is_empty(&self) -> bool {
        self.blocks().next().is_none()
    }
}

/// As many leading whole lines as fit. Whole lines because a context block cut
/// mid-sentence reads as corruption to a small model.
fn head_lines(text: &str, limit: usize, counter: &TokenCounter) -> Option<String> {
    let taken = take_lines(text.lines(), limit, counter);
    (!taken.is_empty()).then(|| taken.join("\n"))
}

/// As many trailing whole lines as fit, in their original order.
fn tail_lines(text: &str, limit: usize, counter: &TokenCounter) -> Option<String> {
    let mut taken = take_lines(text.lines().rev(), limit, counter);
    taken.reverse();
    (!taken.is_empty()).then(|| taken.join("\n"))
}

fn take_lines<'a>(
    lines: impl Iterator<Item = &'a str>,
    limit: usize,
    counter: &TokenCounter,
) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut used = 0usize;
    for line in lines {
        let n = counter.count(line) + 1; // +1 for the newline
        if used + n > limit {
            break;
        }
        out.push(line);
        used += n;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus() -> String {
        let mut lines = vec![
            "# Backup Server Admin Guide".to_string(),
            "Covers PBS 3.x on Debian 12.".into(),
        ];
        for i in 0..60 {
            lines.push(format!("body line {i} with enough words to cost tokens"));
        }
        lines.join("\n")
    }

    fn spans() -> Vec<(i64, i64)> {
        vec![(1, 20), (21, 40), (41, 62)]
    }

    fn budget() -> ContextBudget {
        ContextBudget {
            opening: 30,
            overlap: 20,
        }
    }

    #[test]
    fn the_first_window_gets_no_opening_and_no_preceding_context() {
        let c = WindowContext::build(&corpus(), &spans(), 0, budget(), &TokenCounter::Estimate);
        assert_eq!(c.opening, None, "window 0 already contains the opening");
        assert_eq!(c.before, None, "window 0 has nothing before it");
        assert!(c.after.is_some(), "window 0 has a window after it");
    }

    #[test]
    fn the_last_window_gets_no_following_context() {
        let c = WindowContext::build(&corpus(), &spans(), 2, budget(), &TokenCounter::Estimate);
        assert_eq!(c.after, None);
        assert!(c.before.is_some());
        assert!(c.opening.is_some());
    }

    #[test]
    fn a_middle_window_gets_all_three_blocks_from_the_right_places() {
        let text = corpus();
        let c = WindowContext::build(&text, &spans(), 1, budget(), &TokenCounter::Estimate);

        assert!(
            c.opening
                .as_deref()
                .unwrap()
                .starts_with("# Backup Server Admin Guide"),
            "the opening must be the document's first lines verbatim"
        );
        // The preceding block is the tail of window 0, which ends at line 20.
        assert!(
            c.before.as_deref().unwrap().contains("body line 17"),
            "got {:?}",
            c.before
        );
        // The following block is the head of window 2, which starts at line 41.
        assert!(
            c.after.as_deref().unwrap().contains("body line 38"),
            "got {:?}",
            c.after
        );
    }

    #[test]
    fn every_block_stays_inside_its_budget() {
        let counter = TokenCounter::Estimate;
        let c = WindowContext::build(&corpus(), &spans(), 1, budget(), &counter);
        assert!(counter.count(c.opening.as_deref().unwrap()) <= 30);
        assert!(counter.count(c.before.as_deref().unwrap()) <= 20);
        assert!(counter.count(c.after.as_deref().unwrap()) <= 20);
    }

    #[test]
    fn the_opening_never_runs_into_the_window_it_introduces() {
        // Window 1 starts at line 2, so an opening of 30 tokens would cover
        // lines the window already holds. It must be cut at line 1.
        let text = corpus();
        let spans = vec![(1, 1), (2, 30), (31, 62)];
        let c = WindowContext::build(&text, &spans, 1, budget(), &TokenCounter::Estimate);
        let opening = c.opening.as_deref().unwrap();
        assert!(
            !opening.contains("Covers PBS 3.x"),
            "line 2 belongs to the window itself: {opening:?}"
        );
    }

    #[test]
    fn a_zero_budget_produces_nothing() {
        let c = WindowContext::build(
            &corpus(),
            &spans(),
            1,
            ContextBudget::default(),
            &TokenCounter::Estimate,
        );
        assert_eq!(c, WindowContext::default());
        assert!(c.is_empty());
        assert_eq!(c.blocks().count(), 0);
    }

    #[test]
    fn assembly_is_reproducible() {
        // A retry rebuilds context from stored line numbers alone, so the same
        // spans must always give the same bytes.
        let text = corpus();
        let a = WindowContext::build(&text, &spans(), 1, budget(), &TokenCounter::Estimate);
        let b = WindowContext::build(&text, &spans(), 1, budget(), &TokenCounter::Estimate);
        assert_eq!(a, b);
    }

    #[test]
    fn blocks_yields_every_populated_block() {
        let c = WindowContext::build(&corpus(), &spans(), 1, budget(), &TokenCounter::Estimate);
        assert_eq!(c.blocks().count(), 3);
        let c0 = WindowContext::build(&corpus(), &spans(), 0, budget(), &TokenCounter::Estimate);
        assert_eq!(c0.blocks().count(), 1);
    }
}
