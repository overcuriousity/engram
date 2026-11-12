//! One artifact read in full: the detail pane and the writes made from it.
//! `link_citations` lives here because it links this pane's excerpt numbers.

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

/// Short: a way out of the pane, not a second result rail.
const RELATED_LIMIT: usize = 5;

pub struct ArtifactDetail {
    pub id: String,
    /// Empty where nobody named it; the pane then writes no heading (see `title_of`).
    pub title: String,
    /// `title`, else the text's opening; only the browser tab and history use it.
    pub tab: String,
    /// Sanitized by `markdown::render`. Rendered with `|safe`.
    pub html: String,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub flags: Vec<String>,
    pub flag_detail: Option<String>,
    /// Opening a hidden artifact by link has to say why it is not in results.
    pub superseded_by: Option<String>,
    pub status: crate::store::artifacts::ArtifactStatus,
    pub last_verified_at: Option<i64>,
    /// Conditions the source stated under which this artifact does not apply.
    pub caveats: Vec<String>,
    /// `None` for a merged artifact, which belongs to no corpus.
    pub corpus_id: Option<String>,
    /// The source text for the edit box; markdown does not round-trip from `html`.
    pub text: String,
    /// Empty for a captured artifact that replaced nothing, so the template asks `is_empty`.
    pub lineage: crate::web::lineage_view::Lineage,
    /// Which pane to render: a merge has no corpus or span. Not inferred from `sources`, which a merge can lose.
    pub merged: bool,
    pub synthesized: bool,
    pub cues: Vec<String>,
    /// One of the sources has since been deleted; the text still carries what it said.
    pub orphaned_source: bool,
    /// The source was never captured here: the artifact was restored from the vector store.
    pub corpus_restored: bool,
    /// The search this was opened from, when recorded; see `Store::open_event`.
    pub search_event: Option<String>,
    /// Scrolled to and highlighting the drawn lines; the plain source page where no span is recorded.
    pub source_at_lines: String,
    /// The next passage, when this one stops mid-sentence.
    pub continues_at: Option<String>,
    pub segment_idx: Option<i64>,
    pub slice_label: String,
    pub slice_lines: Vec<crate::web::corpus_view::CorpusLine>,
    /// Space separated; empty outside a search.
    pub terms: String,
    /// From the stored vector, so no inference call; empty until embedded.
    pub related: Vec<RelatedArtifact>,
    /// From co-retrieval rather than resemblance; shown beside `related`, not instead of it.
    pub seen_together: Vec<SeenTogether>,
    /// `None` before the artifact has been integrated.
    pub tag: Option<String>,
    /// One line each: class, the shortened question, and where it last found this.
    pub probes: Vec<String>,
    /// `(n, when, action id)`, oldest first; the action id is the undo target while the condensation is open.
    pub versions: Vec<(i64, String, String)>,
    /// The open condensation's action id, for the button that puts the last version back.
    pub condensed: Option<String>,
    /// Shown regardless of `time.horizon_hours`, unlike the result-row badge.
    pub due_in: Option<String>,
    /// The open reminder, or a blank one the editor offers to make.
    pub reminder: ReminderView,
}

/// Read and posted back in the row's own zone, so a save that changes nothing moves nothing.
pub struct ReminderView {
    /// A blank view is an ordinary entry; saving it makes one.
    pub exists: bool,
    pub when: String,
    pub tz: String,
    /// One of `REPEATS`' names, or `custom` with the rule in `rule`.
    pub repeat: &'static str,
    pub rule: String,
}

/// Weekly, monthly and yearly carry no `BYDAY`/`BYMONTHDAY`: `next_after` takes the day from the occurrence.
pub(crate) const REPEATS: &[(&str, Option<&str>)] = &[
    ("once", None),
    ("daily", Some("FREQ=DAILY")),
    ("weekly", Some("FREQ=WEEKLY")),
    ("monthly", Some("FREQ=MONTHLY")),
    ("yearly", Some("FREQ=YEARLY")),
];

fn reminder_view(m: &crate::store::moments::Moment) -> ReminderView {
    use chrono::TimeZone;
    let tz = crate::core::moments::zone(Some(&m.tz));
    let when =
        m.at.and_then(|a| tz.timestamp_opt(a, 0).single())
            .map(|d| d.format("%Y-%m-%dT%H:%M").to_string())
            .unwrap_or_default();
    let repeat = REPEATS
        .iter()
        .find(|(_, r)| *r == m.rule.as_deref())
        .map_or("custom", |(n, _)| n);
    ReminderView {
        exists: true,
        when,
        tz: tz.name().to_string(),
        repeat,
        rule: m.rule.clone().unwrap_or_default(),
    }
}

pub struct RelatedArtifact {
    pub id: String,
    pub title: String,
    pub snippet: String,
}

pub struct SeenTogether {
    pub id: String,
    pub title: String,
    pub snippet: String,
    /// `None` only for a link nothing can explain yet.
    pub why: Option<String>,
    pub corpus_title: String,
    /// Two documents needing each other is the finding; two passages of one document is not.
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
    /// One result opened out of the rail is still the search section.
    fn section(&self) -> &'static str {
        "search"
    }
}

#[derive(serde::Deserialize)]
struct ArtifactEditForm {
    text: String,
    /// The corpus page swaps one card, the detail pane the whole pane.
    #[serde(default)]
    view: String,
    /// So the highlight survives a save.
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
    /// Absent on the ladder's floor, so this says which cluster, not whether it was an offer.
    #[serde(default)]
    pub(crate) rec: Option<i64>,
    /// Marks the link as an offer's; nothing server-side still holds the offer.
    #[serde(default)]
    pub(crate) rung: Option<String>,
    /// Present only on a rail row; see `SearchOutcome::event`.
    #[serde(default)]
    pub(crate) event: Option<String>,
}

#[derive(serde::Deserialize)]
struct DwellForm {
    #[serde(default)]
    secs: i64,
}

impl ArtifactDetail {
    /// Through `Chunk::in_results`'s own predicate, so a new lifecycle state changes one place.
    fn in_results(&self) -> bool {
        crate::store::artifacts::in_results(self.status, self.superseded_by.as_deref())
    }
}

/// The same rule as `title_of`, read off a vector payload.
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

/// Shared by the pane and `GET /api/v1/artifacts/{id}/related`, so phone and browser agree.
pub(crate) struct Pivots {
    pub related: Vec<RelatedArtifact>,
    pub seen_together: Vec<SeenTogether>,
    pub continues_at: Option<String>,
}

pub(crate) async fn pivots(core: &crate::core::Core, c: &crate::store::artifacts::Chunk) -> Pivots {
    let artifact_id = c.id.as_str();
    // A missing neighbour list is not a missing pane.
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
    // Unreadable links are not a missing pane; gated on `associating()` so switching `[learn]` off hides them.
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
            None => "merged".to_string(),
        };
        seen_together.push(SeenTogether {
            title: title_of(&other),
            snippet: markdown::snippet(&other.text, 90),
            // The judge's line, else the question that bound them.
            why: l
                .reason
                .clone()
                .or_else(|| l.cues.first().map(|c| format!("when asking: {}", c.q))),
            corpus_title,
            cross_corpus: l.cross_corpus,
            id: other.id,
        });
    }
    // Only when the passage stops mid-sentence: it is a second lookup per pane.
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
    Pivots {
        related,
        seen_together,
        continues_at,
    }
}

/// Shared by the pane and `GET /api/v1/artifacts/{id}/about`.
pub(crate) struct About {
    pub tag: Option<String>,
    pub probes: Vec<String>,
    pub versions: Vec<(i64, String, String)>,
    pub condensed: Option<String>,
    pub due_in: Option<String>,
}

pub(crate) async fn about(
    core: &crate::core::Core,
    c: &crate::store::artifacts::Chunk,
) -> Result<About> {
    let artifact_id = c.id.as_str();
    let due_in = core
        .store
        .open_due_for_artifact(artifact_id)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(artifact_id, error = %e, "no due state for this pane");
            None
        })
        // The effective instant, as `due_for` and `open_due` read it, so a snoozed row agrees across the screen.
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
    Ok(About {
        tag,
        probes,
        versions,
        condensed,
        due_in,
    })
}

pub(crate) async fn build_artifact_detail(
    core: &crate::core::Core,
    artifact_id: &str,
    terms: &str,
) -> Result<ArtifactDetail> {
    let c = core.store.get_artifact(artifact_id).await?;
    let html = artifact_html(&c);
    // A merged artifact has no corpus, so no lines and no span.
    let src = match &c.corpus_id {
        Some(id) => Some(core.store.get_corpus(id).await?),
        None => None,
    };
    let slice = match &src {
        Some(s) => crate::web::corpus_view::slice(s, c.corpus_span.as_ref(), 3),
        None => crate::web::corpus_view::CorpusSlice::default(),
    };
    // A missing lineage is not a missing pane.
    let lineage = crate::web::lineage_view::build(&core.store, artifact_id)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(artifact_id, error = %e, "no lineage for this pane");
            Default::default()
        });
    // The fragment scrolls to the span; the query parameters highlight it. Empty for a merge.
    let source_at_lines = match (&c.corpus_id, c.corpus_span.as_ref()) {
        (Some(cid), Some(sp)) => format!(
            "/ui/corpora/{cid}?from={}&to={}#L{}",
            sp.start_line, sp.end_line, sp.start_line
        ),
        (Some(cid), None) => format!("/ui/corpora/{cid}"),
        (None, _) => String::new(),
    };
    let orphaned_source = c.flags.iter().any(|f| f == "orphaned_source");
    let Pivots {
        related,
        seen_together,
        continues_at,
    } = pivots(core, &c).await;
    // Taken before the struct, which moves `c`.
    let title = artifact_title(&c);
    let tab = crate::web::ui::row_label(&c).text;
    // A missing due moment is not a missing pane; undated reminders are left out, as `due_for` does.
    let About {
        tag,
        probes,
        versions,
        condensed,
        due_in,
    } = about(core, &c).await?;
    let reminder = core
        .store
        .open_due_for_artifact(artifact_id)
        .await?
        .map_or_else(
            || ReminderView {
                exists: false,
                when: String::new(),
                tz: String::new(),
                repeat: "once",
                rule: String::new(),
            },
            |m| reminder_view(&m),
        );
    Ok(ArtifactDetail {
        reminder,
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
        corpus_restored: src.as_ref().is_some_and(|s| s.restored_at.is_some()),
        segment_idx: c.segment_idx,
        slice_label: slice.label,
        slice_lines: slice.lines,
        terms: terms.to_string(),
        search_event: None,
    })
}

/// An htmx swap wants the pane's body; a pasted link wants the full page.
async fn artifact_detail(
    tenant: Tenant,
    identity: crate::auth::Identity,
    headers: axum::http::HeaderMap,
    Path(cid): Path<String>,
    Query(p): Query<ArtifactViewParams>,
) -> UiResult<Response> {
    let mut d = build_artifact_detail(&tenant.core, &cid, &p.terms).await?;
    // Only for an artifact still in results and the searcher's own event (`Store::event_is_mine`).
    // A hit on a hidden artifact would inflate recall and MRR over a result no search can give again.
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
    // An offer's open is recorded as `recommended_open`, not an ordinary open, so lucky guesses do not train the profile.
    // The rung is checked against the ladder: it arrives in a query string.
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
    // Here, not in `record_interaction`, because this is where the session is known.
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
        // Back to one appended passage: the run's length lives in the link, not in the form.
        let d = build_artifact_detail(&tenant.core, &cid, &f.terms).await?;
        return Ok(HtmlTemplate(ArtifactDetailFragment { d }).into_response());
    }
    let c = tenant.core.store.get_artifact(&cid).await?;
    Ok(HtmlTemplate(ArtifactFragment {
        c: artifact_view(&c),
    })
    .into_response())
}

#[derive(serde::Deserialize)]
struct ReminderForm {
    #[serde(default)]
    when: String,
    #[serde(default)]
    tz: String,
    #[serde(default)]
    repeat: String,
    /// The RRULE, read only when `repeat` is `custom`.
    #[serde(default)]
    rule: String,
    #[serde(default)]
    terms: String,
}

/// Any artifact, so an ordinary entry becomes a reminder here; answers the pane.
async fn save_reminder(
    tenant: Tenant,
    Path(aid): Path<String>,
    Form(f): Form<ReminderForm>,
) -> UiResult<Response> {
    let tz = crate::core::moments::zone(Some(&f.tz));
    let at = chrono::NaiveDateTime::parse_from_str(&f.when, "%Y-%m-%dT%H:%M")
        .ok()
        .and_then(|dt| crate::core::moments::resolve_local(dt, tz))
        .ok_or_else(|| Error::Validation(format!("when={}: a date and a time", f.when)))?;
    let rule = match REPEATS.iter().find(|(n, _)| *n == f.repeat) {
        Some((_, r)) => r.map(str::to_string),
        None if f.repeat == "custom" => Some(f.rule.trim().to_string()).filter(|r| !r.is_empty()),
        None => return Err(Error::Validation(format!("repeat={}", f.repeat)).into()),
    };
    tenant
        .core
        .edit_reminder(&aid, at, tz.name(), rule.as_deref())
        .await?;
    let d = build_artifact_detail(&tenant.core, &aid, &f.terms).await?;
    Ok(HtmlTemplate(ArtifactDetailFragment { d }).into_response())
}

async fn remove_reminder(
    tenant: Tenant,
    Path(aid): Path<String>,
    Form(f): Form<ReminderForm>,
) -> UiResult<Response> {
    tenant.core.clear_reminder(&aid).await?;
    let d = build_artifact_detail(&tenant.core, &aid, &f.terms).await?;
    Ok(HtmlTemplate(ArtifactDetailFragment { d }).into_response())
}

/// The only thing that removes an artifact; `heal_store_drift` restores what it cannot tell apart.
/// In a list the answer is empty and htmx drops the row; in the pane it lands on the source.
/// An empty 200, not 204: htmx swaps nothing on no-content.
async fn delete_artifact_ui(
    tenant: Tenant,
    headers: axum::http::HeaderMap,
    Path(aid): Path<String>,
) -> UiResult<Response> {
    let a = tenant.core.store.get_artifact(&aid).await?;
    let corpus_id = a.corpus_id.clone();
    tenant.core.trash_artifact(&aid).await?;
    if headers.contains_key("hx-request") {
        let name = crate::web::notice::name_of(a.title.as_deref(), &a.text);
        return Ok(axum::response::Html(crate::web::notice::undo(
            &format!("Deleted '{name}'"),
            &format!("/ui/artifacts/{aid}/restore"),
        ))
        .into_response());
    }
    // A merged artifact has no document to return to.
    Ok(match corpus_id {
        Some(cid) => Redirect::to(&format!("/ui/corpora/{cid}")).into_response(),
        None => Redirect::to("/ui/insights").into_response(),
    })
}

/// `sendBeacon` lands here. A pursuit signal only: an open pane is not an answer, so it never labels the search.
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

/// Final for that pair; the weight is left as is so the decision stays auditable.
async fn dismiss_link(
    tenant: Tenant,
    Path((artifact_id, other_id)): Path<(String, String)>,
) -> UiResult<Response> {
    tenant
        .core
        .store
        .dismiss_link(&artifact_id, &other_id)
        .await?;
    // The row swaps itself out and leaves the pane alone.
    Ok(axum::response::Html(String::new()).into_response())
}

fn link_text(text: &str, n: usize) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        // The parsed number: `[01]` cites excerpt one, and `#cite-01` matches nothing.
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

/// Bounded by `n`, the excerpts shown; an out-of-range bracket stays plain text.
/// Tag interiors and code spans are skipped: `argv[1]` is not a citation, and a wrong link is fabricated provenance.
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
        .route(
            "/ui/artifacts/{id}/links/{other}/dismiss",
            post(dismiss_link),
        )
        .route("/ui/artifacts/{id}/delete", post(delete_artifact_ui))
        .route("/ui/artifacts/{id}/reminder", post(save_reminder))
        .route("/ui/artifacts/{id}/reminder/remove", post(remove_reminder))
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

    #[tokio::test]
    async fn deleting_answers_with_an_undo_notice_and_no_dialog() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core
            .ingest("mount the loop image", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        let first = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .remove(0);
        let (id, before) = (first.id, first.status);
        let mut req = form(&format!("/ui/artifacts/{id}/delete"), &cookie, "");
        req.headers_mut()
            .insert("hx-request", "true".parse().unwrap());
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = body_of(res).await;
        assert!(body.contains("Deleted &#39;"), "{body}");
        assert!(
            body.contains(&format!("hx-post=\"/ui/artifacts/{id}/restore\"")),
            "{body}"
        );
        assert!(!body.contains("confirm("));
        assert_eq!(
            core.store.get_artifact(&id).await.unwrap().status,
            crate::store::artifacts::ArtifactStatus::Trashed
        );
        let trash = get_body(&app, &cookie, "/ui/trash").await;
        assert!(
            trash.contains(&format!("/ui/artifacts/{id}/restore")),
            "{trash}"
        );
        let res = app
            .clone()
            .oneshot(form(&format!("/ui/artifacts/{id}/restore"), &cookie, ""))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(core.store.get_artifact(&id).await.unwrap().status, before);
    }

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

        let d = build_artifact_detail(&core, &aid, "").await.unwrap();
        let due = d.due_in.expect("the pane says a reminder is here");
        assert!(!due.contains("ago"), "the pane read past the snooze: {due}");
    }

    #[tokio::test]
    async fn a_captured_artifact_lists_no_sources() {
        let core = crate::core::test_support::test_core().await;
        let ids = crate::jobs::consolidate::tests::seed(&core, &[("a text", [1.0, 0.0])]).await;

        let d = build_artifact_detail(&core, &ids[0], "").await.unwrap();

        assert!(d.lineage.is_empty());
        assert!(!d.merged);
        assert!(d.corpus_id.is_some());
    }

    #[tokio::test]
    async fn a_merge_that_lost_every_source_still_renders_as_a_merge() {
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
        assert!(
            d.slice_label.starts_with("line ") || d.slice_label.starts_with("lines "),
            "{}",
            d.slice_label
        );
    }

    #[tokio::test]
    async fn a_passage_cut_mid_sentence_points_at_the_one_that_carries_the_rest() {
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
        // The rung arrives in a query string; taken as written, anyone could add rows to the Ops breakdown.
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
        let mut core = crate::core::test_support::test_core().await;
        core.recommend.enabled = true;
        core.learn.enabled = true;
        // Both on, so an ordinary open would be recorded; otherwise this passes on a base that records nothing.
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

        get_body(&app, &cookie, &format!("/ui/artifacts/{aid}")).await;
        background.wait_idle().await;
        let rows = store.interactions_between(0, i64::MAX).await.unwrap();
        assert!(rows.iter().any(|r| r.kind == "opened"));
    }

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
            page.contains(r#"data-var-d="1""#),
            "the earlier merge's own sources are drawn under it: {page}"
        );
        assert!(
            page.contains("Written from 3 artifacts"),
            "the count is of captures, not of the route they took: {page}"
        );
        assert!(page.contains("replaced by this"), "{page}");
    }

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

    #[tokio::test]
    async fn the_pane_marks_an_open_reminder_however_far_out_it_is() {
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
                // An hour of slack: `ago_or_ahead` floors, so an exact multiple flips to "29 days" one second later.
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
        assert!(page.contains("oc-status--due"), "{page}");
        assert!(
            page.contains("30 days"),
            "far outside the 48h horizon, still shown: {page}"
        );
    }

    #[tokio::test]
    async fn the_pane_of_an_artifact_with_no_reminder_carries_no_due_mark() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core.ingest("just a note", "web", None).await.unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        let c = core.store.artifacts_for_corpus(&out.id).await.unwrap()[0]
            .id
            .clone();

        let page = get_body(&app, &cookie, &format!("/ui/artifacts/{c}")).await;
        assert!(!page.contains("oc-status--due"), "{page}");
    }

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
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        let ids = artifacts(&core, &["alpha text"]).await;
        sqlx::query!("DROP TABLE artifact_links")
            .execute(&core.store.pool)
            .await
            .unwrap();
        let d = build_artifact_detail(&core, &ids[0], "").await.unwrap();
        assert!(d.seen_together.is_empty());
    }

    #[tokio::test]
    async fn a_cross_corpus_pair_is_marked_and_a_same_corpus_pair_is_not() {
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
                &format!("/ui/ops/artifacts/{id}/deprecate"),
                &cookie,
                &format!("to=/ui/artifacts/{id}"),
            ))
            .await
            .unwrap();
        assert_eq!(
            res.headers().get("location").unwrap(),
            format!("/ui/artifacts/{id}").as_str()
        );

        let res = app
            .clone()
            .oneshot(form(
                &format!("/ui/ops/artifacts/{id}/reactivate"),
                &cookie,
                "",
            ))
            .await
            .unwrap();
        assert_eq!(res.headers().get("location").unwrap(), "/ui/insights");
    }

    #[tokio::test]
    async fn a_lifecycle_button_pressed_in_the_pane_swaps_the_artifact_not_the_page() {
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
            &format!("/ui/ops/artifacts/{id}/deprecate"),
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
        assert!(!body.contains("<nav"), "{body}");
    }

    #[tokio::test]
    async fn a_return_path_pointing_off_this_ui_is_ignored() {
        // A redirect that follows user input is an open redirect.
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
            for action in ["deprecate", "reactivate"] {
                let res = app
                    .clone()
                    .oneshot(form(
                        &format!("/ui/ops/artifacts/{id}/{action}"),
                        &cookie,
                        &format!("to={}", urlencoding_of(hostile)),
                    ))
                    .await
                    .unwrap();
                assert_eq!(
                    res.headers().get("location").unwrap(),
                    "/ui/insights",
                    "{action} followed {hostile}"
                );
            }
        }
    }

    #[tokio::test]
    async fn an_artifact_that_is_not_embedded_yet_still_opens() {
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

        let a = core
            .store
            .insert_artifacts(&src.id, &[na("## Heading\n\n- one")])
            .await
            .unwrap();
        let written = artifact_view(&core.store.get_artifact(&a[0].id).await.unwrap());
        assert!(written.html.contains("<h2>"), "{}", written.html);

        let d = super::build_artifact_detail(&core, &p[0].id, "")
            .await
            .unwrap();
        assert!(d.html.contains("<pre"), "{}", d.html);
    }

    #[tokio::test]
    async fn the_pane_edits_the_reminder_and_turns_it_back_into_an_entry() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core
            .ingest_capture(crate::core::ingest::Capture::new(
                "Signal-Gruppe bereinigen",
                "ui",
            ))
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
            .unwrap()
            .id;
        let page = get_body(&app, &cookie, &format!("/ui/artifacts/{aid}")).await;
        assert!(page.contains("Add a reminder"), "{page}");

        let uri = format!("/ui/artifacts/{aid}/reminder");
        let html = body_of(
            app.clone()
                .oneshot(form(
                    &uri,
                    &cookie,
                    "when=2026-10-30T11%3A00&tz=Europe%2FBerlin&repeat=weekly&rule=",
                ))
                .await
                .unwrap(),
        )
        .await;
        assert!(html.contains("reminder · weekly"), "{html}");
        assert!(html.contains(r#"value="2026-10-30T11:00""#), "{html}");
        let m = core
            .store
            .open_due_for_artifact(&aid)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(m.rule.as_deref(), Some("FREQ=WEEKLY"));

        let html = body_of(
            app.clone()
                .oneshot(form(
                    &uri,
                    &cookie,
                    "when=2026-10-30T11%3A00&tz=Europe%2FBerlin&repeat=custom&rule=FREQ%3DWEEKLY%3BBYDAY%3DMO%2CFR",
                ))
                .await
                .unwrap(),
        )
        .await;
        assert!(html.contains("reminder · custom"), "{html}");
        assert!(html.contains("FREQ=WEEKLY;BYDAY=MO,FR"), "{html}");

        let bad = app
            .clone()
            .oneshot(form(
                &uri,
                &cookie,
                "when=2026-10-30T11%3A00&tz=Europe%2FBerlin&repeat=custom&rule=FREQ%3DHOURLY",
            ))
            .await
            .unwrap();
        assert_eq!(bad.status(), StatusCode::BAD_REQUEST);

        let html = body_of(
            app.oneshot(form(&format!("{uri}/remove"), &cookie, "terms="))
                .await
                .unwrap(),
        )
        .await;
        assert!(html.contains("Add a reminder"), "{html}");
        assert!(
            core.store
                .open_due_for_artifact(&aid)
                .await
                .unwrap()
                .is_none()
        );
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

        assert_eq!(
            core.store.get_artifact(&c.id).await.unwrap().tags,
            vec!["forensik".to_string()]
        );
    }

    #[tokio::test]
    async fn with_priming_off_the_sitting_moves_no_result() {
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
        // The sitting is a read; writing activation from it would be a self-reinforcing loop.
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
        get_body(&app, &cookie, &format!("/ui/artifacts/{a}")).await;
        core.background.wait_idle().await;
        let before = core
            .store
            .activation_of(std::slice::from_ref(&a))
            .await
            .unwrap();

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

        let capture = get_body(&app, &cookie, "/ui/capture").await;
        assert!(
            capture.contains("regions-rail-focus-source"),
            "the capture door is the workspace: {capture}"
        );

        let ask = get_body(&app, &cookie, "/ui/ask").await;
        assert!(
            ask.contains("regions-rail-focus-source"),
            "the ask door is the workspace: {ask}"
        );

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

        for (uri, body) in [
            ("/ui/search", &search),
            ("/ui/insights", &ops),
            ("/ui/capture", &capture),
            ("/ui/ask", &ask),
        ] {
            assert!(!body.contains("shell-wide"), "{uri} still declares a width");
        }
    }

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
        core.completer = Some(std::sync::Arc::new(crate::infer::fake::FakeCompleter {
            reply: Some("alpha [1], bravo [2], and alpha again [01].".into()),
        }));
        let (app, cookie) = app_with_cookie(core).await;

        let body = ask_over_sse(&app, &cookie, "what+is+alpha").await;
        let rail = rail_html(&body);
        let answer = done_html(&body);

        let cited = pulled(&answer, r##"href="#cite-"##, '"');
        // Without this the test passes on an answer that cites nothing.
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

        // Numbering from zero still resolves every link, one excerpt off, so the numbering 1..n is pinned.
        let ids = pulled(&rail, r#"id="cite-"#, '"');
        assert!(
            ids.len() > 1,
            "an off-by-one cannot show itself over fewer than two excerpts: {rail}"
        );
        let counted: Vec<String> = (1..=ids.len()).map(|i| i.to_string()).collect();
        assert_eq!(ids, counted, "the rail must number 1..n in order: {rail}");

        let rail_titles = pulled(&rail, r#"<span class="rail-title">"#, '<');
        let card_titles = pulled(&answer, r#"<span class="cited-title">"#, '<');
        assert_eq!(
            rail_titles, card_titles,
            "the rail and the answer disagree about which excerpt is which"
        );
    }

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

    #[test]
    fn a_zero_padded_citation_links_to_the_anchor_the_rail_will_carry() {
        let out = super::link_citations("<p>see [01]</p>", 2);
        assert!(out.contains(r##"href="#cite-1""##), "{out}");
        assert!(!out.contains("cite-01"), "{out}");
    }

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
    async fn a_generated_artifact_shows_its_cues_is_listed_on_ops_and_named_in_the_rail() {
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
        assert!(rail.contains("written by a model"), "{rail}");
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
        let page = get_body(&app, &cookie, &format!("/ui/artifacts/{a}")).await;
        assert!(page.contains(&format!("data-artifact=\"{a}\"")), "{page}");
    }

    #[tokio::test]
    async fn a_result_opened_from_the_rail_asks_whether_it_was_the_one() {
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
        let rail = include_str!("templates/_results.html");
        let carries = |attr: &str| {
            rail.contains(&format!(
                r#"{attr}="/ui/artifacts/{{{{ r.artifact_id }}}}?terms={{{{ terms|urlencode }}}}{{% if let Some(ev) = event_id %}}&event={{{{ ev }}}}{{% endif %}}""#
            ))
        };
        assert!(carries("href"), "{rail}");
        assert!(carries("hx-get"), "{rail}");

        let plain = get_body(&app, &cookie, &format!("/ui/artifacts/{a}")).await;
        assert!(
            !plain.contains("Was this what you were looking for?"),
            "{plain}"
        );
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
    async fn a_hit_verdict_confirms_the_note_it_names() {
        let (app, cookie, handle, a, event) = searched_app().await;
        handle.store.set_last_verified_at(&a, 1).await.unwrap();
        let res = app
            .clone()
            .oneshot(form(
                &format!("/ui/search/{event}/verdict"),
                &cookie,
                &format!("verdict=hit&artifact_id={a}"),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let stamp = handle
            .store
            .get_artifact(&a)
            .await
            .unwrap()
            .last_verified_at;
        assert!(stamp.unwrap() > 1, "the confirmed result was not stamped");
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

        app.clone().oneshot(verdict("none")).await.unwrap();
        assert_eq!(handle.store.feedback_stats(0.0).await.unwrap().pending, 1);

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
        // An event id is not a capability: guessing one must not let a caller label another person's search.
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
            sqlx::query_scalar!("SELECT opened_at FROM search_events WHERE id = ?", &theirs)
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
