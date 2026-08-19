//! Group the open gaps and name the new groups.
//!
//! Runs on the retention ticker. Clustering is free; naming costs one
//! efficient-tier call per group of two or more that did not exist before —
//! membership is identity, so the same members are never named twice — and a
//! group named by terms because the model was unavailable is offered to the
//! model again next pass.

use crate::core::Core;
use crate::core::gaps::{
    GAP_LINK_AT, MIN_CLUSTER, cluster, cluster_key, link_threshold, terms_label,
};
use crate::error::Result;
use crate::store::gaps::{GapCluster, GapKind};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Groups of `MIN_CLUSTER` or more: the ones that are stored and shown as
    /// groups. A gap on its own is not one, and is left to the capture page's
    /// ungrouped list.
    pub clusters: usize,
    pub named: usize,
    pub removed: usize,
}

pub async fn sweep(core: &Core) -> Result<SweepReport> {
    let open = core.store.open_gaps(core.embedder.model()).await?;
    // Measured from the base's own recorded queries rather than taken from the
    // constant, and only when there is something to group: `link_threshold`
    // reads a sample of every stored query vector, which is work worth nothing
    // when there is at most one gap to place.
    let link_at = if open.gaps.len() < MIN_CLUSTER {
        GAP_LINK_AT
    } else {
        link_threshold(&core.store.calibration_vecs(core.embedder.model()).await?)
    };
    let vecs: Vec<&[f32]> = open.gaps.iter().map(|g| g.vec.as_slice()).collect();
    let groups = cluster(&vecs, link_at);

    let existing = core.store.cluster_keys().await?;
    let mut report = SweepReport::default();
    let mut live_keys = Vec::with_capacity(groups.len());

    for group in &groups {
        // A group of one is a question, and asking the model to name it buys a
        // restatement of it. It is not stored either: the capture page shows an
        // ungrouped gap under its own words already, which is the truer thing to
        // show — and storing it would mean every gap that later joins it re-keys
        // a group that was never worth a name.
        if group.len() < MIN_CLUSTER {
            continue;
        }
        report.clusters += 1;
        let members: Vec<_> = group
            .iter()
            .map(|&i| (open.gaps[i].gap.kind, open.gaps[i].gap.id.clone()))
            .collect();
        let key = cluster_key(&members);
        live_keys.push(key.clone());
        let known = existing
            .iter()
            .find(|c| c.key == key)
            .map(|c| c.labelled_by.as_str());
        if known == Some("model") {
            continue;
        }
        // Newest first, the order `open_gaps` returned, which is the order
        // `gap_label_prompt` keeps when it caps what one call is shown.
        let texts: Vec<&str> = group
            .iter()
            .map(|&i| open.gaps[i].gap.text.as_str())
            .collect();
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

    // A key this pass did not see is stale — unless this pass did not see
    // everything. Past the cap, the clusters covering the gaps that were left
    // out look exactly like clusters whose members were dismissed, and deleting
    // them took those gaps off the capture page altogether: not ungrouped,
    // gone. The groups keep their last label until a pass reads them again.
    //
    // What cannot wait for that pass is a key this one *replaced*: when a
    // member is dismissed, the survivors re-key, and `put_cluster` has already
    // stored the new key by the time we get here. Leaving the old row too puts
    // those survivors under both headings at once on the capture page, because
    // `gap_rows` resolves every row it finds. So an unseen key that shares a
    // member with a group this pass did store is superseded, not unread, and
    // goes whether or not the pass was partial. A key with no overlap at all is
    // either past the cap or wholly dismissed; only the second is stale, the two
    // are indistinguishable from here, and a wholly dismissed row renders
    // nothing in the meantime — `gap_rows` drops a row with no open members.
    let clustered_now: std::collections::HashSet<(GapKind, &str)> = groups
        .iter()
        .filter(|g| g.len() >= MIN_CLUSTER)
        .flat_map(|g| g.iter())
        .map(|&i| (open.gaps[i].gap.kind, open.gaps[i].gap.id.as_str()))
        .collect();
    let stale: Vec<String> = existing
        .into_iter()
        .filter(|c| !live_keys.contains(&c.key))
        .filter(|c| {
            !open.capped
                || c.members
                    .iter()
                    .any(|(k, id)| clustered_now.contains(&(*k, id.as_str())))
        })
        .map(|c| c.key)
        .collect();
    if open.capped {
        tracing::info!(
            removing = stale.len(),
            "open gaps past the cap; keeping every group this pass could not \
             reach rather than reading a partial pass as a set of dismissals"
        );
    }
    report.removed = stale.len();
    core.store.delete_clusters(&stale).await?;
    Ok(report)
}

/// One bounded call under the background lane. Any failure — endpoint down,
/// unreadable reply — is `None`, and the caller falls back to terms.
async fn name(core: &Core, questions: &[&str]) -> Option<String> {
    // No model, no name: the caller falls back to the cluster's terms.
    let namer = core.gap_namer.clone()?;
    let permit = core.gate.background().await;
    let reply = namer
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
        nothing_here(&core, "E01 read only mount", vec![0.9, 0.1]).await;
        nothing_here(&core, "FAT entries", vec![0.0, 1.0]).await;

        let r = sweep(&core).await.unwrap();
        assert_eq!(
            r,
            SweepReport {
                clusters: 1,
                named: 1,
                removed: 0
            },
            "the lone 'FAT entries' gap is not a group and costs no call"
        );
        let (rows, loose) = core.store.gap_rows(core.embedder.model()).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].members.len(), 3);
        assert!(rows[0].label == "Fake topic" && rows[0].labelled_by == "model");
        assert_eq!(
            loose.iter().map(|g| g.text.as_str()).collect::<Vec<_>>(),
            vec!["FAT entries"],
            "an ungrouped gap is shown under its own words, not under a name \
             bought for it"
        );

        // Same members, no new call.
        assert_eq!(sweep(&core).await.unwrap().named, 0);

        // Dismissing one member changes the cluster: the old key goes, the
        // new one is named.
        core.store.dismiss_gap(GapKind::Ask, &a).await.unwrap();
        let r = sweep(&core).await.unwrap();
        assert_eq!((r.clusters, r.named, r.removed), (1, 1, 1));
    }

    #[tokio::test]
    async fn a_gap_on_its_own_is_never_named() {
        let core = test_core().await;
        nothing_here(&core, "mount an E01", vec![1.0, 0.0]).await;
        nothing_here(&core, "FAT entries", vec![0.0, 1.0]).await;
        assert_eq!(sweep(&core).await.unwrap(), SweepReport::default());
        let (rows, loose) = core.store.gap_rows(core.embedder.model()).await.unwrap();
        assert!(rows.is_empty());
        assert_eq!(loose.len(), 2);
    }

    #[tokio::test]
    async fn without_a_readable_model_a_cluster_is_named_by_its_terms_and_retried_later() {
        let mut core = test_core().await;
        core.gap_namer = Some(std::sync::Arc::new(crate::infer::fake::FakeCompleter {
            reply: Some("not json".into()),
        }));
        nothing_here(&core, "mount an E01 image", vec![1.0]).await;
        nothing_here(&core, "E01 mount read only", vec![1.0]).await;
        let r = sweep(&core).await.unwrap();
        assert_eq!((r.clusters, r.named), (1, 0));
        let (rows, _) = core.store.gap_rows(core.embedder.model()).await.unwrap();
        assert_eq!(rows[0].labelled_by, "terms");
        assert!(rows[0].label.contains("e01"), "{}", rows[0].label);

        core.gap_namer = Some(std::sync::Arc::new(crate::infer::fake::FakeCompleter {
            reply: Some(r#"{"label":"Image mounting"}"#.into()),
        }));
        assert_eq!(sweep(&core).await.unwrap().named, 1);
        assert_eq!(
            core.store.gap_rows(core.embedder.model()).await.unwrap().0[0].label,
            "Image mounting"
        );
    }

    #[tokio::test]
    async fn a_partial_pass_is_not_read_as_a_set_of_dismissals() {
        // Past the cap, the clusters covering the gaps that were left out look
        // exactly like clusters whose members were all dismissed. Deleting them
        // took those gaps off the capture page altogether — not ungrouped, gone
        // — and the next pass would drop the same oldest gaps again, so they
        // never came back.
        let core = test_core().await;
        for i in 0..crate::store::gaps::MAX_OPEN_GAPS + 1 {
            nothing_here(&core, &format!("gap {i}"), vec![1.0, 0.0]).await;
        }
        sweep(&core).await.unwrap();

        // A group over gaps the cap left out: its key cannot appear in any pass
        // that does not read them, which is every pass while the cap bites.
        core.store
            .put_cluster(&GapCluster {
                key: "beyond-the-cap".into(),
                label: "Older holes".into(),
                labelled_by: "model".into(),
                members: vec![(GapKind::Ask, "an-older-gap".into())],
            })
            .await
            .unwrap();

        let after = sweep(&core).await.unwrap();
        assert_eq!(after.removed, 0, "nothing was dismissed");
        assert!(
            core.store
                .cluster_keys()
                .await
                .unwrap()
                .iter()
                .any(|c| c.key == "beyond-the-cap"),
            "the group the partial pass could not see was deleted as stale"
        );
    }

    #[tokio::test]
    async fn a_partial_pass_still_removes_the_key_it_replaced() {
        // The other half of the rule above. Keeping every unseen key while the
        // cap bites also kept the key a dismissal had just re-keyed, and
        // `gap_rows` resolves every row it finds — so the members that survived
        // the dismissal were rendered under the old heading and the new one at
        // once.
        let core = test_core().await;
        let mut ids = Vec::new();
        for i in 0..crate::store::gaps::MAX_OPEN_GAPS + 2 {
            ids.push(nothing_here(&core, &format!("gap {i}"), vec![1.0, 0.0]).await);
        }
        sweep(&core).await.unwrap();
        let before = core.store.cluster_keys().await.unwrap();
        assert_eq!(before.len(), 1, "one group over everything the pass read");

        core.store
            .dismiss_gap(GapKind::Ask, ids.last().unwrap())
            .await
            .unwrap();
        let after = sweep(&core).await.unwrap();
        assert!(
            core.store
                .open_gaps(core.embedder.model())
                .await
                .unwrap()
                .capped,
            "the fixture must keep the cap biting, or this proves nothing"
        );
        assert_eq!(after.removed, 1, "the re-keyed group left one key behind");
        let keys = core.store.cluster_keys().await.unwrap();
        assert!(
            keys.iter().all(|c| c.key != before[0].key),
            "the key this pass replaced was kept alongside the one that replaced it"
        );

        let (rows, _) = core.store.gap_rows(core.embedder.model()).await.unwrap();
        let mut seen = std::collections::HashSet::new();
        for r in &rows {
            for m in &r.members {
                assert!(
                    seen.insert((m.kind, m.id.clone())),
                    "a gap rendered under two headings at once"
                );
            }
        }
    }

    #[tokio::test]
    async fn no_gaps_means_no_clusters_and_no_calls() {
        let core = test_core().await;
        assert_eq!(sweep(&core).await.unwrap(), SweepReport::default());
    }
}
