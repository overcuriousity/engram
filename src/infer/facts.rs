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
fn is_fact(token: &str) -> bool {
    if !token.starts_with(|c: char| c.is_ascii_digit()) {
        return false;
    }
    // A leading digit plus only digits and separators: 30, 1.21.4, 2024-03-01,
    // 8080. Anything with letters in it — `3rd`, `x86_64` — is vocabulary
    // rather than a value.
    token
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | ':'))
}

/// Every value-shaped token in the text, stripped of surrounding punctuation.
pub fn fact_tokens(text: &str) -> BTreeSet<String> {
    text.split_whitespace()
        .map(|t| t.trim_matches(|c: char| !c.is_alphanumeric()).to_string())
        .filter(|t| is_fact(t))
        .collect()
}

/// Whether a pair is worth a model call.
///
/// Both artifacts must state some value, and their values must not be
/// identical. That is all this can safely require. An earlier version also
/// demanded a shared value, on the theory that it showed the two were talking
/// about the same measurable thing — but that is exactly backwards for the case
/// this exists to catch: one artifact says `1.21.4` and the other says
/// `1.30.0`, and they share nothing at all. Whether the two are about the same
/// subject was already settled by the similarity that put them in a pair.
///
/// So a pair stating unrelated values does get through and costs one call. That
/// is the cheap direction of the error, and it is the one to be wrong in.
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
    fn a_word_that_merely_starts_with_a_digit_is_not_a_fact() {
        // `3rd` and `x86_64` are vocabulary, not values. Treating them as facts
        // sends pairs to the model that have nothing to disagree about.
        let f = fact_tokens("The 3rd step targets x86_64 hardware.");
        assert!(f.is_empty(), "{f:?}");
    }
}
