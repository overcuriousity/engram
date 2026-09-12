//! One captured document, read: the page a person opens to see what engram
//! made of what they pasted.
//!
//! Split out of `web::ui` for the reason `web::settings` was. The pane that
//! shows the source lines is `web::corpus_view`; this is the page around it —
//! the passages, what each one became, the coverage the reader is owed, and
//! the four buttons that act on the document: re-read a range, re-process the
//! whole of it, take a promotion back, delete it.

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
    /// This row is a placeholder for a source that was never captured here, so
    /// `raw_text` is its restored artifacts joined rather than a document. The
    /// page has to say so: it otherwise presents reconstructed fragments under
    /// the same "Raw corpus" heading as a real capture, and offers to
    /// re-segment them.
    restored: bool,
    /// The page this was captured from, for the doors that know one. The last
    /// hop back to where the text came from, which is otherwise unrecoverable
    /// once the tab is closed.
    source_url: Option<String>,
    /// An image corpus: the page shows the photo, and the lines below are the
    /// model's reading of it rather than the source itself.
    image: bool,
    /// A PDF corpus: the lines below are docling's extraction of it rather
    /// than the document as it was laid out, and the original is one click
    /// away.
    pdf: bool,
    /// A capture whose reading has not landed — still `describing` or
    /// `extracting`, or parked before any text was read. Nothing to
    /// re-segment; only read it again.
    unread: bool,
    /// Rows of what the door recorded about the capture, already formatted.
    meta_rows: Vec<(String, String)>,
    /// Every other EXIF tag the file carried, by name. Folded away on the page:
    /// a phone emits dozens, and none of them is what someone came here to read
    /// — but the original is not stored, so this is the only place they exist.
    exif_rows: Vec<(String, String)>,
    note: Option<String>,
    /// The source cut where the artifacts claiming it change, each stretch
    /// beside what came of it. Empty for a source there is nothing to band —
    /// a restored placeholder, or a photo not read yet — which falls back to
    /// the flat rendering.
    bands: Vec<BandView>,
    /// Windows synthesis has read because their passages were read — each with
    /// an undo, which puts the verbatim text back in results.
    promoted: Vec<PromotedWindow>,
    /// The artifacts of this capture that name no lines of it, in a section of
    /// their own below the source. Every artifact of a restored placeholder is
    /// here, as is anything written before spans were recorded — and the page
    /// showed none of them once it rendered bands alone.
    unplaced: Vec<ArtifactView>,
    /// Merged and synthesized artifacts with a root in this capture. A merge
    /// belongs to every corpus it drew from, and this is where that shows.
    written_from: Vec<ArtifactView>,
    /// Nothing was captured here at all, so the flat fallback has nothing to
    /// show either.
    lines_empty: bool,
    /// The source as one block, for the fallback. Unnumbered on purpose: the
    /// only thing that reaches it is a restored placeholder, where a line
    /// number is a claim about a document that was never captured here.
    raw_text: String,
    /// How much of the wording survived, as the Recent list measures it.
    /// Stated whether or not a band is red, because the two measures answer
    /// different questions and can disagree.
    coverage: Option<String>,
    /// What this capture is called — the same label Recent, the idle foot and
    /// the day page use, so one source has one name wherever it is named.
    ///
    /// The page had none. It opened on a status chip and two buttons, and the
    /// only thing that said which capture you were looking at was the browser
    /// tab, which said "Source" for every one of them.
    title: String,
}

impl CorpusTemplate {
    /// Which entry in the top row and the tab bar is the one you are inside.
    ///
    /// Read by `layout.html` to set `aria-current="page"`. The empty string is
    /// "none of them", which is a real answer for a page that hangs off no
    /// section.
    ///
    /// A source belongs to no section: it is reached from Recent, from the idle
    /// foot and from a result. Nothing in the row is current, which is the
    /// honest answer rather than lighting one at random.
    fn section(&self) -> &'static str {
        ""
    }
}

/// Which lines to highlight, when the page was opened from an artifact that
/// claims them. Absent for an ordinary visit, which highlights nothing.
#[derive(serde::Deserialize, Default)]
struct LineRange {
    from: Option<i64>,
    to: Option<i64>,
}

/// Whether a capture's coverage is final — whether what no artifact carried is
/// a loss rather than a window nobody has read yet.
///
/// `synthesize::plan` writes every window up front in state `pending`, so a
/// capture still being read has segment rows and no artifacts for most of them.
/// Measured then, every unread line looks uncovered, and the page said so: it
/// named lines that were about to arrive as never reached, and offered to pay
/// for reading them a second time.
///
/// These are the states synthesis sets once every window has resolved. `partial`
/// and `failed` are in the list on purpose — they are where a real loss lives,
/// and gating on `ready` alone would hide the section from exactly the captures
/// that have something to show it.
fn coverage_final(status: &CorpusStatus) -> bool {
    matches!(
        status,
        CorpusStatus::Ready | CorpusStatus::Partial | CorpusStatus::Failed
    )
}

#[derive(serde::Deserialize)]
struct RereadForm {
    /// The band the button sits in. Both ends, because a passage nothing was
    /// written from does not stop at a window boundary, and matching on the
    /// first line alone re-read the window the loss opened in and left the
    /// rest of it exactly as it was.
    from: i64,
    to: i64,
}

/// Read one passage again.
///
/// The window holding that line, not the line itself: a window is wider than
/// the passage, and that is what lets the model read it in its surroundings
/// rather than stripped of them. One model call.
///
/// Nothing already written from this capture is replaced. What comes back is
/// added, and anything it repeats is folded by the dedupe sweep like any other
/// near duplicate.
///
/// The range is what the band said, not what it is: the form carries no token,
/// and taking `from`/`to` at their word let one POST of `from=1&to=999999` —
/// hand-edited, replayed, or arriving from another page in the operator's
/// session — reset and re-enqueue every window of the capture, one paid model
/// call each. So the bands are cut again here, and only a window holding a
/// passage that really is a loss, and really is inside the band pressed, is
/// queued.
async fn reread_uncovered_ui(
    tenant: Tenant,
    Path(cid): Path<String>,
    Form(f): Form<RereadForm>,
) -> UiResult<Response> {
    // Back to the band the button was in. On a nine-hundred-line document,
    // returning to the top after pressing something two thirds of the way down
    // loses the reader's place for no reason.
    let back = Redirect::to(&format!("/ui/corpora/{cid}#L{}", f.from)).into_response();

    // A page left open while the capture was still being read would otherwise
    // offer to re-read lines that are merely not written yet.
    let s = tenant.core.store.get_corpus(&cid).await?;
    if !coverage_final(&s.status) {
        return Ok(back);
    }

    // The same cut the page renders, from the same inputs — and the same two
    // reasons it renders nothing red: a restored placeholder's text is its own
    // artifacts, and an artifact naming no lines may have come from exactly the
    // lines about to be re-read.
    let chunks = tenant.core.store.artifacts_for_corpus(&cid).await?;
    if s.restored_at.is_some() || chunks.iter().any(|c| c.corpus_span.is_none()) {
        return Ok(back);
    }
    let spans: Vec<(String, crate::store::artifacts::CorpusSpan)> = chunks
        .iter()
        .filter_map(|c| c.corpus_span.clone().map(|sp| (c.id.clone(), sp)))
        .collect();
    let lost: Vec<(i64, i64)> = crate::web::corpus_view::bands(&s.raw_text, &spans, None)
        .into_iter()
        .filter(|b| b.gap() && b.from <= f.to && f.from <= b.to)
        .map(|b| (b.from, b.to))
        .collect();
    if lost.is_empty() {
        return Ok(back);
    }

    let segments = tenant.core.store.segments_for_corpus(&cid).await?;
    for w in segments.iter().filter(|w| {
        lost.iter()
            .any(|(a, z)| w.start_line <= *z && *a <= w.end_line)
    }) {
        // A window something is already going to read is left alone. `enqueue`
        // re-arms a conflicting row whatever state it is in, running included,
        // so pressing this twice handed the same window to a second worker: two
        // paid model calls and two sets of artifacts for one passage, then the
        // dedupe sweep to clean up after them.
        if tenant
            .core
            .store
            .live_job(
                crate::store::jobs::Stage::SegmentWindow,
                &crate::jobs::window::unit_target(&cid, w.idx),
            )
            .await?
        {
            continue;
        }
        // `true`: this window was read correctly and missed lines, so it is
        // being added to rather than replaced. Deleting what it already wrote
        // would throw away artifacts that may have been edited, tagged or
        // verified since, for lines that were never the problem.
        tenant.core.store.reset_segment(&cid, w.idx, true).await?;
        tenant
            .core
            .store
            .enqueue(
                crate::store::jobs::Stage::SegmentWindow,
                "segment",
                &crate::jobs::window::unit_target(&cid, w.idx),
            )
            .await?;
    }
    Ok(back)
}

/// Undo a promotion: the window's passages back in results, what the
/// promotion wrote retired, the window `verbatim` again.
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
    let chunks = tenant.core.store.artifacts_for_corpus(&cid).await?;
    let restored = s.restored_at.is_some();

    // A restored placeholder's text is its own artifacts joined back together,
    // so a span into it points at an artifact rather than at a source. Banding
    // it would be a claim that arrangement cannot support; it keeps the flat
    // rendering, and the warning above it already says why.
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

    // An artifact that names no lines was written from somewhere in this
    // capture without saying where: a row from before spans were recorded,
    // anything created outside `window::run`, and every artifact of a restored
    // placeholder. Banding cannot place it, and rendering only bands dropped it
    // off the page altogether — off the only page that can edit or delete it.
    // It gets a section of its own below the source instead.
    let unplaced: Vec<ArtifactView> = chunks
        .iter()
        .filter(|c| restored || c.corpus_span.is_none())
        .map(artifact_view)
        .collect();

    let segments = tenant.core.store.segments_for_corpus(&cid).await?;
    // Until the capture has finished being read, a passage nothing claims is
    // a passage nothing has got to yet. Banded, still — the arrangement is how
    // the page reads — but not red, and not offering to re-read what is
    // already on its way.
    //
    // And nothing is a loss while an artifact of this capture names no lines:
    // it may well have been written from exactly the lines about to be painted
    // red, and the page would be offering to pay to read them again on the
    // strength of a claim it cannot make. `unplaced` says so in words instead.
    let losses_are_final = coverage_final(&s.status) && !segments.is_empty() && unplaced.is_empty();

    // Every row still carries its `L<n>` anchor, inside its band: an artifact's
    // "open at these lines" and the `?from=&to=` highlight both address lines
    // by that id, and banding must not cost the page either of them.
    //
    // An artifact whose span overlaps another's claims every band the overlap
    // cuts, and its card belongs to the first of them. Rendered in each, the
    // page carried the same artifact three times under one set of element ids:
    // "edit" on the second copy opened the editor attached to the first, and
    // delete swapped the first away and left the others behind pointing at a
    // row that no longer exists.
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
            // Split before the band is built: an artifact gets its card in the
            // first band that claims it, and a line pointing up at that card in
            // every later one.
            let (mut artifacts, mut echoes) = (Vec::new(), Vec::new());
            for c in b
                .artifact_ids
                .iter()
                .filter_map(|id| by_id.get(id.as_str()))
            {
                if carded.insert(c.id.clone()) {
                    artifacts.push(artifact_view(c));
                } else {
                    // A link whose whole content is the label, so it falls to
                    // the opening of the text where the artifact has no name
                    // of its own — see `ui::RowLabel`.
                    let label = crate::web::ui::row_label(c);
                    echoes.push(BandEcho {
                        id: c.id.clone(),
                        label: label.text,
                        named: label.named,
                    });
                }
            }
            BandView {
                // What pressing the button would actually read: the window holding
                // this passage, which is wider than it. Saying only "lines 51–53"
                // over a button that reads 1–120 is a promise it does not keep —
                // and a second red band inside the same window really is read too.
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
                // The overlap is still on the page: both artifacts do claim these
                // lines, and a band that silently dropped one of them would read as
                // if only the other did.
                echoes,
                lines: b.lines,
            }
        })
        .collect()
    };

    // Stated whether or not anything is red, because the warning on Recent is
    // computed the other way round: a corpus can be 55% covered with every
    // line claimed, and following that warning has to land somewhere that
    // explains itself rather than on a page with nothing marked.
    let coverage = s.coverage.map(|c| format!("{:.0}%", c * 100.0));
    // A verbatim capture's passages *are* the wording, so the measure is
    // 100% by construction and says nothing. Stated only once something was
    // written from the source.
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
    // A promoted window: `done`, and owning at least one superseded passage.
    let promoted: Vec<PromotedWindow> = segments
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
        .collect();
    Ok(HtmlTemplate(CorpusTemplate {
        id: s.id,
        badge: status_badge(&s.status),
        status: s.status.as_str().to_string(),
        restored,
        source_url: s.source_url.clone(),
        image,
        pdf,
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
    })
    .into_response())
}

/// Everything under `exif.tags`, by name, sorted. The named facts above have
/// their own rows; this is the rest of what the camera wrote, in a block that
/// starts folded — the original file is not kept, so the page is the only place
/// left to read it, and it is still nothing anyone opened the page to see.
fn exif_tag_rows(m: &serde_json::Value) -> Vec<(String, String)> {
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

/// The metadata worth a row on the corpus page, in reading order. Everything
/// else the file carried is under `exif.tags`, folded away below.
fn metadata_rows(m: &serde_json::Value) -> Vec<(String, String)> {
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
    rows
}

async fn delete_corpus_ui(tenant: Tenant, Path(cid): Path<String>) -> UiResult<Response> {
    tenant.core.delete_corpus(&cid).await?;
    Ok(Redirect::to("/ui/capture").into_response())
}

#[derive(serde::Deserialize, Default)]
struct ReprocessForm {
    #[serde(default)]
    stage: Option<String>,
}

/// Re-segment by default; `stage=describe` re-reads a captured image and
/// `stage=extract` re-reads a captured PDF.
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

/// A window a promotion has synthesized, for the corpus page's undo list.
pub struct PromotedWindow {
    pub idx: i64,
    pub from: i64,
    pub to: i64,
}

/// One stretch of the source on the corpus page, beside what came of it.
/// A pointer up at an artifact carded in an earlier band. A link and nothing
/// else, so its label can never be empty — see `ui::RowLabel`.
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
    /// `(id, title)` for the artifacts claiming this band whose card is in an
    /// earlier one — the overlaps. A line pointing up at the card, because the
    /// card itself can only exist once: two copies of it share their element
    /// ids, and edit and delete then reach the wrong one.
    pub echoes: Vec<BandEcho>,
    /// Nothing was written from these lines.
    pub gap: bool,
    /// For a gap band, the lines a re-read would actually cover: the whole
    /// window holding this passage, which is wider than the passage. `None`
    /// when no window holds it and there is nothing to offer.
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

    /// A band echo is a link and nothing else — "↑ ⟨label⟩", pointing up at
    /// the card in the band that owns it. A passage there was pointed at by
    /// the heading of the section it was cut from.
    #[tokio::test]
    async fn a_band_echo_for_a_passage_points_at_it_by_how_its_text_opens() {
        let core = crate::core::test_support::test_core().await;
        let raw = "eins\nzwei\ndrei\nvier";
        let src = core.store.insert_corpus(raw, "web", None).await.unwrap();
        let span = |a: i64, b: i64| Some(crate::store::artifacts::CorpusSpan::located(a, b));
        // Overlapping spans: line 1 is the first alone, lines 2-3 are both, and
        // line 4 the second alone. The first is carded in the opening band and
        // echoed in the one it overlaps into, which is the row under test.
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
        // A captured artifact renders the corpus lines its span claims. A merged
        // one has neither corpus nor span, so the pane shows what it was written
        // from — each source still stored, each still naming its own document.
        // Rendering a corpus it did not come from would put the wrong lines
        // beside it forever, which is the one dishonesty merging must not
        // commit.
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
        // And each source still points at the document it was captured from.
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
        // Everything else the camera wrote is on the page too, folded away and
        // in tag order: this preview is the only copy of it that survives.
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
        // An ordinary capture answers with nothing to read the id out of — the
        // queue fragment is what names it on the page.
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

        // And the form behind that button, reached directly, arms nothing.
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

    /// `enqueue` re-arms a conflicting row whatever state it is in, running
    /// included. Pressing the button twice therefore handed one window to two
    /// workers: two paid model calls and two sets of artifacts for one loss.
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
        // The first window is already on its way — an earlier press of the same
        // button, or the read that is about to fill it.
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
        // Uncovered lines are merged into one range across everything lost in
        // a row, and nothing stops that run at a window boundary. Matching the
        // range's first line alone re-read the window the loss opened in and
        // left the rest of it exactly as it was.
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
        // Both settled and neither producing an artifact: the whole document
        // is one uncovered range spanning both windows.
        for idx in [0, 1] {
            core.store
                .set_segment_state(&out.id, idx, SegmentState::Done, None)
                .await
                .unwrap();
        }
        // And the capture itself has finished being read — `partial` is what
        // synthesis sets for a document whose windows resolved without
        // covering it, and `coverage_final` requires it before naming a loss.
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
        // The anchor still exists — the Recent warning follows it, and it has
        // to land on the sentence that explains why nothing is marked. What a
        // fully claimed corpus has is no red band.
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
        // After embedding, which is what settles the corpus and recomputes the
        // real coverage — this is the reading the row has to warn about.
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
        // A capture read before per-segment windows existed. Its coverage is
        // still measured — against the whole document — but nothing can say
        // which lines were lost, so `#uncovered` renders nothing and the
        // warning must not send anyone there.
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
        // Long enough to be several windows, so "one of them" is meaningful.
        let body = (1..=400)
            .map(|i| format!("line {i} of the document"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let out = core.ingest(&body, "web", None).await.unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        // Settled, or the endpoint rightly refuses: lines a capture has not
        // been read to the end of are not lines it lost. Set directly because
        // this test is about where a re-read is aimed, not about the pipeline
        // that gets a document to Ready.
        core.store
            .set_corpus_status(&out.id, CorpusStatus::Ready)
            .await
            .unwrap();
        let segments = core.store.segments_for_corpus(&out.id).await.unwrap();
        assert!(segments.len() > 1, "the fixture must span several windows");
        let target = segments[0].clone();
        assert!(target.end_line > 3, "the loss below must fit inside it");

        // One loss, lines 2–3, and it has to be a real one: the endpoint cuts
        // the bands again rather than believing the range in the form, so a
        // fixture where nothing was missed queues nothing however it is aimed.
        // Two artifacts around the gap, written directly, because what this
        // test is about is where the button points.
        let total = core
            .store
            .get_corpus(&out.id)
            .await
            .unwrap()
            .raw_text
            .lines()
            .count() as i64;
        sqlx::query("DELETE FROM artifacts WHERE corpus_id = ?")
            .bind(&out.id)
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
        // The old two-lists arrangement is gone.
        assert!(!page.contains("Raw corpus"), "{page}");
        assert!(!page.contains("<h3>Artifacts</h3>"), "{page}");
        // Every line keeps the anchor an artifact's "open at these lines" uses.
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
        // Settled, or nothing is a loss yet: a capture still being read has
        // lines nothing claims because nothing has got to them.
        crate::jobs::embed::run_corpus(&core, &out.id)
            .await
            .unwrap();
        // Pull every span back onto line 1, leaving the rest of the document
        // claimed by nobody. Written straight to the column because nothing in
        // the store edits a span — synthesis computes it and is the only
        // writer, which is right everywhere except here.
        sqlx::query(
            r#"UPDATE artifacts SET corpus_span = '{"start_line":1,"end_line":1}' WHERE corpus_id = ?"#,
        )
        .bind(&out.id)
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
        // Its "source" is its own artifacts joined back together, so a span
        // into it is a claim the arrangement cannot support.
        let (app, cookie, core) = app_session_and_core().await;
        let out = core.ingest("alpha line", "web", None).await.unwrap();
        sqlx::query("UPDATE corpora SET restored_at = 1 WHERE id = ?")
            .bind(&out.id)
            .execute(&core.store.pool)
            .await
            .unwrap();

        let page = get_body(&app, &cookie, &format!("/ui/corpora/{}", out.id)).await;
        assert!(page.contains("Placeholder source"), "{page}");
        assert!(!page.contains("band-gap"), "{page}");
    }

    /// Only a photo waits on the vision model. Any other capture with no text
    /// is a fetch that came back empty or a paste that was, and the fallback
    /// told the operator a job was queued that nobody had started.
    #[tokio::test]
    async fn an_empty_capture_that_is_not_a_photo_names_no_vision_job() {
        let (app, cookie, core) = app_session_and_core().await;
        let s = core.store.insert_corpus("", "web", None).await.unwrap();

        let page = get_body(&app, &cookie, &format!("/ui/corpora/{}", s.id)).await;
        assert!(!page.contains("vision model"), "{page}");
        assert!(page.contains("has no text"), "{page}");
    }

    /// Not banded is not the same as not shown. A placeholder's artifacts are
    /// the only thing it holds, and banding alone left them off their own page.
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
        sqlx::query("UPDATE corpora SET restored_at = 1 WHERE id = ?")
            .bind(&out.id)
            .execute(&core.store.pool)
            .await
            .unwrap();

        let page = get_body(&app, &cookie, &format!("/ui/corpora/{}", out.id)).await;
        assert!(
            page.contains(&format!(r#"id="artifact-{restored}""#)),
            "the placeholder's only content has no card on its own page: {page}"
        );
    }

    /// Rendering bands alone dropped it: banding places an artifact by its
    /// span, and an artifact without one was placed nowhere and shown nowhere
    /// — off the only page that can edit or delete it.
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
        // One artifact from before spans were recorded. Written straight to the
        // column for the same reason `an_unclaimed_passage_...` does: synthesis
        // is the only writer of a span, which is right everywhere except here.
        sqlx::query(
            "UPDATE artifacts SET corpus_span = NULL
              WHERE id = (SELECT id FROM artifacts WHERE corpus_id = ? LIMIT 1)",
        )
        .bind(&out.id)
        .execute(&core.store.pool)
        .await
        .unwrap();
        let orphan = sqlx::query_scalar::<_, String>(
            "SELECT id FROM artifacts WHERE corpus_id = ? AND corpus_span IS NULL",
        )
        .bind(&out.id)
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

    /// It may well have been written from exactly the lines about to be painted
    /// red, and the page would be offering a paid re-read on the strength of a
    /// claim it cannot make.
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
        // Every span gone: under the old rule the whole document is one red
        // band with a button on it, though every artifact of it still exists.
        sqlx::query("UPDATE artifacts SET corpus_span = NULL WHERE corpus_id = ?")
            .bind(&out.id)
            .execute(&core.store.pool)
            .await
            .unwrap();

        let page = get_body(&app, &cookie, &format!("/ui/corpora/{}", out.id)).await;
        assert!(!page.contains("band-gap"), "{page}");
        assert!(!page.contains(r#"name="from""#), "{page}");

        // And the endpoint behind the button agrees, whatever range reaches it.
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

    /// The form carries no token and the range is a claim, not a fact. Taking
    /// it at its word, one POST reset and re-enqueued every window of the
    /// capture — a paid model call each, for lines nothing was missing from.
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

    /// Rendered in each band it touches, one artifact appeared three times
    /// under one set of element ids: "edit" on the second copy opened the
    /// editor of the first, and delete swapped the first away and left the
    /// others pointing at a row that no longer exists.
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
        // Wide, and one inside it: three bands, and the wide one claims all
        // three.
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
        // The two measures answer different questions and can disagree: every
        // line claimed, and still only half the wording carried. Following the
        // warning must not land on a page with nothing to see.
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
