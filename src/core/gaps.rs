//! Grouping the holes. A gap is a question the base could not answer or a
//! search judged to have no answer; two gaps about the same thing are one hole
//! and should be shown as one. Pure functions over stored vectors, so grouping
//! costs no inference and can be tested without any.

use crate::store::gaps::GapKind;

/// Cosine at or above which two gaps are the same hole. A constant with its
/// reasoning here rather than a setting: nothing has measured it yet, and the
/// roadmap's rule is that a default moves after the harness has run. 0.55 is
/// well above what unrelated questions score under the embedders engram is
/// run with, and below what two phrasings of one situation score.
pub const GAP_LINK_AT: f32 = 0.55;

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

/// Single-linkage over cosine: two vectors join at `link_at`, and joining is
/// transitive. Returns groups of indices, each sorted, ordered by their first
/// member. N is tens, so the quadratic pass is fine.
pub fn cluster(vecs: &[Vec<f32>], link_at: f32) -> Vec<Vec<usize>> {
    let n = vecs.len();
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(p: &mut [usize], i: usize) -> usize {
        let mut r = i;
        while p[r] != r {
            r = p[r];
        }
        let mut c = i;
        while p[c] != r {
            let next = p[c];
            p[c] = r;
            c = next;
        }
        r
    }
    for i in 0..n {
        for j in i + 1..n {
            if cosine(&vecs[i], &vecs[j]) >= link_at {
                let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                if a != b {
                    parent[a.max(b)] = a.min(b);
                }
            }
        }
    }
    let mut groups: std::collections::BTreeMap<usize, Vec<usize>> = Default::default();
    for i in 0..n {
        let r = find(&mut parent, i);
        groups.entry(r).or_default().push(i);
    }
    groups.into_values().collect()
}

/// Identity of a cluster: its members, and nothing else.
pub fn cluster_key(members: &[(GapKind, String)]) -> String {
    use sha2::{Digest, Sha256};
    let mut keys: Vec<String> = members
        .iter()
        .map(|(k, id)| format!("{}:{id}\n", k.as_str()))
        .collect();
    keys.sort();
    hex::encode(Sha256::digest(keys.concat().as_bytes()))
}

const STOP: &[&str] = &[
    "a", "an", "the", "and", "or", "of", "to", "in", "on", "for", "with", "how", "do", "i", "is",
    "it", "what", "can", "my", "me", "does", "why", "when", "are", "be", "this", "that", "from",
    "at", "by", "into", "was", "we", "you", "not", "no",
];

/// The three most frequent content words across the texts, or the first text
/// cut short. What a cluster is called before — or without — a model naming it.
pub fn terms_label(texts: &[&str]) -> String {
    let mut counts: std::collections::HashMap<String, usize> = Default::default();
    for t in texts {
        for w in t
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.len() > 2)
        {
            let w = w.to_lowercase();
            if !STOP.contains(&w.as_str()) {
                *counts.entry(w).or_default() += 1;
            }
        }
    }
    let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let words: Vec<String> = ranked.into_iter().take(3).map(|(w, _)| w).collect();
    if words.is_empty() {
        texts
            .first()
            .map(|t| t.chars().take(40).collect())
            .unwrap_or_default()
    } else {
        words.join(" · ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn near_vectors_group_and_far_ones_stand_alone() {
        let v = vec![
            vec![1.0, 0.0, 0.0],
            vec![0.9, 0.1, 0.0], // near 0
            vec![0.0, 1.0, 0.0],
            vec![0.0, 0.95, 0.05], // near 2
            vec![0.0, 0.0, 1.0],   // alone
        ];
        assert_eq!(cluster(&v, 0.55), vec![vec![0, 1], vec![2, 3], vec![4]]);
        assert!(cluster(&[], 0.55).is_empty());
    }

    #[test]
    fn linkage_is_transitive() {
        // 0~1 and 1~2 but 0 and 2 are below the line: one cluster.
        let v = vec![vec![1.0, 0.0], vec![0.7, 0.7], vec![0.0, 1.0]];
        assert_eq!(cluster(&v, 0.6), vec![vec![0, 1, 2]]);
    }

    #[test]
    fn a_key_depends_on_membership_and_not_on_order() {
        let a = vec![
            (GapKind::Ask, "1".to_string()),
            (GapKind::Search, "2".to_string()),
        ];
        let b = vec![
            (GapKind::Search, "2".to_string()),
            (GapKind::Ask, "1".to_string()),
        ];
        assert_eq!(cluster_key(&a), cluster_key(&b));
        assert_ne!(cluster_key(&a), cluster_key(&a[..1]));
    }

    #[test]
    fn a_terms_label_is_the_shared_content_words() {
        let l = terms_label(&[
            "how do I mount an E01 image",
            "mounting E01 images read only",
            "E01 mount fails",
        ]);
        assert!(l.contains("e01") && l.contains("mount"), "{l}");
        assert_eq!(terms_label(&["a of the"]), "a of the");
    }
}
