//! One artifact, read in full: the detail pane and everything done to it
//! from there.
//!
//! The third screen out of `web::ui`. What an artifact says, where it came
//! from, what it was written from and what keeps being retrieved beside it —
//! and the writes a reader makes while looking at it: an edit, a deletion, a
//! dismissed neighbour, a dwell, a corpus marked reviewed.
//!
//! `link_citations` is here rather than with the markdown renderer because
//! what it links are *this pane's* excerpt numbers; the search rail imports
//! it for the same reason it imports the rest of the pane's vocabulary.

use crate::error::{Error, Result};
use crate::fmt::{ago, ago_or_ahead};
use crate::tenants::Tenant;
use crate::web::auth_routes::HtmlTemplate;
use crate::web::markdown;
use crate::web::state::AppState;
use crate::web::ui::{
    ArtifactView, artifact_html, artifact_title, artifact_view, ends_mid_sentence, title_of,
};
use crate::web::ui_error::UiResult;
use askama::Template;
use axum::Router;
use axum::extract::{Form, Path, Query};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};

/// Neighbours shown beside an artifact. A short list, because this is a way
/// out of the pane rather than a second result rail.
const RELATED_LIMIT: usize = 5;

/// A chunk beside the source lines it claims.
pub struct ArtifactDetail {
    pub id: String,
    /// The name a writer gave this text, or empty where nobody did — a
    /// passage and a note have none. The pane writes no heading element for an
    /// empty one; see `title_of`.
    pub title: String,
    /// What a list of names calls this artifact: `title` where there is one,
    /// and otherwise the opening of the text. Only the browser tab and the
    /// history entry use it — they are a list of names and cannot show a card.
    pub tab: String,
    /// Sanitized by `markdown::render`. Rendered with `|safe`.
    pub html: String,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub flags: Vec<String>,
    pub flag_detail: Option<String>,
    /// The artifact this one was hidden in favour of. Opening a hidden artifact
    /// by link has to say why it is not in results, or it reads as a bug.
    pub superseded_by: Option<String>,
    pub status: crate::store::artifacts::ArtifactStatus,
    pub last_verified_at: Option<i64>,
    /// Conditions the source stated under which this artifact does not apply.
    pub caveats: Vec<String>,
    /// `None` for a merged artifact, which belongs to no corpus. The pane shows
    /// what it was made of instead of corpus lines — see `build_artifact_detail`.
    pub corpus_id: Option<String>,
    /// The artifact's own text, for the edit box. `html` is what is read;
    /// this is what is edited, and rendering one back into the other is not
    /// something markdown round-trips.
    pub text: String,
    /// How this artifact came to exist: what it was written from, generation
    /// by generation, and what it replaced. Empty for a captured artifact that
    /// has replaced nothing — which is most of them, and which is why the
    /// template asks `is_empty` rather than `merged` before rendering it.
    pub lineage: crate::web::lineage_view::Lineage,
    /// Which of the two panes to render. A merged artifact belongs to no corpus
    /// and has no span, so the source pane has no document to link and no lines
    /// to list; it shows what the artifact was written from instead.
    ///
    /// The template used to branch on `sources` being empty, which is the same
    /// question only while a merge still has its sources. One that had lost them
    /// all fell through to the captured branch and rendered a "Source · …
    /// highlighted" label over an empty link and an empty line table — on
    /// exactly the artifact whose orphan notice matters most.
    pub merged: bool,
    /// Written from a pursuit: shows the questions it was written for.
    pub synthesized: bool,
    /// Those questions.
    pub cues: Vec<String>,
    /// True when one of those sources has since been deleted. The text still
    /// carries what it said, so this is a missing link rather than missing
    /// knowledge — and saying so beats listing one source fewer in silence.
    pub orphaned_source: bool,
    /// True when this artifact's source was never captured here — the artifact
    /// was restored from the vector store and its corpus row is a placeholder.
    /// The pane shows the source beside the artifact, so it has to say when what
    /// it is showing is the artifact's own text reflected back rather than the
    /// document it was drawn from.
    pub corpus_restored: bool,
    /// The search this was opened from, when it was opened from the rail and
    /// searches are being recorded. What the bar under the artifact and the
    /// dwell timer both report against. See `Store::open_event`.
    pub search_event: Option<String>,
    /// Link to the source, scrolled to and highlighting the exact lines this
    /// artifact was drawn from. Falls back to the plain source page for an
    /// artifact with no recorded span — a restored one, for instance.
    pub source_at_lines: String,
    /// The next passage of the same document, when this one stops in the
    /// middle of a sentence. A segmentation boundary landing mid-clause is not
    /// a thing the pane can prevent, but leaving the reader at "…Einsatz von"
    /// with the rest of the sentence visible in the column beside it and no
    /// way onward is.
    pub continues_at: Option<String>,
    pub segment_idx: Option<i64>,
    pub slice_label: String,
    pub slice_lines: Vec<crate::web::corpus_view::CorpusLine>,
    /// Query terms to highlight, space separated. Empty when the pane was
    /// opened outside a search.
    pub terms: String,
    /// The nearest artifacts to this one. Free in the sense that matters: the
    /// vector is already stored, so this costs no embedding call and no
    /// completion. Empty while the artifact is still waiting to be embedded.
    pub related: Vec<RelatedArtifact>,
    /// What this artifact has been needed alongside, learned from co-retrieval
    /// rather than resemblance. Beside `related`, not instead of it: one list
    /// is what this resembles, the other is what it has been reached for
    /// together with, and they answer different questions.
    pub seen_together: Vec<SeenTogether>,
    /// What the base found when this arrived, as a sentence. `None` before
    /// the artifact has been integrated.
    pub tag: Option<String>,
    /// The probes for this artifact, one line each: class, the question
    /// shortened, and where it last found this.
    pub probes: Vec<String>,
    /// Earlier versions a condensation retired: `(n, when, action id)`,
    /// oldest first. The action id is the undo button's target where the
    /// condensation is still open.
    pub versions: Vec<(i64, String, String)>,
    /// The open condensation's action id, if the live text is a condensed
    /// one: the button that puts the last version back.
    pub condensed: Option<String>,
    /// "in 2 h", "3 d ago", … when this artifact carries an open reminder —
    /// regardless of `time.horizon_hours`, unlike the same badge on a result
    /// row: opening the artifact itself is the one place a reminder set for
    /// next week still deserves to be seen immediately, not only once it
    /// enters the band a list shows.
    pub due_in: Option<String>,
}

/// A neighbour, as one line in the pane.
pub struct RelatedArtifact {
    pub id: String,
    pub title: String,
    pub snippet: String,
}

/// A link, as one line in the pane. Beside the nearest neighbours, not instead
/// of them: one list is what this artifact resembles, the other is what it has
/// been needed alongside, and they answer different questions.
pub struct SeenTogether {
    pub id: String,
    pub title: String,
    pub snippet: String,
    /// The judge's line, or the question that bound the pair. `None` only for a
    /// link with neither, which is a link nothing can explain yet.
    pub why: Option<String>,
    pub corpus_title: String,
    /// Rendered emphasised: two documents needing each other is the finding.
    /// Two passages of one document needing each other is not.
    pub cross_corpus: bool,
}

#[derive(Template)]
#[template(path = "_artifact.html")]
struct ArtifactFragment {
    c: ArtifactView,
}

#[derive(Template)]
#[template(path = "_artifact_detail.html")]
pub(crate) struct ArtifactDetailFragment {
    pub(crate) d: ArtifactDetail,
}

#[derive(Template)]
#[template(path = "artifact_detail.html")]
struct ArtifactDetailPage {
    d: ArtifactDetail,
}

impl ArtifactDetailPage {
    /// Which entry in the top row and the tab bar is the one you are inside.
    ///
    /// Read by `layout.html` to set `aria-current="page"`. The empty string is
    /// "none of them", which is a real answer for a page that hangs off no
    /// section.
    ///
    /// One result, opened out of the rail — still the search half of the app.
    fn section(&self) -> &'static str {
        "search"
    }
}

#[derive(serde::Deserialize)]
struct ArtifactEditForm {
    text: String,
    /// Which shape to answer with. Two screens edit an artifact and they are
    /// not the same size: the corpus page swaps one card in a list, the detail
    /// pane swaps the whole pane. Answering both with a card replaced the pane
    /// — source, lineage and neighbours included — with a list row.
    #[serde(default)]
    view: String,
    /// The search terms the pane was opened with, so the highlight survives a
    /// save. Empty everywhere else.
    #[serde(default)]
    terms: String,
}

#[derive(serde::Deserialize)]
pub(crate) struct ArtifactViewParams {
    #[serde(default)]
    pub(crate) terms: String,
    /// The artifact this one was reached from — a neighbour, an association,
    /// a continuation — when the link came from another artifact's page.
    #[serde(default)]
    pub(crate) via: Option<String>,
    /// The cluster slot this was offered under, when the offer rested on a
    /// learned cluster. Absent on the floor of the ladder, which has none —
    /// so this says which cluster, never whether the link came from an offer.
    #[serde(default)]
    pub(crate) rec: Option<i64>,
    /// The rung it was offered on, and the thing that marks the link as an
    /// offer's at all. Carried on the link because the offer was computed on a
    /// previous request and nothing server-side still holds it — without it,
    /// every click lands in one bucket on Ops.
    #[serde(default)]
    pub(crate) rung: Option<String>,
    /// The search this row was listed by, carried on the link the rail drew.
    /// Present only on a rail row, so it says both which search this open
    /// answers and that it came from a list of answers at all.
    /// See `SearchOutcome::event`.
    #[serde(default)]
    pub(crate) event: Option<String>,
}

#[derive(serde::Deserialize)]
struct DwellForm {
    #[serde(default)]
    secs: i64,
}

impl ArtifactDetail {
    /// `Chunk::in_results`, read off what the pane already holds rather than
    /// fetched again — and through that method's own predicate rather than a
    /// second copy of it, so a third lifecycle state still changes one place.
    fn in_results(&self) -> bool {
        crate::store::artifacts::in_results(self.status, self.superseded_by.as_deref())
    }
}

/// Everything the pane needs, in one place, so the handler is only routing.
/// What the Related list calls a neighbour: the same rule as `title_of`, read
/// off a vector payload instead of a row.
///
/// The snippet fallback that used to stand here is gone: the row prints one
/// under the name anyway, so an untitled neighbour was listed under the first
/// forty characters of the text and then showed the first ninety beneath it.
fn neighbour_title(p: &crate::vector::VectorPayload) -> String {
    let names_itself = p
        .provenance
        .as_deref()
        .map(crate::store::artifacts::Provenance::parse)
        .is_none_or(|p| p.names_its_own_text());
    match names_itself {
        true => p.title.clone().unwrap_or_default(),
        false => String::new(),
    }
}

pub(crate) async fn build_artifact_detail(
    core: &crate::core::Core,
    artifact_id: &str,
    terms: &str,
) -> Result<ArtifactDetail> {
    let c = core.store.get_artifact(artifact_id).await?;
    let html = artifact_html(&c);
    // A merged artifact belongs to no corpus, so there are no lines to show
    // beside it and no span to highlight. Task 15 fills that half of the pane
    // with the artifacts it was written from; until then it renders without a
    // source block rather than claiming a document it did not come from.
    let src = match &c.corpus_id {
        Some(id) => Some(core.store.get_corpus(id).await?),
        None => None,
    };
    // The lines the passage was drawn from, with a little context either
    // side: the source column is the claim about where the text on screen came
    // from, and what is on screen is this one artifact.
    let slice = match &src {
        Some(s) => crate::web::corpus_view::slice(s, c.corpus_span.as_ref(), 3),
        None => crate::web::corpus_view::CorpusSlice::default(),
    };
    // A missing lineage is not a missing pane, for the same reason a missing
    // neighbour list is not: it is a layer over the artifact, and the artifact
    // beside its source is what the page is for.
    let lineage = crate::web::lineage_view::build(&core.store, artifact_id)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(artifact_id, error = %e, "no lineage for this pane");
            Default::default()
        });
    // A missing neighbour list is not a missing pane. The vector store may be
    // down, or this artifact may simply not be embedded yet, and neither is a
    // reason to refuse to show the artifact beside its source.
    let related = core
        .vectors
        .neighbours(artifact_id, RELATED_LIMIT)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(artifact_id, error = %e, "no related artifacts for this pane");
            vec![]
        })
        .into_iter()
        .map(|h| RelatedArtifact {
            title: neighbour_title(&h.payload),
            snippet: markdown::snippet(&h.payload.text, 90),
            id: h.payload.artifact_id,
        })
        .collect();
    // Unreadable links are not a missing pane, for the same reason a missing
    // neighbour list is not: this layer can only ever add. And gated on
    // And gated on `associating()`: a base that learned links and then had
    // `[learn]` switched off must stop rendering them, the same as every other
    // associative surface.
    let anchor = vec![c.id.clone()];
    let seen_together_links = if core.associating() {
        match core
            .store
            .links_from(
                &anchor,
                &[
                    crate::store::links::LinkState::Learning,
                    crate::store::links::LinkState::Related,
                ],
                core.associate.half_life_days,
                crate::store::now(),
                core.associate.show_min,
                RELATED_LIMIT as i64,
            )
            .await
        {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(artifact_id, error = %e, "no links for this pane");
                vec![]
            }
        }
    } else {
        vec![]
    };
    let mut seen_together = Vec::new();
    for l in seen_together_links.into_iter().take(RELATED_LIMIT) {
        let Ok(other) = core.store.get_artifact(&l.other).await else {
            continue;
        };
        let corpus_title = match &other.corpus_id {
            Some(id) => core
                .store
                .get_corpus(id)
                .await
                .ok()
                .and_then(|s| s.title_hint)
                .unwrap_or_else(|| "untitled".into()),
            // A merged artifact belongs to no document, which is worth saying
            // rather than leaving blank.
            None => "merged".to_string(),
        };
        seen_together.push(SeenTogether {
            title: title_of(&other),
            snippet: markdown::snippet(&other.text, 90),
            // The judge's line where there is one; otherwise the question that
            // bound them, which is the link's own explanation and free.
            why: l
                .reason
                .clone()
                .or_else(|| l.cues.first().map(|c| format!("when asking: {}", c.q))),
            corpus_title,
            cross_corpus: l.cross_corpus,
            id: other.id,
        });
    }
    // Built before the struct consumes `c`. The fragment is what makes the
    // browser scroll to the span; the query parameters are what make the page
    // highlight it.
    // Empty for a merged artifact: there is no document to link to, and the
    // template hides the whole source block rather than offering a dead link.
    let source_at_lines = match (&c.corpus_id, c.corpus_span.as_ref()) {
        (Some(cid), Some(sp)) => format!(
            "/ui/corpora/{cid}?from={}&to={}#L{}",
            sp.start_line, sp.end_line, sp.start_line
        ),
        (Some(cid), None) => format!("/ui/corpora/{cid}"),
        (None, _) => String::new(),
    };
    let orphaned_source = c.flags.iter().any(|f| f == "orphaned_source");
    // Only asked when the passage actually stops mid-sentence: the query is a
    // second lookup per pane, and most passages end where a sentence does.
    let continues_at = match (&c.corpus_id, ends_mid_sentence(&c.text)) {
        (Some(cid), true) => core
            .store
            .adjacent_artifacts(cid, c.ordinal)
            .await
            .unwrap_or_default()
            .into_iter()
            .find(|n| n.ordinal > c.ordinal)
            .map(|n| n.id),
        _ => None,
    };
    // The same rule as `artifact_title`, and for the same reason: an ordinal in
    // the ingest is not a name. Taken before the struct, which moves `c`.
    let title = artifact_title(&c);
    let tab = crate::web::ui::row_label(&c).text;
    // A missing due moment is not a missing pane: most artifacts carry none.
    // Undated reminders are left out, the same as `due_for` leaves them out of
    // the list badge — there is no "in 2 h" to say about one.
    let due_in = core
        .store
        .open_due_for_artifact(artifact_id)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(artifact_id, error = %e, "no due state for this pane");
            None
        })
        // The *effective* instant, as `due_for` reads it for the result-row
        // badge and `open_due` for the band. A row snoozed from Tuesday to
        // next Thursday is off the band and its row badge says "in 7 days";
        // read raw, `at` had the pane say "due 2 days ago" about the same
        // reminder, on the same screen.
        .and_then(|m| m.snoozed_until.or(m.at))
        .map(ago_or_ahead);
    let tag = match core.store.integration_of(&c.id).await? {
        Some(i) => Some(match i.tag {
            crate::store::integrations::Tag::Novel => {
                "When it arrived, nothing in the base was near it.".to_string()
            }
            crate::store::integrations::Tag::Known => format!(
                "When it arrived, the base already held something like it{}.",
                i.nearest_id
                    .as_deref()
                    .map(|n| format!(" ({})", crate::web::insights::short(n)))
                    .unwrap_or_default()
            ),
            crate::store::integrations::Tag::Conflict => format!(
                "When it arrived, it disagreed with something the base held: {}",
                i.detail.unwrap_or_default()
            ),
        }),
        None => None,
    };
    let mut probes = Vec::new();
    for p in core.store.rehearsals_of(&c.id).await? {
        let last = core
            .store
            .results_of(&p.id, 1)
            .await?
            .first()
            .map(|r| r.rank);
        let q: String = p.query.chars().take(64).collect();
        probes.push(format!(
            "{} · \u{201c}{}{}\u{201d} · {}",
            p.class.as_str(),
            q,
            if p.query.chars().count() > 64 {
                "…"
            } else {
                ""
            },
            match last {
                Some(Some(r)) => format!("rank {r}"),
                Some(None) => "not found".to_string(),
                None => "not yet rehearsed".to_string(),
            }
        ));
    }
    let versions: Vec<(i64, String, String)> = core
        .store
        .versions_of(&c.id)
        .await?
        .iter()
        .map(|v| (v.n, ago(v.created_at), v.action_id.clone()))
        .collect();
    let condensed = core
        .store
        .open_action_on(&c.id, crate::store::actions::Kind::Condense)
        .await?
        .map(|a| a.id);
    Ok(ArtifactDetail {
        tag,
        probes,
        versions,
        condensed,
        due_in,
        continues_at,
        related,
        seen_together,
        orphaned_source,
        source_at_lines,
        lineage,
        id: c.id,
        title,
        tab,
        html,
        text: c.text,
        category: c.category,
        tags: c.tags,
        flags: c.flags,
        flag_detail: c.flag_detail,
        superseded_by: c.superseded_by,
        status: c.status,
        last_verified_at: c.last_verified_at,
        caveats: c.caveats,
        merged: c.provenance.is_model_written(),
        synthesized: c.provenance == crate::store::artifacts::Provenance::Synthesized,
        cues: c.cues,
        corpus_id: c.corpus_id,
        // A merged artifact has no corpus and so cannot have a restored one.
        corpus_restored: src.as_ref().is_some_and(|s| s.restored_at.is_some()),
        segment_idx: c.segment_idx,
        slice_label: slice.label,
        slice_lines: slice.lines,
        terms: terms.to_string(),
        search_event: None,
    })
}

/// One route, two shapes. An htmx swap wants the pane's body; a pasted link
/// wants a page with navigation around it.
async fn artifact_detail(
    tenant: Tenant,
    identity: crate::auth::Identity,
    headers: axum::http::HeaderMap,
    Path(cid): Path<String>,
    Query(p): Query<ArtifactViewParams>,
) -> UiResult<Response> {
    let mut d = build_artifact_detail(&tenant.core, &cid, &p.terms).await?;
    // Opened from the rail, which named the search that listed it. Stamped
    // here rather than looked up: the id is on the link, so this open is
    // attributed to the search it came from and to no other.
    //
    // Not for an artifact search will no longer return. `eval::export` freezes
    // only active, un-superseded artifacts and drops any pair naming something
    // else, so a hit recorded here would raise the recall and MRR on Insights
    // while contributing nothing to `pairs.json`. The verdict write refuses
    // one for that reason; so does this. Without `search_event` the bar
    // is not drawn, which is the only way a verdict can be given at all.
    // And only the searcher's own event: the id arrives on a link, so it is
    // whatever the caller sent. See `Store::event_is_mine`.
    if tenant.core.learn.enabled
        && d.in_results()
        && let Some(event) = p.event.as_deref()
        && tenant
            .core
            .store
            .event_is_mine(event, &tenant.user.subject)
            .await?
        && tenant.core.store.open_event(event, &cid).await?
    {
        d.search_event = Some(event.to_string());
    }
    // Opening a chunk is the deliberate act that counts as remembering it.
    tenant.core.mark_artifact_seen(&cid);
    // And the act the pursuit sweep reads: opened, or pivoted through — unless
    // this came from the area under the search box, in which case it is written
    // under its own kind and *not* as an ordinary open. A `recommended_open`
    // counted as an open is the first lucky guess growing into a habit the
    // system taught itself. Keyed on the rung and not on the slot: the floor of
    // the ladder carries no slot, and it is the rung with no evidence behind it
    // at all — the last one that should be teaching the profile.
    //
    // The marker is checked against the ladder rather than taken as written:
    // it arrives in a query string, and an unrecognised word would be recorded
    // as the rung it claims and counted on Ops as a row of its own. A link
    // carrying one is not an open under a rung, so it is an ordinary open.
    match p
        .rung
        .as_deref()
        .and_then(crate::core::recommend::Rung::parse)
    {
        Some(rung) => tenant.core.record_recommendation(
            &cid,
            "recommended_open",
            rung.as_str(),
            p.rec,
            Some(&tenant.user.subject),
        ),
        None => tenant
            .core
            .record_interaction(&cid, p.via.as_deref(), Some(&tenant.user.subject)),
    }
    // The live half of the same act. Written here rather than inside
    // `record_interaction` because this is where the session is known — and
    // that is the whole of what keeps the sitting at the web door.
    if let Some(sess) = &identity.session {
        tenant.core.sittings.touched(
            sess,
            &cid,
            crate::store::now(),
            tenant.core.pursuit.idle_secs as i64,
        );
    }
    if headers.contains_key("hx-request") {
        return Ok(HtmlTemplate(ArtifactDetailFragment { d }).into_response());
    }
    Ok(HtmlTemplate(ArtifactDetailPage { d }).into_response())
}

async fn put_artifact(
    tenant: Tenant,
    Path(cid): Path<String>,
    Form(f): Form<ArtifactEditForm>,
) -> UiResult<Response> {
    if f.text.trim().is_empty() {
        return Err(Error::Validation("chunk text is empty".into()).into());
    }
    tenant
        .core
        .store
        .update_artifact_text(&cid, &f.text)
        .await?;
    // The stored vector describes wording that no longer exists.
    tenant
        .core
        .store
        .enqueue(crate::store::jobs::Stage::Embed, "artifact", &cid)
        .await?;
    if f.view == "detail" {
        // Back to one appended passage. The run's length lives in the link the
        // reader last clicked, and a save posts a form rather than that link —
        // carrying it would mean threading a count through every control on
        // the pane to preserve something one click restores.
        let d = build_artifact_detail(&tenant.core, &cid, &f.terms).await?;
        return Ok(HtmlTemplate(ArtifactDetailFragment { d }).into_response());
    }
    let c = tenant.core.store.get_artifact(&cid).await?;
    Ok(HtmlTemplate(ArtifactFragment {
        c: artifact_view(&c),
    })
    .into_response())
}

/// Remove an artifact from both stores, from the page that shows it.
///
/// The deliberate counterpart to what `Core::heal_store_drift` stopped doing on
/// its own. A background pass cannot tell an artifact deleted on purpose from
/// one whose row a crash lost, so it now restores both and this button is the
/// only thing that removes anything — a person who can see the artifact deciding
/// it should go.
///
/// Two callers, two right answers. Pressed in a list — a search result, a card
/// on the source page — the answer is nothing at all: htmx swaps the row that
/// was pressed out of the list, and the page the operator was reading stays
/// where it was. Pressed in the pane, where the whole view *is* the artifact,
/// there is nothing left to stay on, so it lands on the source.
///
/// An empty 200 rather than a 204: htmx treats no-content as "swap nothing",
/// which would leave the deleted artifact on screen until a reload.
async fn delete_artifact_ui(
    tenant: Tenant,
    headers: axum::http::HeaderMap,
    Path(aid): Path<String>,
) -> UiResult<Response> {
    let corpus_id = tenant.core.store.get_artifact(&aid).await?.corpus_id;
    tenant.core.delete_artifact(&aid).await?;
    if headers.contains_key("hx-request") {
        return Ok(axum::response::Html(String::new()).into_response());
    }
    // A merged artifact has no document to return to, so the artifact list is
    // where deleting one leaves you.
    Ok(match corpus_id {
        Some(cid) => Redirect::to(&format!("/ui/corpora/{cid}")).into_response(),
        None => Redirect::to("/ui/insights").into_response(),
    })
}

/// The page saying how long an artifact was open, sent as the reader leaves
/// it. `sendBeacon` lands here; nothing is rendered back.
///
/// A pursuit signal and nothing more. It used to also label the search: a read
/// past twenty seconds was written as a hit, on the theory that recall could
/// come from ordinary use with nothing clicked. It cannot. What the timer
/// measures is a pane that stayed open, which is a tab abandoned as often as it
/// is an answer — and because the beacon flushes as the pane is *left*, it
/// arrived after the buttons it was overwriting and put a hit back on searches
/// a person had just marked "not sure" or undone. The bar under the result is
/// the whole of the answer now.
async fn artifact_dwell(
    tenant: Tenant,
    Path(aid): Path<String>,
    Form(f): Form<DwellForm>,
) -> UiResult<Response> {
    tenant
        .core
        .record_dwell(&aid, f.secs, Some(&tenant.user.subject));
    Ok(axum::http::StatusCode::NO_CONTENT.into_response())
}

/// The operator saying this pair does not belong together.
///
/// Final for that pair: never shown, never judged, never pruned. The weight is
/// left exactly as it is, so the decision stays auditable against the evidence
/// that produced it — undoing one is out of scope, and Ops is where it would go.
async fn dismiss_link(
    tenant: Tenant,
    Path((artifact_id, other_id)): Path<(String, String)>,
) -> UiResult<Response> {
    tenant
        .core
        .store
        .dismiss_link(&artifact_id, &other_id)
        .await?;
    // The row swaps itself out and leaves the pane alone, so the artifact you
    // were reading is still on screen afterwards.
    Ok(axum::response::Html(String::new()).into_response())
}

/// Clearing a flag is a judgement, not a fix: the operator looked at the chunk
/// beside its source lines and decided the warning was noise.
async fn mark_artifact_reviewed(tenant: Tenant, Path(cid): Path<String>) -> UiResult<Response> {
    // For an orphaned merge, "reviewed" means accepted as a merge of what
    // remains — recorded on source_count, or the next sweep re-flags it and
    // the operator's judgement lasts one tick.
    let c = tenant.core.store.get_artifact(&cid).await?;
    if c.flags.iter().any(|f| f == "orphaned_source") {
        tenant.core.store.accept_source_loss(&cid).await?;
    }
    tenant.core.store.clear_artifact_flags(&cid).await?;
    Ok(axum::response::Html(String::new()).into_response())
}

/// The bracket scan, over one run of prose between tags.
fn link_text(text: &str, n: usize) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        // The parsed number, never the digits as written: `[01]` cites excerpt
        // one, and an anchor of `#cite-01` points at nothing the rail emits.
        let cited = digits.parse::<usize>().ok().filter(|i| (1..=n).contains(i));
        match (after[digits.len()..].strip_prefix(']'), cited) {
            (Some(tail), Some(i)) => {
                out.push_str(&format!(
                    r##"<a class="cite" href="#cite-{i}">[{digits}]</a>"##
                ));
                rest = tail;
            }
            _ => {
                out.push('[');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Turns each `[n]` the answer cites into a link to that excerpt's rail item.
///
/// Bounded by `n`, the number of excerpts actually shown: a model writes `[7]`
/// over four excerpts often enough, and a link to a rail item that does not
/// exist scrolls nowhere while reading as a citation that is there. An
/// out-of-range bracket is left as the plain text it is.
///
/// Tag interiors are skipped because an attribute value is not prose, and code
/// spans are skipped because `argv[1]` is not a citation.
///
/// That second exclusion is the opposite of what `mark_unsupported` does over
/// the same markup, and deliberately so. Marking *subtracts* trust, and inside
/// code is where a fabricated command hides, so marking there is the feature.
/// Linking *adds* it: `<a href="#cite-1">[1]</a>` asserts that excerpt 1
/// supports this token, and a reader cannot tell an authored citation from a
/// coincidence. `arr[0]`, `argv[1]`, `results[2]` are exactly the shapes that
/// collide, because excerpt counts are single-digit and so are array indices —
/// on a base whose answers are full of code. Fabricated provenance is the one
/// failure this codebase exists to prevent, and a wrong link is worse than no
/// link.
pub(crate) fn link_citations(html: &str, n: usize) -> String {
    if n == 0 {
        return html.to_string();
    }
    crate::core::ask::check::for_text_between_tags(html, |t, in_code| match in_code {
        true => std::borrow::Cow::Borrowed(t),
        false => std::borrow::Cow::Owned(link_text(t, n)),
    })
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/ui/artifacts/{id}", get(artifact_detail).put(put_artifact))
        .route("/ui/artifacts/{cid}/reviewed", post(mark_artifact_reviewed))
        .route(
            "/ui/artifacts/{id}/links/{other}/dismiss",
            post(dismiss_link),
        )
        .route("/ui/artifacts/{id}/delete", post(delete_artifact_ui))
        .route("/ui/artifacts/{id}/dwell", post(artifact_dwell))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::test_support::{
        app_recommending, app_session_and_core, app_with_cookie, app_with_embedded_corpus,
        app_with_session, artifacts, ask_over_sse, body_of, done_html, drain, flat, form, get_body,
        hold_something, pulled, put_form, rail_html, searched_app, urlencoding_of,
    };
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    /// A passage carries the document heading it happened to sit under —
    /// `split_passages` copies it down, so one heading ends up over twenty
    /// passages. That is a name for the section, never for this text, and the
    /// two surfaces that print a name above the text must not print it.
    #[tokio::test]
    async fn a_passage_shows_no_heading_even_when_the_document_gave_it_one() {
        let core = crate::core::test_support::test_core().await;
        let src = core
            .ingest("body of the passage", "web", None)
            .await
            .unwrap();
        let p = core
            .store
            .insert_artifacts_with_provenance(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    text: "body of the passage".into(),
                    title: Some("Wiederherstellung geloeschter Eintraege".into()),
                    segment_idx: Some(0),
                    ..Default::default()
                }],
                crate::store::artifacts::Provenance::Passage,
            )
            .await
            .unwrap();
        let c = core.store.get_artifact(&p[0].id).await.unwrap();
        assert_eq!(
            artifact_view(&c).title,
            "",
            "the corpus card named a passage"
        );
        let d = super::build_artifact_detail(&core, &p[0].id, "")
            .await
            .unwrap();
        assert_eq!(d.title, "", "the detail pane named a passage");
    }

    /// `d.title` is empty for a passage, but the pane still wrote the element
    /// that holds it: an empty `card-title` before the Edit button, taking the
    /// head row's gap and the heading's line-height over a card whose text
    /// starts at the top. And the browser tab needs a word, so the page title
    /// falls to the opening of the text.
    #[tokio::test]
    async fn the_pane_writes_no_heading_element_over_a_passage() {
        let core = crate::core::test_support::test_core().await;
        let src = core
            .ingest(
                "Der Vorgang setzt voraus, dass das Journal noch steht.",
                "web",
                None,
            )
            .await
            .unwrap();
        let p = core
            .store
            .insert_artifacts_with_provenance(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    text: "Der Vorgang setzt voraus, dass das Journal noch steht.".into(),
                    title: Some("Kapitel 3".into()),
                    segment_idx: Some(0),
                    ..Default::default()
                }],
                crate::store::artifacts::Provenance::Passage,
            )
            .await
            .unwrap();
        let d = super::build_artifact_detail(&core, &p[0].id, "")
            .await
            .unwrap();
        let html = askama::Template::render(&ArtifactDetailFragment { d }).unwrap();
        assert!(!html.contains("Kapitel 3"), "{html}");
        assert!(
            !html.contains("card-title"),
            "an empty heading still took a line: {html}"
        );
    }

    /// The full page still needs a word: a browser tab and a history entry
    /// are a list of names, and `{{ d.title }} — engram` over a passage left
    /// every one of them reading " — engram".
    #[tokio::test]
    async fn the_page_title_of_a_passage_is_how_its_text_opens() {
        let core = crate::core::test_support::test_core().await;
        let text = "Der Vorgang setzt voraus, dass das Journal noch steht.";
        let src = core.ingest(text, "web", None).await.unwrap();
        let p = core
            .store
            .insert_artifacts_with_provenance(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    text: text.into(),
                    title: Some("Kapitel 3".into()),
                    segment_idx: Some(0),
                    ..Default::default()
                }],
                crate::store::artifacts::Provenance::Passage,
            )
            .await
            .unwrap();
        let d = super::build_artifact_detail(&core, &p[0].id, "")
            .await
            .unwrap();
        let html = askama::Template::render(&ArtifactDetailPage { d }).unwrap();
        let tab = html
            .split("<title>")
            .nth(1)
            .and_then(|t| t.split("</title>").next())
            .unwrap_or_default()
            .to_string();
        assert!(
            tab.starts_with("Der Vorgang setzt voraus"),
            "the tab read {tab:?}"
        );
        assert!(!tab.contains("Kapitel 3"), "the tab read {tab:?}");
    }

    /// The Related list is built off vector payloads rather than rows, so it
    /// has its own reading of the same question — and it answered it
    /// differently: `payload.title` straight through, which put the section
    /// heading over a neighbouring passage while the rail beside it showed
    /// none.
    #[test]
    fn a_neighbouring_passage_is_listed_without_a_name() {
        let payload = |provenance: &str, title: Option<&str>| crate::vector::VectorPayload {
            artifact_id: "a".into(),
            corpus_id: "c".into(),
            text: "Der Vorgang setzt voraus, dass das Journal noch steht.".into(),
            title: title.map(str::to_string),
            provenance: Some(provenance.into()),
            ..Default::default()
        };
        assert_eq!(neighbour_title(&payload("passage", Some("Kapitel 3"))), "");
        assert_eq!(neighbour_title(&payload("note", None)), "");
        assert_eq!(
            neighbour_title(&payload("captured", Some("Wie ein Journal steht"))),
            "Wie ein Journal steht",
            "a name a writer gave this text is still shown"
        );
    }

    /// Related and Seen together are rows with a snippet under the name. With
    /// no name the element was still written, so each row opened with an empty
    /// line where the other rows carry a heading.
    #[tokio::test]
    async fn a_related_row_with_no_name_writes_no_heading_element() {
        let core = crate::core::test_support::test_core().await;
        let text = "Der Vorgang setzt voraus, dass das Journal noch steht.";
        let src = core.ingest(text, "web", None).await.unwrap();
        let a = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    text: text.into(),
                    title: Some("Wie ein Journal steht".into()),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();
        let mut d = super::build_artifact_detail(&core, &a[0].id, "")
            .await
            .unwrap();
        d.related = vec![RelatedArtifact {
            id: "n1".into(),
            title: String::new(),
            snippet: "Ein benachbarter Abschnitt".into(),
        }];
        let html = askama::Template::render(&ArtifactDetailFragment { d }).unwrap();
        assert!(html.contains("Ein benachbarter Abschnitt"), "{html}");
        assert!(
            !html.contains("rail-title"),
            "an empty heading still took a line: {html}"
        );
    }

    #[tokio::test]
    async fn a_snoozed_reminder_reads_the_same_in_the_pane_as_in_the_row() {
        use crate::store::moments::{Kind, NewMoment, Source};
        let core = crate::core::test_support::test_core().await;
        let out = core
            .ingest_capture(crate::core::ingest::Capture::new("Pay the rent", "ui"))
            .await
            .unwrap();
        crate::jobs::test_support::drain(&core).await;
        let aid = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .into_iter()
            .find(|c| c.in_results())
            .expect("a live artifact")
            .id;
        let now = core.clock.now();
        let id = core
            .store
            .insert_moment(&NewMoment {
                artifact_id: aid.clone(),
                kind: Kind::Due,
                at: Some(now - 2 * 86_400),
                tz: "Europe/Berlin".into(),
                rule: None,
                source: Source::Set,
                span: None,
                series_id: None,
            })
            .await
            .unwrap();
        core.store.snooze(&id, now + 7 * 86_400).await.unwrap();

        // The band hides it and the result-row badge reads the effective
        // instant. The pane read raw `at` and said "due 2 days ago" about the
        // same row, on the same screen — the defect `Store::due_for`'s comment
        // says was already fixed once.
        let d = build_artifact_detail(&core, &aid, "").await.unwrap();
        let due = d.due_in.expect("the pane says a reminder is here");
        assert!(!due.contains("ago"), "the pane read past the snooze: {due}");
    }

    #[tokio::test]
    async fn a_captured_artifact_lists_no_sources() {
        // The template branches on provenance, and a captured artifact filling
        // this list would put a provenance list it does not have where its
        // corpus lines belong.
        let core = crate::core::test_support::test_core().await;
        let ids = crate::jobs::consolidate::tests::seed(&core, &[("a text", [1.0, 0.0])]).await;

        let d = build_artifact_detail(&core, &ids[0], "").await.unwrap();

        assert!(d.lineage.is_empty());
        assert!(!d.merged);
        assert!(d.corpus_id.is_some());
    }

    #[tokio::test]
    async fn a_merge_that_lost_every_source_still_renders_as_a_merge() {
        // An empty source list is not the same question as "was this captured".
        // The template branched on the list, so a merge whose sources had all
        // been deleted fell through to the captured branch and rendered a
        // "Source · … highlighted" label over an empty link and an empty line
        // table — on exactly the artifact whose orphan notice matters most.
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
        for id in &ids {
            core.store.delete_artifact(id).await.unwrap();
        }

        let d = build_artifact_detail(&core, &m.id, "").await.unwrap();

        assert!(
            d.lineage.roots.is_empty(),
            "the fixture did not lose the sources"
        );
        assert!(d.merged, "a merge was rendered as a captured artifact");
        assert!(
            d.source_at_lines.is_empty() && d.slice_lines.is_empty(),
            "there is no document to link and no lines to show"
        );
    }

    #[tokio::test]
    async fn the_detail_view_pairs_a_chunk_with_the_lines_it_claims() {
        let core = crate::core::test_support::test_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line\n\ncharlie line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        let c = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .remove(0);

        let d = match super::build_artifact_detail(&core, &c.id, "").await {
            Ok(d) => d,
            Err(e) => panic!("detail view failed: {e}"),
        };

        assert_eq!(d.corpus_id.as_deref(), Some(out.id.as_str()));
        assert!(d.html.contains("alpha"), "the chunk body must be rendered");
        assert!(
            !d.slice_lines.is_empty(),
            "the source slice must not be empty"
        );
        assert!(
            d.slice_lines.iter().any(|l| l.in_span),
            "at least one line must be marked as the span"
        );
        // Either form: this artifact's span may be one line or several, and
        // the label says which rather than always saying "lines".
        assert!(
            d.slice_label.starts_with("line ") || d.slice_label.starts_with("lines "),
            "{}",
            d.slice_label
        );
    }

    #[tokio::test]
    async fn a_passage_cut_mid_sentence_points_at_the_one_that_carries_the_rest() {
        // The pane ended "…der bereits vorgestellte Einsatz von" and offered
        // nothing onward, while the source column beside it showed the rest of
        // the sentence. A verbatim-path property: the chunker is what cuts a
        // sentence, so shrink the chunk budget until it does and read the
        // passages as capture wrote them.
        let mut core = crate::core::test_support::test_core().await;
        core.chunk_tokens = 12;
        let out = core
            .ingest(
                "Die erste Vorkehrung ist der bereits vorgestellte Einsatz von\n\n\
                 Hardware-Schreibschutzadaptern, wo immer es möglich ist.",
                "web",
                None,
            )
            .await
            .unwrap();
        crate::jobs::synthesize::plan(&core, &out.id).await.unwrap();
        // The live rows: superseded passages stand behind the artifacts that
        // cover them and are not what the pane walks between.
        let all: Vec<_> = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .into_iter()
            .filter(|c| c.in_results())
            .collect();
        assert!(all.len() > 1, "the fixture produced one passage, not two");

        let first = all.iter().min_by_key(|c| c.ordinal).unwrap();
        let d = super::build_artifact_detail(&core, &first.id, "")
            .await
            .unwrap();
        assert!(
            d.continues_at.is_some(),
            "no way onward from a cut sentence: {:?}",
            first.text
        );

        // And the way onward is a swap, not a navigation. Left as a bare
        // `href` this was the one link in the pane that left it: following it
        // loaded the standalone artifact page, and the results the passage was
        // found in — the box, the rail, the run — went with it. The Related
        // list two blocks down has always swapped in place; this is the same
        // act and takes the same route.
        let html = askama::Template::render(&ArtifactDetailFragment { d }).unwrap();
        let onward = html
            .split(r#"<p class="continues">"#)
            .nth(1)
            .and_then(|s| s.split("</p>").next())
            .expect("the way onward is rendered");
        assert!(onward.contains("continues in the next passage"), "{onward}");
        assert!(onward.contains("hx-get=\"/ui/artifacts/"), "{onward}");
        assert!(
            onward.contains(r#"hx-target="closest [data-terms]""#),
            "it must replace the detail it is printed under: {onward}"
        );
        assert!(
            onward.contains("href=\"/ui/artifacts/"),
            "and keep the plain href for a browser running no script: {onward}"
        );

        let last = all.iter().max_by_key(|c| c.ordinal).unwrap();
        let d = super::build_artifact_detail(&core, &last.id, "")
            .await
            .unwrap();
        assert!(
            d.continues_at.is_none(),
            "the last passage ends on a period and has nothing after it"
        );
    }

    #[tokio::test]
    async fn the_floor_of_the_ladder_is_clicked_like_an_offer_and_not_like_a_result() {
        // The random card has no cluster and so no slot. Hanging the whole
        // offer marker off the slot left it linking like an ordinary result:
        // its opens counted as `opened`, so Ops read `random: shown N, opened
        // 0` for ever — and random is the baseline the block weights would have
        // to be fitted against. Worse, the open fed back into the profile at
        // full weight, which is the self-reinforcement `self_weight = 0.0`
        // exists to close, entered through the one rung with no evidence behind
        // it at all.
        let (app, cookie, store, aid) = app_recommending().await;
        let body = crate::web::test_support::body_of(
            app.clone()
                .oneshot(form("/ui/context", &cookie, "bundle=%7B%7D"))
                .await
                .unwrap(),
        )
        .await;
        assert!(
            body.contains(&format!("/ui/artifacts/{aid}?rung=random")),
            "the card links like an ordinary result: {body}"
        );

        // The browser confirming the card reached the screen. The fragment
        // above computed it; only this says anybody saw it.
        app.clone()
            .oneshot(form(
                "/ui/context/seen",
                &cookie,
                &format!("artifact_id={aid}&rung=random"),
            ))
            .await
            .unwrap();
        get_body(&app, &cookie, &format!("/ui/artifacts/{aid}?rung=random")).await;
        drain().await;
        // A shown and an open, and both of them on the random rung: that pair
        // is the hit rate, and before this the second half could never be
        // written.
        let rows = store.interactions_between(0, i64::MAX).await.unwrap();
        let kinds: Vec<&str> = rows.iter().map(|r| r.kind.as_str()).collect();
        assert_eq!(kinds, vec!["recommended_shown", "recommended_open"]);
        assert!(
            rows.iter()
                .all(|r| r.detail.as_deref().unwrap_or_default().contains("random")),
            "Ops cannot tell which rung: {rows:?}"
        );
    }

    #[tokio::test]
    async fn a_rung_the_ladder_does_not_have_is_an_ordinary_open() {
        // The marker rides in the query string, so its value is whatever the
        // viewer sends. Taken as written it went into the recorded row and from
        // there into `offer_rates`' `GROUP BY rung`, which is to say anyone
        // could add rows to the Ops breakdown by editing a URL — beside the
        // four rungs that exist, in the one table the block weights would be
        // fitted against.
        let (app, cookie, store, aid) = app_recommending().await;
        get_body(
            &app,
            &cookie,
            &format!("/ui/artifacts/{aid}?rung=excellent"),
        )
        .await;
        drain().await;

        let rows = store.interactions_between(0, i64::MAX).await.unwrap();
        let kinds: Vec<&str> = rows.iter().map(|r| r.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec!["opened"],
            "an invented rung was recorded as an offer: {rows:?}"
        );
        assert!(
            store
                .offer_rates(0)
                .await
                .unwrap()
                .iter()
                .all(|r| crate::core::recommend::Rung::parse(&r.rung).is_some()),
            "Ops lists a rung the ladder does not have"
        );
    }

    #[tokio::test]
    async fn taking_an_offer_is_not_an_ordinary_open() {
        // Without this the profile reinforces itself. The row is written, and
        // it is written under its own kind so the sweep can weigh it at
        // `self_weight` — which is zero.
        let mut core = crate::core::test_support::test_core().await;
        core.recommend.enabled = true;
        core.learn.enabled = true;
        // Both on, so an ordinary open *would* be recorded — otherwise this
        // test would pass on a base that records nothing at all.
        core.learn.enabled = true;
        let store = core.store.clone();
        let background = core.background.clone();
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let aid = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    text: "opening hours".into(),
                    title: Some("hours".into()),
                    ..Default::default()
                }],
            )
            .await
            .unwrap()
            .remove(0)
            .id;
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;

        get_body(
            &app,
            &cookie,
            &format!("/ui/artifacts/{aid}?rec=0&rung=pattern"),
        )
        .await;
        background.wait_idle().await;

        let rows = store.interactions_between(0, i64::MAX).await.unwrap();
        let kinds: Vec<&str> = rows.iter().map(|r| r.kind.as_str()).collect();
        assert_eq!(kinds, vec!["recommended_open"], "not an ordinary open");
        assert!(
            rows[0].detail.as_deref().unwrap().contains("pattern"),
            "and it remembers which rung it was offered on: {:?}",
            rows[0].detail
        );

        // And the ordinary path still records an ordinary open, so the branch
        // above is a branch rather than a hole.
        get_body(&app, &cookie, &format!("/ui/artifacts/{aid}")).await;
        background.wait_idle().await;
        let rows = store.interactions_between(0, i64::MAX).await.unwrap();
        assert!(rows.iter().any(|r| r.kind == "opened"));
    }

    /// One merge written from an earlier merge and one fresh capture. A flat
    /// list of roots reads as three equal siblings; the generation between them
    /// is the whole reason the tree exists.
    #[tokio::test]
    async fn the_pane_draws_the_generations_a_merge_came_through() {
        let (app, cookie, core) = app_session_and_core().await;
        let ids = crate::jobs::consolidate::tests::seed_titled(
            &core,
            &[
                ("first capture", "a text", [1.0, 0.0]),
                ("second capture", "b text", [0.93, 0.37]),
                ("third capture", "c text", [0.9, 0.4]),
            ],
        )
        .await;
        let draft = |t: &str| crate::infer::prompt::MergedDraft {
            title: Some(t.into()),
            text: format!("{t} text"),
            category: None,
            tags: vec![],
            caveats: vec![],
        };
        let m1 = crate::jobs::merge::write(&core, &draft("first pass"), &ids[0..2])
            .await
            .unwrap();
        let m2 = crate::jobs::merge::write(
            &core,
            &draft("second pass"),
            &[m1.id.clone(), ids[2].clone()],
        )
        .await
        .unwrap();

        // What `merge::finish` does once the merge is indexed: the sources it
        // was written from are hidden behind it. Set here because this test is
        // about how the pane draws that, not about the write path.
        for hidden in [&ids[2], &m1.id] {
            core.store
                .set_superseded_by(hidden, Some(&m2.id))
                .await
                .unwrap();
        }

        let page = get_body(&app, &cookie, &format!("/ui/artifacts/{}", m2.id)).await;

        assert!(page.contains(r#"class="lineage""#), "{page}");
        assert!(
            page.contains("first pass"),
            "the earlier merge is a node: {page}"
        );
        for t in ["first capture", "second capture", "third capture"] {
            assert!(page.contains(t), "{t} is missing from the lineage: {page}");
        }
        assert!(
            page.contains("--d:1"),
            "the earlier merge's own sources are drawn under it: {page}"
        );
        assert!(
            page.contains("Written from 3 artifacts"),
            "the count is of captures, not of the route they took: {page}"
        );
        // The roots this merge superseded say so where they sit.
        assert!(page.contains("replaced by this"), "{page}");
    }

    /// A captured artifact was written from a document, not from artifacts. Its
    /// column is the document, and a tree there would be an empty claim.
    #[tokio::test]
    async fn the_pane_of_a_capture_still_shows_its_lines() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        let c = core.store.artifacts_for_corpus(&out.id).await.unwrap()[0]
            .id
            .clone();

        let page = get_body(&app, &cookie, &format!("/ui/artifacts/{c}")).await;

        assert!(page.contains("Source"), "{page}");
        assert!(!page.contains(r#"class="lineage""#), "{page}");
    }

    /// The reported gap: a reminder was captured and nothing on the artifact
    /// itself ever said so. Unlike the same badge in the result list, this
    /// one is not bounded to `time.horizon_hours` — the pane is where an
    /// operator checks one specific note, and a reminder set for next week
    /// is exactly the kind `due_for`'s horizon leaves out of that list.
    #[tokio::test]
    async fn the_pane_badges_an_open_reminder_however_far_out_it_is() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core.ingest("water the plants", "web", None).await.unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        let c = core.store.artifacts_for_corpus(&out.id).await.unwrap()[0]
            .id
            .clone();
        core.store
            .insert_moment(&crate::store::moments::NewMoment {
                artifact_id: c.clone(),
                kind: crate::store::moments::Kind::Due,
                // An hour of slack on top of the thirty days. `ago_or_ahead`
                // divides by 86_400 and floors, so an exact multiple reads as
                // "29 days" the moment one second passes between this row
                // being written and the page rendering it — which is a second
                // the full suite spends often enough to fail here and nowhere
                // when the test runs alone.
                at: Some(crate::store::now() + 30 * 86_400 + 3_600),
                tz: "UTC".into(),
                rule: None,
                source: crate::store::moments::Source::Set,
                span: None,
                series_id: None,
            })
            .await
            .unwrap();

        let page = get_body(&app, &cookie, &format!("/ui/artifacts/{c}")).await;
        assert!(page.contains("badge-due"), "{page}");
        assert!(
            page.contains("30 days"),
            "far outside the 48h horizon, still shown: {page}"
        );
    }

    #[tokio::test]
    async fn the_pane_of_an_artifact_with_no_reminder_carries_no_due_badge() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core.ingest("just a note", "web", None).await.unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        let c = core.store.artifacts_for_corpus(&out.id).await.unwrap()[0]
            .id
            .clone();

        let page = get_body(&app, &cookie, &format!("/ui/artifacts/{c}")).await;
        assert!(!page.contains("badge-due"), "{page}");
    }

    /// The corpus page could edit an artifact and the pane could not, on the
    /// screen whose whole subject is one artifact.
    #[tokio::test]
    async fn the_pane_edits_the_artifact_and_comes_back_a_pane() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core.ingest("alpha line", "web", None).await.unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        let c = core.store.artifacts_for_corpus(&out.id).await.unwrap()[0]
            .id
            .clone();

        let page = get_body(&app, &cookie, &format!("/ui/artifacts/{c}")).await;
        assert!(page.contains(&format!(r#"id="edit-{c}""#)), "{page}");

        let res = app
            .clone()
            .oneshot(put_form(
                &format!("/ui/artifacts/{c}"),
                &cookie,
                "view=detail&terms=&text=rewritten+by+hand",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = body_of(res).await;
        assert!(
            body.contains("data-terms"),
            "the pane was replaced by a list card: {body}"
        );
        assert!(body.contains("rewritten by hand"), "{body}");
        assert_eq!(
            core.store.get_artifact(&c).await.unwrap().embed_state,
            crate::store::artifacts::EmbedState::Pending,
            "the stored vector describes wording that no longer exists"
        );
    }

    /// And the corpus page, which swaps one card in a list, still gets a card.
    #[tokio::test]
    async fn the_corpus_card_edit_still_answers_with_a_card() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core.ingest("alpha line", "web", None).await.unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        let c = core.store.artifacts_for_corpus(&out.id).await.unwrap()[0]
            .id
            .clone();

        let res = app
            .clone()
            .oneshot(put_form(
                &format!("/ui/artifacts/{c}"),
                &cookie,
                "text=edited+from+the+corpus+page",
            ))
            .await
            .unwrap();
        let body = body_of(res).await;
        assert!(body.contains(&format!(r#"id="artifact-{c}""#)), "{body}");
        assert!(!body.contains("data-terms"), "{body}");
    }

    #[tokio::test]
    async fn the_pane_lists_the_nearest_other_artifacts() {
        let core = crate::core::test_support::test_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line\n\ncharlie line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        crate::jobs::embed::run_corpus(&core, &out.id)
            .await
            .unwrap();
        let artifacts = core.store.artifacts_for_corpus(&out.id).await.unwrap();
        assert!(
            artifacts.len() > 1,
            "a neighbour list needs something to be a neighbour of"
        );

        let d = super::build_artifact_detail(&core, &artifacts[0].id, "")
            .await
            .unwrap();
        assert!(!d.related.is_empty(), "the pane listed no neighbours");
        assert!(
            d.related.iter().all(|r| r.id != artifacts[0].id),
            "an artifact must not be listed as its own neighbour"
        );
        assert!(d.related.len() <= RELATED_LIMIT);
    }

    #[tokio::test]
    async fn the_pane_lists_what_this_artifact_is_seen_together_with() {
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        let ids = artifacts(&core, &["alpha text", "something else entirely"]).await;
        core.store
            .bump_link(
                &ids[0],
                &ids[1],
                5.0,
                Some("mount forensic image"),
                30.0,
                crate::store::now(),
            )
            .await
            .unwrap();

        let d = build_artifact_detail(&core, &ids[0], "").await.unwrap();
        assert_eq!(d.seen_together.len(), 1);
        assert_eq!(d.seen_together[0].id, ids[1]);
        assert_eq!(
            d.seen_together[0].why.as_deref(),
            Some("when asking: mount forensic image"),
            "an unjudged link explains itself with the question that bound it"
        );
    }

    #[tokio::test]
    async fn a_judged_link_shows_the_judges_line_instead_of_the_query() {
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        let ids = artifacts(&core, &["alpha text", "something else entirely"]).await;
        core.store
            .bump_link(&ids[0], &ids[1], 5.0, Some("q"), 30.0, crate::store::now())
            .await
            .unwrap();
        core.store
            .set_link_state(
                &ids[0],
                &ids[1],
                crate::store::links::LinkState::Related,
                Some("the tool and the error it prints"),
                Some((0, 0)),
            )
            .await
            .unwrap();

        let d = build_artifact_detail(&core, &ids[0], "").await.unwrap();
        assert_eq!(
            d.seen_together[0].why.as_deref(),
            Some("the tool and the error it prints")
        );
    }

    #[tokio::test]
    async fn dismissing_a_link_takes_it_out_for_good_without_losing_the_evidence() {
        // The weight stays, so the decision is auditable; the state is final,
        // so it is never shown, judged or pruned again.
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(&core, &["alpha text", "something else entirely"]).await;
        core.store
            .bump_link(&ids[0], &ids[1], 5.0, Some("q"), 30.0, crate::store::now())
            .await
            .unwrap();

        app.clone()
            .oneshot(form(
                &format!("/ui/artifacts/{}/links/{}/dismiss", ids[0], ids[1]),
                &cookie,
                "",
            ))
            .await
            .unwrap();

        let l = core
            .store
            .get_link(&ids[0], &ids[1])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(l.state, crate::store::links::LinkState::Dismissed);
        assert!(
            l.weight > 0.0,
            "the evidence was thrown away with the decision"
        );
        assert!(
            build_artifact_detail(&core, &ids[0], "")
                .await
                .unwrap()
                .seen_together
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_pane_still_renders_when_the_links_cannot_be_read() {
        // The associative layer can only add. It is not a reason to refuse to
        // show an artifact beside its source.
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        let ids = artifacts(&core, &["alpha text"]).await;
        sqlx::query("DROP TABLE artifact_links")
            .execute(&core.store.pool)
            .await
            .unwrap();
        let d = build_artifact_detail(&core, &ids[0], "").await.unwrap();
        assert!(d.seen_together.is_empty());
    }

    #[tokio::test]
    async fn a_cross_corpus_pair_is_marked_and_a_same_corpus_pair_is_not() {
        // "Two documents needing each other is the finding; two passages of one
        // document needing each other is not" is the whole point of the flag —
        // pin it on the data the pane renders, not on a CSS class name.
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        let ids = artifacts(&core, &["alpha text", "same corpus neighbour"]).await;
        let other_corpus = core.store.insert_corpus("y", "web", None).await.unwrap();
        let made = core
            .store
            .insert_artifacts(
                &other_corpus.id,
                &[crate::store::artifacts::NewArtifact {
                    text: "body of other document".to_string(),
                    title: Some("other document".to_string()),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();
        let cross_id = made[0].id.clone();

        core.store
            .bump_link(&ids[0], &ids[1], 5.0, Some("q1"), 30.0, crate::store::now())
            .await
            .unwrap();
        core.store
            .bump_link(
                &ids[0],
                &cross_id,
                5.0,
                Some("q2"),
                30.0,
                crate::store::now(),
            )
            .await
            .unwrap();

        let d = build_artifact_detail(&core, &ids[0], "").await.unwrap();
        let same = d
            .seen_together
            .iter()
            .find(|r| r.id == ids[1])
            .expect("the same-corpus pair should still be listed");
        let cross = d
            .seen_together
            .iter()
            .find(|r| r.id == cross_id)
            .expect("the cross-corpus pair should be listed");
        assert!(
            !same.cross_corpus,
            "two passages of one document is not the finding"
        );
        assert!(
            cross.cross_corpus,
            "two documents needing each other is the finding"
        );
    }

    #[tokio::test]
    async fn a_related_link_works_on_the_standalone_artifact_page() {
        // The detail partial is both the search pane's content and the whole of
        // `/ui/artifacts/{id}`. A neighbour link that named `#pane` would be
        // dead on the standalone page, which is the one a shared link opens.
        let (app, cookie) = app_with_embedded_corpus().await;
        let rail = get_body(&app, &cookie, "/ui/search/results?q=alpha").await;
        let id = rail
            .split(r#"hx-get="/ui/artifacts/"#)
            .nth(1)
            .and_then(|s| s.split('"').next())
            .and_then(|s| s.split('?').next())
            .expect("no result to open")
            .to_string();

        let page = flat(&get_body(&app, &cookie, &format!("/ui/artifacts/{id}")).await);
        assert!(
            page.contains("Related"),
            "the standalone page must list neighbours"
        );
        assert!(
            !page.contains(r##"hx-target="#pane""##),
            "no pane exists on this page, so nothing may target one"
        );
        assert!(
            page.contains(r#"hx-target="closest [data-terms]""#),
            "a neighbour must swap the detail it is listed under"
        );
    }

    #[tokio::test]
    async fn a_lifecycle_button_comes_back_to_the_page_that_offered_it() {
        // These four actions are rendered both on Ops and on an artifact's own
        // page. Always redirecting to Ops threw a reader who pressed "Confirm
        // still accurate" while reading an artifact onto a queue they were not
        // working through.
        let (app, cookie) = app_with_embedded_corpus().await;
        let rail = get_body(&app, &cookie, "/ui/search/results?q=alpha").await;
        let id = rail
            .split(r#"hx-get="/ui/artifacts/"#)
            .nth(1)
            .and_then(|s| s.split('"').next())
            .and_then(|s| s.split('?').next())
            .expect("no result to open")
            .to_string();

        let res = app
            .clone()
            .oneshot(form(
                &format!("/ui/ops/artifacts/{id}/verify"),
                &cookie,
                &format!("to=/ui/artifacts/{id}"),
            ))
            .await
            .unwrap();
        assert_eq!(
            res.headers().get("location").unwrap(),
            format!("/ui/artifacts/{id}").as_str()
        );

        // Ops sends no `to` and keeps the default.
        let res = app
            .clone()
            .oneshot(form(
                &format!("/ui/ops/artifacts/{id}/deprecate"),
                &cookie,
                "",
            ))
            .await
            .unwrap();
        assert_eq!(res.headers().get("location").unwrap(), "/ui/insights");
    }

    #[tokio::test]
    async fn a_lifecycle_button_pressed_in_the_pane_swaps_the_artifact_not_the_page() {
        // The same fragment is the standalone page and the pane beside the
        // search results, and the hidden `to` can only name one of them. It
        // named the page, so pressing "Confirm still accurate" on a result
        // navigated the whole window there and took the results with it.
        let (app, cookie) = app_with_embedded_corpus().await;
        let rail = get_body(&app, &cookie, "/ui/search/results?q=alpha").await;
        let id = rail
            .split(r#"hx-get="/ui/artifacts/"#)
            .nth(1)
            .and_then(|s| s.split('"').next())
            .and_then(|s| s.split('?').next())
            .expect("no result to open")
            .to_string();

        let mut req = form(
            &format!("/ui/ops/artifacts/{id}/verify"),
            &cookie,
            &format!("to=/ui/artifacts/{id}"),
        );
        req.headers_mut()
            .insert("hx-request", "true".parse().unwrap());
        let res = app.clone().oneshot(req).await.unwrap();

        assert_eq!(res.status(), StatusCode::OK);
        assert!(
            res.headers().get("location").is_none(),
            "a swap must not navigate"
        );
        let body = crate::web::test_support::body_of(res).await;
        assert!(
            body.contains(&format!(r#"data-artifact="{id}""#)),
            "the answer is the artifact, re-rendered: {body}"
        );
        // A fragment, not a whole page: the pane is inside one already.
        assert!(!body.contains("<nav"), "{body}");
    }

    #[tokio::test]
    async fn a_return_path_pointing_off_this_ui_is_ignored() {
        // The field is user input, and a redirect that follows anything handed
        // to it is an open redirect: worth nothing here, a phishing hop
        // everywhere else.
        let (app, cookie) = app_with_embedded_corpus().await;
        let rail = get_body(&app, &cookie, "/ui/search/results?q=alpha").await;
        let id = rail
            .split(r#"hx-get="/ui/artifacts/"#)
            .nth(1)
            .and_then(|s| s.split('"').next())
            .and_then(|s| s.split('?').next())
            .expect("no result to open")
            .to_string();

        for hostile in ["https://evil.example/x", "//evil.example/x", "/ui//evil"] {
            let res = app
                .clone()
                .oneshot(form(
                    &format!("/ui/ops/artifacts/{id}/verify"),
                    &cookie,
                    &format!("to={}", urlencoding_of(hostile)),
                ))
                .await
                .unwrap();
            assert_eq!(
                res.headers().get("location").unwrap(),
                "/ui/insights",
                "followed {hostile}"
            );
        }
    }

    #[tokio::test]
    async fn an_artifact_that_is_not_embedded_yet_still_opens() {
        // Synthesis without the embed job: the pane has to show the artifact
        // beside its source and simply offer no neighbours.
        let core = crate::core::test_support::test_core().await;
        let out = core.ingest("alpha\n\nbravo", "web", None).await.unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        let c = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .remove(0);

        let d = super::build_artifact_detail(&core, &c.id, "")
            .await
            .unwrap();
        assert!(d.related.is_empty());
        assert!(!d.html.is_empty(), "the artifact itself must still render");
    }

    #[tokio::test]
    async fn a_verbatim_passage_card_keeps_the_lines_markdown_would_flatten() {
        let core = crate::core::test_support::test_core().await;
        let src = core
            .ingest("Dateiattribute\n.........24", "web", None)
            .await
            .unwrap();
        let na = |t: &str| crate::store::artifacts::NewArtifact {
            text: t.into(),
            segment_idx: Some(0),
            ..Default::default()
        };
        let p = core
            .store
            .insert_artifacts_with_provenance(
                &src.id,
                &[na("Dateiattribute\n.........24")],
                crate::store::artifacts::Provenance::Passage,
            )
            .await
            .unwrap();
        let card = artifact_view(&core.store.get_artifact(&p[0].id).await.unwrap());
        assert!(card.html.contains("<pre"), "{}", card.html);
        assert!(
            card.html.contains("Dateiattribute\n.........24"),
            "{}",
            card.html
        );

        // And a model-written artifact is still markdown: it was written as
        // markdown, and reading it as plain text would show the syntax.
        let a = core
            .store
            .insert_artifacts(&src.id, &[na("## Heading\n\n- one")])
            .await
            .unwrap();
        let written = artifact_view(&core.store.get_artifact(&a[0].id).await.unwrap());
        assert!(written.html.contains("<h2>"), "{}", written.html);

        // The detail pane renders the same artifact and has to say the same
        // thing about it: it is the half of the search page that shows a
        // passage in full.
        let d = super::build_artifact_detail(&core, &p[0].id, "")
            .await
            .unwrap();
        assert!(d.html.contains("<pre"), "{}", d.html);
    }

    #[tokio::test]
    async fn editing_a_missing_chunk_is_a_404() {
        let (app, cookie) = app_with_session().await;
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui/artifacts/missing")
                    .method("PUT")
                    .header("cookie", &cookie)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from("text=edited"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn every_result_carries_the_id_the_selection_handler_matches_on() {
        let (app, cookie) = app_with_embedded_corpus().await;
        let frag = get_body(&app, &cookie, "/ui/search/results?q=alpha").await;
        assert!(
            frag.contains(r#"role="option" aria-selected="false""#),
            "{frag}"
        );
        assert!(frag.contains("/ui/artifacts/"), "{frag}");
    }

    #[tokio::test]
    async fn tags_are_stored_and_filterable_but_never_rendered() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        let c = core.store.artifacts_for_corpus(&out.id).await.unwrap()[0].clone();
        core.store
            .update_artifact_tags(&c.id, &["forensik".into()])
            .await
            .unwrap();

        let page = get_body(&app, &cookie, &format!("/ui/artifacts/{}", c.id)).await;
        assert!(
            !page.contains("forensik"),
            "no chips on the artifact: {page}"
        );

        let search = get_body(&app, &cookie, "/ui/search").await;
        assert!(
            !search.contains(r#"aria-label="Tag""#),
            "no tag facet row: {search}"
        );

        // Still true, still stored, still the channel pinning rides on.
        assert_eq!(
            core.store.get_artifact(&c.id).await.unwrap().tags,
            vec!["forensik".to_string()]
        );
    }

    #[tokio::test]
    async fn with_priming_off_the_sitting_moves_no_result() {
        // The default. Carrying ships on because it changes no order; this is
        // the part that does, and it waits for the harness.
        let mut c = crate::core::test_support::test_core().await;
        c.learn.enabled = true;
        assert!(
            !c.ranking.read().unwrap().sitting_prime,
            "priming must ship off"
        );
        let core = c.clone();
        let (app, cookie) = app_with_cookie(c).await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let ids: Vec<String> = core
            .store
            .insert_artifacts(
                &src.id,
                &["alpha one", "alpha two", "alpha three", "alpha four"]
                    .iter()
                    .enumerate()
                    .map(|(i, t)| crate::store::artifacts::NewArtifact {
                        ordinal: i as i64,
                        text: (*t).into(),
                        segment_idx: Some(0),
                        ..Default::default()
                    })
                    .collect::<Vec<_>>(),
            )
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        crate::jobs::embed::run_corpus(&core, &src.id)
            .await
            .unwrap();

        let before = get_body(&app, &cookie, "/ui/search/results?q=alpha").await;
        // Read the last one this list returns, then search again.
        for id in &ids {
            get_body(&app, &cookie, &format!("/ui/artifacts/{id}")).await;
        }
        let after = get_body(&app, &cookie, "/ui/search/results?q=alpha").await;

        let rank_of = |html: &str| -> Vec<String> {
            html.match_indices("/ui/artifacts/")
                .map(|(i, _)| html[i + 14..i + 50].to_string())
                .collect()
        };
        assert_eq!(
            rank_of(&before),
            rank_of(&after),
            "the sitting moved a result with priming off"
        );
    }

    #[tokio::test]
    async fn the_sitting_writes_no_activation() {
        // The guard most likely to be lost to a refactor: the sitting is a
        // *read* of what is happening. Writing activation from it would be a
        // loop that reinforces itself, which is the failure mode this whole
        // area is built to close.
        let mut c = crate::core::test_support::test_core().await;
        c.learn.enabled = true;
        let core = c.clone();
        let (app, cookie) = app_with_cookie(c).await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let a = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    text: "mounting an E01".into(),
                    segment_idx: Some(0),
                    ..Default::default()
                }],
            )
            .await
            .unwrap()[0]
            .id
            .clone();
        // Whatever opening it records, record it now, before the sitting is
        // asked for anything.
        get_body(&app, &cookie, &format!("/ui/artifacts/{a}")).await;
        core.background.wait_idle().await;
        let before = core
            .store
            .activation_of(std::slice::from_ref(&a))
            .await
            .unwrap();

        // Reading the sitting, repeatedly, from both pages.
        for _ in 0..3 {
            get_body(&app, &cookie, "/ui/search").await;
        }
        core.background.wait_idle().await;

        assert_eq!(
            core.store.activation_of(&[a]).await.unwrap(),
            before,
            "reading the sitting moved an activation"
        );
    }

    #[tokio::test]
    async fn a_page_declares_what_it_holds_rather_than_how_wide_it_is() {
        // The three shell widths are gone. What is left is a statement about
        // content: a rail beside an artifact beside its source, prose at a
        // reading measure, or a table that is as wide as its columns need.
        // Every one of them starts at the shell's left edge, which is what
        // stops the content column moving as you navigate.
        let (app, cookie, core) = app_session_and_core().await;
        hold_something(&core).await;

        let search = get_body(&app, &cookie, "/ui/search").await;
        assert!(
            search.contains("regions-rail-focus-source"),
            "search is a three-region page: {search}"
        );

        let ops = get_body(&app, &cookie, "/ui/insights").await;
        assert!(
            ops.contains("regions-table"),
            "housekeeping is a table and has no reading measure: {ops}"
        );

        // Capture is a door into the workspace now, not a page of its own, so
        // it declares what the workspace declares.
        let capture = get_body(&app, &cookie, "/ui/capture").await;
        assert!(
            capture.contains("regions-rail-focus-source"),
            "the capture door is the workspace: {capture}"
        );

        // Ask is a door into the workspace now, not a page of its own. The
        // excerpts land in the same rail the results were in, because the rail
        // holds what the current act produced and an ask is a different act
        // from the search before it.
        let ask = get_body(&app, &cookie, "/ui/ask").await;
        assert!(
            ask.contains("regions-rail-focus-source"),
            "the ask door is the workspace: {ask}"
        );

        // No measure on the one page whose whole subject is an artifact and
        // the lines it came from — it is the same split the search pane holds,
        // so it gets the same room rather than a reading column with the rest
        // of the window empty beside it. Fetched as a real artifact page: the
        // assertion is about what `/ui/artifacts/<id>` declares, and pointing
        // it at any other route would pass without testing that.
        let out = core
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        let id = core.store.artifacts_for_corpus(&out.id).await.unwrap()[0]
            .id
            .clone();
        let artifact = get_body(&app, &cookie, &format!("/ui/artifacts/{id}")).await;
        assert!(
            artifact.contains(r#"regions regions-split"#),
            "the artifact page is a split, not prose: {artifact}"
        );

        // No page says how wide it is any more.
        for (uri, body) in [
            ("/ui/search", &search),
            ("/ui/insights", &ops),
            ("/ui/capture", &capture),
            ("/ui/ask", &ask),
        ] {
            assert!(!body.contains("shell-wide"), "{uri} still declares a width");
        }
    }

    /// A citation link and the rail item it lands on are the two halves of one
    /// claim, and they are numbered by two separate passes over two separate
    /// templates: `link_citations` writes the hrefs into the answer, and
    /// `_ask_rail.html` writes the ids into the excerpts. Nothing but this
    /// assertion makes them agree, and a `[1]` that scrolls nowhere reads to a
    /// reader as provenance the base cannot actually show.
    ///
    /// The reply cites `[01]` as well as `[1]`, because the linker anchors on
    /// the parsed number rather than the digits it found: an id of `cite-01`
    /// would satisfy a lazier reading of this and still be a dead link.
    #[tokio::test]
    async fn every_citation_link_in_the_answer_points_at_an_excerpt_the_rail_carries() {
        let mut core = crate::core::test_support::test_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line\n\ncharlie line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        crate::jobs::embed::run_corpus(&core, &out.id)
            .await
            .unwrap();
        // Swapped in after indexing, so the citations in the answer are this
        // reply's and not something retrieval happened to produce.
        core.completer = Some(std::sync::Arc::new(crate::infer::fake::FakeCompleter {
            reply: Some("alpha [1], bravo [2], and alpha again [01].".into()),
        }));
        let (app, cookie) = app_with_cookie(core).await;

        let body = ask_over_sse(&app, &cookie, "what+is+alpha").await;
        let rail = rail_html(&body);
        let answer = done_html(&body);

        let cited = pulled(&answer, r##"href="#cite-"##, '"');
        // Without this the test passes on an answer that cites nothing, which
        // is the state the page was in before this task and the state a broken
        // linker would put it back into.
        assert!(
            !cited.is_empty(),
            "the answer carries no citation links at all, so nothing was checked: {answer}"
        );
        for n in &cited {
            assert!(
                rail.contains(&format!(r#"id="cite-{n}""#)),
                "the answer links to #cite-{n} and the rail carries no such id: {rail}"
            );
        }

        // Coverage on its own is not enough, and a mutation proved it: numbering
        // the rail from zero leaves every id the answer links to still present
        // on the page — one excerpt further down. The links would all resolve
        // and every one of them would cite the wrong artifact, which is the
        // fabricated provenance this whole scheme exists to avoid. So the
        // numbering itself is pinned: 1..n, in the order the rail lists them.
        let ids = pulled(&rail, r#"id="cite-"#, '"');
        assert!(
            ids.len() > 1,
            "an off-by-one cannot show itself over fewer than two excerpts: {rail}"
        );
        let counted: Vec<String> = (1..=ids.len()).map(|i| i.to_string()).collect();
        assert_eq!(ids, counted, "the rail must number 1..n in order: {rail}");

        // And the n-th rail item has to be the n-th excerpt. The answer fragment
        // lists the same citations in the same order under "Artifacts used"
        // (after its own card, which is why the first title is dropped), so the
        // two renderings of one list are checked against each other rather than
        // each being trusted separately.
        let rail_titles = pulled(&rail, r#"<span class="rail-title">"#, '<');
        let mut card_titles = pulled(&answer, r#"<span class="card-title">"#, '<');
        card_titles.remove(0);
        assert_eq!(
            rail_titles, card_titles,
            "the rail and the answer disagree about which excerpt is which"
        );
    }

    /// The model cites more excerpts than it was shown often enough that a link
    /// to a rail item which does not exist is a real outcome; it reads as a
    /// citation and scrolls nowhere.
    #[test]
    fn only_a_bracket_naming_an_excerpt_that_exists_becomes_a_link() {
        let out = super::link_citations("<p>see [1] and [2] but not [9] or [x]</p>", 2);
        assert!(
            out.contains(r##"<a class="cite" href="#cite-1">[1]</a>"##),
            "{out}"
        );
        assert!(
            out.contains(r##"<a class="cite" href="#cite-2">[2]</a>"##),
            "{out}"
        );
        assert!(out.contains("[9]"), "{out}");
        assert!(!out.contains("#cite-9"), "{out}");
        assert!(out.contains("[x]"), "{out}");
    }

    /// A citation link asserts that an excerpt supports the token it wraps.
    /// `argv[1]` is an array index on a base whose answers are full of code,
    /// and the citable range is exactly the range of common indices — so a link
    /// there is provenance the answer never claimed.
    #[test]
    fn an_array_index_inside_a_code_span_is_not_turned_into_a_citation() {
        let out = super::link_citations("<p>see [1]</p><pre><code>argv[1]</code></pre>", 2);
        assert!(
            out.contains(r##"<p>see <a class="cite" href="#cite-1">[1]</a></p>"##),
            "prose still links: {out}"
        );
        assert!(
            out.contains("<code>argv[1]</code>"),
            "code was linked: {out}"
        );
        assert_eq!(out.matches("cite-1").count(), 1, "{out}");
    }

    /// `[01]` cites excerpt one; an anchor of `#cite-01` points at nothing the
    /// rail emits.
    #[test]
    fn a_zero_padded_citation_links_to_the_anchor_the_rail_will_carry() {
        let out = super::link_citations("<p>see [01]</p>", 2);
        assert!(out.contains(r##"href="#cite-1""##), "{out}");
        assert!(!out.contains("cite-01"), "{out}");
    }

    /// It runs over sanitized HTML, where a bracket inside a tag is an
    /// attribute rather than prose.
    #[test]
    fn citation_linking_leaves_the_inside_of_a_tag_alone() {
        let out = super::link_citations(r#"<a href="/x?q=[1]">[1]</a>"#, 1);
        assert_eq!(
            out, r##"<a href="/x?q=[1]"><a class="cite" href="#cite-1">[1]</a></a>"##,
            "{out}"
        );
    }

    #[tokio::test]
    async fn opening_from_another_artifacts_page_records_a_pivot() {
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        let handle = core.clone();
        let (app, cookie) = app_with_cookie(core).await;
        let src = handle
            .store
            .insert_corpus("raw", "web", None)
            .await
            .unwrap();
        let made = handle
            .store
            .insert_artifacts(
                &src.id,
                &[
                    crate::store::artifacts::NewArtifact {
                        text: "a".into(),
                        title: Some("A".into()),
                        ..Default::default()
                    },
                    crate::store::artifacts::NewArtifact {
                        ordinal: 1,
                        text: "b".into(),
                        title: Some("B".into()),
                        ..Default::default()
                    },
                ],
            )
            .await
            .unwrap();
        get_body(&app, &cookie, &format!("/ui/artifacts/{}", made[0].id)).await;
        get_body(
            &app,
            &cookie,
            &format!("/ui/artifacts/{}?via={}", made[1].id, made[0].id),
        )
        .await;
        handle.background.wait_idle().await;
        let now = crate::store::now();
        let got = handle.store.interactions_between(0, now + 1).await.unwrap();
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[0].kind, "opened");
        assert_eq!(got[1].kind, "pivoted");
        assert_eq!(got[1].via.as_deref(), Some(made[0].id.as_str()));
        assert_eq!(got[1].scope.as_deref(), Some("user-1"));
    }

    #[tokio::test]
    async fn a_generated_artifact_shows_its_cues_is_listed_on_ops_and_badged_in_the_rail() {
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        let handle = core.clone();
        let (app, cookie) = app_with_cookie(core).await;
        let src = handle
            .store
            .insert_corpus("raw", "web", None)
            .await
            .unwrap();
        let s = handle
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    text: "source text".into(),
                    title: Some("S".into()),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();
        let g = handle
            .store
            .insert_synthesized_artifact(
                &crate::store::artifacts::NewSynthesized {
                    text: "generated from S".into(),
                    title: Some("Generated title".into()),
                    category: None,
                    tags: vec![],
                    caveats: vec![],
                    cues: vec!["why was this asked".into()],
                },
                &[s[0].id.clone()],
            )
            .await
            .unwrap();
        crate::jobs::embed::run(&handle, &g.id).await.unwrap();
        handle
            .store
            .insert_pursuit(1, &["why was this asked".into()], &[s[0].id.clone()], None)
            .await
            .unwrap();

        let detail = get_body(&app, &cookie, &format!("/ui/artifacts/{}", g.id)).await;
        assert!(
            detail.contains("Written because these were asked"),
            "{detail}"
        );
        assert!(detail.contains("why was this asked"), "{detail}");

        let ops = get_body(&app, &cookie, "/ui/insights").await;
        // One queue now, and the row says what put it there. See `QueueRow`.
        assert!(ops.contains(">generated<"), "{ops}");
        assert!(
            ops.contains(&format!("/ui/ops/artifacts/{}/deprecate", g.id)),
            "{ops}"
        );
        assert!(ops.contains("of searches went quiet"), "{ops}");

        let rail = get_body(
            &app,
            &cookie,
            "/ui/search/results?q=Generated%20title%0Agenerated%20from%20S",
        )
        .await;
        assert!(rail.contains("model-written"), "{rail}");
    }

    #[tokio::test]
    async fn the_page_reports_how_long_an_artifact_was_open() {
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        let handle = core.clone();
        let (app, cookie) = app_with_cookie(core).await;
        let src = handle
            .store
            .insert_corpus("raw", "web", None)
            .await
            .unwrap();
        let a = handle
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    text: "a".into(),
                    ..Default::default()
                }],
            )
            .await
            .unwrap()[0]
            .id
            .clone();
        let res = app
            .clone()
            .oneshot(form(
                &format!("/ui/artifacts/{a}/dwell"),
                &cookie,
                "secs=42",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        handle.background.wait_idle().await;
        let now = crate::store::now();
        let got = handle.store.interactions_between(0, now + 1).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, "dwell");
        assert_eq!(got[0].detail.as_deref(), Some("42"));
        // The detail root names the artifact, which is what the page's timer reads.
        let page = get_body(&app, &cookie, &format!("/ui/artifacts/{a}")).await;
        assert!(page.contains(&format!("data-artifact=\"{a}\"")), "{page}");
    }

    #[tokio::test]
    async fn a_result_opened_from_the_rail_asks_whether_it_was_the_one() {
        // The judge deck asks hours later, out of context, and the honest
        // answer then is "I don't know, I was looking". Under the result just
        // opened the answer is cheap, so that is where it is asked.
        let (app, cookie, handle, a, event) = searched_app().await;
        let page = get_body(&app, &cookie, &format!("/ui/artifacts/{a}?event={event}")).await;
        assert!(
            page.contains("Was this what you were looking for?"),
            "{page}"
        );
        assert!(
            page.contains(&format!("/ui/search/{event}/verdict")),
            "the bar does not name the search: {page}"
        );
        // The rail's own links carry the search that listed them — on the
        // `href` as well as on the `hx-get`. A middle-click, a ⌘-click and a
        // load that reached the page without htmx are all the same open, and
        // the plain `href` used to drop the event: the same act produced a
        // label down one path and silence down the other.
        let rail = include_str!("templates/_results.html");
        let carries = |attr: &str| {
            rail.contains(&format!(
                r#"{attr}="/ui/artifacts/{{{{ r.artifact_id }}}}?terms={{{{ terms|urlencode }}}}{{% if let Some(ev) = event_id %}}&event={{{{ ev }}}}{{% endif %}}""#
            ))
        };
        assert!(carries("href"), "{rail}");
        assert!(carries("hx-get"), "{rail}");

        // Reached any other way — a corpus page, a pasted link — there is no
        // search to be the answer to.
        let plain = get_body(&app, &cookie, &format!("/ui/artifacts/{a}")).await;
        assert!(
            !plain.contains("Was this what you were looking for?"),
            "{plain}"
        );
        // And a bar was not the whole of the open: the rewording after it starts
        // its own event because this one is now the list that was read.
        handle
            .store
            .record_search(
                crate::store::feedback::NewEvent {
                    fold_onto: None,
                    query: "image mount".into(),
                    door: crate::store::feedback::Door::Ui,
                    scope: Some(crate::store::TEST_SUBJECT.into()),
                    filters: "{}".into(),
                    query_vec: vec![0.1, 0.2],
                    embed_model: "fake".into(),
                    candidates: vec![],
                    answered: false,
                    context: None,
                },
                60,
            )
            .await
            .unwrap();
        assert_eq!(handle.store.feedback_stats(0.0).await.unwrap().captured, 2);
    }

    #[tokio::test]
    async fn with_learning_off_a_result_is_just_a_result() {
        let core = crate::core::test_support::test_core().await;
        let handle = core.clone();
        let (app, cookie) = app_with_cookie(core).await;
        let src = handle
            .store
            .insert_corpus("raw", "web", None)
            .await
            .unwrap();
        let a = handle
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    text: "a".into(),
                    ..Default::default()
                }],
            )
            .await
            .unwrap()[0]
            .id
            .clone();
        // With learning off nothing is captured, so the rail draws no event on
        // its links and there is nothing for the bar to be a verdict on.
        let page = get_body(&app, &cookie, &format!("/ui/artifacts/{a}?event=whatever")).await;
        assert!(
            !page.contains("Was this what you were looking for?"),
            "{page}"
        );
        let rail = get_body(&app, &cookie, "/ui/search/results?q=nothing+here").await;
        assert!(!rail.contains("Nothing here has it"), "{rail}");
    }

    #[tokio::test]
    async fn a_long_read_is_a_pursuit_signal_and_never_a_verdict() {
        // A read past some threshold used to be written as the search having
        // found its answer. What that measured was a pane left open, which is
        // an abandoned tab as often as it is an answer — and because the beacon
        // flushes as the pane is *left*, it landed after the buttons under the
        // result and put a hit back onto searches a person had just marked
        // "not sure" or undone. The timer still feeds the pursuit sweep; it no
        // longer labels anything.
        let (app, cookie, handle, a, _event) = searched_app().await;
        let res = app
            .clone()
            .oneshot(form(
                &format!("/ui/artifacts/{a}/dwell"),
                &cookie,
                "secs=42",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        let s = handle.store.feedback_stats(0.0).await.unwrap();
        assert_eq!((s.hits, s.judged), (0, 0), "{s:?}");
        assert_eq!(s.pending, 1, "the search is still an open question");
    }

    #[tokio::test]
    async fn the_bar_takes_yes_no_and_not_sure_and_each_can_be_taken_back() {
        let (app, cookie, handle, a, event) = searched_app().await;
        let verdict = |v: &str| {
            form(
                &format!("/ui/search/{event}/verdict"),
                &cookie,
                &format!("verdict={v}&artifact_id={a}"),
            )
        };
        let res = app.clone().oneshot(verdict("hit")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bar = body_of(res).await;
        assert!(bar.contains("Undo"), "{bar}");
        let s = handle.store.feedback_stats(0.0).await.unwrap();
        assert_eq!((s.hits, s.pending), (1, 0), "{s:?}");

        // Undo: a question again.
        app.clone().oneshot(verdict("none")).await.unwrap();
        assert_eq!(handle.store.feedback_stats(0.0).await.unwrap().pending, 1);

        // No: still a question, for the deck — and one no read may answer.
        app.clone().oneshot(verdict("no")).await.unwrap();
        assert_eq!(handle.store.feedback_stats(0.0).await.unwrap().pending, 1);
        app.clone()
            .oneshot(form(
                &format!("/ui/artifacts/{a}/dwell"),
                &cookie,
                &format!("secs=42&event={event}"),
            ))
            .await
            .unwrap();
        assert_eq!(handle.store.feedback_stats(0.0).await.unwrap().hits, 0);

        // Not sure: no verdict at all — not a discard, which says the search
        // was never real and would drop it from the pairs and out of the
        // purge's exemption on the strength of somebody not remembering. It
        // leaves the waiting figure all the same: nothing asks it again.
        let res = app.clone().oneshot(verdict("skip")).await.unwrap();
        let bar = body_of(res).await;
        assert!(bar.contains("left unanswered"), "{bar}");
        assert!(
            !bar.contains("undo"),
            "a skip is not a verdict to take back"
        );
        let s = handle.store.feedback_stats(0.0).await.unwrap();
        assert_eq!((s.discards, s.judged, s.pending), (0, 0, 0), "{s:?}");
    }

    #[tokio::test]
    async fn a_search_somebody_else_made_cannot_be_opened_or_judged() {
        // Every route that labels a search takes the event id from the page,
        // because the page is what knows which search a row came from. An id
        // is not a capability: guessing at one used to be enough to stamp an
        // open onto another person's search, answer it, or call it a gap —
        // and, by stamping `opened_at`, quietly stop their next keystroke
        // folding as well.
        let (app, cookie, handle, a, _) = searched_app().await;
        let theirs = handle
            .store
            .record_search(
                crate::store::feedback::NewEvent {
                    fold_onto: None,
                    query: "image will not mount".into(),
                    door: crate::store::feedback::Door::Ui,
                    scope: Some("somebody-else".into()),
                    filters: "{}".into(),
                    query_vec: vec![0.1, 0.2],
                    embed_model: "fake".into(),
                    candidates: vec![crate::store::feedback::NewCandidate {
                        artifact_id: a.clone(),
                        score: 1.0,
                        similarity: Some(0.5),
                        shown: true,
                        ..Default::default()
                    }],
                    answered: false,
                    context: None,
                },
                0,
            )
            .await
            .unwrap();

        let page = get_body(&app, &cookie, &format!("/ui/artifacts/{a}?event={theirs}")).await;
        assert!(
            !page.contains("Was this what you were looking for?"),
            "{page}"
        );
        let opened: Option<i64> =
            sqlx::query_scalar("SELECT opened_at FROM search_events WHERE id = ?")
                .bind(&theirs)
                .fetch_one(&handle.store.pool)
                .await
                .unwrap();
        assert_eq!(opened, None, "their next keystroke still folds");

        for body in [
            format!("verdict=hit&artifact_id={a}"),
            format!("verdict=no&artifact_id={a}"),
        ] {
            let res = app
                .clone()
                .oneshot(form(
                    &format!("/ui/search/{theirs}/verdict"),
                    &cookie,
                    &body,
                ))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::NOT_FOUND);
        }
        let res = app
            .clone()
            .oneshot(form(
                &format!("/ui/search/{theirs}/gap?q=image%20will%20not%20mount"),
                &cookie,
                "",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        let s = handle.store.feedback_stats(0.0).await.unwrap();
        assert_eq!((s.judged, s.hits, s.gaps), (0, 0, 0), "{s:?}");
    }

    #[tokio::test]
    async fn a_deprecated_result_is_never_asked_about() {
        // `eval::export` drops any pair naming an artifact search will not
        // return, so a hit recorded against one raises the recall on Insights
        // and contributes nothing to `pairs.json`. The verdict write refuses
        // one, and the bar under a result must not be a way around it.
        let (app, cookie, handle, a, event) = searched_app().await;
        handle
            .store
            .set_artifact_status(&a, crate::store::artifacts::ArtifactStatus::Deprecated)
            .await
            .unwrap();
        let page = get_body(&app, &cookie, &format!("/ui/artifacts/{a}?event={event}")).await;
        assert!(
            !page.contains("Was this what you were looking for?"),
            "{page}"
        );
        // And the write refuses it too, not only the render: a replayed form
        // naming the pair is not the way around the guard.
        let res = app
            .clone()
            .oneshot(form(
                &format!("/ui/search/{event}/verdict"),
                &cookie,
                &format!("verdict=hit&artifact_id={a}"),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert_eq!(handle.store.feedback_stats(0.0).await.unwrap().hits, 0);
    }
}
