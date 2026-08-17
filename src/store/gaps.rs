//! The holes, as rows: unanswered questions and gap searches, and the groups
//! the sweep made of them.

use super::{Store, now};
use crate::error::{Error, Result};
use crate::store::feedback::blob_to_vec;
use sqlx::Row;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapKind {
    Ask,
    Search,
}

impl GapKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            GapKind::Ask => "ask",
            GapKind::Search => "search",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ask" => Some(GapKind::Ask),
            "search" => Some(GapKind::Search),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Gap {
    pub kind: GapKind,
    pub id: String,
    pub text: String,
    pub vec: Vec<f32>,
}

#[derive(Debug, Clone)]
pub struct GapCluster {
    pub key: String,
    pub label: String,
    /// `model` or `terms`.
    pub labelled_by: String,
    pub members: Vec<(GapKind, String)>,
}

/// A cluster as the capture page shows it.
#[derive(Debug, Clone)]
pub struct GapRow {
    pub label: String,
    pub labelled_by: String,
    pub members: Vec<Gap>,
}

/// How many open gaps one pass reads, per kind.
///
/// A cap rather than the whole table, because both readers scale badly in this
/// number and neither says so: `jobs::gaps::sweep` compares every pair of them
/// on every retention tick, and `ui::capture_page` — the page the app opens on —
/// walks the same list with its full query vectors on every load. `cluster`'s
/// "N is tens, so the quadratic pass is fine" was an assumption about an
/// operator's habits, not a property of the query; a few thousand searches
/// judged `gap` made both costs real.
///
/// Newest first, so what is dropped is the oldest — a gap judged this week is
/// the one someone is still trying to fill. `judged_at` is whole seconds, which
/// on its own leaves everything judged inside one second in whatever order the
/// table hands back; the id breaks the tie, and being uuid v7 it breaks it by
/// creation, so the cap never cuts across a single second arbitrarily. `open_gaps` logs when the cap bites,
/// because a grouping that quietly left half the gaps out would read on the page
/// exactly like a grouping of all of them.
pub const MAX_OPEN_GAPS: i64 = 500;

impl Store {
    /// Every open gap with a vector under `embed_model`, newest first, up to
    /// `MAX_OPEN_GAPS` of each kind. A vector under another model is not
    /// comparable and is left out; an empty one (the cache had evicted it)
    /// likewise.
    pub async fn open_gaps(&self, embed_model: &str) -> Result<Vec<Gap>> {
        let mut out = Vec::new();
        for r in sqlx::query(
            "SELECT id, question AS text, query_vec FROM ask_events
             WHERE verdict = 'nothing_here' AND dismissed_at IS NULL
               AND embed_model = ? AND vec_dim > 0
             ORDER BY judged_at DESC, id DESC LIMIT ?",
        )
        .bind(embed_model)
        .bind(MAX_OPEN_GAPS)
        .fetch_all(&self.pool)
        .await?
        {
            out.push(Gap {
                kind: GapKind::Ask,
                id: r.get("id"),
                text: r.get("text"),
                vec: blob_to_vec(&r.get::<Vec<u8>, _>("query_vec")),
            });
        }
        let asks = out.len();
        for r in sqlx::query(
            "SELECT id, query AS text, query_vec FROM search_events
             WHERE verdict = 'gap' AND dismissed_at IS NULL AND embed_model = ?
               AND vec_dim > 0
             ORDER BY judged_at DESC, id DESC LIMIT ?",
        )
        .bind(embed_model)
        .bind(MAX_OPEN_GAPS)
        .fetch_all(&self.pool)
        .await?
        {
            out.push(Gap {
                kind: GapKind::Search,
                id: r.get("id"),
                text: r.get("text"),
                vec: blob_to_vec(&r.get::<Vec<u8>, _>("query_vec")),
            });
        }
        // Counted per kind, because each was capped on its own.
        let searches = out.len() - asks;
        if asks as i64 == MAX_OPEN_GAPS || searches as i64 == MAX_OPEN_GAPS {
            tracing::info!(
                cap = MAX_OPEN_GAPS,
                asks,
                searches,
                "more open gaps than one pass reads; the oldest are left out of this one"
            );
        }
        Ok(out)
    }

    pub async fn dismiss_gap(&self, kind: GapKind, id: &str) -> Result<()> {
        // Two literal statements rather than one built from the kind: nothing
        // from a request reaches the statement text.
        let res = match kind {
            GapKind::Ask => {
                sqlx::query("UPDATE ask_events SET dismissed_at = ? WHERE id = ?")
                    .bind(now())
                    .bind(id)
                    .execute(&self.pool)
                    .await?
            }
            GapKind::Search => {
                sqlx::query("UPDATE search_events SET dismissed_at = ? WHERE id = ?")
                    .bind(now())
                    .bind(id)
                    .execute(&self.pool)
                    .await?
            }
        };
        if res.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    pub async fn cluster_keys(&self) -> Result<Vec<(String, String)>> {
        Ok(sqlx::query("SELECT key, labelled_by FROM gap_clusters")
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(|r| (r.get("key"), r.get("labelled_by")))
            .collect())
    }

    pub async fn delete_clusters(&self, keys: &[String]) -> Result<()> {
        for k in keys {
            sqlx::query("DELETE FROM gap_clusters WHERE key = ?")
                .bind(k)
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    pub async fn put_cluster(&self, c: &GapCluster) -> Result<()> {
        let members = serde_json::to_string(
            &c.members
                .iter()
                .map(|(k, id)| serde_json::json!({"kind": k.as_str(), "id": id}))
                .collect::<Vec<_>>(),
        )
        .map_err(|e| Error::Internal(e.to_string()))?;
        sqlx::query(
            "INSERT INTO gap_clusters (key, label, labelled_by, members, created_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(key) DO UPDATE SET label = excluded.label, labelled_by = excluded.labelled_by",
        )
        .bind(&c.key)
        .bind(&c.label)
        .bind(&c.labelled_by)
        .bind(members)
        .bind(now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The clusters with their open members resolved, and the open gaps no
    /// cluster names yet (judged since the last sweep). A member that has been
    /// dismissed since the sweep is simply absent from its row; a row left with
    /// no members is not returned.
    pub async fn gap_rows(&self, embed_model: &str) -> Result<(Vec<GapRow>, Vec<Gap>)> {
        let open = self.open_gaps(embed_model).await?;
        let mut unclustered: Vec<Gap> = open.clone();
        let mut rows = Vec::new();
        for r in sqlx::query(
            "SELECT label, labelled_by, members FROM gap_clusters ORDER BY created_at DESC",
        )
        .fetch_all(&self.pool)
        .await?
        {
            let members: Vec<serde_json::Value> =
                serde_json::from_str(&r.get::<String, _>("members"))
                    .map_err(|e| Error::Internal(e.to_string()))?;
            let mut resolved = Vec::new();
            for m in members {
                let kind = m["kind"].as_str().and_then(GapKind::parse);
                let id = m["id"].as_str();
                if let (Some(kind), Some(id)) = (kind, id)
                    && let Some(g) = open.iter().find(|g| g.kind == kind && g.id == id)
                {
                    resolved.push(g.clone());
                    unclustered.retain(|u| !(u.kind == kind && u.id == id));
                }
            }
            if !resolved.is_empty() {
                rows.push(GapRow {
                    label: r.get("label"),
                    labelled_by: r.get("labelled_by"),
                    members: resolved,
                });
            }
        }
        Ok((rows, unclustered))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::asks::{AskVerdict, NewAsk};
    use crate::store::feedback::{Door, NewEvent, Verdict};

    async fn nothing_here(store: &Store, q: &str, vec: Vec<f32>) -> String {
        let id = store
            .record_ask(NewAsk {
                question: q.into(),
                scope: None,
                filters: "{}".into(),
                query_vec: vec,
                embed_model: "fake".into(),
                answer: "Not in the knowledge base.".into(),
                abstained: true,
                dropped: 0,
                truncated: false,
                citations: vec![],
            })
            .await
            .unwrap();
        store.judge_ask(&id, AskVerdict::NothingHere).await.unwrap();
        id
    }

    async fn gap_search(store: &Store, q: &str, vec: Vec<f32>) -> String {
        let id = store
            .record_search(
                NewEvent {
                    query: q.into(),
                    door: Door::Api,
                    scope: None,
                    filters: "{}".into(),
                    query_vec: vec,
                    embed_model: "fake".into(),
                    candidates: vec![],
                },
                0,
            )
            .await
            .unwrap();
        store.judge(&id, Verdict::Gap).await.unwrap();
        id
    }

    #[tokio::test]
    async fn open_gaps_are_the_unanswered_questions_and_the_gap_searches_under_this_model() {
        let store = Store::memory().await.unwrap();
        nothing_here(&store, "q1", vec![1.0, 0.0]).await;
        gap_search(&store, "s1", vec![0.0, 1.0]).await;
        // Not gaps: a right answer, an unjudged search, an empty vector.
        let right = store
            .record_ask(NewAsk {
                question: "ok".into(),
                scope: None,
                filters: "{}".into(),
                query_vec: vec![1.0, 1.0],
                embed_model: "fake".into(),
                answer: "yes".into(),
                abstained: false,
                dropped: 0,
                truncated: false,
                citations: vec![],
            })
            .await
            .unwrap();
        store.judge_ask(&right, AskVerdict::Right).await.unwrap();
        nothing_here(&store, "no vector", vec![]).await;
        let gaps = store.open_gaps("fake").await.unwrap();
        assert_eq!(
            gaps.iter().map(|g| g.text.as_str()).collect::<Vec<_>>(),
            vec!["q1", "s1"]
        );
        assert!(store.open_gaps("other-model").await.unwrap().is_empty());
    }

    /// The sweep compares every pair of open gaps and the capture page walks
    /// the same list on every load, so the number of them has to be bounded
    /// somewhere. Newest first, so what a cap drops is the oldest.
    #[tokio::test]
    async fn one_pass_reads_at_most_the_newest_cap_worth_of_gaps() {
        let store = Store::memory().await.unwrap();
        for i in 0..MAX_OPEN_GAPS + 1 {
            nothing_here(&store, &format!("q{i}"), vec![1.0, 0.0]).await;
        }
        let gaps = store.open_gaps("fake").await.unwrap();
        assert_eq!(gaps.len() as i64, MAX_OPEN_GAPS);
        assert_eq!(
            gaps[0].text,
            format!("q{MAX_OPEN_GAPS}"),
            "the newest gap is the one a bounded pass must not drop"
        );
    }

    #[tokio::test]
    async fn a_dismissed_gap_is_no_longer_open() {
        let store = Store::memory().await.unwrap();
        let a = nothing_here(&store, "q1", vec![1.0]).await;
        let s = gap_search(&store, "s1", vec![1.0]).await;
        store.dismiss_gap(GapKind::Ask, &a).await.unwrap();
        assert_eq!(store.open_gaps("fake").await.unwrap().len(), 1);
        store.dismiss_gap(GapKind::Search, &s).await.unwrap();
        assert!(store.open_gaps("fake").await.unwrap().is_empty());
        assert!(matches!(
            store.dismiss_gap(GapKind::Ask, "nope").await,
            Err(Error::NotFound)
        ));
    }

    #[tokio::test]
    async fn rows_resolve_members_and_report_what_no_cluster_names_yet() {
        let store = Store::memory().await.unwrap();
        let a = nothing_here(&store, "q1", vec![1.0]).await;
        let b = nothing_here(&store, "q2", vec![1.0]).await;
        let later = nothing_here(&store, "q3", vec![1.0]).await;
        store
            .put_cluster(&GapCluster {
                key: "k".into(),
                label: "Mounting".into(),
                labelled_by: "model".into(),
                members: vec![(GapKind::Ask, a.clone()), (GapKind::Ask, b.clone())],
            })
            .await
            .unwrap();
        let (rows, loose) = store.gap_rows("fake").await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "Mounting");
        assert_eq!(rows[0].members.len(), 2);
        assert_eq!(
            loose.iter().map(|g| g.id.as_str()).collect::<Vec<_>>(),
            vec![later.as_str()]
        );

        // Dismissing a member thins the row; dismissing both removes it.
        store.dismiss_gap(GapKind::Ask, &a).await.unwrap();
        assert_eq!(store.gap_rows("fake").await.unwrap().0[0].members.len(), 1);
        store.dismiss_gap(GapKind::Ask, &b).await.unwrap();
        assert!(store.gap_rows("fake").await.unwrap().0.is_empty());
    }

    #[tokio::test]
    async fn clusters_can_be_listed_replaced_and_deleted() {
        let store = Store::memory().await.unwrap();
        let c = GapCluster {
            key: "k".into(),
            label: "x".into(),
            labelled_by: "terms".into(),
            members: vec![],
        };
        store.put_cluster(&c).await.unwrap();
        store
            .put_cluster(&GapCluster {
                label: "y".into(),
                labelled_by: "model".into(),
                ..c.clone()
            })
            .await
            .unwrap();
        assert_eq!(
            store.cluster_keys().await.unwrap(),
            vec![("k".to_string(), "model".to_string())]
        );
        store.delete_clusters(&["k".into()]).await.unwrap();
        assert!(store.cluster_keys().await.unwrap().is_empty());
    }
}
