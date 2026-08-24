//! The server half of the vector background: sample the store, project the
//! vectors to 3D, hand the page a picture of its own contents.

/// Fixed seed: the projection must be identical on every request, or a
/// refetch would redraw the same store as a different cloud.
const SEED: u64 = 0x9E37_79B9_7F4A_7C15;

/// xorshift64*, just enough PRNG for a projection matrix. `rand` would do
/// it, at the price of a dependency for thirty lines.
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

/// How far past the bulk a point is allowed to sit.
///
/// The scale below puts the 95th percentile on the unit sphere rather than the
/// single farthest point, so the cloud fills its box instead of collapsing to
/// a speck around one outlier. The tail then has to go somewhere: hard-clamped
/// it would pile a visible shell of points onto the sphere, so it is
/// compressed smoothly into the shim between 1 and this.
const OUTLIER_CEILING: f32 = 1.25;

/// The width to project at: the one most of the vectors share.
///
/// Taken from a count rather than from `vectors.first()`, which let position
/// zero decide the fate of the whole cloud — one empty or off-width leading
/// vector either returned nothing at all or filtered out every good vector
/// behind it, for the full length of a client's cache window and with no log
/// line to say why.
fn modal_dim(vectors: &[Vec<f32>]) -> usize {
    let mut counts: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for v in vectors.iter().filter(|v| !v.is_empty()) {
        *counts.entry(v.len()).or_default() += 1;
    }
    // Ties break on the wider vector, so the choice is at least deterministic
    // across runs — `HashMap` iteration order is not.
    counts
        .into_iter()
        .max_by_key(|&(dim, n)| (n, dim))
        .map(|(dim, _)| dim)
        .unwrap_or(0)
}

/// Project dense vectors to 3D with a fixed random projection
/// (Johnson–Lindenstrauss: a random matrix preserves neighbourhoods well
/// enough that clusters still read as clusters), then scale so the bulk of the
/// cloud fills the unit sphere — the client draws in a fixed-size box and
/// never renormalizes. Coordinates are rounded to 4 decimals: the wire size
/// halves and no eye can tell.
///
/// Nothing non-finite leaves here. `serde_json` writes a non-finite `f32` as
/// `null`, and the client's projection turns one `null` into an exception on
/// every animation frame for the life of the page — so a point that is not
/// finite is dropped at the source instead.
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

    // The 95th percentile, not the maximum. A random projection of
    // high-dimensional vectors concentrates hard around its mean, so the
    // farthest point sits well outside the bulk and dividing by it drew the
    // whole store as a small fuzzy ball in the middle of an empty frame.
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
        // Inside the bulk this is the identity; outside it, the excess is
        // folded into a thin band so the tail stays visible as a tail.
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

use crate::auth::Identity;
use crate::error::Result;
use crate::web::state::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// How long a snapshot that could not be taken is allowed to stand.
///
/// The good answer is cached for the configured refresh window, but an outage
/// must not blank the backdrop for six hours behind a browser cache nobody can
/// reach into. A minute is long enough to stop a reload storm and short enough
/// that the picture comes back on its own.
const FAILURE_MAX_AGE: u64 = 60;

#[derive(Serialize)]
pub struct SampleResponse {
    pub points: Vec<[f32; 3]>,
    pub count: usize,
    /// The client caches a snapshot for this long; carried in the response
    /// so the config value never needs a second door.
    pub refresh_secs: u64,
}

/// `private`, never `public`: the cloud is a projection of one installation's
/// own contents, and a shared cache has no business holding it.
fn cached(body: SampleResponse, max_age: u64) -> Response {
    (
        [(header::CACHE_CONTROL, format!("private, max-age={max_age}"))],
        Json(body),
    )
        .into_response()
}

/// The backdrop's one door. A failure answers an empty cloud rather than an
/// error: the picture is decorative, and a decoration must never take a page
/// down with it.
///
/// Every answer carries a `Cache-Control`, which is what actually holds the
/// budget the module claims. The client's `localStorage` snapshot is the first
/// line and it is not a guarantee — a private window, a full quota or a
/// storage-blocking setting makes `setItem` throw, and without a header on the
/// response every page load in that browser would re-run a scroll of
/// `sample_size` points with their dense vectors attached.
pub async fn sample(State(st): State<AppState>, _id: Identity) -> Result<Response> {
    let cfg = &st.core.ui.background;
    let empty = || SampleResponse {
        points: vec![],
        count: 0,
        refresh_secs: cfg.refresh_secs,
    };
    if !cfg.enabled {
        // Cached like any other answer: an operator who turned the backdrop
        // off should not go on paying a request per page load for it.
        return Ok(cached(empty(), cfg.refresh_secs));
    }
    let sampled = match st.core.vectors.sample(cfg.sample_size).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "vector background sample failed");
            return Ok(cached(empty(), FAILURE_MAX_AGE));
        }
    };
    let vectors: Vec<Vec<f32>> = sampled.into_iter().map(|(_, v)| v).collect();
    let points = project_3d(&vectors);
    Ok(cached(
        SampleResponse {
            count: points.len(),
            points,
            refresh_secs: cfg.refresh_secs,
        },
        cfg.refresh_secs,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn radius(p: &[f32; 3]) -> f32 {
        (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt()
    }

    #[test]
    fn the_projection_is_deterministic() {
        // A refetch redraws the same cloud: the seed is fixed, so two calls
        // over the same vectors agree exactly.
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
        // The whole reason the scale is a percentile. With a plain `max` the
        // bulk below divided by the outlier's radius and drew as a dot.
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
    fn empty_and_zero_width_inputs_yield_no_points() {
        assert!(project_3d(&[]).is_empty());
        assert!(project_3d(&[vec![]]).is_empty());
    }

    #[test]
    fn a_vector_of_the_wrong_width_is_dropped_not_silently_misplaced() {
        // A changed embedding model mid-flight must not smear points across
        // the picture; only vectors matching the modal width count.
        let vs = vec![vec![0.1, 0.2], vec![0.1, 0.2, 0.3], vec![0.4, 0.5, 0.6]];
        assert_eq!(project_3d(&vs).len(), 2);
    }

    #[test]
    fn the_width_comes_from_the_majority_not_from_position_zero() {
        // An empty or off-width vector sorting first used to decide for the
        // whole page: `dim == 0` returned nothing, and a short first vector
        // filtered out every good one behind it.
        let mut vs = vec![vec![]];
        vs.extend((0..10).map(|i| vec![i as f32, 0.5, -0.25]));
        assert_eq!(project_3d(&vs).len(), 10);

        let mut vs = vec![vec![0.1, 0.2]];
        vs.extend((0..10).map(|i| vec![i as f32, 0.5, -0.25]));
        assert_eq!(project_3d(&vs).len(), 10);
    }

    #[test]
    fn nothing_non_finite_reaches_the_wire() {
        // `serde_json` writes a non-finite `f32` as `null`, and one `null`
        // throws inside the client's draw loop twelve times a second.
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
