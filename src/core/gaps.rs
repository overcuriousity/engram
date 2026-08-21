//! Grouping the holes. A gap is a question the base could not answer or a
//! search judged to have no answer; two gaps about the same thing are one hole
//! and should be shown as one. Pure functions over stored vectors, so grouping
//! costs no inference and can be tested without any.

use crate::store::gaps::GapKind;

/// The floor under the linkage line, and the line itself on a base with too
/// little recorded to measure one.
///
/// It used to be the line, on the claim that 0.55 is "well above what unrelated
/// questions score under the embedders engram is run with". That was a guess
/// about geometry, and the wrong kind: under bge-m3 — the default — two
/// unrelated short queries routinely land in 0.45–0.6, and with single linkage
/// being transitive a few dozen of them chain into one group with one name over
/// holes that have nothing to do with each other. What replaces the guess is a
/// measurement of the same quantity from the base's own recorded queries; see
/// `link_threshold`.
pub const GAP_LINK_AT: f32 = 0.55;

/// The line is never raised past this. A base whose queries genuinely do all sit
/// close together — one operator, one narrow subject — would otherwise calibrate
/// its way out of ever grouping anything.
const GAP_LINK_CEILING: f32 = 0.9;

/// Below this many sampled vectors the sample describes the operator's week
/// rather than the embedder, and the floor stands unmeasured.
const MIN_CALIBRATION: usize = 30;

/// The share of sampled pairs the line is put above. Almost every pair of two
/// recorded queries is a pair of unrelated ones, so the 99th percentile of that
/// distribution is about where "unrelated" stops — high enough that ordinary
/// noise cannot chain, low enough to leave the genuinely-close band above it.
const UNRELATED_PERCENTILE: f64 = 0.99;

/// The smallest group worth a name. A group of one is a question, and naming it
/// is a model call spent restating it; an ungrouped gap already shows on the
/// capture page under its own words.
pub const MIN_CLUSTER: usize = 2;

/// The cosine at or above which two gaps are the same hole, measured from the
/// vectors this base has actually recorded.
///
/// Sampled from every recorded query rather than from the gaps alone, because
/// what has to be measured is where *unrelated* questions land: the pairs in a
/// broad sample are overwhelmingly unrelated, so their upper tail is the line.
/// The result is rounded up to a hundredth and clamped to
/// `[GAP_LINK_AT, GAP_LINK_CEILING]` — rounded because a line that moved by
/// 0.001 between sweeps would re-key clusters that had not changed and pay for
/// naming each of them again, and clamped because both ends of the measurement
/// have a failure mode.
///
/// What this still does not measure is whether the groups it produces are the
/// ones an operator would have drawn. That needs labels, and the operator's own
/// actions on a group — dismissing one member against dismissing all of them —
/// are the labels to gather; scoring linkage against them belongs with the rest
/// of the harness, not here.
pub fn link_threshold(sample: &[Vec<f32>]) -> f32 {
    if sample.len() < MIN_CALIBRATION {
        return GAP_LINK_AT;
    }
    let mut cos: Vec<f32> = Vec::with_capacity(sample.len() * (sample.len() - 1) / 2);
    for i in 0..sample.len() {
        for j in i + 1..sample.len() {
            cos.push(cosine(&sample[i], &sample[j]));
        }
    }
    if cos.is_empty() {
        return GAP_LINK_AT;
    }
    cos.sort_by(f32::total_cmp);
    let at = ((cos.len() - 1) as f64 * UNRELATED_PERCENTILE).round() as usize;
    // Up, never down: rounding a measured percentile down would put the line
    // back inside the band it was measured to sit above.
    let measured = (cos[at] * 100.0).ceil() / 100.0;
    measured.clamp(GAP_LINK_AT, GAP_LINK_CEILING)
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

/// Single-linkage over cosine: two vectors join at `link_at`, and joining is
/// transitive. Returns groups of indices, each sorted, ordered by their first
/// member.
///
/// N is bounded by `store::gaps::MAX_OPEN_GAPS` *per kind*, and there are four
/// kinds, so the quadratic pass is up to four times as many comparisons as that
/// constant reads like — two million of them at today's numbers, on the
/// retention tick. That is deliberate and it is the ceiling: it runs on a timer
/// rather than on a request, nothing waits for it, and cutting the cap to make
/// one number match the other would cost the operator gaps on the page to buy
/// back time nobody is holding.
///
/// Borrowed, not owned: the caller holds the vectors it read out of the store,
/// and copying a thousand of them to pass them in was four million floats moved
/// for nothing.
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

    /// Deterministic stand-in for an embedder's output: `spread` 1.0 is
    /// well-distributed directions, 0.0 is every vector on top of one axis.
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
        // Nothing to measure from is not a licence to move the line: a base with
        // three recorded queries would otherwise calibrate off three numbers.
        assert_eq!(link_threshold(&[]), GAP_LINK_AT);
        assert_eq!(
            link_threshold(&pseudo(MIN_CALIBRATION - 1, 64, 1.0)),
            GAP_LINK_AT
        );
    }

    #[test]
    fn a_well_spread_embedder_measures_below_the_floor_and_keeps_it() {
        // Unrelated directions in 64 dimensions score far below 0.55, so the
        // measurement agrees with the floor and the floor stands.
        assert_eq!(link_threshold(&pseudo(60, 64, 1.0)), GAP_LINK_AT);
    }

    #[test]
    fn an_embedder_that_puts_everything_close_together_raises_the_line() {
        // The failure the constant could not see: where unrelated queries
        // routinely score 0.9, linking at 0.55 chains all of them into one
        // group. The line moves up to sit above what was measured, and stops at
        // the ceiling so grouping does not become impossible.
        let t = link_threshold(&pseudo(60, 64, 0.15));
        assert!(t > GAP_LINK_AT, "{t}");
        assert!(t <= GAP_LINK_CEILING, "{t}");
        // Rounded to a hundredth, so a line that moves at all moves visibly —
        // a threshold wobbling by 0.001 between sweeps would re-key unchanged
        // clusters and pay to name each of them again.
        assert_eq!((t * 100.0).fract(), 0.0, "{t}");
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
