use super::SynthesisBudget;

/// Below this a window holds no useful unit of text, so misconfiguration fails loudly instead of making one-word chunks.
pub const MIN_SEGMENT_TOKENS: usize = 256;

/// A real tokenizer where loadable, else the pessimistic chars/3.5 estimate. `Default` is the estimator, which tests rely on.
#[derive(Default)]
pub struct TokenCounter {
    tok: Option<std::sync::Arc<tokenizers::Tokenizer>>,
}

/// Memo of parsed tokenizers: the bundled file is 11 MB and `Tenants::open` builds a `Core` per cache miss.
/// Keyed by spec and cache directory, since a URL's download lands beside the store.
type Loaded = std::collections::HashMap<String, Option<std::sync::Arc<tokenizers::Tokenizer>>>;
static LOADED: std::sync::OnceLock<std::sync::Mutex<Loaded>> = std::sync::OnceLock::new();

/// Under `vendor/`, not `assets/`: `rust_embed` takes all of `assets/` and would serve it at `/assets/tokenizer.json`.
const BUNDLED: &[u8] = include_bytes!("../../vendor/tokenizer.json");

impl TokenCounter {
    pub fn count(&self, text: &str) -> usize {
        match &self.tok {
            Some(t) => t
                .encode_fast(text, false)
                .map(|e| e.len())
                .unwrap_or_else(|_| estimate(text)),
            None => estimate(text),
        }
    }

    /// The longest leading (or trailing) run of whole characters within `limit` tokens; `None` if not one fits.
    /// The 8-chars-per-token search bound is an upper bound, so the cut can only fall short, never over.
    pub fn cut(&self, text: &str, limit: usize, from_start: bool) -> Option<String> {
        if limit == 0 || text.is_empty() {
            return None;
        }
        if self.count(text) <= limit {
            return Some(text.to_string());
        }
        let chars: Vec<char> = text.chars().collect();
        let take = |n: usize| -> String {
            if from_start {
                chars[..n].iter().collect()
            } else {
                chars[chars.len() - n..].iter().collect()
            }
        };
        let (mut lo, mut hi) = (0usize, chars.len().min(limit.saturating_mul(8)));
        while lo < hi {
            // Rounded up so `mid` is past `lo` and the loop cannot stall on a two-wide range.
            let mid = hi - (hi - lo) / 2;
            if self.count(&take(mid)) <= limit {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        (lo > 0).then(|| take(lo))
    }

    /// Keyed by a hash of the URL so a changed link re-fetches.
    pub fn cache_path(cache_dir: &std::path::Path, url: &str) -> std::path::PathBuf {
        use sha2::{Digest, Sha256};
        let h = hex::encode(&Sha256::digest(url.as_bytes())[..8]);
        cache_dir.join(format!("tokenizer-{h}.json"))
    }

    /// Never an error: a tokenizer is an accuracy upgrade, not a reason to refuse startup.
    /// A URL is fetched on its own OS thread, so `reqwest::blocking`'s private runtime is safe inside tokio.
    pub fn load(spec: Option<&str>, cache_dir: &std::path::Path) -> TokenCounter {
        let key = format!("{}\u{0}{}", cache_dir.display(), spec.unwrap_or(""));
        let loaded = LOADED.get_or_init(Default::default);
        // Poisoning is not a reason to re-parse 11 MB: nothing here can be left half-written.
        if let Some(hit) = loaded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .cloned()
        {
            return TokenCounter { tok: hit };
        }
        let tok = Self::parse(spec, cache_dir);
        loaded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, tok.clone());
        TokenCounter { tok }
    }

    /// Fills the `load` memo on a blocking thread, so the 11 MB parse never stalls a tokio worker on a request path.
    pub async fn warm(spec: Option<&str>, cache_dir: &std::path::Path) {
        let spec = spec.map(String::from);
        let dir = cache_dir.to_path_buf();
        let _ = tokio::task::spawn_blocking(move || {
            Self::load(spec.as_deref(), &dir);
        })
        .await;
    }

    fn parse(
        spec: Option<&str>,
        cache_dir: &std::path::Path,
    ) -> Option<std::sync::Arc<tokenizers::Tokenizer>> {
        let from_bytes = |b: &[u8], what: &str| {
            tokenizers::Tokenizer::from_bytes(b)
                .map_err(
                    |e| tracing::warn!(error = %e, what, "tokenizer did not parse; falling back"),
                )
                .ok()
                .map(|mut t| {
                    // BERT-family files (`bge-m3`) carry `truncation` at 512; honoured, every count saturates there.
                    t.with_truncation(None).ok();
                    t.with_padding(None);
                    std::sync::Arc::new(t)
                })
        };
        match spec {
            Some(s) if s.starts_with("http://") || s.starts_with("https://") => {
                let cache = Self::cache_path(cache_dir, s);
                std::fs::read(&cache)
                    .ok()
                    .and_then(|b| from_bytes(&b, "cached"))
                    .or_else(|| {
                        fetch_blocking(s).and_then(|b| {
                            // Parse before caching: a captive portal answers 200 with HTML, and a cached bad file would stick for good.
                            let t = from_bytes(&b, "downloaded")?;
                            let _ = std::fs::create_dir_all(cache_dir);
                            if let Err(e) = std::fs::write(&cache, &b) {
                                tracing::warn!(error = %e, "could not cache the tokenizer; it will re-download next boot");
                            }
                            Some(t)
                        })
                    })
                    .or_else(|| from_bytes(BUNDLED, "bundled"))
            }
            Some(p) => std::fs::read(p)
                .map_err(|e| tracing::warn!(error = %e, path = p, "tokenizer path unreadable; using the bundled default"))
                .ok()
                .and_then(|b| from_bytes(&b, "configured"))
                .or_else(|| from_bytes(BUNDLED, "bundled")),
            None => from_bytes(BUNDLED, "bundled"),
        }
    }
}

/// `reqwest::blocking` has no default timeout, and the caller `join`s this thread.
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

fn fetch_blocking(url: &str) -> Option<Vec<u8>> {
    let url = url.to_string();
    std::thread::spawn(move || {
        reqwest::blocking::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .and_then(|c| c.get(&url).send())
            .and_then(|r| r.error_for_status())
            .and_then(|r| r.bytes())
            .map(|b| b.to_vec())
    })
    .join()
    .ok()
    .and_then(|r| {
        r.map_err(|e| tracing::warn!(error = %e, "tokenizer download failed; estimator in use until next boot"))
            .ok()
    })
}

/// `chars * 2 / 7` is `chars / 3.5` in integer arithmetic.
fn estimate(text: &str) -> usize {
    text.chars().count() * 2 / 7
}

/// Usable input tokens per synthesizer call: the smaller of the context and `max_output_tokens` ceilings.
/// `None` below [`MIN_SEGMENT_TOKENS`]: a saturated zero handed back as 256 made every window a non-retryable 400.
pub fn segment_tokens(budget: SynthesisBudget, prompt_overhead: usize) -> Option<usize> {
    let ratio = budget.output_ratio.max(0.1);
    let usable = budget
        .context_tokens
        .saturating_sub(prompt_overhead)
        .saturating_sub(budget.context.total());
    let by_context = (usable as f32 / (1.0 + ratio)) as usize;
    let by_output = (budget.max_output_tokens as f32 / ratio) as usize;
    let room = by_context.min(by_output);
    (room >= MIN_SEGMENT_TOKENS).then_some(room)
}

/// The estimator's error scales with the prompt, so the margin does too.
const HEADROOM_DIVISOR: usize = 8;

const MIN_HEADROOM_TOKENS: usize = 64;

/// The margin comes out of the reply: uncapped, a packed 32k prompt left a 2k ceiling 1 token.
pub const MAX_HEADROOM_TOKENS: usize = 512;

/// Below this the call is refused rather than sent with a ceiling that guarantees an empty answer.
pub const MIN_REPLY_TOKENS: usize = 64;

/// Prompt plus ceiling must fit the window as the server counts; a 400 there is not retryable.
/// A capped margin comes off the top because [`estimate`] undercounts CJK and markup. Never above `max_output_tokens`.
pub fn ceiling_for_prompt(
    context_tokens: usize,
    prompt_tokens: usize,
    max_output_tokens: usize,
) -> usize {
    raw_ceiling_for_prompt(context_tokens, prompt_tokens, max_output_tokens).max(1)
}

/// `None` instead of a ceiling of 1: an empty 200 looks transient and is retried on every sweep.
pub fn checked_ceiling_for_prompt(
    context_tokens: usize,
    prompt_tokens: usize,
    max_output_tokens: usize,
) -> Option<usize> {
    let ceiling = raw_ceiling_for_prompt(context_tokens, prompt_tokens, max_output_tokens);
    (ceiling >= MIN_REPLY_TOKENS.min(max_output_tokens.max(1))).then_some(ceiling)
}

fn raw_ceiling_for_prompt(
    context_tokens: usize,
    prompt_tokens: usize,
    max_output_tokens: usize,
) -> usize {
    context_tokens
        .saturating_sub(prompt_tokens)
        .saturating_sub(headroom_for_prompt(prompt_tokens))
        .min(max_output_tokens)
}

/// Public so a caller packing a prompt can reserve it up front and agree with the ceiling it gets.
pub fn headroom_for_prompt(prompt_tokens: usize) -> usize {
    (prompt_tokens / HEADROOM_DIVISOR).clamp(MIN_HEADROOM_TOKENS, MAX_HEADROOM_TOKENS)
}

/// Packs retrieved chunks into an `ask` prompt, highest score first.
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

    #[test]
    fn cutting_holds_a_token_budget_on_a_script_the_ratio_does_not_describe() {
        let real = TokenCounter::load(None, std::path::Path::new("/nonexistent-cache"));
        let cn = "今天下午三点在办公室开会讨论季度预算和明年的招聘计划。".repeat(40);
        for counter in [&real, &TokenCounter::default()] {
            for from_start in [true, false] {
                let got = counter.cut(&cn, 40, from_start).expect("something fits");
                assert!(counter.count(&got) <= 40, "over the budget it was given");
                assert!(!got.is_empty());
                if from_start {
                    assert!(cn.starts_with(&got));
                } else {
                    assert!(cn.ends_with(&got));
                }
            }
        }
        let naive: String = cn.chars().take(40 * 7 / 2).collect();
        assert!(real.count(&naive) > 40);

        assert_eq!(real.cut("short", 1_000, true).as_deref(), Some("short"));
        assert_eq!(real.cut("anything", 0, true), None);
        assert_eq!(real.cut("", 10, true), None);
    }

    #[test]
    fn the_bundled_tokenizer_counts_and_differs_from_the_estimator() {
        let real = TokenCounter::load(None, std::path::Path::new("/nonexistent-cache"));
        let est = TokenCounter::default();
        let text = "Der Bericht muss bis Freitag um 16:00 abgegeben werden.";
        assert!(real.count(text) > 0);
        assert_ne!(real.count(text), est.count(text));
    }

    #[test]
    fn a_bad_path_falls_back_to_the_bundled_tokenizer_not_a_failure() {
        let c = TokenCounter::load(Some("/no/such/file.json"), std::path::Path::new("/tmp"));
        let bundled = TokenCounter::load(None, std::path::Path::new("/tmp"));
        assert_eq!(c.count("hello world"), bundled.count("hello world"));
    }

    #[test]
    fn a_tokenizer_file_carrying_truncation_still_counts_the_whole_text() {
        // BERT-family files carry `truncation`; honoured, every count saturates at `max_length`.
        let dir = std::env::temp_dir().join(format!("tok-trunc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("tokenizer.json");
        let truncating = String::from_utf8(BUNDLED.to_vec()).unwrap().replacen(
            "\"truncation\": null",
            "\"truncation\": {\"direction\": \"Right\", \"max_length\": 8, \
                 \"strategy\": \"LongestFirst\", \"stride\": 0}",
            1,
        );
        std::fs::write(&file, &truncating).unwrap();
        let c = TokenCounter::load(Some(file.to_str().unwrap()), &dir);
        let long = "eine Zeile, die sich wiederholt. ".repeat(200);
        assert!(
            c.count(&long) > 8,
            "the count saturated at the file's max_length"
        );
        assert_eq!(
            c.count(&long),
            TokenCounter::load(None, &dir).count(&long),
            "and it is the count the same tokenizer gives without the block"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_cached_url_download_is_read_from_the_cache_file() {
        // The cache hit with no network is the behavior under test.
        let dir = std::env::temp_dir().join(format!("tok-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let url = "https://example.invalid/tokenizer.json";
        let cache = TokenCounter::cache_path(&dir, url);
        std::fs::write(&cache, BUNDLED).unwrap();
        let c = TokenCounter::load(Some(url), &dir);
        assert_ne!(
            c.count("hello world"),
            TokenCounter::default().count("hello world"),
            "the cached real tokenizer must be in use"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    fn budget(ctx: usize, max_out: usize, ratio: f32) -> SynthesisBudget {
        SynthesisBudget {
            context_tokens: ctx,
            max_output_tokens: max_out,
            output_ratio: ratio,
            context: crate::infer::context::ContextBudget::default(),
        }
    }

    #[test]
    fn a_small_prompt_keeps_the_whole_configured_ceiling() {
        assert_eq!(ceiling_for_prompt(8192, 100, 2048), 2048);
    }

    #[test]
    fn a_prompt_that_fills_the_window_takes_the_ceiling_down_with_it() {
        let ceiling = ceiling_for_prompt(8192, 7000, 2048);
        assert!(ceiling < 2048, "the ceiling did not give: {ceiling}");
        assert!(7000 + ceiling < 8192, "prompt plus ceiling still overflows");
    }

    #[test]
    fn the_ceiling_leaves_the_estimate_room_to_be_wrong() {
        let prompt = 4000;
        let ceiling = ceiling_for_prompt(8192, prompt, 100_000);
        assert!(
            ceiling < 8192 - prompt,
            "the whole remainder went out with no margin: {ceiling}"
        );
        assert_eq!(ceiling, 8192 - prompt - prompt / HEADROOM_DIVISOR);
    }

    #[test]
    fn an_impossible_prompt_still_asks_for_a_positive_ceiling() {
        assert_eq!(ceiling_for_prompt(4096, 99_999, 2048), 1);
    }

    #[test]
    fn an_impossible_prompt_has_no_checked_ceiling() {
        assert_eq!(checked_ceiling_for_prompt(4096, 99_999, 2048), None);
        assert_eq!(checked_ceiling_for_prompt(4096, 4000, 2048), None);
        assert_eq!(checked_ceiling_for_prompt(8192, 100, 2048), Some(2048));
    }

    #[test]
    fn the_margin_cannot_eat_the_whole_reply() {
        let prompt = 32768 - 2048 - MAX_HEADROOM_TOKENS;
        assert_eq!(headroom_for_prompt(prompt), MAX_HEADROOM_TOKENS);
        assert_eq!(ceiling_for_prompt(32768, prompt, 2048), 2048);

        let prompt = 8192 - 1024 - MAX_HEADROOM_TOKENS;
        assert_eq!(ceiling_for_prompt(8192, prompt, 1024), 1024);
    }

    #[test]
    fn the_context_budget_comes_out_of_the_window() {
        use crate::infer::context::ContextBudget;

        // A generous output ceiling makes the context window the binding constraint.
        let mut b = budget(32768, 100_000, 1.4);
        let without = segment_tokens(b, 1000);

        b.context = ContextBudget {
            opening: 200,
            overlap: 150,
            neighbors: 0,
        };
        let with = segment_tokens(b, 1000);

        // 660 prompt tokens cost 660 / (1 + 1.4) = 275 of window, since freed input frees output too.
        let (without, with) = (without.expect("a window"), with.expect("a window"));
        assert_eq!(without, 13236);
        assert_eq!(with, 12961);
        assert_eq!(without - with, 275);
    }

    #[test]
    fn no_context_budget_leaves_the_window_exactly_as_it_was() {
        let b = budget(32768, 8192, 1.4);
        assert_eq!(b.context, crate::infer::context::ContextBudget::default());
        assert_eq!(segment_tokens(b, 1000), segment_tokens(b, 1000));
        assert_eq!(b.context.total(), 0);
    }

    #[test]
    fn window_leaves_room_for_a_larger_rewritten_output() {
        // (32768 - 1000) / 2.4 = 13236
        let w = segment_tokens(budget(32768, 100_000, 1.4), 1000).expect("a window");
        assert_eq!(w, 13236);
        assert!(w < 32768 / 2, "window must not assume output is free");
    }

    #[test]
    fn window_is_also_clamped_by_max_output_tokens() {
        // 8192 max output at ratio 1.4 caps the input at 5851.
        let w = segment_tokens(budget(32768, 8192, 1.4), 1000).expect("a window");
        assert_eq!(w, 5851);
    }

    #[test]
    fn a_context_too_small_for_its_own_prompt_is_no_window_at_all() {
        assert_eq!(segment_tokens(budget(1000, 8192, 1.4), 5000), None);
        let just_under = segment_tokens(budget(1000, 8192, 1.4), 400);
        assert!(
            just_under.is_none_or(|w| w >= MIN_SEGMENT_TOKENS),
            "{just_under:?} is under the floor and was still offered"
        );
    }

    #[test]
    fn estimate_counter_is_conservative() {
        let c = TokenCounter::default();
        assert_eq!(c.count(&"a".repeat(35)), 10);
        assert_eq!(c.count(""), 0);
    }

    #[test]
    fn estimate_counts_characters_not_bytes() {
        // Counting bytes as chars would split every German or Japanese source far too small.
        let c = TokenCounter::default();
        assert_eq!(c.count(&"ä".repeat(35)), 10);
    }

    #[test]
    fn pack_stops_at_the_budget() {
        let c = TokenCounter::default();
        let items: Vec<String> = (0..5).map(|_| "x".repeat(35)).collect(); // 10 tokens each
        assert_eq!(pack_by_budget(&items, &c, 25), 2);
        assert_eq!(pack_by_budget(&items, &c, 1000), 5);
        assert_eq!(pack_by_budget(&items, &c, 0), 0);
    }
}
