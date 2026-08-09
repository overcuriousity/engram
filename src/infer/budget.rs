use super::ChunkBudget;

/// Below this a window cannot hold a useful unit of text, so a
/// misconfigured context or an oversized prompt fails loudly at the call site
/// rather than producing single-word chunks.
pub const MIN_WINDOW_TOKENS: usize = 256;

pub enum TokenCounter {
    Exact(Box<tokenizers::Tokenizer>),
    /// Deliberately pessimistic: 3.5 characters per token undercounts nothing
    /// for English prose and code, so budgets stay conservative.
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

/// `chars * 2 / 7` is `chars / 3.5` in integer arithmetic.
fn estimate(text: &str) -> usize {
    text.chars().count() * 2 / 7
}

/// Usable input tokens per chunker call.
///
/// The chunker rewrites rather than splits, so output can exceed input. Two
/// independent ceilings apply: the context has to hold input plus output, and
/// the output itself is capped by `max_output_tokens`. The smaller wins.
pub fn window_tokens(budget: ChunkBudget, prompt_overhead: usize) -> usize {
    let ratio = budget.output_ratio.max(0.1);
    let usable = budget.context_tokens.saturating_sub(prompt_overhead);
    let by_context = (usable as f32 / (1.0 + ratio)) as usize;
    let by_output = (budget.max_output_tokens as f32 / ratio) as usize;
    by_context.min(by_output).max(MIN_WINDOW_TOKENS)
}

/// How many leading items fit inside `budget` tokens. Used to pack retrieved
/// chunks into an `ask` prompt highest score first.
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
    use crate::infer::ChunkBudget;

    fn budget(ctx: usize, max_out: usize, ratio: f32) -> ChunkBudget {
        ChunkBudget {
            context_tokens: ctx,
            max_output_tokens: max_out,
            output_ratio: ratio,
        }
    }

    #[test]
    fn window_leaves_room_for_a_larger_rewritten_output() {
        // 32k context, 1000 tokens of prompt, output up to 1.4x the input.
        // (32768 - 1000) / 2.4 = 13236
        let w = window_tokens(budget(32768, 100_000, 1.4), 1000);
        assert_eq!(w, 13236);
        assert!(w < 32768 / 2, "window must not assume output is free");
    }

    #[test]
    fn window_is_also_clamped_by_max_output_tokens() {
        // Context would allow ~13k, but 8192 max output at ratio 1.4 caps
        // the input at 8192 / 1.4 = 5851.
        let w = window_tokens(budget(32768, 8192, 1.4), 1000);
        assert_eq!(w, 5851);
    }

    #[test]
    fn window_never_returns_zero_or_underflows() {
        // Overhead larger than the whole context must not wrap around.
        let w = window_tokens(budget(1000, 8192, 1.4), 5000);
        assert!(w >= MIN_WINDOW_TOKENS, "got {w}");
    }

    #[test]
    fn estimate_counter_is_conservative() {
        let c = TokenCounter::Estimate;
        // 35 characters / 3.5 = 10 tokens.
        assert_eq!(c.count(&"a".repeat(35)), 10);
        assert_eq!(c.count(""), 0);
    }

    #[test]
    fn estimate_counts_characters_not_bytes() {
        // Multi-byte text must not be counted as if every byte were a char,
        // or every German or Japanese source would be split far too small.
        let c = TokenCounter::Estimate;
        assert_eq!(c.count(&"ä".repeat(35)), 10);
    }

    #[test]
    fn pack_stops_at_the_budget() {
        let c = TokenCounter::Estimate;
        let items: Vec<String> = (0..5).map(|_| "x".repeat(35)).collect(); // 10 tokens each
        assert_eq!(pack_by_budget(&items, &c, 25), 2);
        assert_eq!(pack_by_budget(&items, &c, 1000), 5);
        assert_eq!(pack_by_budget(&items, &c, 0), 0);
    }
}
