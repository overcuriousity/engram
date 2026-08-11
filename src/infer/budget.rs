use super::SynthesisBudget;

pub const MIN_SEGMENT_TOKENS: usize = 256;

pub enum TokenCounter {
    Exact(Box<tokenizers::Tokenizer>),
    Estimate,
}

impl TokenCounter {
    pub fn load(path: Option<&str>) -> TokenCounter {
        match path {
            Some(p) => match tokenizers::Tokenizer::from_file(p) {
                Ok(t) => {
                    tracing::info!(tokenizer = p, "exact token counting enabled");
                    TokenCounter::Exact(Box::new(t))
                }
                Err(e) => {
                    tracing::warn!(
                        tokenizer = p,
                        error = %e,
                        "tokenizer failed to load; falling back to character estimate"
                    );
                    TokenCounter::Estimate
                }
            },
            None => {
                tracing::info!(
                    "no tokenizer configured; using character estimate for token budgets"
                );
                TokenCounter::Estimate
            }
        }
    }

    pub fn count(&self, text: &str) -> usize {
        match self {
            TokenCounter::Exact(t) => t
                .encode(text, false)
                .map(|e| e.len())
                .unwrap_or_else(|_| estimate(text)),
            TokenCounter::Estimate => estimate(text),
        }
    }
}

fn estimate(text: &str) -> usize {
    text.chars().count() * 2 / 7
}

pub fn segment_tokens(budget: SynthesisBudget, prompt_overhead: usize) -> usize {
    let ratio = budget.output_ratio.max(0.1);
    let usable = budget.context_tokens.saturating_sub(prompt_overhead);
    let by_context = (usable as f32 / (1.0 + ratio)) as usize;
    let by_output = (budget.max_output_tokens as f32 / ratio) as usize;
    by_context.min(by_output).max(MIN_SEGMENT_TOKENS)
}

pub fn pack_by_budget(items: &[String], counter: &TokenCounter, budget: usize) -> usize {
    let mut used = 0usize;
    for (i, item) in items.iter().enumerate() {
        let n = counter.count(item);
        if used + n > budget {
            return i;
        }
        used += n;
    }
    items.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infer::SynthesisBudget;

    fn budget(ctx: usize, max_out: usize, ratio: f32) -> SynthesisBudget {
        SynthesisBudget {
            context_tokens: ctx,
            max_output_tokens: max_out,
            output_ratio: ratio,
        }
    }

    #[test]
    fn window_leaves_room_for_a_larger_rewritten_output() {
        let w = segment_tokens(budget(32768, 100_000, 1.4), 1000);
        assert_eq!(w, 13236);
        assert!(w < 32768 / 2, "window must not assume output is free");
    }

    #[test]
    fn window_is_also_clamped_by_max_output_tokens() {
        let w = segment_tokens(budget(32768, 8192, 1.4), 1000);
        assert_eq!(w, 5851);
    }

    #[test]
    fn window_never_returns_zero_or_underflows() {
        let w = segment_tokens(budget(1000, 8192, 1.4), 5000);
        assert!(w >= MIN_SEGMENT_TOKENS, "got {w}");
    }

    #[test]
    fn estimate_counter_is_conservative() {
        let c = TokenCounter::Estimate;
        assert_eq!(c.count(&"a".repeat(35)), 10);
        assert_eq!(c.count(""), 0);
    }

    #[test]
    fn estimate_counts_characters_not_bytes() {
        let c = TokenCounter::Estimate;
        assert_eq!(c.count(&"ä".repeat(35)), 10);
    }

    #[test]
    fn pack_stops_at_the_budget() {
        let c = TokenCounter::Estimate;
        let items: Vec<String> = (0..5).map(|_| "x".repeat(35)).collect();
        assert_eq!(pack_by_budget(&items, &c, 25), 2);
        assert_eq!(pack_by_budget(&items, &c, 1000), 5);
        assert_eq!(pack_by_budget(&items, &c, 0), 0);
    }
}
