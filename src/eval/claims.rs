//! The claim check: what the efficient model said about which excerpt
//! supports each sentence of an answer. Parsing only — the call is made by
//! the harness, and only when asked for.

use crate::infer::prompt::extract_json;
use anyhow::{Context, Result};

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct Claim {
    pub claim: String,
    #[serde(default)]
    pub supported_by: Vec<usize>,
}

#[derive(serde::Deserialize)]
struct Reply {
    claims: Vec<Claim>,
}

/// `shown` is how many excerpts the model was given; a number outside
/// `1..=shown` names nothing and is dropped from that claim rather than
/// counted as support.
pub fn parse_claims(reply: &str, shown: usize) -> Result<Vec<Claim>> {
    let r: Reply = serde_json::from_str(extract_json(reply))
        .context("claim check reply was not the expected JSON")?;
    Ok(r.claims
        .into_iter()
        .map(|mut c| {
            c.supported_by.retain(|n| (1..=shown).contains(n));
            c
        })
        .collect())
}

/// `(claims with at least one supporting excerpt, claims)`.
pub fn supported(claims: &[Claim]) -> (usize, usize) {
    (
        claims.iter().filter(|c| !c.supported_by.is_empty()).count(),
        claims.len(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_well_formed_reply_is_read_and_counted() {
        let claims = parse_claims(
            r#"{"claims":[{"claim":"use -o ro","supported_by":[1]},{"claim":"it is fast","supported_by":[]}]}"#,
            2,
        )
        .unwrap();
        assert_eq!(supported(&claims), (1, 2));
    }

    #[test]
    fn a_claim_naming_an_excerpt_that_was_not_shown_is_unsupported() {
        let claims =
            parse_claims(r#"{"claims":[{"claim":"x","supported_by":[3, 0]}]}"#, 2).unwrap();
        assert_eq!(supported(&claims), (0, 1));
    }

    #[test]
    fn prose_around_the_json_is_tolerated_and_garbage_is_an_error() {
        assert!(
            parse_claims("Here you go:\n```json\n{\"claims\":[]}\n```", 1)
                .unwrap()
                .is_empty()
        );
        assert!(parse_claims("no json here", 1).is_err());
    }
}
