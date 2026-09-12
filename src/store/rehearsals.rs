//! Probes: questions the base can ask itself about one artifact.
//!
//! Two classes and a rule. A `capture` probe is the text of a later capture
//! that landed near this artifact at integration — a query written by a
//! person who was not looking at the answer, which is the standard verdicts
//! are held to. A `cue` probe is a question a model-written artifact was
//! written for. The rule: nothing is ever minted from an artifact's own
//! title, body or tags, because a query written while looking at the answer
//! passes on every system ever built.

use super::feedback::{blob_to_vec, vec_to_blob};
use super::{Cursor, Store, new_id, now};
use crate::error::{Error, Result};
use sqlx::Row;

/// Distinct probes an owner's retained results have to come from before they
/// are an agreement rather than one observation counted twice.
///
/// The rehearsal lap wraps, so on an unchanged base the same probe is replayed
/// on consecutive nights and writes identical rows. Two rows off one probe is
/// one reading repeated; two probes is two questions somebody asked that both
/// landed on the artifact, which is what `sleep::interferers` and
/// `sleep::condense_candidates` claim to have when they act.
pub const MIN_AGREEING_PROBES: usize = 2;

/// Results kept per probe, the oldest dropped as newer ones are written.
///
/// Rows per probe rather than days is the bound this table needs: storage
/// becomes a function of how many probes exist instead of how long the base
/// has been running. A pass writes up to `OBSERVATION_LIMIT` rows, each
/// carrying a JSON list of ids, and every pass runs window functions
/// (`fragile_rehearsals`, `latest_results_under`) over all of them.
///
/// Ten, from what reads back further than the last row. `fragile_rehearsals`
/// compares the last two under a generation; `retract::spanning` pairs a
/// probe's last result before a condensation against its first after, so the
/// older side has to survive the passes between one lap and the next. Past
/// that nothing looks, and `MIN_AGREEING_PROBES` counts probes rather than
/// rows, so keeping more history buys it nothing at all.
const KEEP_PER_PROBE: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Capture,
    Cue,
}

impl Class {
    pub fn as_str(self) -> &'static str {
        match self {
            Class::Capture => "capture",
            Class::Cue => "cue",
        }
    }
    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "capture" => Class::Capture,
            "cue" => Class::Cue,
            other => return Err(Error::Store(format!("rehearsals: unknown class {other}"))),
        })
    }
}

#[derive(Debug, Clone)]
pub struct NewRehearsal {
    pub class: Class,
    pub query: String,
    pub query_vec: Vec<f32>,
    pub embed_model: String,
    pub artifact_id: String,
    pub source_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Rehearsal {
    pub id: String,
    pub created_at: i64,
    pub class: Class,
    pub query: String,
    pub query_vec: Vec<f32>,
    pub embed_model: String,
    pub artifact_id: String,
    pub source_id: Option<String>,
    pub retired_at: Option<i64>,
}

const COLUMNS: &str =
    "id, created_at, class, query, query_vec, embed_model, artifact_id, source_id, retired_at";
/// The same columns off an aliased `rehearsals r`, for the joined reads.
const QUALIFIED: &str = "r.id, r.created_at, r.class, r.query, r.query_vec, r.embed_model, r.artifact_id, r.source_id, r.retired_at";

#[derive(Debug, Clone)]
pub struct NewResult {
    pub rehearsal_id: String,
    pub generation_id: String,
    pub rank: Option<i64>,
    pub outranked_by: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RehearsalResult {
    pub id: String,
    pub rehearsal_id: String,
    pub generation_id: String,
    pub at: i64,
    pub rank: Option<i64>,
    pub outranked_by: Vec<String>,
}

fn outranked(json: &str) -> Result<Vec<String>> {
    serde_json::from_str(json)
        .map_err(|e| Error::Store(format!("rehearsal_results.outranked_by: {e}")))
}

fn read_result(r: &sqlx::sqlite::SqliteRow) -> Result<RehearsalResult> {
    Ok(RehearsalResult {
        id: r.get("id"),
        rehearsal_id: r.get("rehearsal_id"),
        generation_id: r.get("generation_id"),
        at: r.get("at"),
        rank: r.get("rank"),
        outranked_by: outranked(&r.get::<String, _>("outranked_by"))?,
    })
}

fn read(r: &sqlx::sqlite::SqliteRow) -> Result<Rehearsal> {
    Ok(Rehearsal {
        id: r.get("id"),
        created_at: r.get("created_at"),
        class: Class::parse(&r.get::<String, _>("class"))?,
        query: r.get("query"),
        query_vec: blob_to_vec(&r.get::<Vec<u8>, _>("query_vec")),
        embed_model: r.get("embed_model"),
        artifact_id: r.get("artifact_id"),
        source_id: r.get("source_id"),
        retired_at: r.get("retired_at"),
    })
}

impl Store {
    /// `None` when the same (artifact, class, query) is already *live*: a
    /// probe is written once, however often the unit that mints it runs.
    ///
    /// Live, and not ever: a retired row is history and does not stand in the
    /// way of the question being asked again under a new embedder. See
    /// `idx_rehearsals_live`.
    pub async fn record_rehearsal(&self, r: &NewRehearsal) -> Result<Option<String>> {
        let id = new_id();
        let res = sqlx::query(
            "INSERT OR IGNORE INTO rehearsals
               (id, created_at, class, query, query_vec, vec_dim, embed_model, artifact_id, source_id)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(now())
        .bind(r.class.as_str())
        .bind(&r.query)
        .bind(vec_to_blob(&r.query_vec))
        .bind(r.query_vec.len() as i64)
        .bind(&r.embed_model)
        .bind(&r.artifact_id)
        .bind(&r.source_id)
        .execute(&self.pool)
        .await?;
        Ok((res.rows_affected() > 0).then_some(id))
    }

    pub async fn rehearsal(&self, id: &str) -> Result<Option<Rehearsal>> {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM rehearsals WHERE id = ?"
        )))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .as_ref()
        .map(read)
        .transpose()
    }

    /// Live probes after `cursor`, in `(created_at, id)` order. The pair and
    /// not a bare stamp: the clock is seconds, and a limit that cuts inside
    /// one second would otherwise lose the rest of it.
    pub async fn rehearsals_after(&self, cursor: &Cursor, limit: usize) -> Result<Vec<Rehearsal>> {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM rehearsals
              WHERE retired_at IS NULL
                AND (created_at > ? OR (created_at = ? AND id > ?))
              ORDER BY created_at ASC, id ASC
              LIMIT ?"
        )))
        .bind(cursor.at)
        .bind(cursor.at)
        .bind(&cursor.id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(read)
        .collect()
    }

    /// Live probes for one artifact, oldest first.
    pub async fn rehearsals_of(&self, artifact_id: &str) -> Result<Vec<Rehearsal>> {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM rehearsals
              WHERE artifact_id = ? AND retired_at IS NULL
              ORDER BY created_at ASC, id ASC"
        )))
        .bind(artifact_id)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(read)
        .collect()
    }

    pub async fn retire_rehearsal(&self, id: &str, at: i64) -> Result<()> {
        sqlx::query("UPDATE rehearsals SET retired_at = ? WHERE id = ? AND retired_at IS NULL")
            .bind(at)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Retire every live probe of this artifact. Rows stamped.
    pub async fn retire_rehearsals_of(&self, artifact_id: &str, at: i64) -> Result<u64> {
        Ok(sqlx::query(
            "UPDATE rehearsals SET retired_at = ? WHERE artifact_id = ? AND retired_at IS NULL",
        )
        .bind(at)
        .bind(artifact_id)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    pub async fn live_rehearsal_count(&self) -> Result<i64> {
        Ok(
            sqlx::query_scalar("SELECT COUNT(*) FROM rehearsals WHERE retired_at IS NULL")
                .fetch_one(&self.pool)
                .await?,
        )
    }

    pub async fn record_rehearsal_result(&self, r: &NewResult) -> Result<String> {
        let id = new_id();
        sqlx::query(
            "INSERT INTO rehearsal_results (id, rehearsal_id, generation_id, at, rank, outranked_by)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&r.rehearsal_id)
        .bind(&r.generation_id)
        .bind(now())
        .bind(r.rank)
        .bind(serde_json::to_string(&r.outranked_by).unwrap_or_else(|_| "[]".into()))
        .execute(&self.pool)
        .await?;
        // And everything this probe has past `KEEP_PER_PROBE`, here rather
        // than on a sweep. Retention only runs on `feedback.retain_days`,
        // which ships at "keep for ever" — so the table grew by up to
        // `OBSERVATION_LIMIT` JSON-carrying rows a pass with nothing ever
        // taking any of them out, and every pass ran window functions over
        // the lot.
        sqlx::query(
            "DELETE FROM rehearsal_results
              WHERE rehearsal_id = ? AND id NOT IN (
                    SELECT id FROM rehearsal_results WHERE rehearsal_id = ?
                     ORDER BY at DESC, id DESC LIMIT ?)",
        )
        .bind(&r.rehearsal_id)
        .bind(&r.rehearsal_id)
        .bind(KEEP_PER_PROBE as i64)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// Newest first, at most `limit`.
    pub async fn results_of(
        &self,
        rehearsal_id: &str,
        limit: usize,
    ) -> Result<Vec<RehearsalResult>> {
        sqlx::query(
            "SELECT id, rehearsal_id, generation_id, at, rank, outranked_by
               FROM rehearsal_results WHERE rehearsal_id = ?
              ORDER BY at DESC, id DESC LIMIT ?",
        )
        .bind(rehearsal_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(read_result)
        .collect()
    }

    /// The same, bounded to one generation.
    ///
    /// What every *rule* reading retained results wants, and `results_of` is
    /// not it. A rehearsal result is a rank measured under one set of ranking
    /// parameters, so results from two generations are two different
    /// measurements and comparing them across the boundary compares the knobs
    /// rather than the base. Nothing expires them either at the default
    /// `retain_days = 0`, so an unscoped read is the whole history of the
    /// artifact for ever.
    ///
    /// That is not academic. `sleep::condense_candidates` refuses an owner any
    /// of whose results missed (`rank IS NULL`), and `condense_artifact` sets
    /// `embed_state = 'pending'` — so the first rehearsal after a condensation
    /// records exactly such a miss, and unscoped it disabled condense *and*
    /// interference for that owner permanently, on evidence the base itself
    /// had manufactured one generation ago.
    pub async fn results_of_under(
        &self,
        rehearsal_id: &str,
        generation_id: &str,
        limit: usize,
    ) -> Result<Vec<RehearsalResult>> {
        sqlx::query(
            "SELECT id, rehearsal_id, generation_id, at, rank, outranked_by
               FROM rehearsal_results WHERE rehearsal_id = ? AND generation_id = ?
              ORDER BY at DESC, id DESC LIMIT ?",
        )
        .bind(rehearsal_id)
        .bind(generation_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(read_result)
        .collect()
    }

    /// The most recent rank each of these probes was measured at under this
    /// generation. Absent from the map where the probe has no result yet.
    ///
    /// One query for a whole batch, because the caller has the batch in hand
    /// and the alternative is a point lookup per probe on a pass that already
    /// runs `OBSERVATION_LIMIT` searches. The value is nested `Option`
    /// deliberately: absent means never measured, `Some(None)` means measured
    /// and missed, and those are different facts.
    pub async fn latest_ranks_under(
        &self,
        rehearsal_ids: &[String],
        generation_id: &str,
    ) -> Result<std::collections::HashMap<String, Option<i64>>> {
        if rehearsal_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let holes = vec!["?"; rehearsal_ids.len()].join(", ");
        let mut q = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT rehearsal_id, rank FROM (
               SELECT rehearsal_id, rank,
                      ROW_NUMBER() OVER (PARTITION BY rehearsal_id ORDER BY at DESC, id DESC) AS n
                 FROM rehearsal_results
                WHERE generation_id = ? AND rehearsal_id IN ({holes})
             ) WHERE n = 1"
        )))
        .bind(generation_id);
        for id in rehearsal_ids {
            q = q.bind(id);
        }
        Ok(q.fetch_all(&self.pool)
            .await?
            .iter()
            .map(|r| (r.get("rehearsal_id"), r.get("rank")))
            .collect())
    }

    /// Live probes whose last two results under `generation_id` disagree —
    /// found then not, or a different rank, NULL counted as its own value.
    /// What wobbles is what needs rehearsing. Oldest result first.
    pub async fn fragile_rehearsals(
        &self,
        generation_id: &str,
        limit: usize,
    ) -> Result<Vec<Rehearsal>> {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "WITH ranked AS (
               SELECT rehearsal_id, rank, at,
                      ROW_NUMBER() OVER (PARTITION BY rehearsal_id ORDER BY at DESC, id DESC) AS n
                 FROM rehearsal_results WHERE generation_id = ?
             ),
             last_two AS (
               SELECT a.rehearsal_id, a.at
                 FROM ranked a JOIN ranked b
                   ON a.rehearsal_id = b.rehearsal_id AND a.n = 1 AND b.n = 2
                WHERE a.rank IS NOT b.rank
             )
             SELECT {QUALIFIED} FROM rehearsals r
               JOIN last_two l ON l.rehearsal_id = r.id
              WHERE r.retired_at IS NULL
              ORDER BY l.at ASC, r.id ASC LIMIT ?"
        )))
        .bind(generation_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(read)
        .collect()
    }

    /// The latest result per live probe under `generation_id`, with the
    /// probe. Newest results first, at most `limit`.
    pub async fn latest_results_under(
        &self,
        generation_id: &str,
        limit: usize,
    ) -> Result<Vec<(Rehearsal, RehearsalResult)>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "WITH latest AS (
               SELECT id, rehearsal_id, at, rank, outranked_by,
                      ROW_NUMBER() OVER (PARTITION BY rehearsal_id ORDER BY at DESC, id DESC) AS n
                 FROM rehearsal_results WHERE generation_id = ?
             )
             SELECT {QUALIFIED},
                    x.id AS x_id, x.at AS x_at, x.rank AS x_rank, x.outranked_by AS x_outranked_by
               FROM latest x JOIN rehearsals r ON r.id = x.rehearsal_id
              WHERE x.n = 1 AND r.retired_at IS NULL
              ORDER BY x.at DESC, x.id DESC LIMIT ?"
        )))
        .bind(generation_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                let probe = read(r)?;
                let result = RehearsalResult {
                    id: r.get("x_id"),
                    rehearsal_id: probe.id.clone(),
                    generation_id: generation_id.to_string(),
                    at: r.get("x_at"),
                    rank: r.get("x_rank"),
                    outranked_by: outranked(&r.get::<String, _>("x_outranked_by"))?,
                };
                Ok((probe, result))
            })
            .collect()
    }

    /// Artifacts in results that nothing has ever asked for: no live probe,
    /// and no positive observation naming them. The count, and the oldest
    /// `limit` as `(id, title)`.
    pub async fn unrehearsed(&self, limit: usize) -> Result<(i64, Vec<(String, Option<String>)>)> {
        const WHERE: &str = "a.status = 'active' AND a.superseded_by IS NULL AND a.reaped_at IS NULL
                AND a.embed_state = 'embedded'
                AND NOT EXISTS (SELECT 1 FROM rehearsals r WHERE r.artifact_id = a.id AND r.retired_at IS NULL)
                AND NOT EXISTS (SELECT 1 FROM observations o WHERE o.artifact_id = a.id AND o.strength > 0 AND o.excluded_at IS NULL)";
        let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*) FROM artifacts a WHERE {WHERE}"
        )))
        .fetch_one(&self.pool)
        .await?;
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT a.id, a.title FROM artifacts a WHERE {WHERE} ORDER BY a.created_at ASC, a.id ASC LIMIT ?"
        )))
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok((
            count,
            rows.iter().map(|r| (r.get("id"), r.get("title"))).collect(),
        ))
    }

    /// Results older than `retain_days`. Zero keeps for ever, as it does for
    /// the observations this shares a clock with.
    ///
    /// Not the bound on this table — `KEEP_PER_PROBE` is, and it holds at the
    /// shipped zero. This is the operator's separate wish that nothing older
    /// than a window be kept anywhere, and the rehearsal results are part of
    /// "anywhere".
    pub async fn expire_rehearsal_results(&self, retain_days: i64) -> Result<u64> {
        if retain_days <= 0 {
            return Ok(0);
        }
        Ok(sqlx::query("DELETE FROM rehearsal_results WHERE at < ?")
            .bind(now() - retain_days * 86_400)
            .execute(&self.pool)
            .await?
            .rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    pub(crate) async fn owner(store: &Store) -> String {
        let src = store.insert_corpus("raw", "web", None).await.unwrap();
        let new = vec![crate::store::artifacts::NewArtifact {
            text: "the image will not mount".into(),
            ..Default::default()
        }];
        store.insert_artifacts(&src.id, &new).await.unwrap()[0]
            .id
            .clone()
    }

    pub(crate) fn probe(owner: &str, q: &str) -> NewRehearsal {
        NewRehearsal {
            class: Class::Capture,
            query: q.into(),
            query_vec: vec![0.1, 0.2, 0.3],
            embed_model: "fake".into(),
            artifact_id: owner.into(),
            source_id: Some("src".into()),
        }
    }

    #[tokio::test]
    async fn a_probe_is_recorded_once_and_walked_on_a_cursor_until_retired() {
        let store = Store::memory().await.unwrap();
        let o = owner(&store).await;
        let a = store
            .record_rehearsal(&probe(&o, "why won't it mount"))
            .await
            .unwrap()
            .unwrap();
        assert!(
            store
                .record_rehearsal(&probe(&o, "why won't it mount"))
                .await
                .unwrap()
                .is_none()
        );
        let b = store
            .record_rehearsal(&probe(&o, "mount fails on boot"))
            .await
            .unwrap()
            .unwrap();

        let all = store
            .rehearsals_after(&Cursor::default(), 10)
            .await
            .unwrap();
        assert_eq!(
            all.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            [a.as_str(), b.as_str()]
        );
        let read = store.rehearsal(&a).await.unwrap().unwrap();
        assert_eq!(read.query_vec, vec![0.1, 0.2, 0.3]);
        assert_eq!(read.class, Class::Capture);

        let after_a = Cursor {
            at: read.created_at,
            id: a.clone(),
        };
        let rest = store.rehearsals_after(&after_a, 10).await.unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].id, b);

        store.retire_rehearsal(&a, 5).await.unwrap();
        assert_eq!(store.live_rehearsal_count().await.unwrap(), 1);
        assert_eq!(
            store.rehearsals_of(&o).await.unwrap().len(),
            1,
            "retired probes are not listed"
        );
        assert_eq!(store.retire_rehearsals_of(&o, 6).await.unwrap(), 1);
        assert_eq!(store.live_rehearsal_count().await.unwrap(), 0);
    }

    /// Retirement is history, not a bar. The uniqueness used to be total, so
    /// a probe retired for its embedder blocked the fresh one minted at the
    /// new model, and an artifact that had lost its probes never got them
    /// back — which is the rehearsal anchor switched off for good on any base
    /// that changed embedder.
    #[tokio::test]
    async fn a_retired_probe_does_not_block_the_same_question_being_asked_again() {
        let store = Store::memory().await.unwrap();
        let o = owner(&store).await;
        let first = store
            .record_rehearsal(&probe(&o, "why won't it mount"))
            .await
            .unwrap()
            .unwrap();
        store.retire_rehearsal(&first, 5).await.unwrap();

        let again = store
            .record_rehearsal(&NewRehearsal {
                embed_model: "another-embedder".into(),
                query_vec: vec![0.9, 0.8, 0.7],
                ..probe(&o, "why won't it mount")
            })
            .await
            .unwrap()
            .expect("the question can be asked again under a new embedder");
        assert_ne!(again, first);
        let live = store.rehearsals_of(&o).await.unwrap();
        assert_eq!(live.len(), 1, "one live probe, not two");
        assert_eq!(live[0].id, again);
        assert_eq!(live[0].embed_model, "another-embedder");

        // And the bar still stands between two live rows.
        assert!(
            store
                .record_rehearsal(&probe(&o, "why won't it mount"))
                .await
                .unwrap()
                .is_none(),
            "a live probe is still written once"
        );
    }

    pub(crate) async fn generation(store: &Store) -> String {
        store
            .record_generation(&crate::store::generations::NewGeneration {
                params: crate::core::ranking::RankingParams::default().into(),
                embed_recipe: "fake".into(),
                chat_model: "fake".into(),
                ..Default::default()
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn results_are_kept_newest_first_and_a_probe_that_wobbles_is_fragile() {
        let store = Store::memory().await.unwrap();
        let o = owner(&store).await;
        let g = generation(&store).await;
        let steady = store
            .record_rehearsal(&probe(&o, "steady"))
            .await
            .unwrap()
            .unwrap();
        let wobbly = store
            .record_rehearsal(&probe(&o, "wobbly"))
            .await
            .unwrap()
            .unwrap();
        let once = store
            .record_rehearsal(&probe(&o, "once"))
            .await
            .unwrap()
            .unwrap();
        let res = |r: &str, rank: Option<i64>| NewResult {
            rehearsal_id: r.into(),
            generation_id: g.clone(),
            rank,
            outranked_by: vec!["x".into()],
        };
        store
            .record_rehearsal_result(&res(&steady, Some(2)))
            .await
            .unwrap();
        store
            .record_rehearsal_result(&res(&steady, Some(2)))
            .await
            .unwrap();
        store
            .record_rehearsal_result(&res(&wobbly, Some(1)))
            .await
            .unwrap();
        store
            .record_rehearsal_result(&res(&wobbly, None))
            .await
            .unwrap();
        store
            .record_rehearsal_result(&res(&once, Some(3)))
            .await
            .unwrap();

        let w = store.results_of(&wobbly, 10).await.unwrap();
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].rank, None, "newest first");
        assert_eq!(w[0].outranked_by, vec!["x".to_string()]);

        let fragile = store.fragile_rehearsals(&g, 10).await.unwrap();
        assert_eq!(
            fragile.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            [wobbly.as_str()]
        );

        let latest = store.latest_results_under(&g, 10).await.unwrap();
        assert_eq!(latest.len(), 3, "one row per live probe");
        assert!(
            latest
                .iter()
                .any(|(r, x)| r.id == wobbly && x.rank.is_none())
        );

        store.retire_rehearsal(&once, 1).await.unwrap();
        assert_eq!(store.latest_results_under(&g, 10).await.unwrap().len(), 2);
        assert_eq!(
            store.expire_rehearsal_results(0).await.unwrap(),
            0,
            "zero keeps for ever"
        );
    }

    #[tokio::test]
    async fn unrehearsed_is_what_no_probe_and_no_positive_observation_names() {
        let store = Store::memory().await.unwrap();
        let src = store.insert_corpus("raw", "web", None).await.unwrap();
        let new: Vec<_> = ["probed", "opened", "nothing"]
            .iter()
            .enumerate()
            .map(|(i, t)| crate::store::artifacts::NewArtifact {
                ordinal: i as i64,
                text: t.to_string(),
                title: Some(t.to_string()),
                ..Default::default()
            })
            .collect();
        let ids: Vec<String> = store
            .insert_artifacts(&src.id, &new)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        for id in &ids {
            store.mark_embedded(id, "fake", 0).await.unwrap();
        }
        let g = generation(&store).await;
        store.record_rehearsal(&probe(&ids[0], "q")).await.unwrap();
        store
            .record_observation(&crate::store::observations::NewObservation {
                generation_id: g,
                query: "q".into(),
                query_vec: vec![0.1],
                embed_model: "fake".into(),
                artifact_id: Some(ids[1].clone()),
                rank: Some(1),
                source: crate::store::observations::Source::Opened,
                event_id: None,
            })
            .await
            .unwrap();
        let (count, list) = store.unrehearsed(10).await.unwrap();
        assert_eq!(count, 1);
        assert_eq!(list, vec![(ids[2].clone(), Some("nothing".to_string()))]);
    }
}
