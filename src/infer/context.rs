use crate::infer::budget::TokenCounter;

/// Absolute rather than a fraction of the window, so a large context makes context free.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ContextBudget {
    pub opening: usize,
    /// On both sides.
    pub overlap: usize,
    /// For a judged capture call.
    pub neighbors: usize,
}

/// Counted from `prompt::synthesis_prompt`'s fences, all four block kinds; 60 was a guess and too short.
const FENCE_TOKENS: usize = 160;

/// `[id: <uuid>] ` plus newlines; a UUID is a couple of dozen tokens. The title is counted separately.
pub const NEIGHBOR_HEADER_TOKENS: usize = 28;

impl ContextBudget {
    /// Subtracted from the window so the assembled prompt still fits the model.
    pub fn total(&self) -> usize {
        if self.opening == 0 && self.overlap == 0 && self.neighbors == 0 {
            return 0;
        }
        self.opening + 2 * self.overlap + self.neighbors + FENCE_TOKENS
    }
}

/// Derived, never stored: a retry rebuilds it byte-for-byte from stored lines, keeping resumed jobs idempotent.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WindowContext {
    /// Absent for the window that already contains them.
    pub opening: Option<String>,
    pub before: Option<String>,
    pub after: Option<String>,
}

impl WindowContext {
    /// `windows` is the stored splitter output: line ranges cannot reproduce text cut inside a line.
    pub fn build(
        windows: &[&str],
        idx: usize,
        budget: ContextBudget,
        counter: &TokenCounter,
    ) -> WindowContext {
        if idx >= windows.len() {
            return WindowContext::default();
        }

        // Window 0 already opens with the document.
        let opening = (idx > 0 && budget.opening > 0)
            .then(|| windows[0])
            .and_then(|t| head_lines(t, budget.opening, counter));

        let before = (budget.overlap > 0)
            .then(|| idx.checked_sub(1))
            .flatten()
            .and_then(|i| windows.get(i))
            .and_then(|t| tail_lines(t, budget.overlap, counter));

        let after = (budget.overlap > 0)
            .then(|| windows.get(idx + 1))
            .flatten()
            .and_then(|t| head_lines(t, budget.overlap, counter));

        WindowContext {
            opening,
            before,
            after,
        }
    }

    /// Lets callers ask whether text came from context rather than the window.
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

/// Whole lines: a block cut mid-sentence reads as corruption to a small model.
fn head_lines(text: &str, limit: usize, counter: &TokenCounter) -> Option<String> {
    let taken = take_lines(text.lines(), limit, counter);
    if !taken.is_empty() {
        return Some(taken.join("\n"));
    }
    // A paste with no line breaks fits no whole line; cut on characters rather than drop the context.
    counter.cut(text, limit, true)
}

/// In their original order.
fn tail_lines(text: &str, limit: usize, counter: &TokenCounter) -> Option<String> {
    let mut taken = take_lines(text.lines().rev(), limit, counter);
    taken.reverse();
    if !taken.is_empty() {
        return Some(taken.join("\n"));
    }
    counter.cut(text, limit, false)
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

    fn windows() -> Vec<String> {
        let body = |from: usize, to: usize| {
            (from..to)
                .map(|i| format!("body line {i} with enough words to cost tokens"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        vec![
            format!(
                "# Backup Server Admin Guide\nCovers PBS 3.x on Debian 12.\n{}",
                body(0, 18)
            ),
            body(18, 38),
            body(38, 60),
        ]
    }

    fn refs(w: &[String]) -> Vec<&str> {
        w.iter().map(|s| s.as_str()).collect()
    }

    fn budget() -> ContextBudget {
        ContextBudget {
            opening: 30,
            overlap: 20,
            neighbors: 0,
        }
    }

    #[test]
    fn the_first_window_gets_no_opening_and_no_preceding_context() {
        let w = windows();
        let c = WindowContext::build(&refs(&w), 0, budget(), &TokenCounter::default());
        assert_eq!(c.opening, None, "window 0 already contains the opening");
        assert_eq!(c.before, None, "window 0 has nothing before it");
        assert!(c.after.is_some(), "window 0 has a window after it");
    }

    #[test]
    fn the_last_window_gets_no_following_context() {
        let w = windows();
        let c = WindowContext::build(&refs(&w), 2, budget(), &TokenCounter::default());
        assert_eq!(c.after, None);
        assert!(c.before.is_some());
        assert!(c.opening.is_some());
    }

    #[test]
    fn a_middle_window_gets_all_three_blocks_from_the_right_places() {
        let w = windows();
        let c = WindowContext::build(&refs(&w), 1, budget(), &TokenCounter::default());

        assert!(
            c.opening
                .as_deref()
                .unwrap()
                .starts_with("# Backup Server Admin Guide"),
            "the opening must be the document's first lines verbatim"
        );
        assert!(
            c.before.as_deref().unwrap().contains("body line 17"),
            "got {:?}",
            c.before
        );
        assert!(
            c.after.as_deref().unwrap().contains("body line 38"),
            "got {:?}",
            c.after
        );
    }

    #[test]
    fn every_block_stays_inside_its_budget() {
        let counter = TokenCounter::default();
        let w = windows();
        let c = WindowContext::build(&refs(&w), 1, budget(), &counter);
        assert!(counter.count(c.opening.as_deref().unwrap()) <= 30);
        assert!(counter.count(c.before.as_deref().unwrap()) <= 20);
        assert!(counter.count(c.after.as_deref().unwrap()) <= 20);
    }

    #[test]
    fn a_corpus_with_no_line_structure_still_gets_context() {
        // Line numbers cannot address less than a line, so a newline-free corpus needs the stored text.
        let w: Vec<String> = (0..4).map(|i| format!("part{i} ").repeat(40)).collect();
        let c = WindowContext::build(&refs(&w), 2, budget(), &TokenCounter::default());
        assert!(
            c.before.as_deref().unwrap().contains("part1"),
            "got {:?}",
            c.before
        );
        assert!(
            c.after.as_deref().unwrap().contains("part3"),
            "got {:?}",
            c.after
        );
        assert!(c.opening.as_deref().unwrap().contains("part0"));
    }

    #[test]
    fn a_zero_budget_produces_nothing() {
        let w = windows();
        let c = WindowContext::build(
            &refs(&w),
            1,
            ContextBudget::default(),
            &TokenCounter::default(),
        );
        assert_eq!(c, WindowContext::default());
        assert!(c.is_empty());
        assert_eq!(c.blocks().count(), 0);
    }

    #[test]
    fn an_index_past_the_end_produces_nothing() {
        let w = windows();
        let c = WindowContext::build(&refs(&w), 9, budget(), &TokenCounter::default());
        assert_eq!(c, WindowContext::default());
    }

    #[test]
    fn assembly_is_reproducible() {
        let w = windows();
        let a = WindowContext::build(&refs(&w), 1, budget(), &TokenCounter::default());
        let b = WindowContext::build(&refs(&w), 1, budget(), &TokenCounter::default());
        assert_eq!(a, b);
    }

    #[test]
    fn blocks_yields_every_populated_block() {
        let w = windows();
        assert_eq!(
            WindowContext::build(&refs(&w), 1, budget(), &TokenCounter::default())
                .blocks()
                .count(),
            3
        );
        assert_eq!(
            WindowContext::build(&refs(&w), 0, budget(), &TokenCounter::default())
                .blocks()
                .count(),
            1
        );
    }
}
