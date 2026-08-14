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
//! different words are the cleanest thing there is to merge. What survives is
//! the list, which decides nothing: it is a prior for the prompt, and the rule
//! the merge verification enforces — whatever appears in it must survive into
//! the merged text, or a value was dropped.

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

/// Values that not all of these texts state.
///
/// This is what `may_disagree` became once deduplication replaced contradiction
/// hunting. As a gate the predicate was backwards: it admitted a pair only when
/// values *differed*, which discards exactly the pairs that are cleanest to
/// merge — two artifacts saying the same thing in different words have nothing
/// to contradict and everything to combine.
///
/// As a list it is useful in both directions. Handed to the model it names what
/// to look at without deciding anything, because it cannot tell a real
/// disagreement from the same subject described at two levels of detail. Handed
/// to the merge verification it becomes a hard rule: whatever appears here has
/// to survive into the merged text, or the merge dropped a value and is refused.
///
/// Sorted, because it goes into a prompt: the endpoint caches by exact prompt
/// text, and a set iterating in a different order would defeat that for no gain.
pub fn differing_values(texts: &[&str]) -> Vec<String> {
    let sets: Vec<BTreeSet<String>> = texts.iter().map(|t| fact_tokens(t)).collect();
    let mut all: BTreeSet<String> = BTreeSet::new();
    for s in &sets {
        all.extend(s.iter().cloned());
    }
    all.into_iter()
        .filter(|tok| !sets.iter().all(|s| s.contains(tok)))
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
        let f = fact_tokens("Needs v1.21.4 of the toolchain.");
        assert!(f.contains("1.21.4"), "{f:?}");
        assert!(
            differing_values(&[
                "Needs v1.21.4 of the toolchain.",
                "Needs 1.21.4 of the toolchain.",
            ])
            .is_empty(),
            "the v prefix made one value look like two"
        );
        assert_eq!(
            differing_values(&[
                "Needs v1.21.4 of the toolchain.",
                "Needs v1.30.0 of the toolchain.",
            ]),
            vec!["1.21.4".to_string(), "1.30.0".to_string()]
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
        assert_eq!(
            differing_values(&["Wait 30s.", "Wait 90s."]),
            vec!["30s".to_string(), "90s".to_string()]
        );
    }

    #[test]
    fn a_separator_marks_the_rest_as_structured() {
        let f = fact_tokens("Listens on 8080/tcp, tagged 2.0-rc1.");
        assert!(f.contains("8080/tcp"), "{f:?}");
        assert!(f.contains("2.0-rc1"), "{f:?}");
    }

    #[test]
    fn differing_values_names_only_what_is_not_shared() {
        // The prompt prior. A value every artifact states is not something to
        // ask about; a value only some of them state is.
        assert_eq!(
            differing_values(&[
                "The timeout is 30s on port 8080.",
                "The timeout is 90s on port 8080."
            ]),
            vec!["30s".to_string(), "90s".to_string()],
            "the shared port should not be named"
        );
    }

    #[test]
    fn texts_agreeing_on_every_value_differ_in_none() {
        // And this is the case the old gate discarded: nothing to disagree
        // about, everything to merge.
        assert!(
            differing_values(&["Needs v1.21.4 to build.", "To build it you need 1.21.4."])
                .is_empty()
        );
        assert!(differing_values(&["Prose only.", "Different prose."]).is_empty());
    }

    #[test]
    fn a_value_only_one_artifact_states_is_a_differing_value() {
        // It is exactly what a merge must carry over, so verification has to
        // see it even though nothing contradicts it.
        assert_eq!(
            differing_values(&["Mount it first.", "Mount it first. Wait 30s."]),
            vec!["30s".to_string()]
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
