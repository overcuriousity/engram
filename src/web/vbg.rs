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

/// Project dense vectors to 3D with a fixed random projection
/// (Johnson–Lindenstrauss: a random matrix preserves neighbourhoods well
/// enough that clusters still read as clusters), then scale so the farthest
/// point sits on the unit sphere — the client draws in a fixed-size box and
/// never renormalizes. Coordinates are rounded to 4 decimals: the wire size
/// halves and no eye can tell.
pub fn project_3d(vectors: &[Vec<f32>]) -> Vec<[f32; 3]> {
    let Some(dim) = vectors.first().map(|v| v.len()) else {
        return vec![];
    };
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
        .collect();

    let max_r = out
        .iter()
        .map(|p| (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt())
        .fold(0.0f32, f32::max);
    if max_r > 0.0 {
        for p in &mut out {
            for c in p.iter_mut() {
                *c = (*c / max_r * 1e4).round() / 1e4;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_projection_is_deterministic() {
        // A refetch redraws the same cloud: the seed is fixed, so two calls
        // over the same vectors agree exactly.
        let vs = vec![vec![0.1, -0.2, 0.3], vec![0.5, 0.5, 0.5]];
        assert_eq!(project_3d(&vs), project_3d(&vs));
    }

    #[test]
    fn every_point_lands_inside_the_unit_sphere() {
        let vs: Vec<Vec<f32>> = (0..50)
            .map(|i| vec![i as f32 * 0.01, -0.3, (i as f32 * 0.02).sin()])
            .collect();
        for p in project_3d(&vs) {
            let r = (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt();
            assert!(r <= 1.0 + 1e-4, "point escaped the unit sphere: {p:?}");
        }
    }

    #[test]
    fn empty_and_zero_width_inputs_yield_no_points() {
        assert!(project_3d(&[]).is_empty());
        assert!(project_3d(&[vec![]]).is_empty());
    }

    #[test]
    fn a_vector_of_the_wrong_width_is_dropped_not_silently_misplaced() {
        // A changed embedding model mid-flight must not smear points across
        // the picture; only vectors matching the first one's width count.
        let vs = vec![vec![0.1, 0.2], vec![0.1, 0.2, 0.3]];
        assert_eq!(project_3d(&vs).len(), 1);
    }
}
