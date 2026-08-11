use std::collections::BTreeSet;

pub const SIGNATURE_SIZE: usize = 128;

const SHINGLE_WORDS: usize = 5;

fn hash(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

pub fn signature(text: &str) -> Vec<u64> {
    let words: Vec<String> = text.split_whitespace().map(|w| w.to_lowercase()).collect();
    if words.is_empty() {
        return Vec::new();
    }

    let mut smallest: BTreeSet<u64> = BTreeSet::new();
    let step = SHINGLE_WORDS.min(words.len());
    for w in words.windows(step) {
        smallest.insert(hash(&w.join(" ")));
        if smallest.len() > SIGNATURE_SIZE {
            let largest = *smallest.iter().next_back().expect("non-empty");
            smallest.remove(&largest);
        }
    }
    smallest.into_iter().collect()
}

pub fn similarity(a: &[u64], b: &[u64]) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let k = SIGNATURE_SIZE.min(a.len()).min(b.len());
    let mut union: Vec<u64> = a.iter().chain(b.iter()).copied().collect();
    union.sort_unstable();
    union.dedup();
    union.truncate(k);

    let shared = union
        .iter()
        .filter(|h| a.binary_search(h).is_ok() && b.binary_search(h).is_ok())
        .count();
    shared as f64 / k as f64
}

pub fn encode(sig: &[u64]) -> String {
    serde_json::to_string(sig).unwrap_or_else(|_| "[]".into())
}

pub fn decode(s: &str) -> Vec<u64> {
    serde_json::from_str(s).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(n: usize) -> String {
        (0..n)
            .map(|i| format!("line {i} of a reference document about filesystems"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn identical_text_is_perfectly_similar() {
        let a = signature(&doc(200));
        let b = signature(&doc(200));
        assert_eq!(a, b, "the signature must be deterministic");
        assert!((similarity(&a, &b) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn one_changed_byte_still_reads_as_the_same_document() {
        let original = doc(200);
        let edited = original.replacen("filesystems", "filesystem", 1);
        assert_ne!(original, edited);
        let s = similarity(&signature(&original), &signature(&edited));
        assert!(s > 0.95, "one edit dropped similarity to {s}");
    }

    #[test]
    fn unrelated_text_is_not_similar() {
        let a = signature(&doc(200));
        let b = signature(
            &(0..200)
                .map(|i| format!("entirely different sentence number {i} concerning pastry"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let s = similarity(&a, &b);
        assert!(s < 0.2, "unrelated documents scored {s}");
    }

    #[test]
    fn a_document_shorter_than_one_shingle_still_has_a_signature() {
        let sig = signature("just three words");
        assert!(!sig.is_empty());
        assert!(similarity(&sig, &signature("wholly other words here")) < 1.0);
    }

    #[test]
    fn an_empty_signature_is_never_similar_to_anything() {
        assert_eq!(similarity(&[], &signature("some text")), 0.0);
        assert_eq!(similarity(&[], &[]), 0.0);
    }

    #[test]
    fn signatures_round_trip_through_the_column_encoding() {
        let sig = signature(&doc(50));
        assert_eq!(decode(&encode(&sig)), sig);
        assert!(
            decode("not json").is_empty(),
            "a corrupt column must not panic"
        );
    }

    #[test]
    fn the_signature_is_bounded_however_long_the_document() {
        assert!(signature(&doc(10_000)).len() <= SIGNATURE_SIZE);
    }
}
