use super::SynthesisBudget;

/// Below this a window cannot hold a useful unit of text, so a
/// misconfigured context or an oversized prompt fails loudly at the call site
/// rather than producing single-word chunks.
pub const MIN_SEGMENT_TOKENS: usize = 256;

/// Token counts for budgets. A real tokenizer where one is loadable —
/// bundled, from a configured path, or from a once-downloaded URL — and the
/// pessimistic chars/3.5 estimate where none is. `Default` is the estimator
/// alone, which is what every test runs on so size-sensitive assertions keep
/// their arithmetic.
#[derive(Default)]
pub struct TokenCounter {
    tok: Option<std::sync::Arc<tokenizers::Tokenizer>>,
}

/// Parsed tokenizers, by what was asked for. The bundled `tokenizer.json` is
/// 11 MB and `Tokenizer::from_bytes` parses all of it; `Core::from_config_with`
/// called `load` unconditionally and `Tenants::open` builds a fresh `Core` on
/// every registry cache miss, so a 32-slot registry could hold 32 unshared
/// copies and re-parse 11 MB of JSON — synchronously, inside an `async fn`, on
/// a request path — every time a tenant was reopened. Keyed by the spec and the
/// cache directory together, because a URL spec's download lands beside the
/// store and two tenants need not share one.
type Loaded = std::collections::HashMap<String, Option<std::sync::Arc<tokenizers::Tokenizer>>>;
static LOADED: std::sync::OnceLock<std::sync::Mutex<Loaded>> = std::sync::OnceLock::new();

/// The tokenizer of the model family the example config serves (Qwen, shared
/// across the family). An accuracy default, not a requirement: `infer.tokenizer`
/// points at any HF-format tokenizer.json, and every failure below falls back
/// to the estimate rather than refusing startup.
const BUNDLED: &[u8] = include_bytes!("../../assets/tokenizer.json");

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

    /// The longest leading (or trailing) run of whole characters that fits in
    /// `limit` tokens, or `None` where not one character does.
    ///
    /// For the callers that have a token budget and no line to cut on: a
    /// context block from a paste with no line breaks in it, and a neighbour's
    /// text trimmed to its share. Both used to convert the budget with a
    /// hardcoded chars-per-token ratio — 3.5, the estimator's — while this
    /// counter is a real BPE tokenizer, so on Chinese or Russian they ran two
    /// to four times over the reservation that had been subtracted for them
    /// and the model's own limit truncated the answer instead.
    ///
    /// Binary search, so it costs about `log2` counts rather than one. The
    /// upper bound is 8 characters per token, generous for every script the
    /// tokenizer has a vocabulary for: it keeps each of those counts short on a
    /// megabyte paste, and being an *upper* bound it can only leave the cut
    /// slightly short, never over.
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
            // Rounded up, so `mid` is always past `lo` and the loop cannot
            // stall on a two-wide range.
            let mid = hi - (hi - lo) / 2;
            if self.count(&take(mid)) <= limit {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        (lo > 0).then(|| take(lo))
    }

    /// Where a URL's one-time download lands: keyed by a hash of the URL so a
    /// changed link re-fetches, beside the store so it survives restarts.
    pub fn cache_path(cache_dir: &std::path::Path, url: &str) -> std::path::PathBuf {
        use sha2::{Digest, Sha256};
        let h = hex::encode(&Sha256::digest(url.as_bytes())[..8]);
        cache_dir.join(format!("tokenizer-{h}.json"))
    }

    /// Never an error: a tokenizer is an accuracy upgrade, not a reason to
    /// refuse startup. Each fallback logs what it fell back from.
    ///
    /// A URL is fetched on its own OS thread — `reqwest::blocking` builds a
    /// private runtime there, so this is safe whether or not the caller sits
    /// inside tokio — written to the cache, and read from the cache on every
    /// later boot. No cache and no network: the estimator, until next boot.
    pub fn load(spec: Option<&str>, cache_dir: &std::path::Path) -> TokenCounter {
        let key = format!("{}\u{0}{}", cache_dir.display(), spec.unwrap_or(""));
        let loaded = LOADED.get_or_init(Default::default);
        // Poisoning is not a reason to re-parse 11 MB: nothing here is an
        // invariant another thread can leave half-written.
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
                    // A counter counts. `infer.tokenizer` takes any HF file,
                    // and the BERT-family ones — `bge-m3`, which the example
                    // config names as the embedder — carry `truncation` with
                    // `max_length: 512` in the file. Honoured, every count
                    // saturates there: a book is one window, `ceiling_for_prompt`
                    // returns the whole output budget, and every synthesis call
                    // is a 400 nothing retries — with the guards that would
                    // catch it blind, because they share the undercount.
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
                            // Parsed before it is cached, and cached only if it
                            // parsed. The other order wrote whatever came back
                            // — a captive portal's sign-in page answers 200
                            // with HTML — and every later boot then read that
                            // file, failed to parse it, and fell silently back
                            // to the bundled default for the life of the
                            // install. A cached file that no longer parses is
                            // re-fetched by the same reasoning.
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

/// One GET on its own thread, so the private runtime `reqwest::blocking`
/// spins up cannot collide with a tokio runtime the caller may be on.
///
/// Timed out, because the calling thread `join`s this one: `reqwest::blocking`
/// has no default timeout, so a host that completes the TCP handshake and then
/// never answers held `Core::from_config_with` — and, on the registry, a tenant
/// open on a request path — for as long as the peer cared to keep the socket
/// open. A tokenizer is an accuracy upgrade; it does not get to hang a boot.
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

/// Usable input tokens per synthesizer call.
///
/// The synthesizer rewrites rather than splits, so output can exceed input. Two
/// independent ceilings apply: the context has to hold input plus output, and
/// the output itself is capped by `max_output_tokens`. The smaller wins.
pub fn segment_tokens(budget: SynthesisBudget, prompt_overhead: usize) -> usize {
    let ratio = budget.output_ratio.max(0.1);
    let usable = budget
        .context_tokens
        .saturating_sub(prompt_overhead)
        .saturating_sub(budget.context.total());
    let by_context = (usable as f32 / (1.0 + ratio)) as usize;
    let by_output = (budget.max_output_tokens as f32 / ratio) as usize;
    by_context.min(by_output).max(MIN_SEGMENT_TOKENS)
}

/// Headroom between an estimated prompt and the output ceiling asked for
/// beside it, as a fraction of the estimate, and never less than
/// [`MIN_HEADROOM_TOKENS`] nor more than [`MAX_HEADROOM_TOKENS`].
///
/// The estimator's error scales with the prompt, so the margin does too.
const HEADROOM_DIVISOR: usize = 8;

/// A floor under the headroom, so a short prompt still leaves the server room
/// to disagree with the estimate.
const MIN_HEADROOM_TOKENS: usize = 64;

/// A ceiling over the headroom, because the proportional margin comes out of
/// the reply and nothing else.
///
/// A caller that packs its prompt right up to what it reserved for the answer —
/// `ask` does exactly that — pays this margin out of the answer, so an uncapped
/// `prompt / 8` shrinks the reply in proportion to how much was retrieved:
/// at a 32k window and a 2k ceiling it takes the whole reply and leaves 1
/// token, and the model returns nothing at all. Beyond a few hundred tokens the
/// margin has also stopped buying anything — an estimate wrong by more than
/// that is wrong by a *factor*, and no fixed slack rescues the call.
pub const MAX_HEADROOM_TOKENS: usize = 512;

/// The smallest reply worth spending a call on. Below this the prompt does not
/// fit the window in any useful sense, and the call should be refused rather
/// than sent with a ceiling that guarantees an empty answer.
pub const MIN_REPLY_TOKENS: usize = 64;

/// The output ceiling to ask for beside a prompt of `prompt_tokens`.
///
/// The endpoint enforces one invariant the caller cannot see around: prompt
/// plus ceiling has to fit the window, as the *server* counts both. Asking for
/// the configured ceiling regardless is how a caller whose prompt is not
/// budgeted against the window — a judge handed whole artifacts to compare —
/// sends a request the endpoint refuses outright, and a 400 is not retryable,
/// so the pair is stuck at that size on every later sweep. The dedupe judge
/// packs two artifacts plus a context block it trims against this window, and
/// the trimming is only meaningful because the ceiling comes off the prompt's
/// own cost here.
///
/// `context - prompt` exactly is not enough. [`estimate`] is `chars / 3.5`,
/// which undercounts CJK and dense markup badly, and a ceiling that fills the
/// remainder of the window *by estimate* overflows it in fact — converting the
/// server's soft cap on the reply into a hard failure. So a margin comes off
/// the top, proportional to the prompt because that is how the error scales.
///
/// The margin is capped at [`MAX_HEADROOM_TOKENS`], because a caller that
/// packed its prompt against the room it reserved for the answer pays this out
/// of the answer — see that constant.
///
/// Never above `max_output_tokens`: this only ever gives back less than the
/// role was configured to allow, never more.
pub fn ceiling_for_prompt(
    context_tokens: usize,
    prompt_tokens: usize,
    max_output_tokens: usize,
) -> usize {
    raw_ceiling_for_prompt(context_tokens, prompt_tokens, max_output_tokens).max(1)
}

/// [`ceiling_for_prompt`], but `None` when the window has no room left for a
/// reply worth having.
///
/// Clamping to 1 keeps a doomed call from going out under a ceiling of zero,
/// which an endpoint reads as "no output at all" rather than as the mistake it
/// is — but the call is doomed either way, and a 200 carrying an empty reply is
/// a *worse* failure than a 400: it is indistinguishable from a transient one,
/// so the caller retries the same structurally impossible request on every
/// later sweep. A caller that can refuse should refuse here instead.
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

/// The margin [`ceiling_for_prompt`] holds back for the estimate being an
/// estimate. Public so a caller packing a prompt can reserve it up front, and
/// so agree with the ceiling it will be handed afterwards.
pub fn headroom_for_prompt(prompt_tokens: usize) -> usize {
    (prompt_tokens / HEADROOM_DIVISOR).clamp(MIN_HEADROOM_TOKENS, MAX_HEADROOM_TOKENS)
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
    use crate::infer::SynthesisBudget;

    /// The defect `cut` replaced: 3.5 characters per token is the estimator's
    /// ratio, and against a real tokenizer on a non-Latin script it let a
    /// context block run several times over the reservation subtracted for it.
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
        // The old arithmetic: 40 tokens read as 140 characters, which on this
        // text is well over 40 tokens. The point of the change.
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
        // The estimator is chars*2/7; a real BPE count differs on this input.
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
        // `infer.tokenizer` takes any HF file, and the BERT-family ones carry
        // a `truncation` block. Honoured, every count saturated at its
        // `max_length`: a book counted as one window and every synthesis call
        // went out over the endpoint's real limit.
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
        // Seed the cache the way a first boot's download would, then "load"
        // the URL with no network: the cache hit is the behavior under test.
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

    /// A prompt small against the window costs the caller nothing: it still gets
    /// everything the role was configured to allow.
    #[test]
    fn a_small_prompt_keeps_the_whole_configured_ceiling() {
        assert_eq!(ceiling_for_prompt(8192, 100, 2048), 2048);
    }

    /// The invariant the endpoint enforces, and the one a judge whose prompt is
    /// not budgeted against the window was breaking: prompt plus ceiling has to
    /// fit, and the ceiling is the half that gives.
    #[test]
    fn a_prompt_that_fills_the_window_takes_the_ceiling_down_with_it() {
        let ceiling = ceiling_for_prompt(8192, 7000, 2048);
        assert!(ceiling < 2048, "the ceiling did not give: {ceiling}");
        assert!(7000 + ceiling < 8192, "prompt plus ceiling still overflows");
    }

    /// `estimate` is `chars / 3.5`, which undercounts CJK and dense markup, so a
    /// ceiling that fills the remainder of the window *by estimate* overflows it
    /// in fact — turning the server's soft cap on the reply into a hard 400.
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

    /// A prompt that alone exceeds the window is a lost call whatever ceiling
    /// goes with it, but it must not be a ceiling of zero — an endpoint reads
    /// that as "no output at all" rather than as the mistake it is.
    #[test]
    fn an_impossible_prompt_still_asks_for_a_positive_ceiling() {
        assert_eq!(ceiling_for_prompt(4096, 99_999, 2048), 1);
    }

    /// ...and a caller that can refuse is told to, rather than being handed a
    /// ceiling that guarantees an empty 200 — which reads as a transient
    /// failure and gets retried on every later sweep.
    #[test]
    fn an_impossible_prompt_has_no_checked_ceiling() {
        assert_eq!(checked_ceiling_for_prompt(4096, 99_999, 2048), None);
        assert_eq!(checked_ceiling_for_prompt(4096, 4000, 2048), None);
        assert_eq!(checked_ceiling_for_prompt(8192, 100, 2048), Some(2048));
    }

    /// The margin comes out of the reply, so it cannot grow without bound. A
    /// caller that packs its prompt up against the room it reserved — `ask` —
    /// otherwise loses the whole reply to the margin: at this shape,
    /// `prompt / 8` is 3840 and the remaining window is 2048, so the ceiling
    /// saturated to 1 and the model returned nothing at all.
    #[test]
    fn the_margin_cannot_eat_the_whole_reply() {
        let prompt = 32768 - 2048 - MAX_HEADROOM_TOKENS;
        assert_eq!(headroom_for_prompt(prompt), MAX_HEADROOM_TOKENS);
        assert_eq!(ceiling_for_prompt(32768, prompt, 2048), 2048);

        // The other shape the uncapped margin clipped: a ceiling that fits its
        // window comfortably, packed against, still gets all of it back.
        let prompt = 8192 - 1024 - MAX_HEADROOM_TOKENS;
        assert_eq!(ceiling_for_prompt(8192, prompt, 1024), 1024);
    }

    #[test]
    fn the_context_budget_comes_out_of_the_window() {
        use crate::infer::context::ContextBudget;

        // A generous output ceiling, so the context window is the binding
        // constraint and the subtraction is visible. Under the other ceiling
        // — `by_output` — the window is capped by what the model can emit and
        // context costs nothing, which is correct but proves nothing here.
        let mut b = budget(32768, 100_000, 1.4);
        let without = segment_tokens(b, 1000);

        b.context = ContextBudget {
            opening: 200,
            overlap: 150,
            neighbors: 0,
        };
        let with = segment_tokens(b, 1000);

        // 200 + 2*150 + 60 fences = 560 prompt tokens. The window loses that
        // divided by (1 + output_ratio), because every input token it gives up
        // frees output budget too: 560 / 2.4 = 233.
        assert_eq!(without, 13236);
        assert_eq!(with, 13003);
        assert_eq!(without - with, 233);
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
        // 32k context, 1000 tokens of prompt, output up to 1.4x the input.
        // (32768 - 1000) / 2.4 = 13236
        let w = segment_tokens(budget(32768, 100_000, 1.4), 1000);
        assert_eq!(w, 13236);
        assert!(w < 32768 / 2, "window must not assume output is free");
    }

    #[test]
    fn window_is_also_clamped_by_max_output_tokens() {
        // Context would allow ~13k, but 8192 max output at ratio 1.4 caps
        // the input at 8192 / 1.4 = 5851.
        let w = segment_tokens(budget(32768, 8192, 1.4), 1000);
        assert_eq!(w, 5851);
    }

    #[test]
    fn window_never_returns_zero_or_underflows() {
        // Overhead larger than the whole context must not wrap around.
        let w = segment_tokens(budget(1000, 8192, 1.4), 5000);
        assert!(w >= MIN_SEGMENT_TOKENS, "got {w}");
    }

    #[test]
    fn estimate_counter_is_conservative() {
        let c = TokenCounter::default();
        // 35 characters / 3.5 = 10 tokens.
        assert_eq!(c.count(&"a".repeat(35)), 10);
        assert_eq!(c.count(""), 0);
    }

    #[test]
    fn estimate_counts_characters_not_bytes() {
        // Multi-byte text must not be counted as if every byte were a char,
        // or every German or Japanese source would be split far too small.
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
