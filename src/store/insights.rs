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
            // Named, not negated. `!= 'captured'` swept in `passage` rows,
            // which are the document's own words verbatim — the opposite of
            // what this counts. `Provenance::is_model_written` is the
            // predicate, and these are its two values.
            synthesized: sqlx::query_scalar(
                "SELECT count(*) FROM artifacts \
                 WHERE status = 'active' AND provenance IN ('merged', 'synthesized')",
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
        // A non-positive half-life turns decay off — the same reading
        // `links::decayed` gives it, and the one the query path acts on.
        // Clamping it to a second instead said the opposite: every artifact
        // computed to ~0 and the whole base reported as out of reach, while
        // search treated all of it as fully activated.
        let (mut reachable, mut settling, mut fading, mut gone) = (0i64, 0i64, 0i64, 0i64);
        for (activation, at) in rows {
            let a = match half_life_secs > 0 {
                true => {
                    let age = (now - at).max(0) as f64;
                    activation * 0.5f64.powf(age / half_life_secs as f64)
                }
                false => activation,
            };
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::artifacts::{NewArtifact, Provenance};

    fn chunk(text: &str) -> NewArtifact {
        NewArtifact {
            ordinal: 0,
            text: text.into(),
            corpus_span: None,
            title: None,
            category: None,
            tags: vec![],
            segment_idx: None,
            caveats: vec![],
        }
    }

    /// "Written by a model" is the thesis of the application as a number, and
    /// a passage is the document's own words. Counting by negation put every
    /// passage on the model's side of that line.
    #[tokio::test]
    async fn a_passage_is_not_something_a_model_wrote() {
        let store = Store::memory().await.unwrap();
        let src = store.insert_corpus("raw", "web", None).await.unwrap();
        store
            .insert_artifacts(&src.id, &[chunk("the operator's own paste")])
            .await
            .unwrap();
        store
            .insert_artifacts_with_provenance(
                &src.id,
                &[chunk("a paragraph lifted verbatim")],
                Provenance::Passage,
            )
            .await
            .unwrap();
        store
            .insert_artifacts_with_provenance(
                &src.id,
                &[chunk("a rewrite")],
                Provenance::Synthesized,
            )
            .await
            .unwrap();

        let held = store.held().await.unwrap();
        assert_eq!(held.artifacts, 3);
        assert_eq!(
            held.synthesized, 1,
            "only the rewrite was written by a model"
        );
    }

    /// `half_life_days <= 0` is how this project spells "decay off", and the
    /// query path acts on it (`links::decayed`). Clamping it to one second
    /// said the opposite: a base search treats as fully activated reported as
    /// entirely out of reach.
    #[tokio::test]
    async fn decay_turned_off_leaves_everything_where_it_was() {
        let store = Store::memory().await.unwrap();
        let src = store.insert_corpus("raw", "web", None).await.unwrap();
        let made = store
            .insert_artifacts(&src.id, &[chunk("something worth finding")])
            .await
            .unwrap();
        let year = 365 * 86_400;
        store.set_activation(&made[0].id, 1.0, 0).await.unwrap();

        let off = store.fading(0, year).await.unwrap();
        assert_eq!(off[0].label, "reachable");
        assert_eq!(off[0].count, 1, "with decay off, a year changes nothing");
        assert_eq!(off[3].count, 0);

        // And with decay on it still decays, which is the other half of the
        // claim this function's doc comment makes.
        let on = store.fading(30 * 86_400, year).await.unwrap();
        assert_eq!(on[3].label, "out of reach");
        assert_eq!(on[3].count, 1);
    }
}
