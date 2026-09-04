//! The named, versioned settings a base is currently retrieving under.
//!
//! A number that moved has to have something to have moved from. This is that
//! something: one immutable row per set of parameters the base has run under,
//! with the models that computed alongside them, and exactly one of them live.

use super::{Store, new_id, now};
use crate::error::{Error, Result};
use sqlx::Row;

/// Same reason `links.rs` holds one: the read below decides what the write
/// does, and a deferred transaction takes its snapshot before the upgrade.
const IMMEDIATE: &str = "BEGIN IMMEDIATE";

/// The knobs a generation holds. These are the two the runtime sweep already
/// moves; the set is meant to widen, and nothing else has to change when it
/// does, because it is stored as JSON.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GenerationParams {
    pub recency_weight: f32,
    pub per_source_cap: Option<usize>,
}

impl From<crate::core::ranking::RankingParams> for GenerationParams {
    fn from(p: crate::core::ranking::RankingParams) -> Self {
        Self {
            recency_weight: p.recency_weight,
            per_source_cap: p.per_source_cap,
        }
    }
}

#[derive(Debug, Clone)]
pub struct NewGeneration {
    pub params: GenerationParams,
    pub embed_recipe: String,
    pub chat_model: String,
    pub parent_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Generation {
    pub id: String,
    pub created_at: i64,
    pub params: GenerationParams,
    pub embed_recipe: String,
    pub chat_model: String,
    pub parent_id: Option<String>,
}

impl Store {
    /// Record a generation and make it the live one.
    ///
    /// One transaction: a base with two live generations is a base whose
    /// searches cannot say which settings produced them.
    pub async fn record_generation(&self, g: &NewGeneration) -> Result<String> {
        let id = new_id();
        let mut tx = self.pool.begin_with(IMMEDIATE).await?;
        sqlx::query("UPDATE generations SET state = 'superseded' WHERE state = 'live'")
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO generations
               (id, created_at, params, embed_recipe, chat_model, parent_id,
                run_id, predicted, state)
             VALUES (?, ?, ?, ?, ?, ?, NULL, NULL, 'live')",
        )
        .bind(&id)
        .bind(now())
        .bind(json(&g.params)?)
        .bind(&g.embed_recipe)
        .bind(&g.chat_model)
        .bind(&g.parent_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    pub async fn live_generation(&self) -> Result<Option<Generation>> {
        let row = sqlx::query(
            "SELECT id, created_at, params, embed_recipe, chat_model, parent_id
               FROM generations WHERE state = 'live'
              ORDER BY created_at DESC, id DESC LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| {
            Ok(Generation {
                id: r.get("id"),
                created_at: r.get("created_at"),
                params: from_json(&r.get::<String, _>("params"))?,
                embed_recipe: r.get("embed_recipe"),
                chat_model: r.get("chat_model"),
                parent_id: r.get("parent_id"),
            })
        })
        .transpose()
    }
}

/// The live generation for the running configuration, minting one where the
/// base has none or where the models have changed under it.
///
/// A model change mints a generation rather than editing the live one: a
/// generation is immutable, and the point of the row is that observations
/// collected under it name something that still says what it said.
pub async fn ensure_generation(
    store: &Store,
    params: GenerationParams,
    embed_recipe: &str,
    chat_model: &str,
) -> Result<Generation> {
    let live = store.live_generation().await?;
    if let Some(live) = &live
        && live.embed_recipe == embed_recipe
        && live.chat_model == chat_model
    {
        return Ok(live.clone());
    }
    let parent_id = live.map(|g| g.id);
    if parent_id.is_some() {
        tracing::info!(
            embed_recipe,
            chat_model,
            "models changed; observations recorded before this belong to another era"
        );
    }
    let id = store
        .record_generation(&NewGeneration {
            params,
            embed_recipe: embed_recipe.to_string(),
            chat_model: chat_model.to_string(),
            parent_id,
        })
        .await?;
    store
        .live_generation()
        .await?
        .filter(|g| g.id == id)
        .ok_or_else(|| Error::Store("generations: the new generation was not made live".into()))
}

// `Error` has no `From<serde_json::Error>`, so both directions map explicitly.
// This is the shape `eval_runs.rs` already uses; copied rather than replaced by
// a blanket conversion, which would swallow the context in every other store
// module too.
fn json<T: serde::Serialize>(v: &T) -> Result<String> {
    serde_json::to_string(v).map_err(|e| Error::Store(format!("generations: {e}")))
}

fn from_json<T: serde::de::DeserializeOwned>(s: &str) -> Result<T> {
    serde_json::from_str(s).map_err(|e| Error::Store(format!("generations: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> NewGeneration {
        NewGeneration {
            params: GenerationParams {
                recency_weight: 0.05,
                per_source_cap: Some(3),
            },
            embed_recipe: "embeddinggemma:768:asym".into(),
            chat_model: "qwen".into(),
            parent_id: None,
        }
    }

    #[tokio::test]
    async fn a_fresh_base_has_no_generation() {
        let store = Store::memory().await.unwrap();
        assert!(store.live_generation().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn the_generation_recorded_last_is_the_only_live_one() {
        let store = Store::memory().await.unwrap();
        let first = store.record_generation(&sample()).await.unwrap();

        let mut second = sample();
        second.parent_id = Some(first.clone());
        second.params.recency_weight = 0.1;
        let second_id = store.record_generation(&second).await.unwrap();

        let live = store.live_generation().await.unwrap().expect("one is live");
        assert_eq!(live.id, second_id);
        assert_eq!(live.parent_id.as_deref(), Some(first.as_str()));
        assert_eq!(live.params.recency_weight, 0.1);
    }

    #[tokio::test]
    async fn a_base_with_no_generation_gets_one_from_the_running_config() {
        let store = Store::memory().await.unwrap();
        let params = GenerationParams {
            recency_weight: 0.05,
            per_source_cap: Some(3),
        };
        let g = ensure_generation(&store, params, "recipe-a", "qwen")
            .await
            .unwrap();
        assert_eq!(g.params, params);
        assert!(g.parent_id.is_none(), "the first generation has no parent");
    }

    #[tokio::test]
    async fn a_second_boot_under_the_same_models_reuses_the_generation() {
        let store = Store::memory().await.unwrap();
        let params = GenerationParams {
            recency_weight: 0.05,
            per_source_cap: Some(3),
        };
        let first = ensure_generation(&store, params, "recipe-a", "qwen")
            .await
            .unwrap();
        let again = ensure_generation(&store, params, "recipe-a", "qwen")
            .await
            .unwrap();
        assert_eq!(
            first.id, again.id,
            "an unchanged boot must not mint a generation"
        );
    }

    #[tokio::test]
    async fn a_changed_chat_model_starts_a_new_era() {
        // Every citation-derived number shifts when the generator changes.
        // Carrying on under the same generation would compare two things that
        // were never measured the same way.
        let store = Store::memory().await.unwrap();
        let params = GenerationParams {
            recency_weight: 0.05,
            per_source_cap: Some(3),
        };
        let first = ensure_generation(&store, params, "recipe-a", "qwen")
            .await
            .unwrap();
        let second = ensure_generation(&store, params, "recipe-a", "llama")
            .await
            .unwrap();

        assert_ne!(first.id, second.id);
        assert_eq!(second.parent_id.as_deref(), Some(first.id.as_str()));
        assert_eq!(second.params, params, "a model change moves no knob");
    }

    #[tokio::test]
    async fn a_superseded_generation_is_kept_rather_than_replaced() {
        // The journal is the whole point: a parameter that moved has to have
        // something to have moved *from*, months later.
        let store = Store::memory().await.unwrap();
        store.record_generation(&sample()).await.unwrap();
        store.record_generation(&sample()).await.unwrap();

        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM generations")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(n, 2, "the earlier generation must survive its supersession");
    }
}
