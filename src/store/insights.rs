//! What this memory is like, as aggregates over tables that already exist.
//!
//! No new table, no sweep, and no model call: every figure here is a `COUNT`
//! or a `GROUP BY` over rows some other part of the system already writes.
//! That constraint is the one at the top of `ROADMAP.md`, and it is what makes
//! this page cheap enough to open whenever.
//!
//! With one exception, named where it lives: `fading` reads two columns of
//! every active artifact, because the arithmetic it applies is not one SQLite
//! can be relied on to have. It is streamed rather than collected, so the cost
//! is a scan and not a `Vec` the size of the base.
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
        // One round trip, not four in a row: this renders on Insights and,
        // through the idle rail, on the start page. `synthesized` is named,
        // not negated — `!= 'captured'` swept in `passage` rows, which are
        // the document's own words verbatim, the opposite of what it counts.
        // `Provenance::is_model_written` is the predicate, and those are its
        // two values.
        let (corpora, artifacts, segments, synthesized) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM corpora), \
                    (SELECT count(*) FROM artifacts WHERE status = 'active'), \
                    (SELECT count(*) FROM segments), \
                    (SELECT count(*) FROM artifacts \
                     WHERE status = 'active' AND provenance IN ('merged', 'synthesized'))",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(Held {
            corpora,
            artifacts,
            segments,
            synthesized,
        })
    }

    /// The two counts the idle rail introduces the base with. `held()`
    /// computes two more that the rail throws away, and the rail renders on
    /// every box-clear.
    pub async fn held_brief(&self) -> Result<(i64, i64)> {
        Ok(sqlx::query_as(
            "SELECT (SELECT count(*) FROM corpora), \
                    (SELECT count(*) FROM artifacts WHERE status = 'active')",
        )
        .fetch_one(&self.pool)
        .await?)
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
    /// The one figure on this page that is not an aggregate the database
    /// computes. It reads two columns of every active artifact — a full scan,
    /// growing with the base it is measuring — which is the price of applying
    /// the same arithmetic the query path does, so this says what a search
    /// would find rather than what was last written down. Streamed, so that
    /// scan costs four counters rather than a row per artifact held in memory
    /// at once.
    ///
    /// Four bands rather than a histogram: the question is "what is falling
    /// out of reach", which the bottom band answers. The shape of the curve is
    /// not a question anybody has.
    pub async fn fading(&self, half_life_days: f64, now: i64) -> Result<Vec<Bucket>> {
        let mut rows = sqlx::query_as::<_, (f64, i64)>(
            "SELECT activation, activated_at FROM artifacts WHERE status = 'active'",
        )
        .fetch(&self.pool);
        // `links::decayed` is the arithmetic the query path applies — the
        // same formula, the same "non-positive turns decay off" reading. A
        // copy of it here is a copy that can drift, and this page's whole
        // claim is "what a search would find now".
        let (mut reachable, mut settling, mut fading, mut gone) = (0i64, 0i64, 0i64, 0i64);
        while let Some(row) = tokio_stream::StreamExt::next(&mut rows).await {
            let (activation, at) = row?;
            let a = super::links::decayed(activation, at, now, half_life_days);
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

        let off = store.fading(0.0, year).await.unwrap();
        assert_eq!(off[0].label, "reachable");
        assert_eq!(off[0].count, 1, "with decay off, a year changes nothing");
        assert_eq!(off[3].count, 0);

        // And with decay on it still decays, which is the other half of the
        // claim this function's doc comment makes.
        let on = store.fading(30.0, year).await.unwrap();
        assert_eq!(on[3].label, "out of reach");
        assert_eq!(on[3].count, 1);
    }
}
