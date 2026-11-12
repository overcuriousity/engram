//! The server half of the vector background: sample the store, project the
//! vectors to 3D, hand the page a picture of its own contents.

/// Fixed so a refetch redraws the same store as the same cloud.
const SEED: u64 = 0x9E37_79B9_7F4A_7C15;

/// xorshift64*; `rand` would be a dependency for thirty lines.
struct Rng(u64);

impl Rng {
    fn next_f64(&mut self) -> f64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Standard normal via Box–Muller.
    fn gaussian(&mut self) -> f64 {
        let u1 = self.next_f64().max(1e-300);
        let u2 = self.next_f64();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }
}

/// The 95th percentile maps to the unit sphere; the tail is compressed smoothly into the band up to this.
const OUTLIER_CEILING: f32 = 1.25;

/// From a count, not `vectors.first()`, so one off-width leading vector cannot decide the cloud.
fn modal_dim(vectors: &[Vec<f32>]) -> usize {
    let mut counts: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for v in vectors.iter().filter(|v| !v.is_empty()) {
        *counts.entry(v.len()).or_default() += 1;
    }
    // Ties break on the wider vector, since `HashMap` order is not deterministic.
    counts
        .into_iter()
        .max_by_key(|&(dim, n)| (n, dim))
        .map(|(dim, _)| dim)
        .unwrap_or(0)
}

/// Fixed random projection (Johnson–Lindenstrauss), scaled so the bulk fills the unit sphere, rounded to 4 decimals.
/// Non-finite points are dropped: `serde_json` writes them as `null`, which throws in the client every frame.
pub fn project_3d(vectors: &[Vec<f32>]) -> Vec<[f32; 3]> {
    let dim = modal_dim(vectors);
    if dim == 0 {
        return vec![];
    }
    let mut rng = Rng(SEED);
    let matrix: Vec<[f64; 3]> = (0..dim)
        .map(|_| [rng.gaussian(), rng.gaussian(), rng.gaussian()])
        .collect();

    let mut out: Vec<[f32; 3]> = vectors
        .iter()
        .filter(|v| v.len() == dim)
        .map(|v| {
            let mut p = [0.0f64; 3];
            for (i, &x) in v.iter().enumerate() {
                p[0] += x as f64 * matrix[i][0];
                p[1] += x as f64 * matrix[i][1];
                p[2] += x as f64 * matrix[i][2];
            }
            [p[0] as f32, p[1] as f32, p[2] as f32]
        })
        .filter(|p| p.iter().all(|c| c.is_finite()))
        .collect();

    // Centred first: embeddings share a large common component, which would otherwise dominate the scale and the spin.
    if !out.is_empty() {
        let n = out.len() as f64;
        let mut mean = [0.0f64; 3];
        for p in &out {
            for (m, c) in mean.iter_mut().zip(p.iter()) {
                *m += *c as f64;
            }
        }
        for m in &mut mean {
            *m /= n;
        }
        for p in &mut out {
            for (c, m) in p.iter_mut().zip(mean.iter()) {
                *c -= *m as f32;
            }
        }
    }

    // The 95th percentile, not the maximum: projections concentrate around the mean.
    let mut radii: Vec<f32> = out
        .iter()
        .map(|p| (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt())
        .collect();
    radii.sort_by(|a, b| a.total_cmp(b));
    let scale = radii
        .get(((radii.len() as f32 * 0.95) as usize).min(radii.len().saturating_sub(1)))
        .copied()
        .unwrap_or(0.0);
    if scale <= 0.0 {
        return out;
    }
    for p in &mut out {
        let r = (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt() / scale;
        // The identity inside the bulk; the excess outside is folded into a thin band.
        let squashed = if r <= 1.0 {
            r
        } else {
            1.0 + (r - 1.0).tanh() * (OUTLIER_CEILING - 1.0)
        };
        let k = if r > 0.0 { squashed / r / scale } else { 0.0 };
        for c in p.iter_mut() {
            *c = (*c * k * 1e4).round() / 1e4;
        }
    }
    out
}

use crate::error::Result;
use crate::tenants::Tenant;
use axum::Json;
use axum::extract::Query;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
pub struct SampleResponse {
    pub points: Vec<[f32; 3]>,
    pub count: usize,
    /// Account, store generation and vector count; the client sends it back as `have`.
    /// `None` when the store could not be asked: an outage must not invalidate a likely-correct snapshot.
    pub tag: Option<String>,
    /// Set when `have` still matches; `points` is then empty, which otherwise means draw nothing.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub unchanged: bool,
}

/// `no-store`, never `max-age`: one URL answers per tenant and the browser cache outlives sign-out.
/// The tag, checked on every load, holds the client's snapshot instead.
fn no_store(body: SampleResponse) -> Response {
    (
        [(header::CACHE_CONTROL, "private, no-store".to_string())],
        Json(body),
    )
        .into_response()
}

#[derive(Debug, Deserialize, Default)]
pub struct SampleQuery {
    #[serde(default)]
    pub have: Option<String>,
}

/// The slug because `localStorage` is per browser, not per account; the revision because `--reindex` preserves the count.
/// A same-count recapture within one generation still reads as unchanged, which a decoration can live with.
fn tag_for(slug: &str, revision: Option<&str>, count: u64) -> String {
    match revision {
        Some(rev) => format!("{slug}:{rev}:{count}"),
        None => format!("{slug}:{count}"),
    }
}

/// Failures answer an empty cloud: a decoration must never take a page down.
/// A matching `?have=` is answered in a few bytes; the count behind the tag is cheap.
pub async fn sample(tenant: Tenant, Query(q): Query<SampleQuery>) -> Result<Response> {
    let cfg = &tenant.core.ui.background;
    let nothing = |tag: Option<String>| SampleResponse {
        points: vec![],
        count: 0,
        tag,
        unchanged: false,
    };
    if !cfg.enabled {
        // A real tag, so a client drops its cloud once and then settles on one tiny answer per load.
        return Ok(no_store(nothing(Some("off".into()))));
    }
    // Both cheap and independent, so they run together.
    let (count, revision) =
        match tokio::try_join!(tenant.core.vectors.count(), tenant.core.vectors.revision()) {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!(error = %e, "vector background count failed");
                // No tag rather than one that cannot match: a mismatch would wipe every client's snapshot during an outage.
                return Ok(no_store(nothing(None)));
            }
        };
    let tag = tag_for(&tenant.user.slug, revision.as_deref(), count);
    if q.have.as_deref() == Some(tag.as_str()) {
        return Ok(no_store(SampleResponse {
            points: vec![],
            count: count as usize,
            tag: Some(tag),
            unchanged: true,
        }));
    }
    let sampled = match tenant.core.vectors.sample(cfg.sample_size).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "vector background sample failed");
            return Ok(no_store(nothing(None)));
        }
    };
    let vectors: Vec<Vec<f32>> = sampled.into_iter().map(|(_, v)| v).collect();
    let points = project_3d(&vectors);
    // Tagged even when the projection is empty, so the client does not rescroll on every load.
    Ok(no_store(SampleResponse {
        count: points.len(),
        points,
        tag: Some(tag),
        unchanged: false,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn radius(p: &[f32; 3]) -> f32 {
        (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt()
    }

    #[test]
    fn a_reindex_at_the_same_size_still_changes_the_tag() {
        assert_ne!(
            tag_for("ada", Some("engram_v1"), 4200),
            tag_for("ada", Some("engram_v2"), 4200)
        );
    }

    #[test]
    fn two_accounts_of_the_same_size_do_not_share_a_tag() {
        assert_ne!(
            tag_for("ada", Some("engram_v1"), 4200),
            tag_for("bob", Some("engram_v1"), 4200)
        );
    }

    #[test]
    fn a_store_with_no_generation_still_gets_a_tag_that_tracks_its_size() {
        assert_eq!(tag_for("ada", None, 7), "ada:7");
        assert_ne!(tag_for("ada", None, 7), tag_for("ada", None, 8));
    }

    #[test]
    fn the_projection_is_deterministic() {
        let vs = vec![vec![0.1, -0.2, 0.3], vec![0.5, 0.5, 0.5]];
        assert_eq!(project_3d(&vs), project_3d(&vs));
    }

    #[test]
    fn no_point_escapes_the_outlier_ceiling() {
        let vs: Vec<Vec<f32>> = (0..50)
            .map(|i| vec![i as f32 * 0.01, -0.3, (i as f32 * 0.02).sin()])
            .collect();
        for p in project_3d(&vs) {
            let r = radius(&p);
            assert!(
                r <= OUTLIER_CEILING + 1e-4,
                "point escaped the ceiling: {p:?}"
            );
        }
    }

    #[test]
    fn one_far_outlier_does_not_shrink_the_rest_to_a_speck() {
        let mut vs: Vec<Vec<f32>> = (0..200)
            .map(|i| vec![(i as f32 * 0.7).sin(), (i as f32 * 1.3).cos(), 0.05])
            .collect();
        vs.push(vec![900.0, -900.0, 900.0]);
        let out = project_3d(&vs);
        let bulk = &out[..out.len() - 1];
        let mean = bulk.iter().map(radius).sum::<f32>() / bulk.len() as f32;
        assert!(mean > 0.2, "the cloud collapsed around the outlier: {mean}");
    }

    #[test]
    fn a_cloud_that_shares_an_offset_is_still_drawn_around_the_origin() {
        let vs: Vec<Vec<f32>> = (0..300)
            .map(|i| {
                let t = i as f32;
                vec![
                    40.0 + (t * 0.7).sin() * 0.1,
                    -25.0 + (t * 1.3).cos() * 0.1,
                    60.0 + (t * 0.3).sin() * 0.1,
                ]
            })
            .collect();
        let out = project_3d(&vs);
        let n = out.len() as f32;
        for axis in 0..3 {
            let mean = out.iter().map(|p| p[axis]).sum::<f32>() / n;
            assert!(
                mean.abs() < 0.05,
                "axis {axis} sits at {mean}, not around the origin"
            );
        }
        let mean_radius = out.iter().map(radius).sum::<f32>() / n;
        assert!(mean_radius > 0.2, "the cloud collapsed: {mean_radius}");
    }

    #[test]
    fn empty_and_zero_width_inputs_yield_no_points() {
        assert!(project_3d(&[]).is_empty());
        assert!(project_3d(&[vec![]]).is_empty());
    }

    #[test]
    fn a_vector_of_the_wrong_width_is_dropped_not_silently_misplaced() {
        let vs = vec![vec![0.1, 0.2], vec![0.1, 0.2, 0.3], vec![0.4, 0.5, 0.6]];
        assert_eq!(project_3d(&vs).len(), 2);
    }

    #[test]
    fn the_width_comes_from_the_majority_not_from_position_zero() {
        let mut vs = vec![vec![]];
        vs.extend((0..10).map(|i| vec![i as f32, 0.5, -0.25]));
        assert_eq!(project_3d(&vs).len(), 10);

        let mut vs = vec![vec![0.1, 0.2]];
        vs.extend((0..10).map(|i| vec![i as f32, 0.5, -0.25]));
        assert_eq!(project_3d(&vs).len(), 10);
    }

    #[test]
    fn nothing_non_finite_reaches_the_wire() {
        let vs = vec![
            vec![f32::INFINITY, 0.0, 0.0],
            vec![f32::NAN, 1.0, 2.0],
            vec![0.3, -0.1, 0.9],
        ];
        let out = project_3d(&vs);
        assert_eq!(out.len(), 1);
        assert!(out[0].iter().all(|c| c.is_finite()));
    }
}
