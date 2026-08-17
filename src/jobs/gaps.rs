//! Group the open gaps and name the new groups.
//!
//! Runs on the retention ticker. Clustering is free; naming costs one
//! efficient-tier call per cluster that did not exist before — membership is
//! identity, so the same members are never named twice — and a cluster named
//! by terms because the model was unavailable is offered to the model again
//! next pass.

use crate::core::Core;
use crate::core::gaps::{GAP_LINK_AT, cluster, cluster_key, terms_label};
use crate::error::Result;
use crate::store::gaps::GapCluster;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub clusters: usize,
    pub named: usize,
    pub removed: usize,
}

pub async fn sweep(core: &Core) -> Result<SweepReport> {
    let gaps = core.store.open_gaps(core.embedder.model()).await?;
    let vecs: Vec<Vec<f32>> = gaps.iter().map(|g| g.vec.clone()).collect();
    let groups = cluster(&vecs, GAP_LINK_AT);

    let existing = core.store.cluster_keys().await?;
    let mut report = SweepReport {
        clusters: groups.len(),
        ..Default::default()
    };
    let mut live_keys = Vec::with_capacity(groups.len());

    for group in &groups {
        let members: Vec<_> = group
            .iter()
            .map(|&i| (gaps[i].kind, gaps[i].id.clone()))
            .collect();
        let key = cluster_key(&members);
        live_keys.push(key.clone());
        let known = existing
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, by)| by.as_str());
        if known == Some("model") {
            continue;
        }
        let texts: Vec<&str> = group.iter().map(|&i| gaps[i].text.as_str()).collect();
        let (label, labelled_by) = match name(core, &texts).await {
            Some(l) => (l, "model"),
            None => (terms_label(&texts), "terms"),
        };
        if labelled_by == "model" {
            report.named += 1;
        }
        core.store
            .put_cluster(&GapCluster {
                key,
                label,
                labelled_by: labelled_by.into(),
                members,
            })
            .await?;
    }

    let stale: Vec<String> = existing
        .into_iter()
        .map(|(k, _)| k)
        .filter(|k| !live_keys.contains(k))
        .collect();
    report.removed = stale.len();
    core.store.delete_clusters(&stale).await?;
    Ok(report)
}

/// One bounded call under the background lane. Any failure — endpoint down,
/// unreadable reply — is `None`, and the caller falls back to terms.
async fn name(core: &Core, questions: &[&str]) -> Option<String> {
    let permit = core.gate.background().await;
    let reply = core
        .gap_namer
        .complete(
            crate::infer::prompt::GAP_LABEL_SYSTEM,
            &crate::infer::prompt::gap_label_prompt(questions),
        )
        .await;
    permit.finished();
    match reply.and_then(|r| crate::infer::prompt::parse_gap_label(&r)) {
        Ok(label) => Some(label),
        Err(e) => {
            tracing::warn!(error = %e, "could not name a knowledge gap; using its terms");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_support::test_core;
    use crate::store::asks::{AskVerdict, NewAsk};
    use crate::store::gaps::GapKind;

    async fn nothing_here(core: &Core, q: &str, vec: Vec<f32>) -> String {
        let id = core
            .store
            .record_ask(NewAsk {
                question: q.into(),
                scope: None,
                filters: "{}".into(),
                query_vec: vec,
                embed_model: core.embedder.model().to_string(),
                answer: "Not in the knowledge base.".into(),
                abstained: true,
                dropped: 0,
                truncated: false,
                citations: vec![],
            })
            .await
            .unwrap();
        core.store
            .judge_ask(&id, AskVerdict::NothingHere)
            .await
            .unwrap();
        id
    }

    #[tokio::test]
    async fn a_new_cluster_is_named_once_and_a_vanished_one_is_removed() {
        let core = test_core().await;
        let a = nothing_here(&core, "mount an E01", vec![1.0, 0.0]).await;
        nothing_here(&core, "mounting E01 images", vec![0.95, 0.05]).await;
        nothing_here(&core, "FAT entries", vec![0.0, 1.0]).await;

        let r = sweep(&core).await.unwrap();
        assert_eq!(
            r,
            SweepReport {
                clusters: 2,
                named: 2,
                removed: 0
            }
        );
        let (rows, loose) = core.store.gap_rows(core.embedder.model()).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert!(loose.is_empty());
        assert!(
            rows.iter()
                .all(|r| r.label == "Fake topic" && r.labelled_by == "model")
        );

        // Same members, no new call.
        assert_eq!(sweep(&core).await.unwrap().named, 0);

        // Dismissing one member changes the cluster: the old key goes, the
        // new one is named.
        core.store.dismiss_gap(GapKind::Ask, &a).await.unwrap();
        let r = sweep(&core).await.unwrap();
        assert_eq!((r.clusters, r.named, r.removed), (2, 1, 1));
    }

    #[tokio::test]
    async fn without_a_readable_model_a_cluster_is_named_by_its_terms_and_retried_later() {
        let mut core = test_core().await;
        core.gap_namer = std::sync::Arc::new(crate::infer::fake::FakeCompleter {
            reply: Some("not json".into()),
        });
        nothing_here(&core, "mount an E01 image", vec![1.0]).await;
        nothing_here(&core, "E01 mount read only", vec![1.0]).await;
        let r = sweep(&core).await.unwrap();
        assert_eq!((r.clusters, r.named), (1, 0));
        let (rows, _) = core.store.gap_rows(core.embedder.model()).await.unwrap();
        assert_eq!(rows[0].labelled_by, "terms");
        assert!(rows[0].label.contains("e01"), "{}", rows[0].label);

        core.gap_namer = std::sync::Arc::new(crate::infer::fake::FakeCompleter {
            reply: Some(r#"{"label":"Image mounting"}"#.into()),
        });
        assert_eq!(sweep(&core).await.unwrap().named, 1);
        assert_eq!(
            core.store.gap_rows(core.embedder.model()).await.unwrap().0[0].label,
            "Image mounting"
        );
    }

    #[tokio::test]
    async fn no_gaps_means_no_clusters_and_no_calls() {
        let core = test_core().await;
        assert_eq!(sweep(&core).await.unwrap(), SweepReport::default());
    }
}
