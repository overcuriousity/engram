//! One captured document: its passages, what each became, coverage, and the actions on it.
//! The source-line pane itself is `web::corpus_view`.

use crate::error::Error;
use crate::store::corpora::CorpusStatus;
use crate::tenants::Tenant;
use crate::web::auth_routes::HtmlTemplate;
use crate::web::state::AppState;
use crate::web::ui::{ArtifactView, artifact_view, status_badge};
use crate::web::ui_error::UiResult;
use askama::Template;
use axum::Router;
use axum::extract::{Form, Path, Query};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};

#[derive(Template)]
#[template(path = "corpus.html")]
struct CorpusTemplate {
    id: String,
    status: String,
    badge: &'static str,
    /// A placeholder for a source never captured here: `raw_text` is its restored artifacts joined.
    restored: bool,
    /// The last hop back to where the text came from.
    source_url: Option<String>,
    /// The lines below are the model's reading of the photo.
    image: bool,
    /// The lines below are docling's extraction, not the layout.
    pdf: bool,
    /// A captured link, which can be fetched again.
    link: bool,
    /// Still `describing` or `extracting`, or parked before any text was read: only re-reading is offered.
    unread: bool,
    /// Already formatted.
    meta_rows: Vec<(String, String)>,
    /// Folded away on the page; the original is not stored, so this is the only copy.
    exif_rows: Vec<(String, String)>,
    note: Option<String>,
    /// Empty for a restored placeholder or an unread photo, which fall back to the flat rendering.
    bands: Vec<BandView>,
    /// Each with an undo that puts the verbatim text back in results.
    promoted: Vec<PromotedWindow>,
    /// Artifacts naming no lines, listed separately so the only page that can edit them still shows them.
    unplaced: Vec<ArtifactView>,
    /// A merge belongs to every corpus it drew from.
    written_from: Vec<ArtifactView>,
    lines_empty: bool,
    /// Unnumbered: only a restored placeholder reaches it, and line numbers would claim a document.
    raw_text: String,
    /// As the Recent list measures it; stated whether or not a band is red, since the two can disagree.
    coverage: Option<String>,
    /// The same label Recent, the idle foot and the day page use.
    title: String,
    artifact_count: usize,
}

impl CorpusTemplate {
    /// A source belongs to no section, so nothing in the row is current.
    fn section(&self) -> &'static str {
        ""
    }
}

/// Absent for an ordinary visit.
#[derive(serde::Deserialize, Default)]
struct LineRange {
    from: Option<i64>,
    to: Option<i64>,
}

#[derive(serde::Deserialize)]
struct RereadForm {
    /// Both ends: an uncovered run does not stop at a window boundary.
    from: i64,
    to: i64,
}

async fn reread_uncovered_ui(
    tenant: Tenant,
    Path(cid): Path<String>,
    Form(f): Form<RereadForm>,
) -> UiResult<Response> {
    tenant.core.reread_uncovered(&cid, f.from, f.to).await?;
    // Back to the band, so a long document keeps the reader's place.
    Ok(Redirect::to(&format!("/ui/corpora/{cid}#L{}", f.from)).into_response())
}

/// The window's passages back in results, what the promotion wrote retired.
async fn unpromote_ui(tenant: Tenant, Path((cid, idx)): Path<(String, i64)>) -> UiResult<Response> {
    tenant.core.undo_promotion(&cid, idx).await?;
    tenant
        .core
        .store
        .undo_action_on(
            &crate::jobs::promote::window_key(&cid, idx),
            crate::store::actions::Kind::Promote,
            crate::store::actions::UndoneBy::Operator,
            "unpromoted",
        )
        .await?;
    Ok(Redirect::to(&format!("/ui/corpora/{cid}")).into_response())
}

async fn corpus_detail(
    tenant: Tenant,
    Path(cid): Path<String>,
    Query(range): Query<LineRange>,
) -> UiResult<Response> {
    let s = tenant.core.store.get_corpus(&cid).await?;
    let chunks = tenant
        .core
        .store
        .artifacts_for_corpus(&cid)
        .await?
        .into_iter()
        .filter(|c| c.status != crate::store::artifacts::ArtifactStatus::Trashed)
        .collect::<Vec<_>>();
    let restored = s.restored_at.is_some();

    // A restored placeholder keeps the flat rendering: its spans point at artifacts, not a source.
    let spans: Vec<(String, crate::store::artifacts::CorpusSpan)> = if restored {
        Vec::new()
    } else {
        chunks
            .iter()
            .filter_map(|c| c.corpus_span.clone().map(|sp| (c.id.clone(), sp)))
            .collect()
    };
    let by_id: std::collections::HashMap<&str, &crate::store::artifacts::Chunk> =
        chunks.iter().map(|c| (c.id.as_str(), c)).collect();

    // Artifacts naming no lines cannot be banded; they get their own section below the source.
    let unplaced: Vec<ArtifactView> = chunks
        .iter()
        .filter(|c| restored || c.corpus_span.is_none())
        .map(artifact_view)
        .collect();

    let segments = tenant.core.store.segments_for_corpus(&cid).await?;
    // Nothing is a loss until reading has finished, nor while any artifact names no lines.
    let losses_are_final = crate::core::ingest::coverage_final(&s.status)
        && !segments.is_empty()
        && unplaced.is_empty();

    // Rows keep their `L<n>` anchors inside bands.
    // An overlapping artifact is carded once, in its first band: copies would share element ids.
    let mut carded: std::collections::HashSet<String> = std::collections::HashSet::new();
    let bands: Vec<BandView> = if restored {
        Vec::new()
    } else {
        crate::web::corpus_view::bands(
            &s.raw_text,
            &spans,
            range.from.map(|f| (f, range.to.unwrap_or(f))),
        )
        .into_iter()
        .map(|b| {
            let (mut artifacts, mut echoes) = (Vec::new(), Vec::new());
            for c in b
                .artifact_ids
                .iter()
                .filter_map(|id| by_id.get(id.as_str()))
            {
                if carded.insert(c.id.clone()) {
                    artifacts.push(artifact_view(c));
                } else {
                    let label = crate::web::ui::row_label(c);
                    echoes.push(BandEcho {
                        id: c.id.clone(),
                        label: label.text,
                        named: label.named,
                    });
                }
            }
            BandView {
                // The re-read covers the whole window holding this passage, which is wider than it.
                reread: (b.gap() && losses_are_final)
                    .then(|| {
                        segments
                            .iter()
                            .filter(|w| w.start_line <= b.to && b.from <= w.end_line)
                            .fold(None::<(i64, i64)>, |acc, w| {
                                Some(match acc {
                                    Some((a, z)) => (a.min(w.start_line), z.max(w.end_line)),
                                    None => (w.start_line, w.end_line),
                                })
                            })
                            .map(|(a, z)| format!("reads lines {a}–{z}"))
                    })
                    .flatten(),
                gap: b.gap() && losses_are_final,
                from: b.from,
                to: b.to,
                artifacts,
                echoes,
                lines: b.lines,
            }
        })
        .collect()
    };

    // Stated whether or not anything is red: Recent's warning measures coverage differently.
    let coverage = s.coverage.map(|c| format!("{:.0}%", c * 100.0));
    // A verbatim capture is 100% by construction, so stated only once something was written.
    let coverage = if chunks
        .iter()
        .all(|c| c.provenance == crate::store::artifacts::Provenance::Passage)
    {
        None
    } else {
        coverage
    };
    let image = s.origin == crate::core::ingest::ORIGIN_IMAGE;
    let pdf = s.origin == crate::core::ingest::ORIGIN_PDF;
    let link = s.origin == crate::core::ingest::ORIGIN_FETCH && s.source_url.is_some();
    let unread = (image && (s.status == CorpusStatus::Describing || s.raw_text.trim().is_empty()))
        || (pdf && (s.status == CorpusStatus::Extracting || s.raw_text.trim().is_empty()));
    let note = s.metadata["note"].as_str().map(str::to_string);
    let meta_rows = metadata_rows(&s.metadata);
    let exif_rows = exif_tag_rows(&s.metadata);
    let written_from: Vec<ArtifactView> = tenant
        .core
        .store
        .artifacts_originating_in(&cid)
        .await?
        .iter()
        .filter(|c| c.in_results())
        .map(artifact_view)
        .collect();
    let promoted = promoted_windows(&segments, &chunks);
    Ok(HtmlTemplate(CorpusTemplate {
        id: s.id,
        badge: status_badge(&s.status),
        status: s.status.as_str().to_string(),
        restored,
        source_url: s.source_url.clone(),
        image,
        pdf,
        link,
        unread,
        meta_rows,
        exif_rows,
        note,
        bands,
        promoted,
        unplaced,
        written_from,
        lines_empty: s.raw_text.trim().is_empty(),
        raw_text: s.raw_text.clone(),
        coverage,
        title: crate::web::ui::corpus_label(s.title_hint.clone(), &s.raw_text, &s.origin),
        artifact_count: chunks.len(),
    })
    .into_response())
}

/// The rest of what the camera wrote, folded: the original file is not kept.
pub(crate) fn exif_tag_rows(m: &serde_json::Value) -> Vec<(String, String)> {
    let Some(tags) = m["exif"]["tags"].as_object() else {
        return Vec::new();
    };
    let mut rows: Vec<(String, String)> = tags
        .iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}

/// In reading order; everything else is under `exif.tags`.
pub(crate) fn metadata_rows(m: &serde_json::Value) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    let exif = &m["exif"];
    if let Some(t) = exif["taken_at"].as_str() {
        rows.push(("Taken".into(), t.into()));
    }
    if let Some(c) = exif["camera"].as_str() {
        rows.push(("Camera".into(), c.into()));
    }
    if let (Some(lat), Some(lon)) = (exif["gps"]["lat"].as_f64(), exif["gps"]["lon"].as_f64()) {
        rows.push(("Location".into(), format!("{lat}, {lon}")));
    }
    let f = &m["file"];
    if let Some(n) = f["name"].as_str() {
        rows.push(("File".into(), n.into()));
    }
    if let (Some(w), Some(h)) = (f["width"].as_u64(), f["height"].as_u64()) {
        rows.push(("Size".into(), format!("{w}×{h}")));
    }
    if let Some(e) = m["describe"]["error"].as_str() {
        rows.push(("Reading".into(), e.into()));
    }
    if let Some(e) = m["extract"]["error"].as_str() {
        rows.push(("Extraction".into(), e.into()));
    }
    // A link stored because its page could not be read: say why and whether it is still tried.
    if let Some(e) = m["fetch"]["error"].as_str() {
        let said = if m["fetch"]["pending"].as_bool() == Some(true) {
            format!("not read yet ({e}) — tried again in the background")
        } else {
            format!("could not be read ({e}) — Fetch again to retry")
        };
        rows.push(("Page".into(), said));
    }
    rows
}

async fn corpus_title(tenant: &Tenant, cid: &str) -> crate::error::Result<String> {
    let s = tenant.core.store.get_corpus(cid).await?;
    Ok(crate::web::ui::corpus_label(
        s.title_hint.clone(),
        &s.raw_text,
        &s.origin,
    ))
}

/// Irreversible, so the source's own name has to be typed.
async fn delete_corpus_ui(
    tenant: Tenant,
    Path(cid): Path<String>,
    Form(f): Form<crate::web::settings::TypedName>,
) -> UiResult<Response> {
    let title = corpus_title(&tenant, &cid).await?;
    if f.name != title {
        return Ok(crate::web::settings::name_mismatch());
    }
    tenant.core.delete_corpus(&cid).await?;
    Ok(([("hx-redirect", "/ui")], Redirect::to("/ui")).into_response())
}

#[derive(serde::Deserialize, Default)]
struct ReprocessForm {
    #[serde(default)]
    stage: Option<String>,
}

/// Re-segment by default; `stage=describe` re-reads an image, `stage=extract` a PDF.
async fn reprocess_ui(
    tenant: Tenant,
    Path(cid): Path<String>,
    Form(form): Form<ReprocessForm>,
) -> UiResult<Response> {
    let stage = match form.stage {
        None => crate::store::jobs::Stage::Synthesize,
        Some(s) => crate::store::jobs::Stage::parse(&s)
            .ok_or_else(|| Error::Validation(format!("unknown stage `{s}`")))?,
    };
    tenant.core.reprocess(&cid, stage).await?;
    Ok(Redirect::to(&format!("/ui/corpora/{cid}")).into_response())
}

/// `done` and owning at least one superseded passage; the one reading of "promoted".
pub fn promoted_windows(
    segments: &[crate::store::segments::Segment],
    chunks: &[crate::store::artifacts::Chunk],
) -> Vec<PromotedWindow> {
    segments
        .iter()
        .filter(|w| w.state == crate::store::segments::SegmentState::Done)
        .filter(|w| {
            chunks.iter().any(|c| {
                c.segment_idx == Some(w.idx)
                    && c.provenance == crate::store::artifacts::Provenance::Passage
                    && c.superseded_by.is_some()
            })
        })
        .map(|w| PromotedWindow {
            idx: w.idx,
            from: w.start_line,
            to: w.end_line,
        })
        .collect()
}

pub struct PromotedWindow {
    pub idx: i64,
    pub from: i64,
    pub to: i64,
}

/// A pointer up at an artifact carded in an earlier band; never an empty label (see `ui::RowLabel`).
pub struct BandEcho {
    pub id: String,
    pub label: String,
    pub named: bool,
}

pub struct BandView {
    pub from: i64,
    pub to: i64,
    pub lines: Vec<crate::web::corpus_view::CorpusLine>,
    pub artifacts: Vec<ArtifactView>,
    /// Card ids must be unique, so overlaps get a line pointing up at the card.
    pub echoes: Vec<BandEcho>,
    /// Nothing was written from these lines.
    pub gap: bool,
    /// The whole window a re-read would cover; `None` when no window holds it.
    pub reread: Option<String>,
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/ui/corpora/{id}", get(corpus_detail))
        .route("/ui/corpora/{id}/delete", post(delete_corpus_ui))
        .route("/ui/corpora/{id}/reprocess", post(reprocess_ui))
        .route("/ui/corpora/{id}/reread", post(reread_uncovered_ui))
        .route(
            "/ui/corpora/{id}/segments/{idx}/unpromote",
            post(unpromote_ui),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::artifact::build_artifact_detail;
    use crate::web::test_support::{
        an_unread_image, app_session_and_core, app_with_cookie, body_of, form, get_body,
    };
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn deleting_a_source_needs_its_name_typed() {
        let (app, cookie, core) = crate::web::test_support::app_session_and_core().await;
        let out = core
            .ingest("alpha bravo", "web", Some("Field notes"))
            .await
            .unwrap();
        let page =
            crate::web::test_support::get_body(&app, &cookie, &format!("/ui/corpora/{}", out.id))
                .await;
        assert!(
            page.contains("data-oc-confirm-name=\"Field notes\""),
            "{page}"
        );
        let del = |name: &str| {
            crate::web::test_support::form(
                &format!("/ui/corpora/{}/delete", out.id),
                &cookie,
                &format!("name={}", name.replace(' ', "+")),
            )
        };
        let res = app.clone().oneshot(del("Field")).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(core.store.get_corpus(&out.id).await.is_ok());
        let res = app.oneshot(del("Field notes")).await.unwrap();
        assert!(res.status().is_redirection());
        assert!(core.store.get_corpus(&out.id).await.is_err());
    }

    #[test]
    fn a_held_link_names_why_its_page_is_missing() {
        let pending =
            serde_json::json!({"fetch": {"pending": true, "error": "the server answered 503"}});
        let rows = metadata_rows(&pending);
        assert!(
            rows.iter()
                .any(|(k, v)| k == "Page" && v.contains("503") && v.contains("tried again")),
            "{rows:?}"
        );
        let given_up = serde_json::json!({"fetch": {"pending": false, "error": "timed out"}});
        let rows = metadata_rows(&given_up);
        assert!(
            rows.iter()
                .any(|(k, v)| k == "Page" && v.contains("Fetch again")),
            "{rows:?}"
        );
        let read = serde_json::json!({"fetch": {"fetched_at": 1}});
        assert!(metadata_rows(&read).is_empty());
    }

    #[tokio::test]
    async fn a_band_echo_for_a_passage_points_at_it_by_how_its_text_opens() {
        let core = crate::core::test_support::test_core().await;
        let raw = "eins\nzwei\ndrei\nvier";
        let src = core.store.insert_corpus(raw, "web", None).await.unwrap();
        let span = |a: i64, b: i64| Some(crate::store::artifacts::CorpusSpan::located(a, b));
        core.store
            .insert_artifacts_with_provenance(
                &src.id,
                &[
                    crate::store::artifacts::NewArtifact {
                        ordinal: 0,
                        text: "Der Vorgang setzt voraus, dass das Journal noch steht.".into(),
                        title: Some("Kapitel 3".into()),
                        corpus_span: span(1, 3),
                        ..Default::default()
                    },
                    crate::store::artifacts::NewArtifact {
                        ordinal: 1,
                        text: "Ein zweiter Abschnitt.".into(),
                        title: Some("Kapitel 4".into()),
                        corpus_span: span(2, 4),
                        ..Default::default()
                    },
                ],
                crate::store::artifacts::Provenance::Passage,
            )
            .await
            .unwrap();

        let (app, cookie) = app_with_cookie(core).await;
        let html = get_body(&app, &cookie, &format!("/ui/corpora/{}", src.id)).await;
        let echo = html
            .split(r#"<p class="band-echo">"#)
            .nth(1)
            .expect("a band echo is on the page");
        let echo = echo.split("</p>").next().unwrap();
        assert!(!echo.contains("Kapitel"), "the echo read {echo}");
        assert!(
            echo.contains("Der Vorgang setzt voraus"),
            "the echo read {echo}"
        );
    }

    #[tokio::test]
    async fn a_merged_artifact_shows_its_sources_instead_of_corpus_lines() {
        let core = crate::core::test_support::test_core().await;
        let ids = crate::jobs::consolidate::tests::seed(
            &core,
            &[("a text", [1.0, 0.0]), ("b text", [0.93, 0.37])],
        )
        .await;
        let m = crate::jobs::merge::write(
            &core,
            &crate::infer::prompt::MergedDraft {
                title: Some("a and b".into()),
                text: "a text and b text".into(),
                category: None,
                tags: vec![],
                caveats: vec![],
            },
            &ids,
        )
        .await
        .unwrap();

        let d = build_artifact_detail(&core, &m.id, "").await.unwrap();

        assert_eq!(d.corpus_id, None, "a merged artifact claimed a corpus");
        assert!(
            d.slice_lines.is_empty(),
            "a merged artifact rendered lines from a document it did not come from"
        );
        assert_eq!(d.lineage.leaves(), 2);
        let listed: Vec<&str> = d.lineage.roots.iter().map(|s| s.id.as_str()).collect();
        for id in &ids {
            assert!(listed.contains(&id.as_str()), "source {id} is not listed");
        }
        assert!(
            d.lineage
                .roots
                .iter()
                .all(|s| s.source_href.starts_with("/ui/corpora/"))
        );
        assert!(!d.orphaned_source);
    }

    #[tokio::test]
    async fn a_chunk_whose_source_vanished_is_not_a_500() {
        let core = crate::core::test_support::test_core().await;
        let out = core.ingest("alpha\n\nbravo", "web", None).await.unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        let c = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .remove(0);
        core.delete_corpus(&out.id).await.unwrap();

        match build_artifact_detail(&core, &c.id, "").await {
            Err(crate::error::Error::NotFound) => {}
            Err(e) => panic!("expected a not-found, got {e}"),
            Ok(_) => panic!("a chunk whose source was deleted must not resolve"),
        }
    }

    #[tokio::test]
    async fn a_pdf_corpus_page_offers_re_extract_and_names_the_failure() {
        let core = crate::core::test_support::test_core().await;
        let id = core
            .ingest_pdf(crate::core::ingest::PdfCapture {
                bytes: include_bytes!("../../tests/fixtures/one-heading.pdf").to_vec(),
                filename: Some("plan.pdf".into()),
                title_hint: None,
                note: None,
                lang: crate::infer::lang::Lang::default(),
            })
            .await
            .unwrap()
            .id;
        crate::jobs::extract::park_failed(&core, &id, "that PDF holds no extractable text")
            .await
            .unwrap();

        let (app, cookie) = app_with_cookie(core).await;
        let html = get_body(&app, &cookie, &format!("/ui/corpora/{id}")).await;
        assert!(
            html.contains("no extractable text"),
            "the reason is what the page is for: {html}"
        );
        assert!(
            html.contains(r#"value="extract""#),
            "no Re-extract on a PDF that failed: {html}"
        );
        assert!(
            html.contains(&format!("/api/v1/corpora/{id}/file")),
            "the original is not reachable: {html}"
        );
        assert!(
            !html.contains("Re-segment"),
            "nothing was extracted; there is nothing to re-segment: {html}"
        );
    }

    #[tokio::test]
    async fn an_image_corpus_page_shows_the_photo_its_facts_and_the_reading_as_derived() {
        let core = crate::core::test_support::test_core().await;
        let src = core
            .store
            .insert_attached_corpus(
                "h",
                "image",
                Some("IMG.png"),
                None,
                &serde_json::json!({
                    "note": "front porch",
                    "file": {"name": "IMG.png", "width": 4, "height": 2},
                    "exif": {"taken_at": "2026-08-09T14:12:03", "camera": "Pixel",
                             "gps": {"lat": 1.5, "lon": 2.5},
                             "tags": {"LensModel": "24mm f/1.8", "ExposureTime": "1/120"}}
                }),
                crate::store::corpora::Reading::VISION,
                &crate::store::attachments::NewFile {
                    kind: "image",
                    mime: "image/png",
                    filename: Some("IMG.png"),
                    bytes: b"orig",
                    preview: b"prev",
                    width: Some(4),
                    height: Some(2),
                },
            )
            .await
            .unwrap()
            .into_corpus();
        core.store
            .set_read_text(&src.id, "# Porch\n\nblue door", vec![])
            .await
            .unwrap();
        let (app, cookie) = app_with_cookie(core).await;
        let html = get_body(&app, &cookie, &format!("/ui/corpora/{}", src.id)).await;
        assert!(
            html.contains(&format!("/api/v1/corpora/{}/image", src.id)),
            "img src"
        );
        assert_eq!(
            html.matches("front porch").count(),
            1,
            "the note belongs to the photo card and is printed there once: {html}"
        );
        assert!(html.contains("2026-08-09T14:12:03"));
        assert!(html.contains("1.5"));
        assert!(html.contains("All 2 EXIF tags"));
        let (exposure, lens) = (
            html.find("ExposureTime").expect("exposure tag"),
            html.find("LensModel").expect("lens tag"),
        );
        assert!(exposure < lens, "the tags are listed by name");
        assert!(html.contains("24mm f/1.8"));
        assert!(
            html.contains("Transcription"),
            "the text is labelled as derived, not 'Raw corpus'"
        );
        assert!(html.contains("blue door"));
    }

    #[tokio::test]
    async fn an_unread_image_page_offers_re_read_and_not_re_segment() {
        let core = crate::core::test_support::test_core().await;
        let id = an_unread_image(&core).await;
        let (app, cookie) = app_with_cookie(core).await;
        let html = get_body(&app, &cookie, &format!("/ui/corpora/{id}")).await;
        assert!(html.contains("Re-read"));
        assert!(!html.contains("Re-segment"));
    }

    #[tokio::test]
    async fn the_re_read_button_queues_describe() {
        let core = crate::core::test_support::test_core().await;
        let id = an_unread_image(&core).await;
        crate::jobs::describe::park_failed(&core, &id, "HTTP 400")
            .await
            .unwrap();
        let (app, cookie) = app_with_cookie(core.clone()).await;
        let res = app
            .oneshot(form(
                &format!("/ui/corpora/{id}/reprocess"),
                &cookie,
                "stage=describe",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            core.store.get_corpus(&id).await.unwrap().status,
            CorpusStatus::Describing
        );
    }

    #[tokio::test]
    async fn source_detail_shows_the_raw_text() {
        let (app, cookie, core) = app_session_and_core().await;
        let res = app
            .clone()
            .oneshot(form(
                "/ui/capture",
                &cookie,
                "text=alpha+para%0A%0Abeta+para",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let id = core.store.list_corpora(10, 0).await.unwrap()[0].id.clone();

        let res = app
            .oneshot(
                Request::builder()
                    .uri(format!("/ui/corpora/{id}"))
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(body_of(res).await.contains("alpha para"));
    }

    #[tokio::test]
    async fn a_capture_still_being_read_names_no_loss_and_offers_no_re_read() {
        use crate::store::segments::NewSegment;
        let (app, cookie, core) = app_session_and_core().await;
        let out = core
            .ingest("alpha beta\ngamma delta", "web", None)
            .await
            .unwrap();
        core.store
            .upsert_segments(
                &out.id,
                &[NewSegment {
                    start_line: 1,
                    end_line: 2,
                    text: "alpha beta\ngamma delta",
                }],
            )
            .await
            .unwrap();
        core.store
            .set_corpus_status(&out.id, CorpusStatus::Segmenting)
            .await
            .unwrap();

        let page = get_body(&app, &cookie, &format!("/ui/corpora/{}", out.id)).await;
        assert!(
            !page.contains(r#"id="uncovered""#),
            "an unread window was named as a loss: {page}"
        );
        assert!(!page.contains("Read these again"), "{page}");

        let res = app
            .clone()
            .oneshot(form(
                &format!("/ui/corpora/{}/reread", out.id),
                &cookie,
                "from=1&to=6",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert!(
            !core
                .store
                .live_job(
                    crate::store::jobs::Stage::SegmentWindow,
                    &crate::jobs::window::unit_target(&out.id, 0)
                )
                .await
                .unwrap(),
            "a window that had not been read yet was queued to be read again"
        );
    }

    #[tokio::test]
    async fn a_window_already_queued_is_not_re_read_a_second_time() {
        use crate::store::segments::{NewSegment, SegmentState};
        let (app, cookie, core) = app_session_and_core().await;
        let out = core
            .ingest(
                "alpha beta\ngamma delta\nomega sigma\nkappa lambda",
                "web",
                None,
            )
            .await
            .unwrap();
        core.store
            .upsert_segments(
                &out.id,
                &[
                    NewSegment {
                        start_line: 1,
                        end_line: 2,
                        text: "alpha beta\ngamma delta",
                    },
                    NewSegment {
                        start_line: 3,
                        end_line: 4,
                        text: "omega sigma\nkappa lambda",
                    },
                ],
            )
            .await
            .unwrap();
        for idx in [0, 1] {
            core.store
                .set_segment_state(&out.id, idx, SegmentState::Done, None)
                .await
                .unwrap();
        }
        core.store
            .set_corpus_status(&out.id, CorpusStatus::Partial)
            .await
            .unwrap();
        core.store
            .enqueue(
                crate::store::jobs::Stage::SegmentWindow,
                "segment",
                &crate::jobs::window::unit_target(&out.id, 0),
            )
            .await
            .unwrap();

        let res = app
            .clone()
            .oneshot(form(
                &format!("/ui/corpora/{}/reread", out.id),
                &cookie,
                "from=1&to=6",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);

        let states: Vec<SegmentState> = core
            .store
            .segments_for_corpus(&out.id)
            .await
            .unwrap()
            .iter()
            .map(|w| w.state)
            .collect();
        assert_eq!(
            states,
            vec![SegmentState::Done, SegmentState::Pending],
            "the window already queued was reset under the worker holding it"
        );
    }

    #[tokio::test]
    async fn a_loss_crossing_a_window_boundary_re_reads_both_windows() {
        // An uncovered run spans windows; matching its first line alone re-read only one.
        use crate::store::segments::{NewSegment, SegmentState};
        let (app, cookie, core) = app_session_and_core().await;
        let out = core
            .ingest("one\ntwo\nthree\nfour\nfive\nsix", "web", None)
            .await
            .unwrap();
        core.store
            .upsert_segments(
                &out.id,
                &[
                    NewSegment {
                        start_line: 1,
                        end_line: 3,
                        text: "one\ntwo\nthree",
                    },
                    NewSegment {
                        start_line: 4,
                        end_line: 6,
                        text: "four\nfive\nsix",
                    },
                ],
            )
            .await
            .unwrap();
        for idx in [0, 1] {
            core.store
                .set_segment_state(&out.id, idx, SegmentState::Done, None)
                .await
                .unwrap();
        }
        core.store
            .set_corpus_status(&out.id, CorpusStatus::Partial)
            .await
            .unwrap();

        let res = app
            .clone()
            .oneshot(form(
                &format!("/ui/corpora/{}/reread", out.id),
                &cookie,
                "from=1&to=6",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);

        let pending: Vec<i64> = core
            .store
            .pending_segments(&out.id)
            .await
            .unwrap()
            .iter()
            .map(|w| w.idx)
            .collect();
        assert_eq!(pending, vec![0, 1], "the tail of the loss was left unread");
    }

    #[tokio::test]
    async fn a_fully_covered_corpus_marks_nothing_red() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core.ingest("alpha beta gamma", "web", None).await.unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        crate::jobs::embed::run_corpus(&core, &out.id)
            .await
            .unwrap();

        let page = get_body(&app, &cookie, &format!("/ui/corpora/{}", out.id)).await;
        assert!(!page.contains("band-gap"), "nothing was missed: {page}");
    }

    #[tokio::test]
    async fn a_low_coverage_row_links_to_the_lines_that_were_missed() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        crate::jobs::embed::run_corpus(&core, &out.id)
            .await
            .unwrap();
        core.store
            .set_corpus_coverage(&out.id, Some(0.31))
            .await
            .unwrap();

        let frag = get_body(&app, &cookie, "/ui/queue").await;
        assert!(
            frag.contains(&format!("/ui/corpora/{}#uncovered", out.id)),
            "a warning has to lead somewhere: {frag}"
        );
        assert!(frag.contains("qcov-low"), "{frag}");
    }

    #[tokio::test]
    async fn a_low_row_with_no_windows_warns_without_linking() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        crate::jobs::embed::run_corpus(&core, &out.id)
            .await
            .unwrap();
        core.store.clear_segments(&out.id).await.unwrap();
        core.store
            .set_corpus_coverage(&out.id, Some(0.31))
            .await
            .unwrap();

        let frag = get_body(&app, &cookie, "/ui/queue").await;
        assert!(
            frag.contains("qcov-low"),
            "the reading is still worth warning about: {frag}"
        );
        assert!(
            !frag.contains(&format!("/ui/corpora/{}#uncovered", out.id)),
            "linked to a section that renders nothing: {frag}"
        );
    }

    #[tokio::test]
    async fn re_reading_one_passage_leaves_the_other_windows_alone() {
        let (app, cookie, core) = app_session_and_core().await;
        let body = (1..=400)
            .map(|i| format!("line {i} of the document"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let out = core.ingest(&body, "web", None).await.unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        core.store
            .set_corpus_status(&out.id, CorpusStatus::Ready)
            .await
            .unwrap();
        let segments = core.store.segments_for_corpus(&out.id).await.unwrap();
        assert!(segments.len() > 1, "the fixture must span several windows");
        let target = segments[0].clone();
        assert!(target.end_line > 3, "the loss below must fit inside it");

        // The endpoint cuts the bands again rather than trusting the form, so the loss must be real.
        let total = core
            .store
            .get_corpus(&out.id)
            .await
            .unwrap()
            .raw_text
            .lines()
            .count() as i64;
        sqlx::query!("DELETE FROM artifacts WHERE corpus_id = ?", &out.id)
            .execute(&core.store.pool)
            .await
            .unwrap();
        let claim = |ord: i64, a: i64, z: i64| crate::store::artifacts::NewArtifact {
            ordinal: ord,
            text: format!("what lines {a} to {z} said"),
            corpus_span: Some(crate::store::artifacts::CorpusSpan {
                start_line: a,
                end_line: z,
                source: crate::store::artifacts::SpanSource::Located,
            }),
            title: Some(format!("artifact {ord}")),
            ..Default::default()
        };
        core.store
            .insert_artifacts(&out.id, &[claim(0, 1, 1), claim(1, 4, total)])
            .await
            .unwrap();

        let res = app
            .clone()
            .oneshot(form(
                &format!("/ui/corpora/{}/reread", out.id),
                &cookie,
                &format!("from={}&to={}", target.start_line, target.end_line),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);

        let pending = core.store.pending_segments(&out.id).await.unwrap();
        assert_eq!(
            pending.iter().map(|w| w.idx).collect::<Vec<_>>(),
            vec![target.idx],
            "exactly the window holding that line, and no other"
        );
    }

    #[tokio::test]
    async fn re_reading_a_line_in_no_window_is_not_a_500() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core.ingest("alpha line", "web", None).await.unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;

        let res = app
            .clone()
            .oneshot(form(
                &format!("/ui/corpora/{}/reread", out.id),
                &cookie,
                "from=99999&to=99999",
            ))
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::SEE_OTHER,
            "nothing to do is not an error"
        );
    }

    #[tokio::test]
    async fn the_corpus_page_puts_each_passage_beside_what_came_of_it() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line\n\ncharlie line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;

        let page = get_body(&app, &cookie, &format!("/ui/corpora/{}", out.id)).await;
        assert!(page.contains("band"), "the page is banded: {page}");
        assert!(!page.contains("Raw corpus"), "{page}");
        assert!(!page.contains("<h3>Artifacts</h3>"), "{page}");
        assert!(page.contains(r#"id="L1""#), "{page}");
    }

    #[tokio::test]
    async fn an_unclaimed_passage_is_a_gap_band_with_its_own_button() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line\n\ncharlie line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        crate::jobs::embed::run_corpus(&core, &out.id)
            .await
            .unwrap();
        // Written straight to the column: synthesis is the only span writer.
        sqlx::query!(
            r#"UPDATE artifacts SET corpus_span = '{"start_line":1,"end_line":1}' WHERE corpus_id = ?"#,
            &out.id
        )
            .execute(&core.store.pool)
        .await
        .unwrap();

        let page = get_body(&app, &cookie, &format!("/ui/corpora/{}", out.id)).await;
        assert!(
            page.contains("band-gap"),
            "the unclaimed run is red: {page}"
        );
        assert!(
            page.contains(r#"name="from""#),
            "a gap band carries a re-read button naming its first line: {page}"
        );
        assert!(
            page.contains("reads lines"),
            "the button says what it will actually read, which is the whole \
             window and so wider than the band: {page}"
        );
    }

    #[tokio::test]
    async fn a_restored_corpus_is_not_banded() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core.ingest("alpha line", "web", None).await.unwrap();
        sqlx::query!("UPDATE corpora SET restored_at = 1 WHERE id = ?", &out.id)
            .execute(&core.store.pool)
            .await
            .unwrap();

        let page = get_body(&app, &cookie, &format!("/ui/corpora/{}", out.id)).await;
        assert!(page.contains("Placeholder source"), "{page}");
        assert!(!page.contains("band-gap"), "{page}");
    }

    #[tokio::test]
    async fn an_empty_capture_that_is_not_a_photo_names_no_vision_job() {
        let (app, cookie, core) = app_session_and_core().await;
        let s = core.store.insert_corpus("", "web", None).await.unwrap();

        let page = get_body(&app, &cookie, &format!("/ui/corpora/{}", s.id)).await;
        assert!(!page.contains("vision model"), "{page}");
        assert!(page.contains("has no text"), "{page}");
    }

    #[tokio::test]
    async fn a_restored_corpus_still_shows_its_artifacts() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core.ingest("alpha line", "web", None).await.unwrap();
        let restored = core
            .store
            .insert_artifacts(
                &out.id,
                &[crate::store::artifacts::NewArtifact {
                    text: "what the vector store still had".into(),
                    title: Some("recovered".into()),
                    ..Default::default()
                }],
            )
            .await
            .unwrap()[0]
            .id
            .clone();
        sqlx::query!("UPDATE corpora SET restored_at = 1 WHERE id = ?", &out.id)
            .execute(&core.store.pool)
            .await
            .unwrap();

        let page = get_body(&app, &cookie, &format!("/ui/corpora/{}", out.id)).await;
        assert!(
            page.contains(&format!(r#"id="artifact-{restored}""#)),
            "the placeholder's only content has no card on its own page: {page}"
        );
    }

    #[tokio::test]
    async fn an_artifact_naming_no_lines_still_has_its_card() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line\n\ncharlie line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        crate::jobs::embed::run_corpus(&core, &out.id)
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE artifacts SET corpus_span = NULL
              WHERE id = (SELECT id FROM artifacts WHERE corpus_id = ? LIMIT 1)",
            &out.id
        )
        .execute(&core.store.pool)
        .await
        .unwrap();
        let orphan = sqlx::query_scalar!(
            r#"SELECT id AS "id!" FROM artifacts WHERE corpus_id = ? AND corpus_span IS NULL"#,
            &out.id
        )
        .fetch_one(&core.store.pool)
        .await
        .unwrap();

        let page = get_body(&app, &cookie, &format!("/ui/corpora/{}", out.id)).await;
        assert!(
            page.contains("Not placed in the source"),
            "an artifact of this capture is on no page at all: {page}"
        );
        assert!(
            page.contains(&format!(r#"id="artifact-{orphan}""#)),
            "the card is what carries edit and delete: {page}"
        );
    }

    #[tokio::test]
    async fn nothing_is_a_loss_while_an_artifact_names_no_lines() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line\n\ncharlie line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        crate::jobs::embed::run_corpus(&core, &out.id)
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE artifacts SET corpus_span = NULL WHERE corpus_id = ?",
            &out.id
        )
        .execute(&core.store.pool)
        .await
        .unwrap();

        let page = get_body(&app, &cookie, &format!("/ui/corpora/{}", out.id)).await;
        assert!(!page.contains("band-gap"), "{page}");
        assert!(!page.contains(r#"name="from""#), "{page}");

        let res = app
            .clone()
            .oneshot(form(
                &format!("/ui/corpora/{}/reread", out.id),
                &cookie,
                "from=1&to=999999",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert!(
            core.store
                .pending_segments(&out.id)
                .await
                .unwrap()
                .is_empty(),
            "a capture nothing is known to have missed was queued to be re-read"
        );
    }

    #[tokio::test]
    async fn a_re_read_of_a_range_that_lost_nothing_queues_nothing() {
        let (app, cookie, core) = app_session_and_core().await;
        let body = (1..=400)
            .map(|i| format!("line {i} of the document"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let out = core.ingest(&body, "web", None).await.unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        crate::jobs::embed::run_corpus(&core, &out.id)
            .await
            .unwrap();
        core.store
            .set_corpus_status(&out.id, CorpusStatus::Ready)
            .await
            .unwrap();
        let windows = core.store.segments_for_corpus(&out.id).await.unwrap().len();
        assert!(windows > 1, "the fixture must span several windows");

        let res = app
            .clone()
            .oneshot(form(
                &format!("/ui/corpora/{}/reread", out.id),
                &cookie,
                "from=1&to=999999",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert!(
            core.store
                .pending_segments(&out.id)
                .await
                .unwrap()
                .is_empty(),
            "every window of a fully claimed capture was queued to be read again"
        );
    }

    #[tokio::test]
    async fn an_overlapping_artifact_has_exactly_one_card() {
        use crate::store::artifacts::{CorpusSpan, NewArtifact};
        let (app, cookie, core) = app_session_and_core().await;
        let out = core
            .ingest("one\ntwo\nthree\nfour\nfive\nsix", "web", None)
            .await
            .unwrap();
        let art = |ord: i64, a: i64, z: i64| NewArtifact {
            ordinal: ord,
            text: format!("what lines {a} to {z} said"),
            corpus_span: Some(CorpusSpan {
                start_line: a,
                end_line: z,
                source: crate::store::artifacts::SpanSource::Located,
            }),
            title: Some(format!("artifact {ord}")),
            ..Default::default()
        };
        let wide = core
            .store
            .insert_artifacts(&out.id, &[art(0, 1, 6), art(1, 3, 4)])
            .await
            .unwrap()[0]
            .id
            .clone();

        let page = get_body(&app, &cookie, &format!("/ui/corpora/{}", out.id)).await;
        assert_eq!(
            page.matches(&format!(r#"id="artifact-{wide}""#)).count(),
            1,
            "one artifact, one card, one set of element ids: {page}"
        );
        assert!(
            page.contains(&format!(r##"href="#artifact-{wide}""##)),
            "the later bands it claims still point at it: {page}"
        );
    }

    #[tokio::test]
    async fn the_page_states_the_coverage_the_recent_list_warned_about() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core.ingest("alpha line", "web", None).await.unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        core.store
            .set_corpus_coverage(&out.id, Some(0.55))
            .await
            .unwrap();

        let page = get_body(&app, &cookie, &format!("/ui/corpora/{}", out.id)).await;
        assert!(
            page.contains(r#"id="uncovered""#),
            "the anchor still lands: {page}"
        );
        assert!(page.contains("55%"), "{page}");
    }

    #[tokio::test]
    async fn a_promoted_window_is_listed_with_an_undo_that_works() {
        let (app, cookie, core) = app_session_and_core().await;
        let src = core
            .store
            .insert_corpus("l1\nl2", "web", None)
            .await
            .unwrap();
        core.store
            .upsert_segments(
                &src.id,
                &[crate::store::segments::NewSegment {
                    start_line: 1,
                    end_line: 2,
                    text: "l1\nl2",
                }],
            )
            .await
            .unwrap();
        let na = |o: i64, t: &str| crate::store::artifacts::NewArtifact {
            ordinal: o,
            text: t.into(),
            corpus_span: Some(crate::store::artifacts::CorpusSpan {
                start_line: 1,
                end_line: 2,
                source: crate::store::artifacts::SpanSource::Located,
            }),
            segment_idx: Some(0),
            ..Default::default()
        };
        let p = core
            .store
            .insert_artifacts_with_provenance(
                &src.id,
                &[na(0, "passage")],
                crate::store::artifacts::Provenance::Passage,
            )
            .await
            .unwrap();
        let a = core
            .store
            .insert_artifacts(&src.id, &[na(1, "artifact")])
            .await
            .unwrap();
        core.supersede(&p[0].id, &a[0].id).await.unwrap();
        core.store
            .set_segment_state(&src.id, 0, crate::store::segments::SegmentState::Done, None)
            .await
            .unwrap();
        core.store
            .set_corpus_status(&src.id, CorpusStatus::Ready)
            .await
            .unwrap();

        let page = get_body(&app, &cookie, &format!("/ui/corpora/{}", src.id)).await;
        let action = format!("/ui/corpora/{}/segments/0/unpromote", src.id);
        assert!(page.contains(&action), "{page}");

        let res = app
            .clone()
            .oneshot(form(&action, &cookie, ""))
            .await
            .unwrap();
        assert!(res.status().is_redirection(), "{:?}", res.status());
        assert!(
            core.store
                .get_artifact(&p[0].id)
                .await
                .unwrap()
                .in_results()
        );
        let page = get_body(&app, &cookie, &format!("/ui/corpora/{}", src.id)).await;
        assert!(!page.contains(&action), "undo still offered after undoing");
    }

    #[tokio::test]
    async fn a_merge_is_listed_under_each_corpus_it_drew_from() {
        let (app, cookie, core) = app_session_and_core().await;
        let c1 = core.store.insert_corpus("one", "web", None).await.unwrap();
        let c2 = core.store.insert_corpus("two", "web", None).await.unwrap();
        let na = |t: &str| crate::store::artifacts::NewArtifact {
            text: t.into(),
            corpus_span: Some(crate::store::artifacts::CorpusSpan {
                start_line: 1,
                end_line: 1,
                source: crate::store::artifacts::SpanSource::Located,
            }),
            segment_idx: Some(0),
            ..Default::default()
        };
        let r1 = core
            .store
            .insert_artifacts(&c1.id, &[na("root one")])
            .await
            .unwrap()[0]
            .id
            .clone();
        let r2 = core
            .store
            .insert_artifacts(&c2.id, &[na("root two")])
            .await
            .unwrap()[0]
            .id
            .clone();
        let m = core
            .store
            .insert_merged_artifact(
                &crate::store::artifacts::NewMerged {
                    text: "the merge of one and two".into(),
                    title: Some("Merged title".into()),
                    ..Default::default()
                },
                &[r1, r2],
            )
            .await
            .unwrap();
        for c in [&c1, &c2] {
            core.store
                .set_corpus_status(&c.id, CorpusStatus::Ready)
                .await
                .unwrap();
            let page = get_body(&app, &cookie, &format!("/ui/corpora/{}", c.id)).await;
            assert!(page.contains("Written from this source"), "{page}");
            assert!(page.contains(&m.id), "{page}");
        }
    }
}
