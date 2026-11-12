//! Grouping gaps that are about the same thing into one hole.
//! Pure functions over stored vectors, so grouping costs no inference.

use crate::store::gaps::GapKind;

/// The floor under the linkage line, and the line itself where too little is recorded to measure
/// one. Under bge-m3 unrelated short queries score 0.45–0.6, so the measured `unrelated_line`
/// usually wins.
pub const GAP_LINK_AT: f32 = 0.55;

/// A base whose queries all sit close together would otherwise calibrate out of ever grouping.
pub const LINE_CEILING: f32 = 0.9;

/// Below this many pairs the sample describes the operator's week, not the embedder.
const MIN_PAIRS: usize = 400;

/// A group of one is a question; naming it would spend a model call restating it.
pub const MIN_CLUSTER: usize = 2;

/// Where "unrelated" stops in this base's own vectors: median + 3 × 1.4826 × MAD of the pair
/// cosines. Robust rather than a percentile, since the upper tail holds genuinely related pairs.
/// Rounded up to a hundredth so a jittering line does not re-key and re-name clusters each sweep.
pub fn unrelated_line(queries: &[Vec<f32>], artifacts: &[Vec<f32>]) -> Option<f32> {
    let mut cos: Vec<f32> = Vec::new();
    for i in 0..queries.len() {
        for j in i + 1..queries.len() {
            cos.push(cosine(&queries[i], &queries[j]));
        }
        for a in artifacts {
            cos.push(cosine(&queries[i], a));
        }
    }
    if cos.len() < MIN_PAIRS {
        return None;
    }
    cos.sort_by(f32::total_cmp);
    let median = cos[cos.len() / 2];
    let mut dev: Vec<f32> = cos.iter().map(|c| (c - median).abs()).collect();
    dev.sort_by(f32::total_cmp);
    let sigma = 1.4826 * dev[dev.len() / 2];
    Some(((median + 3.0 * sigma) * 100.0).ceil() / 100.0)
}

/// The measured line clamped to `[floor, LINE_CEILING]`; `measured <= 0` means unmeasured.
pub fn line_above(floor: f32, measured: f32) -> f32 {
    if measured <= 0.0 {
        floor
    } else {
        measured.clamp(floor, LINE_CEILING.max(floor))
    }
}

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

/// Single-linkage over cosine: two vectors join at `link_at`, transitively; groups ordered by first
/// member. Quadratic up to 4 × `MAX_OPEN_GAPS`; acceptable because it runs on the retention tick,
/// not a request.
pub fn cluster(vecs: &[&[f32]], link_at: f32) -> Vec<Vec<usize>> {
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
            if cosine(vecs[i], vecs[j]) >= link_at {
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

/// A cluster's name before, or without, a model naming it.
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
        let v = [
            [1.0f32, 0.0, 0.0],
            [0.9, 0.1, 0.0], // near 0
            [0.0, 1.0, 0.0],
            [0.0, 0.95, 0.05], // near 2
            [0.0, 0.0, 1.0],   // alone
        ];
        let v: Vec<&[f32]> = v.iter().map(|x| x.as_slice()).collect();
        assert_eq!(cluster(&v, 0.55), vec![vec![0, 1], vec![2, 3], vec![4]]);
        assert!(cluster(&[], 0.55).is_empty());
    }

    #[test]
    fn linkage_is_transitive() {
        // 0~1 and 1~2 but 0 and 2 are below the line: one cluster.
        let v = [[1.0f32, 0.0], [0.7, 0.7], [0.0, 1.0]];
        let v: Vec<&[f32]> = v.iter().map(|x| x.as_slice()).collect();
        assert_eq!(cluster(&v, 0.6), vec![vec![0, 1, 2]]);
    }

    fn pseudo(n: usize, dim: usize, spread: f32) -> Vec<Vec<f32>> {
        let mut state = 0x2545_f491u32;
        let mut next = move || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        };
        (0..n)
            .map(|_| {
                (0..dim)
                    .map(|d| next() * spread + if d == 0 { 1.0 } else { 0.0 })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn too_small_a_sample_leaves_the_floor_where_it_is() {
        assert_eq!(unrelated_line(&[], &[]), None);
        assert_eq!(
            unrelated_line(&pseudo(10, 64, 1.0), &pseudo(10, 64, 1.0)),
            None
        );
        assert_eq!(line_above(GAP_LINK_AT, 0.0), GAP_LINK_AT);
    }

    #[test]
    fn a_well_spread_embedder_measures_below_the_floor_and_keeps_it() {
        let m = unrelated_line(&pseudo(40, 64, 1.0), &[]).unwrap();
        assert_eq!(line_above(GAP_LINK_AT, m), GAP_LINK_AT);
    }

    #[test]
    fn an_embedder_that_puts_everything_close_together_raises_the_line() {
        let m = unrelated_line(&pseudo(40, 64, 0.15), &[]).unwrap();
        let t = line_above(GAP_LINK_AT, m);
        assert!(t > GAP_LINK_AT, "{t}");
        assert!(t <= LINE_CEILING, "{t}");
        assert_eq!((m * 100.0).fract(), 0.0, "{m}");
    }

    #[test]
    fn a_related_tail_does_not_drag_the_line_up() {
        // Unrelated pairs in a narrow band near 0.5 plus a few related ones near 0.9.
        let mut queries = pseudo(40, 64, 0.6);
        for _ in 0..10 {
            queries.push(queries[0].iter().map(|x| x + 0.01).collect());
        }
        let band = unrelated_line(&pseudo(40, 64, 0.6), &[]).unwrap();
        let tailed = unrelated_line(&queries, &[]).unwrap();
        assert!((tailed - band).abs() <= 0.1, "{band} vs {tailed}");
        assert!(tailed < 0.8, "{tailed}");
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
