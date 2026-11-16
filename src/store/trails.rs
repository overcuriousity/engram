//! Trails: queries a note was confirmed to answer, kept with the query's vector so search can
//! find the note through similar queries. A trail's tier is read off its sources, never stored.

use super::feedback::{blob_to_vec, vec_to_blob};
use super::{Store, new_id, now};
use crate::error::{Error, Result};
use crate::vector::cosine;

/// Cosine at or above which a pairing is the same trail rather than a new one.
pub const NEAR: f32 = 0.95;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrailSource {
    Verdict,
    Ask,
    Decide,
    Label,
    Deep,
}

impl TrailSource {
    pub fn as_str(self) -> &'static str {
        match self {
            TrailSource::Verdict => "verdict",
            TrailSource::Ask => "ask",
            TrailSource::Decide => "decide",
            TrailSource::Label => "label",
            TrailSource::Deep => "deep",
        }
    }

    pub fn by_a_person(self) -> bool {
        matches!(
            self,
            TrailSource::Verdict | TrailSource::Ask | TrailSource::Decide
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Person,
    Model,
}

pub struct NewTrail<'a> {
    pub artifact_id: &'a str,
    pub query: &'a str,
    pub query_vec: &'a [f32],
    pub embed_model: &'a str,
    pub source: TrailSource,
    pub source_id: &'a str,
}

#[derive(Debug, Clone)]
pub struct Trail {
    pub id: String,
    pub artifact_id: String,
    pub query: String,
    pub vec: Vec<f32>,
    pub tier: Tier,
    pub matched: i64,
    pub missed: i64,
}

/// An event's words, vector and embed model, as stored.
pub struct Pairing {
    pub query: String,
    pub vec: Vec<u8>,
    pub embed_model: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrailCounts {
    pub live: i64,
    pub notes: i64,
    pub by_person: i64,
    /// Searches that showed a candidate a trail lifted or brought.
    pub found_through: i64,
}

fn tier(by_person: i64) -> Tier {
    if by_person != 0 {
        Tier::Person
    } else {
        Tier::Model
    }
}

impl Store {
    /// Adds the pairing as a source of the live trail on this note within NEAR (0.95) of the
    /// query, else makes a trail; then enforces `per_note` and `max`. Returns the trail's id.
    pub async fn mint_trail(
        &self,
        t: &NewTrail<'_>,
        per_note: usize,
        max: usize,
    ) -> Result<String> {
        let mut tx = self.pool.begin().await?;
        let held = sqlx::query!(
            r#"SELECT id AS "id!", query AS "query!", query_vec AS "query_vec!", embed_model AS "embed_model!"
                 FROM trails WHERE artifact_id = ? AND retired_at IS NULL"#,
            t.artifact_id
        )
        .fetch_all(&mut *tx)
        .await?;
        // The same words are the same trail whatever their vector; the unique index says so.
        let id = held
            .iter()
            .find(|r| r.query == t.query)
            .or_else(|| {
                held.iter().find(|r| {
                    r.embed_model == t.embed_model
                        && cosine(&blob_to_vec(&r.query_vec), t.query_vec) >= NEAR
                })
            })
            .map(|r| r.id.clone());
        let at = now();
        let id = match id {
            Some(id) => id,
            None => {
                let id = new_id();
                sqlx::query!(
                    "INSERT INTO trails
                       (id, artifact_id, query, query_vec, vec_dim, embed_model, created_at, used_at)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                    &id,
                    t.artifact_id,
                    t.query,
                    vec_to_blob(t.query_vec),
                    t.query_vec.len() as i64,
                    t.embed_model,
                    at,
                    at
                )
                .execute(&mut *tx)
                .await?;
                id
            }
        };
        let source = t.source.as_str();
        sqlx::query!(
            "INSERT OR IGNORE INTO trail_sources (trail_id, source, source_id, at) VALUES (?, ?, ?, ?)",
            &id,
            source,
            t.source_id,
            at
        )
        .execute(&mut *tx)
        .await?;

        // Model tier goes before person tier, then the oldest. The trail just made or joined stays.
        let over = |live: i64, cap: usize| (live - cap as i64).max(0);
        let on_note = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM trails WHERE artifact_id = ? AND retired_at IS NULL",
            t.artifact_id
        )
        .fetch_one(&mut *tx)
        .await?;
        let n = over(on_note, per_note);
        if n > 0 {
            sqlx::query!(
                "UPDATE trails SET retired_at = ? WHERE id IN (
                   SELECT id FROM trails
                    WHERE artifact_id = ? AND retired_at IS NULL AND id != ?
                    ORDER BY EXISTS (SELECT 1 FROM trail_sources s WHERE s.trail_id = trails.id
                                        AND s.source IN ('verdict', 'ask', 'decide')),
                             created_at, id
                    LIMIT ?)",
                at,
                t.artifact_id,
                &id,
                n
            )
            .execute(&mut *tx)
            .await?;
        }
        let live = sqlx::query_scalar!("SELECT COUNT(*) FROM trails WHERE retired_at IS NULL")
            .fetch_one(&mut *tx)
            .await?;
        let n = over(live, max);
        if n > 0 {
            sqlx::query!(
                "UPDATE trails SET retired_at = ? WHERE id IN (
                   SELECT id FROM trails
                    WHERE retired_at IS NULL AND id != ?
                    ORDER BY EXISTS (SELECT 1 FROM trail_sources s WHERE s.trail_id = trails.id
                                        AND s.source IN ('verdict', 'ask', 'decide')),
                             created_at, id
                    LIMIT ?)",
                at,
                &id,
                n
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(id)
    }

    /// Removes the source row from every trail that has it; a trail left with none is retired.
    pub async fn withdraw_trail_source(&self, source: TrailSource, source_id: &str) -> Result<u64> {
        let mut tx = self.pool.begin().await?;
        let source = source.as_str();
        let n = sqlx::query!(
            "DELETE FROM trail_sources WHERE source = ? AND source_id = ?",
            source,
            source_id
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();
        sqlx::query!(
            "UPDATE trails SET retired_at = ?
              WHERE retired_at IS NULL
                AND NOT EXISTS (SELECT 1 FROM trail_sources s WHERE s.trail_id = trails.id)",
            now()
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(n)
    }

    pub async fn live_trails(&self, embed_model: &str) -> Result<Vec<Trail>> {
        let rows = sqlx::query!(
            r#"SELECT t.id AS "id!", t.artifact_id AS "artifact_id!", t.query AS "query!",
                      t.query_vec AS "query_vec!", t.matched AS "matched!", t.missed AS "missed!",
                      EXISTS (SELECT 1 FROM trail_sources s WHERE s.trail_id = t.id
                                 AND s.source IN ('verdict', 'ask', 'decide')) AS "person!: i64"
                 FROM trails t
                WHERE t.retired_at IS NULL AND t.embed_model = ? AND t.vec_dim > 0
                ORDER BY t.created_at, t.id"#,
            embed_model
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| Trail {
                id: r.id,
                artifact_id: r.artifact_id,
                query: r.query,
                vec: blob_to_vec(&r.query_vec),
                tier: tier(r.person),
                matched: r.matched,
                missed: r.missed,
            })
            .collect())
    }

    /// Live trails under another embed model: `(id, query)`, for the re-embed pass.
    pub async fn stale_trails(
        &self,
        embed_model: &str,
        limit: i64,
    ) -> Result<Vec<(String, String)>> {
        Ok(sqlx::query!(
            r#"SELECT id AS "id!", query AS "query!" FROM trails
                WHERE retired_at IS NULL AND embed_model != ?
                ORDER BY created_at, id LIMIT ?"#,
            embed_model,
            limit
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|r| (r.id, r.query))
        .collect())
    }

    pub async fn reembed_trail(&self, id: &str, vec: &[f32], embed_model: &str) -> Result<()> {
        sqlx::query!(
            "UPDATE trails SET query_vec = ?, vec_dim = ?, embed_model = ? WHERE id = ?",
            vec_to_blob(vec),
            vec.len() as i64,
            embed_model,
            id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn trail_confirmed(&self, id: &str) -> Result<()> {
        sqlx::query!(
            "UPDATE trails SET matched = matched + 1, used_at = ? WHERE id = ?",
            now(),
            id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// missed += 1; retires at `missed >= 3 && missed > matched`. True when it retired.
    pub async fn trail_missed(&self, id: &str) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        sqlx::query!("UPDATE trails SET missed = missed + 1 WHERE id = ?", id)
            .execute(&mut *tx)
            .await?;
        let r = sqlx::query!(
            r#"SELECT matched AS "matched!", missed AS "missed!", retired_at FROM trails WHERE id = ?"#,
            id
        )
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| Error::Store(format!("trails: no trail {id}")))?;
        let retire = r.retired_at.is_none() && r.missed >= 3 && r.missed > r.matched;
        if retire {
            sqlx::query!("UPDATE trails SET retired_at = ? WHERE id = ?", now(), id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(retire)
    }

    /// Model trails whose `used_at` is older than `before`. Deleted, not retired: typed words age.
    pub async fn expire_model_trails(&self, before: i64) -> Result<u64> {
        Ok(sqlx::query!(
            "DELETE FROM trails
              WHERE used_at < ?
                AND NOT EXISTS (SELECT 1 FROM trail_sources s WHERE s.trail_id = trails.id
                                   AND s.source IN ('verdict', 'ask', 'decide'))",
            before
        )
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    pub async fn forget_trails(&self) -> Result<u64> {
        Ok(sqlx::query!("DELETE FROM trails")
            .execute(&self.pool)
            .await?
            .rows_affected())
    }

    /// The words, vector and embed model of the event a trail of this source is made from. `Deep`
    /// has no event row of its own: its caller passes the pairing to `Core::mint_trail`.
    pub async fn trail_pairing(
        &self,
        source: TrailSource,
        event_id: &str,
    ) -> Result<Option<Pairing>> {
        Ok(match source {
            TrailSource::Verdict | TrailSource::Label => {
                sqlx::query_as!(
                    Pairing,
                    r#"SELECT query AS "query!", query_vec AS "vec!", embed_model AS "embed_model!"
                         FROM search_events WHERE id = ?"#,
                    event_id
                )
                .fetch_optional(&self.pool)
                .await?
            }
            TrailSource::Ask => sqlx::query_as!(
                Pairing,
                r#"SELECT question AS "query!", query_vec AS "vec!", embed_model AS "embed_model!"
                         FROM ask_events WHERE id = ?"#,
                event_id
            )
            .fetch_optional(&self.pool)
            .await?,
            TrailSource::Decide => sqlx::query_as!(
                Pairing,
                r#"SELECT question AS "query!", query_vec AS "vec!", embed_model AS "embed_model!"
                         FROM decide_events WHERE id = ?"#,
                event_id
            )
            .fetch_optional(&self.pool)
            .await?,
            TrailSource::Deep => None,
        })
    }

    /// The trails a search's candidates carry, each pairing once: `(artifact_id, trail_id, shown,
    /// tier)`. A trail deleted since is left out.
    pub async fn event_trails(&self, event_id: &str) -> Result<Vec<(String, String, bool, Tier)>> {
        let rows = sqlx::query!(
            r#"SELECT c.artifact_id AS "artifact_id!", t.id AS "trail_id!",
                      MAX(c.shown) AS "shown!: i64",
                      EXISTS (SELECT 1 FROM trail_sources s WHERE s.trail_id = t.id
                                 AND s.source IN ('verdict', 'ask', 'decide')) AS "person!: i64"
                 FROM search_candidates c JOIN trails t ON t.id = c.trail_id
                WHERE c.event_id = ?
                GROUP BY c.artifact_id, t.id"#,
            event_id
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.artifact_id, r.trail_id, r.shown != 0, tier(r.person)))
            .collect())
    }

    pub async fn trail_counts(&self) -> Result<TrailCounts> {
        let r = sqlx::query!(
            r#"SELECT COUNT(*) AS "live!",
                      COUNT(DISTINCT artifact_id) AS "notes!",
                      COALESCE(SUM(EXISTS (SELECT 1 FROM trail_sources s WHERE s.trail_id = trails.id
                                              AND s.source IN ('verdict', 'ask', 'decide'))), 0) AS "by_person!: i64"
                 FROM trails WHERE retired_at IS NULL"#
        )
        .fetch_one(&self.pool)
        .await?;
        let found_through = sqlx::query_scalar!(
            "SELECT COUNT(DISTINCT event_id) FROM search_candidates
              WHERE trail_id IS NOT NULL AND shown = 1"
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(TrailCounts {
            live: r.live,
            notes: r.notes,
            by_person: r.by_person,
            found_through,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::TrailSource::*;
    use super::*;

    const M: &str = "fake";

    async fn note(store: &Store) -> String {
        let src = store.insert_corpus("raw", "web", None).await.unwrap();
        let new = vec![crate::store::artifacts::NewArtifact {
            text: "the image will not mount".into(),
            ..Default::default()
        }];
        store.insert_artifacts(&src.id, &new).await.unwrap()[0]
            .id
            .clone()
    }

    fn v(x: f32, y: f32) -> Vec<f32> {
        vec![x, y, 0.0]
    }

    fn new<'a>(
        n: &'a str,
        q: &'a str,
        vec: &'a [f32],
        source: TrailSource,
        id: &'a str,
    ) -> NewTrail<'a> {
        NewTrail {
            artifact_id: n,
            query: q,
            query_vec: vec,
            embed_model: M,
            source,
            source_id: id,
        }
    }

    #[tokio::test]
    async fn a_near_pairing_joins_the_trail_it_resembles() {
        let store = Store::memory().await.unwrap();
        let n = note(&store).await;
        let a = store
            .mint_trail(&new(&n, "q one", &v(1.0, 0.0), Label, "e1"), 8, 20000)
            .await
            .unwrap();
        let b = store
            .mint_trail(&new(&n, "q one!", &v(0.99, 0.05), Verdict, "e2"), 8, 20000)
            .await
            .unwrap();
        assert_eq!(a, b);
        assert_eq!(store.live_trails(M).await.unwrap()[0].tier, Tier::Person);
    }

    #[tokio::test]
    async fn a_trail_survives_one_undo_and_is_retired_by_both() {
        let store = Store::memory().await.unwrap();
        let n = note(&store).await;
        store
            .mint_trail(&new(&n, "q one", &v(1.0, 0.0), Label, "e1"), 8, 20000)
            .await
            .unwrap();
        store
            .mint_trail(&new(&n, "q one!", &v(0.99, 0.05), Verdict, "e2"), 8, 20000)
            .await
            .unwrap();
        store.withdraw_trail_source(Verdict, "e2").await.unwrap();
        assert_eq!(store.live_trails(M).await.unwrap()[0].tier, Tier::Model);
        store.withdraw_trail_source(Label, "e1").await.unwrap();
        assert!(store.live_trails(M).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_ninth_trail_on_a_note_puts_out_a_model_trail_first() {
        let store = Store::memory().await.unwrap();
        let n = note(&store).await;
        // Each 0.5 rad apart: far enough not to join one another.
        let vs: Vec<Vec<f32>> = (0..9)
            .map(|i| v((i as f32 * 0.5).cos(), (i as f32 * 0.5).sin()))
            .collect();
        let person = store
            .mint_trail(&new(&n, "q0", &vs[0], Verdict, "e0"), 8, 20000)
            .await
            .unwrap();
        for (i, vec) in vs.iter().enumerate().skip(1) {
            let (q, e) = (format!("q{i}"), format!("e{i}"));
            store
                .mint_trail(&new(&n, &q, vec, Label, &e), 8, 20000)
                .await
                .unwrap();
        }
        let live = store.live_trails(M).await.unwrap();
        assert_eq!(live.len(), 8);
        assert!(live.iter().any(|t| t.id == person));
    }

    #[tokio::test]
    async fn three_misses_over_no_match_retire_a_trail() {
        let store = Store::memory().await.unwrap();
        let n = note(&store).await;
        let id = store
            .mint_trail(&new(&n, "q", &v(1.0, 0.0), Label, "e1"), 8, 20000)
            .await
            .unwrap();
        assert!(!store.trail_missed(&id).await.unwrap());
        assert!(!store.trail_missed(&id).await.unwrap());
        assert!(store.trail_missed(&id).await.unwrap());
        assert!(store.live_trails(M).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_model_trail_ages_and_a_person_trail_does_not() {
        let store = Store::memory().await.unwrap();
        let n = note(&store).await;
        store
            .mint_trail(&new(&n, "model", &v(1.0, 0.0), Label, "e1"), 8, 20000)
            .await
            .unwrap();
        store
            .mint_trail(&new(&n, "person", &v(0.0, 1.0), Ask, "e2"), 8, 20000)
            .await
            .unwrap();
        assert_eq!(store.expire_model_trails(now() + 1).await.unwrap(), 1);
        let live = store.live_trails(M).await.unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].query, "person");
    }

    #[tokio::test]
    async fn a_trail_under_another_embed_model_is_not_live() {
        let store = Store::memory().await.unwrap();
        let n = note(&store).await;
        store
            .mint_trail(&new(&n, "q", &v(1.0, 0.0), Label, "e1"), 8, 20000)
            .await
            .unwrap();
        assert!(store.live_trails("other").await.unwrap().is_empty());
        assert_eq!(store.stale_trails("other", 10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn deleting_the_note_deletes_its_trails() {
        let store = Store::memory().await.unwrap();
        let n = note(&store).await;
        store
            .mint_trail(&new(&n, "q", &v(1.0, 0.0), Label, "e1"), 8, 20000)
            .await
            .unwrap();
        sqlx::query!("DELETE FROM artifacts WHERE id = ?", &n)
            .execute(&store.pool)
            .await
            .unwrap();
        assert_eq!(store.trail_counts().await.unwrap().live, 0);
        let left = sqlx::query_scalar!("SELECT COUNT(*) FROM trail_sources")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(left, 0);
    }
}
