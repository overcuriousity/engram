//! Could these two artifacts actually disagree?
//!
//! The similarity sweep says two artifacts cover the same ground. That is not
//! the interesting question — the interesting one is whether they state some
//! detail differently, because a wrong artifact ranks exactly as well as a
//! right one and nothing else in the system notices. Answering it properly
//! needs a model, and a model call is minutes on the hardware this is built
//! for, so this narrows the candidate set first at the cost of a scan.
//!
//! The rule is deliberately conservative in one direction only: it must never
//! discard a pair that might disagree, and it is free to pass through pairs
//! that turn out not to. A pair it passes costs one call; a pair it wrongly
//! drops costs a stale artifact nobody ever finds.

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

/// Whether a pair is worth a model call.
///
/// Both artifacts must state some value, and their values must not be
/// identical. That is all this can safely require. An earlier version also
/// demanded a shared value, on the theory that it showed the two were talking
/// about the same measurable thing — but that is exactly backwards for the case
/// this exists to catch: one artifact says `1.21.4` and the other says
/// `1.30.0`, and they share nothing at all.
///
/// So a pair stating unrelated values does get through and costs one call. That
/// is the cheap direction of the error, and it is the one to be wrong in.
///
/// What this deliberately does *not* decide is whether the two are about the
/// same subject. That used to be assumed settled by the similarity that put
/// them in a pair, and it is not: in a reference document the entries for
/// FAT12, FAT16 and FAT32 are near-identical in form and deliberately different
/// in content, so they score 0.91 and every number in them differs. Similarity
/// measures shape. The judge is the only thing here that can read a subject,
/// which is why it is given both titles and told that different named things
/// are not in conflict.
pub fn may_disagree(a: &str, b: &str) -> bool {
    let (fa, fb) = (fact_tokens(a), fact_tokens(b));
    !fa.is_empty() && !fb.is_empty() && fa != fb
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
    fn two_artifacts_giving_a_different_version_may_disagree() {
        assert!(may_disagree(
            "engram requires Rust 1.21.4 to build.",
            "engram requires Rust 1.30.0 to build.",
        ));
    }

    #[test]
    fn the_same_fact_stated_twice_does_not_disagree() {
        assert!(!may_disagree(
            "engram requires Rust 1.21.4 to build.",
            "To build engram you need Rust 1.21.4.",
        ));
    }

    #[test]
    fn a_pair_where_only_one_side_states_a_value_is_not_judged() {
        // Nothing to compare, so nothing a model could rule on.
        assert!(!may_disagree(
            "The mount command attaches a filesystem.",
            "Version 9.9.9 of the pastry compiler ships on 2030-01-01.",
        ));
    }

    #[test]
    fn unrelated_values_do_get_through_and_that_is_deliberate() {
        // The filter is allowed to pass a pair that turns out to be fine — that
        // costs one call. It is not allowed to drop one that disagrees, which
        // would cost a stale artifact nobody ever finds.
        assert!(may_disagree(
            "The timeout is 30 seconds.",
            "It listens on 8080."
        ));
    }

    #[test]
    fn one_artifact_with_no_facts_never_disagrees() {
        assert!(!may_disagree("Prose only.", "Requires 1.2.3."));
    }

    #[test]
    fn versions_carrying_a_v_are_facts_and_equal_the_bare_form() {
        let f = fact_tokens("Needs v1.21.4 of the toolchain.");
        assert!(f.contains("1.21.4"), "{f:?}");
        assert!(!may_disagree(
            "Needs v1.21.4 of the toolchain.",
            "Needs 1.21.4 of the toolchain.",
        ));
        assert!(may_disagree(
            "Needs v1.21.4 of the toolchain.",
            "Needs v1.30.0 of the toolchain.",
        ));
    }

    #[test]
    fn a_number_with_a_unit_is_a_value() {
        // The most common way a timeout or a size is actually written. Before
        // these counted, two artifacts disagreeing about `30s` versus `90s`
        // carried no fact tokens at all and were filed as no-conflict.
        let f = fact_tokens("The timeout is 30s and the batch is 512MB.");
        assert!(f.contains("30s"), "{f:?}");
        assert!(f.contains("512mb"), "{f:?}");
        assert!(may_disagree("Wait 30s.", "Wait 90s."));
    }

    #[test]
    fn a_separator_marks_the_rest_as_structured() {
        let f = fact_tokens("Listens on 8080/tcp, tagged 2.0-rc1.");
        assert!(f.contains("8080/tcp"), "{f:?}");
        assert!(f.contains("2.0-rc1"), "{f:?}");
    }

    #[test]
    fn a_word_that_merely_starts_with_a_digit_is_not_a_fact() {
        // `3rd` and `x86_64` are vocabulary, not values. Treating them as facts
        // sends pairs to the model that have nothing to disagree about.
        let f = fact_tokens("The 3rd step targets x86_64 hardware.");
        assert!(f.is_empty(), "{f:?}");
    }
}
