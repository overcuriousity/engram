//! How an artifact came to exist, as a tree.
//!
//! A merged artifact's pane used to answer "where did this come from" with a
//! flat list of the captured artifacts underneath it. That is what it is *made
//! of*, and it is true, but it is not how it came to be: a merge written from
//! two earlier merges and one fresh capture read as three equal siblings, and
//! the generation between them — the thing that shows a base consolidating —
//! was on the page nowhere.
//!
//! The generation is stored. `artifact_sources` records the resolved closure,
//! child to captured root, and beside each row the `via_id` it entered
//! through; the schema marks that column "rendering only", and this is the
//! rendering it was left there for.
//!
//! What this cannot show is how an artifact's *wording* developed.
//! `update_artifact_text` overwrites in place and no table keeps revisions, so
//! an artifact rewritten by hand five times is indistinguishable here from one
//! nobody has touched. The tree is assembly, not authorship.

use crate::error::Result;
use crate::store::Store;
use crate::store::artifacts::Chunk;
use std::collections::{HashMap, HashSet};

/// How deep the tree may go, and how many nodes it may hold.
///
/// Merging is bounded in practice — each generation halves the number of
/// artifacts standing — but the walk reads rows rather than reasoning about
/// them, and a base with a pathological history should cost a truncated
/// picture rather than a page that never renders. Truncation is stated, never
/// silent: see `Lineage::truncated`.
const MAX_DEPTH: usize = 6;
const MAX_NODES: usize = 200;

/// One artifact in the tree, and what it was written from.
pub struct LineageNode {
    pub id: String,
    pub title: String,
    /// `merge` or `captured` — what kind of artifact this is, in one word.
    pub kind: &'static str,
    pub when: String,
    /// Not rendered. What the levels are ordered by: a tree read top to bottom
    /// is a history, and one ordered by id is a history in no order at all.
    pub created_at: i64,
    /// The document this was drawn from, deep-linked to its exact lines.
    /// Empty for a merge, which belongs to no document.
    pub source_href: String,
    pub source_label: String,
    /// This artifact was superseded by the one at the root of the tree — the
    /// ordinary outcome of a merge, and worth saying on the node rather than
    /// leaving the reader to infer it from the arrangement.
    pub replaced: bool,
    /// The row is gone: a source deleted since the merge was written. Named
    /// rather than dropped, because a tree that silently loses a branch claims
    /// less provenance than the artifact's text carries.
    pub missing: bool,
    pub children: Vec<LineageNode>,
}

/// The whole picture for one artifact.
///
/// `Default` is the empty one: a captured artifact that has replaced nothing,
/// and the fallback when the walk itself fails — a pane must render without
/// its lineage, the same way it renders without its neighbours.
#[derive(Default)]
pub struct Lineage {
    /// What it was written from, nested by generation. Empty for a captured
    /// artifact, which was written from a document rather than from artifacts.
    pub roots: Vec<LineageNode>,
    /// Artifacts this one replaced that are nowhere in the tree above.
    ///
    /// Kept apart on purpose: the dedupe sweep can supersede a near-duplicate
    /// without merging anything, and that is a different claim from "this was
    /// written from it". Rendering both under one heading would say the text
    /// of one is carried by the other, which in this case nothing checked.
    pub also_replaced: Vec<LineageNode>,
    /// The walk hit `MAX_DEPTH` or `MAX_NODES` and the tree is a prefix of the
    /// truth. The page says so; a tree that quietly stops is worse than no
    /// tree, because it reads as a complete history.
    pub truncated: bool,
}

impl Lineage {
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty() && self.also_replaced.is_empty()
    }

    /// The tree, depth first, for the template.
    pub fn flat_roots(&self) -> Vec<FlatNode<'_>> {
        let mut out = Vec::new();
        flatten(&self.roots, 0, &mut out);
        out
    }

    /// The near duplicates it replaced. Flat by nature — nothing was written
    /// from them, so they have no lineage to show under them here.
    pub fn flat_replaced(&self) -> Vec<FlatNode<'_>> {
        let mut out = Vec::new();
        flatten(&self.also_replaced, 0, &mut out);
        out
    }

    /// How many captured artifacts the tree ends in — what "written from four
    /// artifacts" counts. The generations between are not artifacts this was
    /// written from; they are the route it took.
    pub fn leaves(&self) -> usize {
        fn walk(ns: &[LineageNode]) -> usize {
            ns.iter()
                .map(|n| {
                    if n.children.is_empty() {
                        1
                    } else {
                        walk(&n.children)
                    }
                })
                .sum()
        }
        walk(&self.roots)
    }
}

/// One node as the template draws it: the tree flattened, depth first, with
/// the depth it sits at.
///
/// Askama resolves `include` at compile time, so a template cannot include
/// itself and a nested `<ul>` is not expressible. Flattening here rather than
/// contorting the model: the tree is the honest shape to build, test and reason
/// about, and indentation is a rendering concern.
pub struct FlatNode<'a> {
    pub depth: usize,
    /// Last child of its parent — which connector glyph to draw.
    pub last: bool,
    pub n: &'a LineageNode,
}

fn flatten<'a>(ns: &'a [LineageNode], depth: usize, out: &mut Vec<FlatNode<'a>>) {
    for (i, n) in ns.iter().enumerate() {
        out.push(FlatNode {
            depth,
            last: i + 1 == ns.len(),
            n,
        });
        flatten(&n.children, depth + 1, out);
    }
}

/// Everything one page of this needs from the store, read once.
///
/// Corpus titles are looked up per leaf and a wide merge has many leaves from
/// few documents; the cache is what keeps that one query per document rather
/// than one per artifact.
struct Walk<'a> {
    store: &'a Store,
    corpora: HashMap<String, String>,
    /// Ids this artifact superseded, for the `replaced` tag.
    replaced: HashSet<String>,
    /// Every id the tree already holds — cycle guard, and the answer to which
    /// superseded artifacts still need their own section.
    seen: HashSet<String>,
    nodes: usize,
    truncated: bool,
}

/// The lineage of `id`: what it was written from, and what it replaced.
pub async fn build(store: &Store, id: &str) -> Result<Lineage> {
    let mut w = Walk {
        store,
        corpora: HashMap::new(),
        replaced: store
            .artifacts_superseded_by(id)
            .await?
            .into_iter()
            .collect(),
        seen: HashSet::from([id.to_string()]),
        nodes: 0,
        truncated: false,
    };

    let roots = w.expand(id, 0).await?;

    // What it replaced without being written from it. Read after the walk, so
    // "already in the tree" means the whole tree and not the part built so far.
    let mut also_replaced = Vec::new();
    let outside: Vec<String> = w
        .replaced
        .iter()
        .filter(|r| !w.seen.contains(*r))
        .cloned()
        .collect();
    for r in sorted(outside) {
        also_replaced.push(w.node(&r).await);
    }

    Ok(Lineage {
        roots,
        also_replaced,
        truncated: w.truncated,
    })
}

/// Deterministic order for a set: two runs of the same page must not shuffle.
fn sorted(mut ids: Vec<String>) -> Vec<String> {
    ids.sort();
    ids
}

impl Walk<'_> {
    /// The children of `id`: the artifacts it was written from, each with its
    /// own children under it.
    async fn expand(&mut self, id: &str, depth: usize) -> Result<Vec<LineageNode>> {
        if depth >= MAX_DEPTH || self.nodes >= MAX_NODES {
            self.truncated = true;
            return Ok(Vec::new());
        }
        let rows = self.store.sources_with_via(id).await?;

        // Two kinds of row. `via == root` is a root merged in directly; anything
        // else names the intermediate it came through, and those group into one
        // node per intermediate — that node is the generation the flat list had
        // no way to show.
        let mut direct: Vec<String> = Vec::new();
        let mut through: HashMap<String, Vec<String>> = HashMap::new();
        for (root, via) in rows {
            match via {
                // `None` is an intermediate deleted since; the row still knows
                // its root, so the root is shown where it would have hung.
                Some(v) if v != root => through.entry(v).or_default().push(root),
                _ => direct.push(root),
            }
        }

        let mut out: Vec<LineageNode> = Vec::new();
        for root in sorted(direct) {
            if self.nodes >= MAX_NODES {
                self.truncated = true;
                break;
            }
            self.seen.insert(root.clone());
            out.push(self.node(&root).await);
        }
        for via in sorted(through.keys().cloned().collect()) {
            if self.nodes >= MAX_NODES {
                self.truncated = true;
                break;
            }
            // A cycle cannot happen — a merge is written before it supersedes
            // anything, so its sources are always older — but the guard costs a
            // hash lookup and the alternative is a page that never returns.
            if !self.seen.insert(via.clone()) {
                continue;
            }
            let mut n = self.node(&via).await;
            n.children = Box::pin(self.expand(&via, depth + 1)).await?;
            if n.children.is_empty() {
                // The intermediate's own lineage rows are gone, but this
                // artifact's rows still name what came through it. Better the
                // roots under an empty parent than a parent claiming nothing
                // came through it at all.
                for root in sorted(through[&via].clone()) {
                    self.seen.insert(root.clone());
                    n.children.push(self.node(&root).await);
                }
            }
            out.push(n);
        }
        // Oldest first, so the level reads as the order things happened. `sorted`
        // above only makes the walk deterministic; it says nothing about time,
        // and a merge listed before the capture it was written from reads as a
        // history running backwards.
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.title.cmp(&b.title)));
        Ok(out)
    }

    /// One node, read from the store. An id with no row is a source deleted
    /// since; it becomes a node that says so rather than disappearing.
    async fn node(&mut self, id: &str) -> LineageNode {
        self.nodes += 1;
        let replaced = self.replaced.contains(id);
        let Ok(c) = self.store.get_artifact(id).await else {
            return LineageNode {
                id: id.to_string(),
                title: "deleted since".into(),
                kind: "gone",
                when: String::new(),
                // Nothing is known about when it was written; it sorts to the
                // front, where a reader looking for what is missing will be.
                created_at: 0,
                source_href: String::new(),
                source_label: String::new(),
                replaced,
                missing: true,
                children: Vec::new(),
            };
        };
        let (source_href, source_label) = self.source_of(&c).await;
        LineageNode {
            title: crate::web::ui::title_of(&c),
            kind: match c.provenance {
                crate::store::artifacts::Provenance::Merged => "merge",
                crate::store::artifacts::Provenance::Synthesized => "synthesized",
                crate::store::artifacts::Provenance::Passage => "passage",
                crate::store::artifacts::Provenance::Captured => "captured",
                crate::store::artifacts::Provenance::Note => "note",
            },
            when: crate::web::ui::fmt_time(c.created_at),
            created_at: c.created_at,
            source_href,
            source_label,
            replaced,
            missing: false,
            id: c.id,
            children: Vec::new(),
        }
    }

    /// Where a captured artifact was drawn from, as a link and its label. The
    /// same deep link the pane's own Source label uses, so a leaf of the tree
    /// opens the document at the lines it was written from.
    async fn source_of(&mut self, c: &Chunk) -> (String, String) {
        let Some(cid) = c.corpus_id.clone() else {
            return (String::new(), String::new());
        };
        let title = match self.corpora.get(&cid) {
            Some(t) => t.clone(),
            None => {
                let t = match self.store.get_corpus(&cid).await {
                    Ok(s) => s
                        .title_hint
                        .or(s.source_url)
                        .unwrap_or_else(|| "untitled".into()),
                    // The document row is gone; the artifact still names it, and
                    // the link still resolves to a page that says so.
                    Err(_) => "a document no longer stored".to_string(),
                };
                self.corpora.insert(cid.clone(), t.clone());
                t
            }
        };
        match &c.corpus_span {
            Some(sp) => (
                format!(
                    "/ui/corpora/{cid}?from={}&to={}#L{}",
                    sp.start_line, sp.end_line, sp.start_line
                ),
                if sp.start_line == sp.end_line {
                    format!("line {} of {title}", sp.start_line)
                } else {
                    format!("lines {}–{} of {title}", sp.start_line, sp.end_line)
                },
            ),
            // No span: written before spans were recorded, or restored from the
            // vector store. The document is still the honest answer.
            None => (format!("/ui/corpora/{cid}"), format!("from {title}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::artifacts::{NewArtifact, NewMerged};

    async fn captured(s: &Store, n: usize) -> Vec<String> {
        let src = s
            .insert_corpus("one\ntwo\nthree", "web", None)
            .await
            .unwrap();
        let new: Vec<NewArtifact> = (0..n)
            .map(|i| NewArtifact {
                ordinal: i as i64,
                text: format!("artifact {i}"),
                corpus_span: Some(crate::store::artifacts::CorpusSpan {
                    start_line: 1,
                    end_line: 2,
                    source: crate::store::artifacts::SpanSource::Located,
                }),
                title: Some(format!("captured {i}")),
                category: None,
                tags: vec![],
                segment_idx: None,
                caveats: vec![],
            })
            .collect();
        s.insert_artifacts(&src.id, &new)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect()
    }

    fn merged(text: &str) -> NewMerged {
        NewMerged {
            text: text.into(),
            title: Some(text.into()),
            category: None,
            tags: vec![],
            caveats: vec![],
        }
    }

    /// The whole point: the generation between a merge and its captured roots
    /// is on the page. A flat list of roots cannot say it.
    #[tokio::test]
    async fn a_merge_of_a_merge_is_two_generations_deep() {
        let s = Store::memory().await.unwrap();
        let ids = captured(&s, 3).await;
        let m1 = s
            .insert_merged_artifact(&merged("first pass"), &ids[0..2])
            .await
            .unwrap();
        let m2 = s
            .insert_merged_artifact(&merged("second pass"), &[m1.id.clone(), ids[2].clone()])
            .await
            .unwrap();

        let l = build(&s, &m2.id).await.unwrap();

        assert_eq!(l.roots.len(), 2, "one capture and one earlier merge");
        let merge_node = l
            .roots
            .iter()
            .find(|n| n.kind == "merge")
            .expect("the earlier merge is a node of its own");
        assert_eq!(merge_node.id, m1.id);
        assert_eq!(
            merge_node.children.len(),
            2,
            "and it carries the two captures it was written from"
        );
        assert!(merge_node.children.iter().all(|c| c.kind == "captured"));
        let leaf = l
            .roots
            .iter()
            .find(|n| n.kind == "captured")
            .expect("the capture merged in directly is a root");
        assert!(
            leaf.source_label.contains("lines 1–2"),
            "a leaf names the lines it was written from: {}",
            leaf.source_label
        );
        assert!(leaf.source_href.contains("/ui/corpora/"));
        assert!(!l.truncated);
    }

    /// A first-generation merge has no intermediate to show, and must not
    /// invent one: `via == root` there, and every root hangs off the artifact.
    #[tokio::test]
    async fn a_first_generation_merge_is_one_level() {
        let s = Store::memory().await.unwrap();
        let ids = captured(&s, 2).await;
        let m = s
            .insert_merged_artifact(&merged("only pass"), &ids)
            .await
            .unwrap();

        let l = build(&s, &m.id).await.unwrap();

        assert_eq!(l.roots.len(), 2);
        assert!(l.roots.iter().all(|n| n.children.is_empty()));
        assert!(l.roots.iter().all(|n| n.kind == "captured"));
    }

    /// A level read top to bottom is a history. Ordered by id it is a history
    /// in no order at all.
    #[tokio::test]
    async fn a_level_is_ordered_oldest_first() {
        let s = Store::memory().await.unwrap();
        let ids = captured(&s, 3).await;
        let m1 = s
            .insert_merged_artifact(&merged("first pass"), &ids[0..2])
            .await
            .unwrap();
        let m2 = s
            .insert_merged_artifact(&merged("second pass"), &[m1.id.clone(), ids[2].clone()])
            .await
            .unwrap();

        let l = build(&s, &m2.id).await.unwrap();

        let times: Vec<i64> = l.roots.iter().map(|n| n.created_at).collect();
        let mut want = times.clone();
        want.sort();
        assert_eq!(times, want, "the top level runs backwards");
        assert_eq!(
            l.roots.last().unwrap().id,
            m1.id,
            "the merge was written after the capture beside it"
        );
    }

    /// Depth first, with the depth: the template draws indentation from it, so
    /// a child arriving before its parent would draw the history backwards.
    #[tokio::test]
    async fn flattening_keeps_each_node_under_its_parent() {
        let s = Store::memory().await.unwrap();
        let ids = captured(&s, 3).await;
        let m1 = s
            .insert_merged_artifact(&merged("first pass"), &ids[0..2])
            .await
            .unwrap();
        let m2 = s
            .insert_merged_artifact(&merged("second pass"), &[m1.id.clone(), ids[2].clone()])
            .await
            .unwrap();

        let l = build(&s, &m2.id).await.unwrap();
        let flat = l.flat_roots();

        assert_eq!(
            flat.len(),
            4,
            "one capture, one merge, and its two captures"
        );
        let at = |id: &str| flat.iter().position(|f| f.n.id == id).unwrap();
        let merge = at(&m1.id);
        assert_eq!(flat[merge].depth, 0);
        assert!(
            flat[merge + 1].depth == 1 && flat[merge + 2].depth == 1,
            "the merge's own sources follow it, indented"
        );
        assert!(flat.last().unwrap().last, "the last node closes its level");
    }

    /// A captured artifact was written from a document, not from artifacts.
    /// The pane keeps showing it the document; an empty tree is what says so.
    #[tokio::test]
    async fn a_captured_artifact_has_no_tree() {
        let s = Store::memory().await.unwrap();
        let ids = captured(&s, 1).await;
        let l = build(&s, &ids[0]).await.unwrap();
        assert!(l.is_empty());
    }

    /// The merge superseded its roots, and the tree says so on the nodes
    /// themselves rather than leaving the reader to infer it.
    #[tokio::test]
    async fn a_root_the_merge_replaced_is_marked_on_its_node() {
        let s = Store::memory().await.unwrap();
        let ids = captured(&s, 2).await;
        let m = s
            .insert_merged_artifact(&merged("the merge"), &ids)
            .await
            .unwrap();
        s.set_superseded_by(&ids[0], Some(&m.id)).await.unwrap();

        let l = build(&s, &m.id).await.unwrap();

        let replaced: Vec<&LineageNode> = l.roots.iter().filter(|n| n.replaced).collect();
        assert_eq!(replaced.len(), 1);
        assert_eq!(replaced[0].id, ids[0]);
        assert!(
            l.also_replaced.is_empty(),
            "a root it was written from belongs in the tree, not beside it"
        );
    }

    /// Superseding a near-duplicate is not merging: nothing checked that the
    /// winner's text carries the loser's. Listing them together would make
    /// that claim.
    #[tokio::test]
    async fn a_near_duplicate_it_replaced_is_kept_out_of_the_tree() {
        let s = Store::memory().await.unwrap();
        let ids = captured(&s, 3).await;
        let m = s
            .insert_merged_artifact(&merged("the merge"), &ids[0..2])
            .await
            .unwrap();
        s.set_superseded_by(&ids[2], Some(&m.id)).await.unwrap();

        let l = build(&s, &m.id).await.unwrap();

        assert_eq!(l.roots.len(), 2, "the tree is what it was written from");
        assert_eq!(l.also_replaced.len(), 1);
        assert_eq!(l.also_replaced[0].id, ids[2]);
    }

    /// The text still carries what the deleted source said. A branch that
    /// simply vanished would claim less provenance than the artifact has.
    #[tokio::test]
    async fn a_source_deleted_since_is_named_rather_than_dropped() {
        let s = Store::memory().await.unwrap();
        let ids = captured(&s, 2).await;
        let m = s
            .insert_merged_artifact(&merged("the merge"), &ids)
            .await
            .unwrap();
        s.delete_artifact(&ids[0]).await.unwrap();

        let l = build(&s, &m.id).await.unwrap();

        // The row is gone with its lineage row, so what survives is one live
        // source; either way nothing here may claim two.
        assert!(
            l.roots
                .iter()
                .all(|n| !n.missing || n.title == "deleted since"),
            "a missing source is stated"
        );
        assert!(l.roots.iter().any(|n| n.id == ids[1]));
        assert!(!l.roots.iter().any(|n| n.id == ids[0] && !n.missing));
    }
}
