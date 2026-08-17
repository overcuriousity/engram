//! Which values do these artifacts not state the same way?
//!
//! A wrong artifact ranks exactly as well as a right one and nothing else in
//! the system notices, so the values two artifacts give for the same thing are
//! worth singling out. This finds them without a model.
//!
//! It used to answer a yes/no question — "could these two disagree?" — and gate
//! the model on it. That gate was removed on 2026-08-14: it admitted a pair only
//! when values *differed*, which is right for hunting contradictions and exactly
//! backwards for deduplication, where two artifacts saying the same thing in
//! different words are the cleanest thing there is to merge.
//!
//! What replaced the gate was a list of the values not shared by every artifact,
//! handed to the dedupe prompt as a prior. That went too, on 2026-08-17, and only
//! `fact_tokens` is left. The list compared tokens, and `fact_tokens` splits on
//! whitespace, so the comparison answered a question about punctuation rather
//! than about facts: `Win7/8/10` yields no tokens at all, `(Windows 7-10)` yields
//! `7-10`, `Windows 7, 8 und 10` yields `7`, `8`, `10`. Three artifacts stating
//! one thing three ways came out as a difference, and the prompt named four bare
//! integers to the model as values its artifacts disagreed about — which the
//! model then reported as a contradiction, correctly, on that evidence. It also
//! could not distinguish "only one side states this" from "the two state it
//! differently", so a strict superset arrived as a dispute.
//!
//! `fact_tokens` itself stays, and the rule it serves is unchanged: whatever it
//! finds in an artifact must survive into text merged from it, or a value was
//! dropped. That comparison is sound where the list's was not, because it asks
//! whether a token is *present* rather than whether two spellings match, and
//! because numbers are the one thing in this corpus that read the same in German
//! and in English. See `jobs::merge::losses`, its only caller.

use std::collections::BTreeSet;

/// Is this token shaped like something two documents could state differently?
///
/// Bare numbers count: a timeout, a port, a count of retries. Words do not —
/// two artifacts using different prose for the same thing is what synthesis is
/// supposed to produce, not a contradiction.
/// Suffixes that make a bare number a measurement rather than a word.
///
/// A closed list on purpose. Accepting any trailing letters would make `3rd`
/// and `2nd` values, and an ordinal is prose — every step-by-step artifact has
/// one, so every pair would reach the model.
const UNITS: &[&str] = &[
    "s", "ms", "us", "ns", "m", "h", "d", "w", "y", "b", "k", "kb", "mb", "gb", "tb", "pb", "kib",
    "mib", "gib", "tib", "hz", "khz", "mhz", "ghz", "px", "x",
];

/// A version is written `v1.21.4` as often as `1.21.4`, and the `v` is not what
/// makes two artifacts agree or differ. Dropping it here means one artifact
/// saying `v1.21.4` and another saying `1.21.4` compare as the same value
/// rather than as two, which is what keeps them off the model's desk.
fn devee(token: &str) -> &str {
    match token.strip_prefix(['v', 'V']) {
        Some(rest) if rest.starts_with(|c: char| c.is_ascii_digit()) => rest,
        _ => token,
    }
}

fn is_fact(token: &str) -> bool {
    let t = devee(token);
    if !t.starts_with(|c: char| c.is_ascii_digit()) {
        return false;
    }
    // Digits and separators only: 30, 1.21.4, 2024-03-01, 8080.
    let separated = |s: &str| {
        s.chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | ':' | '/' | '_'))
    };
    if separated(t) {
        return true;
    }
    // Otherwise the letters have to earn their place. A number carrying a unit
    // is a value — `30s`, `512mb` — and so is anything a separator has already
    // marked as structured: `8080/tcp`, `2.0-rc1`, `1.21.4-beta`. What this
    // still refuses is a bare number glued to a word: `3rd`, `x86_64`.
    let split = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (digits, rest) = t.split_at(split);
    if digits.is_empty() {
        return false;
    }
    if rest.starts_with(['.', '-', ':', '/', '_']) {
        return true;
    }
    UNITS.contains(&rest.to_ascii_lowercase().as_str())
}

/// Every value-shaped token in the text, stripped of surrounding punctuation.
///
/// Punctuation is trimmed from the edges only: `8080/tcp` and `1.21.4` keep
/// their insides, and a trailing full stop or a wrapping backtick goes.
pub fn fact_tokens(text: &str) -> BTreeSet<String> {
    text.split_whitespace()
        .map(|t| t.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|t| is_fact(t))
        .map(|t| devee(t).to_ascii_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_numbers_and_dates_are_facts() {
        let f = fact_tokens("Requires 1.21.4 or later, 30 seconds, on 2024-03-01, at 8080.");
        assert!(f.contains("1.21.4"), "{f:?}");
        assert!(f.contains("2024-03-01"), "{f:?}");
        assert!(f.contains("30"), "{f:?}");
        assert!(f.contains("8080"), "{f:?}");
    }

    #[test]
    fn ordinary_prose_carries_no_facts() {
        assert!(fact_tokens("Mount the filesystem before writing to it.").is_empty());
    }

    #[test]
    fn versions_carrying_a_v_are_facts_and_equal_the_bare_form() {
        // A merge of one text into the other must not be told a value went
        // missing just because the source wrote the `v`.
        assert_eq!(
            fact_tokens("Needs v1.21.4 of the toolchain."),
            fact_tokens("Needs 1.21.4 of the toolchain."),
            "the v prefix made one value look like two"
        );
    }

    #[test]
    fn a_number_with_a_unit_is_a_value() {
        // The most common way a timeout or a size is actually written. Before
        // these counted, two artifacts disagreeing about `30s` versus `90s`
        // carried no fact tokens at all and were filed as no-conflict.
        let f = fact_tokens("The timeout is 30s and the batch is 512MB.");
        assert!(f.contains("30s"), "{f:?}");
        assert!(f.contains("512mb"), "{f:?}");
    }

    #[test]
    fn a_separator_marks_the_rest_as_structured() {
        let f = fact_tokens("Listens on 8080/tcp, tagged 2.0-rc1.");
        assert!(f.contains("8080/tcp"), "{f:?}");
        assert!(f.contains("2.0-rc1"), "{f:?}");
    }

    #[test]
    fn how_a_version_list_is_punctuated_decides_what_is_extracted() {
        // Why comparing two texts' token sets is not a question about facts.
        // These three lines say one thing, and the tokenizer splits on
        // whitespace, so they yield three unrelated sets. A difference between
        // them was named to the dedupe judge as values the artifacts state
        // differently, and it reported the contradiction that implies. Pinned
        // rather than fixed: presence is all `merge::losses` asks of these, and
        // it is the comparison that was unsound.
        assert!(
            fact_tokens("First Install (Win7/8/10)").is_empty(),
            "digits glued to a word are not values, which is the whole asymmetry"
        );
        assert_eq!(
            fact_tokens("Registry-Werte für USB-Geräte (Windows 7-10):"),
            ["7-10".to_string()].into_iter().collect()
        );
        assert_eq!(
            fact_tokens("gespeichert unter Windows 7, 8 und 10"),
            ["10".to_string(), "7".to_string(), "8".to_string()]
                .into_iter()
                .collect()
        );
    }

    #[test]
    fn a_word_that_merely_starts_with_a_digit_is_not_a_fact() {
        // `3rd` and `x86_64` are vocabulary, not values. Treating them as facts
        // sends pairs to the model that have nothing to disagree about.
        let f = fact_tokens("The 3rd step targets x86_64 hardware.");
        assert!(f.is_empty(), "{f:?}");
    }
}
