//! Evidence about retrieval left by use, written once when it happens. Silence is not
//! recorded: a search nobody acted on may have failed or been answered by the rail.

use super::feedback::{blob_to_vec, vec_to_blob};
use super::{Store, new_id, now};
use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Cited,
    Opened,
    /// About the retrieval as a whole; names no artifact.
    Unsupported,
    /// Weak: a search read off the rail and left satisfied looks the same.
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

    /// Derived here so no caller can invent a weight: the give-up is a quarter of the others
    /// because it alone cannot tell success from silence.
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
    /// `None` for a citation, which comes from an ask.
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
    /// What a buried vector has to match before the two are compared.
    pub embed_model: String,
}

/// A row as `read` reads it, for `query_as!`.
pub(crate) struct ObservationRow {
    pub id: String,
    pub created_at: i64,
    pub generation_id: String,
    pub query: String,
    pub query_vec: Vec<u8>,
    pub artifact_id: Option<String>,
    pub rank: Option<i64>,
    pub source: String,
    pub strength: f64,
    pub event_id: Option<String>,
    pub embed_model: String,
}

fn read(r: ObservationRow) -> Result<Observation> {
    Ok(Observation {
        id: r.id,
        created_at: r.created_at,
        generation_id: r.generation_id,
        query: r.query,
        query_vec: blob_to_vec(&r.query_vec),
        artifact_id: r.artifact_id,
        rank: r.rank,
        source: Source::parse(&r.source)?,
        strength: r.strength as f32,
        event_id: r.event_id,
        embed_model: r.embed_model,
    })
}

/// Takes any executor: an ask writes its citations and observations in one transaction.
pub(crate) async fn insert<'e, E>(ex: E, o: &NewObservation) -> Result<String>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let id = new_id();
    sqlx::query!(
        "INSERT INTO observations
           (id, created_at, generation_id, query, query_vec, vec_dim,
            embed_model, artifact_id, rank, source, strength, event_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        &id,
        now(),
        &o.generation_id,
        &o.query,
        vec_to_blob(&o.query_vec),
        o.query_vec.len() as i64,
        &o.embed_model,
        &o.artifact_id,
        o.rank,
        o.source.as_str(),
        o.source.strength(),
        &o.event_id
    )
    .execute(ex)
    .await?;
    Ok(id)
}

impl Store {
    pub async fn record_observation(&self, o: &NewObservation) -> Result<String> {
        insert(&self.pool, o).await
    }

    /// Excluded rows are kept but not returned: their artifact is gone.
    pub async fn observations_for_generation(
        &self,
        generation_id: &str,
        limit: usize,
    ) -> Result<Vec<Observation>> {
        sqlx::query_as!(
            ObservationRow,
            r#"SELECT id AS "id!", created_at AS "created_at!", generation_id AS "generation_id!", query AS "query!", query_vec AS "query_vec!", artifact_id AS "artifact_id?", rank AS "rank?", source AS "source!", strength AS "strength!", event_id AS "event_id?", embed_model AS "embed_model!" FROM observations
              WHERE generation_id = ? AND excluded_at IS NULL
              ORDER BY created_at DESC, id DESC
              LIMIT ?"#,
            generation_id,
            limit as i64
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(read)
        .collect()
    }

    /// At-or-before: the clock is seconds, and an observation in the same second as the
    /// hiding was still of a live subject.
    pub async fn observations_naming(
        &self,
        artifact_id: &str,
        before: i64,
        embed_recipe: &str,
        chat_model: &str,
        limit: usize,
    ) -> Result<Vec<Observation>> {
        sqlx::query_as!(
            crate::store::observations::ObservationRow,
            r#"SELECT o.id AS "id!", o.created_at AS "created_at!", o.generation_id AS "generation_id!", o.query AS "query!", o.query_vec AS "query_vec!", o.artifact_id AS "artifact_id?", o.rank AS "rank?", o.source AS "source!", o.strength AS "strength!", o.event_id AS "event_id?", o.embed_model AS "embed_model!"
               FROM observations o
               JOIN generations g ON g.id = o.generation_id
              WHERE o.artifact_id = ? AND o.created_at <= ? AND o.strength > 0
                AND o.excluded_at IS NULL
                AND g.embed_recipe = ? AND g.chat_model = ?
              ORDER BY o.created_at DESC, o.id DESC
              LIMIT ?"#,
            artifact_id,
            before,
            embed_recipe,
            chat_model,
            limit as i64
        ).fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(read)
        .collect()
    }

    /// `(created_at, id)`: the clock is seconds, so a bare `created_at > ?` loses rows when
    /// `limit` cuts inside a second. See `Cursor`.
    pub async fn gave_ups_since(
        &self,
        after: &crate::store::Cursor,
        limit: usize,
    ) -> Result<Vec<Observation>> {
        sqlx::query_as!(
            ObservationRow,
            r#"SELECT id AS "id!", created_at AS "created_at!", generation_id AS "generation_id!", query AS "query!", query_vec AS "query_vec!", artifact_id AS "artifact_id?", rank AS "rank?", source AS "source!", strength AS "strength!", event_id AS "event_id?", embed_model AS "embed_model!" FROM observations
              WHERE source = 'gave_up' AND excluded_at IS NULL
                AND (created_at > ? OR (created_at = ? AND id > ?))
              ORDER BY created_at ASC, id ASC
              LIMIT ?"#,
            after.at,
            after.at,
            &after.id,
            limit as i64
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(read)
        .collect()
    }

    /// `artifact_id` has no foreign key on purpose, but a positive observation of a gone
    /// artifact props up its generation's rate. A sweep, because artifacts leave by many doors.
    pub async fn exclude_orphaned_observations(&self) -> Result<u64> {
        Ok(sqlx::query!(
            "UPDATE observations SET excluded_at = ?
              WHERE excluded_at IS NULL
                AND artifact_id IS NOT NULL
                AND artifact_id NOT IN (SELECT id FROM artifacts)",
            now()
        )
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    /// Zero keeps them for ever. `query` and `query_vec` are the person's words, so they age.
    pub async fn expire_observations(&self, retain_days: i64) -> Result<u64> {
        if retain_days <= 0 {
            return Ok(0);
        }
        Ok(sqlx::query!(
            "DELETE FROM observations WHERE created_at < ?",
            now() - retain_days * 86_400
        )
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    /// No cascade reaches here from `search_events`: "erase captured feedback" must call this.
    pub async fn purge_observations(&self) -> Result<u64> {
        Ok(sqlx::query!("DELETE FROM observations")
            .execute(&self.pool)
            .await?
            .rows_affected())
    }

    pub async fn exclude_observation(&self, id: &str) -> Result<()> {
        sqlx::query!(
            "UPDATE observations SET excluded_at = ? WHERE id = ? AND excluded_at IS NULL",
            now(),
            id
        )
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
        // One second is the clock's whole resolution.
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
        let n: i64 = sqlx::query_scalar!("SELECT COUNT(*) FROM observations")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(n, 1, "excluded is not deleted");
    }

    #[tokio::test]
    async fn an_observation_whose_artifact_left_stops_being_read_back() {
        let (store, generation) = base().await;
        let corpus = store.insert_corpus("raw", "web", None).await.unwrap();
        let artifact = store
            .insert_artifacts(
                &corpus.id,
                &[crate::store::artifacts::NewArtifact {
                    text: "the image will not mount".into(),
                    ..Default::default()
                }],
            )
            .await
            .unwrap()[0]
            .id
            .clone();
        store
            .record_observation(&obs(&generation, Some(&artifact), Some(1), Source::Cited))
            .await
            .unwrap();
        // It names no artifact, so it must survive.
        store
            .record_observation(&obs(&generation, None, None, Source::Unsupported))
            .await
            .unwrap();

        assert_eq!(store.exclude_orphaned_observations().await.unwrap(), 0);
        store.delete_corpus(&corpus.id).await.unwrap();
        assert_eq!(store.exclude_orphaned_observations().await.unwrap(), 1);

        let back = store
            .observations_for_generation(&generation, 10)
            .await
            .unwrap();
        assert_eq!(back.len(), 1, "only the one about the whole retrieval");
        assert_eq!(back[0].artifact_id, None);
        assert_eq!(store.exclude_orphaned_observations().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn the_queries_kept_here_age_and_erase_with_the_searches() {
        let (store, generation) = base().await;
        let old = store
            .record_observation(&obs(&generation, Some("art-1"), Some(1), Source::Cited))
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE observations SET created_at = ? WHERE id = ?",
            crate::store::now() - 60 * 86_400,
            &old
        )
        .execute(&store.pool)
        .await
        .unwrap();
        store
            .record_observation(&obs(&generation, Some("art-2"), Some(1), Source::Cited))
            .await
            .unwrap();

        assert_eq!(
            store.expire_observations(0).await.unwrap(),
            0,
            "0 is for ever"
        );
        assert_eq!(store.expire_observations(30).await.unwrap(), 1);
        assert_eq!(store.purge_observations().await.unwrap(), 1);
        let n: i64 = sqlx::query_scalar!("SELECT COUNT(*) FROM observations")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(n, 0);
    }
}
