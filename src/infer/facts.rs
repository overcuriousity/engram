use std::collections::BTreeSet;

const UNITS: &[&str] = &[
    "s", "ms", "us", "ns", "m", "h", "d", "w", "y", "b", "k", "kb", "mb", "gb", "tb", "pb", "kib",
    "mib", "gib", "tib", "hz", "khz", "mhz", "ghz", "px", "x",
];

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
    let separated = |s: &str| {
        s.chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | ':' | '/' | '_'))
    };
    if separated(t) {
        return true;
    }
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

pub fn fact_tokens(text: &str) -> BTreeSet<String> {
    text.split_whitespace()
        .map(|t| t.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|t| is_fact(t))
        .map(|t| devee(t).to_ascii_lowercase())
        .collect()
}

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
        assert!(!may_disagree(
            "The mount command attaches a filesystem.",
            "Version 9.9.9 of the pastry compiler ships on 2030-01-01.",
        ));
    }

    #[test]
    fn unrelated_values_do_get_through_and_that_is_deliberate() {
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
        let f = fact_tokens("The 3rd step targets x86_64 hardware.");
        assert!(f.is_empty(), "{f:?}");
    }
}
