//! Aggregates over existing tables: no new table, sweep or model call. Retrieval figures
//! live in `feedback_stats` so two pages never report two numbers for one thing.

use crate::error::Result;
use crate::store::Store;

#[derive(Debug, Default, Clone)]
pub struct Held {
    pub corpora: i64,
    pub artifacts: i64,
    pub segments: i64,
    /// Said beside the total because "rewriting is earned" is a claim this number tests.
    pub synthesized: i64,
}

#[derive(Debug, Clone)]
pub struct Bucket {
    pub label: &'static str,
    pub count: i64,
}

impl Store {
    pub async fn held(&self) -> Result<Held> {
        // `synthesized` names its values: `!= 'captured'` would sweep in verbatim `passage` rows.
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

    /// The idle rail renders on every box-clear; `held()` computes two counts it would discard.
    pub async fn held_brief(&self) -> Result<(i64, i64)> {
        Ok(sqlx::query_as(
            "SELECT (SELECT count(*) FROM corpora), \
                    (SELECT count(*) FROM artifacts WHERE status = 'active')",
        )
        .fetch_one(&self.pool)
        .await?)
    }

    /// Bands over engagement above the capture baseline (`links::engagement_at`), in units of an
    /// open; the bottom band is exactly zero. A full scan in Rust, because SQLite has `pow` only
    /// with `SQLITE_ENABLE_MATH_FUNCTIONS`; streamed, so it holds four counters.
    pub async fn used(&self, half_life_days: f64, now: i64) -> Result<Vec<Bucket>> {
        let mut rows = sqlx::query_as::<_, (f64, i64, i64)>(
            "SELECT activation, activated_at, created_at FROM artifacts WHERE status = 'active'",
        )
        .fetch(&self.pool);
        // The same formula as priming and promotion, so the three readers cannot drift apart.
        let (mut often, mut reached, mut once, mut never) = (0i64, 0i64, 0i64, 0i64);
        while let Some(row) = tokio_stream::StreamExt::next(&mut rows).await {
            let (activation, at, created_at) = row?;
            let e = super::links::engagement_at(activation, at, created_at, now, half_life_days);
            match e {
                // A confirmation, or three opens.
                _ if e >= 3.0 => often += 1,
                // An open still standing at its full weight.
                _ if e >= 1.0 => reached += 1,
                // Some use, decayed below one open, or a citation (half of one).
                _ if e > 0.0 => once += 1,
                _ => never += 1,
            }
        }
        Ok(vec![
            Bucket {
                label: "reached often",
                count: often,
            },
            Bucket {
                label: "reached",
                count: reached,
            },
            Bucket {
                label: "touched once",
                count: once,
            },
            Bucket {
                label: "never reached",
                count: never,
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
            text: text.into(),
            ..Default::default()
        }
    }

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

    #[tokio::test]
    async fn decay_turned_off_leaves_everything_where_it_was() {
        let store = Store::memory().await.unwrap();
        let src = store.insert_corpus("raw", "web", None).await.unwrap();
        let made = store
            .insert_artifacts(&src.id, &[chunk("something worth finding")])
            .await
            .unwrap();
        // Stamped at the artifact's own `created_at`: both terms of the gap must have the same age.
        let (_, _, created_at) = store
            .activation_of(std::slice::from_ref(&made[0].id))
            .await
            .unwrap()
            .get(&made[0].id)
            .copied()
            .expect("an artifact carries activation");
        store
            .set_activation(&made[0].id, 2.0, created_at)
            .await
            .unwrap();
        let year = created_at + 365 * 86_400;

        let off = store.used(0.0, year).await.unwrap();
        assert_eq!(off[1].label, "reached");
        assert_eq!(off[1].count, 1, "with decay off, a year changes nothing");
        assert_eq!(off[3].count, 0);

        // A year is twelve half-lives: a trace, but not confused with never opened.
        let on = store.used(30.0, year).await.unwrap();
        assert_eq!(on[2].label, "touched once");
        assert_eq!(on[2].count, 1);
        assert_eq!(on[3].count, 0);
    }

    #[tokio::test]
    async fn an_untouched_artifact_never_leaves_the_bottom_band() {
        let store = Store::memory().await.unwrap();
        let src = store.insert_corpus("raw", "web", None).await.unwrap();
        store
            .insert_artifacts(&src.id, &[chunk("captured and never opened")])
            .await
            .unwrap();
        let made = store
            .insert_artifacts(&src.id, &[chunk("captured and confirmed")])
            .await
            .unwrap();
        let (_, _, created_at) = store
            .activation_of(std::slice::from_ref(&made[0].id))
            .await
            .unwrap()
            .get(&made[0].id)
            .copied()
            .expect("an artifact carries activation");
        store
            .set_activation(&made[0].id, 4.0, created_at)
            .await
            .unwrap();

        let now = created_at + 14 * 86_400;
        let bands = store.used(14.0, now).await.unwrap();
        assert_eq!(
            bands[3].count, 1,
            "the untouched one is the only one at zero: {bands:?}"
        );
        assert_eq!(
            bands[1].count, 1,
            "a half-life on, a confirmation is still an open and a half: {bands:?}"
        );
    }
}
