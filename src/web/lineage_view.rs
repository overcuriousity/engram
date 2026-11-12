//! How an artifact came to exist, as a tree built from `artifact_sources.via_id`.
//! It shows assembly, not authorship: text edits keep no revisions.

use crate::error::Result;
use crate::store::Store;
use crate::store::artifacts::Chunk;
use std::collections::{HashMap, HashSet};

/// The walk reads rows rather than reasoning about them, so a pathological history is truncated, and says so.
const MAX_DEPTH: usize = 6;
const MAX_NODES: usize = 200;

/// What `source_href` is built from, and what crosses the API.
#[derive(Clone, serde::Serialize)]
pub struct NodeSource {
    pub corpus_id: String,
    /// The title, the URL where there is none, or a sentence saying it is no longer stored.
    pub label: String,
    /// Absent for an artifact written before spans were recorded, or restored from the vector store.
    pub start_line: Option<i64>,
    pub end_line: Option<i64>,
}

/// Serialised for `GET /api/v1/artifacts/{id}/lineage`; `when`, `source_href` and `source_label` are HTML-only.
#[derive(serde::Serialize)]
pub struct LineageNode {
    pub id: String,
    #[serde(rename = "label")]
    pub title: String,
    /// Whether `title` is a given name; see `ui::RowLabel`.
    pub named: bool,
    /// `merge` or `captured` — what kind of artifact this is, in one word.
    pub kind: &'static str,
    #[serde(skip)]
    pub when: String,
    /// Not rendered; levels are ordered by it.
    pub created_at: i64,
    /// Deep-linked to its exact lines; empty for a merge.
    #[serde(skip)]
    pub source_href: String,
    #[serde(skip)]
    pub source_label: String,
    /// `None` for a merge.
    pub source: Option<NodeSource>,
    /// Superseded by the tree's root, the ordinary outcome of a merge.
    pub replaced: bool,
    /// A source deleted since the merge; named so the tree does not silently lose a branch.
    pub missing: bool,
    pub children: Vec<LineageNode>,
}

/// `Default` is the empty tree, also the fallback when the walk fails.
#[derive(Default, serde::Serialize)]
pub struct Lineage {
    /// Empty for a captured artifact.
    pub roots: Vec<LineageNode>,
    /// Kept apart: a dedupe supersede is not a claim that this was written from it.
    pub also_replaced: Vec<LineageNode>,
    /// The walk hit `MAX_DEPTH` or `MAX_NODES`; the page says so.
    pub truncated: bool,
}

impl Lineage {
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty() && self.also_replaced.is_empty()
    }

    pub fn flat_roots(&self) -> Vec<FlatNode<'_>> {
        let mut out = Vec::new();
        flatten(&self.roots, 0, &mut out);
        out
    }

    /// Flat: nothing was written from them.
    pub fn flat_replaced(&self) -> Vec<FlatNode<'_>> {
        let mut out = Vec::new();
        flatten(&self.also_replaced, 0, &mut out);
        out
    }

    /// The generations between are the route, not sources.
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

/// Askama cannot include a template recursively, so the tree is flattened here with its depth.
pub struct FlatNode<'a> {
    pub depth: usize,
    /// Last child of its parent, for the connector glyph.
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

/// Corpus titles are cached: one query per document, not per leaf.
struct Walk<'a> {
    store: &'a Store,
    corpora: HashMap<String, String>,
    replaced: HashSet<String>,
    /// Cycle guard, and which superseded artifacts still need their own section.
    seen: HashSet<String>,
    nodes: usize,
    truncated: bool,
}

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

    // Read after the walk, so "already in the tree" means the whole tree.
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

/// Deterministic order, so two renders do not shuffle.
fn sorted(mut ids: Vec<String>) -> Vec<String> {
    ids.sort();
    ids
}

impl Walk<'_> {
    async fn expand(&mut self, id: &str, depth: usize) -> Result<Vec<LineageNode>> {
        if depth >= MAX_DEPTH || self.nodes >= MAX_NODES {
            self.truncated = true;
            return Ok(Vec::new());
        }
        let rows = self.store.sources_with_via(id).await?;

        // `via == root` is a direct root; anything else groups under its intermediate, which is the generation.
        let mut direct: Vec<String> = Vec::new();
        let mut through: HashMap<String, Vec<String>> = HashMap::new();
        for (root, via) in rows {
            match via {
                // `None` is an intermediate deleted since; the root still shows.
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
            // A cycle cannot happen (sources are always older), but the guard is cheap.
            if !self.seen.insert(via.clone()) {
                continue;
            }
            let mut n = self.node(&via).await;
            n.children = Box::pin(self.expand(&via, depth + 1)).await?;
            if n.children.is_empty() {
                // The intermediate's rows are gone; show the roots under an empty parent.
                for root in sorted(through[&via].clone()) {
                    self.seen.insert(root.clone());
                    n.children.push(self.node(&root).await);
                }
            }
            out.push(n);
        }
        // Oldest first, so the level reads in the order things happened.
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.title.cmp(&b.title)));
        Ok(out)
    }

    async fn node(&mut self, id: &str) -> LineageNode {
        self.nodes += 1;
        let replaced = self.replaced.contains(id);
        let Ok(c) = self.store.get_artifact(id).await else {
            return LineageNode {
                id: id.to_string(),
                title: "deleted since".into(),
                // Not a name: the tree saying what happened, set as text.
                named: false,
                kind: "gone",
                when: String::new(),
                // Unknown age sorts to the front, where a reader looking for what is missing will be.
                created_at: 0,
                source_href: String::new(),
                source_label: String::new(),
                source: None,
                replaced,
                missing: true,
                children: Vec::new(),
            };
        };
        let (source_href, source_label, source) = self.source_of(&c).await;
        let label = crate::web::ui::row_label(&c);
        LineageNode {
            title: label.text,
            named: label.named,
            kind: match c.provenance {
                crate::store::artifacts::Provenance::Merged => "merge",
                crate::store::artifacts::Provenance::Synthesized => "synthesized",
                crate::store::artifacts::Provenance::Passage => "passage",
                crate::store::artifacts::Provenance::Captured => "captured",
                crate::store::artifacts::Provenance::Note => "note",
            },
            when: crate::fmt::fmt_time(c.created_at),
            created_at: c.created_at,
            source_href,
            source_label,
            source,
            replaced,
            missing: false,
            id: c.id,
            children: Vec::new(),
        }
    }

    /// The same deep link as the pane's Source label.
    async fn source_of(&mut self, c: &Chunk) -> (String, String, Option<NodeSource>) {
        let Some(cid) = c.corpus_id.clone() else {
            return (String::new(), String::new(), None);
        };
        let title = match self.corpora.get(&cid) {
            Some(t) => t.clone(),
            None => {
                let t = match self.store.get_corpus(&cid).await {
                    Ok(s) => s
                        .title_hint
                        .or(s.source_url)
                        .unwrap_or_else(|| "untitled".into()),
                    // The document row is gone; the link still resolves to a page that says so.
                    Err(_) => "a document no longer stored".to_string(),
                };
                self.corpora.insert(cid.clone(), t.clone());
                t
            }
        };
        let source = Some(NodeSource {
            corpus_id: cid.clone(),
            label: title.clone(),
            start_line: c.corpus_span.as_ref().map(|sp| sp.start_line),
            end_line: c.corpus_span.as_ref().map(|sp| sp.end_line),
        });
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
                source,
            ),
            // No span: written before spans were recorded, or restored from the vector store.
            None => (
                format!("/ui/corpora/{cid}"),
                format!("from {title}"),
                source,
            ),
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
                ..Default::default()
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
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_passage_in_the_tree_is_named_by_how_its_text_opens() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("one\ntwo", "web", None).await.unwrap();
        let p = s
            .insert_artifacts_with_provenance(
                &src.id,
                &[NewArtifact {
                    text: "Der Vorgang setzt voraus, dass das Journal noch steht.".into(),
                    title: Some("Kapitel 3".into()),
                    ..Default::default()
                }],
                crate::store::artifacts::Provenance::Passage,
            )
            .await
            .unwrap();
        let winner = captured(&s, 1).await;
        s.set_superseded_by(&p[0].id, Some(&winner[0]))
            .await
            .unwrap();

        let l = build(&s, &winner[0]).await.unwrap();

        let node = l
            .also_replaced
            .iter()
            .find(|n| n.id == p[0].id)
            .expect("the passage it replaced is on the page");
        assert!(
            node.title.starts_with("Der Vorgang setzt voraus"),
            "the node read {:?}",
            node.title
        );
        assert!(!node.named, "the opening of a text is not a name");
    }

    #[tokio::test]
    async fn a_node_named_by_its_opening_is_set_as_text() {
        let core = crate::core::test_support::test_core().await;
        let src = core
            .store
            .insert_corpus("one\ntwo", "web", None)
            .await
            .unwrap();
        let p = core
            .store
            .insert_artifacts_with_provenance(
                &src.id,
                &[NewArtifact {
                    text: "Der Vorgang setzt voraus, dass das Journal noch steht.".into(),
                    title: Some("Kapitel 3".into()),
                    ..Default::default()
                }],
                crate::store::artifacts::Provenance::Passage,
            )
            .await
            .unwrap();
        let winner = captured(&core.store, 1).await;
        core.store
            .set_superseded_by(&p[0].id, Some(&winner[0]))
            .await
            .unwrap();

        let d = crate::web::artifact::build_artifact_detail(&core, &winner[0], "")
            .await
            .unwrap();
        let html =
            askama::Template::render(&crate::web::artifact::ArtifactDetailFragment { d }).unwrap();
        assert!(html.contains("Der Vorgang setzt voraus"), "{html}");
        assert!(html.contains("name-opening"), "{html}");
        let css = include_str!("../../assets/css/73-components.css");
        assert!(css.contains(".name-opening"), "the class has no rule");
    }

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

    #[tokio::test]
    async fn a_captured_artifact_has_no_tree() {
        let s = Store::memory().await.unwrap();
        let ids = captured(&s, 1).await;
        let l = build(&s, &ids[0]).await.unwrap();
        assert!(l.is_empty());
    }

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

        assert!(
            l.roots
                .iter()
                .all(|n| !n.missing || n.title == "deleted since"),
            "a missing source is stated"
        );
        assert!(l.roots.iter().any(|n| n.id == ids[1]));
        assert!(!l.roots.iter().any(|n| n.id == ids[0] && !n.missing));
    }

    async fn json_lineage(core: crate::core::Core, id: &str) -> (u16, serde_json::Value) {
        use tower::ServiceExt;
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/api/v1/artifacts/{id}/lineage"))
                    .header("cookie", cookie)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status().as_u16();
        (status, crate::web::test_support::json_of(res).await)
    }

    #[tokio::test]
    async fn the_json_tree_nests_a_generation_and_carries_facts_not_links() {
        let core = crate::core::test_support::test_core().await;
        let s = &core.store;
        let ids = captured(s, 3).await;
        let m1 = s
            .insert_merged_artifact(&merged("first pass"), &ids[0..2])
            .await
            .unwrap();
        let m2 = s
            .insert_merged_artifact(&merged("second pass"), &[m1.id.clone(), ids[2].clone()])
            .await
            .unwrap();

        let (status, tree) = json_lineage(core.clone(), &m2.id).await;

        assert_eq!(status, 200);
        assert_eq!(tree["truncated"], false);
        let roots = tree["roots"].as_array().unwrap();
        let gen1 = roots
            .iter()
            .find(|n| n["id"] == m1.id.as_str())
            .expect("the merge it came through is a node");
        assert_eq!(gen1["kind"], "merge");
        assert_eq!(gen1["label"], "first pass");
        assert!(gen1["source"].is_null(), "a merge belongs to no document");
        assert_eq!(gen1["children"].as_array().unwrap().len(), 2);
        let leaf = &gen1["children"][0];
        assert_eq!(leaf["kind"], "captured");
        assert_eq!(leaf["source"]["start_line"], 1);
        assert_eq!(leaf["source"]["end_line"], 2);
        assert!(leaf["created_at"].is_number());
        let text = tree.to_string();
        assert!(!text.contains("/ui/"), "an href crossed the API: {text}");
        assert!(
            !text.contains("\"when\""),
            "a formatted time crossed: {text}"
        );
    }

    #[tokio::test]
    async fn the_json_tree_keeps_what_was_replaced_apart_and_names_what_is_gone() {
        let core = crate::core::test_support::test_core().await;
        let s = &core.store;
        let ids = captured(s, 3).await;
        let m = s
            .insert_merged_artifact(&merged("the merge"), &ids[0..2])
            .await
            .unwrap();
        s.set_superseded_by(&ids[2], Some(&m.id)).await.unwrap();
        s.delete_artifact(&ids[0]).await.unwrap();

        let (_, tree) = json_lineage(core.clone(), &m.id).await;

        assert_eq!(tree["also_replaced"][0]["id"], ids[2].as_str());
        for n in tree["roots"].as_array().unwrap() {
            if n["id"] == ids[0].as_str() {
                assert_eq!(n["missing"], true, "a deleted source is stated: {n}");
            }
        }
    }

    #[tokio::test]
    async fn an_artifact_with_no_history_is_an_empty_tree_and_an_unknown_one_a_404() {
        let core = crate::core::test_support::test_core().await;
        let ids = captured(&core.store, 1).await;

        let (status, tree) = json_lineage(core.clone(), &ids[0]).await;
        assert_eq!(status, 200);
        assert!(tree["roots"].as_array().unwrap().is_empty());

        let (status, err) = json_lineage(core, "nope").await;
        assert_eq!(status, 404);
        assert_eq!(err["error"], "not found");
    }

    #[tokio::test]
    async fn versions_are_listed_oldest_first_in_the_envelope() {
        use tower::ServiceExt;
        let core = crate::core::test_support::test_core().await;
        let ids = captured(&core.store, 1).await;
        for text in ["shorter", "shortest"] {
            core.store
                .condense_artifact(&ids[0], None, text, None, &[], serde_json::json!({}))
                .await
                .unwrap()
                .expect("written");
        }
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;
        let get = |uri: String| {
            axum::http::Request::builder()
                .uri(uri)
                .header("cookie", &cookie)
                .body(axum::body::Body::empty())
                .unwrap()
        };
        let res = app
            .clone()
            .oneshot(get(format!("/api/v1/artifacts/{}/versions", ids[0])))
            .await
            .unwrap();
        assert!(res.headers().contains_key("etag"));
        let body = crate::web::test_support::json_of(res).await;
        assert!(body["next"].is_null());
        let items = body["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["n"], 1);
        assert_eq!(items[0]["text"], "artifact 0", "version 1 is the original");
        assert_eq!(items[1]["text"], "shorter");

        let res = app
            .oneshot(get("/api/v1/artifacts/nope/versions".into()))
            .await
            .unwrap();
        assert_eq!(res.status(), 404);
    }
}
