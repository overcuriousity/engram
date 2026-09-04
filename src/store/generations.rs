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

impl From<GenerationParams> for crate::core::ranking::RankingParams {
    fn from(p: GenerationParams) -> Self {
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
    /// The idle pass that proposed it. `None` for one minted at boot or by a
    /// person pressing Apply.
    pub run_id: Option<String>,
    /// What the pass said it would gain, as an MRR delta over the replay. A
    /// generation with a parent and a prediction is one the base is watching.
    pub predicted: Option<f64>,
    /// `live` | `superseded` | `reverted`.
    pub state: String,
}

fn hydrate(r: sqlx::sqlite::SqliteRow) -> Result<Generation> {
    Ok(Generation {
        id: r.get("id"),
        created_at: r.get("created_at"),
        params: from_json(&r.get::<String, _>("params"))?,
        embed_recipe: r.get("embed_recipe"),
        chat_model: r.get("chat_model"),
        parent_id: r.get("parent_id"),
        run_id: r.get("run_id"),
        predicted: r.get("predicted"),
        state: r.get("state"),
    })
}

impl Store {
    /// Record a generation and make it the live one.
    ///
    /// One transaction: a base with two live generations is a base whose
    /// searches cannot say which settings produced them.
    pub async fn record_generation(&self, g: &NewGeneration) -> Result<String> {
        self.insert_live(g, None, None).await
    }

    /// Record a generation the idle pass chose, carrying the run that chose it
    /// and what it promised, and make it live.
    pub async fn adopt_generation(
        &self,
        g: &NewGeneration,
        run_id: &str,
        predicted: f64,
    ) -> Result<String> {
        self.insert_live(g, Some(run_id), Some(predicted)).await
    }

    async fn insert_live(
        &self,
        g: &NewGeneration,
        run_id: Option<&str>,
        predicted: Option<f64>,
    ) -> Result<String> {
        let id = new_id();
        let mut tx = self.pool.begin_with(IMMEDIATE).await?;
        sqlx::query("UPDATE generations SET state = 'superseded' WHERE state = 'live'")
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO generations
               (id, created_at, params, embed_recipe, chat_model, parent_id,
                run_id, predicted, state)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'live')",
        )
        .bind(&id)
        .bind(now())
        .bind(json(&g.params)?)
        .bind(&g.embed_recipe)
        .bind(&g.chat_model)
        .bind(&g.parent_id)
        .bind(run_id)
        .bind(predicted)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Take a generation back: it becomes `reverted` and its parent is live
    /// again. Returns the parent, or `None` — and changes nothing — for a
    /// generation with nowhere to go back to.
    ///
    /// Cheap and complete because a generation is a row. Nothing in the corpus
    /// was touched by adopting it, so nothing has to be untouched here.
    pub async fn revert_generation(&self, id: &str) -> Result<Option<Generation>> {
        let mut tx = self.pool.begin_with(IMMEDIATE).await?;
        let parent: Option<String> =
            sqlx::query_scalar("SELECT parent_id FROM generations WHERE id = ?")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?
                .flatten();
        let Some(parent) = parent else {
            return Ok(None);
        };
        sqlx::query("UPDATE generations SET state = 'reverted' WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE generations SET state = 'live' WHERE id = ?")
            .bind(&parent)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.generation(&parent).await
    }

    pub async fn generation(&self, id: &str) -> Result<Option<Generation>> {
        sqlx::query(
            "SELECT id, created_at, params, embed_recipe, chat_model, parent_id,
                    run_id, predicted, state
               FROM generations WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .map(hydrate)
        .transpose()
    }

    pub async fn live_generation(&self) -> Result<Option<Generation>> {
        sqlx::query(
            "SELECT id, created_at, params, embed_recipe, chat_model, parent_id,
                    run_id, predicted, state
               FROM generations WHERE state = 'live'
              ORDER BY created_at DESC, id DESC LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await?
        .map(hydrate)
        .transpose()
    }

    /// The parameter sets already tried and taken back under these models, so
    /// the chooser does not offer them again. Without this the pass proposes
    /// the same losing candidate every quiet period, adopts it, watches it
    /// fail, and reverts — forever.
    ///
    /// Keyed on the models rather than on a date, because that is what makes
    /// a candidate eligible again: evidence gathered under other models is not
    /// evidence about these, and neither is a failure.
    pub async fn tried_candidates(
        &self,
        embed_recipe: &str,
        chat_model: &str,
    ) -> Result<Vec<GenerationParams>> {
        sqlx::query_scalar::<_, String>(
            "SELECT params FROM generations
              WHERE state = 'reverted' AND embed_recipe = ? AND chat_model = ?",
        )
        .bind(embed_recipe)
        .bind(chat_model)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|p| from_json(p))
        .collect()
    }

    /// Every generation, newest first. The journal a person reads.
    pub async fn generation_history(&self, limit: usize) -> Result<Vec<Generation>> {
        sqlx::query(
            "SELECT id, created_at, params, embed_recipe, chat_model, parent_id,
                    run_id, predicted, state
               FROM generations ORDER BY created_at DESC, id DESC LIMIT ?",
        )
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(hydrate)
        .collect()
    }
}

/// The live generation for the running configuration, minting one where the
/// base has none or where the models have changed under it.
///
/// A model change mints a generation rather than editing the live one: a
/// generation is immutable, and the point of the row is that observations
/// collected under it name something that still says what it said. The new
/// era starts from the live generation's parameters, not the file's — a model
/// change moves no knob — and `params` is only what a base with no generation
/// at all starts from.
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
    let params = live.as_ref().map_or(params, |g| g.params);
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

/// What `meta` remembers the file said the last time this base opened. How a
/// boot tells an edited file from an unchanged one.
const FILE_PARAMS_SEEN: &str = "evolve.file_params";

/// The generation a base serves under from this boot on.
///
/// The file holds the operator's starting point and the database holds what
/// is live, and at boot the two can disagree — the idle pass moved a knob
/// while the file kept saying what it said. Who wins is decided by what
/// changed:
///
/// - the file was edited since the last boot, or autonomy is off: the file
///   wins, and a generation is minted from it so the journal shows the hand
///   that moved the knob. Turning the loop off is therefore the way back to
///   the file, exactly.
/// - otherwise the live generation wins, because nothing about the operator's
///   intent changed and the loop's move is the newer fact.
///
/// The caller serves under whatever comes back. Without this a restart would
/// quietly return the ranking to the file while every observation kept being
/// written under a generation that no longer described it.
pub async fn boot_generation(
    store: &Store,
    file: GenerationParams,
    embed_recipe: &str,
    chat_model: &str,
    autonomous: bool,
) -> Result<Generation> {
    let seen: Option<GenerationParams> = match store.meta_get(FILE_PARAMS_SEEN).await? {
        Some(s) => Some(from_json(&s)?),
        None => None,
    };
    let mut live = ensure_generation(store, file, embed_recipe, chat_model).await?;
    let file_wins = !autonomous || seen != Some(file);
    if live.params != file && file_wins {
        tracing::info!(
            recency_weight = file.recency_weight,
            per_source_cap = ?file.per_source_cap,
            "config.toml sets the ranking; the live generation is superseded by it"
        );
        live = restate_generation(store, &live, file).await?;
    }
    store.meta_set(FILE_PARAMS_SEEN, &json(&file)?).await?;
    Ok(live)
}

/// Journal parameters a person set — by editing the file or pressing Apply —
/// as a generation of the same era, child of the one that was live.
///
/// Every ranking change is a named generation, or the numbers gathered after
/// it have nothing to be about. `run_id` and `predicted` stay empty: nothing
/// proposed this and nothing is watching it.
pub async fn restate_generation(
    store: &Store,
    live: &Generation,
    params: GenerationParams,
) -> Result<Generation> {
    if live.params == params {
        return Ok(live.clone());
    }
    let id = store
        .record_generation(&NewGeneration {
            params,
            embed_recipe: live.embed_recipe.clone(),
            chat_model: live.chat_model.clone(),
            parent_id: Some(live.id.clone()),
        })
        .await?;
    store
        .generation(&id)
        .await?
        .ok_or_else(|| Error::Store("generations: the restated generation was not written".into()))
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
    async fn a_reverted_generation_hands_the_base_back_to_its_parent() {
        let store = Store::memory().await.unwrap();
        let first = store.record_generation(&sample()).await.unwrap();
        let mut second = sample();
        second.parent_id = Some(first.clone());
        second.params.recency_weight = 0.25;
        let second_id = store
            .adopt_generation(&second, "run-1", 0.04)
            .await
            .unwrap();

        let back = store
            .revert_generation(&second_id)
            .await
            .unwrap()
            .expect("a parent");
        assert_eq!(back.id, first);
        assert_eq!(back.state, "live");
        assert_eq!(store.live_generation().await.unwrap().unwrap().id, first);
        assert_eq!(
            store.generation(&second_id).await.unwrap().unwrap().state,
            "reverted"
        );
    }

    #[tokio::test]
    async fn a_reverted_candidate_is_not_offered_again() {
        // Without this the pass proposes the same losing candidate every quiet
        // period, adopts it, watches it fail, and reverts — forever.
        let store = Store::memory().await.unwrap();
        let first = store.record_generation(&sample()).await.unwrap();
        let mut second = sample();
        second.parent_id = Some(first);
        second.params.recency_weight = 0.25;
        let id = store
            .adopt_generation(&second, "run-1", 0.04)
            .await
            .unwrap();
        store.revert_generation(&id).await.unwrap();

        let tried = store
            .tried_candidates(&second.embed_recipe, &second.chat_model)
            .await
            .unwrap();
        assert!(tried.iter().any(|p| p.recency_weight == 0.25));
        assert!(
            store
                .tried_candidates(&second.embed_recipe, "another-model")
                .await
                .unwrap()
                .is_empty(),
            "a failure under other models is not a failure under these"
        );
    }

    #[tokio::test]
    async fn a_generation_with_no_parent_cannot_be_reverted() {
        let store = Store::memory().await.unwrap();
        let id = store.record_generation(&sample()).await.unwrap();
        assert!(store.revert_generation(&id).await.unwrap().is_none());
        assert_eq!(
            store.live_generation().await.unwrap().unwrap().id,
            id,
            "a base with nowhere to go back to stays where it is"
        );
    }

    #[tokio::test]
    async fn what_a_generation_promised_is_kept_with_it() {
        let store = Store::memory().await.unwrap();
        let id = store
            .adopt_generation(&sample(), "run-1", 0.04)
            .await
            .unwrap();
        let live = store.live_generation().await.unwrap().unwrap();
        assert_eq!(live.id, id);
        assert_eq!(live.predicted, Some(0.04));
        assert_eq!(live.run_id.as_deref(), Some("run-1"));
    }

    fn p(recency_weight: f32, per_source_cap: Option<usize>) -> GenerationParams {
        GenerationParams {
            recency_weight,
            per_source_cap,
        }
    }

    /// A base the loop has moved: the file says 0.05/3, the live generation
    /// was adopted at 0.25/3.
    async fn moved_by_the_loop(autonomous: bool) -> (Store, String) {
        let store = Store::memory().await.unwrap();
        let file = p(0.05, Some(3));
        let first = boot_generation(&store, file, "recipe-a", "qwen", autonomous)
            .await
            .unwrap();
        let mut adopted = sample();
        adopted.parent_id = Some(first.id);
        adopted.params = p(0.25, Some(3));
        adopted.embed_recipe = "recipe-a".into();
        let id = store
            .adopt_generation(&adopted, "run-1", 0.04)
            .await
            .unwrap();
        (store, id)
    }

    #[tokio::test]
    async fn a_generation_the_loop_adopted_survives_a_restart() {
        // Without this a restart quietly returned the ranking to the file while
        // every observation kept being written under a generation that no
        // longer described it.
        let (store, adopted) = moved_by_the_loop(true).await;
        let g = boot_generation(&store, p(0.05, Some(3)), "recipe-a", "qwen", true)
            .await
            .unwrap();
        assert_eq!(g.id, adopted);
        assert_eq!(
            g.params,
            p(0.25, Some(3)),
            "the base serves what it adopted"
        );
    }

    #[tokio::test]
    async fn an_edited_file_wins_over_what_the_loop_adopted() {
        let (store, adopted) = moved_by_the_loop(true).await;
        let g = boot_generation(&store, p(0.0, Some(3)), "recipe-a", "qwen", true)
            .await
            .unwrap();
        assert_ne!(g.id, adopted);
        assert_eq!(g.params, p(0.0, Some(3)), "a key written in the file wins");
        assert_eq!(
            g.parent_id.as_deref(),
            Some(adopted.as_str()),
            "and the journal shows the hand that moved it"
        );
        assert!(
            g.predicted.is_none(),
            "nothing is watching a change a person made"
        );
    }

    #[tokio::test]
    async fn switching_autonomy_off_returns_the_base_to_the_file() {
        let (store, adopted) = moved_by_the_loop(true).await;
        let g = boot_generation(&store, p(0.05, Some(3)), "recipe-a", "qwen", false)
            .await
            .unwrap();
        assert_ne!(g.id, adopted);
        assert_eq!(
            g.params,
            p(0.05, Some(3)),
            "off leaves today's behaviour exactly"
        );
    }

    #[tokio::test]
    async fn an_unchanged_boot_mints_nothing() {
        let (store, adopted) = moved_by_the_loop(true).await;
        boot_generation(&store, p(0.05, Some(3)), "recipe-a", "qwen", true)
            .await
            .unwrap();
        boot_generation(&store, p(0.05, Some(3)), "recipe-a", "qwen", true)
            .await
            .unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM generations")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(n, 2, "the first boot's and the adoption, nothing more");
        assert_eq!(store.live_generation().await.unwrap().unwrap().id, adopted);
    }

    #[tokio::test]
    async fn a_new_era_starts_from_the_live_parameters_not_the_files() {
        let (store, _) = moved_by_the_loop(true).await;
        let g = boot_generation(&store, p(0.05, Some(3)), "recipe-b", "qwen", true)
            .await
            .unwrap();
        assert_eq!(g.embed_recipe, "recipe-b");
        assert_eq!(g.params, p(0.25, Some(3)), "a model change moves no knob");
    }

    #[tokio::test]
    async fn a_hand_applied_change_is_a_generation_of_the_same_era() {
        let store = Store::memory().await.unwrap();
        let first = ensure_generation(&store, p(0.05, Some(3)), "recipe-a", "qwen")
            .await
            .unwrap();
        let g = restate_generation(&store, &first, p(0.05, Some(5)))
            .await
            .unwrap();
        assert_eq!(g.parent_id.as_deref(), Some(first.id.as_str()));
        assert_eq!(g.embed_recipe, first.embed_recipe);
        assert_eq!(store.live_generation().await.unwrap().unwrap().id, g.id);
        let same = restate_generation(&store, &g, p(0.05, Some(5)))
            .await
            .unwrap();
        assert_eq!(
            same.id, g.id,
            "restating what is already live mints nothing"
        );
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
