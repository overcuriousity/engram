//! What this memory is like, as aggregates over tables that already exist.
//!
//! No new table, no sweep, and no model call: every figure here is a `COUNT`
//! or a `GROUP BY` over rows some other part of the system already writes.
//! That constraint is the first of the README's three rules, and it is what makes
//! this page cheap enough to open whenever.
//!
//! With one exception, named where it lives: `used` reads three columns of
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

    /// How much use is still standing on the base, bucketed.
    ///
    /// Bands over *engagement* — activation above the capture baseline, decayed
    /// to now (`links::engagement_at`) — not over the raw stored activation.
    /// Read raw, with bands at 0.75 / 0.40 / 0.15, this said something that
    /// stopped being true: those numbers were calibrated when a search bumped
    /// every hit it returned by a full `1.0`, so an artifact in use was
    /// continually topped up and only a neglected one fell. With `retrieved` at
    /// zero nothing but an open, a confirmation or a citation raises the
    /// number, so *every* artifact now decays monotonically from its own
    /// capture baseline: one half-life after capture the whole base read
    /// "settling", two and it read "fading" and then "out of reach", of a base
    /// retrieving perfectly well. The old bottom band measured nothing but the
    /// calendar.
    ///
    /// It cannot be repaired by moving the thresholds, because the claim
    /// underneath it is gone too. With `associate.prime_lift` shipped at zero,
    /// activation does not move a search result at all — so "what a search
    /// would find now" is no longer a thing this column can say. What the
    /// number does still mean is what use put there, which is what promotion
    /// arms on and what priming would read if it were switched on, so that is
    /// what is reported: the bands are in units of an open
    /// (`activation.opened`, the unit the other weights are given in), and the
    /// bottom one is exactly zero — nobody has ever opened, confirmed or cited
    /// it. Engagement decays at the same rate the baseline does, so a band is
    /// "how much use is still standing", not a count of events.
    ///
    /// The one figure on this page that is not an aggregate the database
    /// computes. It reads three columns of every active artifact — a full scan,
    /// growing with the base it is measuring — because the decay needs `pow`,
    /// which SQLite only has when it was built with
    /// `SQLITE_ENABLE_MATH_FUNCTIONS`: a build flag this project does not get
    /// to assume, and whose absence is a 500 on a page rather than a wrong
    /// number. Streamed, so that scan costs four counters rather than a row per
    /// artifact held in memory at once.
    ///
    /// Four bands rather than a histogram: the question is "how much of this is
    /// anybody actually using", which the top and bottom bands answer between
    /// them. The shape of the curve is not a question anybody has.
    pub async fn used(&self, half_life_days: f64, now: i64) -> Result<Vec<Bucket>> {
        let mut rows = sqlx::query_as::<_, (f64, i64, i64)>(
            "SELECT activation, activated_at, created_at FROM artifacts WHERE status = 'active'",
        )
        .fetch(&self.pool);
        // `links::engagement_at` is the arithmetic priming and promotion apply
        // — the same formula, the same "non-positive half-life turns decay
        // off" reading, the same baseline constant. A copy of it here is a copy
        // that can drift, and three readers of "what use added" disagreeing is
        // exactly the failure this page would report as a fact.
        let (mut often, mut reached, mut once, mut never) = (0i64, 0i64, 0i64, 0i64);
        while let Some(row) = tokio_stream::StreamExt::next(&mut rows).await {
            let (activation, at, created_at) = row?;
            let e = super::links::engagement_at(activation, at, created_at, now, half_life_days);
            match e {
                // A confirmation, or three opens.
                _ if e >= 3.0 => often += 1,
                // An open still standing at its full weight.
                _ if e >= 1.0 => reached += 1,
                // Some use, decayed below one open — or a citation, which is
                // half of one.
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
        // The baseline plus one open, both as of the instant of capture — the
        // stamp has to be the artifact's own `created_at`, because engagement
        // is the gap between two terms of the same age.
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

        // And with decay on it still decays, which is the other half of the
        // claim this function's doc comment makes. A year is twelve half-lives:
        // what the open put there is a trace, but it is not nothing, and the
        // artifact is not confused with one nobody ever opened.
        let on = store.used(30.0, year).await.unwrap();
        assert_eq!(on[2].label, "touched once");
        assert_eq!(on[2].count, 1);
        assert_eq!(on[3].count, 0);
    }

    /// The failure this column was rebanded to stop: read raw, an artifact
    /// nobody has ever opened slid down the bands as the calendar moved,
    /// because with `retrieved` at zero nothing tops it back up. Read as
    /// engagement it stays where it belongs — at nothing — however old it is,
    /// and an artifact that *was* opened stays distinguishable from it.
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
        // Baseline plus a confirmation, stamped at capture so both terms have
        // the same age and the whole of the gap between them is the use.
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
