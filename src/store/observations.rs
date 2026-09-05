//! What use left behind.
//!
//! A retrieval attempt that ended in something is evidence about the list that
//! produced it: an excerpt an answer actually drew on, a result somebody
//! opened, an answer asserting what none of its excerpts held, a search given
//! up on. Each is written once, at the moment it happens, and read by nothing
//! a person waits on.
//!
//! Silence is not here, deliberately. A search nobody acted on may be one that
//! failed or one whose answer was read straight off the rail, and those are
//! indistinguishable — so neither is recorded rather than one being guessed.

use super::feedback::{blob_to_vec, vec_to_blob};
use super::{Store, new_id, now};
use crate::error::{Error, Result};
use sqlx::Row;

/// Where an observation came from. The variant decides the weight; see
/// `strength`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// An answer drew on this excerpt — `ask_citations.used`.
    Cited,
    /// A person opened this result from the list it was in.
    Opened,
    /// The answer asserted a literal none of its excerpts held. About the
    /// retrieval as a whole; names no artifact.
    Unsupported,
    /// A search nobody opened, followed by another search from the same
    /// person. Weak: the rail shows snippets, so a search read and walked away
    /// from satisfied looks exactly like one given up on.
    GaveUp,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Cited => "cited",
            Source::Opened => "opened",
            Source::Unsupported => "unsupported",
            Source::GaveUp => "gave_up",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "cited" => Source::Cited,
            "opened" => Source::Opened,
            "unsupported" => Source::Unsupported,
            "gave_up" => Source::GaveUp,
            other => {
                return Err(Error::Store(format!(
                    "observations: unknown source {other}"
                )));
            }
        })
    }

    /// The weight class this source carries.
    ///
    /// Derived here rather than passed in, so that no caller can invent one and
    /// the asymmetry the whole design rests on stays legible in one place: the
    /// give-up is a quarter of the others because it is the only source that
    /// cannot tell success from silence.
    pub fn strength(self) -> f32 {
        match self {
            Source::Cited | Source::Opened => 1.0,
            Source::Unsupported => -1.0,
            Source::GaveUp => -0.25,
        }
    }
}

#[derive(Debug, Clone)]
pub struct NewObservation {
    pub generation_id: String,
    pub query: String,
    pub query_vec: Vec<f32>,
    pub embed_model: String,
    pub artifact_id: Option<String>,
    pub rank: Option<i64>,
    pub source: Source,
    /// The search event this came from: an open or a give-up. `None` for a
    /// citation, which comes from an ask.
    pub event_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Observation {
    pub id: String,
    pub created_at: i64,
    pub generation_id: String,
    pub query: String,
    pub query_vec: Vec<f32>,
    pub artifact_id: Option<String>,
    pub rank: Option<i64>,
    pub source: Source,
    pub strength: f32,
    pub event_id: Option<String>,
    /// The model the query vector came from: what a buried vector has to
    /// match before the two are compared.
    pub embed_model: String,
}

const COLUMNS: &str = "id, created_at, generation_id, query, query_vec, artifact_id,                        rank, source, strength, event_id, embed_model";

fn read(r: &sqlx::sqlite::SqliteRow) -> Result<Observation> {
    Ok(Observation {
        id: r.get("id"),
        created_at: r.get("created_at"),
        generation_id: r.get("generation_id"),
        query: r.get("query"),
        query_vec: blob_to_vec(&r.get::<Vec<u8>, _>("query_vec")),
        artifact_id: r.get("artifact_id"),
        rank: r.get("rank"),
        source: Source::parse(&r.get::<String, _>("source"))?,
        strength: r.get("strength"),
        event_id: r.get("event_id"),
        embed_model: r.get("embed_model"),
    })
}

/// Write one observation through whatever executor the caller has.
///
/// Generic over the executor rather than taking `&Store`, because the writes
/// that matter happen inside a transaction somebody else opened: an ask
/// records its citations and its observations together or neither, or an
/// answer exists that the evidence has no record of.
pub(crate) async fn insert<'e, E>(ex: E, o: &NewObservation) -> Result<String>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let id = new_id();
    sqlx::query(
        "INSERT INTO observations
           (id, created_at, generation_id, query, query_vec, vec_dim,
            embed_model, artifact_id, rank, source, strength, event_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(now())
    .bind(&o.generation_id)
    .bind(&o.query)
    .bind(vec_to_blob(&o.query_vec))
    .bind(o.query_vec.len() as i64)
    .bind(&o.embed_model)
    .bind(&o.artifact_id)
    .bind(o.rank)
    .bind(o.source.as_str())
    .bind(o.source.strength())
    .bind(&o.event_id)
    .execute(ex)
    .await?;
    Ok(id)
}

impl Store {
    pub async fn record_observation(&self, o: &NewObservation) -> Result<String> {
        insert(&self.pool, o).await
    }

    /// What was observed under one generation, newest first.
    ///
    /// Excluded rows are left out rather than removed: the row is kept because
    /// nothing here is deleted, and it is not returned because an observation
    /// whose artifact is gone is not a statement about ranking any more.
    pub async fn observations_for_generation(
        &self,
        generation_id: &str,
        limit: usize,
    ) -> Result<Vec<Observation>> {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM observations
              WHERE generation_id = ? AND excluded_at IS NULL
              ORDER BY created_at DESC, id DESC
              LIMIT ?"
        )))
        .bind(generation_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(read)
        .collect()
    }

    /// Positive observations naming `artifact_id`, made at or before `before`,
    /// under any generation of the era `(embed_recipe, chat_model)`. Newest
    /// first, at most `limit`. What rule 1 replays: the record of a subject
    /// being found, from before it was hidden. At-or-before, because the clock
    /// is seconds and an observation in the same second as the hiding was
    /// still made of a subject that was live.
    pub async fn observations_naming(
        &self,
        artifact_id: &str,
        before: i64,
        embed_recipe: &str,
        chat_model: &str,
        limit: usize,
    ) -> Result<Vec<Observation>> {
        sqlx::query(
            "SELECT o.id, o.created_at, o.generation_id, o.query, o.query_vec, o.artifact_id,
                    o.rank, o.source, o.strength, o.event_id, o.embed_model
               FROM observations o
               JOIN generations g ON g.id = o.generation_id
              WHERE o.artifact_id = ? AND o.created_at <= ? AND o.strength > 0
                AND o.excluded_at IS NULL
                AND g.embed_recipe = ? AND g.chat_model = ?
              ORDER BY o.created_at DESC, o.id DESC
              LIMIT ?",
        )
        .bind(artifact_id)
        .bind(before)
        .bind(embed_recipe)
        .bind(chat_model)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(read)
        .collect()
    }

    /// Give-ups recorded after `after`, oldest first, at most `limit`. What
    /// rule 2 reads, behind a cursor it keeps in `meta`.
    ///
    /// `(created_at, id)` and not `created_at` alone: the clock is seconds, so
    /// a bare `created_at > ?` loses whatever else shares the cursor's second
    /// when `limit` cuts inside it. See `Cursor`.
    pub async fn gave_ups_since(
        &self,
        after: &crate::store::Cursor,
        limit: usize,
    ) -> Result<Vec<Observation>> {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM observations
              WHERE source = 'gave_up' AND excluded_at IS NULL
                AND (created_at > ? OR (created_at = ? AND id > ?))
              ORDER BY created_at ASC, id ASC
              LIMIT ?"
        )))
        .bind(after.at)
        .bind(after.at)
        .bind(&after.id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(read)
        .collect()
    }

    /// Stop reading an observation back without losing it.
    pub async fn exclude_observation(&self, id: &str) -> Result<()> {
        sqlx::query("UPDATE observations SET excluded_at = ? WHERE id = ? AND excluded_at IS NULL")
            .bind(now())
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::generations::{GenerationParams, NewGeneration};

    async fn base() -> (Store, String) {
        let store = Store::memory().await.unwrap();
        let generation = store
            .record_generation(&NewGeneration {
                params: GenerationParams {
                    recency_weight: 0.05,
                    per_source_cap: Some(3),
                    ..Default::default()
                },
                embed_recipe: "recipe-a".into(),
                chat_model: "qwen".into(),
                parent_id: None,
            })
            .await
            .unwrap();
        (store, generation)
    }

    fn obs(
        generation: &str,
        artifact: Option<&str>,
        rank: Option<i64>,
        source: Source,
    ) -> NewObservation {
        NewObservation {
            generation_id: generation.to_string(),
            query: "how did I mount it".into(),
            query_vec: vec![0.1, 0.2, 0.3],
            embed_model: "embeddinggemma".into(),
            artifact_id: artifact.map(str::to_string),
            rank,
            source,
            event_id: None,
        }
    }

    #[tokio::test]
    async fn an_observation_keeps_its_query_vector_so_a_replay_costs_no_embedding() {
        let (store, generation) = base().await;
        store
            .record_observation(&obs(&generation, Some("art-1"), Some(2), Source::Cited))
            .await
            .unwrap();

        let back = store
            .observations_for_generation(&generation, 10)
            .await
            .unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].query_vec, vec![0.1, 0.2, 0.3]);
        assert_eq!(back[0].rank, Some(2));
    }

    #[tokio::test]
    async fn the_source_decides_the_strength_and_the_caller_cannot() {
        let (store, generation) = base().await;
        for (source, want) in [
            (Source::Cited, 1.0),
            (Source::Opened, 1.0),
            (Source::Unsupported, -1.0),
            (Source::GaveUp, -0.25),
        ] {
            store
                .record_observation(&obs(&generation, Some("art-1"), Some(1), source))
                .await
                .unwrap();
            let back = store
                .observations_for_generation(&generation, 1)
                .await
                .unwrap();
            assert_eq!(back[0].strength, want, "{source:?}");
        }
    }

    #[tokio::test]
    async fn an_observation_about_the_whole_retrieval_names_no_artifact() {
        let (store, generation) = base().await;
        store
            .record_observation(&obs(&generation, None, None, Source::Unsupported))
            .await
            .unwrap();
        let back = store
            .observations_for_generation(&generation, 10)
            .await
            .unwrap();
        assert_eq!(back[0].artifact_id, None);
        assert_eq!(back[0].rank, None);
    }

    #[tokio::test]
    async fn the_give_up_cursor_does_not_lose_the_rest_of_its_own_second() {
        let (store, generation) = base().await;
        // Three give-ups, all in one second — which is the whole resolution of
        // the clock, so this is what a busy moment looks like.
        for _ in 0..3 {
            store
                .record_observation(&obs(&generation, None, None, Source::GaveUp))
                .await
                .unwrap();
        }
        let cursor = crate::store::Cursor::default();
        let first = store.gave_ups_since(&cursor, 1).await.unwrap();
        assert_eq!(first.len(), 1);
        let cursor = crate::store::Cursor {
            at: first[0].created_at,
            id: first[0].id.clone(),
        };
        let rest = store.gave_ups_since(&cursor, 10).await.unwrap();
        assert_eq!(rest.len(), 2, "the other two share the cursor's second");
        assert!(rest.iter().all(|o| o.id != first[0].id));
        let last = crate::store::Cursor {
            at: rest[1].created_at,
            id: rest[1].id.clone(),
        };
        assert!(store.gave_ups_since(&last, 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_excluded_observation_is_kept_and_not_read_back() {
        let (store, generation) = base().await;
        let id = store
            .record_observation(&obs(&generation, Some("art-1"), Some(1), Source::Cited))
            .await
            .unwrap();
        store.exclude_observation(&id).await.unwrap();

        assert!(
            store
                .observations_for_generation(&generation, 10)
                .await
                .unwrap()
                .is_empty()
        );
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM observations")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(n, 1, "excluded is not deleted");
    }
}
