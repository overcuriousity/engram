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

/// The knobs a generation holds: everything the idle pass may move. Stored as
/// JSON so the set can widen without a migration — the retrieval knobs and the
/// sitting flip all arrived after the first rows were written, and those rows
/// still read.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GenerationParams {
    pub recency_weight: f32,
    pub per_source_cap: Option<usize>,
    /// Absent in rows written before the retrieval knobs existed, which ran
    /// under the shipped value. The default says so rather than a migration.
    #[serde(default = "crate::config::default_candidate_multiplier")]
    pub candidate_multiplier: usize,
    #[serde(default = "crate::config::default_recency_half_life_days")]
    pub recency_half_life_days: u32,
    /// The three knobs stage 3a put on the ladder. Absent in rows written
    /// before, which ran under the shipped values.
    #[serde(default = "crate::config::default_prime_lift")]
    pub prime_lift: usize,
    #[serde(default = "crate::config::default_spread_max")]
    pub spread_max: usize,
    #[serde(default = "crate::config::default_rerank_knob")]
    pub rerank: bool,
    #[serde(default = "crate::config::default_review_min")]
    pub review_min: f32,
    /// On the ladder later than the rest, so a row from before it decodes as
    /// the shipped `false`.
    #[serde(default = "crate::config::default_sitting_prime")]
    pub sitting_prime: bool,
}

impl Default for GenerationParams {
    fn default() -> Self {
        crate::core::ranking::RankingParams::default().into()
    }
}

impl From<crate::core::ranking::RankingParams> for GenerationParams {
    fn from(p: crate::core::ranking::RankingParams) -> Self {
        Self {
            recency_weight: p.recency_weight,
            per_source_cap: p.per_source_cap,
            candidate_multiplier: p.candidate_multiplier,
            recency_half_life_days: p.recency_half_life_days,
            prime_lift: p.prime_lift,
            spread_max: p.spread_max,
            rerank: p.rerank,
            review_min: p.review_min,
            sitting_prime: p.sitting_prime,
        }
    }
}

impl From<GenerationParams> for crate::core::ranking::RankingParams {
    fn from(p: GenerationParams) -> Self {
        Self {
            recency_weight: p.recency_weight,
            per_source_cap: p.per_source_cap,
            candidate_multiplier: p.candidate_multiplier,
            recency_half_life_days: p.recency_half_life_days,
            prime_lift: p.prime_lift,
            spread_max: p.spread_max,
            rerank: p.rerank,
            review_min: p.review_min,
            sitting_prime: p.sitting_prime,
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

/// Test-only, for the reason `NewArtifact`'s is: in production every field
/// here is a decision, and a field added later must break every call site
/// until somebody answers for it. A fixture has no such duty.
#[cfg(test)]
impl Default for NewGeneration {
    fn default() -> Self {
        Self {
            params: Default::default(),
            embed_recipe: String::new(),
            chat_model: String::new(),
            parent_id: None,
        }
    }
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
    /// `live` | `superseded` | `reverted` | `refused` — the last never was
    /// live: the ladder chose it and rehearsal refused it.
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
        self.insert_live(g, None, None, false)
            .await?
            .ok_or_else(|| Error::Store("generations: an unconditional write was refused".into()))
    }

    /// Record a generation adopted on lived evidence rather than a replay:
    /// no run to name, and `predicted` is the rate that argued for it.
    ///
    /// `None`, and nothing written, where the parent is no longer live — see
    /// `adopt_generation`.
    pub async fn adopt_generation_lived(
        &self,
        g: &NewGeneration,
        predicted: f64,
    ) -> Result<Option<String>> {
        self.insert_live(g, None, Some(predicted), true).await
    }

    /// Record a generation the idle pass chose, carrying the run that chose it
    /// and what it promised, and make it live.
    ///
    /// Only on top of the generation it was measured against. `None`, and
    /// nothing written, where `g.parent_id` is no longer the live one: the pass
    /// reads the live generation when it starts and adopts when it ends, and a
    /// person's Apply in between has made another one live. Superseding that
    /// unread put the loop's candidate back over the person's choice.
    pub async fn adopt_generation(
        &self,
        g: &NewGeneration,
        run_id: &str,
        predicted: f64,
    ) -> Result<Option<String>> {
        self.insert_live(g, Some(run_id), Some(predicted), true)
            .await
    }

    /// `over_parent`: write only if `g.parent_id`, where it names one, is the
    /// live generation — read under the same write lock as the write.
    async fn insert_live(
        &self,
        g: &NewGeneration,
        run_id: Option<&str>,
        predicted: Option<f64>,
        over_parent: bool,
    ) -> Result<Option<String>> {
        let id = new_id();
        let mut tx = self.pool.begin_with(IMMEDIATE).await?;
        if over_parent && let Some(parent) = &g.parent_id {
            let live: Option<String> = sqlx::query_scalar(
                "SELECT id FROM generations WHERE state = 'live'
                  ORDER BY created_at DESC, id DESC LIMIT 1",
            )
            .fetch_optional(&mut *tx)
            .await?;
            if live.as_ref() != Some(parent) {
                return Ok(None);
            }
        }
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
        Ok(Some(id))
    }

    /// A candidate the ladder chose and rehearsal refused. Never live; a row
    /// so `tried_candidates` does not offer it again under these models.
    pub async fn refuse_generation(
        &self,
        g: &NewGeneration,
        run_id: &str,
        predicted: f64,
    ) -> Result<String> {
        let id = new_id();
        sqlx::query(
            "INSERT INTO generations
               (id, created_at, params, embed_recipe, chat_model, parent_id,
                run_id, predicted, state)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'refused')",
        )
        .bind(&id)
        .bind(now())
        .bind(json(&g.params)?)
        .bind(&g.embed_recipe)
        .bind(&g.chat_model)
        .bind(&g.parent_id)
        .bind(run_id)
        .bind(predicted)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// Take a generation back: it becomes `reverted` and its parent is live
    /// again. Returns the parent, or `None` — and changes nothing — for a
    /// generation with nowhere to go back to, or one that is no longer live.
    ///
    /// Cheap and complete because a generation is a row. Nothing in the corpus
    /// was touched by adopting it, so nothing has to be untouched here.
    ///
    /// The second `None` is a person's Apply landing while the idle pass was
    /// measuring the generation it now takes back. Unchecked, the parent was
    /// made live beside the generation the Apply had just minted — two live
    /// rows — and every later pass stopped at the check that the live
    /// generation describes the running parameters, until a restart.
    pub async fn revert_generation(&self, id: &str) -> Result<Option<Generation>> {
        let mut tx = self.pool.begin_with(IMMEDIATE).await?;
        let parent: Option<String> =
            sqlx::query_scalar("SELECT parent_id FROM generations WHERE id = ? AND state = 'live'")
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
              WHERE state IN ('reverted', 'refused') AND embed_recipe = ? AND chat_model = ?",
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
/// - the file was edited since the last boot: the file wins, and a generation
///   is minted from it so the journal shows the hand that moved the knob.
/// - autonomy is off and the live generation is one *the loop* adopted: the
///   file wins for the same reason. Turning the loop off is therefore the way
///   back to the file, exactly.
/// - otherwise the live generation wins, because nothing about the operator's
///   intent changed and the loop's move is the newer fact.
///
/// "The loop adopted it" is `run_id`, and that qualifier is load-bearing: a
/// generation a *person* applied carries none, and reverting one on the
/// autonomy switch reverted the operator rather than the loop. The way in is
/// the ordinary one. `insights::tune_apply` writes `config.toml`, swaps
/// `core.ranking` and restates the generation, but the process-wide `Config`
/// it was built from is loaded once at boot and never reloaded — so the next
/// time this base is evicted and reopened, `generation_check` hands us the
/// *stale* file params, and under `learn.mode = "learning"` (which resolves to
/// `autonomous = "off"`) the old unconditional `!autonomous` undid the Apply in
/// both the journal and the serving core.
///
/// The caller serves under whatever comes back. Without this a restart would
/// quietly return the ranking to the file while every observation kept being
/// written under a generation that no longer described it.
/// Whether the loop chose this generation, as opposed to a person applying it.
///
/// Two ways in, and `run_id` is only the first. The replay sweep names the
/// `eval_runs` row that argued for its candidate, so `run_id` is set — but
/// `tune::adopt_lived`, which adopts on what the band actually earned rather
/// than on a replay, goes through `adopt_generation_lived` and writes
/// `run_id = NULL`. There is no run to name; the evidence is the observations
/// themselves.
///
/// What separates that from a person's Apply is `predicted`. `restate_generation`
/// — the one path a hand reaches, from a `config.toml` edit or the Apply button
/// — leaves both empty, and says so: "nothing proposed this and nothing is
/// watching it". A lived adoption always carries the rate that argued for it.
///
/// So: a run, or a parent and a prediction. `web::insights` already sorts the
/// history by exactly this and renders the middle case as "adopted by the base
/// on what the band earned"; reading it differently here meant a `spread_max`
/// or a `review_min` the loop had chosen was indistinguishable from one
/// somebody typed, and turning autonomy off — which is documented as the way
/// back to the file, exactly — did not bring those knobs back.
///
/// And asked of the generation that set the knobs, which is not always the
/// live one. A model change mints a generation that copies its parent's
/// parameters and names neither a run nor a prediction — nothing proposed it
/// and nothing watches it — so read where it stands it looked like a person's
/// Apply, and switching autonomy off after an embedder change kept every knob
/// the loop had moved. A generation whose models changed and whose parameters
/// did not moved nothing, so the question passes to its parent.
async fn loop_moved(store: &Store, g: &Generation) -> Result<bool> {
    let mut g = g.clone();
    while g.run_id.is_none()
        && g.predicted.is_none()
        && let Some(parent_id) = g.parent_id.clone()
        && let Some(parent) = store.generation(&parent_id).await?
        && parent.params == g.params
        && (parent.embed_recipe != g.embed_recipe || parent.chat_model != g.chat_model)
    {
        g = parent;
    }
    Ok(g.run_id.is_some() || (g.parent_id.is_some() && g.predicted.is_some()))
}

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
    let file_wins = seen != Some(file) || (!autonomous && loop_moved(store, &live).await?);
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
                ..Default::default()
            },
            embed_recipe: "embeddinggemma:768:asym".into(),
            chat_model: "qwen".into(),
            parent_id: None,
        }
    }

    #[test]
    fn a_generation_row_written_before_the_late_knobs_still_reads() {
        let old = r#"{"recency_weight":0.05,"per_source_cap":3,"candidate_multiplier":3,"recency_half_life_days":180}"#;
        let p: GenerationParams = serde_json::from_str(old).unwrap();
        assert_eq!(p.prime_lift, crate::config::default_prime_lift());
        assert_eq!(p.spread_max, crate::config::default_spread_max());
        assert!(p.rerank);
        // The sitting flip joined the ladder later still, and a row from
        // before it ran with the shipped value the same way.
        assert_eq!(p.sitting_prime, crate::config::default_sitting_prime());
        assert!(!p.sitting_prime, "and the shipped value is off");
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
            .unwrap()
            .expect("the parent is live");

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
            .unwrap()
            .expect("the parent is live");
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
            .unwrap()
            .expect("nothing names a parent");
        let live = store.live_generation().await.unwrap().unwrap();
        assert_eq!(live.id, id);
        assert_eq!(live.predicted, Some(0.04));
        assert_eq!(live.run_id.as_deref(), Some("run-1"));
    }

    fn p(recency_weight: f32, per_source_cap: Option<usize>) -> GenerationParams {
        GenerationParams {
            recency_weight,
            per_source_cap,
            ..Default::default()
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
            .unwrap()
            .expect("the parent is live");
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

    /// The loop adopts two ways, and only one of them names a run.
    ///
    /// `tune::adopt_lived` adopts on what the band actually earned rather than
    /// on a replay, so there is no `eval_runs` row to point at and
    /// `adopt_generation_lived` writes `run_id = NULL`. Read as `run_id`
    /// alone, a `spread_max` or a `review_min` the loop had chosen was
    /// indistinguishable from one somebody typed, and switching autonomy off —
    /// which is documented as the way back to the file, exactly — left those
    /// knobs where the loop had put them.
    #[tokio::test]
    async fn switching_autonomy_off_returns_the_base_to_the_file_after_a_lived_adoption() {
        let store = Store::memory().await.unwrap();
        let file = p(0.05, Some(3));
        let first = boot_generation(&store, file, "recipe-a", "qwen", true)
            .await
            .unwrap();
        let mut adopted = sample();
        adopted.parent_id = Some(first.id);
        adopted.params = p(0.25, Some(3));
        adopted.embed_recipe = "recipe-a".into();
        // The lived path: a prediction, and no run to name.
        let id = store
            .adopt_generation_lived(&adopted, 0.62)
            .await
            .unwrap()
            .expect("the parent is live");
        assert!(
            store
                .generation(&id)
                .await
                .unwrap()
                .unwrap()
                .run_id
                .is_none(),
            "a lived adoption names no run, which is the whole trap"
        );

        let g = boot_generation(&store, file, "recipe-a", "qwen", false)
            .await
            .unwrap();
        assert_ne!(g.id, id);
        assert_eq!(
            g.params,
            p(0.05, Some(3)),
            "off is the way back to the file for every knob the loop moved"
        );
    }

    /// And it still survives while autonomy is on, like any other adoption.
    #[tokio::test]
    async fn a_lived_adoption_survives_a_restart_with_autonomy_on() {
        let store = Store::memory().await.unwrap();
        let file = p(0.05, Some(3));
        let first = boot_generation(&store, file, "recipe-a", "qwen", true)
            .await
            .unwrap();
        let mut adopted = sample();
        adopted.parent_id = Some(first.id);
        adopted.params = p(0.25, Some(3));
        adopted.embed_recipe = "recipe-a".into();
        let id = store
            .adopt_generation_lived(&adopted, 0.62)
            .await
            .unwrap()
            .expect("the parent is live");

        let g = boot_generation(&store, file, "recipe-a", "qwen", true)
            .await
            .unwrap();
        assert_eq!(g.id, id, "the base serves what it adopted");
    }

    #[tokio::test]
    async fn switching_autonomy_off_does_not_revert_what_a_person_applied() {
        // The loop's adoptions go back to the file when autonomy is switched
        // off; a person's Apply must not, and `run_id` is what tells them
        // apart. The way in is ordinary and had nothing to do with autonomy:
        // `insights::tune_apply` writes `config.toml`, but the process-wide
        // `Config` a core is rebuilt from is loaded once at boot, so the next
        // open hands us the *stale* file params — and under
        // `learn.mode = "learning"`, which resolves to `autonomous = "off"`,
        // the old unconditional `!autonomous` undid the operator in both the
        // journal and the serving core.
        let store = Store::memory().await.unwrap();
        let file = p(0.05, Some(3));
        let first = boot_generation(&store, file, "recipe-a", "qwen", false)
            .await
            .unwrap();
        let applied = restate_generation(&store, &first, p(0.25, Some(3)))
            .await
            .unwrap();
        assert!(
            applied.run_id.is_none(),
            "nobody proposed this; a person did"
        );

        // The file the reopened core is built from is the stale one.
        let g = boot_generation(&store, file, "recipe-a", "qwen", false)
            .await
            .unwrap();
        assert_eq!(g.id, applied.id, "the Apply survives the reopen");
        assert_eq!(g.params, p(0.25, Some(3)), "and is what the base serves");
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

    /// The era a model change starts copies the loop's knobs and names no run
    /// and no prediction. Switching autonomy off still has to find who moved
    /// them, or the way back to the file stops at the first embedder change.
    #[tokio::test]
    async fn switching_autonomy_off_after_a_model_change_still_returns_the_base_to_the_file() {
        let (store, _) = moved_by_the_loop(true).await;
        let era = boot_generation(&store, p(0.05, Some(3)), "recipe-b", "qwen", true)
            .await
            .unwrap();
        assert!(era.run_id.is_none() && era.predicted.is_none());
        assert_eq!(era.params, p(0.25, Some(3)));

        let g = boot_generation(&store, p(0.05, Some(3)), "recipe-b", "qwen", false)
            .await
            .unwrap();
        assert_eq!(
            g.params,
            p(0.05, Some(3)),
            "the loop's knobs outlived the switch that is the way back to the file"
        );
    }

    /// And a person's Apply carried across a model change is still theirs.
    #[tokio::test]
    async fn a_persons_apply_carried_across_a_model_change_survives_autonomy_off() {
        let store = Store::memory().await.unwrap();
        let file = p(0.05, Some(3));
        let first = boot_generation(&store, file, "recipe-a", "qwen", false)
            .await
            .unwrap();
        restate_generation(&store, &first, p(0.25, Some(3)))
            .await
            .unwrap();
        let g = boot_generation(&store, file, "recipe-b", "qwen", false)
            .await
            .unwrap();
        assert_eq!(g.embed_recipe, "recipe-b");
        assert_eq!(g.params, p(0.25, Some(3)));
    }

    /// The idle pass reads the live generation when it starts and writes when
    /// it ends. A person's Apply in between is the newer fact, and neither a
    /// revert nor an adoption measured against the generation it replaced may
    /// write over it.
    #[tokio::test]
    async fn a_revert_or_an_adoption_over_a_generation_no_longer_live_changes_nothing() {
        let (store, adopted) = moved_by_the_loop(true).await;
        let watched = store.generation(&adopted).await.unwrap().unwrap();
        let applied = restate_generation(&store, &watched, p(0.5, Some(3)))
            .await
            .unwrap();

        assert!(
            store.revert_generation(&adopted).await.unwrap().is_none(),
            "a generation the Apply superseded was taken back over it"
        );
        let mut child = sample();
        child.parent_id = Some(adopted.clone());
        child.params = p(0.75, Some(3));
        assert!(
            store
                .adopt_generation_lived(&child, 0.5)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .adopt_generation(&child, "run-2", 0.1)
                .await
                .unwrap()
                .is_none()
        );

        let live: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM generations WHERE state = 'live'")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(live, 1, "two generations live at once");
        assert_eq!(
            store.live_generation().await.unwrap().unwrap().id,
            applied.id
        );
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

    #[test]
    fn a_generation_written_before_the_retrieval_knobs_still_reads() {
        // Stage 1 and 2 rows. A migration here would be a recreated database,
        // which is the price this schema charges and not one a knob may cost.
        let p: GenerationParams =
            from_json(r#"{"recency_weight":0.05,"per_source_cap":3}"#).unwrap();
        assert_eq!(p.candidate_multiplier, 3);
        assert_eq!(p.recency_half_life_days, 180);
    }

    #[test]
    fn the_two_shapes_of_the_parameters_round_trip() {
        let r = crate::core::ranking::RankingParams {
            recency_weight: 0.1,
            per_source_cap: None,
            candidate_multiplier: 5,
            recency_half_life_days: 90,
            prime_lift: 2,
            spread_max: 5,
            rerank: false,
            review_min: 0.84,
            sitting_prime: true,
        };
        let back: crate::core::ranking::RankingParams = GenerationParams::from(r).into();
        assert_eq!(back, r);
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
