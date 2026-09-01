use crate::infer::budget::TokenCounter;

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
    /// `windows` is every window of the corpus, in order, as the splitter
    /// produced them — the stored text, not a re-derivation from line numbers.
    /// A window's line range cannot reproduce its text when the splitter had to
    /// cut inside a line, and that is precisely the case this has to survive.
    pub fn build(
        windows: &[&str],
        idx: usize,
        budget: ContextBudget,
        counter: &TokenCounter,
    ) -> WindowContext {
        if idx >= windows.len() {
            return WindowContext::default();
        }

        // Window 0 opens with the document, so repeating it there would spend
        // the budget on text the model is already reading.
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
    if !taken.is_empty() {
        return Some(taken.join("\n"));
    }
    cut_chars(text, limit, true)
}

/// As many trailing whole lines as fit, in their original order.
fn tail_lines(text: &str, limit: usize, counter: &TokenCounter) -> Option<String> {
    let mut taken = take_lines(text.lines().rev(), limit, counter);
    taken.reverse();
    if !taken.is_empty() {
        return Some(taken.join("\n"));
    }
    cut_chars(text, limit, false)
}

/// Not one whole line fits. That is not a pathological case here — it is the
/// corpus this whole mechanism exists for, pasted with no line boundaries at
/// all — and no context is worse than context cut mid-word, so the budget is
/// spent on characters. 3.5 per token is the ratio the estimate uses.
fn cut_chars(text: &str, limit: usize, from_start: bool) -> Option<String> {
    if limit == 0 || text.is_empty() {
        return None;
    }
    let max_chars = (limit * 7 / 2).max(16);
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars {
        return Some(text.to_string());
    }
    Some(if from_start {
        chars[..max_chars].iter().collect()
    } else {
        chars[chars.len() - max_chars..].iter().collect()
    })
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

    /// Three windows as the splitter would hand them over: the first opens the
    /// document, the rest are body.
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
        // The preceding block is the tail of window 0, which ends at line 17.
        assert!(
            c.before.as_deref().unwrap().contains("body line 17"),
            "got {:?}",
            c.before
        );
        // The following block is the head of window 2, which starts at line 38.
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
        // The case the stored window text exists for. Line numbers cannot
        // address a unit smaller than a line, so a corpus pasted with no
        // newlines used to re-derive as the whole document for window 0 and as
        // nothing at all for every window after it.
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
        let c = WindowContext::build(&refs(&w), 1, ContextBudget::default(), &TokenCounter::default());
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
        // A retry rebuilds context from the stored windows alone, so the same
        // rows must always give the same bytes.
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
