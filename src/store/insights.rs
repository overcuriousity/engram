//! What this memory is like, as aggregates over tables that already exist.
//!
//! No new table, no sweep, and no model call: every figure here is a `COUNT`
//! or a `GROUP BY` over rows some other part of the system already writes.
//! That constraint is the one at the top of `ROADMAP.md`, and it is what makes
//! this page cheap enough to open whenever.
//!
//! Retrieval is deliberately *not* computed here. `feedback_stats` already
//! reads recall@10 and MRR off the ranks judged searches actually gave, and a
//! second computation over the same rows is how two pages come to report two
//! different numbers for one thing.

use crate::error::Result;
use crate::store::Store;

/// How much is held, and how densely.
#[derive(Debug, Default, Clone)]
pub struct Held {
    pub corpora: i64,
    pub artifacts: i64,
    pub segments: i64,
    /// Artifacts a model wrote — merged, or synthesized from a pursuit. Said
    /// beside the total because "rewriting is earned" is a claim this number
    /// either supports or does not.
    pub synthesized: i64,
}

/// One bar of a distribution: a label and how many fall in it.
#[derive(Debug, Clone)]
pub struct Bucket {
    pub label: &'static str,
    pub count: i64,
}

impl Store {
    /// Counts, in one round trip each. Cheap enough that the page needs no
    /// cache and no staleness to explain.
    pub async fn held(&self) -> Result<Held> {
        Ok(Held {
            corpora: sqlx::query_scalar("SELECT count(*) FROM corpora")
                .fetch_one(&self.pool)
                .await?,
            artifacts: sqlx::query_scalar("SELECT count(*) FROM artifacts WHERE status = 'active'")
                .fetch_one(&self.pool)
                .await?,
            segments: sqlx::query_scalar("SELECT count(*) FROM segments")
                .fetch_one(&self.pool)
                .await?,
            synthesized: sqlx::query_scalar(
                "SELECT count(*) FROM artifacts \
                 WHERE status = 'active' AND provenance != 'captured'",
            )
            .fetch_one(&self.pool)
            .await?,
        })
    }

    /// What is fading, bucketed.
    ///
    /// Accessibility decays lazily: the stored `activation` is the value as of
    /// `activated_at`, and what it is *now* depends on how long ago that was.
    /// The decay is applied here rather than in SQL because the exponential
    /// needs `pow`, which SQLite only has when it was built with
    /// `SQLITE_ENABLE_MATH_FUNCTIONS` — a build flag this project does not get
    /// to assume, and whose absence is a 500 on a page rather than a wrong
    /// number.
    ///
    /// Two columns over the active artifacts is a small read, and it is the
    /// same arithmetic the query path applies, so this says what a search
    /// would find rather than what was last written down.
    ///
    /// Four bands rather than a histogram: the question is "what is falling
    /// out of reach", which the bottom band answers. The shape of the curve is
    /// not a question anybody has.
    pub async fn fading(&self, half_life_secs: i64, now: i64) -> Result<Vec<Bucket>> {
        let rows: Vec<(f64, i64)> = sqlx::query_as(
            "SELECT activation, activated_at FROM artifacts WHERE status = 'active'",
        )
        .fetch_all(&self.pool)
        .await?;
        let half_life = half_life_secs.max(1) as f64;
        let (mut reachable, mut settling, mut fading, mut gone) = (0i64, 0i64, 0i64, 0i64);
        for (activation, at) in rows {
            let age = (now - at).max(0) as f64;
            let a = activation * 0.5f64.powf(age / half_life);
            match a {
                _ if a >= 0.75 => reachable += 1,
                _ if a >= 0.40 => settling += 1,
                _ if a >= 0.15 => fading += 1,
                _ => gone += 1,
            }
        }
        Ok(vec![
            Bucket {
                label: "reachable",
                count: reachable,
            },
            Bucket {
                label: "settling",
                count: settling,
            },
            Bucket {
                label: "fading",
                count: fading,
            },
            Bucket {
                label: "out of reach",
                count: gone,
            },
        ])
    }
}
