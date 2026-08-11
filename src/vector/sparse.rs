const K1: f32 = 1.2;

const LENGTH_NORM: f32 = 1.0;

const MAX_TOKEN_LEN: usize = 40;

const CONNECTORS: [char; 4] = ['-', '_', '.', '/'];

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SparseVector {
    pub indices: Vec<u32>,
    pub values: Vec<f32>,
}

impl SparseVector {
    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }
}

pub fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in text.split(|c: char| !c.is_alphanumeric() && !CONNECTORS.contains(&c)) {
        let token = raw.trim_matches(|c| CONNECTORS.contains(&c)).to_lowercase();
        if token.is_empty() || token.len() > MAX_TOKEN_LEN {
            continue;
        }
        let compound = token.contains(|c| CONNECTORS.contains(&c));
        out.push(token.clone());
        if compound {
            for part in token.split(|c| CONNECTORS.contains(&c)) {
                if !part.is_empty() && part.len() <= MAX_TOKEN_LEN {
                    out.push(part.to_string());
                }
            }
        }
    }
    out
}

pub fn term_id(token: &str) -> u32 {
    let digest = <sha2::Sha256 as sha2::Digest>::digest(token.as_bytes());
    u32::from_le_bytes([digest[0], digest[1], digest[2], digest[3]])
}

pub fn encode_document(text: &str) -> SparseVector {
    let mut counts: std::collections::HashMap<u32, f32> = std::collections::HashMap::new();
    for token in tokenize(text) {
        *counts.entry(term_id(&token)).or_insert(0.0) += 1.0;
    }

    let mut indices = Vec::with_capacity(counts.len());
    let mut values = Vec::with_capacity(counts.len());
    let mut pairs: Vec<(u32, f32)> = counts.into_iter().collect();
    pairs.sort_unstable_by_key(|(id, _)| *id);
    for (id, tf) in pairs {
        indices.push(id);
        values.push(saturate(tf));
    }
    SparseVector { indices, values }
}

pub fn encode_query(text: &str) -> SparseVector {
    let mut ids: Vec<u32> = tokenize(text).iter().map(|t| term_id(t)).collect();
    ids.sort_unstable();
    ids.dedup();
    let values = vec![1.0; ids.len()];
    SparseVector {
        indices: ids,
        values,
    }
}

fn saturate(tf: f32) -> f32 {
    tf * (K1 + 1.0) / (tf + K1 * LENGTH_NORM)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(text: &str) -> Vec<String> {
        tokenize(text)
    }

    #[test]
    fn a_flag_survives_its_leading_dashes_and_is_also_split() {
        let t = tokens("run it with --dry-run first");
        assert!(t.contains(&"dry-run".to_string()), "{t:?}");
        assert!(t.contains(&"dry".to_string()), "{t:?}");
        assert!(t.contains(&"run".to_string()), "{t:?}");
        assert!(
            !t.contains(&"--dry-run".to_string()),
            "leading dashes are punctuation: {t:?}"
        );
    }

    #[test]
    fn identifiers_keep_their_internal_punctuation() {
        let t = tokens("edit src/vector/sparse.rs and file.rs");
        assert!(t.contains(&"src/vector/sparse.rs".to_string()), "{t:?}");
        assert!(t.contains(&"sparse".to_string()), "{t:?}");
        assert!(t.contains(&"file.rs".to_string()), "{t:?}");
        assert!(t.contains(&"rs".to_string()), "{t:?}");
    }

    #[test]
    fn alphanumeric_identifiers_are_one_token() {
        assert_eq!(
            tokens("mounting an E01 image"),
            vec!["mounting", "an", "e01", "image"]
        );
    }

    #[test]
    fn case_does_not_matter() {
        assert_eq!(tokens("Ext4 EXT4 ext4"), vec!["ext4", "ext4", "ext4"]);
    }

    #[test]
    fn punctuation_and_whitespace_never_become_terms() {
        assert!(tokens("   ...  --  //  ").is_empty());
        assert!(tokens("").is_empty());
    }

    #[test]
    fn an_overlong_token_is_dropped() {
        let blob = "a".repeat(MAX_TOKEN_LEN + 1);
        assert!(tokens(&blob).is_empty(), "an unbounded token was indexed");
        let ok = "a".repeat(MAX_TOKEN_LEN);
        assert_eq!(tokens(&ok), vec![ok]);
    }

    #[test]
    fn term_ids_are_stable_and_distinct() {
        assert_eq!(term_id("e01"), term_id("e01"));
        assert_ne!(term_id("e01"), term_id("e02"));
    }

    #[test]
    fn repeated_terms_saturate_rather_than_accumulate() {
        let once = encode_document("alpha");
        let many = encode_document("alpha alpha alpha alpha alpha alpha alpha alpha");
        let id = term_id("alpha");
        let w = |v: &SparseVector| v.values[v.indices.iter().position(|i| *i == id).unwrap()];

        assert!(w(&many) > w(&once), "frequency should count for something");
        assert!(
            w(&many) < 8.0 * w(&once),
            "eight mentions must not be worth eight times one"
        );
        assert!(w(&many) < K1 + 1.0, "saturation has an upper bound");
    }

    #[test]
    fn a_document_vector_is_sorted_and_deduplicated() {
        let v = encode_document("beta alpha beta");
        assert_eq!(v.indices.len(), 2, "a repeated term is one dimension");
        assert_eq!(v.indices.len(), v.values.len());
        assert!(
            v.indices.windows(2).all(|w| w[0] < w[1]),
            "indices must be sorted: {:?}",
            v.indices
        );
    }

    #[test]
    fn a_query_weighs_presence_not_frequency() {
        let v = encode_query("alpha alpha beta");
        assert_eq!(v.indices.len(), 2);
        assert!(v.values.iter().all(|x| *x == 1.0), "{:?}", v.values);
    }

    #[test]
    fn a_query_of_pure_punctuation_is_empty_rather_than_meaningless() {
        assert!(encode_query("?? ...").is_empty());
        assert!(!encode_query("ext4").is_empty());
    }

    #[test]
    fn the_queries_dense_search_gets_wrong_share_terms_with_their_answer() {
        for (query, document) in [
            ("E01", "mounting an E01 image with ewfmount"),
            ("--dry-run", "pass --dry-run to preview the changes"),
            ("ext4", "the ext4 journal replays on mount"),
            ("SIGSEGV", "the process died with SIGSEGV in the parser"),
            ("qdrant_client", "the qdrant_client crate speaks gRPC only"),
            ("/etc/fstab", "entries in /etc/fstab are read at boot"),
        ] {
            let q = encode_query(query);
            let d = encode_document(document);
            assert!(!q.is_empty(), "query `{query}` produced no terms");
            let shared = q.indices.iter().filter(|i| d.indices.contains(i)).count();
            assert!(
                shared > 0,
                "`{query}` shares no term with `{document}`, so lexical search cannot find it"
            );
        }
    }

    #[test]
    fn an_unrelated_document_shares_nothing_with_the_query() {
        let q = encode_query("E01");
        let d = encode_document("configuring a printer on Windows");
        assert!(q.indices.iter().all(|i| !d.indices.contains(i)));
    }
}
